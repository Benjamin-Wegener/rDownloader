//! HTTP(S) connector over Tor streams.
//!
//! `arti-client` only provides raw TCP streams. For `https` the stream is wrapped
//! in TLS via `tokio-rustls` (WebPKI roots); a Hyper HTTP/1.1 client is built on
//! top of that. This means neither the status line nor headers or chunked
//! encoding have to be parsed by hand.
//!
//! The connector is contract-free: it knows neither `contract.rs` nor the
//! download engine and can be tested with any [`StreamProvider`]. The `rpc`
//! submodule exposes the headless control API.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Incoming};
use hyper::client::conn::http1;
use hyper::http::header::{HeaderValue, HOST, RANGE, USER_AGENT};
use hyper::http::{Method, Request, Response, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector as RustlsConnector;
use tracing::debug;
use url::Url;

use crate::t;
use crate::tor::{AsyncReadWrite, TorManager};

pub mod ftp;
pub mod router;
pub mod rpc;
pub mod transport;

mod disposition;

pub use router::SchemeTransport;
pub use transport::TorTransport;

/// Future that yields a raw byte stream to `host:port`.
pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn AsyncReadWrite>>> + Send + 'a>>;

/// Source of raw byte streams: Arti in production, e.g. TCP in tests.
///
/// `isolation` groups streams: the same id may share a circuit, different
/// ids never do.
pub trait StreamProvider: Send + Sync {
    fn connect<'a>(&'a self, isolation: u64, host: &'a str, port: u16) -> ConnectFuture<'a>;
}

impl StreamProvider for TorManager {
    fn connect<'a>(&'a self, isolation: u64, host: &'a str, port: u16) -> ConnectFuture<'a> {
        Box::pin(async move { TorManager::connect_isolated(self, host, port, isolation).await })
    }
}

/// WebPKI roots (Mozilla trust store), as used by browsers.
pub fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// TLS configuration for outgoing connections (HTTP/1.1 only via ALPN).
pub fn tls_config() -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store())
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

fn server_name(host: &str) -> Result<ServerName<'static>> {
    let host = bare_host(host);
    ServerName::try_from(host.to_owned())
        .map_err(|_| anyhow::anyhow!(t!("net.error.tls_hostname", host = format!("{host:?}"))))
}

/// Strips IPv6 brackets (`[::1]` → `::1`).
fn bare_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// Wraps a stream in TLS.
#[derive(Clone)]
pub struct TlsConnector {
    inner: RustlsConnector,
}

impl Default for TlsConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl TlsConnector {
    pub fn new() -> Self {
        Self::with_config(tls_config())
    }

    pub fn with_config(config: Arc<ClientConfig>) -> Self {
        Self {
            inner: RustlsConnector::from(config),
        }
    }

    /// Performs the TLS handshake over an already open byte stream.
    pub async fn connect<S>(&self, stream: S, host: &str) -> Result<TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let name = server_name(host)?;
        self.inner
            .connect(name, stream)
            .await
            .with_context(|| t!("net.error.tls_handshake", host = host))
    }
}

/// Whether `host` is a Tor onion service address (`*.onion`).
///
/// Onion traffic never leaves the Tor network: there is no exit relay, and the
/// connection is end-to-end encrypted, so plain `http://` to an onion address
/// is as private as TLS to a normal site.
pub fn is_onion_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host.strip_suffix(".onion")
        .is_some_and(|name| !name.is_empty())
}

/// Connection target of a URL: port and whether TLS is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectPlan {
    pub port: u16,
    pub tls: bool,
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

fn plan_for(scheme: Option<&str>, port: Option<u16>) -> Result<ConnectPlan> {
    let scheme = scheme.context(t!("net.error.url_without_scheme"))?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        other => bail!(t!("net.error.unsupported_scheme", scheme = other)),
    };
    let port = port
        .or_else(|| default_port(scheme))
        .context(t!("net.error.url_without_port"))?;
    Ok(ConnectPlan { port, tls })
}

/// Determines the port and TLS requirement of a URL.
pub fn connect_plan(url: &Url) -> Result<ConnectPlan> {
    plan_for(Some(url.scheme()), url.port_or_known_default())
}

/// Connects via a [`StreamProvider`] and adds TLS when needed.
#[derive(Clone)]
pub struct TorConnector {
    provider: Arc<dyn StreamProvider>,
    tls: TlsConnector,
}

impl TorConnector {
    pub fn new(tor: Arc<TorManager>) -> Self {
        Self::with_provider(tor, TlsConnector::new())
    }

    pub fn with_provider(provider: Arc<dyn StreamProvider>, tls: TlsConnector) -> Self {
        Self { provider, tls }
    }

    /// Opens a byte stream to `url` (plain HTTP, TLS for HTTPS).
    pub async fn connect(&self, isolation: u64, url: &Url) -> Result<Box<dyn AsyncReadWrite>> {
        let host = url.host_str().context(t!("net.error.url_without_host"))?;
        let plan = connect_plan(url)?;
        self.connect_addr(isolation, host, plan.port, plan.tls)
            .await
    }

    pub async fn connect_addr(
        &self,
        isolation: u64,
        host: &str,
        port: u16,
        tls: bool,
    ) -> Result<Box<dyn AsyncReadWrite>> {
        let host = bare_host(host);
        let stream = self
            .provider
            .connect(isolation, host, port)
            .await
            .with_context(|| t!("net.error.stream", host = host, port = port))?;
        if tls {
            let stream = self.tls.connect(stream, host).await?;
            Ok(Box::new(stream))
        } else {
            Ok(stream)
        }
    }
}

fn default_user_agent() -> String {
    concat!("rdownloader/", env!("CARGO_PKG_VERSION")).to_string()
}

/// Minimal HTTP/1.1 client that opens every connection through the
/// [`TorConnector`].
#[derive(Clone)]
pub struct TorHttpClient {
    connector: TorConnector,
}

impl TorHttpClient {
    pub fn new(tor: Arc<TorManager>) -> Self {
        Self::with_connector(TorConnector::new(tor))
    }

    pub fn with_connector(connector: TorConnector) -> Self {
        Self { connector }
    }

    pub fn connector(&self) -> &TorConnector {
        &self.connector
    }

    pub async fn head(&self, isolation: u64, url: &Url) -> Result<Response<Incoming>> {
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(url.as_str())
            .header(USER_AGENT, default_user_agent())
            .body(Full::new(Bytes::new()))?;
        self.request(isolation, request).await
    }

    /// `range` is inclusive; an end of `None` means "until end of file".
    pub async fn get(
        &self,
        isolation: u64,
        url: &Url,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<Response<Incoming>> {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header(USER_AGENT, default_user_agent());
        if let Some((start, end)) = range {
            let value = match end {
                Some(end) => format!("bytes={start}-{end}"),
                None => format!("bytes={start}-"),
            };
            builder = builder.header(RANGE, value);
        }
        self.request(isolation, builder.body(Full::new(Bytes::new()))?)
            .await
    }

    /// Sends an arbitrary request over a fresh Tor connection.
    pub async fn request<B>(
        &self,
        isolation: u64,
        mut request: Request<B>,
    ) -> Result<Response<Incoming>>
    where
        B: Body + Send + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
    {
        let uri = request.uri().clone();
        let plan = plan_for(uri.scheme_str(), uri.port_u16())?;
        let host = uri
            .host()
            .context(t!("net.error.request_uri_without_host"))?;

        if !request.headers().contains_key(HOST) {
            let authority = authority(&uri, host, plan.port);
            request
                .headers_mut()
                .insert(HOST, HeaderValue::from_str(&authority)?);
        }

        let stream = self
            .connector
            .connect_addr(isolation, host, plan.port, plan.tls)
            .await?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .context(t!("net.error.http_handshake"))?;

        tokio::spawn(async move {
            if let Err(error) = connection.await {
                debug!("HTTP connection closed: {error}");
            }
        });

        sender
            .send_request(request)
            .await
            .context(t!("net.error.send_request"))
    }
}

fn authority(uri: &Uri, host: &str, port: u16) -> String {
    let default = uri.scheme_str().and_then(default_port);
    if default == Some(port) {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::server::conn::http1 as server_http1;
    use hyper::service::service_fn;
    use std::convert::Infallible;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    const TEST_CERT: &[u8] = include_bytes!("testdata/localhost-cert.der");
    const TEST_KEY: &[u8] = include_bytes!("testdata/localhost-key.der");

    fn cert_der() -> CertificateDer<'static> {
        CertificateDer::from(TEST_CERT.to_vec())
    }

    fn key_der() -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(TEST_KEY.to_vec()))
    }

    fn test_client_config() -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(cert_der()).expect("test certificate");
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(config)
    }

    fn test_server_config() -> Arc<ServerConfig> {
        Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der()], key_der())
                .expect("test server config"),
        )
    }

    struct TcpProvider;

    impl StreamProvider for TcpProvider {
        fn connect<'a>(&'a self, _isolation: u64, host: &'a str, port: u16) -> ConnectFuture<'a> {
            Box::pin(async move {
                let stream = tokio::net::TcpStream::connect((host, port))
                    .await
                    .context("TCP connect")?;
                Ok(Box::new(stream) as Box<dyn AsyncReadWrite>)
            })
        }
    }

    #[test]
    fn sources_do_not_use_local_dns_resolution() {
        // Names must reach the exit node via the Tor stream. Anything that
        // resolves locally (default HTTP clients, system resolver APIs) is a
        // DNS leak. Tokens are assembled so this test does not match itself.
        const FORBIDDEN: &[&str] = &[
            concat!("req", "west"),
            concat!("lookup", "_host"),
            concat!("to_socket", "_addrs"),
            concat!("ToSocket", "Addrs"),
            concat!("getaddr", "info"),
        ];

        fn visit(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read source directory") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    visit(&path, files);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        visit(&root, &mut files);

        let mut offenders = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read source file");
            for (line_no, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for needle in FORBIDDEN {
                    if line.contains(needle) {
                        offenders.push(format!("{}:{} ({needle})", path.display(), line_no + 1));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "local DNS resolution outside Tor found in: {offenders:#?}"
        );
    }

    #[test]
    fn webpki_roots_are_loaded() {
        assert!(!root_store().is_empty());
    }

    #[test]
    fn plans_http_https_and_rejects_other_schemes() {
        assert_eq!(
            connect_plan(&Url::parse("http://example.com/a").unwrap()).unwrap(),
            ConnectPlan {
                port: 80,
                tls: false
            }
        );
        assert_eq!(
            connect_plan(&Url::parse("https://example.com/a").unwrap()).unwrap(),
            ConnectPlan {
                port: 443,
                tls: true
            }
        );
        assert_eq!(
            connect_plan(&Url::parse("https://example.com:8443/a").unwrap()).unwrap(),
            ConnectPlan {
                port: 8443,
                tls: true
            }
        );
        assert!(connect_plan(&Url::parse("ftp://example.com/a").unwrap()).is_err());
    }

    #[test]
    fn server_name_accepts_dns_and_ipv6() {
        assert!(server_name("localhost").is_ok());
        assert!(server_name("[::1]").is_ok());
        assert!(server_name("999.999.999.999").is_err());
    }

    #[tokio::test]
    async fn tls_handshake_over_arbitrary_stream() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let acceptor = TlsAcceptor::from(test_server_config());

        let server = tokio::spawn(async move {
            let mut stream = acceptor.accept(server_io).await.expect("TLS accept");
            stream.write_all(b"pong").await.expect("write");
            stream.shutdown().await.expect("shutdown");
        });

        let mut client = TlsConnector::with_config(test_client_config())
            .connect(client_io, "localhost")
            .await
            .expect("TLS connect");

        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"pong");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn https_get_through_connector() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = TlsAcceptor::from(test_server_config());

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let tls = acceptor.accept(tcp).await.expect("TLS accept");
            let service = service_fn(|request: Request<Incoming>| async move {
                assert_eq!(request.uri().path(), "/file");
                assert_eq!(
                    request.headers().get(RANGE).and_then(|v| v.to_str().ok()),
                    Some("bytes=0-3")
                );
                assert!(request.headers().get(HOST).is_some());
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"hello"))))
            });
            server_http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service)
                .await
                .expect("serve");
        });

        let connector = TorConnector::with_provider(
            Arc::new(TcpProvider),
            TlsConnector::with_config(test_client_config()),
        );
        let client = TorHttpClient::with_connector(connector);
        let url = Url::parse(&format!("https://localhost:{port}/file")).unwrap();

        let response = client.get(0, &url, Some((0, Some(3)))).await.expect("GET");
        assert_eq!(response.status(), 200);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(&body[..], b"hello");
        server.await.expect("server task");
    }

    #[test]
    fn onion_hosts_are_recognised() {
        assert!(is_onion_host(
            "duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad.onion"
        ));
        assert!(is_onion_host("Sub.Example.ONION."));
        assert!(!is_onion_host("example.com"));
        assert!(!is_onion_host("notonion"));
        assert!(!is_onion_host(".onion"));
        assert!(!is_onion_host("onion.example.com"));
    }

    #[test]
    fn plain_http_to_onion_needs_no_tls() {
        let url =
            Url::parse("http://duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad.onion/")
                .unwrap();
        assert_eq!(
            connect_plan(&url).unwrap(),
            ConnectPlan {
                port: 80,
                tls: false
            }
        );
    }
}

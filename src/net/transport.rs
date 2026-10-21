//! [`Transport`](crate::contract::Transport) implementation over Tor.
//!
//! Maps contract A (including byte ranges and isolation) onto the
//! Hyper HTTP/1.1 client from [`crate::net`]. Response metadata is derived
//! entirely from HTTP headers; the body stays a stream. Redirects are
//! followed (up to a fixed limit), chunked transfer encoding is decoded by
//! Hyper, and `Content-Disposition` supplies the file name when present.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{
    HeaderMap, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, ETAG,
    LAST_MODIFIED, LOCATION,
};
use hyper::StatusCode;
use url::Url;

use crate::contract::{ByteRange, ByteStream, Response, ResponseHead, Transport};
use crate::net::{disposition, TorHttpClient};
use crate::tor::TorManager;

/// Upper bound for a single request, including every redirect hop.
const MAX_REDIRECTS: usize = 10;

/// Which request to send (and repeat on each redirect hop).
#[derive(Clone, Copy)]
enum RequestKind {
    Head,
    Get(Option<ByteRange>),
}

/// HTTP transport that sends every request over its own Tor stream.
///
/// `isolation` is passed through to [`StreamProvider`](crate::net::StreamProvider)
/// unchanged, so different ids never share a circuit.
pub struct TorTransport {
    http: TorHttpClient,
}

impl TorTransport {
    pub fn new(tor: Arc<TorManager>) -> Self {
        Self {
            http: TorHttpClient::new(tor),
        }
    }

    pub fn with_client(http: TorHttpClient) -> Self {
        Self { http }
    }

    pub fn client(&self) -> &TorHttpClient {
        &self.http
    }

    /// Sends a request and follows redirects, returning the final URL together
    /// with the first non-redirect response.
    async fn fetch(
        &self,
        url: &Url,
        isolation: u64,
        kind: RequestKind,
    ) -> Result<(Url, hyper::Response<Incoming>)> {
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let response = match kind {
                RequestKind::Head => self
                    .http
                    .head(isolation, &current)
                    .await
                    .context("HEAD over Tor")?,
                RequestKind::Get(range) => self
                    .http
                    .get(isolation, &current, range.map(|r| (r.start, r.end)))
                    .await
                    .context("GET over Tor")?,
            };
            match redirect_target(&response, &current)? {
                Some(next) => current = next,
                None => return Ok((current, response)),
            }
        }
        bail!("Too many redirects (limit {MAX_REDIRECTS}) for {url}")
    }
}

/// Returns the resolved `Location` target for a redirect response.
fn redirect_target(response: &hyper::Response<Incoming>, current: &Url) -> Result<Option<Url>> {
    let follow = matches!(
        response.status(),
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    );
    if !follow {
        return Ok(None);
    }
    let Some(location) = response.headers().get(LOCATION) else {
        return Ok(None);
    };
    let location = location
        .to_str()
        .context("Redirect with a non-ASCII Location header")?;
    let next = current
        .join(location)
        .with_context(|| format!("Resolving redirect location {location:?}"))?;
    match next.scheme() {
        "http" | "https" => Ok(Some(next)),
        other => bail!("Redirect to unsupported scheme {other:?}"),
    }
}

#[async_trait]
impl Transport for TorTransport {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
        let (final_url, response) = self.fetch(url, isolation, RequestKind::Head).await?;
        let status = response.status();
        if !status.is_success() {
            bail!("HEAD {final_url}: HTTP {}", status.as_u16());
        }
        head_from(status, response.headers(), &final_url)
    }

    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response> {
        let (final_url, response) = self.fetch(url, isolation, RequestKind::Get(range)).await?;
        let status = response.status();
        if !status.is_success() {
            bail!("GET {final_url}: HTTP {}", status.as_u16());
        }

        let (parts, body) = response.into_parts();
        let head = head_from(parts.status, &parts.headers, &final_url)?;
        let body: ByteStream = Box::pin(
            body.into_data_stream()
                .map(|chunk| chunk.map_err(|e| anyhow::anyhow!("body stream: {e}"))),
        );
        Ok(Response { head, body })
    }
}

fn head_from(status: StatusCode, headers: &HeaderMap, url: &Url) -> Result<ResponseHead> {
    let content_length = header_u64(headers, CONTENT_LENGTH);
    let content_range = headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(total_from_content_range);
    let accepts_ranges = status == StatusCode::PARTIAL_CONTENT
        || headers
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .is_some_and(accepts_ranges_value);

    let filename = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(disposition::filename);

    Ok(ResponseHead {
        status: status.as_u16(),
        final_url: url.clone(),
        content_length,
        total_size: content_range.or(content_length),
        accepts_ranges,
        filename,
        etag: header_string(headers, ETAG),
        last_modified: header_string(headers, LAST_MODIFIED),
    })
}

fn header_string(headers: &HeaderMap, name: hyper::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn header_u64(headers: &HeaderMap, name: hyper::header::HeaderName) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// `Accept-Ranges: bytes` (also within a list) means yes.
fn accepts_ranges_value(value: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("bytes"))
}

/// Reads the total size from `Content-Range: bytes 0-4/100`.
fn total_from_content_range(value: &str) -> Option<u64> {
    value.rsplit_once('/').and_then(|(_, total)| {
        let total = total.trim();
        (total != "*").then(|| total.parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Transport;
    use crate::net::{ConnectFuture, StreamProvider};
    use bytes::Bytes;
    use futures::StreamExt;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::header::RANGE;
    use hyper::server::conn::http1 as server_http1;
    use hyper::service::service_fn;
    use hyper::{Request as HttpRequest, Response as HttpResponse};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    const BODY: &[u8] = b"0123456789";

    struct RecordingProvider {
        seen: Arc<Mutex<Vec<u64>>>,
    }

    impl StreamProvider for RecordingProvider {
        fn connect<'a>(&'a self, isolation: u64, host: &'a str, port: u16) -> ConnectFuture<'a> {
            self.seen.lock().unwrap().push(isolation);
            Box::pin(async move {
                let stream = tokio::net::TcpStream::connect((host, port)).await?;
                Ok(Box::new(stream) as Box<dyn crate::tor::AsyncReadWrite>)
            })
        }
    }

    async fn handle(
        request: HttpRequest<Incoming>,
    ) -> Result<HttpResponse<Full<Bytes>>, Infallible> {
        let range = request
            .headers()
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let response = match request.uri().path() {
            "/file" => HttpResponse::builder()
                .status(200)
                .header(ACCEPT_RANGES, "bytes")
                .header(CONTENT_LENGTH, "10")
                .header(ETAG, "\"abc\"")
                .header(LAST_MODIFIED, "Wed, 21 Oct 2026 00:00:00 GMT")
                .body(Full::new(Bytes::from_static(BODY)))
                .unwrap(),
            "/range" => {
                assert_eq!(range.as_deref(), Some("bytes=2-5"));
                HttpResponse::builder()
                    .status(206)
                    .header(CONTENT_RANGE, "bytes 2-5/10")
                    .header(CONTENT_LENGTH, "4")
                    .body(Full::new(Bytes::from_static(&BODY[2..6])))
                    .unwrap()
            }
            "/open" => {
                assert_eq!(range.as_deref(), Some("bytes=7-"));
                HttpResponse::builder()
                    .status(206)
                    .header(CONTENT_RANGE, "bytes 7-9/10")
                    .header(CONTENT_LENGTH, "3")
                    .body(Full::new(Bytes::from_static(&BODY[7..])))
                    .unwrap()
            }
            "/attachment" => HttpResponse::builder()
                .status(200)
                .header(
                    CONTENT_DISPOSITION,
                    "attachment; filename=\"fallback.bin\"; filename*=UTF-8''caf%C3%A9.txt",
                )
                .header(CONTENT_LENGTH, "10")
                .body(Full::new(Bytes::from_static(BODY)))
                .unwrap(),
            "/redirect" => redirect(302, "/file"),
            "/redirect-range" => redirect(302, "/range"),
            "/chain" => redirect(301, "/redirect"),
            "/relative" => redirect(307, "file"),
            "/loop" => redirect(302, "/loop"),
            _ => HttpResponse::builder()
                .status(404)
                .body(Full::new(Bytes::from_static(b"nope")))
                .unwrap(),
        };
        Ok(response)
    }

    fn redirect(status: u16, location: &str) -> HttpResponse<Full<Bytes>> {
        HttpResponse::builder()
            .status(status)
            .header(LOCATION, location)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    async fn serve(expected_requests: usize) -> (String, Arc<Mutex<Vec<u64>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(async move {
            for _ in 0..expected_requests {
                let (tcp, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let _ = server_http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service_fn(handle))
                        .await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn transport(seen: Arc<Mutex<Vec<u64>>>) -> TorTransport {
        let connector = crate::net::TorConnector::with_provider(
            Arc::new(RecordingProvider { seen }),
            crate::net::TlsConnector::new(),
        );
        TorTransport::with_client(TorHttpClient::with_connector(connector))
    }

    #[tokio::test]
    async fn head_and_get_map_contract_fields() {
        let (base, seen) = serve(4).await;
        let t = transport(seen.clone());

        let head = t
            .head(&Url::parse(&format!("{base}/file")).unwrap(), 0)
            .await
            .unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.total_size, Some(10));
        assert!(head.accepts_ranges);
        assert_eq!(head.etag.as_deref(), Some("\"abc\""));
        assert_eq!(
            head.last_modified.as_deref(),
            Some("Wed, 21 Oct 2026 00:00:00 GMT")
        );
        assert!(head.filename.is_none());

        let full = t
            .get(&Url::parse(&format!("{base}/file")).unwrap(), None, 1)
            .await
            .unwrap();
        assert_eq!(full.head.status, 200);
        assert_eq!(full.head.total_size, Some(10));
        assert_eq!(collect(full.body).await, BODY);

        let ranged = t
            .get(
                &Url::parse(&format!("{base}/range")).unwrap(),
                Some(ByteRange {
                    start: 2,
                    end: Some(5),
                }),
                2,
            )
            .await
            .unwrap();
        assert_eq!(ranged.head.status, 206);
        assert_eq!(ranged.head.content_length, Some(4));
        assert_eq!(ranged.head.total_size, Some(10));
        assert!(ranged.head.accepts_ranges);
        assert_eq!(collect(ranged.body).await, &BODY[2..6]);

        let open = t
            .get(
                &Url::parse(&format!("{base}/open")).unwrap(),
                Some(ByteRange {
                    start: 7,
                    end: None,
                }),
                3,
            )
            .await
            .unwrap();
        assert_eq!(open.head.status, 206);
        assert_eq!(collect(open.body).await, &BODY[7..]);

        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn http_errors_become_errors() {
        let (base, seen) = serve(2).await;
        let t = transport(seen);
        let url = Url::parse(&format!("{base}/missing")).unwrap();
        assert!(t.head(&url, 0).await.is_err());
        assert!(t.get(&url, None, 1).await.is_err());
    }

    #[tokio::test]
    async fn follows_redirects_and_reports_final_url() {
        let (base, seen) = serve(9).await;
        let t = transport(seen);

        let head = t
            .head(&Url::parse(&format!("{base}/redirect")).unwrap(), 0)
            .await
            .unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.final_url, Url::parse(&format!("{base}/file")).unwrap());
        assert_eq!(head.total_size, Some(10));

        let ranged = t
            .get(
                &Url::parse(&format!("{base}/redirect-range")).unwrap(),
                Some(ByteRange {
                    start: 2,
                    end: Some(5),
                }),
                1,
            )
            .await
            .unwrap();
        assert_eq!(ranged.head.status, 206);
        assert_eq!(
            ranged.head.final_url,
            Url::parse(&format!("{base}/range")).unwrap()
        );
        assert_eq!(collect(ranged.body).await, &BODY[2..6]);

        let chained = t
            .head(&Url::parse(&format!("{base}/chain")).unwrap(), 2)
            .await
            .unwrap();
        assert_eq!(
            chained.final_url,
            Url::parse(&format!("{base}/file")).unwrap()
        );

        let relative = t
            .head(&Url::parse(&format!("{base}/relative")).unwrap(), 3)
            .await
            .unwrap();
        assert_eq!(
            relative.final_url,
            Url::parse(&format!("{base}/file")).unwrap()
        );
    }

    #[tokio::test]
    async fn content_disposition_supplies_filename() {
        let (base, seen) = serve(2).await;
        let t = transport(seen);
        let url = Url::parse(&format!("{base}/attachment")).unwrap();

        assert_eq!(
            t.head(&url, 0).await.unwrap().filename.as_deref(),
            Some("café.txt")
        );
        let response = t.get(&url, None, 1).await.unwrap();
        assert_eq!(response.head.filename.as_deref(), Some("café.txt"));
        assert_eq!(collect(response.body).await, BODY);
    }

    #[tokio::test]
    async fn redirect_loops_are_bounded() {
        let (base, seen) = serve(MAX_REDIRECTS + 1).await;
        let t = transport(seen);
        let url = Url::parse(&format!("{base}/loop")).unwrap();
        let error = t.head(&url, 0).await.unwrap_err();
        assert!(error.to_string().contains("Too many redirects"), "{error}");
    }

    #[tokio::test]
    async fn chunked_bodies_are_decoded() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = tcp.read(&mut buf).await.unwrap();
                assert!(read > 0, "client closed before sending headers");
                request.extend_from_slice(&buf[..read]);
            }
            tcp.write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Disposition: attachment; filename=chunked.txt\r\n\
                  Transfer-Encoding: chunked\r\n\r\n\
                  5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
            tcp.flush().await.unwrap();
        });

        let seen = Arc::new(Mutex::new(Vec::new()));
        let t = transport(seen);
        let url = Url::parse(&format!("http://{addr}/stream")).unwrap();
        let response = t.get(&url, None, 42).await.unwrap();
        assert_eq!(response.head.filename.as_deref(), Some("chunked.txt"));
        assert_eq!(response.head.content_length, None);
        assert_eq!(collect(response.body).await, b"hello world");
        server.await.unwrap();
    }

    #[test]
    fn parses_ranges_and_content_range() {
        assert!(accepts_ranges_value("bytes"));
        assert!(accepts_ranges_value("none, BYTES"));
        assert!(!accepts_ranges_value("none"));
        assert_eq!(total_from_content_range("bytes 0-4/100"), Some(100));
        assert_eq!(total_from_content_range("bytes 0-4/*"), None);
    }

    async fn collect(mut body: ByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }
}

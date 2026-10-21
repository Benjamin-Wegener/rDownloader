//! [`Transport`](crate::contract::Transport) implementation over Tor.
//!
//! Maps contract A (including byte ranges and isolation) onto the
//! Hyper HTTP/1.1 client from [`crate::net`]. Response metadata is derived
//! entirely from HTTP headers; the body stays a stream.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use http_body_util::BodyExt;
use hyper::header::{HeaderMap, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED};
use hyper::StatusCode;
use url::Url;

use crate::contract::{ByteRange, ByteStream, Response, ResponseHead, Transport};
use crate::net::TorHttpClient;
use crate::tor::TorManager;

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
}

#[async_trait]
impl Transport for TorTransport {
    async fn head(&self, url: &Url, isolation: u64) -> Result<ResponseHead> {
        let response = self
            .http
            .head(isolation, url)
            .await
            .context("HEAD over Tor")?;
        let status = response.status();
        if !status.is_success() {
            bail!("HEAD {url}: HTTP {}", status.as_u16());
        }
        head_from(status, response.headers(), url)
    }

    async fn get(&self, url: &Url, range: Option<ByteRange>, isolation: u64) -> Result<Response> {
        let requested = range.map(|r| (r.start, r.end));
        let response = self
            .http
            .get(isolation, url, requested)
            .await
            .context("GET over Tor")?;
        let status = response.status();
        if !status.is_success() {
            bail!("GET {url}: HTTP {}", status.as_u16());
        }

        let (parts, body) = response.into_parts();
        let head = head_from(parts.status, &parts.headers, url)?;
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

    Ok(ResponseHead {
        status: status.as_u16(),
        final_url: url.clone(),
        content_length,
        total_size: content_range.or(content_length),
        accepts_ranges,
        // Content-Disposition is handled in net-3.
        filename: None,
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
            _ => HttpResponse::builder()
                .status(404)
                .body(Full::new(Bytes::from_static(b"nope")))
                .unwrap(),
        };
        Ok(response)
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

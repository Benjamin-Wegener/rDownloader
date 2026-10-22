//! Headless control API: a small JSON-RPC 2.0 and REST server.
//!
//! The server is transport-only and does not know how downloads are executed.
//! It forwards every command to a [`JobControl`] implementation and renders the
//! shared progress state as JSON. The daemon in `main.rs` provides that
//! implementation, so the TUI and the headless API use the same job queue.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN, TRANSFER_ENCODING,
    WWW_AUTHENTICATE,
};
use hyper::http::uri::Authority;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info};
use uuid::Uuid;

use crate::download::JobProgress;
use crate::t;

/// Maximum accepted request body (JSON commands are tiny).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Headers sent with every response. No CORS header is set on purpose: pages on
/// other origins must not be able to read responses, even with a token.
const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    (
        "content-security-policy",
        "default-src 'none'; frame-ancestors 'none'",
    ),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cross-origin-resource-policy", "same-origin"),
];

/// Operations the RPC server can invoke on the download daemon.
#[async_trait]
pub trait JobControl: Send + Sync {
    fn list_jobs(&self) -> Vec<JobProgress>;
    fn job(&self, id: Uuid) -> Option<JobProgress>;
    async fn add_job(&self, url: &str) -> Result<Uuid, String>;
    fn pause_job(&self, id: Uuid) -> Result<(), String>;
    fn resume_job(&self, id: Uuid) -> Result<(), String>;
    fn cancel_job(&self, id: Uuid) -> Result<(), String>;
    fn remove_job(&self, id: Uuid) -> Result<(), String>;
}

type ApiBody = BoxBody<Bytes, Infallible>;
type ApiResult = Result<Response<ApiBody>, (StatusCode, String)>;

/// Binds `addr` and serves the control API until the process exits.
pub async fn serve(
    addr: SocketAddr,
    token: Option<String>,
    control: Arc<dyn JobControl>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| t!("main.error.rpc_bind", addr = addr))?;
    serve_on(listener, token, control).await
}

async fn serve_on(
    listener: TcpListener,
    token: Option<String>,
    control: Arc<dyn JobControl>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    info!("RPC listening on http://{addr}");
    loop {
        let (stream, peer) = listener.accept().await?;
        let control = control.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let control = control.clone();
                let token = token.clone();
                handle(req, control, token, addr)
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                debug!("RPC connection from {peer} ended: {e}");
            }
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    control: Arc<dyn JobControl>,
    token: Option<String>,
    local: SocketAddr,
) -> Result<Response<ApiBody>, Infallible> {
    let res = match check_guards(&req, local) {
        Err(error) => Err(error),
        Ok(()) if !authorized(&req, token.as_deref()) => {
            Err((StatusCode::UNAUTHORIZED, t!("rpc.error.unauthorized")))
        }
        Ok(()) => dispatch(req, control).await,
    };
    Ok(match res {
        Ok(response) => response,
        Err((status, message)) => api_error(status, &message),
    })
}

/// Rejects requests a foreign web page could produce: DNS rebinding (foreign
/// `Host`), cross-site form/fetch posts (`Origin`), and non-JSON payloads.
/// Clients such as `curl` send no `Origin`, so they keep working.
fn check_guards(req: &Request<Incoming>, local: SocketAddr) -> Result<(), (StatusCode, String)> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    if !host_is_local(host, local) {
        return Err((StatusCode::FORBIDDEN, t!("rpc.error.invalid_host")));
    }

    if !is_state_changing(req.method()) {
        return Ok(());
    }

    if let Some(origin) = req.headers().get(ORIGIN) {
        let origin = origin
            .to_str()
            .map_err(|_| (StatusCode::FORBIDDEN, t!("rpc.error.cross_origin")))?;
        if !origin_matches(origin, host.unwrap_or_default()) {
            return Err((StatusCode::FORBIDDEN, t!("rpc.error.cross_origin")));
        }
    }

    if has_body(req) && !is_json_content_type(req.headers().get(CONTENT_TYPE)) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            t!("rpc.error.unsupported_media_type"),
        ));
    }

    Ok(())
}

fn is_state_changing(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn has_body(req: &Request<Incoming>) -> bool {
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > 0);
    declared || req.headers().contains_key(TRANSFER_ENCODING)
}

fn is_json_content_type(value: Option<&hyper::header::HeaderValue>) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// A request is local when its `Host` names `localhost`, a loopback address or
/// the concrete address the server is bound to. Host names are never accepted:
/// that is what stops DNS rebinding.
fn host_is_local(host_header: Option<&str>, local: SocketAddr) -> bool {
    let Some(host) = host_header.and_then(authority_host) else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback() || (ip == local.ip() && !local.ip().is_unspecified()),
        Err(_) => false,
    }
}

/// Checks that an `Origin` header points at the same local authority as `Host`.
fn origin_matches(origin: &str, host_header: &str) -> bool {
    let Ok(uri) = origin.parse::<hyper::Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("http") {
        return false;
    }
    let (Some(origin_host), Some(host)) = (uri.host(), authority_host(host_header)) else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(&host)
        && uri.port_u16().unwrap_or(80) == authority_port(host_header)
}

fn authority_host(value: &str) -> Option<String> {
    if value.contains('@') {
        return None;
    }
    let authority: Authority = value.parse().ok()?;
    Some(
        authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
    )
}

fn authority_port(value: &str) -> u16 {
    value
        .parse::<Authority>()
        .ok()
        .and_then(|authority| authority.port_u16())
        .unwrap_or(80)
}

fn authorized(req: &Request<Incoming>, token: Option<&str>) -> bool {
    match token {
        None => true,
        Some(expected) => req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| value == expected),
    }
}

async fn dispatch(req: Request<Incoming>, control: Arc<dyn JobControl>) -> ApiResult {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    match (method.clone(), path.as_str()) {
        (Method::GET, "/") => Ok(json_response(
            StatusCode::OK,
            &json!({
                "name": "rDownloader",
                "version": env!("CARGO_PKG_VERSION"),
                "api": "json-rpc-2.0",
                "endpoints": [
                    "/health", "/api/jobs", "/api/jobs/{id}", "/api/events", "/rpc"
                ],
            }),
        )),
        (Method::GET, "/health") => Ok(json_response(StatusCode::OK, &json!({"status": "ok"}))),
        (Method::GET, "/api/jobs") => Ok(json_response(
            StatusCode::OK,
            &serde_json::to_value(control.list_jobs()).unwrap_or_else(|_| json!([])),
        )),
        (Method::GET, "/api/events") => Ok(events_response(control)),
        (Method::POST, "/api/jobs") => add_job(req, control.as_ref()).await,
        (Method::POST, "/rpc") => handle_rpc(req, control.as_ref()).await,
        _ if path.starts_with("/api/jobs/") => {
            handle_job_route(method, &path, control.as_ref()).await
        }
        _ => Err((StatusCode::NOT_FOUND, t!("rpc.error.not_found_path"))),
    }
}

async fn add_job(req: Request<Incoming>, control: &dyn JobControl) -> ApiResult {
    let params: UrlParams = read_json(req).await?;
    match control.add_job(&params.url).await {
        Ok(id) => Ok(json_response(
            StatusCode::CREATED,
            &json!({"id": id, "ok": true}),
        )),
        Err(message) => Err((StatusCode::BAD_REQUEST, message)),
    }
}

async fn handle_job_route(method: Method, path: &str, control: &dyn JobControl) -> ApiResult {
    let rest = path.trim_start_matches("/api/jobs/");
    let mut parts = rest.split('/');
    let id_text = parts.next().unwrap_or_default();
    let action = parts.next();
    let id = Uuid::parse_str(id_text).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            t!("rpc.error.invalid_job_id", id = id_text),
        )
    })?;

    match (method, action) {
        (Method::GET, None) => match control.job(id) {
            Some(job) => Ok(json_response(
                StatusCode::OK,
                &serde_json::to_value(job).unwrap_or(Value::Null),
            )),
            None => Err(not_found(id)),
        },
        (Method::DELETE, None) => command(control.remove_job(id)),
        (Method::POST, Some("pause")) => command(control.pause_job(id)),
        (Method::POST, Some("resume")) => command(control.resume_job(id)),
        (Method::POST, Some("cancel")) => command(control.cancel_job(id)),
        _ => Err((StatusCode::NOT_FOUND, t!("rpc.error.not_found_path"))),
    }
}

fn command(result: Result<(), String>) -> ApiResult {
    match result {
        Ok(()) => Ok(json_response(StatusCode::OK, &json!({"ok": true}))),
        Err(message) => Err((StatusCode::NOT_FOUND, message)),
    }
}

fn not_found(id: Uuid) -> (StatusCode, String) {
    (
        StatusCode::NOT_FOUND,
        t!("rpc.error.job_not_found", id = id),
    )
}

async fn handle_rpc(req: Request<Incoming>, control: &dyn JobControl) -> ApiResult {
    let call: RpcRequest = read_json(req).await?;
    let RpcRequest { id, method, params } = call;

    match method.as_str() {
        "health" => Ok(rpc_ok(id, json!({"status": "ok"}))),
        "jobs.list" => Ok(rpc_ok(
            id,
            serde_json::to_value(control.list_jobs()).unwrap_or_else(|_| json!([])),
        )),
        "jobs.get" => {
            let params: IdParams = match parse_params(params) {
                Ok(p) => p,
                Err(message) => return Ok(rpc_error(id, -32602, &message)),
            };
            match control.job(params.id) {
                Some(job) => Ok(rpc_ok(id, serde_json::to_value(job).unwrap_or(Value::Null))),
                None => Ok(rpc_error(
                    id,
                    -32004,
                    &t!("rpc.error.job_not_found", id = params.id),
                )),
            }
        }
        "jobs.add" => {
            let params: UrlParams = match parse_params(params) {
                Ok(p) => p,
                Err(message) => return Ok(rpc_error(id, -32602, &message)),
            };
            match control.add_job(&params.url).await {
                Ok(job_id) => Ok(rpc_ok(id, json!({"id": job_id, "ok": true}))),
                Err(message) => Ok(rpc_error(id, -32000, &message)),
            }
        }
        "jobs.pause" | "jobs.resume" | "jobs.cancel" | "jobs.remove" => {
            let params: IdParams = match parse_params(params) {
                Ok(p) => p,
                Err(message) => return Ok(rpc_error(id, -32602, &message)),
            };
            let result = match method.as_str() {
                "jobs.pause" => control.pause_job(params.id),
                "jobs.resume" => control.resume_job(params.id),
                "jobs.cancel" => control.cancel_job(params.id),
                _ => control.remove_job(params.id),
            };
            match result {
                Ok(()) => Ok(rpc_ok(id, json!({"ok": true}))),
                Err(message) => Ok(rpc_error(id, -32000, &message)),
            }
        }
        _ => Ok(rpc_error(
            id,
            -32601,
            &t!("rpc.error.unknown_method", method = method),
        )),
    }
}

#[derive(Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct UrlParams {
    url: String,
}

#[derive(Deserialize)]
struct IdParams {
    id: Uuid,
}

fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, String> {
    serde_json::from_value(params).map_err(|_| t!("rpc.error.invalid_params"))
}

fn events_response(control: Arc<dyn JobControl>) -> Response<ApiBody> {
    // `retry` tells EventSource clients how quickly to reconnect. The first event
    // carries the current queue, so a client needs no separate GET.
    let body = event_stream(control);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no");
    for (name, value) in SECURITY_HEADERS {
        response = response.header(*name, *value);
    }
    response.body(body).expect("valid event-stream response")
}

/// Streams the job list whenever it changes.
///
/// The server polls the shared queue instead of requiring every producer to
/// publish events; the observable behaviour is the same and the daemon keeps a
/// single source of truth.
fn event_stream(control: Arc<dyn JobControl>) -> ApiBody {
    let (tx, mut rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(8);
    tokio::spawn(async move {
        let snapshot =
            || serde_json::to_string(&control.list_jobs()).unwrap_or_else(|_| "[]".to_string());
        let mut last = snapshot();
        let first = format!("retry: 3000\n\nevent: jobs\ndata: {last}\n\n");
        if tx.send(Ok(Frame::data(Bytes::from(first)))).await.is_err() {
            return;
        }

        let mut keep_alive = tokio::time::interval(Duration::from_secs(15));
        keep_alive.tick().await;
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut poll = tokio::time::interval(Duration::from_millis(400));
        poll.tick().await;

        loop {
            tokio::select! {
                _ = keep_alive.tick() => {
                    if tx
                        .send(Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n"))))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ = poll.tick() => {
                    let current = snapshot();
                    if current != last {
                        last = current;
                        let event = format!("event: jobs\ndata: {last}\n\n");
                        if tx.send(Ok(Frame::data(Bytes::from(event)))).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    StreamBody::new(stream).boxed()
}

fn rpc_ok(id: Value, result: Value) -> Response<ApiBody> {
    json_response(
        StatusCode::OK,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response<ApiBody> {
    json_response(
        StatusCode::OK,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    )
}

fn api_error(status: StatusCode, message: &str) -> Response<ApiBody> {
    let mut response = json_response(status, &json!({"error": message}));
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, "Bearer".parse().expect("valid header"));
    }
    response
}

fn json_response(status: StatusCode, value: &Value) -> Response<ApiBody> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .header(CACHE_CONTROL, "no-store")
        .header("Connection", "close");
    for (name, value) in SECURITY_HEADERS {
        response = response.header(*name, *value);
    }
    response
        .body(Full::new(Bytes::from(body)).boxed())
        .expect("valid response")
}

async fn read_json<T: DeserializeOwned>(req: Request<Incoming>) -> Result<T, (StatusCode, String)> {
    let mut body = req.into_body();
    let mut buf = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|_| (StatusCode::BAD_REQUEST, t!("rpc.error.invalid_request")))?;
        if let Some(data) = frame.data_ref() {
            if buf.len() + data.len() > MAX_BODY_BYTES {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    t!("rpc.error.body_too_large"),
                ));
            }
            buf.extend_from_slice(data);
        }
    }
    serde_json::from_slice(&buf)
        .map_err(|_| (StatusCode::BAD_REQUEST, t!("rpc.error.invalid_request")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::JobStatus;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    struct MockControl {
        jobs: Mutex<Vec<JobProgress>>,
    }

    impl MockControl {
        fn new() -> Self {
            Self {
                jobs: Mutex::new(vec![sample_job(Uuid::new_v4())]),
            }
        }
    }

    fn sample_job(id: Uuid) -> JobProgress {
        JobProgress {
            id,
            url: "https://example.com/file.bin".into(),
            filename: "file.bin".into(),
            total_bytes: 1024,
            bytes_done: 0,
            parts: Vec::new(),
            status: JobStatus::Queued,
        }
    }

    #[async_trait]
    impl JobControl for MockControl {
        fn list_jobs(&self) -> Vec<JobProgress> {
            self.jobs.lock().unwrap().clone()
        }

        fn job(&self, id: Uuid) -> Option<JobProgress> {
            self.jobs
                .lock()
                .unwrap()
                .iter()
                .find(|job| job.id == id)
                .cloned()
        }

        async fn add_job(&self, url: &str) -> Result<Uuid, String> {
            if url.is_empty() {
                return Err("empty URL".into());
            }
            let id = Uuid::new_v4();
            let mut job = sample_job(id);
            job.url = url.to_string();
            self.jobs.lock().unwrap().push(job);
            Ok(id)
        }

        fn pause_job(&self, id: Uuid) -> Result<(), String> {
            self.set(id, JobStatus::Paused)
        }

        fn resume_job(&self, id: Uuid) -> Result<(), String> {
            self.set(id, JobStatus::Downloading)
        }

        fn cancel_job(&self, id: Uuid) -> Result<(), String> {
            self.set(id, JobStatus::Cancelled)
        }

        fn remove_job(&self, id: Uuid) -> Result<(), String> {
            let mut jobs = self.jobs.lock().unwrap();
            let before = jobs.len();
            jobs.retain(|job| job.id != id);
            if jobs.len() == before {
                return Err(format!("job not found: {id}"));
            }
            Ok(())
        }
    }

    impl MockControl {
        fn set(&self, id: Uuid, status: JobStatus) -> Result<(), String> {
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or_else(|| format!("job not found: {id}"))?;
            job.status = status;
            Ok(())
        }
    }

    async fn start(token: Option<&str>) -> (SocketAddr, Arc<MockControl>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let control = Arc::new(MockControl::new());
        let control_for_server: Arc<dyn JobControl> = control.clone();
        let token = token.map(str::to_string);
        tokio::spawn(async move {
            let _ = serve_on(listener, token, control_for_server).await;
        });
        (addr, control)
    }

    async fn request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String) {
        let mut extra = Vec::new();
        if let Some(token) = token {
            extra.push(("Authorization", format!("Bearer {token}")));
        }
        let extra: Vec<(&str, &str)> = extra.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let (status, _, response_body) = raw_request(
            addr,
            method,
            path,
            body,
            "localhost",
            &extra,
            Some("application/json"),
        )
        .await;
        (status, response_body)
    }

    #[allow(clippy::too_many_arguments)]
    async fn raw_request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        body: Option<&str>,
        host: &str,
        extra: &[(&str, &str)],
        content_type: Option<&str>,
    ) -> (u16, String, String) {
        let body = body.unwrap_or_default();
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(content_type) = content_type {
            request.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        for (name, value) in extra {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response
            .split_once("\r\n\r\n")
            .unwrap_or((response.as_str(), ""));
        let status = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, headers.to_string(), body.to_string())
    }

    #[tokio::test]
    async fn rest_lists_adds_and_controls_jobs() {
        let (addr, control) = start(None).await;

        let (status, body) = request(addr, "GET", "/api/jobs", None, None).await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&body)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let (status, body) = request(
            addr,
            "POST",
            "/api/jobs",
            Some(r#"{"url":"https://example.com/new.bin"}"#),
            None,
        )
        .await;
        assert_eq!(status, 201);
        let id: Uuid = serde_json::from_str::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        let (status, body) = request(addr, "GET", &format!("/api/jobs/{id}"), None, None).await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["url"],
            "https://example.com/new.bin"
        );

        let (status, _) = request(addr, "POST", &format!("/api/jobs/{id}/pause"), None, None).await;
        assert_eq!(status, 200);
        assert_eq!(control.job(id).unwrap().status, JobStatus::Paused);

        let (status, _) = request(addr, "DELETE", &format!("/api/jobs/{id}"), None, None).await;
        assert_eq!(status, 200);
        assert!(control.job(id).is_none());

        let (status, _) = request(addr, "GET", &format!("/api/jobs/{id}"), None, None).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn json_rpc_roundtrip() {
        let (addr, control) = start(None).await;

        let (status, body) = request(
            addr,
            "POST",
            "/rpc",
            Some(r#"{"jsonrpc":"2.0","id":1,"method":"jobs.list"}"#),
            None,
        )
        .await;
        assert_eq!(status, 200);
        let response: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"].as_array().unwrap().len(), 1);

        let (_, body) = request(
            addr,
            "POST",
            "/rpc",
            Some(r#"{"jsonrpc":"2.0","id":2,"method":"jobs.add","params":{"url":"https://example.com/rpc.bin"}}"#),
            None,
        )
        .await;
        let id: Uuid = serde_json::from_str::<Value>(&body).unwrap()["result"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        let (_, body) = request(
            addr,
            "POST",
            "/rpc",
            Some(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"jobs.cancel","params":{{"id":"{id}"}}}}"#
            )),
            None,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["result"]["ok"],
            true
        );
        assert_eq!(control.job(id).unwrap().status, JobStatus::Cancelled);

        let (_, body) = request(
            addr,
            "POST",
            "/rpc",
            Some(r#"{"jsonrpc":"2.0","id":4,"method":"no.such.method"}"#),
            None,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32601
        );

        let (_, body) = request(
            addr,
            "POST",
            "/rpc",
            Some(r#"{"jsonrpc":"2.0","id":5,"method":"jobs.get","params":{}}"#),
            None,
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32602
        );
    }

    #[tokio::test]
    async fn bearer_token_is_enforced() {
        let (addr, _) = start(Some("secret")).await;

        let (status, _) = request(addr, "GET", "/api/jobs", None, None).await;
        assert_eq!(status, 401);

        let (status, _) = request(addr, "GET", "/api/jobs", None, Some("wrong")).await;
        assert_eq!(status, 401);

        let (status, _) = request(addr, "GET", "/api/jobs", None, Some("secret")).await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn invalid_job_id_is_rejected() {
        let (addr, _) = start(None).await;
        let (status, _) = request(addr, "GET", "/api/jobs/not-a-uuid", None, None).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn foreign_host_headers_are_rejected() {
        let (addr, _) = start(None).await;
        for host in [
            "evil.example",
            "evil.example:8765",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "attacker@127.0.0.1:8765",
        ] {
            let (status, _, _) = raw_request(addr, "GET", "/api/jobs", None, host, &[], None).await;
            assert_eq!(status, 403, "Host {host} must be rejected");
        }

        for host in [
            "localhost".to_string(),
            format!("localhost:{}", addr.port()),
            format!("127.0.0.1:{}", addr.port()),
        ] {
            let (status, _, _) =
                raw_request(addr, "GET", "/api/jobs", None, &host, &[], None).await;
            assert_eq!(status, 200, "Host {host} must be accepted");
        }
    }

    #[tokio::test]
    async fn cross_origin_changes_are_rejected() {
        let (addr, _) = start(None).await;
        let body = r#"{"url":"https://example.com/csrf.bin"}"#;

        for origin in [
            "https://evil.example",
            "http://evil.example",
            "http://localhost:9999",
            "null",
        ] {
            let (status, _, _) = raw_request(
                addr,
                "POST",
                "/api/jobs",
                Some(body),
                "localhost",
                &[("Origin", origin)],
                Some("application/json"),
            )
            .await;
            assert_eq!(status, 403, "Origin {origin} must be rejected");
        }

        let (status, _, _) = raw_request(
            addr,
            "POST",
            "/api/jobs",
            Some(body),
            "localhost",
            &[("Origin", "http://localhost")],
            Some("application/json"),
        )
        .await;
        assert_eq!(status, 201);

        let (status, _, _) = raw_request(
            addr,
            "GET",
            "/api/jobs",
            None,
            "localhost",
            &[("Origin", "https://evil.example")],
            None,
        )
        .await;
        assert_eq!(status, 200, "GET requests are not blocked by Origin");
    }

    #[tokio::test]
    async fn non_json_payloads_are_rejected() {
        let (addr, _) = start(None).await;
        let body = r#"{"url":"https://example.com/form.bin"}"#;

        let (status, _, _) = raw_request(
            addr,
            "POST",
            "/api/jobs",
            Some(body),
            "localhost",
            &[],
            Some("text/plain"),
        )
        .await;
        assert_eq!(status, 415);

        let (status, _, _) = raw_request(
            addr,
            "POST",
            "/api/jobs",
            Some(body),
            "localhost",
            &[],
            None,
        )
        .await;
        assert_eq!(status, 415);

        let (status, _, _) = raw_request(
            addr,
            "POST",
            "/api/jobs",
            Some(body),
            "localhost",
            &[],
            Some("application/json; charset=utf-8"),
        )
        .await;
        assert_eq!(status, 201);

        let (status, _, _) = raw_request(
            addr,
            "POST",
            "/rpc",
            Some(r#"{"jsonrpc":"2.0","id":1,"method":"jobs.list"}"#),
            "localhost",
            &[],
            Some("application/x-www-form-urlencoded"),
        )
        .await;
        assert_eq!(status, 415);

        let id = Uuid::new_v4();
        let (status, _, _) = raw_request(
            addr,
            "POST",
            &format!("/api/jobs/{id}/pause"),
            None,
            "localhost",
            &[],
            None,
        )
        .await;
        assert_eq!(status, 404, "empty POST without Content-Type is allowed");
    }

    #[tokio::test]
    async fn events_stream_the_queue_and_follow_changes() {
        let (addr, control) = start(None).await;
        let id = control.list_jobs()[0].id;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /api/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let first =
            tokio::time::timeout(Duration::from_secs(2), read_until(&mut stream, "file.bin"))
                .await
                .expect("initial event arrives");
        assert!(first.contains("200 OK"), "{first}");
        assert!(first.contains("text/event-stream"), "{first}");
        assert!(first.contains("event: jobs"), "{first}");

        control.pause_job(id).unwrap();
        let update =
            tokio::time::timeout(Duration::from_secs(3), read_until(&mut stream, "Paused"))
                .await
                .expect("changed job is pushed");
        assert!(update.contains("event: jobs"), "{update}");
    }

    #[tokio::test]
    async fn events_require_the_bearer_token() {
        let (addr, _control) = start(Some("secret")).await;
        let (status, _) = request(addr, "GET", "/api/events", None, None).await;
        assert_eq!(status, 401);
    }

    async fn read_until(stream: &mut tokio::net::TcpStream, needle: &str) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                return String::from_utf8_lossy(&buf).into_owned();
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if text.contains(needle) {
                return text.into_owned();
            }
        }
    }

    #[tokio::test]
    async fn security_headers_are_sent() {
        let (addr, _) = start(None).await;
        let (status, headers, _) =
            raw_request(addr, "GET", "/health", None, "localhost", &[], None).await;
        assert_eq!(status, 200);
        let headers = headers.to_ascii_lowercase();
        assert!(headers.contains("x-content-type-options: nosniff"));
        assert!(headers.contains("x-frame-options: deny"));
        assert!(headers.contains("referrer-policy: no-referrer"));
        assert!(headers.contains("content-security-policy: default-src 'none'"));
        assert!(headers.contains("cross-origin-resource-policy: same-origin"));
        assert!(!headers.contains("access-control-allow-origin"));
    }
}

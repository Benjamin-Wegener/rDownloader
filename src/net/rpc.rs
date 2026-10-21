//! Headless control API: a small JSON-RPC 2.0 and REST server.
//!
//! The server is transport-only and does not know how downloads are executed.
//! It forwards every command to a [`JobControl`] implementation and renders the
//! shared progress state as JSON. The daemon in `main.rs` provides that
//! implementation, so the TUI and the headless API use the same job queue.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::{debug, info};
use uuid::Uuid;

use crate::download::JobProgress;
use crate::t;

/// Maximum accepted request body (JSON commands are tiny).
const MAX_BODY_BYTES: usize = 1024 * 1024;

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

type ApiResult = Result<Response<Full<Bytes>>, (StatusCode, String)>;

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
                handle(req, control, token)
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
) -> Result<Response<Full<Bytes>>, Infallible> {
    let res = match authorized(&req, token.as_deref()) {
        false => Err((StatusCode::UNAUTHORIZED, t!("rpc.error.unauthorized"))),
        true => dispatch(req, control).await,
    };
    Ok(match res {
        Ok(response) => response,
        Err((status, message)) => api_error(status, &message),
    })
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
                "name": "rdownloader",
                "version": env!("CARGO_PKG_VERSION"),
                "api": "json-rpc-2.0",
                "endpoints": ["/health", "/api/jobs", "/api/jobs/{id}", "/rpc"],
            }),
        )),
        (Method::GET, "/health") => Ok(json_response(StatusCode::OK, &json!({"status": "ok"}))),
        (Method::GET, "/api/jobs") => Ok(json_response(
            StatusCode::OK,
            &serde_json::to_value(control.list_jobs()).unwrap_or_else(|_| json!([])),
        )),
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

fn rpc_ok(id: Value, result: Value) -> Response<Full<Bytes>> {
    json_response(
        StatusCode::OK,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response<Full<Bytes>> {
    json_response(
        StatusCode::OK,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    )
}

fn api_error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let mut response = json_response(status, &json!({"error": message}));
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, "Bearer".parse().expect("valid header"));
    }
    response
}

fn json_response(status: StatusCode, value: &Value) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .header(CACHE_CONTROL, "no-store")
        .header("Connection", "close")
        .body(Full::new(Bytes::from(body)))
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
        let body = body.unwrap_or_default();
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\n",
            body.len()
        );
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let status = response
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
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
}

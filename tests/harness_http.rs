//! Tests for the test harness (local HTTP server with range support).
//!
//! The harness is the basis for the later integration tests
//! (kill + resume + hash).

mod common;

use common::{fetch, ServerConfig, TestServer, PATH_FILE, PATH_TRUNCATED};

fn payload() -> Vec<u8> {
    (0u8..=255).cycle().take(4096).collect()
}

#[tokio::test]
async fn serves_full_body() {
    let body = payload();
    let server = TestServer::start(ServerConfig::new(body.clone())).await;

    let resp = fetch(server.addr(), PATH_FILE, None).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, body);
    assert_eq!(resp.header("accept-ranges"), Some("bytes"));
}

#[tokio::test]
async fn range_request_returns_206_with_content_range() {
    let body = payload();
    let server = TestServer::start(ServerConfig::new(body.clone())).await;

    let resp = fetch(server.addr(), PATH_FILE, Some((100, Some(199))))
        .await
        .unwrap();
    assert_eq!(resp.status, 206);
    assert_eq!(resp.body, body[100..=199]);
    assert_eq!(
        resp.header("content-range"),
        Some(format!("bytes 100-199/{}", body.len()).as_str())
    );
}

#[tokio::test]
async fn open_range_runs_to_the_end() {
    let body = payload();
    let server = TestServer::start(ServerConfig::new(body.clone())).await;

    let resp = fetch(server.addr(), PATH_FILE, Some((4000, None)))
        .await
        .unwrap();
    assert_eq!(resp.status, 206);
    assert_eq!(resp.body, body[4000..]);
}

#[tokio::test]
async fn range_outside_the_file_returns_416() {
    let server = TestServer::start(ServerConfig::new(payload())).await;

    let resp = fetch(server.addr(), PATH_FILE, Some((999_999, None)))
        .await
        .unwrap();
    assert_eq!(resp.status, 416);
}

#[tokio::test]
async fn without_accept_ranges_server_ignores_range() {
    let body = payload();
    let server = TestServer::start(ServerConfig::new(body.clone()).without_ranges()).await;

    let resp = fetch(server.addr(), PATH_FILE, Some((100, Some(199))))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, body);
    assert_eq!(resp.header("accept-ranges"), None);
}

#[tokio::test]
async fn etag_and_content_disposition_are_sent() {
    let server = TestServer::start(ServerConfig::new(payload()).with_etag("\"abc123\"")).await;

    let resp = fetch(server.addr(), PATH_FILE, None).await.unwrap();
    assert_eq!(resp.header("etag"), Some("\"abc123\""));
}

#[tokio::test]
async fn abort_mid_body_is_detected_as_eof() {
    let server = TestServer::start(ServerConfig::new(payload()).aborting_after(100)).await;

    let err = fetch(server.addr(), PATH_TRUNCATED, None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn requests_are_recorded() {
    let server = TestServer::start(ServerConfig::new(payload())).await;

    fetch(server.addr(), PATH_FILE, Some((0, Some(9))))
        .await
        .unwrap();
    fetch(server.addr(), PATH_FILE, None).await.unwrap();

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].path, PATH_FILE);
    assert_eq!(reqs[0].range, Some((0, Some(9))));
    assert_eq!(reqs[1].range, None);
    assert!(reqs.iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn unknown_path_returns_404() {
    let server = TestServer::start(ServerConfig::new(payload())).await;

    let resp = fetch(server.addr(), "/nope", None).await.unwrap();
    assert_eq!(resp.status, 404);
}

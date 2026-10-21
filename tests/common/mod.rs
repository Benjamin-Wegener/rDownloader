//! Test-Harness: lokaler HTTP-Server mit Range-Support und Abbruch-Simulation.
//!
//! Bewusst abhängigkeitsfrei (nur `tokio`), damit es von jedem Test in
//! `tests/` genutzt werden kann und keine zusätzlichen Crates braucht.
//!
//! Der Server bietet:
//! - `HEAD`/`GET` mit `Accept-Ranges: bytes` und `Range`-Unterstützung (206),
//! - `ETag`/`Last-Modified`/`Content-Disposition`,
//! - Abbruch-Simulation: die Verbindung wird mitten im Body abgerissen,
//! - Aufzeichnung aller Anfragen für Assertions.

#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Eine vom Server empfangene Anfrage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// `Range`-Header als `(start, end)`; `end == None` heißt "bis zum Ende".
    pub range: Option<(u64, Option<u64>)>,
}

/// Konfiguration des Test-Servers.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub body: Vec<u8>,
    /// `Accept-Ranges: bytes` senden und Range-Anfragen beantworten.
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub filename: Option<String>,
    /// Nach wie vielen Bytes der Body abgerissen wird (Abbruch-Simulation).
    /// `None` = kein Abbruch.
    pub abort_after: Option<usize>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            body: Vec::new(),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            filename: None,
            abort_after: None,
        }
    }
}

impl ServerConfig {
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            body: body.into(),
            ..Default::default()
        }
    }

    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }

    pub fn without_ranges(mut self) -> Self {
        self.accept_ranges = false;
        self
    }

    pub fn aborting_after(mut self, bytes: usize) -> Self {
        self.abort_after = Some(bytes);
        self
    }
}

/// Laufender Test-Server.
pub struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl TestServer {
    /// Startet den Server auf einem freien Loopback-Port.
    pub async fn start(config: ServerConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let config = Arc::new(config);
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));

        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let cfg = config.clone();
                let reqs = reqs.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(sock, cfg, reqs).await;
                });
            }
        });

        Self { addr, requests }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Absolute URL für einen Pfad, z. B. `http://127.0.0.1:1234/file`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Alle bisher empfangenen Anfragen.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

async fn handle_connection(
    mut sock: TcpStream,
    config: Arc<ServerConfig>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
) -> io::Result<()> {
    let head = read_head(&mut sock).await?;
    let Some(head) = head else {
        return Ok(());
    };

    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut range = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("range") {
                range = parse_range(value.trim());
            }
        }
    }

    requests
        .lock()
        .expect("requests lock")
        .push(RecordedRequest {
            method: method.clone(),
            path: path.clone(),
            range,
        });

    if path != PATH_FILE && path != PATH_TRUNCATED {
        sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    let total = config.body.len() as u64;
    let mut extra = String::new();
    if config.accept_ranges {
        extra.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(etag) = &config.etag {
        extra.push_str(&format!("ETag: {etag}\r\n"));
    }
    if let Some(lm) = &config.last_modified {
        extra.push_str(&format!("Last-Modified: {lm}\r\n"));
    }
    if let Some(name) = &config.filename {
        extra.push_str(&format!(
            "Content-Disposition: attachment; filename=\"{name}\"\r\n"
        ));
    }

    // Range auswerten.
    let (status, start, end) = match (config.accept_ranges, range) {
        (true, Some((start, end))) => {
            let end = end
                .unwrap_or(total.saturating_sub(1))
                .min(total.saturating_sub(1));
            if start >= total {
                (416, 0, 0)
            } else {
                (206, start, end)
            }
        }
        _ => (200, 0, total.saturating_sub(1)),
    };

    if status == 416 {
        sock.write_all(
            format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
        return Ok(());
    }

    let slice = if total == 0 {
        &config.body[0..0]
    } else {
        &config.body[start as usize..=end as usize]
    };

    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n",
        if status == 206 {
            "Partial Content"
        } else {
            "OK"
        },
        slice.len()
    );
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
    }
    head.push_str(&extra);
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await?;

    if method.eq_ignore_ascii_case("HEAD") {
        sock.flush().await?;
        return Ok(());
    }

    // Body senden – ggf. mitten im Body abbrechen (Abbruch-Simulation).
    let abort_after = if path == PATH_TRUNCATED {
        Some(config.abort_after.unwrap_or(slice.len() / 2))
    } else {
        None
    };
    if let Some(limit) = abort_after {
        let limit = limit.min(slice.len());
        sock.write_all(&slice[..limit]).await?;
        sock.flush().await?;
        // Socket schließen, ohne den Rest zu senden → der Client sieht einen
        // unvollständigen Body.
        drop(sock);
        return Ok(());
    }

    sock.write_all(slice).await?;
    sock.flush().await?;
    Ok(())
}

/// Liest den HTTP-Request-Kopf (bis `\r\n\r\n`).
async fn read_head(sock: &mut TcpStream) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
    }
}

/// Parst `bytes=start-end` / `bytes=start-`.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => None,
        other => Some(other.parse().ok()?),
    };
    Some((start, end))
}

/// Pfad, der den vollen Body liefert.
pub const PATH_FILE: &str = "/file";
/// Pfad, bei dem die Verbindung mitten im Body abgerissen wird.
pub const PATH_TRUNCATED: &str = "/truncated";

/// Antwort eines Roh-HTTP-Client-Requests.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Minimaler HTTP-Client für die Tests (kein zusätzlicher Crate).
///
/// Bricht der Server die Verbindung mitten im Body ab, wird ein Fehler
/// (`UnexpectedEof`) zurückgegeben – genau das Verhalten, gegen das der
/// Download-Kern getestet werden soll.
pub async fn fetch(
    addr: SocketAddr,
    path: &str,
    range: Option<(u64, Option<u64>)>,
) -> io::Result<RawResponse> {
    let mut sock = TcpStream::connect(addr).await?;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some((start, end)) = range {
        match end {
            Some(end) => req.push_str(&format!("Range: bytes={start}-{end}\r\n")),
            None => req.push_str(&format!("Range: bytes={start}-\r\n")),
        }
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).await?;
    sock.flush().await?;

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await?;
    if raw.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty response",
        ));
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header end"))?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    // Unvollständiger Body => Abbruch erkannt.
    let expected = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());
    if let Some(expected) = expected {
        if body.len() < expected {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("body truncated: got {} of {} bytes", body.len(), expected),
            ));
        }
    }

    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

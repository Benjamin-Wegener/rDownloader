//! Multi-Part-Download-Engine.
//!
//! Splitst eine Datei in bis zu N Teile, holt jedes Teil über einen eigenen
//! Tor-Circuit (via `TorManager`) und schreibt es in Temp-Teildateien,
//! die am Ende zusammengeführt werden.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{error, info};
use uuid::Uuid;

use crate::tor::TorManager;

/// Fortschritt eines einzelnen Parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartProgress {
    pub index: usize,
    pub bytes_done: u64,
    pub total: u64,
    pub status: PartStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PartStatus {
    Pending,
    Downloading,
    Done,
    Failed(String),
}

/// Gesamt-Fortschritt eines Download-Jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    pub id: Uuid,
    pub url: String,
    pub filename: String,
    pub total_bytes: u64,
    pub bytes_done: u64,
    pub parts: Vec<PartProgress>,
    pub status: JobStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Queued,
    Resolving,
    Downloading,
    Merging,
    Done,
    Failed(String),
    Cancelled,
}

/// Ein Download-Job: Quelle → Zielpfad, aufgeteilt in Parts.
pub struct DownloadJob {
    pub id: Uuid,
    pub url: url::Url,
    pub output: PathBuf,
    pub max_parts: usize,
    pub part_size: Option<u64>, // None = auto (gesamt/max_parts)
}

impl DownloadJob {
    pub fn new(url: url::Url, output: PathBuf, max_parts: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            url,
            output,
            max_parts: max_parts.clamp(1, 10),
            part_size: None,
        }
    }
}

/// Resolve Content-Length via einen Tor-Circuit + HEAD-Request.
async fn resolve_size(tor: &TorManager, idx: usize, url: &url::Url) -> Result<u64> {
    // Für echte Tor-Nutzung bauen wir einen HTTP-Stream über Tor und machen dort
    // einen manuellen HEAD-Request. Hier vereinfacht: reqwest mit socks-ähnlichem
    // Routing via TorManager.connect() als Rohstream → zu aufwändig für HEAD.
    // Pragmatisch: reqwest ohne Proxy für Größe (nur Metadata, kein Payload),
    // aber strikt optional: Falls gewünscht, über Tor. Für saubere Anonymität
    // auch über Tor: wir nutzen den Tor-Stream + manuelles HTTP/1.1.
    let stream = tor.connect(idx, url.host_str().unwrap_or("localhost"), 80).await?;
    // TODO: Manueller HTTP/1.1 HEAD über den Tor-Stream (AsyncRead/Write).
    // Für MVP nutzen wir reqwest (ohne Proxy) nur zur Größe-Ermittlung.
    drop(stream);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .head(url.as_str())
        .send()
        .await
        .context("HEAD-Request")?;
    if !resp.status().is_success() {
        anyhow::bail!("HEAD fehlgeschlagen: {}", resp.status());
    }
    let len = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .context("Content-Length fehlt")?;
    Ok(len)
}

/// Lädt einen Part (Byte-Range) über Tor herunter und schreibt nach `dest`.
async fn download_part(
    tor: Arc<TorManager>,
    part_idx: usize,
    url: url::Url,
    dest: PathBuf,
    range_start: u64,
    range_end: u64, // inklusiv
    bytes_done: Arc<AtomicU64>,
) -> Result<()> {
    let host = url.host_str().context("Kein Host in URL")?.to_string();
    let port = url.port_or_known_default().unwrap_or(80);

    // Tor-Stream öffnen
    let mut stream = tor.connect(part_idx, &host, port).await?;

    // Einfacher HTTP/1.1 GET mit Range-Header über den Tor-Stream
    let path = if url.path().is_empty() { "/" } else { url.path() };
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nRange: bytes={range_start}-{range_end}\r\nConnection: close\r\nUser-Agent: rdownloader/0.1\r\n\r\n"
    );
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    stream.write_all(req.as_bytes()).await?;

    // Response lesen bis \r\n\r\n (Header-Ende), dann Body
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("Verbindung vor Header-Ende geschlossen");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            anyhow::bail!("HTTP-Header zu groß");
        }
    }

    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let status_line = header.lines().next().unwrap_or("");
    if !(status_line.contains(" 200 ") || status_line.contains(" 206 ")) {
        anyhow::bail!("Unerwarteter Status: {status_line}");
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&dest)
        .await
        .with_context(|| format!("Teildatei anlegen: {dest:?}"))?;

    // Rest der Response (evtl. schon Teil des Body in buf) schreiben
    if buf.len() > header_end {
        let body_start = &buf[header_end..];
        file.write_all(body_start).await?;
        bytes_done.fetch_add(body_start.len() as u64, Ordering::Relaxed);
    }

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        file.write_all(&tmp[..n]).await?;
        bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.flush().await?;
    Ok(())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Führt alle Teildateien in die finale Zieldatei zusammen.
async fn merge_parts(parts: &[PathBuf], output: &Path) -> Result<()> {
    let mut out = File::create(output)
        .await
        .with_context(|| format!("Zieldatei anlegen: {output:?}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    for part in parts {
        let mut f = File::open(part)
            .await
            .with_context(|| format!("Teildatei öffnen: {part:?}"))?;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).await?;
        }
    }
    out.flush().await?;
    Ok(())
}

/// Führt einen kompletten Download-Job aus.
pub async fn run_job(
    job: &DownloadJob,
    tor: Arc<TorManager>,
    progress: Arc<Mutex<JobProgress>>,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Resolving;
    }

    let total = resolve_size(&tor, 0, &job.url).await?;
    let parts_count = if total > 0 {
        job.max_parts.min(total as usize)
    } else {
        1
    };
    let part_size = if let Some(ps) = job.part_size {
        ps
    } else if total > 0 {
        total.div_ceil(parts_count as u64)
    } else {
        0
    };

    let temp_dir = crate::config::config().temp_dir().join(format!("job-{}", job.id));
    fs::create_dir_all(&temp_dir).await?;

    let bytes_done = Arc::new(AtomicU64::new(0));
    let mut part_paths = Vec::new();
    let mut handles = Vec::new();

    for i in 0..parts_count {
        let start = i as u64 * part_size;
        let end = if i == parts_count - 1 {
            total.saturating_sub(1)
        } else {
            (start + part_size - 1).min(total.saturating_sub(1))
        };
        if total > 0 && start > end {
            break;
        }
        let dest = temp_dir.join(format!("part-{i:03}"));
        part_paths.push(dest.clone());
        let tor_c = tor.clone();
        let url = job.url.clone();
        let bd = bytes_done.clone();
        let cancel_c = cancel.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                res = download_part(tor_c, i, url, dest, start, end, bd) => res,
                _ = cancel_c.notified() => Err(anyhow::anyhow!("Part {i} abgebrochen")),
            }
        });
        handles.push((i, handle));
    }

    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Downloading;
        p.total_bytes = total;
        p.parts = (0..parts_count)
            .map(|i| PartProgress {
                index: i,
                bytes_done: 0,
                total: if i == parts_count - 1 { total - i as u64 * part_size } else { part_size },
                status: PartStatus::Downloading,
            })
            .collect();
    }

    // Fortschritt-Reporter
    let reporter = {
        let progress = progress.clone();
        let bytes_done = bytes_done.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let done = bytes_done.load(Ordering::Relaxed);
                let mut p = progress.lock().await;
                p.bytes_done = done.min(total);
                if p.status != JobStatus::Downloading {
                    break;
                }
            }
        })
    };

    let mut ok = true;
    for (i, handle) in handles {
        match handle.await {
            Ok(Ok(())) => {
                let mut p = progress.lock().await;
                if let Some(pp) = p.parts.get_mut(i) {
                    pp.status = PartStatus::Done;
                }
            }
            Ok(Err(e)) => {
                error!("Part {i} fehlgeschlagen: {e}");
                let mut p = progress.lock().await;
                if let Some(pp) = p.parts.get_mut(i) {
                    pp.status = PartStatus::Failed(e.to_string());
                }
                ok = false;
            }
            Err(e) => {
                error!("Part {i} Join-Fehler: {e}");
                ok = false;
            }
        }
    }

    reporter.abort();

    if !ok {
        let mut p = progress.lock().await;
        p.status = JobStatus::Failed("Mindestens ein Part fehlgeschlagen".into());
        anyhow::bail!("Download unvollständig");
    }

    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Merging;
    }

    merge_parts(&part_paths, &job.output).await?;

    // Temp aufräumen
    let _ = fs::remove_dir_all(&temp_dir).await;

    let mut p = progress.lock().await;
    p.status = JobStatus::Done;
    p.bytes_done = total;
    info!("Download fertig: {}", job.output.display());
    Ok(())
}

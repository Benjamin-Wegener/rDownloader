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

use crate::contract::{ByteRange, Transport};
use crate::tor::TorManager;

pub mod checksum;
pub mod resume;
pub mod retry;
pub use resume::{PartOffset, ResumeState};
pub use retry::RetryPolicy;

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
    Retrying { attempt: u32, max: u32 },
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
    /// Expected hash of the finished file; verified after merging when set.
    pub checksum: Option<checksum::Checksum>,
}

impl DownloadJob {
    pub fn new(url: url::Url, output: PathBuf, max_parts: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            url,
            output,
            max_parts: max_parts.clamp(1, 10),
            part_size: None,
            checksum: None,
        }
    }
}

/// Resolve Content-Length via einen Tor-Circuit + HEAD-Request.
async fn resolve_size(tor: &TorManager, url: &url::Url) -> Result<ServerInfo> {
    // Für echte Tor-Nutzung bauen wir einen HTTP-Stream über Tor und machen dort
    // einen manuellen HEAD-Request. Hier vereinfacht: reqwest mit socks-ähnlichem
    // Routing via TorManager.connect() als Rohstream → zu aufwändig für HEAD.
    // Pragmatisch: reqwest ohne Proxy für Größe (nur Metadata, kein Payload),
    // aber strikt optional: Falls gewünscht, über Tor. Für saubere Anonymität
    // auch über Tor: wir nutzen den Tor-Stream + manuelles HTTP/1.1.
    let stream = tor
        .connect(url.host_str().unwrap_or("localhost"), 80)
        .await?;
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
    let accepts_ranges = resp
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .is_some_and(accepts_ranges_value);
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    Ok(ServerInfo {
        total: len,
        accepts_ranges,
        etag,
        last_modified,
    })
}

/// Was der Server über die Ressource verrät.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerInfo {
    total: u64,
    accepts_ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// `Accept-Ranges: bytes` heißt ja; `none` oder alles andere heißt nein.
fn accepts_ranges_value(v: &str) -> bool {
    v.split(',').any(|t| t.trim().eq_ignore_ascii_case("bytes"))
}

/// Prüft per Test-GET `bytes=0-0`, ob der Server Ranges wirklich bedient
/// (HEAD-Header allein sind unzuverlässig): nur `206` mit Teillänge 1 zählt.
pub async fn probe_range_support(
    transport: &dyn Transport,
    url: &url::Url,
    isolation: u64,
) -> Result<bool> {
    let resp = transport
        .get(
            url,
            Some(ByteRange {
                start: 0,
                end: Some(0),
            }),
            isolation,
        )
        .await?;
    Ok(resp.head.status == 206 && resp.head.content_length == Some(1))
}

/// Byte-Bereich eines Parts (inklusive Grenzen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PartPlan {
    index: usize,
    start: u64,
    end: u64,
}

impl PartPlan {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// Teilt `total` Bytes auf. Ohne Range-Support oder bei unbekannter Größe
/// gibt es genau einen Part über die ganze Datei (Single-Connection-Fallback).
fn plan_parts(
    total: u64,
    max_parts: usize,
    part_size: Option<u64>,
    supports_ranges: bool,
) -> Vec<PartPlan> {
    if total == 0 {
        return vec![];
    }
    if !supports_ranges {
        return vec![PartPlan {
            index: 0,
            start: 0,
            end: total - 1,
        }];
    }
    let count = (max_parts.clamp(1, 10) as u64).min(total);
    let size = part_size
        .filter(|&ps| ps > 0)
        .unwrap_or_else(|| total.div_ceil(count));
    (0..)
        .map(|i| (i, i as u64 * size))
        .take_while(|&(_, start)| start < total)
        .map(|(index, start)| PartPlan {
            index,
            start,
            end: (start + size - 1).min(total - 1),
        })
        .collect()
}

/// Stream-isolation id for one part of a job.
///
/// Parts of the same job get distinct ids, and ids are stable across restarts
/// so a resumed part may reuse its previous circuit. The hash also keeps parts
/// of different jobs apart.
fn part_isolation(job: Uuid, part_idx: usize) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    job.hash(&mut hasher);
    part_idx.hash(&mut hasher);
    hasher.finish()
}

/// Lädt einen Part (Byte-Range) über Tor herunter und schreibt nach `dest`.
/// Unterstützt Resuming: Wenn `dest` bereits Daten enthält und Ranges unterstützt werden,
/// wird am vorhandenen Dateiende aufgesetzt.
async fn download_part(
    tor: Arc<TorManager>,
    isolation: u64,
    url: url::Url,
    dest: PathBuf,
    range: Option<(u64, u64)>, // inklusiv; None = ganze Datei ohne Range-Header
    bytes_done: Arc<AtomicU64>,
    part_bytes_done: &mut u64,
) -> Result<()> {
    let host = url.host_str().context("Kein Host in URL")?.to_string();
    let port = url.port_or_known_default().unwrap_or(80);

    // Prüfen, ob bereits Teile dieses Parts auf Festplatte vorliegen (Resume)
    let existing_len = if dest.exists() {
        tokio::fs::metadata(&dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };

    // Range-Berechnung anpassen falls wir resumieren können
    let (effective_range, append_mode) = match range {
        Some((start, end)) => {
            if existing_len >= (end - start + 1) {
                // Part ist bereits vollständig heruntergeladen!
                return Ok(());
            }
            if existing_len > 0 {
                let resume_start = start + existing_len;
                (Some((resume_start, end)), true)
            } else {
                (Some((start, end)), false)
            }
        }
        None => {
            // Kein Range-Support: Neu anfangen
            (None, false)
        }
    };

    // Open the part's own circuit: streams with different isolation ids never
    // share one.
    let mut stream = tor.connect_isolated(&host, port, isolation).await?;

    // Einfacher HTTP/1.1 GET mit Range-Header über den Tor-Stream
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let range_header = effective_range
        .map(|(start, end)| format!("Range: bytes={start}-{end}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\n{range_header}Connection: close\r\nUser-Agent: rdownloader/0.1\r\n\r\n"
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
    let ok_status = if effective_range.is_some() {
        // Ein 200 auf eine Range-Anfrage hieße: Server ignoriert Range → Part wäre die ganze Datei.
        status_line.contains(" 206 ")
    } else {
        status_line.contains(" 200 ")
    };
    if !ok_status {
        anyhow::bail!("Unerwarteter Status: {status_line}");
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append_mode)
        .truncate(!append_mode)
        .open(&dest)
        .await
        .with_context(|| format!("Teildatei anlegen: {dest:?}"))?;

    // Vorherige Bytes dieses Versuchs aus `bytes_done` abziehen (falls Retry nach Teildownload)
    if *part_bytes_done > 0 {
        bytes_done.fetch_sub(*part_bytes_done, Ordering::Relaxed);
        *part_bytes_done = 0;
    }

    // Rest der Response (evtl. schon Teil des Body in buf) schreiben
    if buf.len() > header_end {
        let body_start = &buf[header_end..];
        file.write_all(body_start).await?;
        let len = body_start.len() as u64;
        bytes_done.fetch_add(len, Ordering::Relaxed);
        *part_bytes_done += len;
    }

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        file.write_all(&tmp[..n]).await?;
        let len = n as u64;
        bytes_done.fetch_add(len, Ordering::Relaxed);
        *part_bytes_done += len;
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

    let info = resolve_size(&tor, &job.url).await?;
    let total = info.total;
    let plan = plan_parts(total, job.max_parts, job.part_size, info.accepts_ranges);
    let parts_count = plan.len();
    if !info.accepts_ranges {
        info!("Server ohne Accept-Ranges – Single-Connection-Download");
    }

    let temp_dir = crate::config::config()
        .temp_dir()
        .join(format!("job-{}", job.id));
    fs::create_dir_all(&temp_dir).await?;

    // Resume-Status prüfen / initialisieren
    let resume_state = match ResumeState::load_from_dir(&temp_dir)? {
        Some(saved) => {
            if info.accepts_ranges
                && saved
                    .validate(total, info.etag.as_deref(), info.last_modified.as_deref())
                    .is_ok()
            {
                info!("Resuming download from existing metadata");
                saved
            } else {
                info!("Existing resume metadata invalid or ranges not supported; resetting");
                let parts: Vec<PartOffset> = plan
                    .iter()
                    .map(|p| PartOffset::new(p.index, p.start, p.end))
                    .collect();
                let state = ResumeState::new(
                    job.id,
                    job.url.clone(),
                    total,
                    info.accepts_ranges,
                    info.etag.clone(),
                    info.last_modified.clone(),
                    parts,
                );
                state.save_to_dir(&temp_dir)?;
                state
            }
        }
        None => {
            let parts: Vec<PartOffset> = plan
                .iter()
                .map(|p| PartOffset::new(p.index, p.start, p.end))
                .collect();
            let state = ResumeState::new(
                job.id,
                job.url.clone(),
                total,
                info.accepts_ranges,
                info.etag.clone(),
                info.last_modified.clone(),
                parts,
            );
            state.save_to_dir(&temp_dir)?;
            state
        }
    };

    // Bereits vorhandene Bytes aus Teildateien zählen
    let mut initial_bytes_done = 0u64;
    let mut initial_parts_done = Vec::new();
    for part in &plan {
        let dest = temp_dir.join(format!("part-{:03}", part.index));
        let existing = if dest.exists() {
            tokio::fs::metadata(&dest)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
                .min(part.len())
        } else {
            0
        };
        initial_bytes_done += existing;
        initial_parts_done.push(existing);
    }

    let bytes_done = Arc::new(AtomicU64::new(initial_bytes_done));
    let mut part_paths = Vec::new();
    let mut handles = Vec::new();

    let retry_policy = RetryPolicy::default();
    let job_id = job.id;

    for part in &plan {
        let (i, start, end) = (part.index, part.start, part.end);
        // Ein einzelner Part über die ganze Datei braucht keinen Range-Header.
        let range = (info.accepts_ranges && parts_count > 1).then_some((start, end));
        let dest = temp_dir.join(format!("part-{i:03}"));
        part_paths.push(dest.clone());
        let tor_c = tor.clone();
        let url = job.url.clone();
        let bd = bytes_done.clone();
        let cancel_c = cancel.clone();
        let progress_c = progress.clone();
        let handle = tokio::spawn(async move {
            let part_bytes = Arc::new(AtomicU64::new(0));
            let isolation = part_isolation(job_id, i);
            retry_policy
                .retry_with_cancel(
                    &*cancel_c,
                    |_attempt| {
                        let tor_c = tor_c.clone();
                        let url = url.clone();
                        let dest = dest.clone();
                        let bd = bd.clone();
                        let pb_arc = part_bytes.clone();
                        async move {
                            let mut pb = pb_arc.load(Ordering::Relaxed);
                            let res =
                                download_part(tor_c, isolation, url, dest, range, bd, &mut pb)
                                    .await;
                            pb_arc.store(pb, Ordering::Relaxed);
                            res
                        }
                    },
                    |next_attempt, _delay, _err| {
                        let progress_c = progress_c.clone();
                        tokio::spawn(async move {
                            let mut p = progress_c.lock().await;
                            if let Some(pp) = p.parts.get_mut(i) {
                                pp.status = PartStatus::Retrying {
                                    attempt: next_attempt,
                                    max: retry_policy.max_retries,
                                };
                            }
                        });
                    },
                )
                .await
        });
        handles.push((i, handle));
    }

    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Downloading;
        p.total_bytes = total;
        p.bytes_done = initial_bytes_done;
        p.parts = plan
            .iter()
            .map(|part| PartProgress {
                index: part.index,
                bytes_done: initial_parts_done.get(part.index).copied().unwrap_or(0),
                total: part.len(),
                status: if initial_parts_done.get(part.index).copied().unwrap_or(0) >= part.len() {
                    PartStatus::Done
                } else {
                    PartStatus::Downloading
                },
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

    // Fortschritt vor Merging im Resume-State persistieren
    let mut updated_parts = Vec::new();
    for part in &plan {
        let dest = temp_dir.join(format!("part-{:03}", part.index));
        let written = if dest.exists() {
            tokio::fs::metadata(&dest)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };
        let mut po = PartOffset::new(part.index, part.start, part.end);
        po.bytes_written = written;
        po.done = written >= po.total();
        updated_parts.push(po);
    }
    let mut final_resume_state = resume_state;
    final_resume_state.parts = updated_parts;
    let _ = final_resume_state.save_to_dir(&temp_dir);

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

    if let Some(expected) = &job.checksum {
        if let Err(e) = checksum::verify_file(&job.output, expected).await {
            // A corrupt file must not look like a finished download.
            let _ = fs::remove_file(&job.output).await;
            let mut p = progress.lock().await;
            p.status = JobStatus::Failed(e.to_string());
            return Err(e);
        }
        info!("Checksum verified: {expected}");
    }

    // Temp aufräumen
    let _ = fs::remove_dir_all(&temp_dir).await;

    let mut p = progress.lock().await;
    p.status = JobStatus::Done;
    p.bytes_done = total;
    info!("Download fertig: {}", job.output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{MockResource, MockTransport};

    #[test]
    fn part_isolation_ids_are_stable_and_distinct() {
        let job = Uuid::new_v4();
        let other = Uuid::new_v4();
        assert_eq!(part_isolation(job, 0), part_isolation(job, 0));
        assert_ne!(part_isolation(job, 0), part_isolation(job, 1));
        assert_ne!(part_isolation(job, 0), part_isolation(other, 0));
    }

    #[test]
    fn accept_ranges_header() {
        assert!(accepts_ranges_value("bytes"));
        assert!(accepts_ranges_value("Bytes"));
        assert!(!accepts_ranges_value("none"));
        assert!(!accepts_ranges_value(""));
    }

    #[test]
    fn plan_teilt_lueckenlos() {
        let plan = plan_parts(10, 3, None, true);
        let ranges: Vec<_> = plan.iter().map(|p| (p.start, p.end)).collect();
        assert_eq!(ranges, vec![(0, 3), (4, 7), (8, 9)]);
        assert_eq!(plan.iter().map(PartPlan::len).sum::<u64>(), 10);
    }

    #[test]
    fn plan_nie_mehr_parts_als_bytes() {
        assert_eq!(plan_parts(2, 10, None, true).len(), 2);
    }

    #[test]
    fn plan_ohne_ranges_ist_single_connection() {
        assert_eq!(
            plan_parts(1000, 8, None, false),
            vec![PartPlan {
                index: 0,
                start: 0,
                end: 999
            }]
        );
    }

    #[test]
    fn plan_leere_datei_hat_keine_parts() {
        assert!(plan_parts(0, 4, None, true).is_empty());
    }

    #[test]
    fn plan_respektiert_part_size() {
        let plan = plan_parts(10, 10, Some(4), true);
        assert_eq!(plan.len(), 3);
        assert_eq!(
            plan[2],
            PartPlan {
                index: 2,
                start: 8,
                end: 9
            }
        );
    }

    #[tokio::test]
    async fn probe_erkennt_range_support() {
        let t = MockTransport::new()
            .with_resource("http://x.test/r", MockResource::new(&b"abcdef"[..]));
        let url = url::Url::parse("http://x.test/r").unwrap();
        assert!(probe_range_support(&t, &url, 0).await.unwrap());
    }

    #[tokio::test]
    async fn probe_erkennt_fehlenden_range_support() {
        let mut res = MockResource::new(&b"abcdef"[..]);
        res.accepts_ranges = false;
        let t = MockTransport::new().with_resource("http://x.test/n", res);
        let url = url::Url::parse("http://x.test/n").unwrap();
        assert!(!probe_range_support(&t, &url, 0).await.unwrap());
    }
}

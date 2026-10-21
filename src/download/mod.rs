//! Multi-part download engine.
//!
//! Splits a file into up to N parts, fetches each part over its own
//! Tor circuit (via `TorManager`) and writes it to a temp part file.
//! The part files are merged at the end.

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
use crate::t;
use crate::tor::TorManager;
use futures::StreamExt;

pub mod checksum;
pub mod extract;
pub mod limiter;
pub mod quarantine;
pub mod resume;
pub mod retry;
pub mod sanitize;
pub mod scheduler;
pub mod space;
#[allow(unused_imports)]
pub use quarantine::{apply_quarantine, get_quarantine, QUARANTINE_ATTR_NAME};
pub use resume::{PartOffset, ResumeState};
pub use retry::RetryPolicy;
#[allow(unused_imports)]
pub use sanitize::{resolve_destination, sanitize_filename, DEFAULT_FALLBACK_FILENAME};
#[allow(unused_imports)]
pub use space::{ensure_sufficient_space, preallocate_file, query_available_space, DiskSpace};

/// Progress of a single part.
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

/// Overall progress of a download job.
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
    Paused,
    Merging,
    Done,
    Failed(String),
    Cancelled,
}

/// A download job: source → target path, split into parts.
pub struct DownloadJob {
    pub id: Uuid,
    pub url: url::Url,
    pub output: PathBuf,
    pub max_parts: usize,
    pub part_size: Option<u64>, // None = auto (total/max_parts)
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

/// Resolves size and validators with a HEAD request sent through Tor.
///
/// Nothing is resolved locally: the host name is handed to Arti and resolved
/// by the exit node, so this path cannot leak DNS.
async fn resolve_size(transport: &dyn Transport, url: &url::Url) -> Result<ServerInfo> {
    let head = transport.head(url, 0).await?;
    let total = head
        .total_size
        .context(t!("download.error.content_length_missing"))?;
    Ok(ServerInfo {
        total,
        accepts_ranges: head.accepts_ranges,
        etag: head.etag,
        last_modified: head.last_modified,
    })
}

/// What the server reveals about the resource.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerInfo {
    total: u64,
    accepts_ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// `Accept-Ranges: bytes` means yes; `none` or anything else means no.
fn accepts_ranges_value(v: &str) -> bool {
    v.split(',').any(|t| t.trim().eq_ignore_ascii_case("bytes"))
}

/// Checks with a test GET `bytes=0-0` whether the server actually serves ranges
/// (HEAD headers alone are unreliable): only `206` with a length of 1 counts.
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

/// Byte range of a part (inclusive bounds).
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

/// Splits `total` bytes. Without range support or with an unknown size there
/// is exactly one part over the whole file (single-connection fallback).
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

/// Downloads a part (byte range) through `transport` and writes it to `dest`.
/// Supports resuming: if `dest` already contains data and ranges are supported,
/// it continues at the existing end of the file.
async fn download_part(
    transport: Arc<dyn Transport>,
    isolation: u64,
    url: url::Url,
    dest: PathBuf,
    range: Option<(u64, u64)>, // inclusive; None = whole file without a Range header
    bytes_done: Arc<AtomicU64>,
    part_bytes_done: &mut u64,
) -> Result<()> {
    // Check whether data for this part already exists on disk (resume)
    let existing_len = if dest.exists() {
        tokio::fs::metadata(&dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };

    // Adjust the range calculation if we can resume
    let (effective_range, append_mode) = match range {
        Some((start, end)) => {
            if existing_len >= (end - start + 1) {
                // The part is already fully downloaded!
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
            // No range support: start over
            (None, false)
        }
    };

    // Every part has its own isolation id: streams with different ids never
    // share a circuit. The transport follows redirects, speaks TLS and decodes
    // chunked bodies.
    let response = transport
        .get(
            &url,
            effective_range.map(|(start, end)| ByteRange {
                start,
                end: Some(end),
            }),
            isolation,
        )
        .await?;

    let status = response.head.status;
    // A 200 in response to a range request would mean the server ignores Range → the part would be the whole file.
    let ok_status = if effective_range.is_some() {
        status == 206
    } else {
        status == 200
    };
    if !ok_status {
        anyhow::bail!(t!(
            "download.error.unexpected_status",
            status = status.to_string()
        ));
    }
    let expected_len = response.head.content_length;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append_mode)
        .truncate(!append_mode)
        .open(&dest)
        .await
        .with_context(|| {
            t!(
                "download.error.create_part_file",
                path = format!("{dest:?}")
            )
        })?;

    // Subtract the previous bytes of this attempt from `bytes_done` (in case of a retry after a partial download)
    if *part_bytes_done > 0 {
        bytes_done.fetch_sub(*part_bytes_done, Ordering::Relaxed);
        *part_bytes_done = 0;
    }

    let mut received = 0u64;
    let mut body = response.body;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        let len = chunk.len() as u64;
        limiter::global().acquire(len).await;
        bytes_done.fetch_add(len, Ordering::Relaxed);
        *part_bytes_done += len;
        received += len;
    }
    file.flush().await?;

    // A connection that ends early must fail, so the retry loop continues the part.
    if let Some(expected) = expected_len {
        if received != expected {
            anyhow::bail!(t!(
                "download.error.body_truncated",
                received = received.to_string(),
                expected = expected.to_string()
            ));
        }
    }
    Ok(())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Merges all part files into the final output file.
async fn merge_parts(parts: &[PathBuf], output: &Path, expected_total: u64) -> Result<()> {
    // If output file parent does not exist, create it
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).await?;
        }
    }

    let std_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .with_context(|| t!("download.error.create_output", path = format!("{output:?}")))?;

    // Preallocate destination file space
    if expected_total > 0 {
        let _ = preallocate_file(&std_file, expected_total);
    }

    let mut out = File::from_std(std_file);
    let mut buf = vec![0u8; 64 * 1024];
    for part in parts {
        let mut f = File::open(part)
            .await
            .with_context(|| t!("download.error.open_part", path = format!("{part:?}")))?;
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

/// Runs a complete download job.
pub async fn run_job(
    job: &DownloadJob,
    tor: Arc<TorManager>,
    progress: Arc<Mutex<JobProgress>>,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let transport = Arc::new(crate::net::SchemeTransport::new(tor.clone()));
    // After a failed attempt the part's circuit may be dead: force a fresh one.
    let invalidate: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |isolation| {
        tor.invalidate_isolation(isolation);
    });
    run_job_with(job, transport, invalidate, progress, cancel).await
}

/// The download engine, independent of the network: `transport` fetches the
/// bytes and `invalidate` is called with a part's isolation id after a failure.
pub async fn run_job_with(
    job: &DownloadJob,
    transport: Arc<dyn Transport>,
    invalidate: Arc<dyn Fn(u64) + Send + Sync>,
    progress: Arc<Mutex<JobProgress>>,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Resolving;
    }

    let info = resolve_size(&*transport, &job.url).await?;
    let total = info.total;

    // Check available disk space before starting
    ensure_sufficient_space(&job.output, total)?;

    let plan = plan_parts(total, job.max_parts, job.part_size, info.accepts_ranges);
    let parts_count = plan.len();
    if !info.accepts_ranges {
        info!("Server without Accept-Ranges - single-connection download");
    }

    let temp_dir = crate::config::config()
        .temp_dir()
        .join(format!("job-{}", job.id));
    fs::create_dir_all(&temp_dir).await?;

    // Check / initialize the resume state
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

    // Count bytes already present in part files
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
        // A single part over the whole file needs no Range header.
        let range = (info.accepts_ranges && parts_count > 1).then_some((start, end));
        let dest = temp_dir.join(format!("part-{i:03}"));
        part_paths.push(dest.clone());
        let transport_c = transport.clone();
        let invalidate_c = invalidate.clone();
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
                        let transport_c = transport_c.clone();
                        let invalidate_c = invalidate_c.clone();
                        let url = url.clone();
                        let dest = dest.clone();
                        let bd = bd.clone();
                        let pb_arc = part_bytes.clone();
                        async move {
                            let mut pb = pb_arc.load(Ordering::Relaxed);
                            let res = download_part(
                                transport_c.clone(),
                                isolation,
                                url,
                                dest,
                                range,
                                bd,
                                &mut pb,
                            )
                            .await;
                            if res.is_err() {
                                // The circuit may have died mid-stream. Force a
                                // fresh one for the retry that follows.
                                invalidate_c(isolation);
                            }
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

    // Progress reporter
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
                error!("Part {i} failed: {e}");
                let mut p = progress.lock().await;
                if let Some(pp) = p.parts.get_mut(i) {
                    pp.status = PartStatus::Failed(e.to_string());
                }
                ok = false;
            }
            Err(e) => {
                error!("Part {i} join error: {e}");
                ok = false;
            }
        }
    }

    reporter.abort();

    // Persist progress in the resume state before merging
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
        p.status = JobStatus::Failed(t!("download.error.part_failed"));
        anyhow::bail!(t!("download.error.incomplete"));
    }

    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Merging;
    }

    merge_parts(&part_paths, &job.output, total).await?;

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

    // Clean up temp files
    let _ = fs::remove_dir_all(&temp_dir).await;

    // Apply macOS Gatekeeper quarantine attribute (safe no-op on non-macOS)
    let _ = quarantine::apply_quarantine(&job.output);

    let mut p = progress.lock().await;
    p.status = JobStatus::Done;
    p.bytes_done = total;
    info!("Download complete: {}", job.output.display());
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
    fn plan_splits_without_gaps() {
        let plan = plan_parts(10, 3, None, true);
        let ranges: Vec<_> = plan.iter().map(|p| (p.start, p.end)).collect();
        assert_eq!(ranges, vec![(0, 3), (4, 7), (8, 9)]);
        assert_eq!(plan.iter().map(PartPlan::len).sum::<u64>(), 10);
    }

    #[test]
    fn plan_never_more_parts_than_bytes() {
        assert_eq!(plan_parts(2, 10, None, true).len(), 2);
    }

    #[test]
    fn plan_without_ranges_is_single_connection() {
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
    fn plan_empty_file_has_no_parts() {
        assert!(plan_parts(0, 4, None, true).is_empty());
    }

    #[test]
    fn plan_respects_part_size() {
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
    async fn probe_detects_range_support() {
        let t = MockTransport::new()
            .with_resource("http://x.test/r", MockResource::new(&b"abcdef"[..]));
        let url = url::Url::parse("http://x.test/r").unwrap();
        assert!(probe_range_support(&t, &url, 0).await.unwrap());
    }

    #[tokio::test]
    async fn probe_detects_missing_range_support() {
        let mut res = MockResource::new(&b"abcdef"[..]);
        res.accepts_ranges = false;
        let t = MockTransport::new().with_resource("http://x.test/n", res);
        let url = url::Url::parse("http://x.test/n").unwrap();
        assert!(!probe_range_support(&t, &url, 0).await.unwrap());
    }

    // ── Engine end to end, without a network ──

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    async fn run(
        transport: MockTransport,
        url: &str,
        parts: usize,
    ) -> (Result<()>, Vec<u8>, JobProgress) {
        let dir = std::env::temp_dir().join(format!("rdl-engine-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("out.bin");
        let job = DownloadJob::new(url::Url::parse(url).unwrap(), output.clone(), parts);
        let progress = Arc::new(Mutex::new(JobProgress {
            id: job.id,
            url: url.into(),
            filename: "out.bin".into(),
            total_bytes: 0,
            bytes_done: 0,
            parts: vec![],
            status: JobStatus::Queued,
        }));
        let invalidate: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(|_| {});
        let res = run_job_with(
            &job,
            Arc::new(transport),
            invalidate,
            progress.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        let data = std::fs::read(&output).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        let p = progress.lock().await.clone();
        (res, data, p)
    }

    #[tokio::test]
    async fn multipart_download_reassembles_the_file() {
        let data = payload(10_000);
        let t = MockTransport::new()
            .with_resource("http://x.test/f", MockResource::new(data.clone()))
            .with_chunk_size(700);
        let (res, got, p) = run(t.clone(), "http://x.test/f", 4).await;
        res.unwrap();
        assert_eq!(got, data);
        assert_eq!(p.status, JobStatus::Done);
        // Four parts, each with its own isolation id.
        let isolations: std::collections::HashSet<u64> = t
            .requests()
            .iter()
            .filter_map(|r| match r {
                crate::contract::RecordedRequest::Get {
                    isolation,
                    range: Some(_),
                    ..
                } => Some(*isolation),
                _ => None,
            })
            .collect();
        assert_eq!(isolations.len(), 4);
    }

    #[tokio::test]
    async fn server_without_ranges_is_downloaded_over_one_connection() {
        let data = payload(3_000);
        let mut res = MockResource::new(data.clone());
        res.accepts_ranges = false;
        let t = MockTransport::new().with_resource("http://x.test/f", res);
        let (r, got, _) = run(t.clone(), "http://x.test/f", 4).await;
        r.unwrap();
        assert_eq!(got, data);
        let ranged = t
            .requests()
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    crate::contract::RecordedRequest::Get { range: Some(_), .. }
                )
            })
            .count();
        assert_eq!(ranged, 0, "no Range header without server support");
    }

    #[tokio::test]
    async fn a_dropped_connection_is_retried() {
        let data = payload(4_000);
        let t =
            MockTransport::new().with_resource("http://x.test/f", MockResource::new(data.clone()));
        t.truncate_next_body_after(500);
        let (r, got, _) = run(t, "http://x.test/f", 1).await;
        r.unwrap();
        assert_eq!(got, data, "the retry must produce the complete file");
    }

    #[tokio::test]
    async fn a_short_body_without_error_is_detected() {
        // A transport that ends the body early without reporting an error must still fail.
        struct ShortTransport;
        #[async_trait::async_trait]
        impl Transport for ShortTransport {
            async fn head(&self, url: &url::Url, _: u64) -> Result<crate::contract::ResponseHead> {
                Ok(crate::contract::ResponseHead {
                    status: 200,
                    final_url: url.clone(),
                    content_length: Some(100),
                    total_size: Some(100),
                    accepts_ranges: false,
                    filename: None,
                    etag: None,
                    last_modified: None,
                })
            }
            async fn get(
                &self,
                url: &url::Url,
                _: Option<ByteRange>,
                i: u64,
            ) -> Result<crate::contract::Response> {
                let head = self.head(url, i).await?;
                let body = futures::stream::iter(vec![Ok(bytes::Bytes::from_static(b"short"))]);
                Ok(crate::contract::Response {
                    head,
                    body: Box::pin(body),
                })
            }
        }
        let dir = std::env::temp_dir().join(format!("rdl-engine-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let job = DownloadJob::new(
            url::Url::parse("http://x.test/f").unwrap(),
            dir.join("o.bin"),
            1,
        );
        let progress = Arc::new(Mutex::new(JobProgress {
            id: job.id,
            url: String::new(),
            filename: String::new(),
            total_bytes: 0,
            bytes_done: 0,
            parts: vec![],
            status: JobStatus::Queued,
        }));
        let res = run_job_with(
            &job,
            Arc::new(ShortTransport),
            Arc::new(|_| {}),
            progress,
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            res.is_err(),
            "a truncated body must not count as a finished download"
        );
    }
}

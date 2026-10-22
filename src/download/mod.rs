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
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::contract::{ByteRange, Transport};
use crate::t;
use crate::tor::TorManager;
use futures::StreamExt;

pub mod checksum;
pub mod conditional;
pub mod control;
pub mod extract;
pub mod hooks;
pub mod limiter;
pub mod mirror;
pub mod pieces;
pub mod quarantine;
pub mod resume;
pub mod retry;
pub mod sanitize;
pub mod scheduler;
pub mod slow;
pub mod slowparts;
pub mod space;
pub mod timetable;
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
    /// A local copy to compare against (conditional download): when it is as new
    /// as the server's version, nothing is downloaded.
    pub existing_copy: Option<PathBuf>,
    /// Further URLs of the same file; the engine picks one per part (see `mirror`).
    pub mirrors: Vec<url::Url>,
    /// Piece checksums for chunk-wise verification and repair after merging.
    pub pieces: Option<pieces::PieceHashes>,
}

impl DownloadJob {
    pub fn new(url: url::Url, output: PathBuf, max_parts: usize) -> Self {
        let hint = mirror::hints::lookup(&url);
        Self {
            id: Uuid::new_v4(),
            mirrors: hint.as_ref().map(|h| h.mirrors.clone()).unwrap_or_default(),
            pieces: hint.and_then(|h| h.pieces),
            url,
            output,
            max_parts: max_parts.clamp(1, 10),
            part_size: None,
            checksum: None,
            existing_copy: None,
        }
    }
}

/// Resolves size and validators with a HEAD request sent through Tor.
///
/// Falls back to a probe GET `bytes=0-0` if the server rejects HEAD (e.g. 400 Bad Request,
/// 405 Method Not Allowed) or fails to provide headers.
///
/// Nothing is resolved locally: the host name is handed to Arti and resolved
/// by the exit node, so this path cannot leak DNS.
async fn resolve_size(transport: &dyn Transport, url: &url::Url) -> Result<ServerInfo> {
    match transport.head(url, 0).await {
        Ok(head) => Ok(ServerInfo {
            total: head.total_size,
            accepts_ranges: head.accepts_ranges,
            etag: head.etag,
            last_modified: head.last_modified,
        }),
        Err(head_err) => {
            debug!("HEAD failed for {url} ({head_err:#}), falling back to GET probe");
            let probe = transport
                .get(
                    url,
                    Some(ByteRange {
                        start: 0,
                        end: Some(0),
                    }),
                    0,
                )
                .await?;
            let head = probe.head;
            let accepts_ranges = head.status == 206 && head.content_length == Some(1);
            Ok(ServerInfo {
                total: head.total_size,
                accepts_ranges: head.accepts_ranges || accepts_ranges,
                etag: head.etag,
                last_modified: head.last_modified,
            })
        }
    }
}

/// What the server reveals about the resource.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerInfo {
    /// `None` when the server does not report a size.
    total: Option<u64>,
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

/// What one attempt at a part needs to know.
struct PartSpec {
    isolation: u64,
    url: url::Url,
    dest: PathBuf,
    /// Inclusive byte range; `None` = whole file without a Range header.
    range: Option<(u64, u64)>,
    /// Size of the whole file, if known; a mirror reporting another size is rejected.
    expected_total: Option<u64>,
    /// Pause and cancel of the job this part belongs to.
    control: Arc<control::JobControl>,
    /// Counts every byte this part delivers (read by the slow-part monitor).
    live: Arc<AtomicU64>,
}

/// Downloads a part (byte range) through `transport` and writes it to `dest`.
/// Supports resuming: if `dest` already contains data and ranges are supported,
/// it continues at the existing end of the file.
async fn download_part(
    transport: Arc<dyn Transport>,
    spec: PartSpec,
    bytes_done: Arc<AtomicU64>,
    part_bytes_done: &mut u64,
) -> Result<()> {
    let PartSpec {
        isolation,
        url,
        dest,
        range,
        expected_total,
        control,
        live,
    } = spec;
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
    // A mirror with another file size serves a different file: its bytes must
    // never be mixed into this download.
    if matches!((response.head.total_size, expected_total), (Some(t), Some(e)) if t != e) {
        return Err(mirror::MirrorMismatch.into());
    }

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

    // Starting over (no resume): the bytes of the previous attempt are gone. When the
    // part resumes from its file they are still there and stay counted.
    if !append_mode && *part_bytes_done > 0 {
        bytes_done.fetch_sub(*part_bytes_done, Ordering::Relaxed);
    }
    *part_bytes_done = 0;

    let cfg = crate::config::config();
    let read_timeout = std::time::Duration::from_secs(cfg.read_timeout_secs.max(1));
    let mut guard = slow::SlowGuard::new(cfg.min_speed_kib.saturating_mul(1024), slow::WINDOW);
    let mut received = 0u64;
    let mut body = response.body;
    loop {
        // Paused: stop reading (the connection stays open, nothing is lost).
        control.wait_unpaused().await;
        if control.is_cancelled() {
            // Not an error: an error would be retried. The retry loop is woken by the
            // cancel signal and drops this future (and with it the connection).
            std::future::pending::<()>().await;
        }
        // A connection that delivers nothing for `read_timeout` is dead.
        let waiting_since = std::time::Instant::now();
        let next = tokio::time::timeout(read_timeout, body.next())
            .await
            .map_err(|_| {
                anyhow::anyhow!(t!(
                    "download.error.read_timeout",
                    secs = read_timeout.as_secs().to_string()
                ))
            })?;
        let Some(chunk) = next else { break };
        let chunk = chunk?;
        guard
            .record(chunk.len() as u64, waiting_since.elapsed())
            .map_err(|slow| {
                anyhow::anyhow!(t!(
                    "download.error.too_slow",
                    speed = slow.bytes_per_sec.to_string(),
                    min = slow.minimum.to_string()
                ))
            })?;
        file.write_all(&chunk).await?;
        let len = chunk.len() as u64;
        limiter::global().acquire(len).await;
        bytes_done.fetch_add(len, Ordering::Relaxed);
        live.fetch_add(len, Ordering::Relaxed);
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

/// Checks the merged file piece by piece and re-downloads damaged pieces from the
/// mirrors (each fetched piece is verified before it is written). Fails if a
/// piece cannot be repaired.
async fn verify_and_repair_pieces(
    output: &Path,
    table: &pieces::PieceHashes,
    total: u64,
    can_range: bool,
    transport: &dyn Transport,
    selector: &mirror::MirrorSelector,
    job_id: Uuid,
) -> Result<()> {
    use tokio::io::AsyncSeekExt;

    let bad = pieces::bad_pieces(output, table, total).await?;
    if bad.is_empty() {
        info!("All {} pieces verified", table.hashes.len());
        return Ok(());
    }
    warn!(
        "{} of {} pieces are damaged, repairing",
        bad.len(),
        table.hashes.len()
    );
    if !can_range {
        anyhow::bail!(t!(
            "download.error.pieces_corrupt",
            count = bad.len().to_string()
        ));
    }

    let mut file = OpenOptions::new().write(true).open(output).await?;
    let mut unrepaired = 0usize;
    for index in bad {
        let (start, end) = table.range(index, total);
        let mut repaired = false;
        // Every mirror gets a chance; a piece only counts when its hash matches.
        for m in 0..selector.len() {
            let range = ByteRange {
                start,
                end: Some(end - 1),
            };
            let isolation = part_isolation(job_id, 1000 + index);
            let Ok(resp) = transport.get(selector.url(m), Some(range), isolation).await else {
                continue;
            };
            let mut data = Vec::with_capacity((end - start) as usize);
            let mut body = resp.body;
            let mut failed = false;
            while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(c) => data.extend_from_slice(&c),
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            if failed || data.len() as u64 != end - start {
                continue;
            }
            if checksum::hash_bytes(table.algo, &data).eq_ignore_ascii_case(&table.hashes[index]) {
                file.seek(std::io::SeekFrom::Start(start)).await?;
                file.write_all(&data).await?;
                repaired = true;
                break;
            }
        }
        if !repaired {
            unrepaired += 1;
        }
    }
    file.flush().await?;
    if unrepaired > 0 {
        anyhow::bail!(t!(
            "download.error.pieces_corrupt",
            count = unrepaired.to_string()
        ));
    }
    info!("All damaged pieces repaired");
    Ok(())
}

/// Everything a finished transfer needs after its parts are on disk.
struct FinishCtx<'a> {
    job: &'a DownloadJob,
    transport: &'a dyn Transport,
    selector: &'a mirror::MirrorSelector,
    progress: &'a Arc<Mutex<JobProgress>>,
    temp_dir: &'a Path,
    last_modified: Option<&'a str>,
    accepts_ranges: bool,
}

/// Merges the part files into the output, verifies it (checksum, pieces), cleans up
/// and marks the job done.
async fn finalize_download(ctx: FinishCtx<'_>, part_paths: &[PathBuf], total: u64) -> Result<()> {
    let job = ctx.job;
    {
        let mut p = ctx.progress.lock().await;
        p.status = JobStatus::Merging;
    }

    merge_parts(part_paths, &job.output, total).await?;
    conditional::apply_remote_time(&job.output, ctx.last_modified);

    if let Some(expected) = &job.checksum {
        if let Err(e) = checksum::verify_file(&job.output, expected).await {
            // A corrupt file must not look like a finished download.
            let _ = fs::remove_file(&job.output).await;
            let mut p = ctx.progress.lock().await;
            p.status = JobStatus::Failed(e.to_string());
            return Err(e);
        }
        info!("Checksum verified: {expected}");
    }

    if let Some(table) = job.pieces.as_ref().filter(|p| p.fits(total)) {
        if let Err(e) = verify_and_repair_pieces(
            &job.output,
            table,
            total,
            ctx.accepts_ranges,
            ctx.transport,
            ctx.selector,
            job.id,
        )
        .await
        {
            let _ = fs::remove_file(&job.output).await;
            let mut p = ctx.progress.lock().await;
            p.status = JobStatus::Failed(e.to_string());
            return Err(e);
        }
    }

    // Clean up temp files
    let _ = fs::remove_dir_all(ctx.temp_dir).await;

    // Apply macOS Gatekeeper quarantine attribute (safe no-op on non-macOS)
    let _ = quarantine::apply_quarantine(&job.output);

    let mut p = ctx.progress.lock().await;
    p.status = JobStatus::Done;
    p.bytes_done = total;
    info!("Download complete: {}", job.output.display());
    Ok(())
}

/// Download of a file whose size the server does not report (typical for CDNs and
/// compressed or chunked responses). Such servers rarely support ranges either, so
/// this is one connection with retries from the start; progress shows bytes but no
/// percentage. The size becomes known when the transfer ends.
async fn download_unknown_size(
    job: &DownloadJob,
    transport: Arc<dyn Transport>,
    selector: Arc<mirror::MirrorSelector>,
    progress: Arc<Mutex<JobProgress>>,
    cancel: Arc<tokio::sync::Notify>,
    invalidate: Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<()> {
    info!("Server reports no size: single-connection download without percentage");
    let control = control::get_or_create(job.id);
    let temp_dir = crate::config::config().job_temp_dir(job.id);
    fs::create_dir_all(&temp_dir).await?;
    let dest = temp_dir.join("part-000");

    {
        let mut p = progress.lock().await;
        p.status = JobStatus::Downloading;
        p.total_bytes = 0;
        p.bytes_done = 0;
        p.parts = vec![PartProgress {
            index: 0,
            bytes_done: 0,
            total: 0,
            status: PartStatus::Downloading,
        }];
    }

    let bytes_done = Arc::new(AtomicU64::new(0));
    let reporter = {
        let (progress, bytes_done) = (progress.clone(), bytes_done.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let mut p = progress.lock().await;
                p.bytes_done = bytes_done.load(Ordering::Relaxed);
                if p.status != JobStatus::Downloading {
                    break;
                }
            }
        })
    };

    let isolation = part_isolation(job.id, 0);
    let part_bytes = Arc::new(AtomicU64::new(0));
    let last_mirror = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let retry_policy = RetryPolicy::default();
    let result = retry_policy
        .retry_with_cancel(
            &*cancel,
            |_attempt| {
                let (transport, selector, invalidate) =
                    (transport.clone(), selector.clone(), invalidate.clone());
                let (dest, bytes_done) = (dest.clone(), bytes_done.clone());
                let control = control.clone();
                let (part_bytes, last_mirror) = (part_bytes.clone(), last_mirror.clone());
                async move {
                    let avoid = match last_mirror.load(Ordering::Relaxed) {
                        usize::MAX => None,
                        m => Some(m),
                    };
                    let idx = selector.pick(0, avoid);
                    let spec = PartSpec {
                        isolation,
                        url: selector.url(idx).clone(),
                        dest,
                        range: None,
                        expected_total: None,
                        control: control.clone(),
                        live: Arc::new(AtomicU64::new(0)),
                    };
                    let mut pb = part_bytes.load(Ordering::Relaxed);
                    let started = std::time::Instant::now();
                    let res = download_part(transport, spec, bytes_done, &mut pb).await;
                    part_bytes.store(pb, Ordering::Relaxed);
                    match &res {
                        Ok(()) => selector.record_success(idx, pb, started.elapsed().as_secs_f64()),
                        Err(_) => {
                            selector.record_failure(idx);
                            last_mirror.store(idx, Ordering::Relaxed);
                            invalidate(isolation);
                        }
                    }
                    res
                }
            },
            |_, _, _| {},
        )
        .await;
    reporter.abort();

    if let Err(e) = result {
        let mut p = progress.lock().await;
        p.status = JobStatus::Failed(e.to_string());
        return Err(e);
    }

    let total = fs::metadata(&dest).await.map(|m| m.len()).unwrap_or(0);
    {
        let mut p = progress.lock().await;
        p.total_bytes = total;
        p.bytes_done = total;
        if let Some(part) = p.parts.get_mut(0) {
            part.bytes_done = total;
            part.total = total;
            part.status = PartStatus::Done;
        }
    }
    finalize_download(
        FinishCtx {
            job,
            transport: &*transport,
            selector: &selector,
            progress: &progress,
            temp_dir: &temp_dir,
            last_modified: None,
            accepts_ranges: false,
        },
        &[dest],
        total,
    )
    .await
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

    // HLS/DASH manifests are routed to the segment downloader instead of the
    // byte-range engine.
    if crate::net::looks_like_manifest(&job.url) {
        let downloader = crate::net::SegmentDownloader::new(transport.clone());
        let report = downloader.download(&job.url, &job.output).await?;
        let mut p = progress.lock().await;
        p.total_bytes = report.bytes;
        p.bytes_done = report.bytes;
        p.status = JobStatus::Done;
        return Ok(());
    }

    let mut all_urls = vec![job.url.clone()];
    all_urls.extend(job.mirrors.iter().cloned());
    let selector = Arc::new(mirror::MirrorSelector::new(all_urls));

    // The first URL that answers decides size and validators; the others are
    // checked against this size when they serve a part.
    let mut resolved = None;
    let mut last_error = None;
    for i in 0..selector.len() {
        match resolve_size(&*transport, selector.url(i)).await {
            Ok(info) => {
                resolved = Some(info);
                break;
            }
            Err(e) => {
                warn!("Could not resolve {}: {e:#}", selector.url(i));
                selector.record_failure(i);
                last_error = Some(e);
            }
        }
    }
    let info = match resolved {
        Some(info) => info,
        None => return Err(last_error.expect("at least one URL")),
    };
    let Some(total) = info.total else {
        return download_unknown_size(job, transport, selector, progress, cancel, invalidate).await;
    };

    if let Some(local) = &job.existing_copy {
        if conditional::is_up_to_date(local, total, info.last_modified.as_deref()) {
            info!(
                "Local copy is up to date, skipping download: {}",
                local.display()
            );
            let mut p = progress.lock().await;
            p.total_bytes = total;
            p.bytes_done = total;
            p.status = JobStatus::Done;
            return Ok(());
        }
    }

    // Check available disk space before starting
    ensure_sufficient_space(&job.output, total)?;

    let plan = plan_parts(total, job.max_parts, job.part_size, info.accepts_ranges);
    let parts_count = plan.len();
    if !info.accepts_ranges {
        info!("Server without Accept-Ranges - single-connection download");
    }

    let temp_dir = crate::config::config().job_temp_dir(job.id);
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
    let control = control::get_or_create(job_id);
    let live_counters: Vec<Arc<AtomicU64>> =
        plan.iter().map(|_| Arc::new(AtomicU64::new(0))).collect();
    let restart_signals: Vec<Arc<tokio::sync::Notify>> = plan
        .iter()
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();

    for part in &plan {
        let (i, start, end) = (part.index, part.start, part.end);
        // A single part over the whole file needs no Range header.
        let range = (info.accepts_ranges && parts_count > 1).then_some((start, end));
        let dest = temp_dir.join(format!("part-{i:03}"));
        part_paths.push(dest.clone());
        let transport_c = transport.clone();
        let invalidate_c = invalidate.clone();
        let selector_c = selector.clone();
        let bd = bytes_done.clone();
        let cancel_c = cancel.clone();
        let progress_c = progress.clone();
        let control_c = control.clone();
        let live_c = live_counters[i].clone();
        let restart_c = restart_signals[i].clone();
        let handle = tokio::spawn(async move {
            let part_bytes = Arc::new(AtomicU64::new(0));
            let last_mirror = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
            let isolation = part_isolation(job_id, i);
            retry_policy
                .retry_with_cancel(
                    &*cancel_c,
                    |_attempt| {
                        let transport_c = transport_c.clone();
                        let invalidate_c = invalidate_c.clone();
                        let selector_c = selector_c.clone();
                        let last_mirror = last_mirror.clone();
                        let dest = dest.clone();
                        let bd = bd.clone();
                        let pb_arc = part_bytes.clone();
                        let (control_c, live_c, restart_c) =
                            (control_c.clone(), live_c.clone(), restart_c.clone());
                        async move {
                            let avoid = match last_mirror.load(Ordering::Relaxed) {
                                usize::MAX => None,
                                m => Some(m),
                            };
                            let mirror_idx = selector_c.pick(i, avoid);
                            let url = selector_c.url(mirror_idx).clone();
                            let mut pb = pb_arc.load(Ordering::Relaxed);
                            let (before, started) = (pb, std::time::Instant::now());
                            let res = loop {
                                let spec = PartSpec {
                                    isolation,
                                    url: url.clone(),
                                    dest: dest.clone(),
                                    range,
                                    expected_total: Some(total),
                                    control: control_c.clone(),
                                    live: live_c.clone(),
                                };
                                tokio::select! {
                                    r = download_part(transport_c.clone(), spec, bd.clone(), &mut pb) => break r,
                                    // The monitor found this connection far slower than its siblings:
                                    // continue on a new circuit from the saved offset. Not a failure.
                                    _ = restart_c.notified() => invalidate_c(isolation),
                                }
                            };
                            match &res {
                                Ok(()) => selector_c.record_success(
                                    mirror_idx,
                                    pb.saturating_sub(before),
                                    started.elapsed().as_secs_f64(),
                                ),
                                Err(e) => {
                                    if e.downcast_ref::<mirror::MirrorMismatch>().is_some() {
                                        selector_c.ban(mirror_idx);
                                    } else {
                                        selector_c.record_failure(mirror_idx);
                                    }
                                    last_mirror.store(mirror_idx, Ordering::Relaxed);
                                }
                            }
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

    // Restart single slow connections (rationed, see `slowparts`).
    let slow_monitor =
        (parts_count >= 3 && info.accepts_ranges && crate::config::config().restart_slow_parts)
            .then(|| {
                let probes = plan
                    .iter()
                    .enumerate()
                    .map(|(i, part)| slowparts::PartProbe {
                        live: live_counters[i].clone(),
                        initial: initial_parts_done.get(i).copied().unwrap_or(0),
                        total: part.len(),
                        restart: restart_signals[i].clone(),
                    })
                    .collect();
                tokio::spawn(slowparts::monitor(
                    probes,
                    control.clone(),
                    slowparts::current_limits(),
                ))
            });

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
    if let Some(monitor) = slow_monitor {
        monitor.abort();
    }

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

    finalize_download(
        FinishCtx {
            job,
            transport: &*transport,
            selector: &selector,
            progress: &progress,
            temp_dir: &temp_dir,
            last_modified: info.last_modified.as_deref(),
            accepts_ranges: info.accepts_ranges,
        },
        &part_paths,
        total,
    )
    .await
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

    #[tokio::test]
    async fn resolve_size_falls_back_to_get_when_head_fails() {
        struct HeadFailTransport;
        #[async_trait::async_trait]
        impl Transport for HeadFailTransport {
            async fn head(
                &self,
                _url: &url::Url,
                _isolation: u64,
            ) -> Result<crate::contract::ResponseHead> {
                anyhow::bail!("400 Bad Request");
            }
            async fn get(
                &self,
                url: &url::Url,
                _range: Option<ByteRange>,
                _isolation: u64,
            ) -> Result<crate::contract::Response> {
                Ok(crate::contract::Response {
                    head: crate::contract::ResponseHead {
                        status: 206,
                        final_url: url.clone(),
                        content_length: Some(1),
                        total_size: Some(42_000),
                        accepts_ranges: true,
                        filename: Some("test.bin".into()),
                        etag: Some("test-etag".into()),
                        last_modified: None,
                    },
                    body: Box::pin(futures::stream::empty()),
                })
            }
        }

        let url = url::Url::parse("https://dl3.vimm.net/download").unwrap();
        let info = resolve_size(&HeadFailTransport, &url).await.unwrap();
        assert_eq!(info.total, Some(42_000));
        assert!(info.accepts_ranges);
        assert_eq!(info.etag.as_deref(), Some("test-etag"));
    }

    // ── conditional download ──

    async fn run_with_copy(
        copy: Option<PathBuf>,
        res: MockResource,
    ) -> (Result<()>, JobProgress, MockTransport) {
        let t = MockTransport::new().with_resource("http://x.test/f", res);
        let dir = std::env::temp_dir().join(format!("rdl-engine-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut job = DownloadJob::new(
            url::Url::parse("http://x.test/f").unwrap(),
            dir.join("o.bin"),
            2,
        );
        job.existing_copy = copy;
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
            Arc::new(t.clone()),
            Arc::new(|_| {}),
            progress.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        let p = progress.lock().await.clone();
        (res, p, t)
    }

    fn local_copy(content: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rdl-local-{}.bin", Uuid::new_v4()));
        std::fs::write(&p, content).unwrap();
        p
    }

    fn get_count(t: &MockTransport) -> usize {
        t.requests()
            .iter()
            .filter(|r| matches!(r, crate::contract::RecordedRequest::Get { .. }))
            .count()
    }

    #[tokio::test]
    async fn current_local_copy_skips_the_download() {
        let data = payload(1_000);
        let local = local_copy(&data);
        let mut res = MockResource::new(data);
        res.last_modified = Some("Wed, 01 Jan 2020 00:00:00 GMT".into()); // older than the file
        let (r, p, t) = run_with_copy(Some(local.clone()), res).await;
        r.unwrap();
        assert_eq!(p.status, JobStatus::Done);
        assert_eq!(get_count(&t), 0, "nothing may be downloaded");
        let _ = std::fs::remove_file(local);
    }

    #[tokio::test]
    async fn outdated_local_copy_is_downloaded_again() {
        let data = payload(1_000);
        let local = local_copy(&data);
        let mut res = MockResource::new(data);
        res.last_modified = Some("Wed, 01 Jan 2099 00:00:00 GMT".into()); // newer than the file
        let (r, p, t) = run_with_copy(Some(local.clone()), res).await;
        r.unwrap();
        assert_eq!(p.status, JobStatus::Done);
        assert!(get_count(&t) > 0);
        let _ = std::fs::remove_file(local);
    }

    #[tokio::test]
    async fn local_copy_of_other_size_is_downloaded_again() {
        let local = local_copy(b"short");
        let mut res = MockResource::new(payload(1_000));
        res.last_modified = Some("Wed, 01 Jan 2020 00:00:00 GMT".into());
        let (r, _, t) = run_with_copy(Some(local.clone()), res).await;
        r.unwrap();
        assert!(get_count(&t) > 0);
        let _ = std::fs::remove_file(local);
    }

    // ── mirrors and piece repair ──

    async fn run_job_with_mirrors(
        transport: MockTransport,
        primary: &str,
        mirrors: &[&str],
        pieces: Option<pieces::PieceHashes>,
        parts: usize,
    ) -> (Result<()>, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("rdl-engine-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("out.bin");
        let mut job = DownloadJob::new(url::Url::parse(primary).unwrap(), output.clone(), parts);
        job.mirrors = mirrors
            .iter()
            .map(|m| url::Url::parse(m).unwrap())
            .collect();
        job.pieces = pieces;
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
            Arc::new(transport),
            Arc::new(|_| {}),
            progress,
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        let data = std::fs::read(&output).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        (res, data)
    }

    fn hosts_used(t: &MockTransport) -> std::collections::HashSet<String> {
        t.requests()
            .iter()
            .filter_map(|r| match r {
                crate::contract::RecordedRequest::Get {
                    url,
                    range: Some(_),
                    ..
                } => Some(url.host_str().unwrap().to_string()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn parts_are_spread_over_mirrors() {
        let data = payload(8_000);
        let t = MockTransport::new()
            .with_resource("http://a.test/f", MockResource::new(data.clone()))
            .with_resource("http://b.test/f", MockResource::new(data.clone()));
        let (r, got) =
            run_job_with_mirrors(t.clone(), "http://a.test/f", &["http://b.test/f"], None, 4).await;
        r.unwrap();
        assert_eq!(got, data);
        assert_eq!(hosts_used(&t).len(), 2, "both mirrors must serve parts");
    }

    #[tokio::test]
    async fn dead_primary_is_replaced_by_a_mirror() {
        let data = payload(5_000);
        // Only the mirror exists; the primary answers with an error.
        let t =
            MockTransport::new().with_resource("http://b.test/f", MockResource::new(data.clone()));
        let (r, got) =
            run_job_with_mirrors(t, "http://a.test/f", &["http://b.test/f"], None, 3).await;
        r.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn mirror_with_another_file_size_is_never_used() {
        let data = payload(6_000);
        let t = MockTransport::new()
            .with_resource("http://a.test/f", MockResource::new(data.clone()))
            .with_resource("http://evil.test/f", MockResource::new(payload(7_777)));
        let (r, got) =
            run_job_with_mirrors(t, "http://a.test/f", &["http://evil.test/f"], None, 4).await;
        r.unwrap();
        assert_eq!(
            got, data,
            "no byte of the wrong file may end up in the result"
        );
    }

    fn piece_table(data: &[u8], length: usize) -> pieces::PieceHashes {
        pieces::PieceHashes::new(
            checksum::HashAlgo::Sha1,
            length as u64,
            data.chunks(length)
                .map(|c| checksum::hash_bytes(checksum::HashAlgo::Sha1, c))
                .collect(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn damaged_pieces_are_repaired_from_another_mirror() {
        let good = payload(4_000);
        // Mirror a has the right size but wrong bytes in the first half, which is
        // exactly the part it is asked for (part 0 goes to the first mirror).
        let mut bad = good.clone();
        bad[..2_000].iter_mut().for_each(|b| *b ^= 0xff);
        let t = MockTransport::new()
            .with_resource("http://a.test/f", MockResource::new(bad))
            .with_resource("http://b.test/f", MockResource::new(good.clone()));
        let table = piece_table(&good, 500);
        let (r, got) = run_job_with_mirrors(
            t.clone(),
            "http://a.test/f",
            &["http://b.test/f"],
            Some(table),
            2,
        )
        .await;
        r.unwrap();
        assert_eq!(got, good, "damaged pieces must have been replaced");
        // Repair fetches single 500-byte pieces.
        let piece_requests = t
            .requests()
            .iter()
            .filter(|r| {
                matches!(r, crate::contract::RecordedRequest::Get { range: Some(r), .. }
                    if r.end.map(|e| e - r.start + 1) == Some(500))
            })
            .count();
        assert!(
            piece_requests >= 4,
            "expected piece-wise repair requests, saw {piece_requests}"
        );
    }

    #[tokio::test]
    async fn unrepairable_pieces_fail_the_job_and_remove_the_file() {
        let good = payload(2_000);
        let mut bad = good.clone();
        bad[0] ^= 0xff;
        // The only mirror serves the damaged data.
        let t = MockTransport::new().with_resource("http://a.test/f", MockResource::new(bad));
        let table = piece_table(&good, 500);
        let (r, got) = run_job_with_mirrors(t, "http://a.test/f", &[], Some(table), 1).await;
        assert!(r.is_err());
        assert!(
            got.is_empty(),
            "a file that fails verification must not stay behind"
        );
    }

    #[test]
    fn jobs_pick_up_import_hints() {
        let primary = url::Url::parse("http://hinted.test/engine-hint").unwrap();
        let mirror = url::Url::parse("http://mirror.test/engine-hint").unwrap();
        mirror::hints::register(&primary, vec![mirror.clone()], None);
        let job = DownloadJob::new(primary, PathBuf::from("/tmp/x"), 2);
        assert_eq!(job.mirrors, vec![mirror]);
    }

    // ── servers that report no size ──

    fn no_length_resource(data: &[u8]) -> MockResource {
        let mut res = MockResource::new(data.to_vec());
        res.hide_size = true;
        res
    }

    #[tokio::test]
    async fn download_without_content_length_completes() {
        let data = payload(9_000);
        let t = MockTransport::new()
            .with_resource("http://x.test/f", no_length_resource(&data))
            .with_chunk_size(1_000);
        let (r, got, p) = run(t.clone(), "http://x.test/f", 4).await;
        r.unwrap();
        assert_eq!(got, data);
        assert_eq!(p.status, JobStatus::Done);
        assert_eq!(
            p.total_bytes, 9_000,
            "the size is known once the transfer ends"
        );
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
        assert_eq!(ranged, 0, "no Range request when the size is unknown");
    }

    #[tokio::test]
    async fn download_without_content_length_survives_a_dropped_connection() {
        let data = payload(4_000);
        let t = MockTransport::new().with_resource("http://x.test/f", no_length_resource(&data));
        t.truncate_next_body_after(700);
        let (r, got, _) = run(t, "http://x.test/f", 2).await;
        r.unwrap();
        assert_eq!(
            got, data,
            "the retry restarts and delivers the complete file"
        );
    }

    #[tokio::test]
    async fn empty_response_without_length_is_an_empty_file() {
        let t = MockTransport::new().with_resource("http://x.test/f", no_length_resource(b""));
        let (r, got, p) = run(t, "http://x.test/f", 1).await;
        r.unwrap();
        assert!(got.is_empty());
        assert_eq!(p.status, JobStatus::Done);
    }

    // ── pause, cancel and slow-part restarts against a throttled transport ──

    use std::sync::atomic::AtomicUsize;

    /// Serves `data` in chunks with a delay per chunk. `delay_for(request_no, range_start)`
    /// decides how slow each request is, so a test can make one connection crawl.
    struct ThrottledTransport {
        data: Vec<u8>,
        chunk: usize,
        delay_for: Box<dyn Fn(usize, u64) -> Duration + Send + Sync>,
        chunks_served: Arc<AtomicUsize>,
        requests: Arc<std::sync::Mutex<Vec<(u64, u64)>>>, // (range start, isolation)
    }

    use std::time::Duration;

    #[async_trait::async_trait]
    impl Transport for ThrottledTransport {
        async fn head(&self, url: &url::Url, _: u64) -> Result<crate::contract::ResponseHead> {
            Ok(crate::contract::ResponseHead {
                status: 200,
                final_url: url.clone(),
                content_length: Some(self.data.len() as u64),
                total_size: Some(self.data.len() as u64),
                accepts_ranges: true,
                filename: None,
                etag: None,
                last_modified: None,
            })
        }

        async fn get(
            &self,
            url: &url::Url,
            range: Option<ByteRange>,
            isolation: u64,
        ) -> Result<crate::contract::Response> {
            let (start, end) = match range {
                Some(r) => (
                    r.start as usize,
                    r.end.map_or(self.data.len(), |e| e as usize + 1),
                ),
                None => (0, self.data.len()),
            };
            let request_no = {
                let mut r = self.requests.lock().unwrap();
                r.push((start as u64, isolation));
                r.len()
            };
            let delay = (self.delay_for)(request_no, start as u64);
            let slice = bytes::Bytes::copy_from_slice(&self.data[start..end]);
            let (chunk, served) = (self.chunk, self.chunks_served.clone());
            let head = crate::contract::ResponseHead {
                status: if range.is_some() { 206 } else { 200 },
                final_url: url.clone(),
                content_length: Some(slice.len() as u64),
                total_size: Some(self.data.len() as u64),
                accepts_ranges: true,
                filename: None,
                etag: None,
                last_modified: None,
            };
            let body = futures::stream::unfold(0usize, move |pos| {
                let (slice, served) = (slice.clone(), served.clone());
                async move {
                    if pos >= slice.len() {
                        return None;
                    }
                    tokio::time::sleep(delay).await;
                    let n = chunk.min(slice.len() - pos);
                    served.fetch_add(1, Ordering::SeqCst);
                    Some((Ok(slice.slice(pos..pos + n)), pos + n))
                }
            });
            Ok(crate::contract::Response {
                head,
                body: Box::pin(body),
            })
        }
    }

    fn new_progress(id: Uuid) -> Arc<Mutex<JobProgress>> {
        Arc::new(Mutex::new(JobProgress {
            id,
            url: String::new(),
            filename: String::new(),
            total_bytes: 0,
            bytes_done: 0,
            parts: vec![],
            status: JobStatus::Queued,
        }))
    }

    fn temp_output() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rdl-ctl-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("o.bin"))
    }

    #[tokio::test]
    async fn a_paused_download_stops_and_resumes_with_the_right_content() {
        let data = payload(60_000);
        let served = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(ThrottledTransport {
            data: data.clone(),
            chunk: 1_000,
            delay_for: Box::new(|_, _| Duration::from_millis(15)),
            chunks_served: served.clone(),
            requests: Default::default(),
        });
        let (dir, output) = temp_output();
        let job = DownloadJob::new(
            url::Url::parse("http://x.test/f").unwrap(),
            output.clone(),
            1,
        );
        let control = control::get_or_create(job.id);
        let progress = new_progress(job.id);
        let task = {
            let (progress, t) = (progress.clone(), transport.clone());
            let job_id = job.id;
            let out = output.clone();
            tokio::spawn(async move {
                let job = DownloadJob {
                    id: job_id,
                    ..DownloadJob::new(url::Url::parse("http://x.test/f").unwrap(), out, 1)
                };
                run_job_with(
                    &job,
                    t,
                    Arc::new(|_| {}),
                    progress,
                    Arc::new(tokio::sync::Notify::new()),
                )
                .await
            })
        };
        // Slow machines (CI) need a while before the first chunk arrives: wait for data.
        for _ in 0..500 {
            if served.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        control.pause();
        // Chunks already in flight may still arrive; after that nothing moves any more.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let frozen = served.load(Ordering::SeqCst);
        assert!(
            frozen > 0 && frozen < 60,
            "paused mid-way (served {frozen})"
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            served.load(Ordering::SeqCst),
            frozen,
            "no data may flow while paused"
        );
        assert!(!task.is_finished());

        control.resume();
        task.await.unwrap().unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), data);
        control::remove(job.id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_cancelled_download_stops_the_transfer() {
        let served = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(ThrottledTransport {
            data: payload(200_000),
            chunk: 1_000,
            delay_for: Box::new(|_, _| Duration::from_millis(15)),
            chunks_served: served.clone(),
            requests: Default::default(),
        });
        let (dir, output) = temp_output();
        let job = DownloadJob::new(url::Url::parse("http://x.test/f").unwrap(), output, 2);
        let control = control::get_or_create(job.id);
        let cancel = control.cancel.clone();
        let progress = new_progress(job.id);
        let task = tokio::spawn({
            let t = transport.clone();
            async move { run_job_with(&job, t, Arc::new(|_| {}), progress, cancel).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        control.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("stops promptly")
            .unwrap();
        assert!(result.is_err(), "a cancelled job is not a success");
        let after = served.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(served.load(Ordering::SeqCst), after, "no data after cancel");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn one_crawling_connection_is_restarted_on_a_new_circuit() {
        let part = 240_000usize;
        let data = payload(4 * part);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = Arc::new(ThrottledTransport {
            data: data.clone(),
            chunk: 2_000,
            // The first connection of part 0 crawls; everything else is fast.
            delay_for: Box::new(move |no, start| {
                if start < part as u64 && no <= 4 {
                    Duration::from_millis(400) // 5 KB/s
                } else {
                    Duration::from_millis(10) // 200 KB/s
                }
            }),
            chunks_served: Default::default(),
            requests: requests.clone(),
        });
        let (dir, output) = temp_output();
        let mut job = DownloadJob::new(
            url::Url::parse("http://x.test/f").unwrap(),
            output.clone(),
            4,
        );
        job.part_size = Some(part as u64);
        let progress = new_progress(job.id);
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            run_job_with(
                &job,
                transport,
                Arc::new(|_| {}),
                progress,
                Arc::new(tokio::sync::Notify::new()),
            ),
        )
        .await
        .expect("finishes");
        result.unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), data);

        let reqs = requests.lock().unwrap().clone();
        let first_part: Vec<_> = reqs
            .iter()
            .filter(|(start, _)| *start < part as u64)
            .collect();
        assert!(
            first_part.len() >= 2,
            "part 0 must have been restarted: {reqs:?}"
        );
        assert_ne!(first_part[0].1, 0);
        assert!(first_part.len() <= 3, "restarts must be rationed: {reqs:?}");
        let others = reqs
            .iter()
            .filter(|(start, _)| *start >= part as u64)
            .count();
        assert_eq!(others, 3, "the fast parts are never restarted: {reqs:?}");
        control::remove(job.id);
        let _ = std::fs::remove_dir_all(dir);
    }
}

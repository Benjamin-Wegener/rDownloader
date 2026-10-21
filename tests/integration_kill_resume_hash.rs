//! Integration test for Definition of Done M1: Kill + Resume + Hash verification.
//!
//! Tests the complete flow:
//! 1. Multi-part range downloading using ResumeState metadata and part offsets.
//! 2. Simulated process kill / network interruption mid-download.
//! 3. Resume from saved part offsets and validation against ETag.
//! 4. Merging completed parts into the target output file.
//! 5. Verifying that the merged file's SHA-256 hash matches the expected checksum.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;
use uuid::Uuid;

use common::{fetch, ServerConfig, TestServer, PATH_FILE};
use rdownloader::download::checksum::{self, Checksum, HashAlgo};
use rdownloader::download::resume::{PartOffset, ResumeState};

/// Helper to manage a temporary directory that is cleaned up on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let p = std::env::temp_dir().join(format!("rdl-test-{prefix}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).expect("create temp dir");
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Downloads a part via the test HTTP client with Range headers, supporting resume.
/// Writes to `dest` and updates `bytes_done`. If `abort_after` is reached, stops prematurely.
async fn download_part_direct(
    url: &Url,
    dest: &Path,
    range: (u64, u64),
    bytes_done: Arc<AtomicU64>,
    abort_after: Option<u64>,
) -> anyhow::Result<bool> {
    let (start, end) = range;
    let existing_len = if dest.exists() {
        fs::metadata(dest).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    if existing_len >= (end - start + 1) {
        return Ok(true); // Part already complete
    }

    let resume_start = start + existing_len;
    let effective_range = Some((resume_start, Some(end)));

    let socket_addr = url
        .socket_addrs(|| None)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no socket addr for {}", url))?;

    let resp = fetch(socket_addr, url.path(), effective_range).await?;
    if resp.status != 206 && resp.status != 200 {
        anyhow::bail!("unexpected status {}", resp.status);
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(existing_len > 0)
        .truncate(existing_len == 0)
        .open(dest)
        .await?;

    let mut bytes_to_write = resp.body.as_slice();
    let mut finished = true;

    if let Some(limit) = abort_after {
        if (bytes_to_write.len() as u64) > limit {
            bytes_to_write = &bytes_to_write[..limit as usize];
            finished = false;
        }
    }

    file.write_all(bytes_to_write).await?;
    file.flush().await?;

    let written = bytes_to_write.len() as u64;
    bytes_done.fetch_add(written, Ordering::SeqCst);

    Ok(finished)
}

/// Merges partial files in order into the output file.
async fn merge_parts(parts: &[PathBuf], output: &Path) -> anyhow::Result<()> {
    let mut out = File::create(output).await?;
    let mut buf = vec![0u8; 64 * 1024];
    for part in parts {
        let mut f = File::open(part).await?;
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

#[tokio::test]
async fn test_kill_resume_and_hash_verification() {
    // 1. Generate 128 KiB test payload with deterministic pattern
    let total_size = 128 * 1024;
    let payload: Vec<u8> = (0..total_size)
        .map(|i| ((i * 31 + 7) % 251) as u8)
        .collect();

    let etag = "\"etag-integration-m1\"";
    let server = TestServer::start(ServerConfig::new(payload.clone()).with_etag(etag)).await;

    let server_url = Url::parse(&server.url(PATH_FILE)).unwrap();
    let temp_workspace = TempDir::new("m1-kill-resume");
    let job_id = Uuid::new_v4();
    let job_temp_dir = temp_workspace.path().join(format!("job-{job_id}"));
    fs::create_dir_all(&job_temp_dir).await.unwrap();

    let output_file = temp_workspace.path().join("final_output.bin");

    // 2. Plan 4 parts:
    // Part 0: 0..32767
    // Part 1: 32768..65535
    // Part 2: 65536..98303
    // Part 3: 98304..131071
    let num_parts = 4;
    let part_size = (total_size / num_parts) as u64;
    let mut parts_meta = Vec::new();
    let mut part_paths = Vec::new();

    for i in 0..num_parts {
        let start = i as u64 * part_size;
        let end = if i == num_parts - 1 {
            (total_size - 1) as u64
        } else {
            start + part_size - 1
        };
        parts_meta.push(PartOffset::new(i, start, end));
        part_paths.push(job_temp_dir.join(format!("part-{i:03}")));
    }

    // Initialize initial resume state
    let mut state = ResumeState::new(
        job_id,
        server_url.clone(),
        total_size as u64,
        true,
        Some(etag.to_string()),
        None,
        parts_meta.clone(),
    );
    state.save_to_dir(&job_temp_dir).unwrap();

    // 3. PHASE 1: Simulate partial download and process kill / abort
    let bytes_done = Arc::new(AtomicU64::new(0));

    // Part 0: downloads completely (32 KiB)
    let p0_ok = download_part_direct(
        &server_url,
        &part_paths[0],
        (parts_meta[0].start, parts_meta[0].end),
        bytes_done.clone(),
        None,
    )
    .await
    .unwrap();
    assert!(p0_ok);

    // Part 1: downloads only 10 KiB then aborts (kill simulated)
    let p1_ok = download_part_direct(
        &server_url,
        &part_paths[1],
        (parts_meta[1].start, parts_meta[1].end),
        bytes_done.clone(),
        Some(10 * 1024),
    )
    .await
    .unwrap();
    assert!(!p1_ok);

    // Part 2: downloads only 5 KiB then aborts
    let p2_ok = download_part_direct(
        &server_url,
        &part_paths[2],
        (parts_meta[2].start, parts_meta[2].end),
        bytes_done.clone(),
        Some(5 * 1024),
    )
    .await
    .unwrap();
    assert!(!p2_ok);

    // Part 3: untouched (0 bytes)

    // Save partial progress in ResumeState as the process is "killed"
    for (i, part) in parts_meta.iter_mut().enumerate() {
        let written = if part_paths[i].exists() {
            fs::metadata(&part_paths[i]).await.unwrap().len()
        } else {
            0
        };
        part.bytes_written = written;
        part.done = written >= part.total();
    }
    state.parts = parts_meta.clone();
    state.save_to_dir(&job_temp_dir).unwrap();

    assert_eq!(parts_meta[0].bytes_written, 32768);
    assert_eq!(parts_meta[1].bytes_written, 10240);
    assert_eq!(parts_meta[2].bytes_written, 5120);
    assert_eq!(parts_meta[3].bytes_written, 0);

    // 4. PHASE 2: Restart / Resume
    // Process boots up, finds existing ResumeState on disk, and validates ETag
    let loaded_state = ResumeState::load_from_dir(&job_temp_dir)
        .unwrap()
        .expect("state exists");
    assert!(loaded_state
        .validate(total_size as u64, Some(etag), None)
        .is_ok());

    // Resume each part to completion
    for (i, (part_meta, part_path)) in loaded_state.parts.iter().zip(part_paths.iter()).enumerate()
    {
        let ok = download_part_direct(
            &server_url,
            part_path,
            (part_meta.start, part_meta.end),
            bytes_done.clone(),
            None,
        )
        .await
        .unwrap();
        assert!(ok, "Part {i} must complete on resume");
    }

    // Verify all parts are complete
    for (i, (part, part_path)) in loaded_state.parts.iter().zip(part_paths.iter()).enumerate() {
        let len = fs::metadata(part_path).await.unwrap().len();
        assert_eq!(len, part.total(), "Part {i} length mismatch");
    }

    // 5. PHASE 3: Merge and Hash verification
    merge_parts(&part_paths, &output_file).await.unwrap();

    // Verify file content matches original payload byte for byte
    let merged_data = fs::read(&output_file).await.unwrap();
    assert_eq!(merged_data.len(), payload.len());
    assert_eq!(merged_data, payload);

    // Verify SHA-256 hash using checksum::verify_file
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    let expected_hex = format!("{:x}", hasher.finalize());

    let checksum = Checksum::new(HashAlgo::Sha256, &expected_hex).unwrap();
    let verify_result = checksum::verify_file(&output_file, &checksum).await;
    assert!(
        verify_result.is_ok(),
        "Checksum verification must succeed: {verify_result:?}"
    );
}

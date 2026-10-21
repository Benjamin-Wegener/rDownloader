use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use url::Url;
use uuid::Uuid;

/// Progress and bounds of a single part for resuming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartOffset {
    pub index: usize,
    pub start: u64,
    pub end: u64,
    pub bytes_written: u64,
    pub done: bool,
}

impl PartOffset {
    pub fn new(index: usize, start: u64, end: u64) -> Self {
        Self {
            index,
            start,
            end,
            bytes_written: 0,
            done: false,
        }
    }

    /// Total bytes expected for this part.
    pub fn total(&self) -> u64 {
        if self.end >= self.start {
            self.end - self.start + 1
        } else {
            0
        }
    }

    /// Current absolute file offset to resume from for this part.
    pub fn current_offset(&self) -> u64 {
        self.start.saturating_add(self.bytes_written)
    }

    /// Remaining bytes for this part.
    pub fn remaining(&self) -> u64 {
        self.total().saturating_sub(self.bytes_written)
    }
}

/// Persistent resume metadata stored in `<temp_dir>/job-<id>/meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeState {
    pub job_id: Uuid,
    pub url: Url,
    pub total_bytes: u64,
    pub supports_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub parts: Vec<PartOffset>,
}

impl ResumeState {
    pub fn new(
        job_id: Uuid,
        url: Url,
        total_bytes: u64,
        supports_ranges: bool,
        etag: Option<String>,
        last_modified: Option<String>,
        parts: Vec<PartOffset>,
    ) -> Self {
        Self {
            job_id,
            url,
            total_bytes,
            supports_ranges,
            etag,
            last_modified,
            parts,
        }
    }

    /// Path to meta.json inside a job's temp directory.
    pub fn meta_file(temp_dir: &Path) -> PathBuf {
        temp_dir.join("meta.json")
    }

    /// Load resume state from disk.
    pub fn load_from_dir(temp_dir: &Path) -> Result<Option<Self>> {
        let path = Self::meta_file(temp_dir);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read resume meta file: {path:?}"))?;
        let state: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse resume meta JSON: {path:?}"))?;
        Ok(Some(state))
    }

    /// Save resume state atomically to disk.
    pub fn save_to_dir(&self, temp_dir: &Path) -> Result<()> {
        let path = Self::meta_file(temp_dir);
        let tmp_path = temp_dir.join("meta.json.tmp");
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize resume state to JSON")?;
        std::fs::write(&tmp_path, content)
            .with_context(|| format!("Failed to write temporary resume file: {tmp_path:?}"))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to replace resume file with: {path:?}"))?;
        Ok(())
    }

    /// Validate that the remote resource metadata matches our recorded resume state.
    pub fn validate(
        &self,
        new_total: u64,
        new_etag: Option<&str>,
        new_last_modified: Option<&str>,
    ) -> Result<()> {
        if self.total_bytes != new_total {
            bail!(
                "Total file size changed: previously {}, now {}",
                self.total_bytes,
                new_total
            );
        }

        // If we have an ETag and server provides one, they must match
        if let (Some(saved), Some(current)) = (&self.etag, new_etag) {
            if saved != current {
                bail!("ETag mismatch: saved '{saved}' != remote '{current}'");
            }
        }

        // If we have Last-Modified and server provides one, they must match
        if let (Some(saved), Some(current)) = (&self.last_modified, new_last_modified) {
            if saved != current {
                bail!("Last-Modified mismatch: saved '{saved}' != remote '{current}'");
            }
        }

        Ok(())
    }

    /// Calculate total bytes already written across all parts.
    pub fn total_bytes_done(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes_written).sum()
    }

    /// Checks if all parts are completely downloaded.
    pub fn is_complete(&self) -> bool {
        !self.parts.is_empty() && self.parts.iter().all(|p| p.done || p.remaining() == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_offset_calculations() {
        let mut p = PartOffset::new(0, 100, 199);
        assert_eq!(p.total(), 100);
        assert_eq!(p.current_offset(), 100);
        assert_eq!(p.remaining(), 100);

        p.bytes_written = 40;
        assert_eq!(p.current_offset(), 140);
        assert_eq!(p.remaining(), 60);

        p.bytes_written = 100;
        assert_eq!(p.current_offset(), 200);
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn validation_checks() {
        let state = ResumeState::new(
            Uuid::new_v4(),
            Url::parse("http://example.com/file").unwrap(),
            1000,
            true,
            Some("\"abc-123\"".into()),
            Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
            vec![PartOffset::new(0, 0, 999)],
        );

        // Valid matching metadata
        assert!(state
            .validate(
                1000,
                Some("\"abc-123\""),
                Some("Wed, 21 Oct 2026 07:28:00 GMT")
            )
            .is_ok());

        // Size mismatch
        assert!(state.validate(2000, Some("\"abc-123\""), None).is_err());

        // ETag mismatch
        assert!(state.validate(1000, Some("\"diff-999\""), None).is_err());

        // Last-Modified mismatch
        assert!(state
            .validate(1000, None, Some("Thu, 22 Oct 2026 07:28:00 GMT"))
            .is_err());
    }

    #[test]
    fn save_and_load_resume_state() {
        let unique = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("rdl-test-resume-{}", unique));
        std::fs::create_dir_all(&dir).unwrap();

        let state = ResumeState::new(
            unique,
            Url::parse("http://example.com/data.bin").unwrap(),
            500,
            true,
            Some("etag1".into()),
            None,
            vec![PartOffset::new(0, 0, 249), PartOffset::new(1, 250, 499)],
        );

        state.save_to_dir(&dir).unwrap();
        let loaded = ResumeState::load_from_dir(&dir).unwrap().unwrap();
        assert_eq!(state, loaded);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! History of the finished downloads.
//!
//! A download reaches the history as soon as it ends (done, failed or
//! cancelled). The history view shows the entries with their statistics, the
//! exports write them to a file (see [`super::export`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::download::JobStatus;

use super::TrackEntry;

/// A finished download with the statistics of the history view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: Uuid,
    pub filename: String,
    pub url: String,
    pub output: String,
    pub status: JobStatus,
    pub total_bytes: u64,
    pub bytes_done: u64,
    pub parts: usize,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// Wall-clock duration of the download in seconds.
    pub duration_secs: u64,
    /// Average throughput in bytes per second.
    pub avg_bytes_per_sec: u64,
}

impl HistoryItem {
    /// Stable, machine-readable name of the status (used in the exports).
    pub fn status_name(&self) -> &'static str {
        match self.status {
            JobStatus::Queued => "queued",
            JobStatus::Resolving => "resolving",
            JobStatus::Downloading => "downloading",
            JobStatus::Paused => "paused",
            JobStatus::Merging => "merging",
            JobStatus::Done => "done",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Failed(_) => "failed",
        }
    }
}

impl TrackEntry {
    /// Builds the history item of this entry (with the computed statistics).
    pub fn to_history_item(&self) -> HistoryItem {
        let duration_secs = (self.updated_at - self.created_at).num_seconds().max(0) as u64;
        let avg_bytes_per_sec = self.bytes_done.checked_div(duration_secs).unwrap_or(0);
        HistoryItem {
            id: self.id,
            filename: self.filename.clone(),
            url: self.url.clone(),
            output: self.output.clone(),
            status: self.status.clone(),
            total_bytes: self.total_bytes,
            bytes_done: self.bytes_done,
            parts: self.tor_parts,
            started_at: self.created_at,
            finished_at: self.updated_at,
            duration_secs,
            avg_bytes_per_sec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn entry(status: JobStatus) -> TrackEntry {
        let now = Utc::now();
        TrackEntry {
            id: Uuid::new_v4(),
            url: "http://example.com/a.bin".into(),
            filename: "a.bin".into(),
            output: "/tmp/a.bin".into(),
            total_bytes: 1000,
            bytes_done: 1000,
            status,
            created_at: now,
            updated_at: now + Duration::seconds(10),
            tor_parts: 4,
            etag: None,
            last_modified: None,
        }
    }

    #[test]
    fn statistics_are_computed_from_the_entry() {
        let item = entry(JobStatus::Done).to_history_item();
        assert_eq!(item.duration_secs, 10);
        assert_eq!(item.avg_bytes_per_sec, 100);
        assert_eq!(item.parts, 4);
        assert_eq!(item.status_name(), "done");
    }

    #[test]
    fn a_download_without_duration_has_no_average_speed() {
        let mut e = entry(JobStatus::Failed("boom".into()));
        e.updated_at = e.created_at;
        let item = e.to_history_item();
        assert_eq!(item.duration_secs, 0);
        assert_eq!(item.avg_bytes_per_sec, 0);
        assert_eq!(item.status_name(), "failed");
    }
}

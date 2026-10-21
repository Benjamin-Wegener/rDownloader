//! Progress tracking and persistence (JSON store under `~/.local/share/rdownloader/`).
//!
//! The tracker is the session store of the app: every download is written to
//! disk while it runs, so that the next start can continue the open ones
//! (see [`Tracker::open_entries`] and `restore_session` in `main.rs`).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::warn;
use uuid::Uuid;

use crate::download::{DownloadJob, JobProgress, JobStatus};

pub mod export;
pub mod history;

use history::HistoryItem;

/// What the app does with a tracked download when it starts again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    /// The download is continued; the engine resumes from its temp directory.
    Continue,
    /// The download is shown in the queue, but not started (paused, or no Tor).
    KeepPaused,
    /// Nothing to do: the file was already merged completely.
    Finished,
}

/// A download entry in the tracker (persisted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackEntry {
    pub id: Uuid,
    pub url: String,
    pub filename: String,
    pub output: String,
    pub total_bytes: u64,
    pub bytes_done: u64,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tor_parts: usize,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

impl TrackEntry {
    pub fn from_progress(p: &JobProgress, output: &str, tor_parts: usize) -> Self {
        let now = Utc::now();
        Self {
            id: p.id,
            url: p.url.clone(),
            filename: p.filename.clone(),
            output: output.to_string(),
            total_bytes: p.total_bytes,
            bytes_done: p.bytes_done,
            status: p.status.clone(),
            created_at: now,
            updated_at: now,
            tor_parts,
            etag: None,
            last_modified: None,
        }
    }

    /// Entry for a job that was just queued, before a single byte was moved.
    ///
    /// The tracker needs the entry *before* the transfer starts: a session that
    /// is killed during the download must still be restorable.
    pub fn from_job(job: &DownloadJob, output: &str, tor_parts: usize) -> Self {
        let now = Utc::now();
        let filename = Path::new(output)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| output.to_string());
        Self {
            id: job.id,
            url: job.url.to_string(),
            filename,
            output: output.to_string(),
            total_bytes: 0,
            bytes_done: 0,
            status: JobStatus::Queued,
            created_at: now,
            updated_at: now,
            tor_parts,
            etag: None,
            last_modified: None,
        }
    }

    /// True while the job is not in a terminal state.
    ///
    /// Terminal states are done, failed and cancelled: such a job is not
    /// continued in a later session.
    pub fn is_open(&self) -> bool {
        !matches!(
            self.status,
            JobStatus::Done | JobStatus::Failed(_) | JobStatus::Cancelled
        )
    }

    /// True when the job was in flight when the app stopped.
    ///
    /// A paused job stays paused after a restart, a running one is continued.
    pub fn was_running(&self) -> bool {
        matches!(
            self.status,
            JobStatus::Queued | JobStatus::Resolving | JobStatus::Downloading | JobStatus::Merging
        )
    }

    /// True when the output file exists and is complete.
    ///
    /// A download that was merged right before the shutdown has no resume data
    /// left, so it must not be downloaded a second time.
    pub fn output_complete(&self) -> bool {
        if self.total_bytes == 0 {
            return false;
        }
        match std::fs::metadata(Path::new(&self.output)) {
            Ok(m) => m.is_file() && m.len() == self.total_bytes,
            Err(_) => false,
        }
    }

    /// Rebuilds the job of this entry.
    ///
    /// The persisted id is kept: the engine derives the temp directory from it
    /// and therefore finds the resume data of the previous session.
    pub fn to_download_job(&self) -> Result<DownloadJob> {
        let url = url::Url::parse(&self.url)
            .with_context(|| format!("Invalid URL in tracker: {}", self.url))?;
        let mut job = DownloadJob::new(url, PathBuf::from(&self.output), self.tor_parts);
        job.id = self.id;
        Ok(job)
    }

    /// Decides what happens to this entry on the next start.
    ///
    /// `can_download` is false when no transport is available (e.g. `--no-tor`).
    pub fn restore_action(&self, can_download: bool) -> RestoreAction {
        if self.output_complete() {
            return RestoreAction::Finished;
        }
        if self.was_running() && can_download {
            return RestoreAction::Continue;
        }
        RestoreAction::KeepPaused
    }
}

/// Thread-safe, persisting tracker.
#[derive(Clone)]
pub struct Tracker {
    path: PathBuf,
    entries: Arc<Mutex<Vec<TrackEntry>>>,
}

impl Tracker {
    /// Directory of the app's data files (tracker and exports).
    pub fn data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rdownloader")
    }

    /// Default location of the tracker file.
    pub fn default_path() -> PathBuf {
        Self::data_dir().join("downloads.json")
    }

    /// Opens the tracker stored at `path` (tests use a temporary file).
    pub fn at(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).context("Create tracker directory")?;
        }
        let entries = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Tracker file {path:?} is unreadable ({e}); starting empty");
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).context("Load tracker"),
        };
        Ok(Self {
            path,
            entries: Arc::new(Mutex::new(entries)),
        })
    }

    pub fn new() -> Result<Self> {
        Self::at(Self::default_path())
    }

    pub fn add(&self, entry: TrackEntry) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        // Upsert: same id replaces
        if let Some(existing) = list.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            list.push(entry);
        }
        self.save(&list)?;
        Ok(())
    }

    /// Adds an entry only if its id is unknown.
    ///
    /// A restored download keeps the progress it made in the previous session.
    pub fn register(&self, entry: TrackEntry) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        if !list.iter().any(|e| e.id == entry.id) {
            list.push(entry);
            self.save(&list)?;
        }
        Ok(())
    }

    pub fn update_from_progress(
        &self,
        p: &JobProgress,
        output: &str,
        tor_parts: usize,
    ) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        if let Some(e) = list.iter_mut().find(|e| e.id == p.id) {
            // A cancelled job stays cancelled, even if the engine still reports
            // progress for it.
            if !matches!(e.status, JobStatus::Cancelled) {
                e.status = p.status.clone();
            }
            e.bytes_done = p.bytes_done;
            e.total_bytes = p.total_bytes;
            e.updated_at = Utc::now();
        } else {
            let mut entry = TrackEntry::from_progress(p, output, tor_parts);
            entry.created_at = Utc::now();
            list.push(entry);
        }
        self.save(&list)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<TrackEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// All entries that were not finished when the app stopped.
    pub fn open_entries(&self) -> Vec<TrackEntry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.is_open())
            .cloned()
            .collect()
    }

    /// The finished downloads with their statistics, newest first.
    pub fn history(&self) -> Vec<HistoryItem> {
        let mut items: Vec<HistoryItem> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| !e.is_open())
            .map(|e| e.to_history_item())
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.finished_at));
        items
    }

    pub fn remove(&self, id: Uuid) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        list.retain(|e| e.id != id);
        self.save(&list)?;
        Ok(())
    }

    /// Sets the status of an entry; unknown ids are ignored.
    pub fn set_status(&self, id: Uuid, status: JobStatus) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        if let Some(e) = list.iter_mut().find(|e| e.id == id) {
            e.status = status;
            e.updated_at = Utc::now();
            self.save(&list)?;
        }
        Ok(())
    }

    /// Writes the store atomically: a kill during the write must not destroy
    /// the session state of the previous downloads.
    fn save(&self, list: &[TrackEntry]) -> Result<()> {
        let s = serde_json::to_string_pretty(list)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, s).context("Write tracker")?;
        std::fs::rename(&tmp, &self.path).context("Replace tracker file")?;
        Ok(())
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new().expect("Tracker init")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::JobStatus;

    fn temp_tracker() -> (Tracker, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rdl-tracker-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("downloads.json");
        (Tracker::at(path.clone()).unwrap(), dir)
    }

    fn entry(status: JobStatus) -> TrackEntry {
        let id = Uuid::new_v4();
        TrackEntry {
            id,
            url: "http://example.com/file.bin".into(),
            filename: "file.bin".into(),
            output: "/tmp/file.bin".into(),
            total_bytes: 100,
            bytes_done: 10,
            status,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tor_parts: 4,
            etag: None,
            last_modified: None,
        }
    }

    #[test]
    fn entries_survive_a_restart() {
        let (tracker, dir) = temp_tracker();
        let e = entry(JobStatus::Downloading);
        let id = e.id;
        tracker.add(e).unwrap();

        // New instance, same file: the entry is still there.
        let reopened = Tracker::at(dir.join("downloads.json")).unwrap();
        let list = reopened.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert!(matches!(list[0].status, JobStatus::Downloading));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn open_entries_exclude_finished_jobs() {
        let (tracker, dir) = temp_tracker();
        tracker.add(entry(JobStatus::Downloading)).unwrap();
        tracker.add(entry(JobStatus::Done)).unwrap();
        tracker.add(entry(JobStatus::Cancelled)).unwrap();
        tracker
            .add(entry(JobStatus::Failed("boom".into())))
            .unwrap();
        tracker.add(entry(JobStatus::Paused)).unwrap();

        let open = tracker.open_entries();
        assert_eq!(open.len(), 2);
        assert!(open.iter().all(|e| e.is_open()));
        // Only the running one is continued, the paused one stays paused.
        assert_eq!(open.iter().filter(|e| e.was_running()).count(), 1);
        assert_eq!(open.iter().filter(|e| !e.was_running()).count(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn to_download_job_keeps_id_and_output() {
        let e = entry(JobStatus::Downloading);
        let job = e.to_download_job().unwrap();
        assert_eq!(job.id, e.id);
        assert_eq!(job.output.to_string_lossy(), e.output);
        assert_eq!(job.max_parts, 4);
        assert_eq!(job.url.as_str(), e.url);
    }

    #[test]
    fn invalid_url_is_reported() {
        let mut e = entry(JobStatus::Downloading);
        e.url = "not a url".into();
        assert!(e.to_download_job().is_err());
    }

    #[test]
    fn output_complete_checks_the_file_size() {
        let dir = std::env::temp_dir().join(format!("rdl-tracker-out-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.bin");
        std::fs::write(&file, b"0123456789").unwrap();

        let mut e = entry(JobStatus::Downloading);
        e.output = file.to_string_lossy().to_string();
        e.total_bytes = 10;
        assert!(e.output_complete());

        e.total_bytes = 11;
        assert!(!e.output_complete());

        // Unknown size: never complete.
        e.total_bytes = 0;
        assert!(!e.output_complete());

        // Missing file: never complete.
        e.total_bytes = 10;
        e.output = dir.join("missing.bin").to_string_lossy().to_string();
        assert!(!e.output_complete());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelled_entries_stay_cancelled() {
        let (tracker, dir) = temp_tracker();
        let e = entry(JobStatus::Queued);
        let id = e.id;
        tracker.add(e).unwrap();
        tracker.set_status(id, JobStatus::Cancelled).unwrap();

        // The engine may still report progress - the entry stays cancelled.
        let mut p = JobProgress {
            id,
            url: "http://example.com/file.bin".into(),
            filename: "file.bin".into(),
            total_bytes: 100,
            bytes_done: 50,
            parts: vec![],
            status: JobStatus::Downloading,
        };
        tracker
            .update_from_progress(&p, "/tmp/file.bin", 4)
            .unwrap();
        p.bytes_done = 60;
        tracker
            .update_from_progress(&p, "/tmp/file.bin", 4)
            .unwrap();

        let list = tracker.list();
        assert!(matches!(list[0].status, JobStatus::Cancelled));
        assert_eq!(list[0].bytes_done, 60);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_action_depends_on_status_and_output() {
        let dir = std::env::temp_dir().join(format!("rdl-tracker-restore-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.bin");
        std::fs::write(&file, b"0123456789").unwrap();

        let mut e = entry(JobStatus::Downloading);
        e.output = file.to_string_lossy().to_string();
        // The file is still incomplete (100 bytes expected, 10 written).
        e.total_bytes = 100;
        // A running download with a transport is continued.
        assert_eq!(e.restore_action(true), RestoreAction::Continue);
        // Without a transport it only shows up in the queue.
        assert_eq!(e.restore_action(false), RestoreAction::KeepPaused);

        // A paused download stays paused.
        let mut paused = entry(JobStatus::Paused);
        paused.output = file.to_string_lossy().to_string();
        paused.total_bytes = 100;
        assert_eq!(paused.restore_action(true), RestoreAction::KeepPaused);

        // A finished file is not downloaded again.
        let mut done = entry(JobStatus::Downloading);
        done.output = file.to_string_lossy().to_string();
        done.total_bytes = 10;
        assert_eq!(done.restore_action(true), RestoreAction::Finished);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn register_keeps_the_progress_of_a_restored_entry() {
        let (tracker, dir) = temp_tracker();
        let mut e = entry(JobStatus::Downloading);
        e.bytes_done = 42;
        let id = e.id;
        tracker.add(e).unwrap();

        let mut fresh = entry(JobStatus::Queued);
        fresh.id = id;
        fresh.bytes_done = 0;
        tracker.register(fresh).unwrap();

        let list = tracker.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].bytes_done, 42);
        assert!(matches!(list[0].status, JobStatus::Downloading));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn history_contains_only_finished_downloads() {
        let (tracker, dir) = temp_tracker();
        let mut done = entry(JobStatus::Done);
        done.filename = "done.bin".into();
        tracker.add(done).unwrap();
        let mut failed = entry(JobStatus::Failed("boom".into()));
        failed.filename = "failed.bin".into();
        tracker.add(failed).unwrap();
        tracker.add(entry(JobStatus::Downloading)).unwrap();

        let history = tracker.history();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|h| h.status_name() != "downloading"));
        // The newest entry comes first.
        assert!(history[0].finished_at >= history[1].finished_at);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_deletes_the_entry() {
        let (tracker, dir) = temp_tracker();
        let e = entry(JobStatus::Downloading);
        let id = e.id;
        tracker.add(e).unwrap();
        tracker.remove(id).unwrap();
        assert!(tracker.list().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_tracker_file_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("rdl-tracker-bad-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("downloads.json");
        std::fs::write(&path, "{ not json").unwrap();

        let tracker = Tracker::at(path).unwrap();
        assert!(tracker.list().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let (tracker, dir) = temp_tracker();
        tracker.add(entry(JobStatus::Downloading)).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");

        let _ = std::fs::remove_dir_all(dir);
    }
}

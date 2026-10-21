//! Fortschritt-Tracking & Persistenz (JSON-Store unter `~/.local/share/rdownloader/`).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::download::{JobProgress, JobStatus};

/// Ein Eintrag im Download-Tracker (persistiert).
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
        }
    }
}

/// Thread-sicherer, persistierender Tracker.
pub struct Tracker {
    path: PathBuf,
    entries: Arc<Mutex<Vec<TrackEntry>>>,
}

impl Tracker {
    pub fn new() -> Result<Self> {
        let base = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rdownloader");
        std::fs::create_dir_all(&base).context("Data-Dir anlegen")?;
        let path = base.join("downloads.json");
        let entries = if path.exists() {
            let s = std::fs::read_to_string(&path).context("Tracker laden")?;
            serde_json::from_str(&s).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            entries: Arc::new(Mutex::new(entries)),
        })
    }

    pub fn add(&self, entry: TrackEntry) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        // Upsert: gleiche ID ersetzen
        if let Some(existing) = list.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            list.push(entry);
        }
        self.save(&list)?;
        Ok(())
    }

    pub fn update_from_progress(&self, p: &JobProgress, output: &str, tor_parts: usize) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        if let Some(e) = list.iter_mut().find(|e| e.id == p.id) {
            e.bytes_done = p.bytes_done;
            e.total_bytes = p.total_bytes;
            e.status = p.status.clone();
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

    pub fn remove(&self, id: Uuid) -> Result<()> {
        let mut list = self.entries.lock().unwrap();
        list.retain(|e| e.id != id);
        self.save(&list)?;
        Ok(())
    }

    fn save(&self, list: &[TrackEntry]) -> Result<()> {
        let s = serde_json::to_string_pretty(list)?;
        std::fs::write(&self.path, s).context("Tracker speichern")?;
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

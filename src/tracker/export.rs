//! Export of the download history as CSV or JSON.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::history::HistoryItem;
use super::Tracker;

/// Format of an export file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    /// File extension of this format.
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }
}

/// The JSON export: the entries with a small header.
#[derive(Debug, Serialize)]
struct ExportDocument<'a> {
    exported_at: DateTime<Utc>,
    count: usize,
    entries: Vec<JsonEntry<'a>>,
}

/// One entry of the JSON export.
///
/// Like the history item, but with the stable status name instead of the
/// enum variant, so that JSON and CSV use the same values.
#[derive(Debug, Serialize)]
struct JsonEntry<'a> {
    id: Uuid,
    filename: &'a str,
    url: &'a str,
    output: &'a str,
    status: &'static str,
    total_bytes: u64,
    bytes_done: u64,
    parts: usize,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration_secs: u64,
    avg_bytes_per_sec: u64,
}

impl<'a> From<&'a HistoryItem> for JsonEntry<'a> {
    fn from(it: &'a HistoryItem) -> Self {
        Self {
            id: it.id,
            filename: &it.filename,
            url: &it.url,
            output: &it.output,
            status: it.status_name(),
            total_bytes: it.total_bytes,
            bytes_done: it.bytes_done,
            parts: it.parts,
            started_at: it.started_at,
            finished_at: it.finished_at,
            duration_secs: it.duration_secs,
            avg_bytes_per_sec: it.avg_bytes_per_sec,
        }
    }
}

/// Default location of an export file.
pub fn default_path(format: ExportFormat) -> PathBuf {
    path_in(&Tracker::data_dir(), format)
}

/// File name of an export of the given format.
pub fn file_name(format: ExportFormat) -> String {
    format!("history.{}", format.extension())
}

/// Location of an export file inside `dir`.
pub fn path_in(dir: &Path, format: ExportFormat) -> PathBuf {
    dir.join(file_name(format))
}

/// Renders the history as CSV: a header line and one line per entry.
pub fn to_csv(items: &[HistoryItem]) -> String {
    let mut out = String::new();
    out.push_str(
        "id,filename,url,output,status,total_bytes,bytes_done,parts,started_at,finished_at,duration_secs,avg_bytes_per_sec\n",
    );
    for it in items {
        let fields = [
            it.id.to_string(),
            it.filename.clone(),
            it.url.clone(),
            it.output.clone(),
            it.status_name().to_string(),
            it.total_bytes.to_string(),
            it.bytes_done.to_string(),
            it.parts.to_string(),
            it.started_at.to_rfc3339(),
            it.finished_at.to_rfc3339(),
            it.duration_secs.to_string(),
            it.avg_bytes_per_sec.to_string(),
        ];
        let row: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

/// Renders the history as JSON.
pub fn to_json(items: &[HistoryItem]) -> Result<String> {
    let doc = ExportDocument {
        exported_at: Utc::now(),
        count: items.len(),
        entries: items.iter().map(JsonEntry::from).collect(),
    };
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Writes the history to `path`.
pub fn write(path: &Path, items: &[HistoryItem], format: ExportFormat) -> Result<()> {
    let content = match format {
        ExportFormat::Csv => to_csv(items),
        ExportFormat::Json => to_json(items)?,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).context("Create export directory")?;
    }
    std::fs::write(path, content).with_context(|| format!("Write export to {path:?}"))?;
    Ok(())
}

/// Escapes one CSV field: values with a comma, a quote or a line break are
/// quoted, inner quotes are doubled.
fn csv_field(s: &str) -> String {
    if s.contains(&[',', '"', '\n', '\r'][..]) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::JobStatus;
    use chrono::Duration;
    use uuid::Uuid;

    fn item(filename: &str) -> HistoryItem {
        let now = Utc::now();
        HistoryItem {
            id: Uuid::new_v4(),
            filename: filename.into(),
            url: "http://example.com/a.bin".into(),
            output: "/tmp/a.bin".into(),
            status: JobStatus::Done,
            total_bytes: 1000,
            bytes_done: 1000,
            parts: 2,
            started_at: now,
            finished_at: now + Duration::seconds(5),
            duration_secs: 5,
            avg_bytes_per_sec: 200,
        }
    }

    #[test]
    fn csv_has_a_header_and_one_line_per_entry() {
        let csv = to_csv(&[item("a.bin"), item("b.bin")]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("id,filename,url,output,status,"));
        assert!(lines[1].contains("a.bin"));
        assert!(lines[1].contains("done"));
        assert!(lines[2].contains("b.bin"));
    }

    #[test]
    fn csv_escapes_separators_and_quotes() {
        let mut it = item("we,ird\".bin");
        it.url = "http://example.com/a?x=1,2".into();
        let csv = to_csv(&[it]);
        let line = csv.lines().nth(1).unwrap();
        assert!(line.contains("\"we,ird\"\".bin\""), "{line}");
        assert!(line.contains("\"http://example.com/a?x=1,2\""), "{line}");
    }

    #[test]
    fn json_contains_the_entries_and_the_count() {
        let json = to_json(&[item("a.bin")]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["entries"][0]["filename"], "a.bin");
        assert_eq!(value["entries"][0]["status"], "done");
        assert!(value["exported_at"].is_string());
    }

    #[test]
    fn writing_creates_the_file() {
        let dir = std::env::temp_dir().join(format!("rdl-export-test-{}", Uuid::new_v4()));
        let path = dir.join("sub").join("history.csv");
        write(&path, &[item("a.bin")], ExportFormat::Csv).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("a.bin"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extension_follows_the_format() {
        assert_eq!(ExportFormat::Csv.extension(), "csv");
        assert_eq!(ExportFormat::Json.extension(), "json");
        assert!(default_path(ExportFormat::Json).ends_with("history.json"));
    }
}

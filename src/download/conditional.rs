//! Conditional download: skip files whose local copy is already current.
//!
//! The server's `Last-Modified` (already known from the size request) is compared
//! with the local file instead of sending `If-Modified-Since`, so Tor round trips
//! stay the same.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Parses an HTTP date (`Wed, 21 Oct 2026 07:28:00 GMT`).
pub fn parse_http_date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(value.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// True when `local` has the remote size and is not older than the remote copy.
/// Without a usable `Last-Modified` nothing can be proven, so the answer is `false`.
pub fn is_up_to_date(local: &Path, remote_size: u64, remote_last_modified: Option<&str>) -> bool {
    let Some(remote_time) = remote_last_modified.and_then(parse_http_date) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(local) else {
        return false;
    };
    if !meta.is_file() || meta.len() != remote_size {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    DateTime::<Utc>::from(modified) >= remote_time
}

/// After a download: give the file the server's modification time, so the next
/// conditional check compares like with like. Best effort.
pub fn apply_remote_time(path: &Path, remote_last_modified: Option<&str>) {
    let Some(time) = remote_last_modified.and_then(parse_http_date) else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) else {
        return;
    };
    let _ = file.set_modified(SystemTime::from(time));
}

/// The existing local file for `dir`/`proposed_name`, if conditional downloads are on.
pub fn existing_copy(dir: &Path, proposed_name: &str) -> Option<PathBuf> {
    if !crate::config::config().conditional_get {
        return None;
    }
    let path = dir.join(super::sanitize_filename(proposed_name));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_file(content: &[u8]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rdl-cond-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f.bin");
        std::fs::write(&f, content).unwrap();
        (dir, f)
    }

    #[test]
    fn parses_http_dates() {
        let d = parse_http_date("Wed, 21 Oct 2026 07:28:00 GMT").unwrap();
        assert_eq!(d.to_rfc3339(), "2026-10-21T07:28:00+00:00");
        assert!(parse_http_date("yesterday").is_none());
    }

    #[test]
    fn up_to_date_needs_same_size_and_not_older() {
        let (dir, f) = temp_file(b"12345");
        let date = "Wed, 21 Oct 2026 07:28:00 GMT";
        // The file was just written: newer than the (past) remote date.
        apply_remote_time(&f, Some("Wed, 21 Oct 2026 07:28:00 GMT"));
        assert!(
            is_up_to_date(&f, 5, Some(date)),
            "same time counts as current"
        );
        assert!(!is_up_to_date(&f, 6, Some(date)), "different size");
        assert!(
            !is_up_to_date(&f, 5, Some("Thu, 22 Oct 2026 07:28:00 GMT")),
            "remote is newer"
        );
        assert!(
            !is_up_to_date(&f, 5, None),
            "no Last-Modified, nothing proven"
        );
        assert!(!is_up_to_date(&dir.join("missing"), 5, Some(date)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remote_time_is_applied_to_the_file() {
        let (dir, f) = temp_file(b"x");
        apply_remote_time(&f, Some("Wed, 21 Oct 2026 07:28:00 GMT"));
        let modified = DateTime::<Utc>::from(std::fs::metadata(&f).unwrap().modified().unwrap());
        assert_eq!(modified.timestamp(), 1_792_567_680);
        // Garbage must not touch the file.
        apply_remote_time(&f, Some("garbage"));
        let again = DateTime::<Utc>::from(std::fs::metadata(&f).unwrap().modified().unwrap());
        assert_eq!(again, modified);
        let _ = Duration::ZERO;
        let _ = std::fs::remove_dir_all(dir);
    }
}

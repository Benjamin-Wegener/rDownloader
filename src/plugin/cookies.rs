//! Cookie import from exported files and browser profiles.
//!
//! Supported sources:
//! - **Netscape `cookies.txt`** (the format exported by Firefox/Chromium
//!   extensions and by `curl`/`wget`)
//! - **Firefox** `cookies.sqlite` (`moz_cookies`, values are plain text)
//! - **Chromium** `Cookies` SQLite databases (plain-text values are imported;
//!   OS-keyring encrypted rows are skipped and counted)
//!
//! Cookie values are wrapped in [`Secret`] and never serialized or logged.
//! Paths and the `Secure` flag are honoured when building request headers.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

use super::account::Secret;

/// Format of an imported cookie store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieFormat {
    Netscape,
    Firefox,
    Chromium,
}

/// One imported cookie, before it is scoped to an [`AccountSession`](super::AccountSession).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedCookie {
    pub name: String,
    pub value: Secret,
    /// Registrable host the cookie belongs to; a leading dot is stripped.
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Result of importing a cookie store.
#[derive(Debug, Clone)]
pub struct CookieImportReport {
    pub format: CookieFormat,
    pub cookies: Vec<ImportedCookie>,
    /// Lines/rows that were malformed or had no name/value.
    pub skipped_invalid: usize,
    /// Cookies that were already expired at import time.
    pub skipped_expired: usize,
    /// Chromium rows whose value is encrypted with the OS keyring.
    pub skipped_encrypted: usize,
}

impl CookieImportReport {
    fn new(format: CookieFormat) -> Self {
        Self {
            format,
            cookies: Vec::new(),
            skipped_invalid: 0,
            skipped_expired: 0,
            skipped_encrypted: 0,
        }
    }
}

/// Imports a cookie store from a file or a browser profile directory.
///
/// A directory is searched for the usual profile files (`cookies.sqlite`,
/// `Cookies`, `Network/Cookies`, `cookies.txt`). The format is detected from
/// the file content, not its name.
pub fn import_cookie_file(path: &Path) -> Result<CookieImportReport> {
    let path = resolve_cookie_path(path)?;
    if is_sqlite_database(&path)? {
        import_sqlite_cookies(&path)
    } else {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading cookie file {}", path.display()))?;
        Ok(parse_netscape_cookies(&text))
    }
}

/// Parses the Netscape `cookies.txt` format.
///
/// A line has seven tab-separated fields:
/// `domain  include_subdomains  path  secure  expires  name  value`.
/// Lines starting with `#` are comments, except `#HttpOnly_` which marks the
/// cookie as HTTP-only.
pub fn parse_netscape_cookies(text: &str) -> CookieImportReport {
    let mut report = CookieImportReport::new(CookieFormat::Netscape);
    let now = Utc::now();

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let (http_only, line) = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => (true, rest),
            None => {
                if line.trim_start().starts_with('#') {
                    continue;
                }
                (false, line)
            }
        };

        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 7 {
            report.skipped_invalid += 1;
            continue;
        }
        let domain = normalize_domain(fields[0]);
        let cookie_path = normalize_path(fields[2]);
        let secure = parse_bool(fields[3]);
        let expires = fields[4].trim().parse::<i64>().unwrap_or(0);
        let name = fields[5].trim();
        let value = fields[6].trim();

        if domain.is_empty() || name.is_empty() || value.is_empty() {
            report.skipped_invalid += 1;
            continue;
        }
        let expires_at = expiry_from_unix(expires);
        if is_expired(expires_at, now) {
            report.skipped_expired += 1;
            continue;
        }
        report.cookies.push(ImportedCookie {
            name: name.to_string(),
            value: Secret::new(value),
            domain,
            path: cookie_path,
            secure,
            http_only,
            expires_at,
        });
    }
    report
}

fn import_sqlite_cookies(path: &Path) -> Result<CookieImportReport> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening cookie database {}", path.display()))?;
    let has_firefox = table_exists(&conn, "moz_cookies")?;
    let has_chromium = table_exists(&conn, "cookies")?;
    if has_firefox {
        read_firefox_cookies_from(&conn)
    } else if has_chromium {
        read_chromium_cookies_from(&conn)
    } else {
        bail!(
            "{} is a SQLite database but has no cookies table (moz_cookies/cookies)",
            path.display()
        )
    }
}

/// Reads a Firefox `cookies.sqlite` database.
pub fn read_firefox_cookies(path: &Path) -> Result<CookieImportReport> {
    let conn = open_read_only(path)?;
    read_firefox_cookies_from(&conn)
}

/// Reads a Chromium `Cookies` database.
pub fn read_chromium_cookies(path: &Path) -> Result<CookieImportReport> {
    let conn = open_read_only(path)?;
    read_chromium_cookies_from(&conn)
}

fn open_read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening cookie database {}", path.display()))
}

fn read_firefox_cookies_from(conn: &Connection) -> Result<CookieImportReport> {
    let mut report = CookieImportReport::new(CookieFormat::Firefox);
    let mut stmt = conn
        .prepare("SELECT name, value, host, path, expiry, isSecure, isHttpOnly FROM moz_cookies")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let now = Utc::now();
    for row in rows {
        let (name, value, host, cookie_path, expiry, secure, http_only) = row?;
        let domain = normalize_domain(&host);
        if domain.is_empty() || name.trim().is_empty() || value.is_empty() {
            report.skipped_invalid += 1;
            continue;
        }
        let expires_at = expiry_from_unix(expiry);
        if is_expired(expires_at, now) {
            report.skipped_expired += 1;
            continue;
        }
        report.cookies.push(ImportedCookie {
            name: name.trim().to_string(),
            value: Secret::new(value),
            domain,
            path: normalize_path(&cookie_path),
            secure: secure != 0,
            http_only: http_only != 0,
            expires_at,
        });
    }
    Ok(report)
}

fn read_chromium_cookies_from(conn: &Connection) -> Result<CookieImportReport> {
    let mut report = CookieImportReport::new(CookieFormat::Chromium);
    let mut stmt = conn.prepare(
        "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly FROM cookies",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let now = Utc::now();
    for row in rows {
        let (host, name, value, encrypted, cookie_path, expires_utc, secure, http_only) = row?;
        let domain = normalize_domain(&host);
        if domain.is_empty() || name.trim().is_empty() {
            report.skipped_invalid += 1;
            continue;
        }
        if value.is_empty() {
            if !encrypted.is_empty() {
                report.skipped_encrypted += 1;
                continue;
            }
            report.skipped_invalid += 1;
            continue;
        }
        let expires_at = chromium_expiry(expires_utc);
        if is_expired(expires_at, now) {
            report.skipped_expired += 1;
            continue;
        }
        report.cookies.push(ImportedCookie {
            name: name.trim().to_string(),
            value: Secret::new(value),
            domain,
            path: normalize_path(&cookie_path),
            secure: secure != 0,
            http_only: http_only != 0,
            expires_at,
        });
    }
    Ok(report)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn is_sqlite_database(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening cookie file {}", path.display()))?;
    let mut header = [0u8; 16];
    let read = file.read(&mut header)?;
    Ok(&header[..read] == b"SQLite format 3\0")
}

fn resolve_cookie_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if !path.is_dir() {
        bail!("cookie source does not exist: {}", path.display());
    }
    let candidates = [
        "cookies.sqlite",
        "Cookies",
        "Network/Cookies",
        "cookies.txt",
    ];
    for candidate in candidates {
        let full = path.join(candidate);
        if full.is_file() {
            return Ok(full);
        }
    }
    bail!(
        "no cookie store (cookies.sqlite, Cookies, cookies.txt) found in {}",
        path.display()
    )
}

fn normalize_domain(domain: &str) -> String {
    domain.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn normalize_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(value.trim().to_ascii_uppercase().as_str(), "TRUE" | "1")
}

fn expiry_from_unix(seconds: i64) -> Option<DateTime<Utc>> {
    if seconds <= 0 {
        None
    } else {
        DateTime::from_timestamp(seconds, 0)
    }
}

/// Chromium stores microseconds since 1601-01-01 (Windows file time).
fn chromium_expiry(micros: i64) -> Option<DateTime<Utc>> {
    if micros <= 0 {
        return None;
    }
    const WINDOWS_TO_UNIX_SECONDS: i64 = 11_644_473_600;
    let unix_seconds = micros / 1_000_000 - WINDOWS_TO_UNIX_SECONDS;
    if unix_seconds <= 0 {
        None
    } else {
        DateTime::from_timestamp(unix_seconds, 0)
    }
}

fn is_expired(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    expires_at.is_some_and(|expiry| expiry <= now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix_seconds(years_from_now: i64) -> i64 {
        Utc::now().timestamp() + years_from_now * 365 * 24 * 3600
    }

    #[test]
    fn parses_netscape_format() {
        let future = unix_seconds(5);
        let text = format!(
            "# Netscape HTTP Cookie File\n\
             .example.com\tTRUE\t/\tTRUE\t{future}\tsid\tabc123\n\
             #HttpOnly_.example.com\tTRUE\t/download\tFALSE\t0\ttoken\txyz\n\
             other.test\tFALSE\t/\tFALSE\t0\ttheme\tdark\n\
             malformed line\n"
        );
        let report = parse_netscape_cookies(&text);
        assert_eq!(report.format, CookieFormat::Netscape);
        assert_eq!(report.cookies.len(), 3);
        assert_eq!(report.skipped_invalid, 1);

        let sid = &report.cookies[0];
        assert_eq!(sid.name, "sid");
        assert_eq!(sid.domain, "example.com");
        assert_eq!(sid.value.expose(), "abc123");
        assert!(sid.secure);
        assert!(!sid.http_only);
        assert!(sid.expires_at.is_some());

        let token = &report.cookies[1];
        assert!(token.http_only);
        assert_eq!(token.path, "/download");
        assert!(token.expires_at.is_none());
    }

    #[test]
    fn skips_expired_and_empty_netscape_cookies() {
        let expired = unix_seconds(-1);
        let text = format!(
            ".a.test\tTRUE\t/\tFALSE\t{expired}\tsid\told\n\
             .a.test\tTRUE\t/\tFALSE\t0\tempty\t\n"
        );
        let report = parse_netscape_cookies(&text);
        assert!(report.cookies.is_empty());
        assert_eq!(report.skipped_expired, 1);
        assert_eq!(report.skipped_invalid, 1);
    }

    fn write_firefox_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_cookies (
                name TEXT, value TEXT, host TEXT, path TEXT,
                expiry INTEGER, isSecure INTEGER, isHttpOnly INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES ('sid', 'abc', '.hoster.test', '/', ?1, 1, 1)",
            [unix_seconds(3600)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES ('gone', 'x', '.hoster.test', '/', 1, 0, 0)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn reads_firefox_cookies() {
        let dir = std::env::temp_dir().join(format!("rdl-cookies-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cookies.sqlite");
        write_firefox_db(&db);

        let report = read_firefox_cookies(&db).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(report.format, CookieFormat::Firefox);
        assert_eq!(report.cookies.len(), 1);
        assert_eq!(report.cookies[0].domain, "hoster.test");
        assert_eq!(report.cookies[0].value.expose(), "abc");
        assert!(report.cookies[0].secure);
        assert!(report.cookies[0].http_only);
        assert_eq!(report.skipped_expired, 1);
    }

    fn write_chromium_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE cookies (
                host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB,
                path TEXT, expires_utc INTEGER, is_secure INTEGER, is_httponly INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies VALUES ('.hoster.test', 'sid', 'abc', X'', '/', ?1, 1, 1)",
            [chromium_micros(unix_seconds(3600))],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies VALUES ('.hoster.test', 'secret', '', X'763130', '/', 0, 0, 0)",
            [],
        )
        .unwrap();
    }

    fn chromium_micros(unix_seconds: i64) -> i64 {
        (unix_seconds + 11_644_473_600) * 1_000_000
    }

    #[test]
    fn reads_chromium_plaintext_and_counts_encrypted() {
        let dir = std::env::temp_dir().join(format!("rdl-cookies-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");
        write_chromium_db(&db);

        let report = import_cookie_file(&db).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(report.format, CookieFormat::Chromium);
        assert_eq!(report.cookies.len(), 1);
        assert_eq!(report.cookies[0].name, "sid");
        assert_eq!(report.skipped_encrypted, 1);
    }

    #[test]
    fn imports_from_profile_directory() {
        let dir = std::env::temp_dir().join(format!("rdl-cookies-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write_firefox_db(&dir.join("cookies.sqlite"));

        let report = import_cookie_file(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(report.format, CookieFormat::Firefox);
        assert_eq!(report.cookies.len(), 1);
    }

    #[test]
    fn unknown_database_is_rejected() {
        let dir = std::env::temp_dir().join(format!("rdl-cookies-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("other.sqlite");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (x INTEGER);")
            .unwrap();

        let result = import_cookie_file(&db);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
    }
}

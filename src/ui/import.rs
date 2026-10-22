//! Batch import (http, https and ftp links): turns pasted text, a link-list file or the clipboard into download URLs.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use url::Url;

/// Import files larger than this are rejected (a link list is never this big).
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;
/// Upper bound on links taken from one import.
const MAX_LINKS: usize = 10_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Import {
    pub urls: Vec<Url>,
    /// Tokens that looked like links but were not usable (bad scheme, parse error).
    pub skipped: usize,
}

/// Extracts http(s) URLs from free text: one per line, several per line, plain
/// lists, M3U/M3U8 playlists (`#EXTINF` lines are comments) and HTML-ish text.
/// Order is preserved, duplicates are dropped.
pub fn parse_urls(text: &str) -> Import {
    let mut out = Import::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for token in line.split(|c: char| c.is_whitespace() || c == ',') {
            let token =
                token.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | ';'));
            if !["http://", "https://", "ftp://"]
                .iter()
                .any(|p| token.starts_with(p))
            {
                continue;
            }
            match Url::parse(token) {
                Ok(url) if url.host_str().is_some() => {
                    if !out.urls.contains(&url) && out.urls.len() < MAX_LINKS {
                        out.urls.push(url);
                    }
                }
                _ => out.skipped += 1,
            }
        }
    }
    out
}

pub fn import_file(path: &Path) -> Result<Import> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", path.display());
    }
    if meta.len() > MAX_IMPORT_BYTES {
        bail!(
            "{} is too large for an import ({} bytes)",
            path.display(),
            meta.len()
        );
    }
    let container = crate::plugin::container::import_container_file(path)?;
    // Mirrors and piece checksums travel to the job through the hint table.
    for item in &container.items {
        crate::download::mirror::hints::register(
            &item.url,
            item.mirrors.clone(),
            item.pieces.clone(),
        );
    }
    Ok(Import {
        urls: container.urls(),
        skipped: container.skipped,
    })
}

/// Reads the system clipboard through the platform's command line tool
/// (no extra dependency): `pbpaste`, `wl-paste`, `xclip` or `xsel`.
pub fn read_clipboard() -> Result<String> {
    const TOOLS: &[(&str, &[&str])] = &[
        ("pbpaste", &[]),
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (tool, args) in TOOLS {
        if let Ok(out) = Command::new(tool).args(*args).output() {
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    bail!("no clipboard tool found (pbpaste, wl-paste, xclip or xsel)")
}

/// Expands a leading `~` so users can type `~/links.txt`.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => path.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(i: &Import) -> Vec<&str> {
        i.urls.iter().map(Url::as_str).collect()
    }

    #[test]
    fn plain_list_with_comments_and_blanks() {
        let i = parse_urls("# my links\n\nhttp://a.test/1\n  https://b.test/2  \n");
        assert_eq!(urls(&i), ["http://a.test/1", "https://b.test/2"]);
        assert_eq!(i.skipped, 0);
    }

    #[test]
    fn m3u_playlist() {
        let i = parse_urls("#EXTM3U\n#EXTINF:123,Title\nhttp://a.test/s.mp3\n#EXTINF:5,Other\nhttp://a.test/t.mp3\n");
        assert_eq!(urls(&i), ["http://a.test/s.mp3", "http://a.test/t.mp3"]);
    }

    #[test]
    fn several_links_per_line_and_wrapping_characters() {
        let i = parse_urls("see <http://a.test/1>, \"http://a.test/2\" (https://a.test/3);");
        assert_eq!(
            urls(&i),
            ["http://a.test/1", "http://a.test/2", "https://a.test/3"]
        );
    }

    #[test]
    fn duplicates_dropped_other_schemes_ignored_bad_urls_counted() {
        let i = parse_urls("http://a.test/x\nhttp://a.test/x\nsftp://a.test/f\nmailto:a@b.c\nhttp://\nhttps://[bad\n");
        assert_eq!(urls(&i), ["http://a.test/x"]);
        assert_eq!(i.skipped, 2);
    }

    #[test]
    fn link_count_is_capped() {
        let text: String = (0..MAX_LINKS + 50)
            .map(|n| format!("http://a.test/{n}\n"))
            .collect();
        assert_eq!(parse_urls(&text).urls.len(), MAX_LINKS);
    }

    #[test]
    fn import_file_roundtrip_and_errors() {
        let dir = std::env::temp_dir().join(format!("rdl-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("links.txt");
        std::fs::write(&file, "http://a.test/1\nhttp://a.test/2\n").unwrap();
        assert_eq!(import_file(&file).unwrap().urls.len(), 2);
        assert!(import_file(&dir).is_err(), "directories are rejected");
        assert!(import_file(&dir.join("missing.txt")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn home_expansion() {
        assert_eq!(expand_home("/abs/x"), std::path::PathBuf::from("/abs/x"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_home("~/x"), home.join("x"));
        }
    }
}

//! Central filename sanitization and collision resolver.
//!
//! Provides defense against:
//! - Directory traversal attacks (`../`, `..\`, absolute paths, root components)
//! - Reserved device names on Windows (`CON`, `PRN`, `AUX`, `NUL`, `COM1`..`COM9`, `LPT1`..`LPT9`)
//! - Illegal filesystem characters (`<`, `>`, `:`, `"`, `/`, `\`, `|`, `?`, `*`, control characters)
//! - Hidden / dangerous prefix trimming (e.g. leading dots, trailing dots, trailing spaces)
//! - Filename collision resolution (`file.zip` -> `file (1).zip`, `file (2).zip`, ...)

use std::path::{Path, PathBuf};

/// Reserved Windows device names (case-insensitive).
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters that are forbidden in filenames across common filesystems (FAT, NTFS, ext4, APFS).
const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Default filename when input is empty or completely invalid.
pub const DEFAULT_FALLBACK_FILENAME: &str = "download.bin";

/// Sanitizes an untrusted filename proposed by any source (Plugin, Container, Content-Disposition, URL).
///
/// Guaranteed properties:
/// 1. Contains no path separators or traversal elements (`/`, `\`, `..`).
/// 2. Contains no forbidden filesystem characters or control characters (0x00..0x1F, 0x7F).
/// 3. Is not a Windows reserved device name (e.g., `CON`, `NUL.txt`).
/// 4. Is not empty and does not consist solely of dots or spaces.
/// 5. Length is capped to a safe boundary (255 bytes).
pub fn sanitize_filename(candidate: &str) -> String {
    // 1. Extract purely the final path segment (strip directory traversal)
    let candidate = candidate.trim();
    let base = candidate.rsplit(['/', '\\']).next().unwrap_or(candidate);

    // 2. Filter out forbidden characters and control characters
    let mut cleaned: String = base
        .chars()
        .filter(|ch| !ch.is_control() && !FORBIDDEN_CHARS.contains(ch))
        .collect();

    // 3. Trim trailing dots and spaces (Windows / DOS issue)
    let trimmed = cleaned.trim_end_matches(['.', ' ']).trim_start_matches(' ');
    cleaned = trimmed.to_string();

    // 4. Handle empty, single dot, or double dot
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return DEFAULT_FALLBACK_FILENAME.to_string();
    }

    // 5. Check against Windows reserved names (both with and without extension)
    let stem = cleaned.split('.').next().unwrap_or(&cleaned);
    if WINDOWS_RESERVED
        .iter()
        .any(|&reserved| stem.eq_ignore_ascii_case(reserved))
    {
        cleaned = format!("_{cleaned}");
    }

    // 6. Truncate to maximum 255 bytes while maintaining UTF-8 boundary
    if cleaned.len() > 255 {
        cleaned = truncate_to_byte_limit(&cleaned, 255);
    }

    if cleaned.is_empty() {
        DEFAULT_FALLBACK_FILENAME.to_string()
    } else {
        cleaned
    }
}

/// Helper to safely truncate a UTF-8 string to a maximum byte length without splitting code points.
fn truncate_to_byte_limit(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    // Find the latest valid char boundary <= max_bytes
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }

    // If an extension exists, try to preserve it
    if let Some(dot_idx) = s.rfind('.') {
        let ext = &s[dot_idx..];
        if ext.len() < 32 && ext.len() < boundary {
            let stem_max = boundary - ext.len();
            let mut stem_boundary = stem_max;
            while stem_boundary > 0 && !s.is_char_boundary(stem_boundary) {
                stem_boundary -= 1;
            }
            return format!("{}{ext}", &s[..stem_boundary]);
        }
    }

    s[..boundary].to_string()
}

/// Resolves a target path within `dest_dir` using `proposed_name`.
///
/// Automatically sanitizes the filename and, if a file already exists at that path,
/// appends an incrementing counter: `name.ext` -> `name (1).ext`, `name (2).ext`, etc.
pub fn resolve_destination(dest_dir: &Path, proposed_name: &str) -> PathBuf {
    let safe_name = sanitize_filename(proposed_name);
    let target = dest_dir.join(&safe_name);

    if !target.exists() {
        return target;
    }

    // Split into stem and extension
    let (stem, ext) = match safe_name.rfind('.') {
        Some(idx) if idx > 0 => (&safe_name[..idx], &safe_name[idx..]),
        _ => (safe_name.as_str(), ""),
    };

    let mut counter = 1;
    loop {
        let numbered_name = format!("{stem} ({counter}){ext}");
        let candidate_path = dest_dir.join(numbered_name);
        if !candidate_path.exists() {
            return candidate_path;
        }
        counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_traversal_is_stripped() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename(r"..\..\Windows\System32\cmd.exe"),
            "cmd.exe"
        );
        assert_eq!(sanitize_filename("/root/.bashrc"), ".bashrc");
        assert_eq!(
            sanitize_filename(r"C:\Users\Admin\secrets.txt"),
            "secrets.txt"
        );
    }

    #[test]
    fn forbidden_characters_are_removed() {
        assert_eq!(sanitize_filename("hello:world?*.txt"), "helloworld.txt");
        assert_eq!(sanitize_filename("foo<bar>baz|qux.zip"), "foobarbazqux.zip");
        assert_eq!(
            sanitize_filename("control\u{0000}\u{001F}char.bin"),
            "controlchar.bin"
        );
    }

    #[test]
    fn empty_and_dot_names_fallback() {
        assert_eq!(sanitize_filename(""), DEFAULT_FALLBACK_FILENAME);
        assert_eq!(sanitize_filename("."), DEFAULT_FALLBACK_FILENAME);
        assert_eq!(sanitize_filename(".."), DEFAULT_FALLBACK_FILENAME);
        assert_eq!(sanitize_filename("   "), DEFAULT_FALLBACK_FILENAME);
        assert_eq!(sanitize_filename("???:::"), DEFAULT_FALLBACK_FILENAME);
    }

    #[test]
    fn windows_reserved_names_are_prefixed() {
        assert_eq!(sanitize_filename("con"), "_con");
        assert_eq!(sanitize_filename("CON.TXT"), "_CON.TXT");
        assert_eq!(sanitize_filename("aux.tar.gz"), "_aux.tar.gz");
        assert_eq!(sanitize_filename("NUL"), "_NUL");
        assert_eq!(sanitize_filename("lpt1"), "_lpt1");
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(sanitize_filename("file.txt..."), "file.txt");
        assert_eq!(sanitize_filename("file.txt   "), "file.txt");
    }

    #[test]
    fn destination_collision_resolution() {
        let dir = std::env::temp_dir().join(format!("rdl-sanitize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let first = resolve_destination(&dir, "archive.zip");
        assert_eq!(first, dir.join("archive.zip"));
        std::fs::write(&first, b"first").unwrap();

        let second = resolve_destination(&dir, "archive.zip");
        assert_eq!(second, dir.join("archive (1).zip"));
        std::fs::write(&second, b"second").unwrap();

        let third = resolve_destination(&dir, "archive.zip");
        assert_eq!(third, dir.join("archive (2).zip"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn byte_limit_truncation_preserves_extension() {
        let long_stem = "a".repeat(300);
        let filename = format!("{long_stem}.iso");
        let sanitized = sanitize_filename(&filename);
        assert!(sanitized.len() <= 255);
        assert!(sanitized.ends_with(".iso"));
    }
}

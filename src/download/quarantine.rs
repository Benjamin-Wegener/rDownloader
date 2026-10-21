//! macOS Gatekeeper quarantine attribute (`com.apple.quarantine`).
//!
//! Applies the quarantine flag to newly downloaded files on macOS so that
//! Gatekeeper verifies them before first execution.
//!
//! Format: `0083;HEX_TIMESTAMP;AGENT_NAME;UUID`
//! e.g. `0083;66a1b2c3;rdownloader;4B29E3DC-55B2-4F1E-9C3E-9419138B21AC`

use anyhow::{bail, Context, Result};
use std::path::Path;

pub const QUARANTINE_ATTR_NAME: &str = "com.apple.quarantine";
pub const DEFAULT_AGENT: &str = "rdownloader";

/// Formats a quarantine string.
/// Flags:
/// `0083`: Standard file downloaded from the Internet (quarantine bitmask).
pub fn format_quarantine_value(agent: &str, timestamp: u64, uuid: &uuid::Uuid) -> String {
    format!(
        "0083;{:08x};{};{}",
        timestamp,
        agent,
        uuid.to_string().to_uppercase()
    )
}

/// Applies the `com.apple.quarantine` extended attribute to the target file if running on macOS.
/// On other platforms, this is a clean no-op.
pub fn apply_quarantine(path: &Path) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = uuid::Uuid::new_v4();
        let value = format_quarantine_value(DEFAULT_AGENT, now, &id);

        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path contains null byte: {}", path.display()))?;
        let c_name = CString::new(QUARANTINE_ATTR_NAME)?;

        let ret = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
                0,
            )
        };

        if ret != 0 {
            let err = std::io::Error::last_os_error();
            bail!(
                "failed to set {QUARANTINE_ATTR_NAME} on {}: {err}",
                path.display()
            );
        }

        Ok(true)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Reads the `com.apple.quarantine` attribute if present on the target file.
pub fn get_quarantine(path: &Path) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path contains null byte: {}", path.display()))?;
        let c_name = CString::new(QUARANTINE_ATTR_NAME)?;

        let size = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };

        if size < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOATTR) {
                return Ok(None);
            }
            bail!(
                "failed to get {QUARANTINE_ATTR_NAME} on {}: {err}",
                path.display()
            );
        }

        let mut buffer = vec![0u8; size as usize];
        let ret = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                0,
                0,
            )
        };

        if ret < 0 {
            let err = std::io::Error::last_os_error();
            bail!(
                "failed to read {QUARANTINE_ATTR_NAME} on {}: {err}",
                path.display()
            );
        }

        buffer.truncate(ret as usize);
        let s = String::from_utf8(buffer).context("quarantine attribute is not valid UTF-8")?;
        Ok(Some(s))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_quarantine_string_structure() {
        let u = uuid::Uuid::parse_str("4b29e3dc-55b2-4f1e-9c3e-9419138b21ac").unwrap();
        let q = format_quarantine_value("rdownloader", 0x66a1b2c3, &u);
        assert_eq!(
            q,
            "0083;66a1b2c3;rdownloader;4B29E3DC-55B2-4F1E-9C3E-9419138B21AC"
        );
    }

    #[test]
    fn apply_and_get_quarantine_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rdl-quar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test_download.bin");
        std::fs::write(&file_path, b"test content").unwrap();

        let applied = apply_quarantine(&file_path).expect("apply quarantine succeeded");
        #[cfg(target_os = "macos")]
        {
            assert!(applied);
            let val = get_quarantine(&file_path).expect("get quarantine succeeded");
            assert!(val.is_some());
            let s = val.unwrap();
            assert!(s.starts_with("0083;"));
            assert!(s.contains("rdownloader"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!applied);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

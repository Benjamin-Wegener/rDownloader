//! Filesystem disk space checking and file pre-allocation.
//!
//! Provides utilities to:
//! - Check available free disk space on the target filesystem before starting a download
//! - Pre-allocate disk space for the target download file to prevent fragmentation
//!   and ensure sufficient capacity early

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::path::Path;

/// Information about a filesystem's disk space capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub available_bytes: u64,
    pub total_bytes: u64,
}

#[cfg(unix)]
pub fn query_available_space(path: &Path) -> Result<DiskSpace> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Use the directory of the path if the target file does not yet exist
    let check_path = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    p
                }
            })
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };

    let c_path = CString::new(check_path.as_os_str().as_bytes())
        .with_context(|| format!("path contains null byte: {}", check_path.display()))?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if res != 0 {
        let err = std::io::Error::last_os_error();
        bail!("statvfs failed on {}: {err}", check_path.display());
    }

    let frsize = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };

    let available = (stat.f_bavail as u64).saturating_mul(frsize);
    let total = (stat.f_blocks as u64).saturating_mul(frsize);

    Ok(DiskSpace {
        available_bytes: available,
        total_bytes: total,
    })
}

#[cfg(windows)]
pub fn query_available_space(_path: &Path) -> Result<DiskSpace> {
    // Basic fallback for Windows builds (can use GetDiskFreeSpaceExW if libc/winapi available)
    Ok(DiskSpace {
        available_bytes: u64::MAX,
        total_bytes: u64::MAX,
    })
}

#[cfg(not(any(unix, windows)))]
pub fn query_available_space(_path: &Path) -> Result<DiskSpace> {
    Ok(DiskSpace {
        available_bytes: u64::MAX,
        total_bytes: u64::MAX,
    })
}

/// Ensures that `required_bytes` are available on the filesystem holding `target_path`.
///
/// A safety buffer (e.g. 50 MB) is added to prevent completely exhausting the drive.
pub fn ensure_sufficient_space(target_path: &Path, required_bytes: u64) -> Result<()> {
    if required_bytes == 0 {
        return Ok(());
    }

    let space = query_available_space(target_path)?;
    const SAFETY_MARGIN: u64 = 10 * 1024 * 1024; // 10 MiB safety headroom

    let needed = required_bytes.saturating_add(SAFETY_MARGIN);
    if space.available_bytes < needed {
        bail!(
            "Insufficient disk space on {}: need {} bytes (including safety margin), but only {} bytes available",
            target_path.display(),
            needed,
            space.available_bytes
        );
    }

    Ok(())
}

/// Pre-allocates disk space for a file without writing all zeroes manually where supported.
///
/// On macOS / iOS: Uses `fcntl(F_PREALLOCATE)`
/// On Linux: Uses `fallocate(FALLOC_FL_KEEP_SIZE)` or `posix_fallocate`
/// Fallback: Sets file length via `set_len`
pub fn preallocate_file(file: &File, size: u64) -> Result<()> {
    if size == 0 {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let mut alloc_struct = libc::fstore_t {
            fst_flags: libc::F_ALLOCATECONTIG | libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: size as libc::off_t,
            fst_bytesalloc: 0,
        };

        let ret = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut alloc_struct) };
        if ret != 0 {
            // Contiguous allocation may fail due to fragmentation, retry without contiguous flag
            alloc_struct.fst_flags = libc::F_ALLOCATEALL;
            let ret2 = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut alloc_struct) };
            if ret2 != 0 {
                // Fallback to ftruncate / set_len
                file.set_len(size)
                    .with_context(|| format!("set_len fallback failed for size {size}"))?;
                return Ok(());
            }
        }

        // Even with F_PREALLOCATE, ftruncate is typically called to adjust EOF mark
        let _ = file.set_len(size);
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::posix_fallocate(fd, 0, size as libc::off_t) };
        if ret == 0 {
            return Ok(());
        }
        // Fallback
        file.set_len(size)
            .with_context(|| format!("set_len fallback failed for size {size}"))?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        file.set_len(size)
            .with_context(|| format!("set_len failed for size {size}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_available_space_returns_positive_numbers() {
        let tmp = std::env::temp_dir();
        let space = query_available_space(&tmp).expect("query space works");
        assert!(space.available_bytes > 0);
        assert!(space.total_bytes >= space.available_bytes);
    }

    #[test]
    fn ensure_sufficient_space_accepts_small_size() {
        let tmp = std::env::temp_dir().join(format!("space-check-{}", uuid::Uuid::new_v4()));
        assert!(ensure_sufficient_space(&tmp, 1024).is_ok());
    }

    #[test]
    fn ensure_sufficient_space_rejects_huge_size() {
        let tmp = std::env::temp_dir().join(format!("space-check-{}", uuid::Uuid::new_v4()));
        let huge_size = u64::MAX / 2;
        assert!(ensure_sufficient_space(&tmp, huge_size).is_err());
    }

    #[test]
    fn preallocate_file_creates_sparse_or_sized_file() {
        let dir = std::env::temp_dir().join(format!("rdl-prealloc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.bin");

        let f = File::create(&path).unwrap();
        let target_size = 10 * 1024 * 1024; // 10 MiB
        preallocate_file(&f, target_size).unwrap();
        drop(f);

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), target_size);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

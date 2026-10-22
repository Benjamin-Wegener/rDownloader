//! Auto-extraction of finished archives (.zip, .7z, .rar, including multi-part sets).
//!
//! Archives come from untrusted servers, so extraction is defensive:
//! - entry names are checked ([`safe_join`]): no absolute paths, no `..`, no NUL;
//! - symbolic links inside archives are never created;
//! - existing files are never overwritten (output goes to a fresh directory);
//! - archive permissions are ignored (no setuid bits);
//! - the number of entries and the total unpacked size are limited ([`Limits`]),
//!   which stops zip bombs while they unpack.
//!
//! ZIP and 7z are unpacked in-process. RAR has no pure-Rust decoder, so an external
//! tool is used (`unrar`, `7zz`/`7z` or `bsdtar`, in that order); its output is
//! verified afterwards. Split ZIP archives (`.z01`, `.zip.001`) are not supported.

use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    SevenZ,
    Rar,
}

/// All files of one archive, in volume order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSet {
    pub kind: ArchiveKind,
    pub parts: Vec<PathBuf>,
}

impl ArchiveSet {
    pub fn first(&self) -> &Path {
        &self.parts[0]
    }

    /// Name for the output directory: the first part without archive suffixes.
    pub fn stem(&self) -> String {
        let name = self
            .first()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let lower = name.to_ascii_lowercase();
        let mut end = name.len();
        for suffix in [".001", ".rar", ".zip", ".7z"] {
            if lower[..end].ends_with(suffix) {
                end -= suffix.len();
            }
        }
        // `name.part01` / `name.part1` → `name`
        let base = &name[..end];
        match base.to_ascii_lowercase().rfind(".part") {
            Some(i) if base[i + 5..].bytes().all(|b| b.is_ascii_digit()) && i + 5 < base.len() => {
                base[..i].to_string()
            }
            _ => base.to_string(),
        }
    }
}

/// Limits that bound the damage of a malicious archive.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_total_bytes: u64,
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_entries: 100_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub dest: PathBuf,
    pub files: usize,
    pub bytes: u64,
    /// Entries that were deliberately not extracted (symlinks).
    pub skipped: usize,
}

// ───────────────────────── detection ─────────────────────────

fn sibling(dir: &Path, name: &str) -> Option<PathBuf> {
    // Case-insensitive, since the files of one set may differ in case.
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        e.file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(name)
            .then(|| e.path())
    })
}

/// Existing files `make(start)`, `make(start + 1)`, … up to the first gap.
fn run_of(dir: &Path, start: u32, make: impl Fn(u32) -> String) -> Vec<PathBuf> {
    (start..).map_while(|n| sibling(dir, &make(n))).collect()
}

/// `(prefix, digits)` for names like `base.part03.rar` → (`base`, `03`).
fn split_part_rar(lower_name: &str) -> Option<(usize, usize)> {
    let stem = lower_name.strip_suffix(".rar")?;
    let i = stem.rfind(".part")?;
    let digits = &stem[i + 5..];
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some((i, digits.len()))
}

/// Works out which archive the finished file `path` belongs to.
///
/// Returns the set only when `path` is the newest part present, so each arriving
/// part triggers at most one attempt, and `None` for files that are no archive.
/// The set may still be incomplete (the last volume is unknown from names alone);
/// extraction then fails cleanly and the next arriving part retries.
pub fn detect(path: &Path) -> Option<ArchiveSet> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy().to_string();
    let lower = name.to_ascii_lowercase();

    let (kind, parts) = if lower.ends_with(".zip") {
        (ArchiveKind::Zip, vec![path.to_path_buf()])
    } else if lower.ends_with(".7z") {
        (ArchiveKind::SevenZ, vec![path.to_path_buf()])
    } else if let Some(base) = lower.rfind(".7z.").filter(|&i| {
        let tail = &lower[i + 4..];
        !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())
    }) {
        let width = lower.len() - base - 4;
        let prefix = &name[..base + 3];
        (
            ArchiveKind::SevenZ,
            run_of(dir, 1, |n| format!("{prefix}.{n:0width$}")),
        )
    } else if let Some((i, width)) = split_part_rar(&lower) {
        let prefix = &name[..i];
        (
            ArchiveKind::Rar,
            run_of(dir, 1, |n| format!("{prefix}.part{n:0width$}.rar")),
        )
    } else if let Some(stem) = lower.strip_suffix(".rar") {
        // Old style: name.rar, then name.r00, name.r01 …
        let stem = &name[..stem.len()];
        let mut parts = vec![path.to_path_buf()];
        parts.extend(run_of(dir, 0, |n| format!("{stem}.r{n:02}")));
        (ArchiveKind::Rar, parts)
    } else if lower.len() > 4
        && lower[lower.len() - 4..].starts_with(".r")
        && lower[lower.len() - 2..].bytes().all(|b| b.is_ascii_digit())
    {
        // A volume `name.r00`: the set is anchored at `name.rar`.
        let stem = &name[..name.len() - 4];
        let first = sibling(dir, &format!("{stem}.rar"))?;
        let mut parts = vec![first];
        parts.extend(run_of(dir, 0, |n| format!("{stem}.r{n:02}")));
        (ArchiveKind::Rar, parts)
    } else {
        return None;
    };

    let newest = parts.last()?;
    let is_newest = newest.file_name().map(|n| n.to_ascii_lowercase())
        == path.file_name().map(|n| n.to_ascii_lowercase());
    // Multi-part sets must start at the first volume.
    (is_newest && parts[0].exists()).then_some(ArchiveSet { kind, parts })
}

// ───────────────────────── safety helpers ─────────────────────────

/// Joins an archive entry name onto `dest`, refusing anything that could leave it.
pub fn safe_join(dest: &Path, entry_name: &str) -> Result<PathBuf> {
    if entry_name.is_empty() || entry_name.contains('\0') {
        bail!("invalid entry name");
    }
    let normalized = entry_name.replace('\\', "/");
    let mut out = dest.to_path_buf();
    for comp in Path::new(&normalized).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => bail!("unsafe entry path: {entry_name:?}"),
        }
    }
    if out == dest {
        bail!("entry resolves to the destination itself: {entry_name:?}");
    }
    Ok(out)
}

/// Tracks how much an archive has produced so far.
struct Budget {
    limits: Limits,
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            bail!("archive has more than {} entries", self.limits.max_entries);
        }
        Ok(())
    }

    /// Copies `reader` into `writer`, aborting once the total size limit is hit.
    fn copy(&mut self, reader: &mut dyn Read, writer: &mut dyn Write) -> Result<u64> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                return Ok(written);
            }
            self.bytes += n as u64;
            if self.bytes > self.limits.max_total_bytes {
                bail!(
                    "archive unpacks to more than {} bytes",
                    self.limits.max_total_bytes
                );
            }
            writer.write_all(&buf[..n])?;
            written += n as u64;
        }
    }
}

fn write_entry(dest: &Path, name: &str, reader: &mut dyn Read, budget: &mut Budget) -> Result<()> {
    let path = safe_join(dest, name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // `create_new`: never overwrite, never follow a pre-existing symlink.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    budget.copy(reader, &mut file)?;
    Ok(())
}

/// A fresh directory next to the archive: `name`, `name-1`, `name-2` …
fn fresh_dir(parent: &Path, stem: &str) -> Result<PathBuf> {
    let stem = if stem.is_empty() { "extracted" } else { stem };
    for n in 0..1000 {
        let candidate = if n == 0 {
            parent.join(stem)
        } else {
            parent.join(format!("{stem}-{n}"))
        };
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("creating extraction directory"),
        }
    }
    bail!("no free extraction directory name for {stem}")
}

// ───────────────────────── formats ─────────────────────────

fn extract_zip(set: &ArchiveSet, dest: &Path, budget: &mut Budget) -> Result<(usize, usize)> {
    if set.parts.len() != 1 {
        bail!("split ZIP archives are not supported");
    }
    let mut zip = zip::ZipArchive::new(io::BufReader::new(File::open(set.first())?))
        .context("reading ZIP")?;
    let (mut files, mut skipped) = (0, 0);
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        budget.entry()?;
        if entry.encrypted() {
            bail!("encrypted ZIP entries are not supported");
        }
        if entry.is_symlink() {
            skipped += 1;
            continue;
        }
        let name = entry
            .name()
            .map_err(|e| anyhow::anyhow!("bad ZIP entry name: {e}"))?
            .to_string();
        if entry.is_dir() {
            fs::create_dir_all(safe_join(dest, name.trim_end_matches('/'))?)?;
        } else {
            write_entry(dest, &name, &mut entry, budget)?;
            files += 1;
        }
    }
    Ok((files, skipped))
}

/// Reads several volume files as one contiguous, seekable stream.
struct MultiReader {
    files: Vec<(File, u64)>,
    total: u64,
    pos: u64,
}

impl MultiReader {
    fn open(parts: &[PathBuf]) -> io::Result<Self> {
        let mut files = Vec::new();
        let mut total = 0;
        for p in parts {
            let f = File::open(p)?;
            let len = f.metadata()?.len();
            total += len;
            files.push((f, len));
        }
        Ok(Self {
            files,
            total,
            pos: 0,
        })
    }
}

impl Read for MultiReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut offset = self.pos;
        for (file, len) in &mut self.files {
            if offset < *len {
                file.seek(SeekFrom::Start(offset))?;
                let want = buf.len().min((*len - offset) as usize);
                let n = file.read(&mut buf[..want])?;
                self.pos += n as u64;
                return Ok(n);
            }
            offset -= *len;
        }
        Ok(0)
    }
}

impl Seek for MultiReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let new = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => self.total as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

fn extract_7z(set: &ArchiveSet, dest: &Path, budget: &mut Budget) -> Result<(usize, usize)> {
    let reader = MultiReader::open(&set.parts)?;
    let mut sz = sevenz_rust2::ArchiveReader::new(reader, sevenz_rust2::Password::empty())
        .map_err(|e| anyhow::anyhow!("reading 7z: {e}"))?;
    let mut files = 0;
    let mut failure: Option<anyhow::Error> = None;
    let outcome = sz.for_each_entries(|entry, reader| {
        let mut run = || -> Result<()> {
            budget.entry()?;
            if entry.is_directory() {
                // 7z may contain a nameless root directory entry; there is nothing to create.
                let dir = entry.name().trim_end_matches('/');
                if !dir.is_empty() {
                    fs::create_dir_all(safe_join(dest, dir)?)?;
                }
            } else if entry.has_stream() {
                write_entry(dest, entry.name(), reader, budget)?;
                files += 1;
            }
            Ok(())
        };
        match run() {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false) // stop iterating
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    outcome.map_err(|e| anyhow::anyhow!("unpacking 7z: {e}"))?;
    Ok((files, 0))
}

fn tool_exists(tool: &str) -> bool {
    Command::new(tool).arg("--help").output().is_ok()
}

fn extract_rar(set: &ArchiveSet, dest: &Path) -> Result<()> {
    let first = set.first();
    let status = if tool_exists("unrar") {
        Command::new("unrar")
            .args(["x", "-idq", "-o-", "-y"])
            .arg(first)
            .arg(format!("{}/", dest.display()))
            .status()
    } else if let Some(tool) = ["7zz", "7z"].into_iter().find(|t| tool_exists(t)) {
        Command::new(tool)
            .args(["x", "-y"])
            .arg(format!("-o{}", dest.display()))
            .arg(first)
            .status()
    } else if tool_exists("bsdtar") {
        Command::new("bsdtar")
            .arg("-xf")
            .arg(first)
            .arg("-C")
            .arg(dest)
            .arg("--no-same-owner")
            .status()
    } else {
        bail!("RAR needs an external tool: install unrar, 7-Zip (7zz) or bsdtar");
    }
    .context("running the RAR tool")?;
    if !status.success() {
        bail!("the RAR tool failed (missing volume or damaged archive?)");
    }
    Ok(())
}

/// External tools are not trusted blindly: drop symlinks and verify containment.
fn sanitize_tree(dest: &Path, budget: &mut Budget) -> Result<(usize, u64, usize)> {
    let root = dest.canonicalize()?;
    let (mut files, mut bytes, mut removed) = (0, 0, 0);
    let mut stack = vec![dest.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            budget.entry()?;
            if kind.is_symlink() {
                fs::remove_file(&path)?;
                removed += 1;
            } else if kind.is_dir() {
                if !path.canonicalize()?.starts_with(&root) {
                    bail!("extracted path escapes the destination: {}", path.display());
                }
                stack.push(path);
            } else {
                files += 1;
                bytes += entry.metadata()?.len();
                if bytes > budget.limits.max_total_bytes {
                    bail!(
                        "archive unpacked to more than {} bytes",
                        budget.limits.max_total_bytes
                    );
                }
            }
        }
    }
    Ok((files, bytes, removed))
}

// ───────────────────────── public API ─────────────────────────

/// Unpacks `set` into a fresh directory inside `parent`.
/// On failure the partly written directory is removed.
pub fn extract_set(set: &ArchiveSet, parent: &Path, limits: Limits) -> Result<Report> {
    let dest = fresh_dir(parent, &set.stem())?;
    let mut budget = Budget {
        limits,
        entries: 0,
        bytes: 0,
    };
    let result = match set.kind {
        ArchiveKind::Zip => extract_zip(set, &dest, &mut budget).map(|(f, s)| (f, budget.bytes, s)),
        ArchiveKind::SevenZ => {
            extract_7z(set, &dest, &mut budget).map(|(f, s)| (f, budget.bytes, s))
        }
        ArchiveKind::Rar => {
            extract_rar(set, &dest).and_then(|()| sanitize_tree(&dest, &mut budget))
        }
    };
    match result {
        Ok((files, bytes, skipped)) => Ok(Report {
            dest,
            files,
            bytes,
            skipped,
        }),
        Err(e) => {
            let _ = fs::remove_dir_all(&dest);
            Err(e)
        }
    }
}

/// Files for which extraction already succeeded or is running (one attempt per set).
fn claimed() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static CLAIMED: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    CLAIMED.get_or_init(Default::default)
}

/// Called after a download finished. Extracts the archive the file belongs to, if
/// it is one and all parts seem present. `Ok(None)` means: nothing to do (yet).
pub fn auto_extract(
    finished: &Path,
    limits: Limits,
    delete_archives: bool,
) -> Result<Option<Report>> {
    let Some(set) = detect(finished) else {
        return Ok(None);
    };
    let key = set.first().to_path_buf();
    if !claimed().lock().unwrap().insert(key.clone()) {
        return Ok(None); // another job is already handling this set
    }
    let parent = set.first().parent().unwrap_or(Path::new(".")).to_path_buf();
    match extract_set(&set, &parent, limits) {
        Ok(report) => {
            if delete_archives {
                for part in &set.parts {
                    let _ = fs::remove_file(part);
                }
            }
            Ok(Some(report))
        }
        Err(e) => {
            // Not claimed any more: a later part (or a retry) may complete the set.
            claimed().lock().unwrap().remove(&key);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("rdl-extract-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn touch(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::write(&p, b"x").unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    fn names(p: &[PathBuf]) -> Vec<String> {
        p.iter()
            .map(|x| x.file_name().unwrap().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let d = Path::new("/out");
        assert_eq!(safe_join(d, "a/b.txt").unwrap(), Path::new("/out/a/b.txt"));
        assert_eq!(safe_join(d, "a\\b.txt").unwrap(), Path::new("/out/a/b.txt"));
        for bad in ["../x", "a/../../x", "/etc/passwd", "", "a\0b", "..", "."] {
            assert!(safe_join(d, bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn detects_single_and_multi_part_sets() {
        let t = Tmp::new();
        let z = t.touch("a.ZIP");
        assert_eq!(detect(&z).unwrap().kind, ArchiveKind::Zip);
        assert!(detect(&t.touch("notes.txt")).is_none());

        t.touch("b.7z.001");
        let second = t.touch("b.7z.002");
        let set = detect(&second).unwrap();
        assert_eq!(set.kind, ArchiveKind::SevenZ);
        assert_eq!(names(&set.parts), ["b.7z.001", "b.7z.002"]);
        assert!(
            detect(&t.0.join("b.7z.001")).is_none(),
            "not the newest part any more"
        );

        t.touch("c.part01.rar");
        let p2 = t.touch("c.part02.rar");
        let set = detect(&p2).unwrap();
        assert_eq!(
            (set.kind, names(&set.parts)),
            (
                ArchiveKind::Rar,
                vec!["c.part01.rar".into(), "c.part02.rar".into()]
            )
        );
        assert_eq!(set.stem(), "c");

        t.touch("d.rar");
        let r1 = t.touch("d.r00");
        let set = detect(&r1).unwrap();
        assert_eq!(names(&set.parts), ["d.rar", "d.r00"]);
    }

    #[test]
    fn later_part_without_first_is_not_a_set() {
        let t = Tmp::new();
        assert!(detect(&t.touch("e.part02.rar")).is_none());
        assert!(detect(&t.touch("f.7z.002")).is_none());
    }

    #[test]
    fn extracts_zip_into_fresh_directory() {
        let t = Tmp::new();
        let z = t.0.join("pack.zip");
        make_zip(&z, &[("a.txt", b"hello"), ("dir/b.txt", b"world")]);
        let set = detect(&z).unwrap();
        let r = extract_set(&set, &t.0, Limits::default()).unwrap();
        assert_eq!((r.files, r.bytes), (2, 10));
        assert_eq!(fs::read(r.dest.join("dir/b.txt")).unwrap(), b"world");
        // A second extraction never overwrites: it gets its own directory.
        let r2 = extract_set(&set, &t.0, Limits::default()).unwrap();
        assert_ne!(r.dest, r2.dest);
    }

    #[test]
    fn zip_slip_is_blocked_and_cleaned_up() {
        let t = Tmp::new();
        let z = t.0.join("evil.zip");
        make_zip(&z, &[("ok.txt", b"fine"), ("../escaped.txt", b"bad")]);
        let err = extract_set(&detect(&z).unwrap(), &t.0, Limits::default()).unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");
        assert!(!t.0.join("escaped.txt").exists());
        assert!(!t.0.join("evil").exists(), "partial output is removed");
    }

    #[test]
    fn size_and_entry_limits_stop_bombs() {
        let t = Tmp::new();
        let z = t.0.join("bomb.zip");
        make_zip(&z, &[("big.bin", &vec![0u8; 1_000_000])]);
        let set = detect(&z).unwrap();
        let small = Limits {
            max_total_bytes: 1000,
            ..Limits::default()
        };
        assert!(extract_set(&set, &t.0, small)
            .unwrap_err()
            .to_string()
            .contains("unpacks to more"));
        let few = Limits {
            max_entries: 0,
            ..Limits::default()
        };
        assert!(extract_set(&set, &t.0, few).is_err());
    }

    #[test]
    fn extracts_split_7z_volumes() {
        let t = Tmp::new();
        let src = t.0.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("one.txt"), b"first file").unwrap();
        fs::write(src.join("sub/two.txt"), vec![7u8; 5000]).unwrap();
        let archive = t.0.join("whole.7z");
        sevenz_rust2::compress_to_path(&src, &archive).unwrap();

        // Cut the archive into three volumes.
        let data = fs::read(&archive).unwrap();
        fs::remove_file(&archive).unwrap();
        let cut = data.len() / 3;
        fs::write(t.0.join("vol.7z.001"), &data[..cut]).unwrap();
        fs::write(t.0.join("vol.7z.002"), &data[cut..2 * cut]).unwrap();
        let last = t.0.join("vol.7z.003");
        fs::write(&last, &data[2 * cut..]).unwrap();

        let set = detect(&last).unwrap();
        assert_eq!(set.parts.len(), 3);
        let r = extract_set(&set, &t.0, Limits::default()).unwrap();
        assert_eq!(r.files, 2);
        assert_eq!(
            fs::read(r.dest.join("sub/two.txt")).unwrap(),
            vec![7u8; 5000]
        );
    }

    #[test]
    fn incomplete_7z_volumes_fail_cleanly() {
        let t = Tmp::new();
        let src = t.0.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f.txt"), vec![1u8; 4000]).unwrap();
        let archive = t.0.join("x.7z");
        sevenz_rust2::compress_to_path(&src, &archive).unwrap();
        let data = fs::read(&archive).unwrap();
        fs::remove_file(&archive).unwrap();
        let only_first = t.0.join("y.7z.001");
        fs::write(&only_first, &data[..data.len() / 2]).unwrap();
        let set = detect(&only_first).unwrap();
        assert!(extract_set(&set, &t.0, Limits::default()).is_err());
        assert!(!t.0.join("y").exists());
    }

    #[test]
    fn auto_extract_ignores_other_files_and_deletes_on_request() {
        let t = Tmp::new();
        assert!(
            auto_extract(&t.touch("movie.mkv"), Limits::default(), false)
                .unwrap()
                .is_none()
        );

        let z = t.0.join("del.zip");
        make_zip(&z, &[("a.txt", b"hi")]);
        let r = auto_extract(&z, Limits::default(), true).unwrap().unwrap();
        assert_eq!(r.files, 1);
        assert!(!z.exists(), "archive removed after success");
    }

    #[test]
    fn stem_strips_archive_suffixes() {
        let mk = |n: &str| ArchiveSet {
            kind: ArchiveKind::Rar,
            parts: vec![PathBuf::from(n)],
        };
        assert_eq!(mk("/x/Show.S01.part01.rar").stem(), "Show.S01");
        assert_eq!(mk("/x/data.7z.001").stem(), "data");
        assert_eq!(mk("/x/plain.zip").stem(), "plain");
    }
}

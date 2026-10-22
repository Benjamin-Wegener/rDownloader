//! Partial data of downloads (`job-<id>/part-…` folders) that nothing refers to any more.
//!
//! They pile up when downloads are removed or cancelled by older versions, or when a
//! program was killed. Cleaning up is always explicit and never touches the folder of a
//! download that is still known (running, stopped, failed): those can be resumed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A `job-<id>` folder and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub id: Uuid,
    pub path: PathBuf,
    pub bytes: u64,
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| {
                    let p = e.path();
                    match e.metadata() {
                        Ok(m) if m.is_dir() => dir_size(&p),
                        Ok(m) => m.len(),
                        Err(_) => 0,
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// The id in a folder name like `job-0123...-uuid`, or `None` for every other name.
pub fn job_id_of(name: &str) -> Option<Uuid> {
    name.strip_prefix("job-")
        .and_then(|rest| Uuid::parse_str(rest).ok())
}

/// All job folders below `roots` whose id is not in `keep`.
pub fn find_orphans(roots: &[PathBuf], keep: &HashSet<Uuid>) -> Vec<Orphan> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Only real folders: a symlink named like a job folder is never followed.
            let is_real_dir = entry
                .file_type()
                .is_ok_and(|t| t.is_dir() && !t.is_symlink());
            let id = entry.file_name().to_str().and_then(job_id_of);
            if let (true, Some(id)) = (is_real_dir, id) {
                if !keep.contains(&id) && !out.iter().any(|o: &Orphan| o.id == id) {
                    out.push(Orphan {
                        id,
                        bytes: dir_size(&path),
                        path,
                    });
                }
            }
        }
    }
    out
}

/// Deletes the given folders; returns how many were removed and how many bytes that freed.
pub fn remove_orphans(orphans: &[Orphan]) -> (usize, u64) {
    let mut removed = (0, 0);
    for o in orphans {
        if std::fs::remove_dir_all(&o.path).is_ok() {
            removed.0 += 1;
            removed.1 += o.bytes;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!("rdl-partial-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn job_dir(root: &Path, id: Uuid, bytes: usize) -> PathBuf {
        let d = root.join(format!("job-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("part-000"), vec![0u8; bytes]).unwrap();
        d
    }

    #[test]
    fn only_job_folders_are_recognised() {
        let id = Uuid::new_v4();
        assert_eq!(job_id_of(&format!("job-{id}")), Some(id));
        for bad in [
            "job-",
            "job-notauuid",
            "other",
            &id.to_string(),
            "job-..%2f",
        ] {
            assert_eq!(job_id_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn known_downloads_are_never_touched() {
        let root = temp_root();
        let (kept, orphan) = (Uuid::new_v4(), Uuid::new_v4());
        let kept_dir = job_dir(&root, kept, 1000);
        job_dir(&root, orphan, 2500);
        std::fs::create_dir_all(root.join("something-else")).unwrap();
        std::fs::write(root.join("note.txt"), b"x").unwrap();

        let keep: HashSet<Uuid> = [kept].into();
        let found = find_orphans(std::slice::from_ref(&root), &keep);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].id, found[0].bytes), (orphan, 2500));

        assert_eq!(remove_orphans(&found), (1, 2500));
        assert!(kept_dir.exists(), "the folder of a known download stays");
        assert!(root.join("something-else").exists() && root.join("note.txt").exists());
        assert!(find_orphans(std::slice::from_ref(&root), &keep).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_same_job_in_two_roots_is_listed_once_and_missing_roots_are_fine() {
        let (a, b) = (temp_root(), temp_root());
        let id = Uuid::new_v4();
        job_dir(&a, id, 10);
        job_dir(&b, id, 10);
        let found = find_orphans(
            &[a.clone(), b.clone(), PathBuf::from("/no/such/dir")],
            &HashSet::new(),
        );
        assert_eq!(found.len(), 1);
        std::fs::remove_dir_all(a).unwrap();
        std::fs::remove_dir_all(b).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_named_like_a_job_folder_is_ignored() {
        let root = temp_root();
        let target = temp_root();
        std::fs::write(target.join("precious"), b"keep me").unwrap();
        let id = Uuid::new_v4();
        std::os::unix::fs::symlink(&target, root.join(format!("job-{id}"))).unwrap();
        assert!(find_orphans(std::slice::from_ref(&root), &HashSet::new()).is_empty());
        std::fs::remove_dir_all(root).unwrap();
        assert!(target.join("precious").exists());
        std::fs::remove_dir_all(target).unwrap();
    }
}

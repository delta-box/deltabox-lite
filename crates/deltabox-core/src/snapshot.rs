//! Scan snapshots, snapshot diffing, and filesystem tree helpers.

use crate::changeset::{describe_change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{Error, Result};
use crate::session::now_ms;
use crate::worker::ScanEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const BASE_SCAN_FILE: &str = "base_scan.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanSnapshot {
    pub entries: Vec<ScanEntry>,
    pub taken_at_ms: u64,
}

pub fn load_snapshot(path: &Path) -> Result<Option<ScanSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

pub fn save_snapshot(path: &Path, entries: &[ScanEntry]) -> Result<()> {
    let snapshot = ScanSnapshot {
        entries: entries.to_vec(),
        taken_at_ms: now_ms(),
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&snapshot)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SnapshotDiff {
    pub path: String,
    pub kind: ChangeKind,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
}

pub fn diff_snapshots(from: &[ScanEntry], to: &[ScanEntry]) -> Vec<SnapshotDiff> {
    let mut by_path: HashMap<&str, &ScanEntry> =
        from.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut out = Vec::new();
    for entry in to {
        match by_path.remove(entry.path.as_str()) {
            None => out.push(SnapshotDiff {
                path: entry.path.clone(),
                kind: ChangeKind::Added,
                old_size: None,
                new_size: Some(entry.size),
            }),
            Some(old) => {
                if old.size != entry.size
                    || old.mtime_ns != entry.mtime_ns
                    || old.kind != entry.kind
                {
                    out.push(SnapshotDiff {
                        path: entry.path.clone(),
                        kind: ChangeKind::Modified,
                        old_size: Some(old.size),
                        new_size: Some(entry.size),
                    });
                }
            }
        }
    }
    for old in by_path.values() {
        out.push(SnapshotDiff {
            path: old.path.clone(),
            kind: ChangeKind::Deleted,
            old_size: Some(old.size),
            new_size: None,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Recursively copy a tree, preserving symlinks and file modes.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<usize> {
    let mut copied = 0usize;
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|_| Error::Backend("copy path escapes source".into()))?;
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if ft.is_symlink() {
            let link = std::fs::read_link(entry.path())?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&target);
            std::os::unix::fs::symlink(link, &target)?;
            copied += 1;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            copy_mode(entry.path(), &target)?;
            preserve_mtime(entry.path(), &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Move project files out of the way (used to persist deletions on
/// commit/abort). Missing sources are ignored.
pub fn move_paths_to_dir(root: &Path, rel_paths: &[String], dst_dir: &Path) -> Result<usize> {
    let mut moved = 0usize;
    for rel in rel_paths {
        let src = root.join(rel);
        let dst = dst_dir.join(rel);
        match std::fs::symlink_metadata(&src) {
            Ok(_) => {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&src, &dst)?;
                moved += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(moved)
}

/// Read a file's content only when it is small enough for a text diff.
pub fn read_capped_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {
            if m.len() > DEFAULT_TEXT_SIZE_LIMIT {
                return Ok(None);
            }
            Ok(Some(std::fs::read(path)?))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn describe(
    path: &str,
    kind: ChangeKind,
    old_size: Option<u64>,
    new_size: Option<u64>,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
) -> crate::changeset::Change {
    describe_change(
        path,
        kind,
        old_size,
        new_size,
        old_bytes,
        new_bytes,
        DEFAULT_TEXT_SIZE_LIMIT,
    )
}

fn copy_mode(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(src)?.permissions().mode();
    let mut perms = std::fs::metadata(dst)?.permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(dst, perms)?;
    Ok(())
}

fn preserve_mtime(src: &Path, dst: &Path) -> Result<()> {
    let mtime = std::fs::metadata(src)?.modified()?;
    let file = std::fs::File::options().write(true).open(dst)?;
    file.set_modified(mtime)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, mtime: i64) -> ScanEntry {
        ScanEntry {
            path: path.into(),
            size,
            mtime_ns: mtime,
            kind: crate::worker::ScanEntryKind::File,
        }
    }

    #[test]
    fn diff_detects_add_modify_delete() {
        let from = vec![entry("a", 1, 1), entry("b", 1, 1), entry("c", 1, 1)];
        let to = vec![entry("a", 2, 1), entry("b", 1, 1), entry("d", 1, 1)];
        let diffs = diff_snapshots(&from, &to);
        assert_eq!(diffs.len(), 3);
        assert!(diffs
            .iter()
            .any(|d| d.path == "a" && d.kind == ChangeKind::Modified));
        assert!(diffs
            .iter()
            .any(|d| d.path == "c" && d.kind == ChangeKind::Deleted));
        assert!(diffs
            .iter()
            .any(|d| d.path == "d" && d.kind == ChangeKind::Added));
    }

    #[test]
    fn copy_tree_preserves_symlinks_and_modes() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("f.txt"), "x").unwrap();
        std::os::unix::fs::symlink("f.txt", src.path().join("link")).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let copied = copy_tree(src.path(), dst.path()).unwrap();
        assert_eq!(copied, 2);
        assert!(dst.path().join("link").is_symlink());
        assert_eq!(
            std::fs::read_to_string(dst.path().join("f.txt")).unwrap(),
            "x"
        );
    }

    #[test]
    fn move_paths_moves_and_ignores_missing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("keep.txt"), "keep").unwrap();
        let dst = tempfile::tempdir().unwrap();
        let moved = move_paths_to_dir(
            root.path(),
            &["keep.txt".into(), "missing.txt".into()],
            dst.path(),
        )
        .unwrap();
        assert_eq!(moved, 1);
        assert!(!root.path().join("keep.txt").exists());
        assert!(dst.path().join("keep.txt").exists());
    }
}

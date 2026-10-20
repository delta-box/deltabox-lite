//! Scan snapshots, snapshot diffing, and filesystem tree helpers.

use crate::changeset::{describe_change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{Error, Result};
use crate::session::now_ms;
use crate::worker::{ScanEntry, ScanEntryKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{copy, Read, Write};
use std::path::Path;

pub const BASE_SCAN_FILE: &str = "base_scan.json";
pub const CURRENT_SCAN_SNAPSHOT_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanSnapshot {
    #[serde(default)]
    pub version: u32,
    pub entries: Vec<ScanEntry>,
    pub taken_at_ms: u64,
}

impl ScanSnapshot {
    pub fn has_current_fingerprints(&self) -> bool {
        self.version == CURRENT_SCAN_SNAPSHOT_VERSION
            && self.entries.iter().all(|entry| entry.fingerprint.is_some())
    }
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
        version: CURRENT_SCAN_SNAPSHOT_VERSION,
        entries: entries.to_vec(),
        taken_at_ms: now_ms(),
    };
    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(&snapshot)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
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
                if scan_entry_changed(old, entry) {
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

fn scan_entry_changed(old: &ScanEntry, new: &ScanEntry) -> bool {
    if old.kind != new.kind {
        return true;
    }
    match (&old.fingerprint, &new.fingerprint) {
        (Some(old_fingerprint), Some(new_fingerprint)) => {
            old_fingerprint != new_fingerprint
                || (old.kind != ScanEntryKind::Directory
                    && (old.size != new.size || old.mtime_ns != new.mtime_ns))
        }
        (None, None) => old.size != new.size || old.mtime_ns != new.mtime_ns,
        _ => true,
    }
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
            preserve_symlink_times(entry.path(), &target)?;
            copied += 1;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            clone_or_copy_file(entry.path(), &target)?;
            preserve_mtime(entry.path(), &target)?;
            copy_mode(entry.path(), &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Clone a regular file with copy-on-write when the filesystem supports it.
/// Unsupported filesystems transparently fall back to an ordinary byte copy.
pub fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<u64> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(src.as_os_str().as_bytes())
            .map_err(|_| Error::Backend("source path contains a NUL byte".into()))?;
        let target = CString::new(dst.as_os_str().as_bytes())
            .map_err(|_| Error::Backend("target path contains a NUL byte".into()))?;
        const CLONE_ACL: libc::c_uint = 0x0004;
        if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), CLONE_ACL) } == 0 {
            make_owner_writable(dst)?;
            return Ok(std::fs::metadata(dst)?.len());
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOTSUP | libc::EXDEV | libc::EINVAL) => {}
            Some(libc::EEXIST) => {
                std::fs::remove_file(dst)?;
                if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), CLONE_ACL) } == 0 {
                    make_owner_writable(dst)?;
                    return Ok(std::fs::metadata(dst)?.len());
                }
            }
            _ => {}
        }
        if dst.exists() {
            std::fs::remove_file(dst)?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        const FICLONE: libc::c_ulong = 0x4004_9409;
        use std::os::fd::AsRawFd;
        let source = File::open(src)?;
        let target = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dst)?;
        let cloned = unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0;
        drop(target);
        if cloned {
            return Ok(std::fs::metadata(dst)?.len());
        }
        std::fs::remove_file(dst)?;
        if let Some(copied) = copy_sparse_file(src, dst)? {
            return Ok(copied);
        }
    }
    let mut source = File::open(src)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    Ok(copy(&mut source, &mut target)?)
}

#[cfg(target_os = "linux")]
fn copy_sparse_file(src: &Path, dst: &Path) -> Result<Option<u64>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;

    let source = File::open(src)?;
    let length = source.metadata()?.len();
    let target = OpenOptions::new().write(true).create_new(true).open(dst)?;
    target.set_len(length)?;
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    while offset < length {
        let data = unsafe { libc::lseek(source.as_raw_fd(), offset as i64, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            if error.raw_os_error() == Some(libc::EINVAL) {
                drop(target);
                std::fs::remove_file(dst)?;
                return Ok(None);
            }
            return Err(error.into());
        }
        let hole = unsafe { libc::lseek(source.as_raw_fd(), data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut position = data as u64;
        let end = (hole as u64).min(length);
        while position < end {
            let count = buffer.len().min((end - position) as usize);
            source.read_exact_at(&mut buffer[..count], position)?;
            target.write_all_at(&buffer[..count], position)?;
            position += count as u64;
        }
        offset = end;
    }
    Ok(Some(length))
}

#[cfg(target_os = "macos")]
fn make_owner_writable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

/// Copy `src` to `dst`, skipping files already frozen in `prev` with the same
/// size/mtime/type. Used for checkpoint freeze so unchanged upper files are
/// not re-copied on every checkpoint.
pub fn copy_tree_incremental(src: &Path, prev: Option<&Path>, dst: &Path) -> Result<usize> {
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
            continue;
        }
        let unchanged = if let Some(p) = prev {
            let prev_path = p.join(rel);
            match (
                std::fs::symlink_metadata(&prev_path),
                std::fs::symlink_metadata(entry.path()),
            ) {
                (Ok(a), Ok(b)) => same_entry(&prev_path, entry.path(), &a, &b)?,
                _ => false,
            }
        } else {
            false
        };
        if unchanged {
            continue;
        }
        if ft.is_symlink() {
            let link = std::fs::read_link(entry.path())?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&target);
            std::os::unix::fs::symlink(link, &target)?;
            preserve_symlink_times(entry.path(), &target)?;
            copied += 1;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            preserve_mtime(entry.path(), &target)?;
            copy_mode(entry.path(), &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn same_entry(
    a_path: &Path,
    b_path: &Path,
    a: &std::fs::Metadata,
    b: &std::fs::Metadata,
) -> Result<bool> {
    if a.len() != b.len()
        || a.file_type().is_symlink() != b.file_type().is_symlink()
        || a.modified().ok() != b.modified().ok()
    {
        return Ok(false);
    }
    if a.file_type().is_symlink() {
        return Ok(std::fs::read_link(a_path)? == std::fs::read_link(b_path)?);
    }
    if !a.is_file() || !b.is_file() {
        return Ok(false);
    }
    let (mut a, mut b) = (File::open(a_path)?, File::open(b_path)?);
    let (mut a_buf, mut b_buf) = ([0u8; 64 * 1024], [0u8; 64 * 1024]);
    loop {
        let a_len = a.read(&mut a_buf)?;
        let b_len = b.read(&mut b_buf)?;
        if a_len != b_len || a_buf[..a_len] != b_buf[..b_len] {
            return Ok(false);
        }
        if a_len == 0 {
            return Ok(true);
        }
    }
}

pub fn preserve_symlink_times(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(src)?;
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    let path = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| Error::Backend("symlink path contains a NUL byte".into()))?;
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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

/// Build scan entries for a merged path list. Metadata is read natively from
/// the COW upper layer first, then the project lower layer, so the worker
/// only has to provide the exact merged path set (including deletions).
pub fn build_scan_entries(paths: &[String], upper: &Path, lower: &Path) -> Vec<ScanEntry> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let upper_path = upper.join(path);
        let lower_path = lower.join(path);
        let meta = match std::fs::symlink_metadata(&upper_path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::symlink_metadata(&lower_path).ok()
            }
            Err(_) => None,
        };
        if let Some(meta) = meta {
            let kind = if meta.file_type().is_symlink() {
                crate::worker::ScanEntryKind::Symlink
            } else {
                crate::worker::ScanEntryKind::File
            };
            out.push(ScanEntry {
                path: path.clone(),
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
                kind,
                fingerprint: None,
            });
        }
    }
    out
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
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
            fingerprint: None,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn clone_or_copy_preserves_sparse_extents() {
        use std::os::unix::fs::{FileExt, MetadataExt};

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        let file = File::create(&source).unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        file.write_all_at(b"tail", 16 * 1024 * 1024 - 4).unwrap();
        clone_or_copy_file(&source, &destination).unwrap();
        let metadata = std::fs::metadata(destination).unwrap();
        assert_eq!(metadata.len(), 16 * 1024 * 1024);
        assert!(metadata.blocks() * 512 < metadata.len());
    }

    #[test]
    fn incremental_copy_skips_unchanged_files() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("same.txt"), "same").unwrap();
        std::fs::write(src.path().join("changed.txt"), "v1").unwrap();
        let prev = tempfile::tempdir().unwrap();
        // Simulate a previous freeze: copies preserve mtimes.
        copy_tree_incremental(src.path(), None, prev.path()).unwrap();
        // Now change one file in the live tree.
        std::fs::write(src.path().join("changed.txt"), "v2").unwrap();
        preserve_mtime(&prev.path().join("changed.txt"), &src.path().join("changed.txt")).unwrap();
        let dst = tempfile::tempdir().unwrap();

        let copied = copy_tree_incremental(src.path(), Some(prev.path()), dst.path()).unwrap();
        assert_eq!(copied, 1, "only changed.txt is copied");
        assert_eq!(
            std::fs::read_to_string(dst.path().join("changed.txt")).unwrap(),
            "v2"
        );
        assert!(!dst.path().join("same.txt").exists());
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

    #[test]
    fn build_scan_entries_prefers_upper() {
        let root = tempfile::tempdir().unwrap();
        let upper = root.path().join("upper");
        let lower = root.path().join("lower");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::create_dir_all(&lower).unwrap();
        std::fs::write(lower.join("a.txt"), "lower").unwrap();
        std::fs::write(upper.join("a.txt"), "upper").unwrap();
        std::fs::write(lower.join("b.txt"), "b").unwrap();
        std::os::unix::fs::symlink("a.txt", lower.join("link")).unwrap();

        let entries = build_scan_entries(
            &["a.txt".into(), "b.txt".into(), "link".into()],
            &upper,
            &lower,
        );
        assert_eq!(entries.len(), 3);
        let a = entries.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(a.size, 5); // "upper"
        assert!(entries.iter().any(|e| e.path == "link"));
    }
}

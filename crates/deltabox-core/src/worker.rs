//! Helpers shared by the sandbox backends: the output cap, capped pipe
//! reading, and project scans.
//!
//! The in-sandbox worker that used to live here is gone: commands now run in
//! the calling process with a sandbox built for each one (see
//! `crate::backend::run_once`).

#[cfg(any(target_os = "macos", test))]
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
#[cfg(any(target_os = "macos", test))]
use std::path::Path;

pub const MAX_EXEC_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanEntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanFingerprint {
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub kind: ScanEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<ScanFingerprint>,
}

/// Directories excluded from project scans.
pub fn is_excluded_dir(name: &str) -> bool {
    name == ".deltabox" || name == ".git"
}

pub(crate) fn read_capped(mut reader: impl Read) -> (String, bool) {
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n <= MAX_EXEC_OUTPUT_BYTES {
                    buf.extend_from_slice(&chunk[..n]);
                } else if buf.len() < MAX_EXEC_OUTPUT_BYTES {
                    let room = MAX_EXEC_OUTPUT_BYTES - buf.len();
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

/// Walk the merged sandbox view and return relative paths of files and
/// symlinks. Metadata is built supervisor-side (native stats), so this scan
/// only pays interception cost for readdir calls.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn scan_root(root: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(&e.file_name().to_string_lossy()));
    for entry in walker {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let ft = entry.file_type();
        if ft.is_dir() {
            continue;
        }
        if !(ft.is_file() || ft.is_symlink()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::Backend("scan path escapes root".into()))?;
        paths.push(rel.to_string_lossy().into_owned());
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_truncates_large_output() {
        let big = vec![b'a'; MAX_EXEC_OUTPUT_BYTES + 100];
        let (text, truncated) = read_capped(big.as_slice());
        assert!(truncated);
        assert_eq!(text.len(), MAX_EXEC_OUTPUT_BYTES);
        let (small, truncated) = read_capped(b"hi".as_slice());
        assert!(!truncated);
        assert_eq!(small, "hi");
    }

    #[test]
    fn scan_skips_git_and_deltabox() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".deltabox")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "x").unwrap();
        std::fs::write(dir.path().join(".deltabox/state"), "x").unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let paths = scan_root(dir.path()).unwrap();
        assert_eq!(paths, vec!["a.txt".to_string()]);
    }
}

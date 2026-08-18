use crate::changeset::ChangeKind;
use crate::error::Result;
use crate::state_layout::state_root;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Project-wide publication log: one JSON line per successful publication,
/// appended while publish.lock is held (which is what makes a plain append
/// file race-free). `publog.head` mirrors the latest seq so readers get it in
/// O(1) instead of scanning the log.
///
/// Consumers: commit-time baseline fast-forward and the folded-publication
/// count (both platforms), sibling-vs-stranger arbitration for macOS
/// validation, and the stale-backup guard in publication recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PublogEntry {
    pub seq: u64,
    pub session_id: String,
    pub ts_ms: u64,
    pub changes: Vec<PublogChange>,
    /// sha256 of each published regular file's content, keyed by path.
    /// Directories/symlinks are absent: consumers must fail closed for them.
    #[serde(default)]
    pub digests: std::collections::BTreeMap<String, String>,
}

/// Platform-neutral record of one published path; the Linux provider maps
/// these 1:1 onto its PublishChange for baseline refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PublogChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(default)]
    pub existed: bool,
    #[serde(default)]
    pub namespace_changed: bool,
}

fn log_path(project: &Path) -> Result<std::path::PathBuf> {
    Ok(state_root(project)?.join("publog.jsonl"))
}

fn head_path(project: &Path) -> Result<std::path::PathBuf> {
    Ok(state_root(project)?.join("publog.head"))
}

/// Latest published seq for the project; 0 when nothing was ever published.
pub(crate) fn head_seq(project: &Path) -> Result<u64> {
    match fs::read_to_string(head_path(project)?) {
        Ok(raw) => Ok(raw.trim().parse().unwrap_or(0)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

/// Appends one publication; caller must hold publish.lock. Returns its seq.
pub(crate) fn append(
    project: &Path,
    session_id: &str,
    changes: &[PublogChange],
    digests: std::collections::BTreeMap<String, String>,
) -> Result<u64> {
    let seq = head_seq(project)? + 1;
    let entry = PublogEntry {
        seq,
        session_id: session_id.to_string(),
        ts_ms: crate::session::now_ms(),
        changes: changes.to_vec(),
        digests,
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(project)?)?;
    file.write_all(serde_json::to_string(&entry)?.as_bytes())?;
    file.write_all(b"\n")?;
    fs::write(head_path(project)?, format!("{seq}\n"))?;
    Ok(seq)
}

/// Entries with seq strictly greater than `after`, oldest first. Linear scan:
/// the log is per-project and bounded by publication count, and callers hit
/// this only on commit/sync, never on the hot status path.
pub(crate) fn entries_since(project: &Path, after: u64) -> Result<Vec<PublogEntry>> {
    let raw = match fs::read_to_string(log_path(project)?) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut entries = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: PublogEntry = serde_json::from_str(line)?;
        if entry.seq > after {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// sha256 hex digest of one regular file, for publog entries.
pub(crate) fn file_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

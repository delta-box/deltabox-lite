use crate::changeset::ChangeKind;
use crate::error::{Error, Result};
use crate::state_layout::state_root;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
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

/// Parsed view of the log plus whatever a crashed appender left behind.
struct LogState {
    entries: Vec<PublogEntry>,
    /// Byte length of the newline-terminated prefix.
    clean_len: u64,
    tail: Tail,
}

/// The final, newline-less fragment of the log, if any. Only a crashed
/// appender can produce one: the entry bytes and the trailing `\n` are
/// separate writes, so the fragment is either a complete entry that lost its
/// newline or torn JSON. A malformed line *with* a newline was never written
/// by us and is reported as corruption instead.
enum Tail {
    None,
    MissingNewline(Box<PublogEntry>),
    Torn,
}

fn read_log(project: &Path) -> Result<LogState> {
    let path = log_path(project)?;
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LogState {
                entries: Vec::new(),
                clean_len: 0,
                tail: Tail::None,
            })
        }
        Err(error) => return Err(error.into()),
    };
    let mut entries = Vec::new();
    let mut offset = 0usize;
    let mut clean_len = 0usize;
    let mut tail = Tail::None;
    while offset < raw.len() {
        match raw[offset..].iter().position(|byte| *byte == b'\n') {
            Some(position) => {
                let end = offset + position;
                let text = std::str::from_utf8(&raw[offset..end])
                    .map_err(|error| corrupt(&path, &format!("entry is not UTF-8: {error}")))?;
                if !text.trim().is_empty() {
                    entries.push(
                        serde_json::from_str::<PublogEntry>(text)
                            .map_err(|error| corrupt(&path, &error.to_string()))?,
                    );
                }
                offset = end + 1;
                clean_len = offset;
            }
            None => {
                tail = match std::str::from_utf8(&raw[offset..])
                    .ok()
                    .and_then(|text| serde_json::from_str::<PublogEntry>(text).ok())
                {
                    Some(entry) => Tail::MissingNewline(Box::new(entry)),
                    None => Tail::Torn,
                };
                break;
            }
        }
    }
    Ok(LogState {
        entries,
        clean_len: clean_len as u64,
        tail,
    })
}

fn corrupt(path: &Path, detail: &str) -> Error {
    Error::Backend(format!(
        "corrupt publog entry in {}: {detail}",
        path.display()
    ))
}

fn cached_head(project: &Path) -> Result<Option<u64>> {
    match fs::read_to_string(head_path(project)?) {
        Ok(raw) => Ok(raw.trim().parse().ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn tail_seq(state: &LogState) -> u64 {
    let logged = state
        .entries
        .iter()
        .map(|entry| entry.seq)
        .max()
        .unwrap_or(0);
    match &state.tail {
        Tail::MissingNewline(entry) => logged.max(entry.seq),
        _ => logged,
    }
}

/// Latest published seq for the project; 0 when nothing was ever published.
/// `publog.head` is only an O(1) cache of the log tail: it can be missing,
/// corrupt, or one entry behind the log (a crash between the entry append and
/// the head update), so the log itself stays authoritative and the newer of
/// the two wins. A cache miss must never make published entries invisible.
pub(crate) fn head_seq(project: &Path) -> Result<u64> {
    let state = read_log(project)?;
    Ok(cached_head(project)?.unwrap_or(0).max(tail_seq(&state)))
}

/// Appends one publication; caller must hold publish.lock. Returns its seq.
pub(crate) fn append(
    project: &Path,
    session_id: &str,
    changes: &[PublogChange],
    digests: std::collections::BTreeMap<String, String>,
) -> Result<u64> {
    let state = read_log(project)?;
    let seq = cached_head(project)?.unwrap_or(0).max(tail_seq(&state)) + 1;
    let entry = PublogEntry {
        seq,
        session_id: session_id.to_string(),
        ts_ms: crate::session::now_ms(),
        changes: changes.to_vec(),
        digests,
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(log_path(project)?)?;
    // Repair a crashed appender's leftovers before extending the log, so the
    // "only the final line can lack a newline" invariant holds for readers.
    match state.tail {
        Tail::Torn => file.set_len(state.clean_len)?,
        Tail::MissingNewline(_) => {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        }
        Tail::None => {}
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(serde_json::to_string(&entry)?.as_bytes())?;
    file.write_all(b"\n")?;
    let head = head_path(project)?;
    let staged = head.with_extension("head.tmp");
    fs::write(&staged, format!("{seq}\n"))?;
    fs::rename(&staged, &head)?;
    Ok(seq)
}

/// Entries with seq strictly greater than `after`, oldest first. Linear scan:
/// the log is per-project and bounded by publication count, and callers hit
/// this only on commit/sync, never on the hot status path. A complete final
/// entry that lost its newline to a crash still counts; torn JSON does not.
pub(crate) fn entries_since(project: &Path, after: u64) -> Result<Vec<PublogEntry>> {
    let state = read_log(project)?;
    let mut entries = state.entries;
    if let Tail::MissingNewline(entry) = state.tail {
        entries.push(*entry);
    }
    entries.retain(|entry| entry.seq > after);
    Ok(entries)
}

/// Files above this size publish without a digest: hashing is ~2ms/MiB at
/// commit time, and digest endorsement only ever benefits merge-sized files.
/// An absent digest fails closed — the path simply conflicts as it would
/// have before the publication log existed.
pub(crate) const DIGEST_CAP_BYTES: u64 = 4 * 1024 * 1024;

/// Capped digest for publog entries; None when the file exceeds the cap.
pub(crate) fn file_digest_capped(path: &Path) -> Result<Option<String>> {
    if fs::metadata(path)?.len() > DIGEST_CAP_BYTES {
        return Ok(None);
    }
    file_digest(path).map(Some)
}

/// sha256 hex digest of one regular file, for publog entries.
pub(crate) fn file_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(state_root(dir.path()).unwrap()).unwrap();
        dir
    }

    fn change(path: &str) -> PublogChange {
        PublogChange {
            path: path.into(),
            kind: ChangeKind::Added,
            existed: false,
            namespace_changed: false,
        }
    }

    fn add(project: &Path, path: &str) -> u64 {
        append(project, "session-a", &[change(path)], Default::default()).unwrap()
    }

    #[test]
    fn sequences_and_cursor() {
        let dir = project();
        assert_eq!(add(dir.path(), "a.txt"), 1);
        assert_eq!(add(dir.path(), "b.txt"), 2);
        assert_eq!(add(dir.path(), "c.txt"), 3);
        assert_eq!(head_seq(dir.path()).unwrap(), 3);
        let entries = entries_since(dir.path(), 1).unwrap();
        assert_eq!(
            entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(entries_since(dir.path(), 3).unwrap().is_empty());
    }

    #[test]
    fn missing_head_recovers_from_log() {
        let dir = project();
        add(dir.path(), "a.txt");
        add(dir.path(), "b.txt");
        fs::remove_file(head_path(dir.path()).unwrap()).unwrap();
        assert_eq!(head_seq(dir.path()).unwrap(), 2);
        assert_eq!(add(dir.path(), "c.txt"), 3);
    }

    #[test]
    fn corrupt_head_recovers_from_log() {
        let dir = project();
        add(dir.path(), "a.txt");
        add(dir.path(), "b.txt");
        fs::write(head_path(dir.path()).unwrap(), "not-a-number\n").unwrap();
        assert_eq!(head_seq(dir.path()).unwrap(), 2);
        assert_eq!(add(dir.path(), "c.txt"), 3);
    }

    #[test]
    fn stale_head_never_reuses_a_seq() {
        let dir = project();
        add(dir.path(), "a.txt");
        add(dir.path(), "b.txt");
        // A crash between the entry append and the head update leaves the
        // cache one behind the log; the log must win.
        fs::write(head_path(dir.path()).unwrap(), "1\n").unwrap();
        assert_eq!(head_seq(dir.path()).unwrap(), 2);
        assert_eq!(add(dir.path(), "c.txt"), 3);
    }

    #[test]
    fn torn_tail_is_ignored_and_repaired() {
        let dir = project();
        add(dir.path(), "a.txt");
        add(dir.path(), "b.txt");
        let log = log_path(dir.path()).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"{\"seq\":9999,\"session_id\":\"tor")
            .unwrap();
        drop(file);
        assert_eq!(head_seq(dir.path()).unwrap(), 2);
        assert_eq!(entries_since(dir.path(), 0).unwrap().len(), 2);
        assert_eq!(add(dir.path(), "c.txt"), 3);
        assert!(!fs::read_to_string(&log).unwrap().contains("tor"));
        assert_eq!(
            entries_since(dir.path(), 0)
                .unwrap()
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn newline_less_complete_entry_still_counts() {
        let dir = project();
        add(dir.path(), "a.txt");
        let orphan = PublogEntry {
            seq: 2,
            session_id: "session-b".into(),
            ts_ms: 0,
            changes: vec![change("b.txt")],
            digests: Default::default(),
        };
        let log = log_path(dir.path()).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(serde_json::to_string(&orphan).unwrap().as_bytes())
            .unwrap();
        drop(file);
        assert_eq!(head_seq(dir.path()).unwrap(), 2);
        assert_eq!(entries_since(dir.path(), 0).unwrap().len(), 2);
        assert_eq!(add(dir.path(), "c.txt"), 3);
        assert_eq!(entries_since(dir.path(), 0).unwrap().len(), 3);
    }

    #[test]
    fn interior_corruption_is_an_error() {
        let dir = project();
        add(dir.path(), "a.txt");
        let log = log_path(dir.path()).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"garbage\n").unwrap();
        drop(file);
        assert!(entries_since(dir.path(), 0).is_err());
        assert!(head_seq(dir.path()).is_err());
    }
}

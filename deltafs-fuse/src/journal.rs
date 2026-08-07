use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("undo directory must be outside the source tree")]
    UndoInsideSource,
    #[error("mutation path escapes the source tree: {0}")]
    PathEscape(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, JournalError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreimageKind {
    Absent,
    Regular,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreimageRef {
    pub capture_id: u64,
    pub path: String,
    pub kind: PreimageKind,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum JournalEvent {
    Ready {
        operation_id: u64,
        operation: String,
        preimages: Vec<PreimageRef>,
    },
    Applied {
        operation_id: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationTicket {
    operation_id: u64,
}

#[derive(Debug)]
struct Inner {
    next_operation_id: u64,
    next_capture_id: u64,
    journal: File,
    captured: HashMap<String, PreimageRef>,
}

#[derive(Debug)]
pub struct UndoStore {
    source: PathBuf,
    root: PathBuf,
    inner: Mutex<Inner>,
}

impl UndoStore {
    pub fn open(source: &Path, root: &Path) -> Result<Self> {
        let source = source.canonicalize()?;
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        if root.starts_with(&source) {
            return Err(JournalError::UndoInsideSource);
        }
        fs::create_dir_all(root.join("preimages"))?;
        let journal_path = root.join("journal.jsonl");
        let (next_operation_id, next_capture_id, captured) = recover(&journal_path)?;
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(journal_path)?;
        Ok(Self {
            source,
            root,
            inner: Mutex::new(Inner {
                next_operation_id,
                next_capture_id,
                journal,
                captured,
            }),
        })
    }

    pub fn begin(&self, operation: &str, paths: &[PathBuf]) -> Result<MutationTicket> {
        let mut inner = self.inner.lock().expect("undo journal mutex poisoned");
        let operation_id = inner.next_operation_id;
        inner.next_operation_id += 1;

        let mut preimages = Vec::with_capacity(paths.len());
        for path in paths {
            let relative = path
                .strip_prefix(&self.source)
                .map_err(|_| JournalError::PathEscape(path.clone()))?;
            let key = relative.to_string_lossy().into_owned();
            let preimage = if let Some(existing) = inner.captured.get(&key) {
                existing.clone()
            } else {
                let capture_id = inner.next_capture_id;
                inner.next_capture_id += 1;
                let captured = self.capture(capture_id, path, &key)?;
                inner.captured.insert(key, captured.clone());
                captured
            };
            preimages.push(preimage);
        }

        append_event(
            &mut inner.journal,
            &JournalEvent::Ready {
                operation_id,
                operation: operation.to_owned(),
                preimages,
            },
        )?;
        inner.journal.flush()?;
        Ok(MutationTicket { operation_id })
    }

    pub fn applied(&self, ticket: MutationTicket) -> Result<()> {
        let mut inner = self.inner.lock().expect("undo journal mutex poisoned");
        append_event(
            &mut inner.journal,
            &JournalEvent::Applied {
                operation_id: ticket.operation_id,
            },
        )?;
        inner.journal.flush()?;
        Ok(())
    }

    pub fn journal_path(&self) -> PathBuf {
        self.root.join("journal.jsonl")
    }

    fn capture(&self, capture_id: u64, path: &Path, key: &str) -> Result<PreimageRef> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PreimageRef {
                    capture_id,
                    path: key.to_owned(),
                    kind: PreimageKind::Absent,
                    size: 0,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    data: None,
                });
            }
            Err(error) => return Err(error.into()),
        };

        let file_type = metadata.file_type();
        let (kind, data) = if file_type.is_file() {
            let relative = format!("preimages/{capture_id:016x}.data");
            let destination = self.root.join(&relative);
            let temporary = self.root.join(format!("preimages/.{capture_id:016x}.tmp"));
            fs::copy(path, &temporary)?;
            fs::rename(temporary, destination)?;
            (PreimageKind::Regular, Some(relative))
        } else if file_type.is_dir() {
            (PreimageKind::Directory, None)
        } else if file_type.is_symlink() {
            let relative = format!("preimages/{capture_id:016x}.symlink");
            let target = fs::read_link(path)?;
            fs::write(self.root.join(&relative), target.as_os_str().as_bytes())?;
            (PreimageKind::Symlink, Some(relative))
        } else {
            (PreimageKind::Other, None)
        };

        Ok(PreimageRef {
            capture_id,
            path: key.to_owned(),
            kind,
            size: metadata.len(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            data,
        })
    }
}

fn recover(path: &Path) -> Result<(u64, u64, HashMap<String, PreimageRef>)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((1, 1, HashMap::new()));
        }
        Err(error) => return Err(error.into()),
    };
    if bytes.is_empty() {
        return Ok((1, 1, HashMap::new()));
    }

    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if complete_len != bytes.len() {
        OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(complete_len as u64)?;
    }

    let mut max_operation_id = 0;
    let mut max_capture_id = 0;
    let mut captured = HashMap::new();
    for line in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: JournalEvent = serde_json::from_slice(line)?;
        match event {
            JournalEvent::Ready {
                operation_id,
                preimages,
                ..
            } => {
                max_operation_id = max_operation_id.max(operation_id);
                for preimage in preimages {
                    max_capture_id = max_capture_id.max(preimage.capture_id);
                    captured.entry(preimage.path.clone()).or_insert(preimage);
                }
            }
            JournalEvent::Applied { operation_id } => {
                max_operation_id = max_operation_id.max(operation_id);
            }
        }
    }
    Ok((max_operation_id + 1, max_capture_id + 1, captured))
}

fn append_event(journal: &mut File, event: &JournalEvent) -> Result<()> {
    serde_json::to_writer(&mut *journal, event)?;
    journal.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn read_events(path: &Path) -> Vec<JournalEvent> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn ready_and_preimage_exist_before_mutation() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let target = source.join("file.txt");
        fs::write(&target, b"old").unwrap();
        let store = UndoStore::open(&source, &undo).unwrap();

        let ticket = store.begin("write", std::slice::from_ref(&target)).unwrap();
        let events = read_events(&store.journal_path());
        let JournalEvent::Ready { preimages, .. } = &events[0] else {
            panic!("first event must be ready");
        };
        assert_eq!(
            fs::read(undo.join(preimages[0].data.as_ref().unwrap())).unwrap(),
            b"old"
        );
        assert_eq!(fs::read(&target).unwrap(), b"old");

        fs::write(&target, b"new").unwrap();
        store.applied(ticket).unwrap();
        assert!(matches!(
            read_events(&store.journal_path())[1],
            JournalEvent::Applied { .. }
        ));
    }

    #[test]
    fn repeated_mutation_reuses_epoch_preimage() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let target = source.join("file.txt");
        fs::write(&target, b"v1").unwrap();
        let store = UndoStore::open(&source, &undo).unwrap();

        let first = store.begin("write", std::slice::from_ref(&target)).unwrap();
        fs::write(&target, b"v2").unwrap();
        store.applied(first).unwrap();
        let second = store.begin("write", std::slice::from_ref(&target)).unwrap();
        fs::write(&target, b"v3").unwrap();
        store.applied(second).unwrap();

        let events = read_events(&store.journal_path());
        let ids: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                JournalEvent::Ready { preimages, .. } => Some(preimages[0].capture_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![1, 1]);
        assert_eq!(
            fs::read(undo.join("preimages/0000000000000001.data")).unwrap(),
            b"v1"
        );
    }

    #[test]
    fn absent_preimage_is_recorded_for_create() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let store = UndoStore::open(&source, &undo).unwrap();
        let target = source.join("new.txt");

        store.begin("create", &[target]).unwrap();
        let events = read_events(&store.journal_path());
        let JournalEvent::Ready { preimages, .. } = &events[0] else {
            panic!("first event must be ready");
        };
        assert_eq!(preimages[0].kind, PreimageKind::Absent);
    }

    #[test]
    fn multi_path_mutation_keeps_distinct_preimages() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let first = source.join("first.txt");
        let second = source.join("second.txt");
        fs::write(&first, b"first preimage").unwrap();
        fs::write(&second, b"second preimage").unwrap();
        let store = UndoStore::open(&source, &undo).unwrap();

        store.begin("rename", &[first, second]).unwrap();
        let events = read_events(&store.journal_path());
        let JournalEvent::Ready { preimages, .. } = &events[0] else {
            panic!("first event must be ready");
        };
        assert_ne!(preimages[0].capture_id, preimages[1].capture_id);
        assert_eq!(
            fs::read(undo.join(preimages[0].data.as_ref().unwrap())).unwrap(),
            b"first preimage"
        );
        assert_eq!(
            fs::read(undo.join(preimages[1].data.as_ref().unwrap())).unwrap(),
            b"second preimage"
        );
    }

    #[test]
    fn reopen_recovers_ids_and_epoch_preimages() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let target = source.join("file.txt");
        fs::write(&target, b"v1").unwrap();

        let store = UndoStore::open(&source, &undo).unwrap();
        let first = store.begin("write", std::slice::from_ref(&target)).unwrap();
        fs::write(&target, b"v2").unwrap();
        store.applied(first).unwrap();
        drop(store);

        let store = UndoStore::open(&source, &undo).unwrap();
        let second = store.begin("write", std::slice::from_ref(&target)).unwrap();
        fs::write(&target, b"v3").unwrap();
        store.applied(second).unwrap();

        let events = read_events(&store.journal_path());
        let ready: Vec<(u64, u64)> = events
            .iter()
            .filter_map(|event| match event {
                JournalEvent::Ready {
                    operation_id,
                    preimages,
                    ..
                } => Some((*operation_id, preimages[0].capture_id)),
                _ => None,
            })
            .collect();
        assert_eq!(ready, vec![(1, 1), (2, 1)]);
        assert_eq!(
            fs::read(undo.join("preimages/0000000000000001.data")).unwrap(),
            b"v1"
        );
    }

    #[test]
    fn reopen_discards_incomplete_journal_tail() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let first_target = source.join("first.txt");
        let second_target = source.join("second.txt");
        fs::write(&first_target, b"first").unwrap();
        fs::write(&second_target, b"second").unwrap();

        let store = UndoStore::open(&source, &undo).unwrap();
        let first = store
            .begin("write", std::slice::from_ref(&first_target))
            .unwrap();
        store.applied(first).unwrap();
        let journal_path = store.journal_path();
        drop(store);
        OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .unwrap()
            .write_all(br#"{"phase":"ready""#)
            .unwrap();

        let store = UndoStore::open(&source, &undo).unwrap();
        let second = store
            .begin("write", std::slice::from_ref(&second_target))
            .unwrap();
        store.applied(second).unwrap();

        let events = read_events(&journal_path);
        let operation_ids: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                JournalEvent::Ready { operation_id, .. } => Some(*operation_id),
                JournalEvent::Applied { .. } => None,
            })
            .collect();
        assert_eq!(operation_ids, vec![1, 2]);
    }

    #[test]
    fn undo_directory_inside_source_is_rejected() {
        let source = tempdir().unwrap();
        let error = UndoStore::open(source.path(), &source.path().join("undo")).unwrap_err();
        assert!(matches!(error, JournalError::UndoInsideSource));
    }
}

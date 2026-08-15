use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const TAIL_READ_BLOCK_SIZE: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalKind {
    SessionStart,
    SessionStop,
    Exec,
    Write,
    Checkpoint,
    Commit,
    Abort,
    Diff,
    Error,
    ReviewBegin,
    ReviewSkip,
    Recovery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub ts_ms: u64,
    pub session_id: String,
    pub kind: JournalKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Append-only JSONL journal. `flush_sync()` provides durability for
/// checkpoint / commit / abort boundaries.
pub struct Journal {
    path: PathBuf,
    file: File,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    pub fn append(&mut self, entry: JournalEntry) -> Result<()> {
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn flush_sync(&mut self) -> Result<()> {
        // fdatasync: flush journal data + size without a metadata fsync,
        // which is the durability level an append-only log needs.
        self.file.sync_data()?;
        if let Some(parent) = self.path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    pub fn tail(&self, n: usize) -> Result<Vec<JournalEntry>> {
        if n == 0 {
            return Ok(Vec::new());
        }

        let mut file = File::open(&self.path)?;
        let mut cursor = file.metadata()?.len();
        let mut blocks = Vec::new();
        let mut separators = 0;
        while cursor > 0 {
            let block_len = cursor.min(TAIL_READ_BLOCK_SIZE as u64) as usize;
            cursor -= block_len as u64;
            file.seek(SeekFrom::Start(cursor))?;
            let mut block = vec![0; block_len];
            file.read_exact(&mut block)?;
            separators += block.iter().filter(|byte| **byte == b'\n').count();
            blocks.push(block);
            // One extra separator lets us discard a partial/extra line at the
            // front while retaining the requested number of complete entries.
            if separators > n {
                break;
            }
        }

        let mut bytes = Vec::with_capacity(blocks.iter().map(Vec::len).sum());
        for block in blocks.iter().rev() {
            bytes.extend_from_slice(block);
        }
        let start = if cursor == 0 {
            0
        } else {
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |position| position + 1)
        };
        let mut entries = BufReader::new(&bytes[start..])
            .lines()
            .map(|line| Ok(serde_json::from_str::<JournalEntry>(&line?)?))
            .collect::<Result<Vec<_>>>()?;
        if entries.len() > n {
            entries.drain(..entries.len() - n);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_appends_and_tails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        for i in 0..3 {
            journal
                .append(JournalEntry {
                    ts_ms: i,
                    session_id: "s".into(),
                    kind: JournalKind::Exec,
                    data: None,
                })
                .unwrap();
        }
        let tail = journal.tail(2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].ts_ms, 1);
        assert_eq!(tail[1].ts_ms, 2);

        let reopened = Journal::open(&path).unwrap();
        let tail = reopened.tail(3).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].ts_ms, 0);
    }

    #[test]
    fn journal_tail_reads_only_the_requested_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut bytes = vec![0xff; TAIL_READ_BLOCK_SIZE * 2];
        bytes.push(b'\n');
        for ts_ms in 0..3 {
            bytes.extend_from_slice(
                &serde_json::to_vec(&JournalEntry {
                    ts_ms,
                    session_id: "s".into(),
                    kind: JournalKind::Exec,
                    data: None,
                })
                .unwrap(),
            );
            bytes.push(b'\n');
        }
        std::fs::write(&path, bytes).unwrap();

        let journal = Journal::open(&path).unwrap();
        let tail = journal.tail(2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].ts_ms, 1);
        assert_eq!(tail[1].ts_ms, 2);
    }
}

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    count: u64,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let count = {
            let f = File::open(path)?;
            BufReader::new(f).lines().count() as u64
        };
        Ok(Self {
            path: path.to_path_buf(),
            file,
            count,
        })
    }

    pub fn append(&mut self, entry: JournalEntry) -> Result<()> {
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        self.count += 1;
        Ok(())
    }

    pub fn flush_sync(&mut self) -> Result<()> {
        // fdatasync: flush journal data + size without a metadata fsync,
        // which is the durability level an append-only log needs.
        self.file.sync_data()?;
        Ok(())
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn tail(&self, n: usize) -> Result<Vec<JournalEntry>> {
        let f = File::open(&self.path)?;
        let lines: Vec<String> = BufReader::new(f).lines().collect::<std::io::Result<_>>()?;
        let start = lines.len().saturating_sub(n);
        let mut out = Vec::new();
        for l in &lines[start..] {
            out.push(serde_json::from_str::<JournalEntry>(l)?);
        }
        Ok(out)
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
        assert_eq!(journal.count(), 3);
        let tail = journal.tail(2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].ts_ms, 1);
        assert_eq!(tail[1].ts_ms, 2);

        let reopened = Journal::open(&path).unwrap();
        assert_eq!(reopened.count(), 3);
    }
}

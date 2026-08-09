use crc32fast::hash;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use super::model::{
    ApplyOutcome, JournalBody, JournalEnvelope, JournalEvent, ReadyRecord, FORMAT_VERSION,
};
use super::{EngineError, Result};

#[derive(Debug, Clone)]
pub struct LoadedRecord {
    pub record_id: u64,
    pub ready: ReadyRecord,
    pub outcome: Option<ApplyOutcome>,
}

#[derive(Debug, Clone, Default)]
pub struct LoadedJournal {
    pub records: Vec<LoadedRecord>,
    pub next_sequence: u64,
}

pub fn append(path: &Path, body: JournalBody, sync: bool) -> Result<()> {
    let envelope = envelope(body)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &envelope)?;
    file.write_all(b"\n")?;
    file.flush()?;
    if sync {
        file.sync_data()?;
    }
    Ok(())
}

pub fn encoded_len(body: JournalBody) -> Result<u64> {
    let bytes = serde_json::to_vec(&envelope(body)?)?;
    Ok(bytes.len() as u64 + 1)
}

fn envelope(body: JournalBody) -> Result<JournalEnvelope> {
    let payload = serde_json::to_vec(&body)?;
    Ok(JournalEnvelope {
        format_version: FORMAT_VERSION,
        payload_length: payload.len() as u64,
        checksum: hash(&payload),
        payload: body,
    })
}

pub fn load(path: &Path, expected_epoch: u64) -> Result<LoadedJournal> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedJournal {
                records: Vec::new(),
                next_sequence: 1,
            });
        }
        Err(error) => return Err(error.into()),
    };
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(EngineError::CorruptJournal(format!(
            "torn final record in {}",
            path.display()
        )));
    }

    let mut sequence = 0;
    let mut records: BTreeMap<u64, LoadedRecord> = BTreeMap::new();
    for (line_index, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let envelope: JournalEnvelope = serde_json::from_slice(line).map_err(|error| {
            EngineError::CorruptJournal(format!(
                "{} line {}: {error}",
                path.display(),
                line_index + 1
            ))
        })?;
        if envelope.format_version != FORMAT_VERSION {
            return Err(EngineError::UnsupportedFormat {
                found: envelope.format_version,
                supported: FORMAT_VERSION,
            });
        }
        let payload = serde_json::to_vec(&envelope.payload)?;
        if envelope.payload_length != payload.len() as u64 || envelope.checksum != hash(&payload) {
            return Err(EngineError::CorruptJournal(format!(
                "checksum or length mismatch in {} line {}",
                path.display(),
                line_index + 1
            )));
        }
        if envelope.payload.epoch != expected_epoch {
            return Err(EngineError::CorruptJournal(format!(
                "epoch mismatch in {} line {}",
                path.display(),
                line_index + 1
            )));
        }
        if envelope.payload.sequence != sequence + 1 {
            return Err(EngineError::CorruptJournal(format!(
                "non-monotonic sequence in {} line {}",
                path.display(),
                line_index + 1
            )));
        }
        sequence = envelope.payload.sequence;
        match envelope.payload.event {
            JournalEvent::Ready { record } => {
                if records
                    .insert(
                        envelope.payload.record_id,
                        LoadedRecord {
                            record_id: envelope.payload.record_id,
                            ready: record,
                            outcome: None,
                        },
                    )
                    .is_some()
                {
                    return Err(EngineError::CorruptJournal(format!(
                        "duplicate READY for record {}",
                        envelope.payload.record_id
                    )));
                }
            }
            JournalEvent::Applied { outcome } => {
                let record = records
                    .get_mut(&envelope.payload.record_id)
                    .ok_or_else(|| {
                        EngineError::CorruptJournal(format!(
                            "APPLIED without READY for record {}",
                            envelope.payload.record_id
                        ))
                    })?;
                if record.outcome.replace(outcome).is_some() {
                    return Err(EngineError::CorruptJournal(format!(
                        "duplicate APPLIED for record {}",
                        envelope.payload.record_id
                    )));
                }
            }
        }
    }
    Ok(LoadedJournal {
        records: records.into_values().collect(),
        next_sequence: sequence + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::{OperationKind, ReadyRecord};
    use std::collections::BTreeSet;
    use tempfile::tempdir;

    fn ready() -> ReadyRecord {
        ReadyRecord {
            operation: OperationKind::Create,
            actions: Vec::new(),
            touched: BTreeSet::from(["a".into()]),
            preimage_bytes: 0,
        }
    }

    #[test]
    fn checksum_and_sequence_are_enforced() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        append(
            &path,
            JournalBody {
                epoch: 4,
                sequence: 1,
                record_id: 7,
                event: JournalEvent::Ready { record: ready() },
            },
            false,
        )
        .unwrap();
        append(
            &path,
            JournalBody {
                epoch: 4,
                sequence: 2,
                record_id: 7,
                event: JournalEvent::Applied {
                    outcome: ApplyOutcome::Applied,
                },
            },
            false,
        )
        .unwrap();
        let loaded = load(&path, 4).unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.next_sequence, 3);

        let mut bytes = fs::read(&path).unwrap();
        let position = bytes.iter().position(|byte| *byte == b'a').unwrap();
        bytes[position] = b'b';
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            load(&path, 4),
            Err(EngineError::CorruptJournal(_))
        ));
    }

    #[test]
    fn torn_tail_is_corruption_not_silently_discarded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.log");
        fs::write(&path, b"{\"partial\":true}").unwrap();
        assert!(matches!(
            load(&path, 1),
            Err(EngineError::CorruptJournal(_))
        ));
    }
}

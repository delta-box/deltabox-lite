use crc32fast::hash;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use super::model::FORMAT_VERSION;
use super::{EngineError, Result};

#[derive(Serialize, Deserialize)]
struct Stored<T> {
    format_version: u32,
    payload_length: u64,
    checksum: u32,
    payload: T,
}

pub fn write_state<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    let stored = Stored {
        format_version: FORMAT_VERSION,
        payload_length: payload.len() as u64,
        checksum: hash(&payload),
        payload: value,
    };
    let bytes = serde_json::to_vec_pretty(&stored)?;
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::Invariant("state path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.flush()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

pub fn read_state<T: DeserializeOwned + Serialize>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    let stored: Stored<T> = serde_json::from_slice(&bytes)
        .map_err(|error| EngineError::CorruptState(format!("{}: {error}", path.display())))?;
    if stored.format_version != FORMAT_VERSION {
        return Err(EngineError::UnsupportedFormat {
            found: stored.format_version,
            supported: FORMAT_VERSION,
        });
    }
    let payload = serde_json::to_vec(&stored.payload)?;
    if stored.payload_length != payload.len() as u64 || stored.checksum != hash(&payload) {
        return Err(EngineError::CorruptState(format!(
            "checksum or length mismatch in {}",
            path.display()
        )));
    }
    Ok(stored.payload)
}

pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

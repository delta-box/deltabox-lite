use crc32fast::hash;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use super::model::{FORMAT_VERSION, LEGACY_FORMAT_VERSION};
use super::{EngineError, Result};

#[derive(Serialize, Deserialize)]
struct Stored<T> {
    format_version: u32,
    #[serde(default)]
    kind: Option<String>,
    payload_length: u64,
    checksum: u32,
    payload: T,
}

pub fn write_state<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_state_with(path, value, false)
}

pub fn write_state_durable<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_state_with(path, value, true)
}

fn write_state_with<T: Serialize>(path: &Path, value: &T, durable: bool) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    let stored = Stored {
        format_version: FORMAT_VERSION,
        kind: Some(state_kind(path)?.into()),
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
    if durable {
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    if durable {
        sync_dir(parent)?;
    }
    Ok(())
}

pub fn sync_file(path: &Path) -> io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_all()
}

pub fn sync_data(path: &Path) -> io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_data()
}

pub fn sync_dir(path: &Path) -> io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_all()
}

pub fn remove_file_durable(path: &Path) -> io::Result<()> {
    let parent = path.parent();
    remove_if_exists(path)?;
    if let Some(parent) = parent {
        sync_dir(parent)?;
    }
    Ok(())
}

pub fn read_state<T: DeserializeOwned + Serialize>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    let stored: Stored<T> = serde_json::from_slice(&bytes)
        .map_err(|error| EngineError::CorruptState(format!("{}: {error}", path.display())))?;
    validate_version(stored.format_version)?;
    validate_kind(path, stored.format_version, stored.kind.as_deref())?;
    let payload = serde_json::to_vec(&stored.payload)?;
    if stored.payload_length != payload.len() as u64 || stored.checksum != hash(&payload) {
        return Err(EngineError::CorruptState(format!(
            "checksum or length mismatch in {}",
            path.display()
        )));
    }
    Ok(stored.payload)
}

pub fn upgrade_file(path: &Path) -> Result<bool> {
    let bytes = fs::read(path)?;
    let mut stored: Stored<Value> = serde_json::from_slice(&bytes)
        .map_err(|error| EngineError::CorruptState(format!("{}: {error}", path.display())))?;
    validate_version(stored.format_version)?;
    validate_kind(path, stored.format_version, stored.kind.as_deref())?;
    let payload = serde_json::to_vec(&stored.payload)?;
    if stored.payload_length != payload.len() as u64 || stored.checksum != hash(&payload) {
        return Err(EngineError::CorruptState(format!(
            "checksum or length mismatch in {}",
            path.display()
        )));
    }
    if stored.format_version == FORMAT_VERSION {
        return Ok(false);
    }
    stored.format_version = FORMAT_VERSION;
    stored.kind = Some(state_kind(path)?.into());
    write_envelope(path, &stored)?;
    Ok(true)
}

fn write_envelope<T: Serialize>(path: &Path, stored: &Stored<T>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(stored)?;
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::Invariant("state path has no parent".into()))?;
    let temporary = path.with_extension("upgrade.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_dir(parent)?;
    Ok(())
}

fn validate_version(found: u32) -> Result<()> {
    if found != LEGACY_FORMAT_VERSION && found != FORMAT_VERSION {
        return Err(EngineError::UnsupportedFormat {
            found,
            supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

fn validate_kind(path: &Path, version: u32, found: Option<&str>) -> Result<()> {
    if version == FORMAT_VERSION {
        let expected = state_kind(path)?;
        if found != Some(expected) {
            return Err(EngineError::CorruptState(format!(
                "state kind mismatch in {}: expected {expected}, found {}",
                path.display(),
                found.unwrap_or("missing")
            )));
        }
    }
    Ok(())
}

fn state_kind(path: &Path) -> Result<&'static str> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("session.json") => Ok("session"),
        Some("inodes.idx") => Ok("inodes"),
        Some("touched.idx") => Ok("touched"),
        Some("epoch.json") => Ok("epoch"),
        Some("active.json") => Ok("recovery"),
        Some(name)
            if name.ends_with(".marker")
                && path
                    .parent()
                    .is_some_and(|parent| parent.ends_with("commits")) =>
        {
            Ok("commit")
        }
        _ => Err(EngineError::Invariant(format!(
            "unknown state file {}",
            path.display()
        ))),
    }
}

pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

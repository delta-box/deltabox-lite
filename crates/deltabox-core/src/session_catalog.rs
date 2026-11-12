//! Read-only discovery of a project's authoritative session records.

use crate::protocol::{FilesystemMode, SessionStatus};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Persisted session identity and configuration, without runtime credentials
/// or expensive working-tree statistics. `status` is the last stored status;
/// listing does not check whether a helper process is alive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub project: PathBuf,
    pub started_at_ms: u64,
    pub status: SessionStatus,
    pub resolved_fs_mode: FilesystemMode,
    /// Same guarantee string as `SessionInfo`: `process-crash` or `strict`.
    pub durability: String,
}

pub(crate) fn read_sessions(root: &Path, project: &Path) -> Result<Vec<SessionSummary>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        // Session creation always uses lowercase hyphenated UUIDs. Ignore
        // registry files, engine directories, and incomplete temporary names.
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            continue;
        };
        if uuid.to_string() != id {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || is_link(&metadata) {
            continue;
        }
        let path = entry.path().join("session.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Startup may not have published its record, or concurrent
            // cleanup may have removed it after read_dir returned the entry.
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if is_link(&metadata) {
            continue;
        }
        if !metadata.is_file() {
            return Err(invalid_record(&path, "expected a regular file"));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let summary =
            parse_record(&bytes).map_err(|error| invalid_record(&path, &error.to_string()))?;
        if summary.session_id != id {
            return Err(invalid_record(
                &path,
                "session ID does not match its directory",
            ));
        }
        if summary.project != project {
            return Err(invalid_record(
                &path,
                "project does not match this coordinator",
            ));
        }
        sessions.push(summary);
    }
    sessions.sort_by(|left, right| {
        left.started_at_ms
            .cmp(&right.started_at_ms)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    Ok(sessions)
}

fn invalid_record(path: &Path, message: &str) -> Error {
    Error::Backend(format!(
        "invalid session record {}: {message}",
        path.display()
    ))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject junctions and other reparse points as well as symlinks.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(not(windows))]
fn parse_record(bytes: &[u8]) -> std::result::Result<SessionSummary, serde_json::Error> {
    let meta: crate::session::SessionMeta = serde_json::from_slice(bytes)?;
    Ok(SessionSummary {
        session_id: meta.session_id,
        project: meta.project,
        started_at_ms: meta.started_at_ms,
        status: meta.status,
        resolved_fs_mode: meta.filesystem.resolved_fs_mode,
        durability: meta.filesystem.durability,
    })
}

#[cfg(windows)]
fn parse_record(bytes: &[u8]) -> std::result::Result<SessionSummary, serde_json::Error> {
    let meta: crate::windows::engine::Manifest = serde_json::from_slice(bytes)?;
    Ok(SessionSummary {
        session_id: meta.session_id,
        project: meta.source,
        started_at_ms: meta.started_at_ms,
        status: meta.status,
        resolved_fs_mode: meta.fs_mode,
        durability: match meta.durability {
            crate::protocol::DurabilityMode::Fast => "process-crash",
            crate::protocol::DurabilityMode::Strict => "strict",
        }
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIRST: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND: &str = "22222222-2222-4222-8222-222222222222";
    const THIRD: &str = "33333333-3333-4333-8333-333333333333";

    fn record(project: &Path, id: &str, started: u64) -> serde_json::Value {
        #[cfg(not(windows))]
        let value = json!({
            "session_id": id, "project": project, "started_at_ms": started,
            "status": "running", "filesystem": crate::protocol::FilesystemInfo::default(),
            "token": "must-not-be-returned",
        });
        #[cfg(windows)]
        let value = json!({
            "version": 1, "session_id": id, "source": project, "started_at_ms": started,
            "status": "running", "fs_mode": "cbcow-through", "durability": "strict",
            "view": project, "scratch": project, "token": "must-not-be-returned",
            "policy": crate::policy::Policy::default_agent(), "next_id": 1,
            "accepted": "accepted", "checkpoints": [], "review": null, "rollback": null,
        });
        value
    }

    fn write_record(root: &Path, id: &str, value: &serde_json::Value) -> PathBuf {
        let directory = root.join(id);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.json");
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn reads_records_without_registry_and_sorts_by_timestamp_then_id() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        write_record(root.path(), THIRD, &record(&project, THIRD, 20));
        write_record(root.path(), SECOND, &record(&project, SECOND, 10));
        write_record(root.path(), FIRST, &record(&project, FIRST, 10));
        // Startup debris and unrelated state must not be interpreted as sessions.
        fs::create_dir(root.path().join(uuid::Uuid::new_v4().to_string())).unwrap();
        fs::create_dir(root.path().join("macos-native")).unwrap();
        write_record(
            root.path(),
            &FIRST.replace('-', ""),
            &record(&project, FIRST, 1),
        );
        let sessions = read_sessions(root.path(), &project).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![FIRST, SECOND, THIRD]
        );
        // Even an unreadable global registry must have no influence on discovery.
        fs::write(root.path().join("sessions.json"), b"broken registry").unwrap();
        assert_eq!(read_sessions(root.path(), &project).unwrap(), sessions);
        let serialized = serde_json::to_string(&sessions).unwrap();
        assert!(!serialized.contains("must-not-be-returned"));
        assert!(!serialized.contains("token"));
    }

    #[test]
    fn leaves_records_and_missing_roots_untouched() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let path = write_record(root.path(), FIRST, &record(&project, FIRST, 1));
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        read_sessions(root.path(), &project).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        let missing = root.path().join("missing");
        assert!(read_sessions(&missing, &project).unwrap().is_empty());
        assert!(!missing.exists());
    }

    #[test]
    fn rejects_malformed_metadata_and_identity_mismatches_with_paths() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let path = write_record(root.path(), FIRST, &record(&project, SECOND, 1));
        let error = read_sessions(root.path(), &project)
            .unwrap_err()
            .to_string();
        assert!(error.contains("session ID"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
        write_record(
            root.path(),
            FIRST,
            &record(&root.path().join("foreign"), FIRST, 1),
        );
        let error = read_sessions(root.path(), &project)
            .unwrap_err()
            .to_string();
        assert!(error.contains("project does not match"), "{error}");
        fs::write(&path, b"{ torn JSON").unwrap();
        let error = read_sessions(root.path(), &project)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid session record"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlink_directories_and_metadata() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let path = write_record(outside.path(), FIRST, &record(&project, FIRST, 1));
        symlink(path.parent().unwrap(), root.path().join(FIRST)).unwrap();
        fs::create_dir(root.path().join(SECOND)).unwrap();
        symlink(&path, root.path().join(SECOND).join("session.json")).unwrap();
        assert!(read_sessions(root.path(), &project).unwrap().is_empty());
    }
}

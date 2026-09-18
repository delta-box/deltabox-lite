use crate::error::{Error, Result};
use crate::protocol::FilesystemMode;
use std::fs;
use std::path::Path;

/// Mode-aware project admission.
///
/// cbcow-through writes the host tree directly and stays exclusive: it admits
/// only when no other lease of either kind is live. cbcow-private works in a
/// private tree and takes one entry under `shared/`: any number may coexist,
/// blocked only by a live exclusive holder.
///
/// Liveness is judged by the holder's durable session record (`is_live`),
/// never by process liveness: lock holders can be killed at any time, and the
/// durable status is what session recovery itself trusts. Dead entries are
/// reclaimed on the spot. The caller must hold the project admission lock
/// (cbcow.lock) around acquire, so reclaim/check/grant is atomic.
pub(crate) fn acquire(
    leases: &Path,
    legacy_exclusive: &Path,
    mode: FilesystemMode,
    session_id: &str,
    is_live: impl Fn(&str) -> bool,
) -> Result<()> {
    let shared = leases.join("shared");
    fs::create_dir_all(&shared)?;

    let mut live_exclusive = None;
    for candidate in [leases.join("exclusive"), legacy_exclusive.to_path_buf()] {
        if let Ok(holder) = fs::read_to_string(&candidate) {
            let holder = holder.trim().to_string();
            if is_live(&holder) {
                live_exclusive = Some(holder);
            } else {
                let _ = fs::remove_file(&candidate);
            }
        }
    }

    let mut live_shared = Vec::new();
    for entry in fs::read_dir(&shared)? {
        let path = entry?.path();
        let Ok(holder) = fs::read_to_string(&path) else {
            continue;
        };
        let holder = holder.trim().to_string();
        if is_live(&holder) {
            live_shared.push(holder);
        } else {
            let _ = fs::remove_file(&path);
        }
    }

    if let Some(holder) = live_exclusive {
        return Err(Error::SessionBusy(holder));
    }
    match mode {
        FilesystemMode::CbcowThrough => {
            if let Some(holder) = live_shared.first() {
                return Err(Error::SessionBusy(holder.clone()));
            }
            fs::write(leases.join("exclusive"), format!("{session_id}\n"))?;
        }
        FilesystemMode::CbcowPrivate => {
            fs::write(shared.join(session_id), format!("{session_id}\n"))?;
        }
        FilesystemMode::CopyPrivate | FilesystemMode::Git => {}
    }
    Ok(())
}

/// Releases whatever lease `session_id` holds. Tolerates absence: a crashed
/// session's lease is reclaimed by the next `acquire` instead.
pub(crate) fn release(leases: &Path, legacy_exclusive: &Path, session_id: &str) {
    for candidate in [leases.join("exclusive"), legacy_exclusive.to_path_buf()] {
        if fs::read_to_string(&candidate).is_ok_and(|holder| holder.trim() == session_id) {
            let _ = fs::remove_file(candidate);
        }
    }
    let _ = fs::remove_file(leases.join("shared").join(session_id));
}

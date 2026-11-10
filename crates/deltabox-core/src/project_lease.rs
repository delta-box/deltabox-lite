use crate::error::{Error, Result};
use crate::protocol::FilesystemMode;
use std::fs;
use std::path::Path;

/// What admission concludes about one lease holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    /// Stopped or gone: the lease entry is reclaimed on the spot.
    Dead,
    /// Running, and either detached or owned by a live process.
    Live,
    /// Running, but its attached owner process died. The entry stays: the
    /// session still has pending changes that a new owner adopts and then
    /// commits or aborts. Admission reports it instead of reclaiming it.
    Orphaned,
}

/// Judges a holder from its durable record plus its owner lock. A running
/// session is live unless it was attached and its owner lock is now free.
pub(crate) fn judge(running: bool, store: &Path) -> Holder {
    if !running {
        return Holder::Dead;
    }
    match crate::owner::probe(store) {
        crate::owner::Ownership::Orphaned { .. } => Holder::Orphaned,
        _ => Holder::Live,
    }
}

/// Mode-aware project admission.
///
/// cbcow-through writes the host tree directly and stays exclusive: it admits
/// only when no other lease of either kind is held. cbcow-private works in a
/// private tree and takes one entry under `shared/`: any number may coexist,
/// blocked only by a held exclusive lease.
///
/// Liveness is judged by `judge`: the holder's durable session record, plus,
/// for attached sessions, its owner lock — the durable status alone cannot
/// see an owner that was killed before it could record a stop. Dead entries
/// are reclaimed on the spot; orphaned ones are reported as
/// `SessionOrphaned`. The caller must hold the project admission lock
/// (cbcow.lock) around acquire, so reclaim/check/grant is atomic.
pub(crate) fn acquire(
    leases: &Path,
    legacy_exclusive: &Path,
    mode: FilesystemMode,
    session_id: &str,
    holder: impl Fn(&str) -> Holder,
) -> Result<()> {
    let shared = leases.join("shared");
    fs::create_dir_all(&shared)?;

    let mut exclusive = None;
    for candidate in [leases.join("exclusive"), legacy_exclusive.to_path_buf()] {
        if let Ok(id) = fs::read_to_string(&candidate) {
            let id = id.trim().to_string();
            match holder(&id) {
                Holder::Dead => {
                    let _ = fs::remove_file(&candidate);
                }
                state => exclusive = Some((id, state)),
            }
        }
    }

    let mut held_shared = Vec::new();
    for entry in fs::read_dir(&shared)? {
        let path = entry?.path();
        let Ok(id) = fs::read_to_string(&path) else {
            continue;
        };
        let id = id.trim().to_string();
        match holder(&id) {
            Holder::Dead => {
                let _ = fs::remove_file(&path);
            }
            state => held_shared.push((id, state)),
        }
    }

    let refuse = |(id, state): (String, Holder)| match state {
        Holder::Orphaned => Error::SessionOrphaned(id),
        _ => Error::SessionBusy(id),
    };
    if let Some(held) = exclusive {
        return Err(refuse(held));
    }
    match mode {
        FilesystemMode::CbcowThrough => {
            // Prefer naming a live holder: an orphan is only actionable once
            // nothing live stands in the way.
            held_shared.sort_by_key(|(_, state)| *state == Holder::Orphaned);
            if let Some(held) = held_shared.into_iter().next() {
                return Err(refuse(held));
            }
            fs::write(leases.join("exclusive"), format!("{session_id}\n"))?;
        }
        FilesystemMode::CbcowPrivate => {
            fs::write(shared.join(session_id), format!("{session_id}\n"))?;
        }
        FilesystemMode::CopyPrivate => {}
    }
    Ok(())
}

/// Releases whatever lease `session_id` holds. Tolerates absence: a crashed
/// detached session's lease is reclaimed by the next `acquire` once it is
/// stopped; an orphan's lease is released by the stop that follows adoption.
pub(crate) fn release(leases: &Path, legacy_exclusive: &Path, session_id: &str) {
    for candidate in [leases.join("exclusive"), legacy_exclusive.to_path_buf()] {
        if fs::read_to_string(&candidate).is_ok_and(|holder| holder.trim() == session_id) {
            let _ = fs::remove_file(candidate);
        }
    }
    let _ = fs::remove_file(leases.join("shared").join(session_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn admit(
        leases: &Path,
        mode: FilesystemMode,
        id: &str,
        states: &HashMap<&str, Holder>,
    ) -> Result<()> {
        acquire(leases, &leases.join("legacy"), mode, id, |sid| {
            states.get(sid).copied().unwrap_or(Holder::Dead)
        })
    }

    #[test]
    fn orphaned_exclusive_holder_is_reported_not_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let states = HashMap::from([("old", Holder::Orphaned)]);
        admit(dir.path(), FilesystemMode::CbcowThrough, "old", &HashMap::new()).unwrap();

        for mode in [FilesystemMode::CbcowThrough, FilesystemMode::CbcowPrivate] {
            let error = admit(dir.path(), mode, "new", &states).unwrap_err();
            assert!(matches!(&error, Error::SessionOrphaned(id) if id == "old"), "{error}");
        }
        // Still recorded, so the adopter's later stop releases it.
        assert_eq!(fs::read_to_string(dir.path().join("exclusive")).unwrap(), "old\n");
    }

    #[test]
    fn dead_exclusive_holder_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        admit(dir.path(), FilesystemMode::CbcowThrough, "old", &HashMap::new()).unwrap();
        admit(dir.path(), FilesystemMode::CbcowThrough, "new", &HashMap::new()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("exclusive")).unwrap(), "new\n");
    }

    #[test]
    fn through_start_names_a_live_shared_holder_before_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let none = HashMap::new();
        admit(dir.path(), FilesystemMode::CbcowPrivate, "orphan", &none).unwrap();
        admit(dir.path(), FilesystemMode::CbcowPrivate, "live", &none).unwrap();
        let states = HashMap::from([("orphan", Holder::Orphaned), ("live", Holder::Live)]);
        let error = admit(dir.path(), FilesystemMode::CbcowThrough, "new", &states).unwrap_err();
        assert!(matches!(&error, Error::SessionBusy(id) if id == "live"), "{error}");

        // An orphaned private sibling does not block another private session.
        admit(dir.path(), FilesystemMode::CbcowPrivate, "new", &states).unwrap();
    }

    #[test]
    fn judge_reads_owner_lock() {
        let store = tempfile::tempdir().unwrap();
        assert_eq!(judge(false, store.path()), Holder::Dead);
        assert_eq!(judge(true, store.path()), Holder::Live, "detached stays live");
        let guard = crate::owner::try_attach(store.path()).unwrap().unwrap();
        assert_eq!(judge(true, store.path()), Holder::Live);
        drop(guard);
        assert_eq!(judge(true, store.path()), Holder::Orphaned);
    }
}

//! Session ownership: whether a live process is attached to a session.
//!
//! A session's durable record (`status: running`) says what the session last
//! claimed about itself; a process killed with SIGKILL never gets to rewrite
//! it. Ownership answers the other question, "is the process that owns this
//! session still alive?", with an OS lock the kernel releases on any death.
//!
//! Two files live in the session store:
//!
//! - `owner.lock` — held exclusively by the attached owner for the session's
//!   whole life (flock on Unix, an unshared handle on Windows).
//! - `owner.json` — the attachment marker, written only *after* the lock is
//!   held, so no reader can see a marker whose lock is free because its owner
//!   has not taken it yet.
//!
//! No marker means the session is detached (a CLI session, whose starting
//! process exits immediately): such sessions keep the old semantics, and a
//! running one is always treated as live. A marker with a free lock means the
//! owner died: the session is orphaned. Orphans are never reclaimed
//! implicitly; a new owner adopts one and then decides to commit or abort.
//!
//! `owner.lock` sits outside the global lock order in `locks.rs`: only its
//! owner ever holds it for long, and everybody else only try-acquires it, so
//! it can never be part of a wait cycle.

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const OWNER_LOCK: &str = "owner.lock";
pub(crate) const OWNER_RECORD: &str = "owner.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnerRecord {
    pid: u32,
    attached_at_ms: u64,
}

/// Who, if anyone, owns a session right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// Never attached: lifecycle is managed explicitly (CLI).
    Detached,
    /// A live process holds the owner lock.
    Attached { pid: Option<u32> },
    /// Attached once, but the owner is gone.
    Orphaned { pid: Option<u32> },
}

/// Held by the attached owner; dropping it (or dying) releases the session.
pub(crate) struct OwnerGuard {
    #[cfg(not(windows))]
    _lock: crate::locks::Lock,
    #[cfg(windows)]
    _lock: fs::File,
}

#[cfg(not(windows))]
fn try_lock(store: &Path) -> Result<Option<OwnerGuard>> {
    Ok(crate::locks::Lock::try_acquire(&store.join(OWNER_LOCK))?
        .map(|lock| OwnerGuard { _lock: lock }))
}

#[cfg(windows)]
fn try_lock(store: &Path) -> Result<Option<OwnerGuard>> {
    match crate::windows::helper::lock(&store.join(OWNER_LOCK), Duration::ZERO) {
        Ok(file) => Ok(Some(OwnerGuard { _lock: file })),
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: another handle is open.
        Err(crate::Error::Io(error)) if matches!(error.raw_os_error(), Some(32 | 33)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_pid(store: &Path) -> Option<u32> {
    let raw = fs::read(store.join(OWNER_RECORD)).ok()?;
    serde_json::from_slice::<OwnerRecord>(&raw).ok().map(|r| r.pid)
}

/// Attaches the calling process as owner. `None` when someone else holds the
/// lock. A prober holds the lock for an instant, so a few short retries keep
/// a racing probe from failing a legitimate adopt.
pub(crate) fn try_attach(store: &Path) -> Result<Option<OwnerGuard>> {
    let mut attempt = 0;
    let guard = loop {
        if let Some(guard) = try_lock(store)? {
            break guard;
        }
        attempt += 1;
        if attempt >= 5 {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let record = OwnerRecord {
        pid: std::process::id(),
        attached_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    };
    // Written after the lock is held, and atomically, so a reader never sees
    // a marker without a held lock behind it (except after the owner dies).
    let tmp = store.join(format!("{OWNER_RECORD}.tmp"));
    fs::write(&tmp, serde_json::to_vec(&record)?)?;
    fs::rename(&tmp, store.join(OWNER_RECORD))?;
    Ok(Some(guard))
}

/// Reports ownership without changing it. Safe to call from any process,
/// including the owner itself (a separate open conflicts with its own lock).
pub(crate) fn probe(store: &Path) -> Ownership {
    if !store.join(OWNER_RECORD).is_file() {
        return Ownership::Detached;
    }
    let pid = read_pid(store);
    match try_lock(store) {
        Ok(Some(_probe)) => Ownership::Orphaned { pid },
        // Held, or unprobeable: never call a session dead on doubt.
        Ok(None) | Err(_) => Ownership::Attached { pid },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmarked_session_is_detached() {
        let store = tempfile::tempdir().unwrap();
        assert_eq!(probe(store.path()), Ownership::Detached);
    }

    #[test]
    fn owner_is_attached_until_released_then_orphaned_then_adoptable() {
        let store = tempfile::tempdir().unwrap();
        let pid = Some(std::process::id());
        let guard = try_attach(store.path()).unwrap().expect("first attach");
        assert_eq!(probe(store.path()), Ownership::Attached { pid });
        assert!(try_attach(store.path()).unwrap().is_none(), "second owner refused");

        drop(guard); // what the kernel does when the owner dies
        assert_eq!(probe(store.path()), Ownership::Orphaned { pid });

        let adopted = try_attach(store.path()).unwrap().expect("adopt orphan");
        assert_eq!(probe(store.path()), Ownership::Attached { pid });
        drop(adopted);
    }
}

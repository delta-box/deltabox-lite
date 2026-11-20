//! Fence on the next lock: no command outlives its caller past the session's
//! next operation.
//!
//! A command runs in the caller's process, under the session's operation lock.
//! If the caller is SIGKILLed or crashes, no `Drop` runs: the kernel releases
//! the lock, but on macOS the command's process tree keeps running (on Linux,
//! only its direct child dies). To keep later operations from running while
//! that tree still writes, the caller records the command's process group in
//! the session state while it holds the lock, and whoever takes the lock next
//! stops any group still recorded there before doing anything else.
//!
//! Mirrors [`crate::cancel`]: the thread that runs a command opens a [`scope`]
//! naming the record file, and the backend calls [`record`] right after it
//! spawns the command. Outside a scope, [`record`] does nothing.

use crate::command_executor::{process_start, ProcessStart};
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a fenced group gets to exit after SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Serialize, Deserialize)]
struct Recorded {
    pgid: i32,
    /// Start time of the group's leader, so a later reuse of the same number
    /// for an unrelated process is never mistaken for this group.
    leader: ProcessStart,
}

thread_local! {
    static CURRENT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Restores the previous scope and removes this command's record on drop.
pub struct Scope {
    path: PathBuf,
    previous: Option<PathBuf>,
}

/// Record commands started on this thread in `path` until the scope drops.
/// The caller must hold the session's operation lock for the whole scope.
pub fn scope(path: PathBuf) -> Scope {
    let previous = CURRENT.with(|current| current.replace(Some(path.clone())));
    Scope { path, previous }
}

impl Drop for Scope {
    fn drop(&mut self) {
        // The command has finished and its caller is alive, so there is
        // nothing left for the next lock holder to fence.
        let _ = remove(&self.path);
        let previous = self.previous.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Record the process group of a command just spawned on this thread.
pub fn record(pgid: i32) {
    let Some(path) = CURRENT.with(|current| current.borrow().clone()) else {
        return;
    };
    let Some(leader) = u32::try_from(pgid).ok().and_then(process_start) else {
        return;
    };
    // Best effort: a missing record only loses the crash fence, never the
    // command itself.
    let _ = write(&path, &Recorded { pgid, leader });
}

/// Stop whatever a dead caller left running, as recorded in `path`. Call with
/// the session's operation lock held, before any other work. Returns whether
/// a live group was found and stopped.
pub fn reap(path: &Path) -> Result<bool> {
    let recorded: Recorded = match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(recorded) => recorded,
            // A torn record names no group we can trust; drop it.
            Err(_) => {
                remove(path)?;
                return Ok(false);
            }
        },
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let ours = is_recorded_group(&recorded);
    if ours {
        stop_group(recorded.pgid);
    }
    remove(path)?;
    Ok(ours)
}

/// Whether the recorded group still exists and is the one that was recorded.
///
/// A process group number cannot be reused while the group has members. So if
/// the group exists and its leader is gone, the members are ours. If a process
/// with the leader's number exists, it is ours only when its start time
/// matches; otherwise the number was reused after our group ended.
fn is_recorded_group(recorded: &Recorded) -> bool {
    if recorded.pgid <= 1 || !group_exists(recorded.pgid) {
        return false;
    }
    match process_start(recorded.pgid as u32) {
        Some(start) => start == recorded.leader,
        None => true,
    }
}

fn stop_group(pgid: i32) {
    // SAFETY: killpg has no memory-safety preconditions.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let deadline = Instant::now() + GRACE;
    while group_exists(pgid) {
        if Instant::now() >= deadline {
            // SAFETY: as above.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn group_exists(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group exists. EPERM still means
    // it exists.
    let signalled = unsafe { libc::killpg(pgid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn write(path: &Path, recorded: &Recorded) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(recorded)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn spawn_group(script: &str) -> std::process::Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_recorded_group_left_by_a_dead_caller_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-group.json");
        let mut child = spawn_group("sleep 60 & sleep 60");
        let pgid = child.id() as i32;
        {
            let _scope = scope(path.clone());
            record(pgid);
            assert!(path.exists());
            // A crashed caller never drops its scope; simulate that.
            std::mem::forget(_scope);
            CURRENT.with(|current| *current.borrow_mut() = None);
        }
        assert!(reap(&path).unwrap());
        assert!(!path.exists());
        child.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_exists(pgid) {
            assert!(Instant::now() < deadline, "fenced group survived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_finished_command_leaves_nothing_to_fence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-group.json");
        let mut child = spawn_group("true");
        {
            let _scope = scope(path.clone());
            record(child.id() as i32);
        }
        child.wait().unwrap();
        assert!(!path.exists());
        assert!(!reap(&path).unwrap());
    }

    #[test]
    fn a_reused_group_number_is_never_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-group.json");
        // A live group whose leader started at a different time than recorded
        // stands in for an unrelated group that reused the number.
        let mut child = spawn_group("sleep 60");
        let pgid = child.id() as i32;
        let mut leader = process_start(pgid as u32).unwrap();
        leader = ProcessStart {
            coarse: leader.coarse.wrapping_sub(1),
            ..leader
        };
        write(&path, &Recorded { pgid, leader }).unwrap();
        assert!(!reap(&path).unwrap());
        assert!(!path.exists());
        assert!(group_exists(pgid), "an unrelated group must not be stopped");
        // SAFETY: cleaning up the group this test started.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        child.wait().unwrap();
    }

    #[test]
    fn record_outside_a_scope_is_a_no_op() {
        record(std::process::id() as i32);
    }
}

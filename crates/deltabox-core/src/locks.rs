use crate::error::{Error, Result};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

/// Blocking-exclusive file lock, released on drop and by the kernel if the
/// holder dies (flock semantics — required because sandboxed processes may
/// signal and kill same-UID lock holders on Landlock-degraded kernels).
///
/// Global lock order, acquired strictly left to right and never in reverse:
///
///   cbcow.lock -> session.lock -> command-executor operation lock
///       -> publish.lock -> registry.lock
///
/// `session start` takes cbcow.lock but never a session lock; mutating
/// operations take their session lock but never cbcow.lock. publish.lock
/// serializes host publication and publication recovery. registry.lock is
/// innermost and guards only the sessions.json read-modify-write: perform no
/// other lock acquisition and no unrelated I/O while holding it.
pub(crate) struct Lock(File);

impl Lock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_with(path, libc::LOCK_EX)
    }

    /// Shared (reader) mode: any number of shared holders coexist, while an
    /// exclusive `acquire` waits for all of them and excludes them all.
    /// Session-start host scans hold the publish lock in this mode so
    /// sibling starts overlap; publication and publication recovery keep
    /// taking it exclusive and therefore still never run during a scan.
    pub(crate) fn acquire_shared(path: &Path) -> Result<Self> {
        Self::acquire_with(path, libc::LOCK_SH)
    }

    /// Non-blocking exclusive attempt: `None` when another process holds the
    /// lock. Probes whether a start's admission pin is still alive — the pin
    /// is an flock, so the kernel releases it the moment the holder dies.
    pub(crate) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(Error::Io(error));
        }
        Ok(Some(Self(file)))
    }

    fn acquire_with(path: &Path, operation: libc::c_int) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

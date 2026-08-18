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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
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

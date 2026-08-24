//! Cross-process file locks shared by Linux session managers.

use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Blocking advisory lock over a persistent, private file.
pub(crate) struct FileLock(File);

impl FileLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn serializes_callers_and_keeps_the_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.lock");
        let first = FileLock::acquire(&path).unwrap();
        let second_path = path.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _second = FileLock::acquire(&second_path).unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
        assert!(path.exists());
    }

    #[test]
    fn ignores_legacy_pid_contents_when_no_process_holds_flock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.lock");
        std::fs::write(&path, "4294967294\n").unwrap();

        let lock = FileLock::acquire(&path).unwrap();
        drop(lock);
        assert!(path.exists());
    }
}

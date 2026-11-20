//! Session operation lock, command environment, and retirement of the command
//! helper that older versions ran.
//!
//! Commands run in the calling process (see `crate::backend::run_once`), so
//! no session-scoped command helper exists any more. What remains here:
//!
//! - the session's operation lock, an `flock` held by whoever runs a command
//!   or changes session state;
//! - the environment allowlist a command inherits from its caller;
//! - process start times, to tell a live process from a later one that reused
//!   its number;
//! - stopping a command helper an older version left running, identified by
//!   its ready record, and cleaning up its files.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a legacy helper gets to exit after SIGTERM before SIGKILL.
const LEGACY_STOP_GRACE: Duration = Duration::from_secs(5);

/// Paths of a session's command endpoint. Only the operation lock is in use;
/// the socket, token and ready file belonged to the command helper and are
/// kept so a helper left by an older version can be found and retired.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub ready_file: PathBuf,
    pub operation_lock: PathBuf,
}

impl Endpoint {
    pub fn new(socket: PathBuf, token_file: PathBuf, ready_file: PathBuf) -> Self {
        let operation_lock = ready_file.with_extension("lock");
        Self {
            socket,
            token_file,
            ready_file,
            operation_lock,
        }
    }
}

/// Held by state-changing operations and for the whole of every command, so
/// commit, abort and checkpoint never run while a command writes. The file is
/// opened close-on-exec, so a command never inherits it.
pub struct OperationLock(File);

impl OperationLock {
    fn acquire(path: &Path) -> Result<Self> {
        prepare_private_parent(path)?;
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

impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn lock_operations(endpoint: &Endpoint) -> Result<OperationLock> {
    OperationLock::acquire(&endpoint.operation_lock)
}

/// Capture the small inherited environment allowlist, then the request's
/// explicit values, which take precedence.
pub fn capture_environment(
    explicit: Option<Vec<(String, String)>>,
) -> Option<Vec<(String, String)>> {
    let mut environment = Vec::new();
    for key in [
        "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TERM", "TZ", "USER",
    ] {
        if let Ok(value) = std::env::var(key) {
            environment.push((key.to_owned(), value));
        }
    }
    environment.extend(explicit.unwrap_or_default());
    (!environment.is_empty()).then_some(environment)
}

/// The part of an older helper's ready record needed to identify it.
#[derive(Debug, Deserialize)]
struct ReadyRecord {
    pid: u32,
    process_start: ProcessStart,
}

/// When a process started, to tell a live process from a later one that
/// reused its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessStart {
    pub(crate) coarse: u64,
    pub(crate) fine: u64,
}

/// Stop a command helper that an older version left running for this session,
/// then remove its files. Does nothing when there is none.
pub fn shutdown(endpoint: &Endpoint) -> Result<()> {
    if let Some(pid) = legacy_helper(endpoint)? {
        // SAFETY: kill has no memory-safety preconditions; the pid was just
        // matched against the helper's recorded start time.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + LEGACY_STOP_GRACE;
        while legacy_helper(endpoint)?.is_some() {
            if Instant::now() >= deadline {
                // SAFETY: as above.
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    remove_if_exists(&endpoint.ready_file)?;
    remove_if_exists(&endpoint.socket)?;
    Ok(())
}

/// Best-effort removal of the endpoint's files after the owning session has
/// become durably stopped. Callers still hold the session and operation locks
/// here, so unlinking the open lock file cannot split live mutual exclusion.
pub fn cleanup_stopped(endpoint: &Endpoint) {
    if !matches!(legacy_helper(endpoint), Ok(None)) {
        return;
    }
    let _ = fs::remove_file(&endpoint.ready_file);
    let _ = fs::remove_file(&endpoint.socket);
    let _ = fs::remove_file(&endpoint.token_file);
    let _ = fs::remove_file(&endpoint.operation_lock);
    if let Some(runtime) = endpoint.socket.parent() {
        let _ = fs::remove_dir(runtime);
    }
}

/// The pid of a still-running legacy helper, matched by its start time so a
/// reused process number is never signalled.
fn legacy_helper(endpoint: &Endpoint) -> Result<Option<u32>> {
    let ready: Option<ReadyRecord> = match fs::read(&endpoint.ready_file) {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(ready
        .filter(|ready| process_start(ready.pid) == Some(ready.process_start))
        .map(|ready| ready.pid))
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn process_start(pid: u32) -> Option<ProcessStart> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    let mut fields = after_name.split_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    Some(ProcessStart {
        coarse: fields.nth(18)?.parse().ok()?,
        fine: 0,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn process_start(pid: u32) -> Option<ProcessStart> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(size).ok()?,
        )
    };
    if read != i32::try_from(size).ok()? {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(ProcessStart {
        coarse: info.pbi_start_tvsec,
        fine: info.pbi_start_tvusec,
    })
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn endpoint(dir: &Path) -> Endpoint {
        Endpoint::new(
            dir.join("command.sock"),
            dir.join("command-token"),
            dir.join("command.ready"),
        )
    }

    #[test]
    fn a_legacy_helper_is_stopped_and_its_files_removed() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(dir.path());
        let mut helper = Command::new("sleep").arg("60").spawn().unwrap();
        let start = process_start(helper.id()).unwrap();
        fs::write(
            &endpoint.ready_file,
            serde_json::json!({
                "pid": helper.id(),
                "process_start": start,
                "runtime_environment": [],
                "nonce": "older-version",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(&endpoint.socket, b"").unwrap();
        shutdown(&endpoint).unwrap();
        assert!(!helper.wait().unwrap().success());
        assert!(!endpoint.ready_file.exists());
        assert!(!endpoint.socket.exists());
    }

    #[test]
    fn a_reused_helper_pid_is_never_signalled() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(dir.path());
        let mut bystander = Command::new("sleep").arg("60").spawn().unwrap();
        let start = process_start(bystander.id()).unwrap();
        let stale = ProcessStart {
            coarse: start.coarse.wrapping_sub(1),
            ..start
        };
        fs::write(
            &endpoint.ready_file,
            serde_json::json!({"pid": bystander.id(), "process_start": stale}).to_string(),
        )
        .unwrap();
        shutdown(&endpoint).unwrap();
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "bystander was signalled"
        );
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[test]
    fn the_operation_lock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(dir.path());
        let held = lock_operations(&endpoint).unwrap();
        let file = OpenOptions::new()
            .write(true)
            .open(&endpoint.operation_lock)
            .unwrap();
        let busy = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(busy, 0, "a second holder got the lock");
        drop(held);
        let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(free, 0);
    }
}

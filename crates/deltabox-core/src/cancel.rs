//! Cooperative cancellation for a command that is running on this thread.
//!
//! A command's lifetime is bound to the operation that requested it. The
//! thread that runs a command opens a [`scope`], backends register a
//! "killer" for each process tree they start, and any other thread holding the
//! returned [`CancelHandle`] can end the command with [`CancelHandle::cancel`].
//! Outside a scope, [`register`] does nothing, so callers that do not need
//! cancellation are unaffected.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

type Killer = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct State {
    cancelled: bool,
    next_id: u64,
    killers: Vec<(u64, Killer)>,
}

/// Cloneable handle that cancels the command running inside a [`Scope`].
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<State>>);

impl CancelHandle {
    /// Kill everything registered in the scope. Cancellation is sticky: a
    /// command that registers after this call is killed immediately, so a
    /// cancel that races with process start is never lost.
    pub fn cancel(&self) {
        let killers: Vec<Killer> = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.cancelled = true;
            state.killers.iter().map(|(_, k)| Arc::clone(k)).collect()
        };
        for killer in killers {
            killer();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).cancelled
    }
}

thread_local! {
    static CURRENT: RefCell<Option<CancelHandle>> = const { RefCell::new(None) };
}

/// Guard that makes a [`CancelHandle`] current for the calling thread.
pub struct Scope {
    previous: Option<CancelHandle>,
}

/// Open a cancellation scope on the calling thread.
pub fn scope() -> (Scope, CancelHandle) {
    let handle = CancelHandle::default();
    let scope = enter(&handle);
    (scope, handle)
}

/// Make an existing handle current for the calling thread.
///
/// Use this when the handle must exist before the thread that runs the
/// command does, for example to register it where another thread can cancel
/// it before the command has started. A cancel that already happened is not
/// lost: anything registered afterwards is killed immediately.
pub fn enter(handle: &CancelHandle) -> Scope {
    let previous = CURRENT.with(|current| current.replace(Some(handle.clone())));
    Scope { previous }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Removes its killer from the scope when the command has finished.
pub struct Registration {
    scope: Option<(CancelHandle, u64)>,
}

/// Register a killer for a process tree that was just started on this thread.
/// Keep the returned guard alive until the command has been reaped.
pub fn register(killer: impl Fn() + Send + Sync + 'static) -> Registration {
    let Some(handle) = CURRENT.with(|current| current.borrow().clone()) else {
        return Registration { scope: None };
    };
    let killer: Killer = Arc::new(killer);
    let (id, already_cancelled) = {
        let mut state = handle.0.lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next_id;
        state.next_id += 1;
        state.killers.push((id, Arc::clone(&killer)));
        (id, state.cancelled)
    };
    if already_cancelled {
        killer();
    }
    Registration {
        scope: Some((handle, id)),
    }
}

impl Registration {
    /// Whether a cancel scope is open, i.e. whether a cancel can ever arrive.
    pub fn is_active(&self) -> bool {
        self.scope.is_some()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some((handle, id)) = self.scope.take() {
            let mut state = handle.0.lock().unwrap_or_else(|e| e.into_inner());
            state.killers.retain(|(candidate, _)| *candidate != id);
        }
    }
}

/// SIGKILL a whole process group, including grandchildren.
#[cfg(unix)]
pub fn kill_process_group(pgid: i32) {
    if pgid > 1 {
        // SAFETY: killpg has no memory-safety preconditions.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// How long processes left behind by a finished command get to exit after
/// SIGTERM before they are sent SIGKILL.
pub const SURVIVOR_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// End everything still running in a command's process group after its main
/// process has exited, and return how many processes were found.
///
/// A command owns the processes it starts: `sleep 30 & echo started` returns
/// once `sh` exits, and the `sleep` is stopped here instead of outliving the
/// command (and its review scope) unseen. This mirrors `bwrap --unshare-pid`
/// and container runtimes, where the sandbox ends with its main process.
/// Survivors get SIGTERM, up to `grace` to exit, then SIGKILL.
///
/// A descendant that left the group (`setsid`) is not reached; the caller's
/// bounded pipe drain still keeps it from hanging the command.
#[cfg(unix)]
pub fn terminate_survivors(pgid: i32, grace: std::time::Duration) -> u32 {
    if pgid <= 1 {
        return 0;
    }
    let found = live_group_members(pgid);
    if found == 0 {
        return 0;
    }
    // SAFETY: killpg has no memory-safety preconditions.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + grace;
    while live_group_members(pgid) > 0 {
        if std::time::Instant::now() >= deadline {
            kill_process_group(pgid);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    found
}

/// Count running (non-zombie) processes in a process group. The group's
/// leader has already been reaped, so the pgid cannot name a new group.
#[cfg(target_os = "linux")]
fn live_group_members(pgid: i32) -> u32 {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return group_exists(pgid);
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .filter(|stat| {
            // Fields after the parenthesised command name: state ppid pgrp ...
            let Some((_, rest)) = stat.rsplit_once(')') else {
                return false;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next();
            let pgrp = fields.nth(1).and_then(|value| value.parse::<i32>().ok());
            pgrp == Some(pgid) && state != Some("Z") && state != Some("X")
        })
        .count() as u32
}

/// Without a cheap per-group process listing, report whether the group still
/// has any member at all (zombies included, so the grace period may run out
/// before SIGKILL; the result is the same).
#[cfg(all(unix, not(target_os = "linux")))]
fn live_group_members(pgid: i32) -> u32 {
    group_exists(pgid)
}

#[cfg(unix)]
fn group_exists(pgid: i32) -> u32 {
    // SAFETY: signal 0 only checks that the group exists and may be signalled.
    u32::from(unsafe { libc::killpg(pgid, 0) } == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(unix)]
    #[test]
    fn terminate_survivors_stops_background_children_of_an_exited_command() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo started"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let mut stdout = child.stdout.take().unwrap();
        assert!(child.wait().unwrap().success());

        let started = std::time::Instant::now();
        assert_eq!(terminate_survivors(pgid, SURVIVOR_GRACE), 1);
        // The sleep held stdout open; once it is stopped the pipe reaches EOF.
        let mut output = String::new();
        stdout.read_to_string(&mut output).unwrap();
        assert_eq!(output, "started\n");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(terminate_survivors(pgid, SURVIVOR_GRACE), 0);
    }

    #[cfg(unix)]
    #[test]
    fn terminate_survivors_is_a_no_op_for_a_command_without_children() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "true"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(terminate_survivors(pgid, SURVIVOR_GRACE), 0);
    }

    #[test]
    fn register_without_scope_is_a_no_op() {
        let _registration = register(|| panic!("must not run"));
    }

    #[test]
    fn cancel_runs_registered_killers_once_per_call() {
        let (_scope, handle) = scope();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let _registration = register(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        handle.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(handle.is_cancelled());
    }

    #[test]
    fn cancel_before_register_kills_immediately() {
        let (_scope, handle) = scope();
        handle.cancel();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let _registration = register(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn finished_commands_are_not_killed() {
        let (_scope, handle) = scope();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        drop(register(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        handle.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn enter_installs_an_existing_handle_and_keeps_an_earlier_cancel() {
        let handle = CancelHandle::default();
        handle.cancel();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let scope = enter(&handle);
        let registration = register(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        assert!(registration.is_active());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(registration);
        drop(scope);
        assert!(!register(|| {}).is_active(), "scope is restored on drop");
    }

    #[test]
    fn cancel_from_another_thread_reaches_the_scope() {
        let (_scope, handle) = scope();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let _registration = register(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        std::thread::spawn(move || handle.cancel()).join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

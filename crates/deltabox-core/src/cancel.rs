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
    let previous = CURRENT.with(|current| current.replace(Some(handle.clone())));
    (Scope { previous }, handle)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

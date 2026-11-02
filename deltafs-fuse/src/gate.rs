//! Write gate: lets a publisher hold every namespace-visible mutation of a
//! mounted view at a quiescent point. A private commit reads the change list,
//! copies the touched paths out of the backing tree and then accepts the
//! engine state; without the gate a process still running in the session can
//! change the tree between those steps, so the host misses files the engine
//! has already accepted.
//!
//! Writers enter around each engine mutation. A freeze first stops new
//! entries, then waits for in-flight ones to drain; it lasts until `thaw`.
//! Reads never pass through the gate.

use std::io;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct WriteGate {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    writers: usize,
    // Set while a freeze waits for in-flight writers, so new writers queue
    // behind it instead of starving it.
    closing: bool,
    frozen: Option<u64>,
    next_id: u64,
}

pub struct GateEntry<'a> {
    gate: &'a WriteGate,
}

impl WriteGate {
    /// Block while the gate is closing or frozen, then admit one writer.
    pub fn enter(&self) -> GateEntry<'_> {
        let mut state = self.state.lock().expect("write gate poisoned");
        while state.closing || state.frozen.is_some() {
            state = self.changed.wait(state).expect("write gate poisoned");
        }
        state.writers += 1;
        GateEntry { gate: self }
    }

    /// Close the gate and wait for in-flight writers. Returns the freeze id
    /// that `thaw` and `is_frozen` take.
    pub fn freeze(&self, drain_timeout: Duration) -> io::Result<u64> {
        let mut state = self.state.lock().expect("write gate poisoned");
        if state.closing || state.frozen.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "writes are already frozen",
            ));
        }
        state.closing = true;
        let deadline = Instant::now() + drain_timeout;
        while state.writers > 0 {
            let now = Instant::now();
            if now >= deadline {
                state.closing = false;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "in-flight writes did not drain",
                ));
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .expect("write gate poisoned")
                .0;
        }
        state.closing = false;
        state.next_id += 1;
        let id = state.next_id;
        state.frozen = Some(id);
        Ok(id)
    }

    pub fn thaw(&self, id: u64) {
        let mut state = self.state.lock().expect("write gate poisoned");
        if state.frozen == Some(id) {
            state.frozen = None;
            self.changed.notify_all();
        }
    }

    /// Release any freeze, for shutdown: blocked writers must finish before
    /// the mount can be torn down.
    pub fn thaw_all(&self) {
        let mut state = self.state.lock().expect("write gate poisoned");
        state.frozen = None;
        self.changed.notify_all();
    }

    pub fn is_frozen(&self, id: u64) -> bool {
        self.state.lock().expect("write gate poisoned").frozen == Some(id)
    }
}

impl Drop for GateEntry<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().expect("write gate poisoned");
        state.writers -= 1;
        if state.writers == 0 {
            self.gate.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn freeze_waits_for_in_flight_writers_and_blocks_new_ones() {
        let gate = Arc::new(WriteGate::default());
        let entry = gate.enter();
        let frozen = Arc::new(AtomicBool::new(false));
        let freezer = {
            let gate = gate.clone();
            let frozen = frozen.clone();
            std::thread::spawn(move || {
                let id = gate.freeze(Duration::from_secs(5)).unwrap();
                frozen.store(true, Ordering::SeqCst);
                id
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !frozen.load(Ordering::SeqCst),
            "freeze must wait for the writer"
        );
        drop(entry);
        let id = freezer.join().unwrap();
        assert!(gate.is_frozen(id));

        let entered = Arc::new(AtomicBool::new(false));
        let writer = {
            let gate = gate.clone();
            let entered = entered.clone();
            std::thread::spawn(move || {
                let _entry = gate.enter();
                entered.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::SeqCst),
            "a frozen gate must hold writers"
        );
        gate.thaw(id);
        writer.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
        assert!(!gate.is_frozen(id));
    }

    #[test]
    fn a_stuck_writer_times_out_the_freeze_and_reopens_the_gate() {
        let gate = WriteGate::default();
        let entry = gate.enter();
        let error = gate.freeze(Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(entry);
        drop(gate.enter());
    }

    #[test]
    fn only_one_freeze_at_a_time_and_stale_ids_do_not_thaw() {
        let gate = WriteGate::default();
        let first = gate.freeze(Duration::from_secs(1)).unwrap();
        assert_eq!(
            gate.freeze(Duration::from_secs(1)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        gate.thaw(first);
        let second = gate.freeze(Duration::from_secs(1)).unwrap();
        gate.thaw(first);
        assert!(gate.is_frozen(second));
        gate.thaw_all();
        assert!(!gate.is_frozen(second));
    }
}

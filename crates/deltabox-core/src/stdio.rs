//! Where a command's output goes: captured into its `ExecResult` (the default),
//! or streamed into pipes the caller owns.
//!
//! Mirrors [`crate::cancel`]: the thread that runs a command opens a [`scope`]
//! with the write ends of the caller's pipes, and the backend's output readers
//! relay into them instead of capturing. Everything else about running the
//! command (locks, fence, journaling, copy-private transactions) is unchanged,
//! because the backends and session managers do not change shape: only where
//! the bytes end up does.
//!
//! Relaying is a blocking copy, so backpressure carries through: a caller that
//! reads slowly fills its pipe, the relay blocks, the command's pipe fills, and
//! the command blocks on write. A caller that closes its read end makes the
//! relay stop, and the command then gets `EPIPE` like any other pipe writer.

use crate::worker::read_capped;
use std::cell::RefCell;
use std::io::{PipeWriter, Read};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// The caller's side of a streamed command.
struct Redirect {
    stdout: Option<PipeWriter>,
    stderr: Option<PipeWriter>,
    pid: Arc<AtomicU32>,
}

thread_local! {
    static CURRENT: RefCell<Option<Redirect>> = const { RefCell::new(None) };
}

/// Restores the previous redirect on drop. Dropping it also drops any write
/// end the command never took, so the caller sees EOF.
pub struct Scope {
    previous: Option<Redirect>,
}

/// Stream the output of the command run next on this thread into `stdout`
/// and `stderr` (the write ends of the caller's pipes). `pid` receives the
/// command's process id as soon as it has started.
pub fn scope(stdout: PipeWriter, stderr: PipeWriter, pid: Arc<AtomicU32>) -> Scope {
    let redirect = Redirect {
        stdout: Some(stdout),
        stderr: Some(stderr),
        pid,
    };
    let previous = CURRENT.with(|current| current.replace(Some(redirect)));
    Scope { previous }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Whether the command run next on this thread streams its output. Paths that
/// cannot stream (the reused persistent shell) start a fresh process instead.
pub(crate) fn active() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

/// Report the started command's process id to the caller, if it is streaming.
pub(crate) fn started(pid: u32) {
    CURRENT.with(|current| {
        if let Some(redirect) = current.borrow().as_ref() {
            redirect.pid.store(pid, Ordering::Release);
        }
    });
}

/// Readers for a started command's stdout and stderr: each captures (capped)
/// or, when the caller is streaming, relays into the caller's pipe.
pub(crate) struct Readers {
    stdout: JoinHandle<(String, bool)>,
    stderr: JoinHandle<(String, bool)>,
}

/// Start reading a command's output pipes on their own threads.
pub(crate) fn read(
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
) -> Readers {
    let (out_sink, err_sink) = CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .map_or((None, None), |redirect| {
                (redirect.stdout.take(), redirect.stderr.take())
            })
    });
    Readers {
        stdout: std::thread::spawn(move || drain(stdout, out_sink)),
        stderr: std::thread::spawn(move || drain(stderr, err_sink)),
    }
}

impl Readers {
    /// Wait for both pipes to reach EOF. Returns the captured stdout, stderr
    /// and whether either was truncated; streamed output is not captured.
    pub(crate) fn join(self) -> crate::error::Result<(String, String, bool)> {
        let failed = |_| crate::error::Error::Backend("output reader failed".into());
        let (stdout, out_cut) = self.stdout.join().map_err(failed)?;
        let (stderr, err_cut) = self.stderr.join().map_err(failed)?;
        Ok((stdout, stderr, out_cut || err_cut))
    }
}

fn drain(mut source: impl Read, sink: Option<PipeWriter>) -> (String, bool) {
    match sink {
        None => read_capped(source),
        Some(mut sink) => {
            // A write error means the caller closed its end: stop relaying and
            // drop our read end, so the command's next write fails with EPIPE.
            let _ = std::io::copy(&mut source, &mut sink);
            (String::new(), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn without_a_scope_output_is_captured() {
        assert!(!active());
        let (out, err, truncated) = read(&b"out"[..], &b"err"[..]).join().unwrap();
        assert_eq!(
            (out.as_str(), err.as_str(), truncated),
            ("out", "err", false)
        );
    }

    #[test]
    fn a_scope_relays_into_the_callers_pipes_and_closes_them() {
        let (mut out_read, out_write) = std::io::pipe().unwrap();
        let (mut err_read, err_write) = std::io::pipe().unwrap();
        let pid = Arc::new(AtomicU32::new(0));
        let scope = scope(out_write, err_write, Arc::clone(&pid));
        assert!(active());
        started(42);
        assert_eq!(pid.load(Ordering::Acquire), 42);
        let (out, err, _) = read(&b"streamed"[..], &b"warned"[..]).join().unwrap();
        assert!(
            out.is_empty() && err.is_empty(),
            "streamed output is not captured"
        );
        drop(scope);
        assert!(!active());
        let mut text = String::new();
        out_read.read_to_string(&mut text).unwrap();
        assert_eq!(text, "streamed");
        text.clear();
        err_read.read_to_string(&mut text).unwrap();
        assert_eq!(text, "warned");
    }

    #[test]
    fn a_closed_caller_pipe_stops_the_relay() {
        let (out_read, out_write) = std::io::pipe().unwrap();
        let (_err_read, err_write) = std::io::pipe().unwrap();
        drop(out_read);
        let _scope = scope(out_write, err_write, Arc::default());
        let (source_read, mut source_write) = std::io::pipe().unwrap();
        let readers = read(source_read, &b""[..]);
        // The relay hits EPIPE on its first write and drops the command's
        // pipe, so the command fails instead of blocking forever.
        let failed = (0..64).any(|_| source_write.write_all(&[b'x'; 65536]).is_err());
        assert!(failed, "the command kept writing into a closed caller pipe");
        readers.join().unwrap();
    }
}

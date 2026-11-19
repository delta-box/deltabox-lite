//! Streaming output sink and bounded pipe readers for running commands.
//!
//! Mirrors [`crate::cancel`]: the thread that runs a command opens an
//! [`enter`] scope with an [`OutputSink`], backends forward stdout/stderr
//! chunks as they arrive, and callers that do not install a sink incur no
//! streaming overhead.

use crate::worker::MAX_EXEC_OUTPUT_BYTES;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long [`PipeReader::finish`] waits for EOF after the main process has
/// exited and its process group has been cleaned up. Guards against a
/// descendant that escaped its process group (e.g. via `setsid`) while still
/// holding a pipe write end open.
pub(crate) const POST_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Receives live stdout/stderr chunks while a command is running.
pub trait OutputSink: Send + Sync {
    fn write(&self, stream: OutputStream, bytes: &[u8]);
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<dyn OutputSink>>> = const { RefCell::new(None) };
}

/// Guard that restores the previous thread-local [`OutputSink`] on drop.
pub struct Scope {
    previous: Option<Arc<dyn OutputSink>>,
}

/// Make `sink` current for the calling thread until the returned [`Scope`] drops.
pub fn enter(sink: Arc<dyn OutputSink>) -> Scope {
    let previous = CURRENT.with(|current| current.replace(Some(sink)));
    Scope { previous }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Return the active [`OutputSink`] for the calling thread, if any.
pub fn current() -> Option<Arc<dyn OutputSink>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Forward `bytes` to the active [`OutputSink`] on this thread, if one is set.
pub fn emit(stream: OutputStream, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(sink) = current() {
        sink.write(stream, bytes);
    }
}

#[derive(Default)]
struct PipeBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl PipeBuffer {
    fn push(&mut self, chunk: &[u8]) {
        if self.bytes.len() + chunk.len() <= MAX_EXEC_OUTPUT_BYTES {
            self.bytes.extend_from_slice(chunk);
        } else if self.bytes.len() < MAX_EXEC_OUTPUT_BYTES {
            let room = MAX_EXEC_OUTPUT_BYTES - self.bytes.len();
            self.bytes.extend_from_slice(&chunk[..room]);
            self.truncated = true;
        } else {
            self.truncated = true;
        }
    }
}

/// Background pipe reader that streams chunks to an optional [`OutputSink`],
/// captures up to [`MAX_EXEC_OUTPUT_BYTES`], and supports bounded post-exit
/// completion so an escaped descendant holding a pipe cannot hang callers.
pub(crate) struct PipeReader {
    done: Receiver<()>,
    buffer: Arc<Mutex<PipeBuffer>>,
    active: Arc<AtomicBool>,
}

impl PipeReader {
    pub(crate) fn spawn(
        mut reader: impl Read + Send + 'static,
        stream: OutputStream,
        sink: Option<Arc<dyn OutputSink>>,
    ) -> Self {
        let buffer = Arc::new(Mutex::new(PipeBuffer::default()));
        let captured = Arc::clone(&buffer);
        let active = Arc::new(AtomicBool::new(true));
        let streaming = Arc::clone(&active);
        let (tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !streaming.load(Ordering::Acquire) {
                            break;
                        }
                        let slice = &chunk[..n];
                        if let Some(sink) = &sink {
                            sink.write(stream, slice);
                        }
                        captured
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(slice);
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(());
        });
        Self {
            done,
            buffer,
            active,
        }
    }

    /// Wait up to `timeout` for EOF, then stop forwarding and return whatever
    /// was captured so far.
    pub(crate) fn finish(self, timeout: Duration) -> (String, bool) {
        let _ = self.done.recv_timeout(timeout);
        self.active.store(false, Ordering::Release);
        let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        (
            String::from_utf8_lossy(&buf.bytes).into_owned(),
            buf.truncated,
        )
    }

    /// Wait until `deadline` for EOF, sharing one drain window across pipes.
    pub(crate) fn finish_by(self, deadline: Instant) -> (String, bool) {
        self.finish(deadline.saturating_duration_since(Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    #[derive(Default)]
    struct CollectSink(Mutex<Vec<(OutputStream, Vec<u8>)>>);

    impl OutputSink for CollectSink {
        fn write(&self, stream: OutputStream, bytes: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .push((stream, bytes.to_vec()));
        }
    }

    #[test]
    fn scope_installs_and_restores_sink() {
        assert!(current().is_none());
        let outer = Arc::new(CollectSink::default());
        let _outer_scope = enter(outer.clone());
        emit(OutputStream::Stdout, b"one");
        {
            let inner = Arc::new(CollectSink::default());
            let _inner_scope = enter(inner.clone());
            emit(OutputStream::Stderr, b"two");
            assert_eq!(
                *inner.0.lock().unwrap(),
                vec![(OutputStream::Stderr, b"two".to_vec())]
            );
        }
        emit(OutputStream::Stdout, b"three");
        assert_eq!(
            *outer.0.lock().unwrap(),
            vec![
                (OutputStream::Stdout, b"one".to_vec()),
                (OutputStream::Stdout, b"three".to_vec()),
            ]
        );
    }

    #[test]
    fn pipe_reader_streams_and_captures_until_eof() {
        let sink = Arc::new(CollectSink::default());
        let reader = PipeReader::spawn(
            std::io::Cursor::new(b"hello world".to_vec()),
            OutputStream::Stdout,
            Some(sink.clone()),
        );
        let (text, truncated) = reader.finish(Duration::from_secs(2));
        assert_eq!(text, "hello world");
        assert!(!truncated);
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![(OutputStream::Stdout, b"hello world".to_vec())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn pipe_reader_finish_does_not_block_when_write_end_stays_open() {
        use std::os::unix::io::FromRawFd;

        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };

        let sink = Arc::new(CollectSink::default());
        let reader = PipeReader::spawn(read_end, OutputStream::Stdout, Some(sink.clone()));
        write_end.write_all(b"started\n").unwrap();
        write_end.flush().unwrap();

        let started = Instant::now();
        let (text, truncated) = reader.finish(Duration::from_millis(80));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(text, "started\n");
        assert!(!truncated);
        drop(write_end);
    }
}

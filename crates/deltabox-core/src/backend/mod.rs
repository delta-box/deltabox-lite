use crate::error::Result;
use crate::protocol::{AbortResult, CommitResult, ExecRequest, ExecResult, SessionSpec};
use crate::worker::{LiveProcess, ScanEntry};
use std::path::{Path, PathBuf};

/// Platform sandbox backend. The core delegates everything OS-specific here.
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn is_supported(&self) -> bool;
    fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()>;
    fn stop_session(&mut self, session_id: &str) -> Result<()>;
    /// Round-trip health check against the session worker.
    fn ping(&mut self, session_id: &str) -> Result<()>;
    fn exec(&mut self, session_id: &str, req: ExecRequest) -> Result<ExecResult>;
    /// Read a file from the sandboxed view (upper-then-lower), denying paths
    /// outside the project or matching the policy deny list.
    fn read_file(&mut self, session_id: &str, path: &Path) -> Result<Option<Vec<u8>>>;
    /// Write a file through the sandbox (lands in the COW upper layer).
    fn write_file(&mut self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()>;
    /// Scan the sandboxed project view. Additions/modifications live in the
    /// COW upper layer; deletions are visible only through this scan.
    fn scan(&mut self, session_id: &str) -> Result<Vec<ScanEntry>>;
    /// Processes a finished `exec` left running in the session sandbox.
    /// Backends that cannot observe them report none.
    fn live_processes(&mut self, _session_id: &str) -> Result<Vec<LiveProcess>> {
        Ok(Vec::new())
    }
    /// Terminate those processes; returns any that survived.
    fn stop_live_processes(&mut self, _session_id: &str) -> Result<Vec<LiveProcess>> {
        Ok(Vec::new())
    }
    /// Path of the current COW upper directory.
    fn current_upper(&mut self, session_id: &str) -> Result<PathBuf>;
    /// Freeze the current COW state into `dest_dir/upper`, skipping files
    /// already frozen in `prev_upper` (same size/mtime).
    fn freeze_upper(
        &mut self,
        session_id: &str,
        dest_dir: &Path,
        prev_upper: Option<&Path>,
    ) -> Result<()>;
    /// Merge pending changes into the real project and restart the session.
    fn commit_changes(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        head_dir: &Path,
        deleted: Vec<String>,
    ) -> Result<CommitResult>;
    /// Merge a command-local COW layer into a durable shadow and stop the
    /// runtime. Service-free callers do not need the post-commit restart used
    /// by a warm server session.
    fn materialize_changes(
        &mut self,
        session_id: &str,
        head_dir: &Path,
        deleted: Vec<String>,
    ) -> Result<usize> {
        Ok(self
            .commit_changes(session_id, "materialize", head_dir, deleted)?
            .applied)
    }
    /// Discard pending changes and restore the head checkpoint state.
    fn abort_changes(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        head_dir: &Path,
        deleted: Vec<String>,
    ) -> Result<AbortResult>;
}

#[cfg(target_os = "linux")]
pub mod linux_sandlock;
#[cfg(target_os = "linux")]
pub use linux_sandlock::SandlockBackend as DefaultBackend;

#[cfg(target_os = "macos")]
pub mod macos_seatbelt;
#[cfg(target_os = "macos")]
pub use macos_seatbelt::SeatbeltBackend as DefaultBackend;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub mod stub;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use stub::StubBackend as DefaultBackend;

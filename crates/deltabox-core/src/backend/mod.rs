use crate::error::Result;
use crate::protocol::{
    AbortResult, CommitResult, DiffResult, ExecRequest, ExecResult, SessionSpec,
};
use std::path::Path;

/// Platform sandbox backend. The core delegates everything OS-specific here.
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn is_supported(&self) -> bool;
    fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()>;
    fn stop_session(&mut self, session_id: &str) -> Result<()>;
    fn exec(&mut self, session_id: &str, req: ExecRequest) -> Result<ExecResult>;
    fn pending_change_count(&mut self, session_id: &str) -> Result<usize>;
    fn diff(
        &mut self,
        session_id: &str,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<DiffResult>;
    fn commit_changes(&mut self, session_id: &str, checkpoint_id: &str) -> Result<CommitResult>;
    fn abort_changes(&mut self, session_id: &str, checkpoint_id: &str) -> Result<AbortResult>;
}

#[cfg(target_os = "linux")]
pub mod linux_sandlock;
#[cfg(target_os = "linux")]
pub use linux_sandlock::SandlockBackend as DefaultBackend;

#[cfg(not(target_os = "linux"))]
pub mod stub;
#[cfg(not(target_os = "linux"))]
pub use stub::StubBackend as DefaultBackend;

// Keep `Path` referenced at the trait level for future backend methods.
#[allow(dead_code)]
fn _path_placeholder(_p: &Path) {}

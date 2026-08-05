use super::SandboxBackend;
use crate::error::{Error, Result};
use crate::protocol::{
    AbortResult, CommitResult, DiffResult, ExecRequest, ExecResult, SessionSpec,
};

/// Linux backend powered by sandlock-core (Landlock + seccomp + COW).
///
/// Phase 1 will replace this placeholder with a real confined worker that
/// owns the COW layer across exec calls.
#[derive(Default)]
pub struct SandlockBackend;

impl SandboxBackend for SandlockBackend {
    fn name(&self) -> &'static str {
        "sandlock (phase 1 pending)"
    }

    fn is_supported(&self) -> bool {
        false
    }

    fn start_session(&mut self, _session_id: &str, _spec: &SessionSpec) -> Result<()> {
        Ok(())
    }

    fn stop_session(&mut self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    fn exec(&mut self, _session_id: &str, _req: ExecRequest) -> Result<ExecResult> {
        Err(Error::Unsupported(
            "Linux backend is not implemented yet (Phase 1)".into(),
        ))
    }

    fn pending_change_count(&mut self, _session_id: &str) -> Result<usize> {
        Ok(0)
    }

    fn diff(
        &mut self,
        _session_id: &str,
        _from: Option<&str>,
        _to: Option<&str>,
    ) -> Result<DiffResult> {
        Ok(DiffResult::default())
    }

    fn commit_changes(&mut self, _session_id: &str, _checkpoint_id: &str) -> Result<CommitResult> {
        Err(Error::Unsupported(
            "Linux backend is not implemented yet (Phase 1)".into(),
        ))
    }

    fn abort_changes(&mut self, _session_id: &str, _checkpoint_id: &str) -> Result<AbortResult> {
        Err(Error::Unsupported(
            "Linux backend is not implemented yet (Phase 1)".into(),
        ))
    }
}

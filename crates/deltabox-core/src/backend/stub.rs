use super::SandboxBackend;
use crate::error::{Error, Result};
use crate::protocol::{
    AbortResult, CommitResult, DiffResult, ExecRequest, ExecResult, SessionSpec,
};

/// Placeholder backend for platforms without a real implementation yet.
#[derive(Default)]
pub struct StubBackend;

impl SandboxBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
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
            "exec requires a real backend (Linux sandlock or macOS seatbelt); the stub backend does not execute commands".into(),
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
        Err(Error::Unsupported("commit requires a real backend".into()))
    }

    fn abort_changes(&mut self, _session_id: &str, _checkpoint_id: &str) -> Result<AbortResult> {
        Err(Error::Unsupported("abort requires a real backend".into()))
    }
}

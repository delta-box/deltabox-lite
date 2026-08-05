use super::SandboxBackend;
use crate::error::{Error, Result};
use crate::protocol::{AbortResult, CommitResult, ExecRequest, ExecResult, SessionSpec};
use crate::worker::ScanEntry;
use std::path::{Path, PathBuf};

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

    fn ping(&mut self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    fn exec(&mut self, _session_id: &str, _req: ExecRequest) -> Result<ExecResult> {
        Err(Error::Unsupported(
            "exec requires a real backend (Linux sandlock or macOS seatbelt); the stub backend does not execute commands".into(),
        ))
    }

    fn scan(&mut self, _session_id: &str) -> Result<Vec<ScanEntry>> {
        Ok(Vec::new())
    }

    fn current_upper(&mut self, _session_id: &str) -> Result<PathBuf> {
        Err(Error::Unsupported(
            "current_upper requires a real backend".into(),
        ))
    }

    fn freeze_upper(&mut self, _session_id: &str, _dest_dir: &Path) -> Result<()> {
        Ok(())
    }

    fn commit_changes(
        &mut self,
        _session_id: &str,
        _checkpoint_id: &str,
        _head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<CommitResult> {
        Err(Error::Unsupported("commit requires a real backend".into()))
    }

    fn abort_changes(
        &mut self,
        _session_id: &str,
        _checkpoint_id: &str,
        _head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<AbortResult> {
        Err(Error::Unsupported("abort requires a real backend".into()))
    }
}

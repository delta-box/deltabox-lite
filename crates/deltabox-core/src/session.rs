use crate::backend::SandboxBackend;
use crate::checkpoint::{CheckpointStore, DEFAULT_CHECKPOINT_CAP};
use crate::error::{Error, Result};
use crate::journal::{Journal, JournalEntry, JournalKind};
use crate::policy::Policy;
use crate::protocol::{
    AbortParams, AbortResult, CheckpointInfo, CheckpointParams, CommitParams, CommitResult,
    DiffParams, DiffResult, ExecParams, ExecRequest, ExecResult, SessionInfo, SessionSpec,
    SessionStatus,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const SESSION_REGISTRY_FILE: &str = "sessions.json";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub project: PathBuf,
    #[serde(default)]
    pub policy_path: Option<PathBuf>,
    pub started_at_ms: u64,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRegistry {
    pub sessions: Vec<SessionMeta>,
}

impl SessionRegistry {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(SESSION_REGISTRY_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = root.join(SESSION_REGISTRY_FILE);
        let tmp = root.join(format!("{SESSION_REGISTRY_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, session_id: &str) -> Option<&SessionMeta> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    pub fn upsert(&mut self, meta: SessionMeta) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == meta.session_id)
        {
            *existing = meta;
        } else {
            self.sessions.push(meta);
        }
    }
}

/// Bookkeeping + orchestration layer. OS-specific work goes to the backend.
pub struct SessionManager {
    root: PathBuf,
    project: PathBuf,
    registry: SessionRegistry,
    backend: Box<dyn SandboxBackend>,
}

impl SessionManager {
    pub fn new(project: &Path, backend: Box<dyn SandboxBackend>) -> Result<Self> {
        let project = project.canonicalize()?;
        let root = project.join(".deltabox");
        std::fs::create_dir_all(&root)?;
        let registry = SessionRegistry::load(&root)?;
        Ok(Self {
            root,
            project,
            registry,
            backend,
        })
    }

    pub fn start(&mut self, policy_path: Option<&Path>) -> Result<SessionInfo> {
        let session_id = Uuid::new_v4().to_string();
        let store_dir = self.root.join(&session_id);
        std::fs::create_dir_all(&store_dir)?;

        let policy = match policy_path {
            Some(p) => Policy::load(p)?,
            None => Policy::default_agent(),
        };
        let spec = SessionSpec {
            project: self.project.clone(),
            store_dir: store_dir.clone(),
            policy: policy.clone(),
        };
        self.backend.start_session(&session_id, &spec)?;

        CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let mut journal = Journal::open(&store_dir.join("journal.jsonl"))?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.clone(),
            kind: JournalKind::SessionStart,
            data: None,
        })?;
        journal.flush_sync()?;

        let meta = SessionMeta {
            session_id: session_id.clone(),
            project: self.project.clone(),
            policy_path: policy_path.map(|p| p.to_path_buf()),
            started_at_ms: now_ms(),
            status: SessionStatus::Running,
        };
        self.registry.upsert(meta);
        self.registry.save(&self.root)?;
        self.info_from_meta(&session_id)
    }

    pub fn stop(&mut self, session_id: &str) -> Result<SessionInfo> {
        let meta = self.require_meta(session_id)?.clone();
        if meta.status == SessionStatus::Stopped {
            return Err(Error::SessionStopped(session_id.to_string()));
        }
        self.backend.stop_session(session_id)?;
        let mut journal = self.open_journal(session_id)?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.to_string(),
            kind: JournalKind::SessionStop,
            data: None,
        })?;
        journal.flush_sync()?;
        self.registry.upsert(SessionMeta {
            status: SessionStatus::Stopped,
            ..meta
        });
        self.registry.save(&self.root)?;
        self.info_from_meta(session_id)
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        self.info_from_meta(session_id)
    }

    pub fn exec(&mut self, params: ExecParams) -> Result<ExecResult> {
        self.ensure_running(&params.session_id)?;
        let req = ExecRequest {
            argv: params.argv,
            cwd: params.cwd,
            env: params.env,
        };
        let result = self.backend.exec(&params.session_id, req)?;
        self.journal_append(
            &params.session_id,
            JournalKind::Exec,
            Some(serde_json::to_value(&result).unwrap_or_default()),
        )?;
        Ok(result)
    }

    pub fn checkpoint(&mut self, params: CheckpointParams) -> Result<CheckpointInfo> {
        self.ensure_running(&params.session_id)?;
        let store_dir = self.store_dir(&params.session_id);
        let change_count = self.backend.pending_change_count(&params.session_id)?;
        let mut manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let meta = manifest.create(params.label.as_deref(), change_count)?;
        self.journal_append(
            &params.session_id,
            JournalKind::Checkpoint,
            Some(serde_json::to_value(&meta).unwrap_or_default()),
        )?;
        Ok(CheckpointInfo {
            id: meta.id,
            label: meta.label,
            created_at_ms: meta.created_at_ms,
            change_count: meta.change_count,
            committed: meta.committed,
        })
    }

    pub fn diff(&mut self, params: DiffParams) -> Result<DiffResult> {
        self.ensure_running(&params.session_id)?;
        let DiffParams {
            session_id,
            from,
            to,
        } = params;
        let mut result = self
            .backend
            .diff(&session_id, from.as_deref(), to.as_deref())?;
        result.from = from;
        result.to = to;
        Ok(result)
    }

    pub fn commit(&mut self, params: CommitParams) -> Result<CommitResult> {
        self.ensure_running(&params.session_id)?;
        let store_dir = self.store_dir(&params.session_id);
        let mut manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let target = params
            .checkpoint
            .clone()
            .unwrap_or_else(|| manifest.head().map(str::to_string).unwrap_or_default());
        if target.is_empty() {
            return Err(Error::CheckpointNotFound(
                "no checkpoint to commit".to_string(),
            ));
        }
        let result = self.backend.commit_changes(&params.session_id, &target)?;
        manifest.commit(&target)?;
        self.journal_append(
            &params.session_id,
            JournalKind::Commit,
            Some(serde_json::to_value(&result).unwrap_or_default()),
        )?;
        Ok(result)
    }

    pub fn abort(&mut self, params: AbortParams) -> Result<AbortResult> {
        self.ensure_running(&params.session_id)?;
        let store_dir = self.store_dir(&params.session_id);
        let manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let target = params
            .checkpoint
            .clone()
            .unwrap_or_else(|| manifest.head().map(str::to_string).unwrap_or_default());
        if target.is_empty() {
            return Err(Error::CheckpointNotFound(
                "no checkpoint to abort to".to_string(),
            ));
        }
        let result = self.backend.abort_changes(&params.session_id, &target)?;
        self.journal_append(
            &params.session_id,
            JournalKind::Abort,
            Some(serde_json::to_value(&result).unwrap_or_default()),
        )?;
        Ok(result)
    }

    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        self.registry.sessions.clone()
    }

    pub fn registry_path(&self) -> PathBuf {
        self.root.join(SESSION_REGISTRY_FILE)
    }

    fn info_from_meta(&self, session_id: &str) -> Result<SessionInfo> {
        let meta = self.require_meta(session_id)?;
        let store_dir = self.store_dir(session_id);
        let manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        Ok(SessionInfo {
            session_id: meta.session_id.clone(),
            project: meta.project.clone(),
            started_at_ms: meta.started_at_ms,
            status: meta.status.clone(),
            checkpoint_count: manifest.list().len(),
            pending_changes: 0,
        })
    }

    fn require_meta(&self, session_id: &str) -> Result<&SessionMeta> {
        self.registry
            .get(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))
    }

    fn ensure_running(&self, session_id: &str) -> Result<()> {
        let meta = self.require_meta(session_id)?;
        if meta.status != SessionStatus::Running {
            return Err(Error::SessionStopped(session_id.to_string()));
        }
        Ok(())
    }

    fn store_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    fn open_journal(&self, session_id: &str) -> Result<Journal> {
        Journal::open(&self.store_dir(session_id).join("journal.jsonl"))
    }

    fn journal_append(
        &mut self,
        session_id: &str,
        kind: JournalKind,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut journal = self.open_journal(session_id)?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.to_string(),
            kind,
            data,
        })?;
        journal.flush_sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SandboxBackend;
    use crate::protocol::{ExecRequest, ExecResult, SessionSpec};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestBackend {
        pending: Arc<Mutex<usize>>,
        committed: Arc<Mutex<usize>>,
        aborted: Arc<Mutex<usize>>,
    }

    impl SandboxBackend for TestBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        fn is_supported(&self) -> bool {
            true
        }
        fn start_session(&mut self, _id: &str, _spec: &SessionSpec) -> Result<()> {
            Ok(())
        }
        fn stop_session(&mut self, _id: &str) -> Result<()> {
            Ok(())
        }
        fn exec(&mut self, _id: &str, req: ExecRequest) -> Result<ExecResult> {
            *self.pending.lock().unwrap() += 1;
            Ok(ExecResult {
                exit_code: Some(0),
                stdout: req.argv.join(" "),
                stderr: String::new(),
                duration_ms: 1,
            })
        }
        fn pending_change_count(&mut self, _id: &str) -> Result<usize> {
            Ok(*self.pending.lock().unwrap())
        }
        fn diff(
            &mut self,
            _id: &str,
            _from: Option<&str>,
            _to: Option<&str>,
        ) -> Result<DiffResult> {
            Ok(DiffResult {
                pending_changes: *self.pending.lock().unwrap(),
                ..Default::default()
            })
        }
        fn commit_changes(&mut self, _id: &str, checkpoint_id: &str) -> Result<CommitResult> {
            *self.committed.lock().unwrap() += 1;
            *self.pending.lock().unwrap() = 0;
            Ok(CommitResult {
                checkpoint_id: checkpoint_id.to_string(),
                applied: 1,
            })
        }
        fn abort_changes(&mut self, _id: &str, checkpoint_id: &str) -> Result<AbortResult> {
            *self.aborted.lock().unwrap() += 1;
            *self.pending.lock().unwrap() = 0;
            Ok(AbortResult {
                checkpoint_id: checkpoint_id.to_string(),
                restored: 1,
            })
        }
    }

    #[test]
    fn session_lifecycle_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            SessionManager::new(dir.path(), Box::new(TestBackend::default())).unwrap();
        let info = manager.start(None).unwrap();
        assert_eq!(info.status, SessionStatus::Running);
        assert_eq!(info.checkpoint_count, 0);

        let exec = manager
            .exec(ExecParams {
                session_id: info.session_id.clone(),
                argv: vec!["echo".into(), "hi".into()],
                cwd: None,
                env: None,
            })
            .unwrap();
        assert_eq!(exec.stdout, "echo hi");

        let ckpt = manager
            .checkpoint(CheckpointParams {
                session_id: info.session_id.clone(),
                label: Some("after echo".into()),
            })
            .unwrap();
        assert_eq!(ckpt.change_count, 1);
        assert_eq!(
            manager.status(&info.session_id).unwrap().checkpoint_count,
            1
        );

        let committed = manager
            .commit(CommitParams {
                session_id: info.session_id.clone(),
                checkpoint: None,
            })
            .unwrap();
        assert_eq!(committed.checkpoint_id, ckpt.id);

        manager
            .checkpoint(CheckpointParams {
                session_id: info.session_id.clone(),
                label: None,
            })
            .unwrap();
        let aborted = manager
            .abort(AbortParams {
                session_id: info.session_id,
                checkpoint: None,
            })
            .unwrap();
        assert_eq!(aborted.restored, 1);
    }

    #[test]
    fn registry_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager =
            SessionManager::new(dir.path(), Box::new(TestBackend::default())).unwrap();
        let info = manager.start(None).unwrap();
        drop(manager);
        let reopened = SessionManager::new(dir.path(), Box::new(TestBackend::default())).unwrap();
        let status = reopened.status(&info.session_id).unwrap();
        assert_eq!(status.status, SessionStatus::Running);
    }
}

use crate::backend::SandboxBackend;
use crate::changeset::{Change, ChangeKind};
use crate::checkpoint::{CheckpointStore, DEFAULT_CHECKPOINT_CAP};
use crate::error::{Error, Result};
use crate::journal::{Journal, JournalEntry, JournalKind};
use crate::policy::Policy;
use crate::protocol::{
    AbortParams, AbortResult, CheckpointInfo, CheckpointParams, CommitParams, CommitResult,
    DiffParams, DiffResult, ExecParams, ExecRequest, ExecResult, FsReadResult, SessionInfo,
    SessionSpec, SessionStatus,
};
use crate::snapshot::{
    diff_snapshots, load_snapshot, read_capped_file, save_snapshot, BASE_SCAN_FILE,
};
use crate::worker::ScanEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const SESSION_REGISTRY_FILE: &str = "sessions.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// Flush journal entries without fsync (low-latency default).
    #[default]
    Flush,
    /// fdatasync journal entries at checkpoint/commit/abort boundaries.
    Fsync,
}

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
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        File::open(root)?.sync_all()?;
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
    durability: Durability,
}

impl SessionManager {
    pub fn new(project: &Path, backend: Box<dyn SandboxBackend>) -> Result<Self> {
        Self::new_with_durability(project, backend, Durability::default())
    }

    pub fn new_with_durability(
        project: &Path,
        backend: Box<dyn SandboxBackend>,
        durability: Durability,
    ) -> Result<Self> {
        let project = project.canonicalize()?;
        let root = project.join(".deltabox");
        std::fs::create_dir_all(&root)?;
        let registry = SessionRegistry::load(&root)?;
        Ok(Self {
            root,
            project,
            registry,
            backend,
            durability,
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
            host_project: Some(self.project.clone()),
            store_dir: store_dir.clone(),
            policy: policy.clone(),
        };
        self.backend.start_session(&session_id, &spec)?;
        let base_scan = match self.backend.scan(&session_id) {
            Ok(scan) => scan,
            Err(e) => {
                let _ = self.backend.stop_session(&session_id);
                return Err(e);
            }
        };
        save_snapshot(&store_dir.join(BASE_SCAN_FILE), &base_scan)?;

        CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let mut journal = Journal::open(&store_dir.join("journal.jsonl"))?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.clone(),
            kind: JournalKind::SessionStart,
            data: None,
        })?;
        self.sync_journal(&mut journal)?;

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
        self.sync_journal(&mut journal)?;
        self.registry.upsert(SessionMeta {
            status: SessionStatus::Stopped,
            ..meta
        });
        self.registry.save(&self.root)?;
        self.info_from_meta(session_id)
    }

    pub fn ping(&mut self, session_id: &str) -> Result<()> {
        self.ensure_running(session_id)?;
        self.backend.ping(session_id)
    }

    pub fn status(&mut self, session_id: &str) -> Result<SessionInfo> {
        let meta = self.require_meta(session_id)?.clone();
        let store_dir = self.store_dir(session_id);
        let manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let pending_changes = if meta.status == SessionStatus::Running {
            let current = self.backend.scan(session_id)?;
            let base = load_snapshot(&store_dir.join(BASE_SCAN_FILE))?
                .map(|s| s.entries)
                .unwrap_or_default();
            diff_snapshots(&base, &current).len()
        } else {
            0
        };
        Ok(SessionInfo {
            session_id: meta.session_id.clone(),
            project: meta.project.clone(),
            started_at_ms: meta.started_at_ms,
            status: meta.status.clone(),
            checkpoint_count: manifest.list().len(),
            pending_changes,
        })
    }

    pub fn exec(&mut self, params: ExecParams) -> Result<ExecResult> {
        let label = params.label.clone();
        self.ensure_running(&params.session_id)?;
        let req = ExecRequest {
            argv: params.argv,
            cwd: params.cwd,
            env: params.env,
        };
        let result = self.backend.exec(&params.session_id, req)?;
        // Exec results are informational; flush without fsync so per-command
        // latency is not bound to disk durability.
        let mut journal = self.open_journal(&params.session_id)?;
        let data = match &label {
            Some(l) => serde_json::json!({"label": l, "result": &result}),
            None => serde_json::to_value(&result).unwrap_or_default(),
        };
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: params.session_id.clone(),
            kind: JournalKind::Exec,
            data: Some(data),
        })?;
        Ok(result)
    }

    pub fn read_file(&mut self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        use base64::Engine;
        self.ensure_running(session_id)?;
        let bytes = self
            .backend
            .read_file(session_id, path)?
            .unwrap_or_default();
        Ok(FsReadResult {
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            size: bytes.len(),
        })
    }

    pub fn write_file(&mut self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        self.ensure_running(session_id)?;
        self.backend.write_file(session_id, path, content)?;
        self.journal_append(
            session_id,
            JournalKind::Write,
            Some(serde_json::json!({"path": path})),
        )?;
        Ok(())
    }

    pub fn checkpoint(&mut self, params: CheckpointParams) -> Result<CheckpointInfo> {
        self.ensure_running(&params.session_id)?;
        let store_dir = self.store_dir(&params.session_id);
        let current = self.backend.scan(&params.session_id)?;
        let previous = self.previous_snapshot(&store_dir)?;
        let changes = diff_snapshots(&previous, &current);

        let mut manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let prev_head = manifest.head().map(str::to_string);
        let meta = manifest.create(params.label.as_deref(), changes.len())?;
        let dir = store_dir.join("checkpoints").join(&meta.id);
        std::fs::create_dir_all(&dir)?;
        let prev_upper = prev_head
            .as_deref()
            .map(|h| store_dir.join("checkpoints").join(h).join("upper"));
        self.backend
            .freeze_upper(&params.session_id, &dir, prev_upper.as_deref())?;
        save_snapshot(&dir.join("scan.json"), &current)?;

        let current_paths: HashSet<&str> = current.iter().map(|e| e.path.as_str()).collect();
        let deleted: Vec<String> = previous
            .iter()
            .filter(|p| !current_paths.contains(p.path.as_str()))
            .map(|p| p.path.clone())
            .collect();
        std::fs::write(dir.join("deleted.json"), serde_json::to_vec(&deleted)?)?;

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
        let store_dir = self.store_dir(&params.session_id);

        let (from_dir, from_entries, from_label) = if let Some(from) = &params.from {
            let dir = store_dir.join("checkpoints").join(from);
            let snapshot = load_snapshot(&dir.join("scan.json"))?
                .ok_or_else(|| Error::CheckpointNotFound(from.clone()))?;
            (Some(dir), snapshot.entries, Some(from.clone()))
        } else {
            let snapshot = load_snapshot(&store_dir.join(BASE_SCAN_FILE))?;
            (None, snapshot.map(|s| s.entries).unwrap_or_default(), None)
        };

        let (_, to_entries, to_label, to_upper) = if let Some(to) = &params.to {
            let dir = store_dir.join("checkpoints").join(to);
            let snapshot = load_snapshot(&dir.join("scan.json"))?
                .ok_or_else(|| Error::CheckpointNotFound(to.clone()))?;
            (
                Some(dir.clone()),
                snapshot.entries,
                Some(to.clone()),
                dir.join("upper"),
            )
        } else {
            let current = self.backend.scan(&params.session_id)?;
            let upper = self.backend.current_upper(&params.session_id)?;
            (None, current, None, upper)
        };

        let changes =
            self.build_changes(from_dir.as_deref(), &from_entries, &to_entries, &to_upper)?;
        Ok(DiffResult {
            from: from_label,
            to: to_label,
            pending_changes: changes.len(),
            changes,
        })
    }

    pub fn commit(&mut self, params: CommitParams) -> Result<CommitResult> {
        self.ensure_running(&params.session_id)?;
        let store_dir = self.store_dir(&params.session_id);
        let mut manifest = CheckpointStore::open(&store_dir, DEFAULT_CHECKPOINT_CAP)?;
        let head = manifest.head().map(str::to_string).unwrap_or_default();
        let target = params.checkpoint.clone().unwrap_or_else(|| head.clone());
        if target.is_empty() {
            return Err(Error::CheckpointNotFound(
                "no checkpoint to commit".to_string(),
            ));
        }
        if target != head {
            return Err(Error::Unsupported(
                "commit currently supports the head checkpoint only".to_string(),
            ));
        }

        let head_dir = store_dir.join("checkpoints").join(&head);
        let current = self.backend.scan(&params.session_id)?;
        let base = load_snapshot(&store_dir.join(BASE_SCAN_FILE))?
            .map(|s| s.entries)
            .unwrap_or_default();
        let current_paths: HashSet<&str> = current.iter().map(|e| e.path.as_str()).collect();
        let deleted: Vec<String> = base
            .iter()
            .filter(|p| !current_paths.contains(p.path.as_str()))
            .map(|p| p.path.clone())
            .collect();
        let result = self
            .backend
            .commit_changes(&params.session_id, &head, &head_dir, deleted)?;
        manifest.commit(&head)?;
        save_snapshot(&store_dir.join(BASE_SCAN_FILE), &current)?;
        let _ = std::fs::remove_dir_all(head_dir.join("upper"));
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
        let head = manifest.head().map(str::to_string).unwrap_or_default();
        let target = params.checkpoint.clone().unwrap_or_else(|| head.clone());
        if target.is_empty() {
            return Err(Error::CheckpointNotFound(
                "no checkpoint to abort to".to_string(),
            ));
        }
        if target != head {
            return Err(Error::Unsupported(
                "abort currently supports the head checkpoint only".to_string(),
            ));
        }

        let head_dir = store_dir.join("checkpoints").join(&head);
        let head_scan = load_snapshot(&head_dir.join("scan.json"))?
            .map(|s| s.entries)
            .unwrap_or_default();
        let deleted_at_head: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(head_dir.join("deleted.json"))?)
                .unwrap_or_default();
        let current = self.backend.scan(&params.session_id)?;
        let current_paths: HashSet<&str> = current.iter().map(|e| e.path.as_str()).collect();
        let to_remove: Vec<String> = deleted_at_head
            .into_iter()
            .filter(|p| !current_paths.contains(p.as_str()))
            .collect();
        let result = self
            .backend
            .abort_changes(&params.session_id, &head, &head_dir, to_remove)?;
        save_snapshot(&store_dir.join(BASE_SCAN_FILE), &head_scan)?;
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

    fn build_changes(
        &self,
        from_dir: Option<&Path>,
        from_entries: &[ScanEntry],
        to_entries: &[ScanEntry],
        to_upper: &Path,
    ) -> Result<Vec<Change>> {
        let diffs = diff_snapshots(from_entries, to_entries);
        let mut out = Vec::with_capacity(diffs.len());
        for diff in diffs {
            let change = match diff.kind {
                ChangeKind::Added => {
                    let new = self.read_new_bytes(to_upper, &diff.path)?;
                    crate::snapshot::describe(
                        &diff.path,
                        ChangeKind::Added,
                        None,
                        diff.new_size,
                        None,
                        new.as_deref(),
                    )
                }
                ChangeKind::Deleted => {
                    let old = self.read_old_bytes(&diff.path, from_dir)?;
                    crate::snapshot::describe(
                        &diff.path,
                        ChangeKind::Deleted,
                        diff.old_size,
                        None,
                        old.as_deref(),
                        None,
                    )
                }
                ChangeKind::Modified => {
                    let old = self.read_old_bytes(&diff.path, from_dir)?;
                    let new = self.read_new_bytes(to_upper, &diff.path)?;
                    crate::snapshot::describe(
                        &diff.path,
                        ChangeKind::Modified,
                        diff.old_size,
                        diff.new_size,
                        old.as_deref(),
                        new.as_deref(),
                    )
                }
            };
            out.push(change);
        }
        Ok(out)
    }

    fn read_old_bytes(&self, rel: &str, from_dir: Option<&Path>) -> Result<Option<Vec<u8>>> {
        if let Some(dir) = from_dir {
            let checkpoint_upper = dir.join("upper").join(rel);
            if checkpoint_upper.exists() {
                return read_capped_file(&checkpoint_upper);
            }
        }
        read_capped_file(&self.project.join(rel))
    }

    fn read_new_bytes(&self, upper: &Path, rel: &str) -> Result<Option<Vec<u8>>> {
        let upper_path = upper.join(rel);
        if upper_path.exists() {
            return read_capped_file(&upper_path);
        }
        read_capped_file(&self.project.join(rel))
    }

    fn previous_snapshot(&self, store_dir: &Path) -> Result<Vec<ScanEntry>> {
        let manifest = CheckpointStore::open(store_dir, DEFAULT_CHECKPOINT_CAP)?;
        if let Some(head) = manifest.head() {
            let dir = store_dir.join("checkpoints").join(head);
            Ok(load_snapshot(&dir.join("scan.json"))?
                .map(|s| s.entries)
                .unwrap_or_default())
        } else {
            Ok(load_snapshot(&store_dir.join(BASE_SCAN_FILE))?
                .map(|s| s.entries)
                .unwrap_or_default())
        }
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
        self.sync_journal(&mut journal)
    }

    fn sync_journal(&self, journal: &mut Journal) -> Result<()> {
        if self.durability == Durability::Fsync {
            journal.flush_sync()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SandboxBackend;
    use crate::worker::{ScanEntry, ScanEntryKind};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestBackend {
        specs: Arc<Mutex<HashMap<String, SessionSpec>>>,
        pending: Arc<Mutex<usize>>,
        committed: Arc<Mutex<usize>>,
        aborted: Arc<Mutex<usize>>,
        fail_scan: Arc<Mutex<bool>>,
    }

    impl TestBackend {
        fn fake_entries(&self, count: usize) -> Vec<ScanEntry> {
            (0..count)
                .map(|i| ScanEntry {
                    path: format!("f{i}.txt"),
                    size: 1,
                    mtime_ns: i as i64,
                    kind: ScanEntryKind::File,
                })
                .collect()
        }
    }

    impl SandboxBackend for TestBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        fn is_supported(&self) -> bool {
            true
        }
        fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()> {
            self.specs
                .lock()
                .unwrap()
                .insert(session_id.to_string(), spec.clone());
            Ok(())
        }
        fn stop_session(&mut self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn ping(&mut self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn exec(&mut self, _session_id: &str, req: ExecRequest) -> Result<ExecResult> {
            *self.pending.lock().unwrap() += 1;
            Ok(ExecResult {
                exit_code: Some(0),
                stdout: req.argv.join(" "),
                stderr: String::new(),
                duration_ms: 1,
                truncated: false,
            })
        }
        fn read_file(&mut self, _session_id: &str, _path: &Path) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn write_file(&mut self, _session_id: &str, _path: &Path, _content: Vec<u8>) -> Result<()> {
            Ok(())
        }
        fn scan(&mut self, _session_id: &str) -> Result<Vec<ScanEntry>> {
            if *self.fail_scan.lock().unwrap() {
                return Err(Error::Backend("injected scan failure".into()));
            }
            Ok(self.fake_entries(*self.pending.lock().unwrap()))
        }
        fn current_upper(&mut self, _session_id: &str) -> Result<PathBuf> {
            Ok(PathBuf::from("/tmp/test-upper"))
        }
        fn freeze_upper(
            &mut self,
            _session_id: &str,
            dest_dir: &Path,
            _prev_upper: Option<&Path>,
        ) -> Result<()> {
            std::fs::create_dir_all(dest_dir.join("upper"))?;
            Ok(())
        }
        fn commit_changes(
            &mut self,
            _session_id: &str,
            checkpoint_id: &str,
            _head_dir: &Path,
            _deleted: Vec<String>,
        ) -> Result<CommitResult> {
            *self.committed.lock().unwrap() += 1;
            *self.pending.lock().unwrap() = 0;
            Ok(CommitResult {
                checkpoint_id: checkpoint_id.to_string(),
                applied: 1,
            })
        }
        fn abort_changes(
            &mut self,
            _session_id: &str,
            checkpoint_id: &str,
            _head_dir: &Path,
            _deleted: Vec<String>,
        ) -> Result<AbortResult> {
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
                label: Some("greeting".into()),
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

        let diff = manager
            .diff(DiffParams {
                session_id: info.session_id.clone(),
                from: None,
                to: None,
            })
            .unwrap();
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].path, "f0.txt");

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
        let mut reopened =
            SessionManager::new(dir.path(), Box::new(TestBackend::default())).unwrap();
        let status = reopened.status(&info.session_id).unwrap();
        assert_eq!(status.status, SessionStatus::Running);
    }

    #[test]
    fn status_propagates_backend_scan_failure() {
        let dir = tempfile::tempdir().unwrap();
        let backend = TestBackend::default();
        let fail_scan = Arc::clone(&backend.fail_scan);
        let mut manager = SessionManager::new(dir.path(), Box::new(backend)).unwrap();
        let info = manager.start(None).unwrap();
        *fail_scan.lock().unwrap() = true;

        let error = manager.status(&info.session_id).unwrap_err();
        assert!(error.to_string().contains("injected scan failure"));
    }
}

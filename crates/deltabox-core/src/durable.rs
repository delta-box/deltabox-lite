//! Service-free durable sessions.
//!
//! The session store, not a process, is authoritative. Each CLI invocation
//! opens the durable working tree, briefly launches a confined backend for
//! execution, materializes its COW result into the working tree, and exits.

use crate::backend::SandboxBackend;
use crate::changeset::{Change, ChangeKind};
use crate::checkpoint::{CheckpointStore, DEFAULT_CHECKPOINT_CAP};
use crate::error::{Error, Result};
use crate::journal::{Journal, JournalEntry, JournalKind};
use crate::policy::Policy;
use crate::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, ExecParams, ExecRequest, ExecResult, FilesystemInfo,
    FsReadResult, ReviewBeginParams, SessionInfo, SessionSpec, SessionStatus,
};
use crate::session::{now_ms, SessionMeta, SessionRegistry};
use crate::snapshot::{
    clone_or_copy_file, diff_snapshots, load_snapshot, preserve_symlink_times, read_capped_file,
    save_snapshot,
};
use crate::worker::{is_excluded_dir, ScanEntry, ScanEntryKind};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const ACCEPTED_DIR: &str = "accepted";
const WORKING_DIR: &str = "working";
const CHECKPOINT_TREE_DIR: &str = "tree";
const LOCK_FILE: &str = "session.lock";
const SESSION_META_FILE: &str = "session.json";
const ACTIVE_REVIEW_FILE: &str = "active_review.json";
const ACCEPTED_SCAN_FILE: &str = "accepted_scan.json";
const WORKING_SCAN_FILE: &str = "working_scan.json";
const TRANSACTION_FILE: &str = "transaction.json";
const TRANSACTIONS_DIR: &str = "transactions";
const MAX_FS_READ_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TransactionKind {
    Working,
    Commit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TransactionPhase {
    Building,
    Prepared,
    WorkingMoved,
    Executing,
    Swapped,
    HostApplied,
    StateUpdated,
    Journaled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionMarker {
    id: String,
    kind: TransactionKind,
    phase: TransactionPhase,
    #[serde(default)]
    paths: Vec<TransactionPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boundary: Option<TransactionBoundary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionBoundary {
    kind: JournalKind,
    data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionPath {
    path: String,
    existed: bool,
}

pub struct DurableSessionManager {
    project: PathBuf,
    root: PathBuf,
    worker_bin: Option<PathBuf>,
}

impl DurableSessionManager {
    pub fn new(project: &Path, worker_bin: Option<PathBuf>) -> Result<Self> {
        let project = project.canonicalize()?;
        let root = project.join(".deltabox");
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            project,
            root,
            worker_bin,
        })
    }

    pub fn start(&self, policy_path: Option<&Path>) -> Result<SessionInfo> {
        self.start_with_filesystem(policy_path, FilesystemInfo::default())
    }

    pub fn start_with_filesystem(
        &self,
        policy_path: Option<&Path>,
        filesystem: FilesystemInfo,
    ) -> Result<SessionInfo> {
        let session_id = Uuid::new_v4().to_string();
        let store = self.store_dir(&session_id);
        std::fs::create_dir_all(&store)?;
        let lock = SessionLock::acquire(&store)?;
        let policy_path = policy_path
            .map(|path| {
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.project.join(path)
                };
                path.canonicalize()
            })
            .transpose()?;
        if let Some(path) = &policy_path {
            Policy::load(path)?;
        }
        copy_project_tree(&self.project, &store.join(ACCEPTED_DIR))?;
        copy_project_tree(&store.join(ACCEPTED_DIR), &store.join(WORKING_DIR))?;
        self.refresh_indexes(&store)?;
        CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP)?;
        self.append_sync(
            &session_id,
            JournalKind::SessionStart,
            Some(serde_json::json!({"engine": "durable-service-free"})),
        )?;
        let meta = SessionMeta {
            session_id: session_id.clone(),
            project: self.project.clone(),
            policy_path,
            started_at_ms: now_ms(),
            status: SessionStatus::Running,
            filesystem,
        };
        save_session_meta(&store, &meta)?;
        let mut registry = SessionRegistry::load(&self.root)?;
        registry.upsert(meta);
        registry.save(&self.root)?;
        drop(lock);
        self.status(&session_id)
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        let _lock = SessionLock::acquire(&self.store_dir(session_id))?;
        self.recover(session_id)?;
        let meta = self.require_meta(session_id)?;
        let store = self.store_dir(session_id);
        let manifest = CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP)?;
        let pending_changes = if meta.status == SessionStatus::Running {
            diff_snapshots(&self.accepted_scan(&store)?, &self.working_scan(&store)?).len()
        } else {
            0
        };
        Ok(SessionInfo {
            session_id: meta.session_id,
            project: meta.project,
            started_at_ms: meta.started_at_ms,
            status: meta.status,
            checkpoint_count: manifest.list().len(),
            pending_changes,
            filesystem: meta.filesystem,
        })
    }

    pub fn stop(&self, session_id: &str) -> Result<SessionInfo> {
        let lock = SessionLock::acquire(&self.store_dir(session_id))?;
        self.recover(session_id)?;
        let mut meta = self.require_meta(session_id)?;
        meta.status = SessionStatus::Stopped;
        save_session_meta(&self.store_dir(session_id), &meta)?;
        let mut registry = SessionRegistry::load(&self.root)?;
        registry.upsert(meta);
        registry.save(&self.root)?;
        self.append_sync(session_id, JournalKind::SessionStop, None)?;
        drop(lock);
        self.status(session_id)
    }

    pub fn remove_stopped(&self, session_id: &str) -> Result<()> {
        let store = self.store_dir(session_id);
        let lock = SessionLock::acquire(&store)?;
        let meta = self.require_meta(session_id)?;
        if meta.status != SessionStatus::Stopped {
            return Err(Error::SessionBusy(session_id.into()));
        }
        let mut registry = SessionRegistry::load(&self.root)?;
        registry.remove(session_id);
        registry.save(&self.root)?;
        drop(lock);
        std::fs::remove_dir_all(store)?;
        Ok(())
    }

    pub fn exec(&self, params: ExecParams) -> Result<ExecResult> {
        let _lock = self.lock_running(&params.session_id)?;
        let working = self.working_dir(&params.session_id);
        let cwd = params
            .cwd
            .as_deref()
            .map(|p| map_project_path(&self.project, &working, p))
            .transpose()?;
        let request = ExecRequest {
            argv: params.argv,
            cwd,
            env: params.env,
        };
        let result = self.run_and_materialize(&params.session_id, |backend, sid| {
            backend.exec(sid, request)
        })?;
        self.append(
            &params.session_id,
            JournalKind::Exec,
            Some(serde_json::json!({"label": params.label, "result": result})),
        )?;
        Ok(result)
    }

    pub fn read_file(&self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        let _lock = self.lock_running(session_id)?;
        let policy = self.policy(session_id)?;
        let rel = relative_project_path(&self.project, path)?;
        if policy.path_is_denied(&rel) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        let working = self.working_dir(session_id);
        let bytes = match resolve_read_path(&working, &rel, &policy)? {
            Some(path) => {
                let meta = std::fs::metadata(&path)?;
                if !meta.is_file() {
                    return Err(Error::Backend(
                        "fs.read target is not a regular file".into(),
                    ));
                }
                if meta.len() > MAX_FS_READ_BYTES {
                    return Err(Error::Backend("fs.read file exceeds 16 MiB limit".into()));
                }
                std::fs::read(path)?
            }
            None => Vec::new(),
        };
        Ok(FsReadResult {
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            size: bytes.len(),
        })
    }

    pub fn write_file(&self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        let _lock = self.lock_running(session_id)?;
        let policy = self.policy(session_id)?;
        let rel = relative_project_path(&self.project, path)?;
        if policy.path_is_denied(&rel) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        if policy
            .limits
            .max_write_bytes
            .is_some_and(|limit| content.len() as u64 > limit)
        {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        let accepted_scan = self.accepted_scan(&self.store_dir(session_id))?;
        self.update_working_transaction(session_id, |candidate| {
            let target = candidate.join(&rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            remove_path(&target)?;
            std::fs::write(&target, content)?;
            let candidate_scan = scan_tree(candidate)?;
            validate_pending_scan(&policy, &accepted_scan, &candidate_scan)?;
            Ok(())
        })?;
        self.append(
            session_id,
            JournalKind::Write,
            Some(serde_json::json!({"path": rel})),
        )
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        let _lock = self.lock_running(&params.session_id)?;
        self.checkpoint_locked(params)
    }

    fn checkpoint_locked(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        self.checkpoint_locked_with_cap(params, DEFAULT_CHECKPOINT_CAP)
    }

    fn checkpoint_locked_with_cap(
        &self,
        params: CheckpointParams,
        checkpoint_cap: usize,
    ) -> Result<CheckpointInfo> {
        let store = self.store_dir(&params.session_id);
        let accepted = self.accepted_scan(&store)?;
        let working = self.working_scan(&store)?;
        let mut manifest = CheckpointStore::open(&store, checkpoint_cap)?;
        let checkpoint_id = Uuid::new_v4().to_string();
        let checkpoint_root = store.join("checkpoints").join(&checkpoint_id);
        let checkpoint_tmp = store
            .join("checkpoints")
            .join(format!(".{checkpoint_id}.tmp"));
        copy_project_tree(
            &store.join(WORKING_DIR),
            &checkpoint_tmp.join(CHECKPOINT_TREE_DIR),
        )?;
        save_snapshot(&checkpoint_tmp.join("scan.json"), &working)?;
        sync_dir(&checkpoint_tmp)?;
        std::fs::rename(&checkpoint_tmp, &checkpoint_root)?;
        sync_dir(&store.join("checkpoints"))?;
        let protected = self
            .load_active_review(&params.session_id)?
            .map(|review| review.checkpoint_id);
        let meta = manifest.create_with_id_protected(
            checkpoint_id,
            params.label.as_deref(),
            diff_snapshots(&accepted, &working).len(),
            protected.as_deref(),
        )?;
        for id in manifest.take_pruned() {
            remove_path(&store.join("checkpoints").join(id))?;
        }
        sync_dir(&store.join("checkpoints"))?;
        self.append_sync(
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

    pub fn diff(&self, params: DiffParams) -> Result<DiffResult> {
        let _lock = self.lock_running(&params.session_id)?;
        self.diff_locked(params)
    }

    fn diff_locked(&self, params: DiffParams) -> Result<DiffResult> {
        let store = self.store_dir(&params.session_id);
        let from = params
            .from
            .as_deref()
            .map(|id| self.checkpoint_tree(&store, id))
            .transpose()?
            .unwrap_or_else(|| store.join(ACCEPTED_DIR));
        let to = params
            .to
            .as_deref()
            .map(|id| self.checkpoint_tree(&store, id))
            .transpose()?
            .unwrap_or_else(|| store.join(WORKING_DIR));
        let from_scan = self.scan_for_tree(&store, &from)?;
        let to_scan = self.scan_for_tree(&store, &to)?;
        let policy = self.policy(&params.session_id)?;
        let changes =
            describe_tree_diff_with_scans(&from, &to, &from_scan, &to_scan, Some(&policy))?;
        Ok(DiffResult {
            from: params.from,
            to: params.to,
            pending_changes: changes.len(),
            changes,
        })
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        let _lock = self.lock_running(&params.session_id)?;
        self.commit_locked(params)
    }

    fn commit_locked(&self, params: CommitParams) -> Result<CommitResult> {
        let store = self.store_dir(&params.session_id);
        let accepted = store.join(ACCEPTED_DIR);
        let working = store.join(WORKING_DIR);
        let expected = self.accepted_scan(&store)?;
        let working_scan = self.working_scan(&store)?;
        let policy = self.policy(&params.session_id)?;
        validate_pending_scan(&policy, &expected, &working_scan)?;
        validate_tree_symlinks(&working, &policy)?;
        let actual = scan_tree(&self.project)?;
        if !diff_snapshots(&expected, &actual).is_empty() {
            return Err(Error::ProjectConflict(
                "real project no longer matches the accepted baseline".into(),
            ));
        }
        let transaction_id = Uuid::new_v4().to_string();
        let txn = store.join(TRANSACTIONS_DIR).join(&transaction_id);
        let host_backup = txn.join("host_backup");
        let working_backup = txn.join("working_backup");
        std::fs::create_dir_all(&txn)?;
        copy_project_tree(&self.project, &host_backup)?;
        link_project_tree(&working, &working_backup)?;
        self.save_transaction(
            &store,
            &TransactionMarker {
                id: transaction_id.clone(),
                kind: TransactionKind::Commit,
                phase: TransactionPhase::Prepared,
                paths: Vec::new(),
                boundary: None,
            },
        )?;
        maybe_crash("commit_prepared");
        let applied = apply_tree_delta(&accepted, &working, &self.project)?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Commit,
            TransactionPhase::HostApplied,
        )?;
        maybe_crash("commit_host_applied");
        copy_project_tree(&self.project, &accepted)?;
        copy_project_tree(&accepted, &working)?;
        self.refresh_indexes(&store)?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Commit,
            TransactionPhase::StateUpdated,
        )?;
        maybe_crash("commit_state_updated");
        let id = params.checkpoint.unwrap_or_else(|| "accepted".into());
        self.append_sync(
            &params.session_id,
            JournalKind::Commit,
            Some(serde_json::json!({
                "checkpoint_id": id,
                "applied": applied,
                "transaction_id": transaction_id,
            })),
        )?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Commit,
            TransactionPhase::Journaled,
        )?;
        self.save_active_review(&params.session_id, None)?;
        self.finish_transaction(&store, &txn)?;
        Ok(CommitResult {
            checkpoint_id: id,
            applied,
        })
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        let _lock = self.lock_running(&params.session_id)?;
        self.abort_locked(params)
    }

    fn abort_locked(&self, params: AbortParams) -> Result<AbortResult> {
        let store = self.store_dir(&params.session_id);
        let source = params
            .checkpoint
            .as_deref()
            .map(|id| self.checkpoint_tree(&store, id))
            .transpose()?
            .unwrap_or_else(|| store.join(ACCEPTED_DIR));
        let restored = describe_tree_diff(&source, &store.join(WORKING_DIR))?.len();
        let id = params.checkpoint.unwrap_or_else(|| "accepted".into());
        self.update_working_transaction_with_boundary(
            &params.session_id,
            |candidate| {
                copy_project_tree(&source, candidate)?;
                Ok(())
            },
            Some(TransactionBoundary {
                kind: JournalKind::Abort,
                data: serde_json::json!({"checkpoint_id": id.clone(), "restored": restored}),
            }),
        )?;
        Ok(AbortResult {
            checkpoint_id: id,
            restored,
        })
    }

    pub fn begin_review(&self, params: ReviewBeginParams) -> Result<ActiveReview> {
        let _lock = self.lock_running(&params.session_id)?;
        if let Some(active) = self.load_active_review(&params.session_id)? {
            return Ok(active);
        }
        let checkpoint = self.checkpoint_locked(CheckpointParams {
            session_id: params.session_id.clone(),
            label: params.label.clone(),
        })?;
        let active = ActiveReview {
            checkpoint_id: checkpoint.id,
            kind: params.kind,
            label: params.label,
            started_at_ms: now_ms(),
        };
        self.save_active_review(&params.session_id, Some(&active))?;
        self.append_sync(
            &params.session_id,
            JournalKind::ReviewBegin,
            Some(serde_json::to_value(&active).unwrap_or_default()),
        )?;
        Ok(active)
    }

    pub fn review_status(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let _lock = self.lock_running(session_id)?;
        self.load_active_review(session_id)
    }

    pub fn review_diff(&self, session_id: &str) -> Result<DiffResult> {
        let _lock = self.lock_running(session_id)?;
        let active = self.require_active_review(session_id)?;
        self.diff_locked(DiffParams {
            session_id: session_id.to_string(),
            from: Some(active.checkpoint_id),
            to: None,
        })
    }

    pub fn review_commit(&self, session_id: &str) -> Result<CommitResult> {
        let _lock = self.lock_running(session_id)?;
        self.require_active_review(session_id)?;
        let result = self.commit_locked(CommitParams {
            session_id: session_id.to_string(),
            checkpoint: None,
        })?;
        self.save_active_review(session_id, None)?;
        Ok(result)
    }

    pub fn review_abort(&self, session_id: &str) -> Result<AbortResult> {
        let _lock = self.lock_running(session_id)?;
        let active = self.require_active_review(session_id)?;
        let result = self.abort_locked(AbortParams {
            session_id: session_id.to_string(),
            checkpoint: Some(active.checkpoint_id),
        })?;
        self.save_active_review(session_id, None)?;
        Ok(result)
    }

    pub fn review_skip(&self, session_id: &str) -> Result<ActiveReview> {
        let _lock = self.lock_running(session_id)?;
        let active = self.require_active_review(session_id)?;
        self.append_sync(
            session_id,
            JournalKind::ReviewSkip,
            Some(serde_json::to_value(&active).unwrap_or_default()),
        )?;
        Ok(active)
    }

    fn run_and_materialize<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&mut dyn SandboxBackend, &str) -> Result<T>,
    ) -> Result<T> {
        let store = self.store_dir(session_id);
        let before = self.working_scan(&store)?;
        let accepted = self.accepted_scan(&store)?;
        let policy = self.policy(session_id)?;
        self.update_working_copy_transaction(session_id, |candidate| {
            let spec = SessionSpec {
                project: candidate.to_path_buf(),
                host_project: Some(self.project.clone()),
                store_dir: store.join("runtime"),
                policy: policy.clone(),
                resolved_deny_paths: None,
            };
            let mut backend = make_direct_backend(self.worker_bin.clone());
            backend.start_session(session_id, &spec)?;
            let result = operation(backend.as_mut(), session_id);
            let current = backend.scan(session_id);
            let stop = backend.stop_session(session_id);
            let value = result?;
            let current = current?;
            stop?;

            let changed_paths: HashSet<String> = diff_snapshots(&before, &current)
                .into_iter()
                .map(|change| change.path)
                .collect();
            validate_materialized_result(&policy, &accepted, &current, candidate, &changed_paths)?;
            Ok(value)
        })
    }

    fn lock_running(&self, session_id: &str) -> Result<SessionLock> {
        let lock = SessionLock::acquire(&self.store_dir(session_id))?;
        self.recover(session_id)?;
        let meta = self.require_meta(session_id)?;
        if meta.status != SessionStatus::Running {
            return Err(Error::SessionStopped(session_id.into()));
        }
        Ok(lock)
    }

    fn policy(&self, session_id: &str) -> Result<Policy> {
        match self.require_meta(session_id)?.policy_path {
            Some(path) => Policy::load(&path),
            None => Ok(Policy::default_agent()),
        }
    }

    fn require_meta(&self, session_id: &str) -> Result<SessionMeta> {
        let path = self.store_dir(session_id).join(SESSION_META_FILE);
        if path.exists() {
            return Ok(serde_json::from_slice(&std::fs::read(path)?)?);
        }
        SessionRegistry::load(&self.root)?
            .get(session_id)
            .cloned()
            .ok_or_else(|| Error::SessionNotFound(session_id.into()))
    }

    fn load_active_review(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let path = self.store_dir(session_id).join(ACTIVE_REVIEW_FILE);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
    }

    fn require_active_review(&self, session_id: &str) -> Result<ActiveReview> {
        self.load_active_review(session_id)?
            .ok_or_else(|| Error::ReviewNotActive(session_id.to_string()))
    }

    fn save_active_review(&self, session_id: &str, active: Option<&ActiveReview>) -> Result<()> {
        let store = self.store_dir(session_id);
        let path = store.join(ACTIVE_REVIEW_FILE);
        match active {
            Some(active) => {
                let tmp = store.join(format!("{ACTIVE_REVIEW_FILE}.tmp"));
                let mut file = File::create(&tmp)?;
                file.write_all(&serde_json::to_vec_pretty(active)?)?;
                file.sync_all()?;
                std::fs::rename(tmp, path)?;
                sync_dir(&store)?;
            }
            None => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        sync_dir(&store)?;
        Ok(())
    }

    fn update_working_transaction<T>(
        &self,
        session_id: &str,
        update: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        self.update_working_transaction_with_boundary(session_id, update, None)
    }

    fn update_working_copy_transaction<T>(
        &self,
        session_id: &str,
        update: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let store = self.store_dir(session_id);
        let transaction_id = Uuid::new_v4().to_string();
        let txn = store.join(TRANSACTIONS_DIR).join(&transaction_id);
        let candidate = txn.join("candidate");
        let working = store.join(WORKING_DIR);
        let backup = txn.join("working_backup");
        std::fs::create_dir_all(&txn)?;
        copy_project_tree(&working, &candidate)?;
        self.save_transaction(
            &store,
            &TransactionMarker {
                id: transaction_id.clone(),
                kind: TransactionKind::Working,
                phase: TransactionPhase::Building,
                paths: Vec::new(),
                boundary: None,
            },
        )?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::Prepared,
        )?;
        std::fs::rename(&working, &backup)?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::WorkingMoved,
        )?;
        maybe_crash("working_moved");
        std::fs::rename(&candidate, &working)?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::Executing,
        )?;

        let value = match update(&working) {
            Ok(value) => value,
            Err(error) => {
                remove_path(&working)?;
                std::fs::rename(&backup, &working)?;
                self.finish_transaction(&store, &txn)?;
                return Err(error);
            }
        };
        maybe_crash("exec_candidate_complete");
        let scan = scan_tree(&working)?;
        self.set_transaction_phase(
            &store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::Swapped,
        )?;
        maybe_crash("working_swapped");
        save_snapshot(&store.join(WORKING_SCAN_FILE), &scan)?;
        self.finish_transaction(&store, &txn)?;
        Ok(value)
    }

    fn update_working_transaction_with_boundary<T>(
        &self,
        session_id: &str,
        update: impl FnOnce(&Path) -> Result<T>,
        boundary: Option<TransactionBoundary>,
    ) -> Result<T> {
        self.update_working_transaction_inner(session_id, update, boundary)
    }

    fn update_working_transaction_inner<T>(
        &self,
        session_id: &str,
        update: impl FnOnce(&Path) -> Result<T>,
        boundary: Option<TransactionBoundary>,
    ) -> Result<T> {
        let store = self.store_dir(session_id);
        let transaction_id = Uuid::new_v4().to_string();
        let txn = store.join(TRANSACTIONS_DIR).join(&transaction_id);
        let candidate = txn.join("candidate");
        std::fs::create_dir_all(&txn)?;
        link_project_tree(&store.join(WORKING_DIR), &candidate)?;
        self.save_transaction(
            &store,
            &TransactionMarker {
                id: transaction_id,
                kind: TransactionKind::Working,
                phase: TransactionPhase::Building,
                paths: Vec::new(),
                boundary,
            },
        )?;
        let value = match update(&candidate) {
            Ok(value) => value,
            Err(error) => {
                self.finish_transaction(&store, &txn)?;
                return Err(error);
            }
        };
        let scan = scan_tree(&candidate)?;
        self.swap_working_candidate(session_id, &store, &txn, &scan)?;
        Ok(value)
    }

    fn swap_working_candidate(
        &self,
        session_id: &str,
        store: &Path,
        txn: &Path,
        scan: &[ScanEntry],
    ) -> Result<()> {
        let transaction_id = txn
            .file_name()
            .ok_or_else(|| Error::Backend("transaction has no id".into()))?
            .to_string_lossy()
            .into_owned();
        let working = store.join(WORKING_DIR);
        let backup = txn.join("working_backup");
        let candidate = txn.join("candidate");
        self.set_transaction_phase(
            store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::Prepared,
        )?;
        maybe_crash("working_prepared");
        std::fs::rename(&working, &backup)?;
        self.set_transaction_phase(
            store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::WorkingMoved,
        )?;
        maybe_crash("working_moved");
        std::fs::rename(&candidate, &working)?;
        self.set_transaction_phase(
            store,
            &transaction_id,
            TransactionKind::Working,
            TransactionPhase::Swapped,
        )?;
        maybe_crash("working_swapped");
        save_snapshot(&store.join(WORKING_SCAN_FILE), scan)?;
        let marker: TransactionMarker =
            serde_json::from_slice(&std::fs::read(store.join(TRANSACTION_FILE))?)?;
        if let Some(boundary) = marker.boundary {
            self.append_sync(
                session_id,
                boundary.kind.clone(),
                Some(transaction_data(boundary.data, &transaction_id)),
            )?;
            if boundary.kind == JournalKind::Abort {
                clear_active_review(store)?;
            }
            self.set_transaction_phase(
                store,
                &transaction_id,
                TransactionKind::Working,
                TransactionPhase::Journaled,
            )?;
        }
        self.finish_transaction(store, txn)
    }

    fn save_transaction(&self, store: &Path, marker: &TransactionMarker) -> Result<()> {
        let path = store.join(TRANSACTION_FILE);
        let tmp = store.join(format!("{TRANSACTION_FILE}.tmp"));
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(marker)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)?;
        sync_dir(store)
    }

    fn set_transaction_phase(
        &self,
        store: &Path,
        id: &str,
        kind: TransactionKind,
        phase: TransactionPhase,
    ) -> Result<()> {
        let path = store.join(TRANSACTION_FILE);
        let mut marker: TransactionMarker = serde_json::from_slice(&std::fs::read(path)?)?;
        if marker.id != id || marker.kind != kind {
            return Err(Error::Backend(
                "transaction marker identity mismatch".into(),
            ));
        }
        marker.phase = phase;
        self.save_transaction(store, &marker)
    }

    fn finish_transaction(&self, store: &Path, txn: &Path) -> Result<()> {
        remove_path(&store.join(TRANSACTION_FILE))?;
        sync_dir(store)?;
        remove_path(txn)?;
        if let Some(parent) = txn.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }

    fn recover(&self, session_id: &str) -> Result<()> {
        let store = self.store_dir(session_id);
        remove_path(&store.join(format!("{TRANSACTION_FILE}.tmp")))?;
        let marker_path = store.join(TRANSACTION_FILE);
        if marker_path.exists() {
            let marker: TransactionMarker = serde_json::from_slice(&std::fs::read(&marker_path)?)?;
            let txn = store.join(TRANSACTIONS_DIR).join(&marker.id);
            match marker.kind {
                TransactionKind::Working => {
                    let published = matches!(
                        marker.phase,
                        TransactionPhase::Swapped | TransactionPhase::Journaled
                    );
                    if !published && !marker.paths.is_empty() {
                        for path in &marker.paths {
                            let target = store.join(WORKING_DIR).join(&path.path);
                            remove_path(&target)?;
                            if path.existed {
                                let source = txn.join("path_backup").join(&path.path);
                                copy_entry(
                                    &source,
                                    &target,
                                    std::fs::symlink_metadata(&source)?.file_type(),
                                )?;
                            }
                        }
                    } else if !published {
                        let working = store.join(WORKING_DIR);
                        let backup = txn.join("working_backup");
                        if backup.exists() {
                            remove_path(&working)?;
                            std::fs::rename(&backup, &working)?;
                        }
                    } else if let Some(boundary) = &marker.boundary {
                        if !self.journal_has_transaction(session_id, &marker.id)? {
                            self.append_sync(
                                session_id,
                                boundary.kind.clone(),
                                Some(transaction_data(boundary.data.clone(), &marker.id)),
                            )?;
                        }
                        if boundary.kind == JournalKind::Abort {
                            clear_active_review(&store)?;
                        }
                    }
                }
                TransactionKind::Commit => {
                    let committed = marker.phase == TransactionPhase::Journaled
                        || (marker.phase == TransactionPhase::StateUpdated
                            && self.journal_has_transaction(session_id, &marker.id)?);
                    if committed {
                        self.save_active_review(session_id, None)?;
                    } else {
                        let host_backup = txn.join("host_backup");
                        let working_backup = txn.join("working_backup");
                        replace_project_tree(&host_backup, &self.project)?;
                        copy_project_tree(&host_backup, &store.join(ACCEPTED_DIR))?;
                        link_project_tree(&working_backup, &store.join(WORKING_DIR))?;
                    }
                }
            }
            self.refresh_indexes(&store)?;
            self.append_sync(
                session_id,
                JournalKind::Recovery,
                Some(serde_json::json!({
                    "transaction_id": marker.id,
                    "kind": marker.kind,
                    "phase": marker.phase,
                })),
            )?;
            self.finish_transaction(&store, &txn)?;
        }
        let transactions = store.join(TRANSACTIONS_DIR);
        if transactions.exists() {
            for entry in std::fs::read_dir(&transactions)? {
                remove_path(&entry?.path())?;
            }
        }
        self.reconcile_checkpoints(&store)?;
        Ok(())
    }

    fn reconcile_checkpoints(&self, store: &Path) -> Result<()> {
        let mut manifest = CheckpointStore::open(store, DEFAULT_CHECKPOINT_CAP)?;
        let known: HashSet<String> = manifest
            .list()
            .into_iter()
            .map(|checkpoint| checkpoint.id.clone())
            .collect();
        let incomplete: Vec<String> = manifest
            .list()
            .into_iter()
            .filter(|checkpoint| {
                !store
                    .join("checkpoints")
                    .join(&checkpoint.id)
                    .join(CHECKPOINT_TREE_DIR)
                    .exists()
            })
            .map(|checkpoint| checkpoint.id.clone())
            .collect();
        for id in incomplete {
            manifest.remove(&id)?;
            remove_path(&store.join("checkpoints").join(id))?;
        }
        let roots = store.join("checkpoints");
        if roots.exists() {
            for entry in std::fs::read_dir(&roots)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !known.contains(&name) {
                    remove_path(&entry.path())?;
                }
            }
        }
        Ok(())
    }

    fn journal_has_transaction(&self, session_id: &str, transaction_id: &str) -> Result<bool> {
        let journal = Journal::open(&self.store_dir(session_id).join("journal.jsonl"))?;
        Ok(journal.tail(16)?.iter().any(|entry| {
            entry
                .data
                .as_ref()
                .and_then(|data| data.get("transaction_id"))
                == Some(&serde_json::Value::String(transaction_id.to_string()))
        }))
    }

    fn accepted_scan(&self, store: &Path) -> Result<Vec<ScanEntry>> {
        self.load_or_scan(&store.join(ACCEPTED_SCAN_FILE), &store.join(ACCEPTED_DIR))
    }

    fn working_scan(&self, store: &Path) -> Result<Vec<ScanEntry>> {
        self.load_or_scan(&store.join(WORKING_SCAN_FILE), &store.join(WORKING_DIR))
    }

    fn load_or_scan(&self, index: &Path, tree: &Path) -> Result<Vec<ScanEntry>> {
        if let Some(snapshot) = load_snapshot(index)? {
            return Ok(snapshot.entries);
        }
        let scan = scan_tree(tree)?;
        save_snapshot(index, &scan)?;
        Ok(scan)
    }

    fn refresh_indexes(&self, store: &Path) -> Result<()> {
        save_snapshot(
            &store.join(ACCEPTED_SCAN_FILE),
            &scan_tree(&store.join(ACCEPTED_DIR))?,
        )?;
        save_snapshot(
            &store.join(WORKING_SCAN_FILE),
            &scan_tree(&store.join(WORKING_DIR))?,
        )
    }

    fn scan_for_tree(&self, store: &Path, tree: &Path) -> Result<Vec<ScanEntry>> {
        if tree == store.join(ACCEPTED_DIR) {
            return self.accepted_scan(store);
        }
        if tree == store.join(WORKING_DIR) {
            return self.working_scan(store);
        }
        let checkpoint = tree
            .parent()
            .ok_or_else(|| Error::Backend("checkpoint tree has no parent".into()))?;
        self.load_or_scan(&checkpoint.join("scan.json"), tree)
    }

    fn checkpoint_tree(&self, store: &Path, id: &str) -> Result<PathBuf> {
        CheckpointStore::open(store, DEFAULT_CHECKPOINT_CAP)?.get(id)?;
        let path = store.join("checkpoints").join(id).join(CHECKPOINT_TREE_DIR);
        if !path.exists() {
            return Err(Error::CheckpointNotFound(id.into()));
        }
        Ok(path)
    }

    fn append(
        &self,
        session_id: &str,
        kind: JournalKind,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut journal = Journal::open(&self.store_dir(session_id).join("journal.jsonl"))?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.into(),
            kind,
            data,
        })
    }

    fn append_sync(
        &self,
        session_id: &str,
        kind: JournalKind,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut journal = Journal::open(&self.store_dir(session_id).join("journal.jsonl"))?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.into(),
            kind,
            data,
        })?;
        journal.flush_sync()
    }

    fn store_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    fn working_dir(&self, session_id: &str) -> PathBuf {
        self.store_dir(session_id).join(WORKING_DIR)
    }
}

fn save_session_meta(store: &Path, meta: &SessionMeta) -> Result<()> {
    let path = store.join(SESSION_META_FILE);
    let tmp = store.join(format!("{SESSION_META_FILE}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(meta)?)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    sync_dir(store)
}

fn transaction_data(mut data: serde_json::Value, transaction_id: &str) -> serde_json::Value {
    match &mut data {
        serde_json::Value::Object(object) => {
            object.insert(
                "transaction_id".into(),
                serde_json::Value::String(transaction_id.into()),
            );
            data
        }
        value => serde_json::json!({
            "value": value.take(),
            "transaction_id": transaction_id,
        }),
    }
}

fn clear_active_review(store: &Path) -> Result<()> {
    remove_path(&store.join(ACTIVE_REVIEW_FILE))?;
    remove_path(&store.join(format!("{ACTIVE_REVIEW_FILE}.tmp")))?;
    sync_dir(store)
}

#[cfg(target_os = "linux")]
fn make_direct_backend(worker_bin: Option<PathBuf>) -> Box<dyn SandboxBackend> {
    Box::new(crate::backend::linux_sandlock::SandlockBackend::new_direct(
        worker_bin,
    ))
}

#[cfg(target_os = "macos")]
fn make_direct_backend(_worker_bin: Option<PathBuf>) -> Box<dyn SandboxBackend> {
    Box::new(crate::backend::macos_seatbelt::SeatbeltBackend::new_direct())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn make_direct_backend(_worker_bin: Option<PathBuf>) -> Box<dyn SandboxBackend> {
    Box::new(crate::backend::DefaultBackend)
}

struct SessionLock {
    path: PathBuf,
    pid: u32,
}

impl SessionLock {
    fn acquire(store: &Path) -> Result<Self> {
        std::fs::create_dir_all(store)?;
        let path = store.join(LOCK_FILE);
        let pid = std::process::id();
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{pid}")?;
                    file.sync_all()?;
                    sync_dir(store)?;
                    return Ok(Self { path, pid });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                    if owner.is_some_and(process_is_alive) {
                        return Err(Error::SessionBusy(store.display().to_string()));
                    }
                    match std::fs::remove_file(&path) {
                        Ok(()) => sync_dir(store)?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::SessionBusy(store.display().to_string()))
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let owned = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            == Some(self.pid);
        if owned && std::fs::remove_file(&self.path).is_ok() {
            if let Some(parent) = self.path.parent() {
                let _ = sync_dir(parent);
            }
        }
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

fn relative_project_path(project: &Path, path: &Path) -> Result<PathBuf> {
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.join(path)
    };
    let full = canonicalize_allow_missing(&full)?;
    full.strip_prefix(project)
        .map(Path::to_path_buf)
        .map_err(|_| Error::InvalidPolicy("path outside project".into()))
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| Error::InvalidPolicy("path has no existing ancestor".into()))?;
        suffix.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| Error::InvalidPolicy("path has no existing ancestor".into()))?;
    }
    let mut resolved = existing.canonicalize()?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn map_project_path(project: &Path, working: &Path, path: &Path) -> Result<PathBuf> {
    Ok(working.join(relative_project_path(project, path)?))
}

fn copy_project_tree(src: &Path, dst: &Path) -> Result<usize> {
    project_tree(src, dst, false)
}

fn link_project_tree(src: &Path, dst: &Path) -> Result<usize> {
    project_tree(src, dst, true)
}

fn project_tree(src: &Path, dst: &Path, hardlink_files: bool) -> Result<usize> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    std::fs::create_dir_all(dst)?;
    let mut copied = 0;
    let walker = walkdir::WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".deltabox");
    for entry in walker {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|_| Error::Backend("copy path escaped source".into()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if hardlink_files && entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::hard_link(entry.path(), &target) {
                Ok(()) => {}
                Err(_) => {
                    copy_entry(entry.path(), &target, entry.file_type()).map_err(|error| {
                        Error::Backend(format!(
                            "copy {} to {}: {error}",
                            entry.path().display(),
                            target.display()
                        ))
                    })?
                }
            }
        } else {
            copy_entry(entry.path(), &target, entry.file_type()).map_err(|error| {
                Error::Backend(format!(
                    "copy {} to {}: {error}",
                    entry.path().display(),
                    target.display()
                ))
            })?;
        }
        if !entry.file_type().is_dir() {
            copied += 1;
        }
    }
    Ok(copied)
}

fn copy_entry(src: &Path, dst: &Path, file_type: std::fs::FileType) -> Result<()> {
    if file_type.is_dir() {
        std::fs::create_dir_all(dst)?;
    } else if file_type.is_symlink() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_path(dst)?;
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        preserve_symlink_times(src, dst)?;
    } else if file_type.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_path(dst)?;
        clone_or_copy_file(src, dst)?;
        let target = File::options().write(true).open(dst)?;
        target.set_modified(std::fs::metadata(src)?.modified()?)?;
        std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn scan_tree(root: &Path) -> Result<Vec<ScanEntry>> {
    let mut entries = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| !is_excluded_dir(&entry.file_name().to_string_lossy()));
    for entry in walker {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let ft = entry.file_type();
        if !(ft.is_file() || ft.is_symlink()) {
            continue;
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        entries.push(ScanEntry {
            path: entry
                .path()
                .strip_prefix(root)
                .map_err(|_| Error::Backend("scan escaped root".into()))?
                .to_string_lossy()
                .into_owned(),
            size: meta.len(),
            mtime_ns,
            kind: if ft.is_symlink() {
                ScanEntryKind::Symlink
            } else {
                ScanEntryKind::File
            },
        });
    }
    Ok(entries)
}

fn validate_pending_scan(
    policy: &Policy,
    accepted: &[ScanEntry],
    working: &[ScanEntry],
) -> Result<()> {
    let diffs = diff_snapshots(accepted, working);
    let mut storage_bytes = 0u64;
    for diff in diffs {
        if policy.path_is_denied(Path::new(&diff.path)) {
            return Err(Error::Backend(format!(
                "path denied by policy: {}",
                diff.path
            )));
        }
        if matches!(diff.kind, ChangeKind::Added | ChangeKind::Modified) {
            storage_bytes = storage_bytes.saturating_add(diff.new_size.unwrap_or(0));
        }
    }
    if policy
        .limits
        .max_storage_bytes
        .is_some_and(|limit| storage_bytes > limit)
    {
        return Err(Error::Backend(format!(
            "pending changes use {storage_bytes} bytes, exceeding max_storage_bytes"
        )));
    }
    Ok(())
}

fn validate_materialized_result(
    policy: &Policy,
    accepted: &[ScanEntry],
    current: &[ScanEntry],
    upper: &Path,
    changed_paths: &HashSet<String>,
) -> Result<()> {
    for path in changed_paths {
        if policy.path_is_denied(Path::new(path)) {
            return Err(Error::Backend(format!("path denied by policy: {path}")));
        }
    }
    validate_pending_scan(policy, accepted, current)?;
    validate_changed_symlinks(upper, policy, changed_paths)
}

fn validate_changed_symlinks(
    root: &Path,
    policy: &Policy,
    changed_paths: &HashSet<String>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for changed in changed_paths {
        let path = root.join(changed);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let rel = Path::new(changed);
        validate_symlink_target(rel, &std::fs::read_link(&path)?, policy)?;
        if let Ok(canonical) = path.canonicalize() {
            let canonical_root = root.canonicalize()?;
            if !canonical.starts_with(&canonical_root) {
                return Err(Error::Backend(format!(
                    "symlink escapes project: {}",
                    rel.display()
                )));
            }
            let target_rel = canonical
                .strip_prefix(&canonical_root)
                .map_err(|_| Error::Backend("symlink target escaped tree".into()))?;
            if policy.path_is_denied(target_rel) {
                return Err(Error::Backend(format!(
                    "symlink targets a denied path: {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_tree_symlinks(root: &Path, policy: &Policy) -> Result<()> {
    let mut paths = HashSet::new();
    if root.exists() {
        for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
            let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
            if entry.file_type().is_symlink() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| Error::Backend("symlink path escaped tree".into()))?;
                paths.insert(rel.to_string_lossy().into_owned());
            }
        }
    }
    validate_changed_symlinks(root, policy, &paths)
}

fn validate_symlink_target(rel: &Path, target: &Path, policy: &Policy) -> Result<()> {
    if target.is_absolute() {
        return Err(Error::Backend(format!(
            "absolute symlink target is not allowed: {}",
            rel.display()
        )));
    }
    let mut normalized = PathBuf::new();
    let combined = rel.parent().unwrap_or_else(|| Path::new("")).join(target);
    for component in combined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => normalized.push(value),
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(Error::Backend(format!(
                        "symlink escapes project: {}",
                        rel.display()
                    )));
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(Error::Backend(format!(
                    "symlink escapes project: {}",
                    rel.display()
                )));
            }
        }
    }
    if policy.path_is_denied(&normalized) {
        return Err(Error::Backend(format!(
            "symlink targets a denied path: {}",
            rel.display()
        )));
    }
    Ok(())
}

fn resolve_read_path(root: &Path, rel: &Path, policy: &Policy) -> Result<Option<PathBuf>> {
    let candidate = root.join(rel);
    match std::fs::symlink_metadata(&candidate) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let canonical_root = root.canonicalize()?;
    let canonical = candidate.canonicalize().map_err(|error| {
        Error::Backend(format!("fs.read cannot resolve {}: {error}", rel.display()))
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(Error::Backend(format!(
            "fs.read symlink escapes project: {}",
            rel.display()
        )));
    }
    let resolved_rel = canonical
        .strip_prefix(&canonical_root)
        .map_err(|_| Error::Backend("fs.read target escaped project".into()))?;
    if policy.path_is_denied(resolved_rel) {
        return Err(Error::Backend("fs.read target denied by policy".into()));
    }
    Ok(Some(canonical))
}

fn read_capped_file_within(
    root: &Path,
    rel: &Path,
    policy: Option<&Policy>,
) -> Result<Option<Vec<u8>>> {
    let path = match policy {
        Some(policy) => match resolve_read_path(root, rel, policy)? {
            Some(path) => path,
            None => return Ok(None),
        },
        None => root.join(rel),
    };
    read_capped_file(&path)
}

fn describe_tree_diff(from: &Path, to: &Path) -> Result<Vec<Change>> {
    describe_tree_diff_with_scans(from, to, &scan_tree(from)?, &scan_tree(to)?, None)
}

fn describe_tree_diff_with_scans(
    from: &Path,
    to: &Path,
    from_scan: &[ScanEntry],
    to_scan: &[ScanEntry],
    policy: Option<&Policy>,
) -> Result<Vec<Change>> {
    let diffs = diff_snapshots(from_scan, to_scan);
    diffs
        .into_iter()
        .filter(|diff| !policy.is_some_and(|policy| policy.path_is_denied(Path::new(&diff.path))))
        .map(|diff| {
            let old = read_capped_file_within(from, Path::new(&diff.path), policy)?;
            let new = read_capped_file_within(to, Path::new(&diff.path), policy)?;
            Ok(crate::snapshot::describe(
                &diff.path,
                diff.kind,
                diff.old_size,
                diff.new_size,
                old.as_deref(),
                new.as_deref(),
            ))
        })
        .collect()
}

fn replace_project_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dst)? {
        let entry = entry?;
        if entry.file_name() != ".deltabox" {
            remove_path(&entry.path())?;
        }
    }
    for entry in walkdir::WalkDir::new(src).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|_| Error::Backend("restore path escaped backup".into()))?;
        copy_entry(entry.path(), &dst.join(rel), entry.file_type())?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn maybe_crash(point: &str) {
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_AT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

fn apply_tree_delta(from: &Path, to: &Path, dest: &Path) -> Result<usize> {
    let diffs = diff_snapshots(&scan_tree(from)?, &scan_tree(to)?);
    for diff in &diffs {
        let target = dest.join(&diff.path);
        match diff.kind {
            ChangeKind::Deleted => remove_path(&target)?,
            ChangeKind::Added | ChangeKind::Modified => {
                let source = to.join(&diff.path);
                copy_entry(
                    &source,
                    &target,
                    std::fs::symlink_metadata(&source)?.file_type(),
                )?;
            }
        }
    }
    Ok(diffs.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_delta_ignore_internal_state() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(src.path().join(".deltabox/x")).unwrap();
        std::fs::write(src.path().join(".deltabox/x/secret"), "x").unwrap();
        let dst = tempfile::tempdir().unwrap();
        copy_project_tree(src.path(), dst.path()).unwrap();
        assert!(dst.path().join("a.txt").exists());
        assert!(!dst.path().join(".deltabox").exists());
    }

    #[test]
    fn default_abort_behavior_restores_accepted_tree() {
        let accepted = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();
        std::fs::write(accepted.path().join("README.md"), "base").unwrap();
        copy_project_tree(accepted.path(), working.path()).unwrap();
        std::fs::write(working.path().join("a.txt"), "pending").unwrap();
        std::fs::remove_file(working.path().join("README.md")).unwrap();
        copy_project_tree(accepted.path(), working.path()).unwrap();
        assert!(!working.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(working.path().join("README.md")).unwrap(),
            "base"
        );
    }

    #[test]
    fn checkpoint_pruning_removes_physical_trees() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("a.txt"), "a").unwrap();
        let manager = DurableSessionManager::new(project.path(), None).unwrap();
        let session = manager.start(None).unwrap();
        let cap = 2;
        for _ in 0..=cap {
            manager
                .checkpoint_locked_with_cap(
                    CheckpointParams {
                        session_id: session.session_id.clone(),
                        label: None,
                    },
                    cap,
                )
                .unwrap();
        }
        let roots = manager.store_dir(&session.session_id).join("checkpoints");
        assert_eq!(std::fs::read_dir(roots).unwrap().count(), cap);
    }

    #[test]
    fn recovery_removes_manifest_entry_without_checkpoint_tree() {
        let project = tempfile::tempdir().unwrap();
        let manager = DurableSessionManager::new(project.path(), None).unwrap();
        let session = manager.start(None).unwrap();
        let store = manager.store_dir(&session.session_id);
        let mut manifest = CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP).unwrap();
        let incomplete = manifest.create(Some("incomplete"), 0).unwrap();

        let status = manager.status(&session.session_id).unwrap();
        assert_eq!(status.checkpoint_count, 0);
        let manifest = CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP).unwrap();
        assert!(manifest.get(&incomplete.id).is_err());
    }

    #[test]
    fn active_review_checkpoint_is_protected_from_cap_pruning() {
        let project = tempfile::tempdir().unwrap();
        let manager = DurableSessionManager::new(project.path(), None).unwrap();
        let session = manager.start(None).unwrap();
        let review = manager
            .begin_review(ReviewBeginParams {
                session_id: session.session_id.clone(),
                kind: crate::protocol::ReviewKind::Task,
                label: None,
            })
            .unwrap();
        let cap = 2;
        for _ in 0..=cap {
            manager
                .checkpoint_locked_with_cap(
                    CheckpointParams {
                        session_id: session.session_id.clone(),
                        label: None,
                    },
                    cap,
                )
                .unwrap();
        }
        let store = manager.store_dir(&session.session_id);
        let manifest = CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP).unwrap();
        assert!(manifest.get(&review.checkpoint_id).is_ok());
        manager.review_abort(&session.session_id).unwrap();
    }

    #[test]
    fn live_process_lock_is_busy_and_stale_lock_recovers() {
        let store = tempfile::tempdir().unwrap();
        std::fs::write(
            store.path().join(LOCK_FILE),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        assert!(matches!(
            SessionLock::acquire(store.path()),
            Err(Error::SessionBusy(_))
        ));

        std::fs::write(store.path().join(LOCK_FILE), "4294967294\n").unwrap();
        let lock = SessionLock::acquire(store.path()).unwrap();
        drop(lock);
        assert!(!store.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn status_propagates_corrupt_scan_index() {
        let project = tempfile::tempdir().unwrap();
        let manager = DurableSessionManager::new(project.path(), None).unwrap();
        let session = manager.start(None).unwrap();
        let index = manager
            .store_dir(&session.session_id)
            .join(WORKING_SCAN_FILE);
        std::fs::write(index, b"not json").unwrap();

        assert!(manager.status(&session.session_id).is_err());
    }
}

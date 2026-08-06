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
    CommitResult, DiffParams, DiffResult, ExecParams, ExecRequest, ExecResult, FsReadResult,
    ReviewBeginParams, SessionInfo, SessionSpec, SessionStatus,
};
use crate::session::{now_ms, SessionMeta, SessionRegistry};
use crate::snapshot::{
    clone_or_copy_file, diff_snapshots, load_snapshot, read_capped_file, save_snapshot,
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
        self.append(
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
        self.append(session_id, JournalKind::SessionStop, None)?;
        drop(lock);
        self.status(session_id)
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
        let bytes = match std::fs::read(self.working_dir(session_id).join(rel)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
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
        self.update_working_transaction(session_id, |candidate| {
            let target = candidate.join(&rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            remove_path(&target)?;
            std::fs::write(&target, content)?;
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
        self.append(
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
        let changes = describe_tree_diff_with_scans(&from, &to, &from_scan, &to_scan)?;
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
        self.update_working_transaction(&params.session_id, |candidate| {
            copy_project_tree(&source, candidate)?;
            Ok(())
        })?;
        let id = params.checkpoint.unwrap_or_else(|| "accepted".into());
        self.append_sync(
            &params.session_id,
            JournalKind::Abort,
            Some(serde_json::json!({"checkpoint_id": id, "restored": restored})),
        )?;
        self.save_active_review(&params.session_id, None)?;
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
        self.append(
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
        self.append(
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
        let working = store.join(WORKING_DIR);
        let spec = SessionSpec {
            project: working.clone(),
            store_dir: store.join("runtime"),
            policy: self.policy(session_id)?,
        };
        let mut backend = make_backend(self.worker_bin.clone());
        backend.start_session(session_id, &spec)?;
        let result = operation(backend.as_mut(), session_id);
        let current = backend.scan(session_id);
        match (result, current) {
            (Ok(value), Ok(current)) => {
                let current_paths: HashSet<&str> =
                    current.iter().map(|e| e.path.as_str()).collect();
                let deleted: Vec<String> = before
                    .iter()
                    .filter(|entry| !current_paths.contains(entry.path.as_str()))
                    .map(|entry| entry.path.clone())
                    .collect();
                let upper = backend.current_upper(session_id)?;
                let mut changed_paths: HashSet<String> = upper_paths(&upper)?.into_iter().collect();
                changed_paths.extend(deleted.iter().cloned());
                if changed_paths.is_empty() {
                    backend.stop_session(session_id)?;
                    return Ok(value);
                }
                let txn_id = Uuid::new_v4().to_string();
                let txn = store.join(TRANSACTIONS_DIR).join(&txn_id);
                let backup = txn.join("path_backup");
                std::fs::create_dir_all(&backup)?;
                let mut changed_paths: Vec<String> = changed_paths.into_iter().collect();
                changed_paths.sort();
                let mut paths = Vec::with_capacity(changed_paths.len());
                for path in changed_paths {
                    let source = working.join(&path);
                    let existed = std::fs::symlink_metadata(&source).is_ok();
                    if existed {
                        let target = backup.join(&path);
                        copy_entry(
                            &source,
                            &target,
                            std::fs::symlink_metadata(&source)?.file_type(),
                        )?;
                    }
                    paths.push(TransactionPath { path, existed });
                }
                self.save_transaction(
                    &store,
                    &TransactionMarker {
                        id: txn_id.clone(),
                        kind: TransactionKind::Working,
                        phase: TransactionPhase::Prepared,
                        paths,
                    },
                )?;
                maybe_crash("materialize_prepared");
                backend.materialize_changes(session_id, &txn.join("materialized"), deleted)?;
                self.set_transaction_phase(
                    &store,
                    &txn_id,
                    TransactionKind::Working,
                    TransactionPhase::Swapped,
                )?;
                maybe_crash("materialize_applied");
                save_snapshot(&store.join(WORKING_SCAN_FILE), &current)?;
                self.finish_transaction(&store, &txn)?;
                Ok(value)
            }
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    fn lock_running(&self, session_id: &str) -> Result<SessionLock> {
        let meta = self.require_meta(session_id)?;
        if meta.status != SessionStatus::Running {
            return Err(Error::SessionStopped(session_id.into()));
        }
        let lock = SessionLock::acquire(&self.store_dir(session_id))?;
        self.recover(session_id)?;
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
                std::fs::write(&tmp, serde_json::to_vec_pretty(active)?)?;
                std::fs::rename(tmp, path)?;
            }
            None => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }

    fn update_working_transaction<T>(
        &self,
        session_id: &str,
        update: impl FnOnce(&Path) -> Result<T>,
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
            },
        )?;
        let value = update(&candidate)?;
        let scan = scan_tree(&candidate)?;
        self.swap_working_candidate(&store, &txn, &scan)?;
        Ok(value)
    }

    fn swap_working_candidate(&self, store: &Path, txn: &Path, scan: &[ScanEntry]) -> Result<()> {
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
        remove_path(txn)
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
                    if marker.phase != TransactionPhase::Swapped && !marker.paths.is_empty() {
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
                    } else if marker.phase != TransactionPhase::Swapped {
                        let working = store.join(WORKING_DIR);
                        let backup = txn.join("working_backup");
                        if backup.exists() {
                            remove_path(&working)?;
                            std::fs::rename(&backup, &working)?;
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
    std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn make_backend(worker_bin: Option<PathBuf>) -> Box<dyn SandboxBackend> {
    Box::new(crate::backend::linux_sandlock::SandlockBackend::new(
        worker_bin,
    ))
}

#[cfg(not(target_os = "linux"))]
fn make_backend(_worker_bin: Option<PathBuf>) -> Box<dyn SandboxBackend> {
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
                    return Ok(Self { path, pid });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                    if owner.is_some_and(process_is_alive) {
                        return Err(Error::SessionBusy(store.display().to_string()));
                    }
                    let _ = std::fs::remove_file(&path);
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
        if owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
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

fn upper_paths(upper: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    if !upper.exists() {
        return Ok(paths);
    }
    for entry in walkdir::WalkDir::new(upper)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        if entry.file_type().is_file() || entry.file_type().is_symlink() {
            let rel = entry
                .path()
                .strip_prefix(upper)
                .map_err(|_| Error::Backend("upper path escaped root".into()))?;
            paths.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(paths)
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

fn describe_tree_diff(from: &Path, to: &Path) -> Result<Vec<Change>> {
    describe_tree_diff_with_scans(from, to, &scan_tree(from)?, &scan_tree(to)?)
}

fn describe_tree_diff_with_scans(
    from: &Path,
    to: &Path,
    from_scan: &[ScanEntry],
    to_scan: &[ScanEntry],
) -> Result<Vec<Change>> {
    let diffs = diff_snapshots(from_scan, to_scan);
    diffs
        .into_iter()
        .map(|diff| {
            let old = read_capped_file(&from.join(&diff.path))?;
            let new = read_capped_file(&to.join(&diff.path))?;
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
    fn default_abort_contract_restores_accepted_tree() {
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
}

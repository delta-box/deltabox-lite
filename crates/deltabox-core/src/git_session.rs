//! Zero-FUSE git-backed sessions: bubblewrap isolation plus git ghost storage.
//!
//! The agent runs directly against the real project tree inside a `bwrap`
//! sandbox; every snapshot is a ghost commit in a private bare side repo that
//! lives outside the project, so nothing session-owned is ever bind-mounted
//! into the sandbox and no `/dev/fuse` or filesystem helper is involved.

use crate::error::{Error, Result};
use crate::file_lock::FileLock;
use crate::policy::Policy;
use crate::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, ExecParams, ExecResult, FilesystemInfo, FilesystemMode,
    FsReadResult, ReviewBeginParams, SessionInfo, SessionStatus,
};
use crate::providers::isolation::BubblewrapIsolation;
use crate::providers::storage::{GitGhostStorage, GHOST_DIFF_BASIS_MAX_BYTES, GHOST_REF_PREFIX};
use crate::providers::{canonicalize_allow_missing, IsolationProvider, StorageProvider};
use crate::session::now_ms;
use crate::state_layout::git_session_base;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SESSION_META_FILE: &str = "session.json";
const ACTIVE_REVIEW_FILE: &str = "active_review.json";
const LOCK_FILE: &str = "session.lock";
const MAX_FS_READ_BYTES: u64 = 16 * 1024 * 1024;

/// Git-backed session record. Authoritative on disk; every CLI or `serve`
/// invocation reloads it, so no daemon is required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSessionMeta {
    pub session_id: String,
    pub project: PathBuf,
    #[serde(default)]
    pub isolation: Option<String>,
    #[serde(default)]
    pub policy_path: Option<PathBuf>,
    #[serde(default)]
    pub baseline_seq: u64,
    #[serde(default)]
    pub pending_seqs: Vec<u64>,
    pub started_at_ms: u64,
    #[serde(default)]
    pub status: SessionStatus,
}

/// Combines bubblewrap isolation with git ghost storage for one project.
pub struct GitSessionManager {
    project: PathBuf,
    isolation: BubblewrapIsolation,
}

impl GitSessionManager {
    pub fn new(project: &Path) -> Result<Self> {
        let project = project.canonicalize()?;
        let isolation = BubblewrapIsolation::new(project.clone());
        Ok(Self { project, isolation })
    }

    /// Whether `session_id` names a git session of this project.
    pub fn is_git_session(&self, session_id: &str) -> Result<bool> {
        let path = self.session_dir(session_id)?.join(SESSION_META_FILE);
        if !path.exists() {
            return Ok(false);
        }
        let meta: GitSessionMeta = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(meta.project == self.project)
    }

    pub fn start(
        &self,
        policy: Option<&Path>,
        isolation: Option<&str>,
        group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        if group_commit_ms.is_some() {
            return Err(Error::InvalidDurability(
                "group_commit_ms requires strict durability".into(),
            ));
        }
        let isolation = match isolation {
            None | Some("bubblewrap") => Ok("bubblewrap"),
            Some(other) => Err(Error::Unsupported(format!(
                "isolation provider {other:?} is not supported; git mode accepts only bubblewrap"
            ))),
        }?;
        if !self.isolation.available() {
            return Err(Error::Unsupported(
                "bubblewrap is unavailable (missing binary or user namespaces denied); install bubblewrap and retry".into(),
            ));
        }
        let policy_path = policy
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
        let session_id = Uuid::new_v4().to_string();
        let dir = self.session_dir(&session_id)?;
        std::fs::create_dir_all(&dir)?;
        let mut storage = self.open_storage(&session_id)?;
        let baseline = storage.snapshot()?;
        let meta = GitSessionMeta {
            session_id: session_id.clone(),
            project: self.project.clone(),
            isolation: Some(isolation.to_string()),
            policy_path,
            baseline_seq: ref_seq(&baseline)?,
            pending_seqs: Vec::new(),
            started_at_ms: now_ms(),
            status: SessionStatus::Running,
        };
        self.save_meta(&meta)?;
        self.info(&meta)
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        let lock = FileLock::acquire(&self.session_dir(session_id)?.join(LOCK_FILE))?;
        let meta = self.require_meta(session_id)?;
        let info = self.info(&meta);
        drop(lock);
        info
    }

    pub fn stop(&self, session_id: &str) -> Result<SessionInfo> {
        let lock = FileLock::acquire(&self.session_dir(session_id)?.join(LOCK_FILE))?;
        let mut meta = self.require_meta(session_id)?;
        let mut storage = self.open_storage(&meta.session_id)?;
        storage.close()?;
        self.save_review(session_id, None)?;
        meta.status = SessionStatus::Stopped;
        self.save_meta(&meta)?;
        drop(lock);
        self.status(session_id)
    }

    pub fn remove_stopped(&self, session_id: &str) -> Result<()> {
        let dir = self.session_dir(session_id)?;
        let lock = FileLock::acquire(&dir.join(LOCK_FILE))?;
        let meta = self.require_meta(session_id)?;
        if meta.status != SessionStatus::Stopped {
            return Err(Error::SessionBusy(session_id.into()));
        }
        drop(lock);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn exec(&self, params: ExecParams) -> Result<ExecResult> {
        let _locked = self.lock_running(&params.session_id)?;
        self.isolation.exec(params.argv, params.cwd, params.env)
    }

    pub fn read_file(&self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        let (_lock, meta) = self.lock_running(session_id)?;
        let policy = self.policy(&meta)?;
        let rel = relative_project_path(&self.project, path)?;
        if policy.path_is_denied(&rel) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        let target = self.project.join(&rel);
        let bytes = if target.exists() {
            let file_meta = std::fs::metadata(&target)?;
            if !file_meta.is_file() {
                return Err(Error::Backend(
                    "fs.read target is not a regular file".into(),
                ));
            }
            if file_meta.len() > MAX_FS_READ_BYTES {
                return Err(Error::Backend("fs.read file exceeds 16 MiB limit".into()));
            }
            std::fs::read(&target)?
        } else {
            Vec::new()
        };
        Ok(FsReadResult {
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            size: bytes.len(),
        })
    }

    pub fn write_file(&self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        let (_lock, meta) = self.lock_running(session_id)?;
        let policy = self.policy(&meta)?;
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
        let target = self.project.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        Ok(())
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        let (_lock, meta) = self.lock_running(&params.session_id)?;
        self.checkpoint_locked(params, meta)
    }

    fn checkpoint_locked(
        &self,
        params: CheckpointParams,
        mut meta: GitSessionMeta,
    ) -> Result<CheckpointInfo> {
        let mut storage = self.open_storage(&meta.session_id)?;
        let change_count = storage.diff(&baseline_ref(&meta))?.pending_changes;
        let ghost_ref = storage.snapshot()?;
        meta.pending_seqs.push(ref_seq(&ghost_ref)?);
        self.save_meta(&meta)?;
        Ok(CheckpointInfo {
            id: ghost_ref,
            label: params.label,
            created_at_ms: now_ms(),
            change_count,
            committed: false,
        })
    }

    pub fn diff(&self, params: DiffParams) -> Result<DiffResult> {
        let (_lock, meta) = self.lock_running(&params.session_id)?;
        let storage = self.open_storage(&meta.session_id)?;
        let from = params.from.unwrap_or_else(|| baseline_ref(&meta));
        match params.to {
            None => storage.diff(&from),
            Some(to) => storage.diff_trees(&from, &to),
        }
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        let (_lock, meta) = self.lock_running(&params.session_id)?;
        self.commit_locked(params, meta)
    }

    fn commit_locked(
        &self,
        params: CommitParams,
        mut meta: GitSessionMeta,
    ) -> Result<CommitResult> {
        let mut storage = self.open_storage(&meta.session_id)?;
        let applied = storage.diff(&baseline_ref(&meta))?.pending_changes;
        let refs = storage.list_refs()?;
        for ghost_ref in refs {
            storage.drop_ref(&ghost_ref)?;
        }
        let fresh = storage.snapshot()?;
        meta.baseline_seq = ref_seq(&fresh)?;
        meta.pending_seqs.clear();
        self.save_meta(&meta)?;
        self.save_review(&params.session_id, None)?;
        Ok(CommitResult {
            checkpoint_id: params.checkpoint.unwrap_or_else(|| "accepted".to_string()),
            applied,
        })
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        let (_lock, meta) = self.lock_running(&params.session_id)?;
        self.abort_locked(params, meta)
    }

    fn abort_locked(&self, params: AbortParams, mut meta: GitSessionMeta) -> Result<AbortResult> {
        let mut storage = self.open_storage(&meta.session_id)?;
        let target = params
            .checkpoint
            .clone()
            .unwrap_or_else(|| baseline_ref(&meta));
        let restored = storage.diff(&target)?.pending_changes;
        storage.restore(&target)?;
        let refs = storage.list_refs()?;
        for ghost_ref in refs {
            storage.drop_ref(&ghost_ref)?;
        }
        let fresh = storage.snapshot()?;
        meta.baseline_seq = ref_seq(&fresh)?;
        meta.pending_seqs.clear();
        self.save_meta(&meta)?;
        self.save_review(&params.session_id, None)?;
        Ok(AbortResult {
            checkpoint_id: target,
            restored,
        })
    }

    pub fn begin_review(&self, params: ReviewBeginParams) -> Result<ActiveReview> {
        let (_lock, meta) = self.lock_running(&params.session_id)?;
        if let Some(active) = self.review(&params.session_id)? {
            return Ok(active);
        }
        let checkpoint = self.checkpoint_locked(
            CheckpointParams {
                session_id: params.session_id.clone(),
                label: params.label.clone(),
            },
            meta,
        )?;
        let active = ActiveReview {
            checkpoint_id: checkpoint.id,
            kind: params.kind,
            label: params.label,
            started_at_ms: now_ms(),
        };
        self.save_review(&params.session_id, Some(&active))?;
        Ok(active)
    }

    pub fn review_status(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let _locked = self.lock_running(session_id)?;
        self.review(session_id)
    }

    pub fn review_diff(&self, session_id: &str) -> Result<DiffResult> {
        let _locked = self.lock_running(session_id)?;
        let active = self.require_review(session_id)?;
        let storage = self.open_storage(session_id)?;
        storage.diff(&active.checkpoint_id)
    }

    pub fn review_commit(&self, session_id: &str) -> Result<CommitResult> {
        let (_lock, meta) = self.lock_running(session_id)?;
        self.require_review(session_id)?;
        self.commit_locked(
            CommitParams {
                session_id: session_id.to_string(),
                checkpoint: None,
            },
            meta,
        )
    }

    pub fn review_abort(&self, session_id: &str) -> Result<AbortResult> {
        let (_lock, meta) = self.lock_running(session_id)?;
        let active = self.require_review(session_id)?;
        self.abort_locked(
            AbortParams {
                session_id: session_id.to_string(),
                checkpoint: Some(active.checkpoint_id),
            },
            meta,
        )
    }

    pub fn review_skip(&self, session_id: &str) -> Result<ActiveReview> {
        let _locked = self.lock_running(session_id)?;
        self.require_review(session_id)
    }

    fn info(&self, meta: &GitSessionMeta) -> Result<SessionInfo> {
        let (checkpoint_count, pending_changes) = if meta.status == SessionStatus::Running {
            let storage = self.open_storage(&meta.session_id)?;
            let pending = storage.diff(&baseline_ref(meta))?.pending_changes;
            (meta.pending_seqs.len(), pending)
        } else {
            (meta.pending_seqs.len(), 0)
        };
        Ok(SessionInfo {
            session_id: meta.session_id.clone(),
            project: meta.project.clone(),
            started_at_ms: meta.started_at_ms,
            status: meta.status.clone(),
            checkpoint_count,
            pending_changes,
            filesystem: git_filesystem_info(meta),
        })
    }

    fn policy(&self, meta: &GitSessionMeta) -> Result<Policy> {
        match &meta.policy_path {
            Some(path) => Policy::load(path),
            None => Ok(Policy::default_agent()),
        }
    }

    fn review(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let path = self.session_dir(session_id)?.join(ACTIVE_REVIEW_FILE);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
    }

    fn require_review(&self, session_id: &str) -> Result<ActiveReview> {
        self.review(session_id)?
            .ok_or_else(|| Error::ReviewNotActive(session_id.to_string()))
    }

    fn save_review(&self, session_id: &str, active: Option<&ActiveReview>) -> Result<()> {
        let dir = self.session_dir(session_id)?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(ACTIVE_REVIEW_FILE);
        let tmp = dir.join(format!("{ACTIVE_REVIEW_FILE}.tmp"));
        match active {
            Some(active) => {
                let mut file = File::create(&tmp)?;
                file.write_all(&serde_json::to_vec_pretty(active)?)?;
                file.sync_all()?;
                std::fs::rename(&tmp, &path)?;
            }
            None => {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                match std::fs::remove_file(&tmp) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        sync_dir(&dir)
    }

    fn require_meta(&self, session_id: &str) -> Result<GitSessionMeta> {
        let path = self.session_dir(session_id)?.join(SESSION_META_FILE);
        let raw = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::SessionNotFound(session_id.into())
            } else {
                Error::Io(error)
            }
        })?;
        let meta: GitSessionMeta = serde_json::from_slice(&raw)?;
        if meta.project != self.project {
            return Err(Error::SessionNotFound(session_id.into()));
        }
        Ok(meta)
    }

    fn save_meta(&self, meta: &GitSessionMeta) -> Result<()> {
        let dir = self.session_dir(&meta.session_id)?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(SESSION_META_FILE);
        let tmp = dir.join(format!("{SESSION_META_FILE}.tmp"));
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(meta)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        sync_dir(&dir)?;
        Ok(())
    }

    fn open_storage(&self, session_id: &str) -> Result<GitGhostStorage> {
        let dir = self.session_dir(session_id)?;
        let tmp = dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        Ok(GitGhostStorage::new(
            self.project.clone(),
            dir.join("undo.git"),
            tmp,
            GHOST_DIFF_BASIS_MAX_BYTES,
        ))
    }

    fn session_dir(&self, session_id: &str) -> Result<PathBuf> {
        Ok(git_session_base()?.join("git_sessions").join(session_id))
    }

    fn lock_running(&self, session_id: &str) -> Result<(FileLock, GitSessionMeta)> {
        let lock = FileLock::acquire(&self.session_dir(session_id)?.join(LOCK_FILE))?;
        let meta = self.require_meta(session_id)?;
        if meta.status != SessionStatus::Running {
            return Err(Error::SessionStopped(session_id.into()));
        }
        Ok((lock, meta))
    }
}

fn git_filesystem_info(meta: &GitSessionMeta) -> FilesystemInfo {
    FilesystemInfo {
        requested_fs_mode: FilesystemMode::Git,
        resolved_fs_mode: FilesystemMode::Git,
        host_visibility: "immediate".into(),
        durability: "process-crash".into(),
        group_commit_ms: None,
        isolation: meta
            .isolation
            .clone()
            .unwrap_or_else(|| "bubblewrap".to_string()),
        state_provider: Some("git-ghost".into()),
        fallback_reason: None,
        rollback_guarantee: "byte-for-byte-restore".into(),
        tainted: false,
        mountpoint: None,
        helper_pid: None,
        helper_restarts: 0,
        mount_namespace: false,
    }
}

fn baseline_ref(meta: &GitSessionMeta) -> String {
    format!("{GHOST_REF_PREFIX}{}", meta.baseline_seq)
}

fn ref_seq(ghost_ref: &str) -> Result<u64> {
    ghost_ref
        .strip_prefix(GHOST_REF_PREFIX)
        .and_then(|suffix| suffix.parse().ok())
        .ok_or_else(|| Error::Backend(format!("invalid ghost ref {ghost_ref:?}")))
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

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

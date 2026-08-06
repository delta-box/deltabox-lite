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
    AbortParams, AbortResult, CheckpointInfo, CheckpointParams, CommitParams, CommitResult,
    DiffParams, DiffResult, ExecParams, ExecRequest, ExecResult, FsReadResult, SessionInfo,
    SessionSpec, SessionStatus,
};
use crate::session::{now_ms, SessionMeta, SessionRegistry};
use crate::snapshot::{diff_snapshots, read_capped_file};
use crate::worker::{is_excluded_dir, ScanEntry, ScanEntryKind};
use base64::Engine;
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
        let meta = self.require_meta(session_id)?;
        let store = self.store_dir(session_id);
        let manifest = CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP)?;
        let pending_changes = if meta.status == SessionStatus::Running {
            diff_snapshots(
                &scan_tree(&store.join(ACCEPTED_DIR))?,
                &scan_tree(&store.join(WORKING_DIR))?,
            )
            .len()
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
        let target = self.working_dir(session_id).join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        self.append(
            session_id,
            JournalKind::Write,
            Some(serde_json::json!({"path": rel})),
        )
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        let _lock = self.lock_running(&params.session_id)?;
        let store = self.store_dir(&params.session_id);
        let accepted = scan_tree(&store.join(ACCEPTED_DIR))?;
        let working = scan_tree(&store.join(WORKING_DIR))?;
        let mut manifest = CheckpointStore::open(&store, DEFAULT_CHECKPOINT_CAP)?;
        let meta = manifest.create(
            params.label.as_deref(),
            diff_snapshots(&accepted, &working).len(),
        )?;
        copy_project_tree(
            &store.join(WORKING_DIR),
            &store
                .join("checkpoints")
                .join(&meta.id)
                .join(CHECKPOINT_TREE_DIR),
        )?;
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
        let changes = describe_tree_diff(&from, &to)?;
        Ok(DiffResult {
            from: params.from,
            to: params.to,
            pending_changes: changes.len(),
            changes,
        })
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        let _lock = self.lock_running(&params.session_id)?;
        let store = self.store_dir(&params.session_id);
        let accepted = store.join(ACCEPTED_DIR);
        let working = store.join(WORKING_DIR);
        let expected = scan_tree(&accepted)?;
        let actual = scan_tree(&self.project)?;
        if !diff_snapshots(&expected, &actual).is_empty() {
            return Err(Error::ProjectConflict(
                "real project no longer matches the accepted baseline".into(),
            ));
        }
        let applied = apply_tree_delta(&accepted, &working, &self.project)?;
        copy_project_tree(&self.project, &accepted)?;
        copy_project_tree(&accepted, &working)?;
        let id = params.checkpoint.unwrap_or_else(|| "accepted".into());
        self.append(
            &params.session_id,
            JournalKind::Commit,
            Some(serde_json::json!({"checkpoint_id": id, "applied": applied})),
        )?;
        Ok(CommitResult {
            checkpoint_id: id,
            applied,
        })
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        let _lock = self.lock_running(&params.session_id)?;
        let store = self.store_dir(&params.session_id);
        let source = params
            .checkpoint
            .as_deref()
            .map(|id| self.checkpoint_tree(&store, id))
            .transpose()?
            .unwrap_or_else(|| store.join(ACCEPTED_DIR));
        let restored = describe_tree_diff(&source, &store.join(WORKING_DIR))?.len();
        copy_project_tree(&source, &store.join(WORKING_DIR))?;
        let id = params.checkpoint.unwrap_or_else(|| "accepted".into());
        self.append(
            &params.session_id,
            JournalKind::Abort,
            Some(serde_json::json!({"checkpoint_id": id, "restored": restored})),
        )?;
        Ok(AbortResult {
            checkpoint_id: id,
            restored,
        })
    }

    fn run_and_materialize<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&mut dyn SandboxBackend, &str) -> Result<T>,
    ) -> Result<T> {
        let store = self.store_dir(session_id);
        let working = store.join(WORKING_DIR);
        let before = scan_tree(&working)?;
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
                let deleted = before
                    .iter()
                    .filter(|entry| !current_paths.contains(entry.path.as_str()))
                    .map(|entry| entry.path.clone())
                    .collect();
                let txn = store.join("transactions").join(Uuid::new_v4().to_string());
                std::fs::create_dir_all(&txn)?;
                backend.materialize_changes(session_id, &txn, deleted)?;
                let _ = std::fs::remove_dir_all(&txn);
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
        SessionLock::acquire(&self.store_dir(session_id))
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
    full.strip_prefix(project)
        .map(Path::to_path_buf)
        .map_err(|_| Error::InvalidPolicy("path outside project".into()))
}

fn map_project_path(project: &Path, working: &Path, path: &Path) -> Result<PathBuf> {
    Ok(working.join(relative_project_path(project, path)?))
}

fn copy_project_tree(src: &Path, dst: &Path) -> Result<usize> {
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
        copy_entry(entry.path(), &dst.join(rel), entry.file_type())?;
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
    } else if file_type.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())?;
        let target = File::options().write(true).open(dst)?;
        target.set_modified(std::fs::metadata(src)?.modified()?)?;
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
    let diffs = diff_snapshots(&scan_tree(from)?, &scan_tree(to)?);
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
}

use crate::backend::{
    linux_sandlock::{resolve_deny_paths, SandlockBackend},
    SandboxBackend,
};
use crate::changeset::{describe_change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{Error, Result};
use crate::journal::{Journal, JournalEntry, JournalKind};
use crate::pathmap::PathMapper;
use crate::policy::Policy;
use crate::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, ExecParams, ExecRequest, ExecResult, FilesystemInfo,
    FsReadResult, ReviewBeginParams, SessionInfo, SessionSpec, SessionStatus,
};
use crate::session::{now_ms, SessionMeta, SessionRegistry};
use base64::Engine;
use deltafs_fuse::control::{self, Command as ControlCommand, ResultValue};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const MOUNT_RECORD: &str = "mount.json";
const SESSION_META: &str = "session.json";
const ACTIVE_REVIEW: &str = "active_review.json";
const SESSION_LOCK: &str = "session.lock";
const PROJECT_LEASE: &str = "active-cbcow";
const DENY_PATHS: &str = "deny-paths.json";
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MountRecord {
    mountpoint: PathBuf,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
    token_file: PathBuf,
    ready_file: PathBuf,
    helper_pid: u32,
    helper_start_ticks: u64,
    restarts: u64,
}

pub struct Manager {
    project: PathBuf,
    root: PathBuf,
    helper_bin: PathBuf,
}

impl Manager {
    pub fn new(project: &Path, helper_bin: PathBuf) -> Result<Self> {
        let project = project.canonicalize()?;
        let root = project.join(".deltabox");
        fs::create_dir_all(&root)?;
        Ok(Self {
            project,
            root,
            helper_bin,
        })
    }

    pub fn start(&self, policy_path: Option<&Path>) -> Result<SessionInfo> {
        let _project_lock = Lock::acquire(&self.root.join("cbcow.lock"))?;
        self.require_available_project()?;
        let policy_path = resolve_policy(&self.project, policy_path)?;
        let policy = match &policy_path {
            Some(path) => Policy::load(path)?,
            None => Policy::default_agent(),
        };
        let deny_paths = resolve_deny_paths(&policy, &self.project)?;

        let session_id = Uuid::new_v4().to_string();
        let store = self.store(&session_id);
        fs::create_dir_all(&store)?;
        save_json(&store.join(DENY_PATHS), &deny_paths)?;
        let state_root = state_base()?.join(&session_id);
        let runtime_dir = runtime_base().join(&session_id);
        fs::create_dir_all(state_root.join("engine"))?;
        fs::create_dir_all(&runtime_dir)?;
        fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
        let mountpoint = runtime_dir.join("mount");
        fs::create_dir_all(&mountpoint)?;
        let token_file = state_root.join("token");
        write_private(
            &token_file,
            format!("{}{}\n", Uuid::new_v4(), Uuid::new_v4()).as_bytes(),
        )?;

        let mut record = MountRecord {
            mountpoint,
            state_dir: state_root.join("engine"),
            socket: runtime_dir.join("control.sock"),
            token_file,
            ready_file: runtime_dir.join("ready"),
            runtime_dir,
            helper_pid: 0,
            helper_start_ticks: 0,
            restarts: 0,
        };
        save_json(&store.join(MOUNT_RECORD), &record)?;
        if let Err(error) = self.spawn_helper(&mut record) {
            return match self.cleanup_start(&session_id, &record) {
                Ok(()) => Err(Error::FilesystemUnavailable(error.to_string())),
                Err(cleanup) => Err(Error::Backend(format!(
                    "{error}; filesystem startup cleanup failed: {cleanup}"
                ))),
            };
        }

        let filesystem = FilesystemInfo::cbcow_through(
            record.mountpoint.clone(),
            record.helper_pid,
            record.restarts,
        );
        let meta = SessionMeta {
            session_id: session_id.clone(),
            project: self.project.clone(),
            policy_path,
            started_at_ms: now_ms(),
            status: SessionStatus::Running,
            filesystem: filesystem.clone(),
        };
        let result = (|| {
            save_json(&store.join(MOUNT_RECORD), &record)?;
            self.save_meta(&meta)?;
            fs::write(self.root.join(PROJECT_LEASE), format!("{session_id}\n"))?;
            self.append(&session_id, JournalKind::SessionStart, None)?;
            Ok(SessionInfo {
                session_id: session_id.clone(),
                project: self.project.clone(),
                started_at_ms: meta.started_at_ms,
                status: SessionStatus::Running,
                checkpoint_count: 0,
                pending_changes: 0,
                filesystem,
            })
        })();
        match result {
            Ok(info) => Ok(info),
            Err(error) => match self.cleanup_start(&session_id, &record) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Backend(format!(
                    "{error}; filesystem startup cleanup failed: {cleanup}"
                ))),
            },
        }
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        let _lock = self.lock(session_id)?;
        let meta = self.load_meta(session_id)?;
        if meta.status == SessionStatus::Stopped {
            return Ok(SessionInfo {
                session_id: meta.session_id,
                project: meta.project,
                started_at_ms: meta.started_at_ms,
                status: meta.status,
                checkpoint_count: 0,
                pending_changes: 0,
                filesystem: meta.filesystem,
            });
        }
        let record = self.ensure_helper(session_id)?;
        let status = match self.request(&record, ControlCommand::Status)? {
            ResultValue::Status(status) => status,
            _ => {
                return Err(Error::Backend(
                    "unexpected filesystem status response".into(),
                ))
            }
        };
        save_json(&self.store(session_id).join(MOUNT_RECORD), &record)?;
        Ok(SessionInfo {
            session_id: meta.session_id,
            project: meta.project,
            started_at_ms: meta.started_at_ms,
            status: meta.status,
            checkpoint_count: status.checkpoints.len().saturating_sub(1),
            pending_changes: status.touched_paths,
            filesystem: FilesystemInfo::cbcow_through(
                record.mountpoint,
                record.helper_pid,
                record.restarts,
            ),
        })
    }

    pub fn stop(&self, session_id: &str) -> Result<SessionInfo> {
        let _lock = self.lock(session_id)?;
        let mut meta = self.running_meta(session_id)?;
        let record = self.ensure_helper(session_id)?;
        if self.pending(&record)? != 0 {
            match self.request(&record, ControlCommand::Abort { target: None })? {
                ResultValue::Aborted => {}
                _ => return Err(Error::Backend("unexpected abort response".into())),
            }
            self.append(session_id, JournalKind::Abort, None)?;
        }
        self.save_review(session_id, None)?;
        if self.request(&record, ControlCommand::Shutdown).is_err() {
            self.stop_helper(&record);
        }
        if wait_unmounted(&record.mountpoint).is_err() {
            detach_mount(&record.mountpoint)?;
            wait_unmounted(&record.mountpoint)?;
        }
        meta.status = SessionStatus::Stopped;
        meta.filesystem = FilesystemInfo::cbcow_through(
            record.mountpoint.clone(),
            record.helper_pid,
            record.restarts,
        );
        meta.filesystem.helper_pid = None;
        self.save_meta(&meta)?;
        if fs::read_to_string(self.root.join(PROJECT_LEASE))
            .is_ok_and(|value| value.trim() == session_id)
        {
            let _ = fs::remove_file(self.root.join(PROJECT_LEASE));
        }
        self.append(session_id, JournalKind::SessionStop, None)?;
        Ok(SessionInfo {
            session_id: meta.session_id,
            project: meta.project,
            started_at_ms: meta.started_at_ms,
            status: meta.status,
            checkpoint_count: 0,
            pending_changes: 0,
            filesystem: meta.filesystem,
        })
    }

    pub fn exec(&self, params: ExecParams) -> Result<ExecResult> {
        let _lock = self.lock(&params.session_id)?;
        let meta = self.running_meta(&params.session_id)?;
        let record = self.ensure_helper(&params.session_id)?;
        let mapper = PathMapper::new(&self.project, &record.mountpoint);
        let mut policy = self.policy(&meta)?;
        let deny_paths: Vec<PathBuf> =
            serde_json::from_slice(&fs::read(self.store(&params.session_id).join(DENY_PATHS))?)?;
        let mut deny_paths: Vec<PathBuf> = deny_paths
            .into_iter()
            .map(|path| mapper.map_path(&path))
            .collect();
        let state_root = record
            .state_dir
            .parent()
            .expect("engine state has a session root")
            .to_path_buf();
        deny_paths.push(state_root.clone());
        policy
            .filesystem
            .deny
            .push(state_root.to_string_lossy().into_owned());
        let request = ExecRequest {
            argv: params
                .argv
                .into_iter()
                .map(|value| mapper.map_arg(&value))
                .collect(),
            cwd: params.cwd.as_deref().map(|path| mapper.map_path(path)),
            env: params
                .env
                .map(|values| {
                    values
                        .into_iter()
                        .map(|(key, value)| mapper.map_env(&key, &value).map(|value| (key, value)))
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?,
        };
        let spec = SessionSpec {
            project: record.mountpoint.clone(),
            host_project: Some(self.project.clone()),
            store_dir: record.runtime_dir.join("command"),
            policy,
            resolved_deny_paths: Some(deny_paths),
        };
        let mut backend = SandlockBackend::new_direct(Some(self.helper_bin.clone()));
        backend.start_session(&params.session_id, &spec)?;
        let result = backend.exec(&params.session_id, request);
        let stop = backend.stop_session(&params.session_id);
        let result = result?;
        stop?;
        self.append(
            &params.session_id,
            JournalKind::Exec,
            Some(serde_json::json!({"label": params.label, "result": result})),
        )?;
        Ok(result)
    }

    pub fn read_file(&self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        let _lock = self.lock(session_id)?;
        let meta = self.running_meta(session_id)?;
        let record = self.ensure_helper(session_id)?;
        let relative = relative_path(&self.project, path)?;
        if self.policy(&meta)?.path_is_denied(&relative) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        let target = checked_mount_path(&record.mountpoint, &relative)?;
        let metadata = fs::metadata(&target)?;
        if !metadata.is_file() || metadata.len() > MAX_READ_BYTES {
            return Err(Error::Backend(
                "fs.read target is unavailable or too large".into(),
            ));
        }
        let bytes = fs::read(target)?;
        Ok(FsReadResult {
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            size: bytes.len(),
        })
    }

    pub fn write_file(&self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        let _lock = self.lock(session_id)?;
        let meta = self.running_meta(session_id)?;
        let record = self.ensure_helper(session_id)?;
        let policy = self.policy(&meta)?;
        let relative = relative_path(&self.project, path)?;
        if policy.path_is_denied(&relative) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        if policy
            .limits
            .max_write_bytes
            .is_some_and(|limit| content.len() as u64 > limit)
        {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        let target = checked_mount_path(&record.mountpoint, &relative)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, content)?;
        self.append(
            session_id,
            JournalKind::Write,
            Some(serde_json::json!({"path": relative})),
        )
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        let _lock = self.lock(&params.session_id)?;
        self.checkpoint_locked(params)
    }

    fn checkpoint_locked(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        self.running_meta(&params.session_id)?;
        let record = self.ensure_helper(&params.session_id)?;
        let pending = self.pending(&record)?;
        let checkpoint = match self.request(
            &record,
            ControlCommand::Checkpoint {
                label: params.label.clone(),
            },
        )? {
            ResultValue::Checkpoint(checkpoint) => checkpoint,
            _ => return Err(Error::Backend("unexpected checkpoint response".into())),
        };
        self.append(&params.session_id, JournalKind::Checkpoint, None)?;
        Ok(CheckpointInfo {
            id: checkpoint.id.to_string(),
            label: checkpoint.label,
            created_at_ms: now_ms(),
            change_count: pending,
            committed: false,
        })
    }

    pub fn diff(&self, params: DiffParams) -> Result<DiffResult> {
        let _lock = self.lock(&params.session_id)?;
        self.diff_locked(params)
    }

    fn diff_locked(&self, params: DiffParams) -> Result<DiffResult> {
        if params.to.is_some() {
            return Err(Error::Unsupported(
                "diff --to is unavailable for this filesystem".into(),
            ));
        }
        self.running_meta(&params.session_id)?;
        let record = self.ensure_helper(&params.session_id)?;
        let from = parse_checkpoint(params.from.as_deref())?;
        let changes = match self.request(&record, ControlCommand::Diff { from })? {
            ResultValue::Diff(changes) => changes
                .into_iter()
                .map(|change| {
                    let kind = match change.kind {
                        deltafs_fuse::engine::ChangeKind::Added => ChangeKind::Added,
                        deltafs_fuse::engine::ChangeKind::Deleted => ChangeKind::Deleted,
                        deltafs_fuse::engine::ChangeKind::Modified
                        | deltafs_fuse::engine::ChangeKind::Metadata => ChangeKind::Modified,
                    };
                    describe_change(
                        &change.path,
                        kind,
                        change.old_size,
                        change.new_size,
                        change.old_bytes.as_deref(),
                        change.new_bytes.as_deref(),
                        DEFAULT_TEXT_SIZE_LIMIT,
                    )
                })
                .collect::<Vec<_>>(),
            _ => return Err(Error::Backend("unexpected diff response".into())),
        };
        self.append(&params.session_id, JournalKind::Diff, None)?;
        Ok(DiffResult {
            from: params.from,
            to: None,
            pending_changes: changes.len(),
            changes,
        })
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        let _lock = self.lock(&params.session_id)?;
        self.commit_locked(params)
    }

    fn commit_locked(&self, params: CommitParams) -> Result<CommitResult> {
        self.running_meta(&params.session_id)?;
        let record = self.ensure_helper(&params.session_id)?;
        let applied = self.pending(&record)?;
        let target = parse_checkpoint(params.checkpoint.as_deref())?;
        let checkpoint = match self.request(&record, ControlCommand::Commit { target })? {
            ResultValue::Committed(checkpoint) => checkpoint,
            _ => return Err(Error::Backend("unexpected commit response".into())),
        };
        self.save_review(&params.session_id, None)?;
        self.append(&params.session_id, JournalKind::Commit, None)?;
        Ok(CommitResult {
            checkpoint_id: checkpoint.id.to_string(),
            applied,
        })
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        let _lock = self.lock(&params.session_id)?;
        self.abort_locked(params)
    }

    fn abort_locked(&self, params: AbortParams) -> Result<AbortResult> {
        self.running_meta(&params.session_id)?;
        let record = self.ensure_helper(&params.session_id)?;
        let restored = self.pending(&record)?;
        let target = parse_checkpoint(params.checkpoint.as_deref())?;
        match self.request(&record, ControlCommand::Abort { target })? {
            ResultValue::Aborted => {}
            _ => return Err(Error::Backend("unexpected abort response".into())),
        }
        self.append(&params.session_id, JournalKind::Abort, None)?;
        Ok(AbortResult {
            checkpoint_id: target.unwrap_or(0).to_string(),
            restored,
        })
    }

    pub fn begin_review(&self, params: ReviewBeginParams) -> Result<ActiveReview> {
        let _lock = self.lock(&params.session_id)?;
        self.running_meta(&params.session_id)?;
        if let Some(active) = self.review(&params.session_id)? {
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
        self.save_review(&params.session_id, Some(&active))?;
        self.append(&params.session_id, JournalKind::ReviewBegin, None)?;
        Ok(active)
    }

    pub fn review_status(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        self.review(session_id)
    }

    pub fn review_diff(&self, session_id: &str) -> Result<DiffResult> {
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let active = self.require_review(session_id)?;
        self.diff_locked(DiffParams {
            session_id: session_id.into(),
            from: Some(active.checkpoint_id),
            to: None,
        })
    }

    pub fn review_commit(&self, session_id: &str) -> Result<CommitResult> {
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        self.require_review(session_id)?;
        self.commit_locked(CommitParams {
            session_id: session_id.into(),
            checkpoint: None,
        })
    }

    pub fn review_abort(&self, session_id: &str) -> Result<AbortResult> {
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let active = self.require_review(session_id)?;
        let result = self.abort_locked(AbortParams {
            session_id: session_id.into(),
            checkpoint: Some(active.checkpoint_id),
        })?;
        self.save_review(session_id, None)?;
        Ok(result)
    }

    pub fn review_skip(&self, session_id: &str) -> Result<ActiveReview> {
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let active = self.require_review(session_id)?;
        self.append(session_id, JournalKind::ReviewSkip, None)?;
        Ok(active)
    }

    fn ensure_helper(&self, session_id: &str) -> Result<MountRecord> {
        let mut record = self.load_record(session_id)?;
        if self.request(&record, ControlCommand::Ping).is_ok() {
            return Ok(record);
        }
        self.stop_helper(&record);
        wait_helper_stopped(&record)?;
        detach_mount(&record.mountpoint)?;
        wait_unmounted(&record.mountpoint)?;
        fs::create_dir_all(&record.runtime_dir)?;
        fs::create_dir_all(&record.mountpoint)?;
        record.restarts += 1;
        self.spawn_helper(&mut record)?;
        save_json(&self.store(session_id).join(MOUNT_RECORD), &record)?;
        self.append(session_id, JournalKind::Recovery, None)?;
        Ok(record)
    }

    fn spawn_helper(&self, record: &mut MountRecord) -> Result<()> {
        let _ = fs::remove_file(&record.ready_file);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(record.state_dir.parent().unwrap().join("helper.log"))?;
        let child = Command::new(&self.helper_bin)
            .arg("fs-helper")
            .arg("--source")
            .arg(&self.project)
            .arg("--mountpoint")
            .arg(&record.mountpoint)
            .arg("--state-dir")
            .arg(&record.state_dir)
            .arg("--socket")
            .arg(&record.socket)
            .arg("--token-file")
            .arg(&record.token_file)
            .arg("--ready-file")
            .arg(&record.ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|error| Error::Backend(format!("start filesystem helper: {error}")))?;
        record.helper_pid = child.id();
        record.helper_start_ticks = process_start_ticks(child.id()).ok_or_else(|| {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            Error::Backend("filesystem helper exited during startup".into())
        })?;
        let token = fs::read_to_string(&record.token_file)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if record.ready_file.exists()
                && control::request(&record.socket, token.trim(), ControlCommand::Ping).is_ok()
            {
                return Ok(());
            }
            if process_start_ticks(record.helper_pid) != Some(record.helper_start_ticks) {
                return Err(Error::Backend(
                    "filesystem helper exited during startup".into(),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.stop_helper(record);
        Err(Error::Backend(
            "filesystem helper did not become ready".into(),
        ))
    }

    fn stop_helper(&self, record: &MountRecord) {
        if process_start_ticks(record.helper_pid) == Some(record.helper_start_ticks) {
            unsafe {
                libc::kill(record.helper_pid as i32, libc::SIGTERM);
            }
        }
    }

    fn request(&self, record: &MountRecord, command: ControlCommand) -> Result<ResultValue> {
        let token = fs::read_to_string(&record.token_file)?;
        control::request(&record.socket, token.trim(), command)
            .map_err(|error| Error::Backend(format!("filesystem control: {error}")))
    }

    fn pending(&self, record: &MountRecord) -> Result<usize> {
        match self.request(record, ControlCommand::Status)? {
            ResultValue::Status(status) => Ok(status.touched_paths),
            _ => Err(Error::Backend(
                "unexpected filesystem status response".into(),
            )),
        }
    }

    fn cleanup_start(&self, session_id: &str, record: &MountRecord) -> Result<()> {
        self.stop_helper(record);
        detach_mount(&record.mountpoint)?;
        wait_unmounted(&record.mountpoint)?;
        wait_helper_stopped(record)?;

        let lease = self.root.join(PROJECT_LEASE);
        match fs::read_to_string(&lease) {
            Ok(value) if value.trim() == session_id => fs::remove_file(lease)?,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut registry = SessionRegistry::load(&self.root)?;
        let old_len = registry.sessions.len();
        registry
            .sessions
            .retain(|session| session.session_id != session_id);
        if registry.sessions.len() != old_len {
            registry.save(&self.root)?;
        }

        let store = self.store(session_id);
        let state_root = record
            .state_dir
            .parent()
            .expect("engine state has a session root");
        for directory in [record.runtime_dir.as_path(), state_root, store.as_path()] {
            match fs::remove_dir_all(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn policy(&self, meta: &SessionMeta) -> Result<Policy> {
        match &meta.policy_path {
            Some(path) => Policy::load(path),
            None => Ok(Policy::default_agent()),
        }
    }

    fn require_available_project(&self) -> Result<()> {
        let lease = self.root.join(PROJECT_LEASE);
        if let Ok(session_id) = fs::read_to_string(&lease) {
            if self
                .load_meta(session_id.trim())
                .is_ok_and(|meta| meta.status == SessionStatus::Running)
            {
                return Err(Error::SessionBusy(session_id.trim().into()));
            }
            fs::remove_file(lease)?;
        }
        Ok(())
    }

    fn running_meta(&self, session_id: &str) -> Result<SessionMeta> {
        let meta = self.load_meta(session_id)?;
        if meta.status == SessionStatus::Running {
            Ok(meta)
        } else {
            Err(Error::SessionStopped(session_id.into()))
        }
    }

    fn load_meta(&self, session_id: &str) -> Result<SessionMeta> {
        let path = self.store(session_id).join(SESSION_META);
        if !path.exists() {
            return Err(Error::SessionNotFound(session_id.into()));
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn save_meta(&self, meta: &SessionMeta) -> Result<()> {
        let store = self.store(&meta.session_id);
        save_json(&store.join(SESSION_META), meta)?;
        let mut registry = SessionRegistry::load(&self.root)?;
        registry.upsert(meta.clone());
        registry.save(&self.root)
    }

    fn load_record(&self, session_id: &str) -> Result<MountRecord> {
        Ok(serde_json::from_slice(&fs::read(
            self.store(session_id).join(MOUNT_RECORD),
        )?)?)
    }

    fn review(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let path = self.store(session_id).join(ACTIVE_REVIEW);
        if path.exists() {
            Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
        } else {
            Ok(None)
        }
    }

    fn require_review(&self, session_id: &str) -> Result<ActiveReview> {
        self.review(session_id)?
            .ok_or_else(|| Error::ReviewNotActive(session_id.into()))
    }

    fn save_review(&self, session_id: &str, review: Option<&ActiveReview>) -> Result<()> {
        let path = self.store(session_id).join(ACTIVE_REVIEW);
        match review {
            Some(review) => save_json(&path, review),
            None if path.exists() => fs::remove_file(path).map_err(Error::from),
            None => Ok(()),
        }
    }

    fn append(
        &self,
        session_id: &str,
        kind: JournalKind,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut journal = Journal::open(&self.store(session_id).join("journal.jsonl"))?;
        journal.append(JournalEntry {
            ts_ms: now_ms(),
            session_id: session_id.into(),
            kind,
            data,
        })
    }

    fn store(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    fn lock(&self, session_id: &str) -> Result<Lock> {
        Lock::acquire(&self.store(session_id).join(SESSION_LOCK))
    }
}

struct Lock(File);

impl Lock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn resolve_policy(project: &Path, policy: Option<&Path>) -> Result<Option<PathBuf>> {
    policy
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                project.join(path)
            }
            .canonicalize()
            .map_err(Error::from)
        })
        .transpose()
}

fn state_base() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DELTABOX_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("deltabox-lite"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Backend("HOME is unavailable".into()))?;
    Ok(home.join(".local/state/deltabox-lite"))
}

fn runtime_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/deltabox-{}", unsafe { libc::geteuid() })))
        .join("deltabox-lite")
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

fn parse_checkpoint(value: Option<&str>) -> Result<Option<u64>> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| Error::CheckpointNotFound(value.into()))
        })
        .transpose()
}

fn relative_path(project: &Path, path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        path.strip_prefix(project)
            .map_err(|_| Error::Backend("path is outside project".into()))?
    } else {
        path
    };
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) || relative
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == ".deltabox")
    {
        return Err(Error::Backend("path is outside project".into()));
    }
    Ok(relative.to_path_buf())
}

fn checked_mount_path(mountpoint: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = mountpoint.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Backend("path crosses a symlink".into()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(mountpoint.join(relative))
}

fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    let mut fields = after_name.split_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        unsafe {
            libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
        }
        return None;
    }
    fields.nth(18)?.parse().ok()
}

fn wait_helper_stopped(record: &MountRecord) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && process_start_ticks(record.helper_pid) == Some(record.helper_start_ticks)
    {
        thread::sleep(Duration::from_millis(10));
    }
    if process_start_ticks(record.helper_pid) == Some(record.helper_start_ticks) {
        Err(Error::Backend(
            "filesystem helper did not stop during cleanup".into(),
        ))
    } else {
        Ok(())
    }
}

fn detach_mount(mountpoint: &Path) -> Result<()> {
    let path = CString::new(mountpoint.as_os_str().as_bytes())
        .map_err(|_| Error::Backend("mountpoint contains a null byte".into()))?;
    if unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) } == 0 || !is_mounted(mountpoint) {
        return Ok(());
    }
    let program = deltafs_fuse::doctor::inspect()
        .fusermount
        .ok_or_else(|| Error::Backend("fusermount is unavailable".into()))?;
    let status = Command::new(program).arg("-uz").arg(mountpoint).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Backend("failed to detach stale FUSE mount".into()))
    }
}

fn wait_unmounted(mountpoint: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && is_mounted(mountpoint) {
        thread::sleep(Duration::from_millis(10));
    }
    if is_mounted(mountpoint) {
        Err(Error::Backend("FUSE mount did not detach".into()))
    } else {
        Ok(())
    }
}

fn is_mounted(mountpoint: &Path) -> bool {
    let target = mountpoint.to_string_lossy();
    fs::read_to_string("/proc/self/mountinfo").is_ok_and(|mounts| {
        mounts.lines().any(|line| {
            line.split_whitespace()
                .nth(4)
                .is_some_and(|path| path == target)
        })
    })
}

#[cfg(not(windows))]
use crate::durable::DurableSessionManager;
use crate::error::{Error, Result};
#[cfg(not(windows))]
use crate::protocol::FilesystemInfo;
use crate::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, DurabilityMode, ExecParams, ExecResult, FilesystemMode,
    FsReadResult, ReviewBeginParams, SessionInfo,
};
use crate::session_catalog::SessionSummary;
#[cfg(not(windows))]
use crate::session::SessionMeta;
#[cfg(not(windows))]
use crate::state_layout::state_root;
use std::collections::HashMap;
#[cfg(not(windows))]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

/// Command execution behavior selected once when a coordinator is embedded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommandExecutionMode {
    /// Preserve the existing per-platform sandbox helper path.
    #[default]
    Sandboxed,
    /// Spawn commands directly with the embedding process environment.
    Trusted,
}

/// Immutable coordinator configuration, safe to share across worker tasks.
#[derive(Debug, Clone, Default)]
pub struct CoordinatorOptions {
    pub helper_bin: Option<PathBuf>,
    pub command_execution: CommandExecutionMode,
    /// Explicit engine state base for embedded callers.
    pub state_dir: Option<PathBuf>,
    /// Explicit ephemeral runtime base for embedded callers.
    pub runtime_dir: Option<PathBuf>,
    /// Keep routing and backend session state resident for an in-process host.
    pub embedded_fast: bool,
    /// Attach this process as the owner of every session it starts or adopts.
    ///
    /// For long-lived hosts (the Node addon). The owner holds an OS lock for
    /// the session's life; if the process dies without stopping, the session
    /// becomes *orphaned* — it keeps its changes and its project lease, and a
    /// later owner can `adopt` it to commit or abort. Without this flag
    /// sessions are detached (CLI): they outlive their starting process by
    /// design, so a running one is always treated as live.
    pub attach_sessions: bool,
}

pub struct SessionCoordinator {
    #[cfg(not(windows))]
    project: PathBuf,
    #[cfg(not(windows))]
    session_root: PathBuf,
    routes: RwLock<HashMap<String, FilesystemMode>>,
    attach_sessions: bool,
    /// Owner locks for the sessions this process is attached to.
    owners: Mutex<HashMap<String, crate::owner::OwnerGuard>>,
    #[cfg(not(windows))]
    copy_private: DurableSessionManager,
    #[cfg(windows)]
    windows_cbcow: crate::windows_cbcow::Manager,
    #[cfg(target_os = "linux")]
    cbcow: crate::cbcow::Manager,
    #[cfg(target_os = "macos")]
    macos_cbcow: crate::macos_cbcow::Manager,
}

impl SessionCoordinator {
    pub fn new(project: &Path, worker_bin: Option<PathBuf>) -> Result<Self> {
        Self::new_with_options(
            project,
            CoordinatorOptions {
                helper_bin: worker_bin,
                ..CoordinatorOptions::default()
            },
        )
    }

    pub fn new_with_options(project: &Path, options: CoordinatorOptions) -> Result<Self> {
        let project = project.canonicalize()?;
        // Resolve once: listing must not create a removed state directory or
        // change roots if the process environment changes after construction.
        #[cfg(not(windows))]
        let session_root = state_root(&project)?;
        let attach_sessions = options.attach_sessions;
        let helper_bin = options
            .helper_bin
            .clone()
            .unwrap_or(std::env::current_exe()?);
        #[cfg(not(windows))]
        let copy_private = DurableSessionManager::new_with_execution_mode(
            &project,
            Some(helper_bin.clone()),
            options.command_execution,
        )?;
        #[cfg(target_os = "linux")]
        let cbcow = crate::cbcow::Manager::new_with_execution_mode(
            &project,
            helper_bin.clone(),
            options.command_execution,
            options.state_dir.clone(),
            options.runtime_dir.clone(),
            options.embedded_fast,
        )?;
        #[cfg(target_os = "macos")]
        let macos_cbcow = crate::macos_cbcow::Manager::new_with_execution_mode(
            &project,
            helper_bin,
            options.command_execution,
            options.state_dir,
            options.embedded_fast,
        )?;
        #[cfg(windows)]
        let windows_cbcow = crate::windows_cbcow::Manager::new_with_options(
            &project,
            CoordinatorOptions {
                helper_bin: Some(helper_bin),
                ..options
            },
        )?;
        Ok(Self {
            #[cfg(not(windows))]
            project,
            #[cfg(not(windows))]
            session_root,
            routes: RwLock::new(HashMap::new()),
            attach_sessions,
            owners: Mutex::new(HashMap::new()),
            #[cfg(not(windows))]
            copy_private,
            #[cfg(windows)]
            windows_cbcow,
            #[cfg(target_os = "linux")]
            cbcow,
            #[cfg(target_os = "macos")]
            macos_cbcow,
        })
    }

    pub fn start(
        &self,
        policy: Option<&Path>,
        requested: Option<FilesystemMode>,
        require_requested: bool,
    ) -> Result<SessionInfo> {
        self.start_with_durability(
            policy,
            requested,
            require_requested,
            DurabilityMode::Fast,
            None,
        )
    }

    pub fn start_with_durability(
        &self,
        policy: Option<&Path>,
        requested: Option<FilesystemMode>,
        require_requested: bool,
        durability: DurabilityMode,
        group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        if group_commit_ms.is_some() && durability != DurabilityMode::Strict {
            return Err(Error::InvalidDurability(
                "group_commit_ms requires strict durability".into(),
            ));
        }
        if group_commit_ms == Some(0) {
            return Err(Error::InvalidDurability(
                "group_commit_ms must be greater than zero".into(),
            ));
        }
        let requested = requested.unwrap_or(default_mode());
        let info = match requested {
            FilesystemMode::CopyPrivate => {
                if durability == DurabilityMode::Strict {
                    return Err(Error::InvalidDurability(
                        "strict durability requires a CB-CoW filesystem mode".into(),
                    ));
                }
                #[cfg(not(windows))]
                {
                    self.copy_private.start_with_filesystem(
                        policy,
                        FilesystemInfo::copy_private(requested, None),
                    )
                }
                #[cfg(windows)]
                {
                    self.windows_cbcow.start_copy_private(policy)
                }
            }
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => self.start_cbcow(
                policy,
                requested,
                require_requested,
                durability,
                group_commit_ms,
            ),
        }?;
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .insert(info.session_id.clone(), info.filesystem.resolved_fs_mode);
        if self.attach_sessions {
            // Until this attach lands the session reads as detached, which
            // admission treats as live, so no sibling can see it as orphaned.
            let attached = self.session_store(&info.session_id).and_then(|store| {
                crate::owner::try_attach(&store)?.ok_or_else(|| {
                    Error::SessionBusy(format!("{}: owner lock already held", info.session_id))
                })
            });
            match attached {
                Ok(guard) => {
                    self.owners_map().insert(info.session_id.clone(), guard);
                }
                Err(error) => {
                    // An unowned session must not outlive a start that failed.
                    let _ = self.stop(&info.session_id);
                    return Err(error);
                }
            }
        }
        Ok(info)
    }

    /// Takes ownership of an orphaned session: one that was attached to a
    /// process which died before stopping it. The session keeps every pending
    /// change, so the caller then decides to commit or abort.
    ///
    /// Refused for detached sessions (they have no owner to replace) and for
    /// sessions whose owner is still alive. Adopting a session this
    /// coordinator already owns just reports its status.
    pub fn adopt(&self, session_id: &str) -> Result<SessionInfo> {
        if !self.attach_sessions {
            return Err(Error::Unsupported(
                "adopting a session requires a coordinator with attach_sessions".into(),
            ));
        }
        if self.owners_map().contains_key(session_id) {
            return self.status(session_id);
        }
        let store = self.session_store(session_id)?;
        if !self.is_running(session_id)? {
            return Err(Error::SessionStopped(session_id.into()));
        }
        match crate::owner::probe(&store) {
            crate::owner::Ownership::Detached => {
                return Err(Error::SessionBusy(format!(
                    "{session_id} is a detached session; manage it with the CLI"
                )))
            }
            crate::owner::Ownership::Attached { pid } => {
                return Err(Error::SessionBusy(format!(
                    "{session_id} is owned by live process {}",
                    pid.map_or_else(|| "(unknown pid)".into(), |pid| pid.to_string())
                )))
            }
            crate::owner::Ownership::Orphaned { .. } => {}
        }
        let guard = crate::owner::try_attach(&store)?.ok_or_else(|| {
            Error::SessionBusy(format!("{session_id} was adopted by another process"))
        })?;
        self.owners_map().insert(session_id.into(), guard);
        // Status also restarts a dead filesystem helper, so the adopted
        // session is usable right away.
        self.status(session_id)
    }

    /// Running sessions of this project whose attached owner died. Each one
    /// can be passed to `adopt`.
    pub fn orphaned_sessions(&self) -> Result<Vec<String>> {
        let mut orphans = Vec::new();
        let root = self.sessions_root()?;
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Ok(orphans);
        };
        for entry in entries.flatten() {
            let store = entry.path();
            let Some(id) = store.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_err() || !self.is_running(id).unwrap_or(false) {
                continue;
            }
            if matches!(
                crate::owner::probe(&store),
                crate::owner::Ownership::Orphaned { .. }
            ) {
                orphans.push(id.to_string());
            }
        }
        orphans.sort();
        Ok(orphans)
    }

    fn owners_map(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, crate::owner::OwnerGuard>> {
        self.owners.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sessions_root(&self) -> Result<PathBuf> {
        #[cfg(windows)]
        return Ok(self.windows_cbcow.sessions_root().to_path_buf());
        #[cfg(not(windows))]
        Ok(self.session_root.clone())
    }

    /// List this project's persisted sessions without recovering state,
    /// starting helpers, or scanning the working tree. Status is the last
    /// recorded status, not a liveness probe. Results are oldest first.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        #[cfg(not(windows))]
        {
            crate::session_catalog::read_sessions(&self.session_root, &self.project)
        }
        #[cfg(windows)]
        {
            self.windows_cbcow.list_sessions()
        }
    }

    fn session_store(&self, session_id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(session_id)
            .map_err(|_| Error::SessionNotFound(session_id.into()))?;
        Ok(self.sessions_root()?.join(session_id))
    }

    /// Reads the durable status without starting a helper.
    fn is_running(&self, session_id: &str) -> Result<bool> {
        let path = self.session_store(session_id)?.join("session.json");
        let raw = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::SessionNotFound(session_id.into())
            } else {
                Error::Io(error)
            }
        })?;
        let record: serde_json::Value = serde_json::from_slice(&raw)?;
        Ok(record.get("status").and_then(|s| s.as_str()) == Some("running"))
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.status(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.status(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.status(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.status(session_id),
        }
    }

    pub fn stop(&self, session_id: &str) -> Result<SessionInfo> {
        let info = match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.stop(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.stop(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.stop(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.stop(session_id),
        }?;
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .remove(session_id);
        // Released only after the stop recorded itself, so no observer ever
        // sees a free owner lock on a session that is still running.
        self.owners_map().remove(session_id);
        Ok(info)
    }

    pub fn release(&self, session_id: &str) -> Result<SessionInfo> {
        let info = match self.route(session_id)? {
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.release(session_id),
            _ => Err(Error::Unsupported(
                "preserve-current release requires macOS cbcow-through".into(),
            )),
        }?;
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .remove(session_id);
        self.owners_map().remove(session_id);
        Ok(info)
    }

    pub fn remove_stopped(&self, session_id: &str) -> Result<()> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.remove_stopped(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.remove_stopped(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.remove_stopped(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.remove_stopped(session_id),
        }
    }

    pub fn exec(&self, params: ExecParams) -> Result<ExecResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.exec(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.exec(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.exec(params),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.exec(params),
        }
    }

    pub fn read_file(&self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.read_file(session_id, path),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.read_file(session_id, path),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.read_file(session_id, path),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.read_file(session_id, path),
        }
    }

    /// Read raw bytes without the JSON/Base64 compatibility layer.
    pub fn read_file_bytes(&self, session_id: &str, path: &Path) -> Result<Vec<u8>> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.read_file_bytes(session_id, path),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.read_file_bytes(session_id, path),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.read_file_bytes(session_id, path),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.read_file_bytes(session_id, path),
        }
    }

    pub fn write_file(&self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        self.write_file_bytes(session_id, path, &content)
    }

    /// Write borrowed bytes, avoiding a JS Buffer copy for native callers.
    pub fn write_file_bytes(&self, session_id: &str, path: &Path, content: &[u8]) -> Result<()> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.write_file(session_id, path, content.to_vec()),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.write_file_bytes(session_id, path, content),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.write_file_bytes(session_id, path, content),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.write_file_bytes(session_id, path, content),
        }
    }

    /// Register exact paths mutated directly through an embedded mountpoint.
    pub fn record_file_changes(&self, session_id: &str, paths: &[PathBuf]) -> Result<()> {
        match self.route(session_id)? {
            Route::CopyPrivate(_) => Err(Error::Unsupported(
                "direct mount tracking requires a CB-CoW filesystem mode".into(),
            )),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.record_file_changes(session_id, paths),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.record_file_changes(session_id, paths),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.record_file_changes(session_id, paths),
        }
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.checkpoint(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.checkpoint(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.checkpoint(params),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.checkpoint(params),
        }
    }

    pub fn diff(&self, params: DiffParams) -> Result<DiffResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.diff(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.diff(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.diff(params),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.diff(params),
        }
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.commit(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.commit(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.commit(params),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.commit(params),
        }
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.abort(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.abort(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.abort(params),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.abort(params),
        }
    }

    pub fn sync(&self, session_id: &str) -> Result<crate::protocol::SyncResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(_) => Err(crate::error::Error::Unsupported(
                "session.sync requires a cbcow-private session".into(),
            )),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.sync(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.sync(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.sync(session_id),
        }
    }

    pub fn begin_review(&self, params: ReviewBeginParams) -> Result<ActiveReview> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.begin_review(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.begin_review(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.begin_review(params),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.begin_review(params),
        }
    }

    pub fn review_status(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_status(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_status(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_status(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.review_status(session_id),
        }
    }

    pub fn review_diff(&self, session_id: &str) -> Result<DiffResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_diff(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_diff(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_diff(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.review_diff(session_id),
        }
    }

    pub fn review_commit(&self, session_id: &str) -> Result<CommitResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_commit(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_commit(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_commit(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.review_commit(session_id),
        }
    }

    pub fn review_abort(&self, session_id: &str) -> Result<AbortResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_abort(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_abort(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_abort(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.review_abort(session_id),
        }
    }

    pub fn review_skip(&self, session_id: &str) -> Result<ActiveReview> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_skip(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_skip(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_skip(session_id),
            #[cfg(windows)]
            Route::WindowsCbcow(manager) => manager.review_skip(session_id),
        }
    }

    #[cfg(target_os = "linux")]
    fn start_cbcow(
        &self,
        policy: Option<&Path>,
        requested: FilesystemMode,
        required: bool,
        durability: DurabilityMode,
        group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        let report = deltafs_fuse::doctor::inspect();
        if report.mount_ready {
            match self
                .cbcow
                .start(policy, requested, durability, group_commit_ms)
            {
                Ok(info) => return Ok(info),
                Err(Error::FilesystemUnavailable(reason))
                    if !required && durability == DurabilityMode::Fast =>
                {
                    return self.start_fallback(policy, requested, reason)
                }
                Err(error) => return Err(error),
            }
        }
        let reason = report.notes.join("; ");
        if required || durability == DurabilityMode::Strict {
            return Err(Error::Unsupported(reason));
        }
        self.start_fallback(policy, requested, reason)
    }

    #[cfg(target_os = "macos")]
    fn start_cbcow(
        &self,
        policy: Option<&Path>,
        requested: FilesystemMode,
        required: bool,
        durability: DurabilityMode,
        group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        match self
            .macos_cbcow
            .start(policy, requested, durability, group_commit_ms)
        {
            Ok(info) => Ok(info),
            Err(Error::FilesystemUnavailable(reason))
                if !required && durability == DurabilityMode::Fast =>
            {
                self.start_fallback(policy, requested, reason)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    fn start_cbcow(
        &self,
        policy: Option<&Path>,
        requested: FilesystemMode,
        required: bool,
        durability: DurabilityMode,
        _group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        let reason = format!("{requested:?} is unavailable on this platform");
        if required || durability == DurabilityMode::Strict {
            return Err(Error::Unsupported(reason));
        }
        self.start_fallback(policy, requested, reason)
    }

    #[cfg(windows)]
    fn start_cbcow(
        &self,
        policy: Option<&Path>,
        requested: FilesystemMode,
        _required: bool,
        durability: DurabilityMode,
        group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        self.windows_cbcow
            .start(policy, requested, durability, group_commit_ms)
    }

    #[cfg(not(windows))]
    fn start_fallback(
        &self,
        policy: Option<&Path>,
        requested: FilesystemMode,
        reason: String,
    ) -> Result<SessionInfo> {
        self.copy_private.start_with_filesystem(
            policy,
            FilesystemInfo::copy_private(requested, Some(reason)),
        )
    }

    fn route(&self, session_id: &str) -> Result<Route<'_>> {
        if let Some(mode) = self
            .routes
            .read()
            .expect("coordinator route cache poisoned")
            .get(session_id)
            .copied()
        {
            return self.route_mode(mode);
        }
        #[cfg(windows)]
        let mode = self.windows_cbcow.session_mode(session_id)?;
        #[cfg(not(windows))]
        let mode = {
            let path = state_root(&self.project)?
                .join(session_id)
                .join("session.json");
            let meta: SessionMeta = serde_json::from_slice(&fs::read(&path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Error::SessionNotFound(session_id.into())
                } else {
                    Error::Io(error)
                }
            })?)?;
            meta.filesystem.resolved_fs_mode
        };
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .insert(session_id.into(), mode);
        self.route_mode(mode)
    }

    fn route_mode(&self, mode: FilesystemMode) -> Result<Route<'_>> {
        match mode {
            #[cfg(not(windows))]
            FilesystemMode::CopyPrivate => Ok(Route::CopyPrivate(&self.copy_private)),
            #[cfg(windows)]
            FilesystemMode::CopyPrivate => Ok(Route::CopyPrivate(&self.windows_cbcow)),
            #[cfg(windows)]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => {
                Ok(Route::WindowsCbcow(&self.windows_cbcow))
            }
            #[cfg(target_os = "linux")]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => {
                Ok(Route::Cbcow(&self.cbcow))
            }
            #[cfg(target_os = "macos")]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => {
                Ok(Route::MacosCbcow(&self.macos_cbcow))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => Err(Error::Unsupported(
                "this CB-CoW session requires Linux".into(),
            )),
        }
    }
}

enum Route<'a> {
    #[cfg(not(windows))]
    CopyPrivate(&'a DurableSessionManager),
    #[cfg(windows)]
    CopyPrivate(&'a crate::windows_cbcow::Manager),
    #[cfg(windows)]
    WindowsCbcow(&'a crate::windows_cbcow::Manager),
    #[cfg(target_os = "linux")]
    Cbcow(&'a crate::cbcow::Manager),
    #[cfg(target_os = "macos")]
    MacosCbcow(&'a crate::macos_cbcow::Manager),
}

fn default_mode() -> FilesystemMode {
    FilesystemMode::CbcowThrough
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(not(windows))]
    #[test]
    fn list_sessions_uses_persisted_records_without_recovery_or_registry() {
        let project = tempfile::tempdir().unwrap();
        let coordinator = SessionCoordinator::new(project.path(), None).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = coordinator.session_root.join(&id);
        fs::create_dir(&directory).unwrap();
        // A running native session has no runtime, engine, or helper at all:
        // status() would attempt recovery, but discovery needs only this file.
        let mut filesystem = crate::protocol::FilesystemInfo::default();
        filesystem.resolved_fs_mode = FilesystemMode::CbcowThrough;
        let meta = SessionMeta {
            session_id: id.clone(),
            project: project.path().canonicalize().unwrap(),
            policy_path: None,
            started_at_ms: 42,
            status: crate::protocol::SessionStatus::Running,
            filesystem,
        };
        fs::write(
            directory.join("session.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        fs::write(coordinator.session_root.join("sessions.json"), b"stale registry").unwrap();
        let sessions = coordinator.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, id);
        assert_eq!(sessions[0].resolved_fs_mode, FilesystemMode::CbcowThrough);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert!(coordinator.routes.read().unwrap().is_empty());
        fs::remove_dir_all(&coordinator.session_root).unwrap();
        assert!(coordinator.list_sessions().unwrap().is_empty());
        assert!(!coordinator.session_root.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_routes_persisted_modes_from_custom_state_after_reopen() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let options = CoordinatorOptions {
            state_dir: Some(root.path().join("state")),
            runtime_dir: Some(root.path().join("runtime")),
            ..Default::default()
        };
        let first = SessionCoordinator::new_with_options(&project, options.clone()).unwrap();
        let started = first
            .start(None, Some(FilesystemMode::CopyPrivate), true)
            .unwrap();
        first.stop(&started.session_id).unwrap();
        drop(first);

        let projects = options.state_dir.as_ref().unwrap().join("projects");
        let project_state = fs::read_dir(projects)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest_path = project_state.join(&started.session_id).join("session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        // Routing must read native manifests without mounting or starting a helper.
        for mode in [
            FilesystemMode::CopyPrivate,
            FilesystemMode::CbcowThrough,
            FilesystemMode::CbcowPrivate,
        ] {
            manifest["fs_mode"] = serde_json::to_value(mode).unwrap();
            fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            let reopened = SessionCoordinator::new_with_options(&project, options.clone()).unwrap();
            let sessions = reopened.list_sessions().unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, started.session_id);
            assert_eq!(sessions[0].resolved_fs_mode, mode);
            for _ in 0..2 {
                let route = reopened.route(&started.session_id).unwrap();
                if mode == FilesystemMode::CopyPrivate {
                    assert!(matches!(route, Route::CopyPrivate(_)));
                } else {
                    assert!(matches!(route, Route::WindowsCbcow(_)));
                }
            }
            assert!(matches!(
                reopened.status("invalid-id"),
                Err(Error::SessionNotFound(_))
            ));
        }
    }

    #[test]
    fn copy_private_session_routes_after_coordinator_reopen() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("tracked.txt"), "accepted").unwrap();
        let first = SessionCoordinator::new(project.path(), None).unwrap();
        let started = first
            .start(None, Some(FilesystemMode::CopyPrivate), true)
            .unwrap();
        assert_eq!(
            started.filesystem.resolved_fs_mode,
            FilesystemMode::CopyPrivate
        );
        drop(first);

        let reopened = SessionCoordinator::new(project.path(), None).unwrap();
        let status = reopened.status(&started.session_id).unwrap();
        assert_eq!(status.session_id, started.session_id);
        assert_eq!(
            status.filesystem.requested_fs_mode,
            FilesystemMode::CopyPrivate
        );
        assert_eq!(status.filesystem.fallback_reason, None);
    }

    #[test]
    #[cfg(not(windows))]
    fn fallback_session_routes_after_coordinator_reopen() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("tracked.txt"), "accepted").unwrap();
        let first = SessionCoordinator::new(project.path(), None).unwrap();
        let started = first
            .start_fallback(
                None,
                FilesystemMode::CbcowThrough,
                "native provider unavailable".into(),
            )
            .unwrap();
        assert_eq!(
            started.filesystem.resolved_fs_mode,
            FilesystemMode::CopyPrivate
        );
        assert_eq!(
            started.filesystem.fallback_reason.as_deref(),
            Some("native provider unavailable")
        );
        drop(first);

        let reopened = SessionCoordinator::new(project.path(), None).unwrap();
        let status = reopened.status(&started.session_id).unwrap();
        assert_eq!(status.session_id, started.session_id);
        assert_eq!(
            status.filesystem.requested_fs_mode,
            FilesystemMode::CbcowThrough
        );
        assert_eq!(
            status.filesystem.resolved_fs_mode,
            FilesystemMode::CopyPrivate
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn legacy_session_without_filesystem_metadata_stays_copy_private() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("tracked.txt"), "accepted").unwrap();
        let first = SessionCoordinator::new(project.path(), None).unwrap();
        let started = first
            .start(None, Some(FilesystemMode::CopyPrivate), true)
            .unwrap();
        drop(first);

        let meta_path = project
            .path()
            .join(".deltabox")
            .join(&started.session_id)
            .join("session.json");
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta.as_object_mut().unwrap().remove("filesystem");
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();

        let reopened = SessionCoordinator::new(project.path(), None).unwrap();
        let status = reopened.status(&started.session_id).unwrap();
        assert_eq!(
            status.filesystem.resolved_fs_mode,
            FilesystemMode::CopyPrivate
        );
        assert_eq!(status.filesystem.host_visibility, "on-commit");
    }

    #[test]
    fn strict_durability_rejects_copy_private() {
        let project = tempfile::tempdir().unwrap();
        let coordinator = SessionCoordinator::new(project.path(), None).unwrap();
        let error = coordinator
            .start_with_durability(
                None,
                Some(FilesystemMode::CopyPrivate),
                true,
                DurabilityMode::Strict,
                None,
            )
            .unwrap_err();
        assert!(matches!(error, Error::InvalidDurability(_)));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    #[test]
    fn required_cbcow_mode_does_not_fallback() {
        let project = tempfile::tempdir().unwrap();
        let coordinator = SessionCoordinator::new(project.path(), None).unwrap();
        let error = coordinator
            .start(None, Some(FilesystemMode::CbcowThrough), true)
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    #[test]
    fn strict_cbcow_mode_does_not_fallback() {
        let project = tempfile::tempdir().unwrap();
        let coordinator = SessionCoordinator::new(project.path(), None).unwrap();
        let error = coordinator
            .start_with_durability(
                None,
                Some(FilesystemMode::CbcowThrough),
                false,
                DurabilityMode::Strict,
                None,
            )
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    #[test]
    fn default_mode_records_cbcow_fallback() {
        let project = tempfile::tempdir().unwrap();
        let coordinator = SessionCoordinator::new(project.path(), None).unwrap();
        let info = coordinator.start(None, None, false).unwrap();
        assert_eq!(
            info.filesystem.requested_fs_mode,
            FilesystemMode::CbcowThrough
        );
        assert_eq!(
            info.filesystem.resolved_fs_mode,
            FilesystemMode::CopyPrivate
        );
        assert!(info.filesystem.fallback_reason.is_some());
    }
}

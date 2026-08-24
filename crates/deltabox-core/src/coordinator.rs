use crate::durable::DurableSessionManager;
use crate::error::{Error, Result};
#[cfg(target_os = "linux")]
use crate::git_session::GitSessionManager;
use crate::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, DurabilityMode, ExecParams, ExecResult, FilesystemInfo,
    FilesystemMode, FsReadResult, ReviewBeginParams, SessionInfo,
};
use crate::session::SessionMeta;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

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
}

pub struct SessionCoordinator {
    project: PathBuf,
    routes: RwLock<HashMap<String, FilesystemMode>>,
    copy_private: DurableSessionManager,
    #[cfg(target_os = "linux")]
    cbcow: crate::cbcow::Manager,
    #[cfg(target_os = "macos")]
    macos_cbcow: crate::macos_cbcow::Manager,
    #[cfg(target_os = "linux")]
    git: GitSessionManager,
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
        let helper_bin = options
            .helper_bin
            .clone()
            .unwrap_or(std::env::current_exe()?);
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
        #[cfg(target_os = "linux")]
        let git = GitSessionManager::new(
            &project,
            options
                .state_dir
                .clone()
                .map_or_else(default_state_base, Ok)?,
            options
                .runtime_dir
                .clone()
                .unwrap_or_else(default_runtime_base),
        )?;
        #[cfg(target_os = "macos")]
        let macos_cbcow = crate::macos_cbcow::Manager::new_with_execution_mode(
            &project,
            helper_bin,
            options.command_execution,
            options.state_dir,
            options.embedded_fast,
        )?;
        Ok(Self {
            project,
            routes: RwLock::new(HashMap::new()),
            copy_private,
            #[cfg(target_os = "linux")]
            cbcow,
            #[cfg(target_os = "macos")]
            macos_cbcow,
            #[cfg(target_os = "linux")]
            git,
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
                self.copy_private
                    .start_with_filesystem(policy, FilesystemInfo::copy_private(requested, None))
            }
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => self.start_cbcow(
                policy,
                requested,
                require_requested,
                durability,
                group_commit_ms,
            ),
            FilesystemMode::Git => {
                if durability == DurabilityMode::Strict {
                    return Err(Error::Unsupported(
                        "git mode supports fast durability only".into(),
                    ));
                }
                #[cfg(target_os = "linux")]
                {
                    self.git.start(policy, Some("bubblewrap"), group_commit_ms)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err(Error::Unsupported("git mode requires Linux".into()))
                }
            }
        }?;
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .insert(info.session_id.clone(), info.filesystem.resolved_fs_mode);
        Ok(info)
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.status(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.status(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.status(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.status(session_id),
        }
    }

    pub fn stop(&self, session_id: &str) -> Result<SessionInfo> {
        let info = match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.stop(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.stop(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.stop(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.stop(session_id),
        }?;
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .remove(session_id);
        Ok(info)
    }

    pub fn remove_stopped(&self, session_id: &str) -> Result<()> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.remove_stopped(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.remove_stopped(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.remove_stopped(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.remove_stopped(session_id),
        }
    }

    pub fn exec(&self, params: ExecParams) -> Result<ExecResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.exec(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.exec(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.exec(params),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.exec(params),
        }
    }

    pub fn read_file(&self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.read_file(session_id, path),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.read_file(session_id, path),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.read_file(session_id, path),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.read_file(session_id, path),
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
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.read_file_bytes(session_id, path),
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
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.write_file_bytes(session_id, path, content),
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
            #[cfg(target_os = "linux")]
            Route::Git(_) => Err(Error::Unsupported(
                "direct mount tracking is unavailable in git mode".into(),
            )),
        }
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.checkpoint(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.checkpoint(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.checkpoint(params),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.checkpoint(params),
        }
    }

    pub fn diff(&self, params: DiffParams) -> Result<DiffResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.diff(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.diff(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.diff(params),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.diff(params),
        }
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.commit(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.commit(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.commit(params),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.commit(params),
        }
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.abort(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.abort(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.abort(params),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.abort(params),
        }
    }

    pub fn begin_review(&self, params: ReviewBeginParams) -> Result<ActiveReview> {
        match self.route(&params.session_id)? {
            Route::CopyPrivate(manager) => manager.begin_review(params),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.begin_review(params),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.begin_review(params),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.begin_review(params),
        }
    }

    pub fn review_status(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_status(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_status(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_status(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.review_status(session_id),
        }
    }

    pub fn review_diff(&self, session_id: &str) -> Result<DiffResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_diff(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_diff(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_diff(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.review_diff(session_id),
        }
    }

    pub fn review_commit(&self, session_id: &str) -> Result<CommitResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_commit(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_commit(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_commit(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.review_commit(session_id),
        }
    }

    pub fn review_abort(&self, session_id: &str) -> Result<AbortResult> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_abort(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_abort(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_abort(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.review_abort(session_id),
        }
    }

    pub fn review_skip(&self, session_id: &str) -> Result<ActiveReview> {
        match self.route(session_id)? {
            Route::CopyPrivate(manager) => manager.review_skip(session_id),
            #[cfg(target_os = "linux")]
            Route::Cbcow(manager) => manager.review_skip(session_id),
            #[cfg(target_os = "macos")]
            Route::MacosCbcow(manager) => manager.review_skip(session_id),
            #[cfg(target_os = "linux")]
            Route::Git(manager) => manager.review_skip(session_id),
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
        #[cfg(target_os = "linux")]
        if self.git.is_git_session(session_id)? {
            self.routes
                .write()
                .expect("coordinator route cache poisoned")
                .insert(session_id.into(), FilesystemMode::Git);
            return Ok(Route::Git(&self.git));
        }
        let path = self
            .project
            .join(".deltabox")
            .join(session_id)
            .join("session.json");
        let meta: SessionMeta = serde_json::from_slice(&fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::SessionNotFound(session_id.into())
            } else {
                Error::Io(error)
            }
        })?)?;
        let mode = meta.filesystem.resolved_fs_mode;
        self.routes
            .write()
            .expect("coordinator route cache poisoned")
            .insert(session_id.into(), mode);
        self.route_mode(mode)
    }

    fn route_mode(&self, mode: FilesystemMode) -> Result<Route<'_>> {
        match mode {
            FilesystemMode::CopyPrivate => Ok(Route::CopyPrivate(&self.copy_private)),
            #[cfg(target_os = "linux")]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => {
                Ok(Route::Cbcow(&self.cbcow))
            }
            #[cfg(target_os = "macos")]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => {
                Ok(Route::MacosCbcow(&self.macos_cbcow))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate => Err(Error::Unsupported(
                "this CB-CoW session requires Linux".into(),
            )),
            #[cfg(target_os = "linux")]
            FilesystemMode::Git => Ok(Route::Git(&self.git)),
            #[cfg(not(target_os = "linux"))]
            FilesystemMode::Git => Err(Error::Unsupported("git mode requires Linux".into())),
        }
    }
}

enum Route<'a> {
    CopyPrivate(&'a DurableSessionManager),
    #[cfg(target_os = "linux")]
    Cbcow(&'a crate::cbcow::Manager),
    #[cfg(target_os = "macos")]
    MacosCbcow(&'a crate::macos_cbcow::Manager),
    #[cfg(target_os = "linux")]
    Git(&'a GitSessionManager),
}

fn default_mode() -> FilesystemMode {
    FilesystemMode::CbcowThrough
}

#[cfg(target_os = "linux")]
fn default_state_base() -> Result<PathBuf> {
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

#[cfg(target_os = "linux")]
fn default_runtime_base() -> PathBuf {
    if let Some(path) = std::env::var_os("DELTABOX_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/deltabox-{}", unsafe { libc::geteuid() })))
        .join("deltabox-lite")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn required_cbcow_mode_does_not_fallback() {
        let project = tempfile::tempdir().unwrap();
        let coordinator = SessionCoordinator::new(project.path(), None).unwrap();
        let error = coordinator
            .start(None, Some(FilesystemMode::CbcowThrough), true)
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

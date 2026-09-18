//! Node-API surface for embedding Deltabox in a Node.js host process.

use deltabox_core::changeset::{Change, ChangeKind};
use deltabox_core::coordinator::{CommandExecutionMode, CoordinatorOptions, SessionCoordinator};
use deltabox_core::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, DurabilityMode, ExecParams, ExecResult, FilesystemMode,
    ReviewBeginParams, ReviewKind, SessionInfo, SessionStatus,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn native_error(error: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}

async fn run_blocking<T>(
    operation: impl FnOnce() -> napi::Result<T> + Send + 'static,
) -> napi::Result<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(native_error)?
}

#[napi(object)]
pub struct NativeEnvironmentEntry {
    pub key: String,
    pub value: String,
}

#[napi(object)]
pub struct NativeSessionInfo {
    #[napi(js_name = "session_id")]
    pub session_id: String,
    pub project: String,
    #[napi(js_name = "started_at_ms")]
    pub started_at_ms: f64,
    pub status: String,
    #[napi(js_name = "checkpoint_count")]
    pub checkpoint_count: f64,
    #[napi(js_name = "pending_changes")]
    pub pending_changes: f64,
    #[napi(js_name = "requested_fs_mode")]
    pub requested_fs_mode: String,
    #[napi(js_name = "resolved_fs_mode")]
    pub resolved_fs_mode: String,
    #[napi(js_name = "host_visibility")]
    pub host_visibility: String,
    pub durability: String,
    #[napi(js_name = "group_commit_ms")]
    pub group_commit_ms: Option<f64>,
    pub isolation: String,
    #[napi(js_name = "isolation_provider")]
    pub isolation_provider: Either<String, Null>,
    #[napi(js_name = "state_provider")]
    pub state_provider: Either<String, Null>,
    #[napi(js_name = "fallback_reason")]
    pub fallback_reason: Either<String, Null>,
    #[napi(js_name = "rollback_guarantee")]
    pub rollback_guarantee: String,
    pub tainted: bool,
    pub mountpoint: Option<String>,
}

#[napi(object)]
pub struct NativeExecResult {
    #[napi(js_name = "exit_code")]
    pub exit_code: Either<i32, Null>,
    pub stdout: String,
    pub stderr: String,
    #[napi(js_name = "duration_ms")]
    pub duration_ms: f64,
    pub truncated: bool,
}

#[napi(object)]
pub struct NativeActiveReview {
    #[napi(js_name = "checkpoint_id")]
    pub checkpoint_id: String,
    pub kind: String,
    pub label: Either<String, Null>,
    #[napi(js_name = "started_at_ms")]
    pub started_at_ms: f64,
}

#[napi(object)]
pub struct NativeCheckpointInfo {
    pub id: String,
    pub label: Either<String, Null>,
    #[napi(js_name = "created_at_ms")]
    pub created_at_ms: f64,
    #[napi(js_name = "change_count")]
    pub change_count: f64,
    pub committed: bool,
}

#[napi(object)]
pub struct NativeChange {
    pub path: String,
    pub kind: String,
    #[napi(js_name = "old_size")]
    pub old_size: Either<f64, Null>,
    #[napi(js_name = "new_size")]
    pub new_size: Either<f64, Null>,
    #[napi(js_name = "is_binary")]
    pub is_binary: bool,
    #[napi(js_name = "added_lines")]
    pub added_lines: f64,
    #[napi(js_name = "removed_lines")]
    pub removed_lines: f64,
    #[napi(js_name = "text_diff")]
    pub text_diff: Option<String>,
}

#[napi(object)]
pub struct NativeDiffResult {
    pub from: Either<String, Null>,
    pub to: Either<String, Null>,
    #[napi(js_name = "pending_changes")]
    pub pending_changes: f64,
    pub changes: Vec<NativeChange>,
}

#[napi(object)]
pub struct NativeCommitResult {
    #[napi(js_name = "checkpoint_id")]
    pub checkpoint_id: String,
    pub applied: f64,
}

#[napi(object)]
pub struct NativeAbortResult {
    #[napi(js_name = "checkpoint_id")]
    pub checkpoint_id: String,
    pub restored: f64,
}

/// Configuration snapshot for one embedded coordinator, fixed at construction.
///
/// `fs_mode` picks the storage mode every session on this coordinator starts
/// in; `isolation` picks how commands run. Both are constructor-scoped because
/// the coordinator and its active filesystem state live for the whole process.
#[napi(object)]
pub struct NativeDeltaboxOptions {
    #[napi(js_name = "stateDir")]
    pub state_dir: Option<String>,
    #[napi(js_name = "runtimeDir")]
    pub runtime_dir: Option<String>,
    #[napi(js_name = "fsMode")]
    pub fs_mode: Option<String>,
    pub isolation: Option<String>,
}

/// How a command reaches the host. `Deltabox` keeps the session's own sandbox
/// and deny set; `None` lets the command share the Node process ambient
/// environment, which is why the caller has to ask for it by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeIsolation {
    Deltabox,
    None,
}

impl NativeIsolation {
    fn name(self) -> &'static str {
        match self {
            Self::Deltabox => "deltabox",
            Self::None => "none",
        }
    }
}

struct ResolvedNativeOptions {
    state_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    fs_mode: FilesystemMode,
    isolation: NativeIsolation,
}

#[napi]
pub struct NativeDeltabox {
    coordinator: Arc<SessionCoordinator>,
    fs_mode: FilesystemMode,
    isolation: NativeIsolation,
}

#[napi]
impl NativeDeltabox {
    #[napi(constructor)]
    pub fn new(
        project_root: String,
        helper_binary: String,
        options: Option<NativeDeltaboxOptions>,
    ) -> napi::Result<Self> {
        let options = resolve_native_options(options)?;
        let command_execution = match options.isolation {
            NativeIsolation::Deltabox => CommandExecutionMode::Sandboxed,
            NativeIsolation::None => CommandExecutionMode::Trusted,
        };
        let coordinator = SessionCoordinator::new_with_options(
            Path::new(&project_root),
            CoordinatorOptions {
                helper_bin: Some(PathBuf::from(helper_binary)),
                command_execution,
                state_dir: options.state_dir,
                runtime_dir: options.runtime_dir,
                embedded_fast: true,
            },
        )
        .map_err(native_error)?;
        Ok(Self {
            coordinator: Arc::new(coordinator),
            fs_mode: options.fs_mode,
            isolation: options.isolation,
        })
    }

    #[napi]
    pub async fn start_session(
        &self,
        policy: Option<String>,
        durability: Option<String>,
        group_commit_ms: Option<f64>,
        filesystem_mode: Option<String>,
        require_filesystem_mode: Option<bool>,
    ) -> napi::Result<NativeSessionInfo> {
        let coordinator = Arc::clone(&self.coordinator);
        let durability = parse_durability(durability.as_deref())?;
        let group_commit_ms = parse_nonnegative_integer("groupCommitMs", group_commit_ms)?;
        let (filesystem_mode, require_filesystem_mode) = parse_filesystem_request(
            filesystem_mode.as_deref(),
            require_filesystem_mode,
            self.fs_mode,
        )?;
        let isolation = self.isolation;
        run_blocking(move || {
            coordinator
                .start_with_durability(
                    policy.as_deref().map(Path::new),
                    filesystem_mode,
                    require_filesystem_mode,
                    durability,
                    group_commit_ms,
                )
                .map(|value| session_info(value, isolation))
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn stop_session(&self, session_id: String) -> napi::Result<NativeSessionInfo> {
        let coordinator = Arc::clone(&self.coordinator);
        let isolation = self.isolation;
        run_blocking(move || {
            coordinator
                .stop(&session_id)
                .map(|value| session_info(value, isolation))
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn session_status(&self, session_id: String) -> napi::Result<NativeSessionInfo> {
        let coordinator = Arc::clone(&self.coordinator);
        let isolation = self.isolation;
        run_blocking(move || {
            coordinator
                .status(&session_id)
                .map(|value| session_info(value, isolation))
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn exec(
        &self,
        session_id: String,
        argv: Vec<String>,
        cwd: Option<String>,
        environment: Option<Vec<NativeEnvironmentEntry>>,
        label: Option<String>,
    ) -> napi::Result<NativeExecResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .exec(ExecParams {
                    session_id,
                    argv,
                    cwd: cwd.map(PathBuf::from),
                    env: environment.map(|values| {
                        values
                            .into_iter()
                            .map(|entry| (entry.key, entry.value))
                            .collect()
                    }),
                    label,
                })
                .map(exec_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn read_file(&self, session_id: String, path: String) -> napi::Result<Buffer> {
        let coordinator = Arc::clone(&self.coordinator);
        let output = run_blocking(move || {
            coordinator
                .read_file_bytes(&session_id, Path::new(&path))
                .map_err(native_error)
        })
        .await?;
        Ok(output.into())
    }

    #[napi]
    pub async fn write_file(
        &self,
        session_id: String,
        path: String,
        content: Buffer,
    ) -> napi::Result<()> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .write_file_bytes(&session_id, Path::new(&path), &content)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn record_file_changes(
        &self,
        session_id: String,
        paths: Vec<String>,
    ) -> napi::Result<()> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            let paths = paths.into_iter().map(PathBuf::from).collect::<Vec<_>>();
            coordinator
                .record_file_changes(&session_id, &paths)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn checkpoint(
        &self,
        session_id: String,
        label: Option<String>,
    ) -> napi::Result<NativeCheckpointInfo> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .checkpoint(CheckpointParams { session_id, label })
                .map(checkpoint_info)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn diff(
        &self,
        session_id: String,
        from: Option<String>,
        to: Option<String>,
    ) -> napi::Result<NativeDiffResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .diff(DiffParams {
                    session_id,
                    from,
                    to,
                })
                .map(diff_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn commit(
        &self,
        session_id: String,
        checkpoint: Option<String>,
    ) -> napi::Result<NativeCommitResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .commit(CommitParams {
                    session_id,
                    checkpoint,
                    ours: false,
                })
                .map(commit_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn abort(
        &self,
        session_id: String,
        checkpoint: Option<String>,
    ) -> napi::Result<NativeAbortResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .abort(AbortParams {
                    session_id,
                    checkpoint,
                })
                .map(abort_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn begin_review(
        &self,
        session_id: String,
        kind: String,
        label: Option<String>,
    ) -> napi::Result<NativeActiveReview> {
        let coordinator = Arc::clone(&self.coordinator);
        let kind = parse_review_kind(&kind)?;
        run_blocking(move || {
            coordinator
                .begin_review(ReviewBeginParams {
                    session_id,
                    kind,
                    label,
                })
                .map(active_review)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn review_status(
        &self,
        session_id: String,
    ) -> napi::Result<Either<NativeActiveReview, Null>> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .review_status(&session_id)
                .map(|value| nullable(value.map(active_review)))
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn review_diff(&self, session_id: String) -> napi::Result<NativeDiffResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .review_diff(&session_id)
                .map(diff_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn review_commit(&self, session_id: String) -> napi::Result<NativeCommitResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .review_commit(&session_id)
                .map(commit_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn review_abort(&self, session_id: String) -> napi::Result<NativeAbortResult> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .review_abort(&session_id)
                .map(abort_result)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn review_skip(&self, session_id: String) -> napi::Result<NativeActiveReview> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .review_skip(&session_id)
                .map(active_review)
                .map_err(native_error)
        })
        .await
    }
}

fn parse_durability(value: Option<&str>) -> napi::Result<DurabilityMode> {
    match value.unwrap_or("fast") {
        "fast" => Ok(DurabilityMode::Fast),
        "strict" => Ok(DurabilityMode::Strict),
        value => Err(napi::Error::from_reason(format!(
            "unsupported durability mode {value:?}"
        ))),
    }
}

fn parse_filesystem_request(
    value: Option<&str>,
    required: Option<bool>,
    default_mode: FilesystemMode,
) -> napi::Result<(Option<FilesystemMode>, bool)> {
    match value {
        None => Ok((Some(default_mode), required.unwrap_or(true))),
        Some("auto") => Ok((None, required.unwrap_or(false))),
        Some("cbcow-through") => Ok((
            Some(FilesystemMode::CbcowThrough),
            required.unwrap_or(false),
        )),
        Some("cbcow-private") => Ok((
            Some(FilesystemMode::CbcowPrivate),
            required.unwrap_or(false),
        )),
        Some("copy-private") => Ok((Some(FilesystemMode::CopyPrivate), required.unwrap_or(false))),
        Some(value) => Err(napi::Error::from_reason(format!(
            "unsupported filesystem mode {value:?}"
        ))),
    }
}

/// Resolve the constructor snapshot, rejecting modes this build cannot serve.
///
/// Only the modes the coordinator implements are accepted: the CB-CoW family
/// and `copy-private` for storage, and the Deltabox or ambient sandbox for
/// command isolation. Anything else fails at construction rather than at the
/// first call, so a host cannot half-configure a coordinator.
fn resolve_native_options(
    options: Option<NativeDeltaboxOptions>,
) -> napi::Result<ResolvedNativeOptions> {
    let options = options.unwrap_or(NativeDeltaboxOptions {
        state_dir: None,
        runtime_dir: None,
        fs_mode: None,
        isolation: None,
    });
    let fs_mode = match options.fs_mode.as_deref().unwrap_or("cbcow-through") {
        "cbcow-through" => FilesystemMode::CbcowThrough,
        "cbcow-private" => FilesystemMode::CbcowPrivate,
        "copy-private" => FilesystemMode::CopyPrivate,
        value => {
            return Err(napi::Error::from_reason(format!(
                "unsupported native fsMode {value:?}"
            )))
        }
    };
    let isolation = match options.isolation.as_deref().unwrap_or("deltabox") {
        "deltabox" => NativeIsolation::Deltabox,
        "none" => NativeIsolation::None,
        value => {
            return Err(napi::Error::from_reason(format!(
                "unsupported native isolation {value:?}"
            )))
        }
    };
    Ok(ResolvedNativeOptions {
        state_dir: options.state_dir.map(PathBuf::from),
        runtime_dir: options.runtime_dir.map(PathBuf::from),
        fs_mode,
        isolation,
    })
}

fn parse_review_kind(value: &str) -> napi::Result<ReviewKind> {
    match value {
        "task" => Ok(ReviewKind::Task),
        "tool" => Ok(ReviewKind::Tool),
        "risky" => Ok(ReviewKind::Risky),
        value => Err(napi::Error::from_reason(format!(
            "unsupported review kind {value:?}"
        ))),
    }
}

fn parse_nonnegative_integer(name: &str, value: Option<f64>) -> napi::Result<Option<u64>> {
    value
        .map(|value| {
            if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > u64::MAX as f64
            {
                return Err(napi::Error::from_reason(format!(
                    "{name} must be a non-negative integer"
                )));
            }
            Ok(value as u64)
        })
        .transpose()
}

fn filesystem_mode(value: FilesystemMode) -> String {
    match value {
        FilesystemMode::CbcowThrough => "cbcow-through",
        FilesystemMode::CbcowPrivate => "cbcow-private",
        FilesystemMode::CopyPrivate => "copy-private",
    }
    .into()
}

/// Project one core session record onto the addon's shape.
///
/// The embedded coordinator knows the isolation profile it was constructed
/// with, while the core record only carries the resolved OS mechanism, so the
/// profile is passed in: an ambient-sandbox coordinator reports `none` instead
/// of claiming a sandbox its commands never enter.
fn session_info(value: SessionInfo, isolation: NativeIsolation) -> NativeSessionInfo {
    let filesystem = value.filesystem;
    NativeSessionInfo {
        session_id: value.session_id,
        project: value.project.to_string_lossy().into_owned(),
        started_at_ms: value.started_at_ms as f64,
        status: session_status(value.status),
        checkpoint_count: value.checkpoint_count as f64,
        pending_changes: value.pending_changes as f64,
        requested_fs_mode: filesystem_mode(filesystem.requested_fs_mode),
        resolved_fs_mode: filesystem_mode(filesystem.resolved_fs_mode),
        host_visibility: filesystem.host_visibility,
        durability: filesystem.durability,
        group_commit_ms: filesystem.group_commit_ms.map(|value| value as f64),
        isolation: isolation.name().into(),
        isolation_provider: match isolation {
            NativeIsolation::Deltabox => nullable(filesystem.isolation_provider),
            NativeIsolation::None => Either::B(Null),
        },
        state_provider: nullable(filesystem.state_provider),
        fallback_reason: nullable(filesystem.fallback_reason),
        rollback_guarantee: filesystem.rollback_guarantee,
        tainted: filesystem.tainted,
        mountpoint: filesystem
            .mountpoint
            .map(|path| path.to_string_lossy().into_owned()),
    }
}

fn session_status(value: SessionStatus) -> String {
    match value {
        SessionStatus::Running => "running",
        SessionStatus::Stopped => "stopped",
    }
    .into()
}

fn exec_result(value: ExecResult) -> NativeExecResult {
    NativeExecResult {
        exit_code: nullable(value.exit_code),
        stdout: value.stdout,
        stderr: value.stderr,
        duration_ms: value.duration_ms as f64,
        truncated: value.truncated,
    }
}

fn review_kind(value: ReviewKind) -> String {
    match value {
        ReviewKind::Task => "task",
        ReviewKind::Tool => "tool",
        ReviewKind::Risky => "risky",
    }
    .into()
}

fn active_review(value: ActiveReview) -> NativeActiveReview {
    NativeActiveReview {
        checkpoint_id: value.checkpoint_id,
        kind: review_kind(value.kind),
        label: nullable(value.label),
        started_at_ms: value.started_at_ms as f64,
    }
}

fn checkpoint_info(value: CheckpointInfo) -> NativeCheckpointInfo {
    NativeCheckpointInfo {
        id: value.id,
        label: nullable(value.label),
        created_at_ms: value.created_at_ms as f64,
        change_count: value.change_count as f64,
        committed: value.committed,
    }
}

fn change_kind(value: ChangeKind) -> String {
    match value {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
    .into()
}

fn change(value: Change) -> NativeChange {
    NativeChange {
        path: value.path,
        kind: change_kind(value.kind),
        old_size: nullable(value.old_size.map(|size| size as f64)),
        new_size: nullable(value.new_size.map(|size| size as f64)),
        is_binary: value.is_binary,
        added_lines: value.added_lines as f64,
        removed_lines: value.removed_lines as f64,
        text_diff: value.text_diff,
    }
}

fn diff_result(value: DiffResult) -> NativeDiffResult {
    NativeDiffResult {
        from: nullable(value.from),
        to: nullable(value.to),
        pending_changes: value.pending_changes as f64,
        changes: value.changes.into_iter().map(change).collect(),
    }
}

fn nullable<T>(value: Option<T>) -> Either<T, Null> {
    value.map_or(Either::B(Null), Either::A)
}

fn commit_result(value: CommitResult) -> NativeCommitResult {
    NativeCommitResult {
        checkpoint_id: value.checkpoint_id,
        applied: value.applied as f64,
    }
}

fn abort_result(value: AbortResult) -> NativeAbortResult {
    NativeAbortResult {
        checkpoint_id: value.checkpoint_id,
        restored: value.restored as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(fs_mode: Option<&str>, isolation: Option<&str>) -> NativeDeltaboxOptions {
        NativeDeltaboxOptions {
            state_dir: Some("/state".into()),
            runtime_dir: Some("/runtime".into()),
            fs_mode: fs_mode.map(str::to_string),
            isolation: isolation.map(str::to_string),
        }
    }

    #[test]
    fn native_options_default_to_cbcow_through_with_deltabox_isolation() {
        let resolved = resolve_native_options(None).unwrap();
        assert_eq!(resolved.fs_mode, FilesystemMode::CbcowThrough);
        assert_eq!(resolved.isolation, NativeIsolation::Deltabox);
        assert_eq!(resolved.state_dir, None);
        assert_eq!(resolved.runtime_dir, None);
    }

    #[test]
    fn native_options_accept_the_private_modes_and_the_ambient_sandbox() {
        for fs_mode in ["cbcow-through", "cbcow-private", "copy-private"] {
            let resolved = resolve_native_options(Some(options(Some(fs_mode), Some("none"))))
                .unwrap();
            assert_eq!(resolved.isolation, NativeIsolation::None);
            assert_eq!(resolved.state_dir, Some(PathBuf::from("/state")));
            assert_eq!(resolved.runtime_dir, Some(PathBuf::from("/runtime")));
        }
    }

    #[test]
    fn native_options_reject_modes_this_build_does_not_serve() {
        for fs_mode in ["git", "auto", "cbcow"] {
            assert!(resolve_native_options(Some(options(Some(fs_mode), None))).is_err());
        }
        for isolation in ["bubblewrap", "docker", "trusted"] {
            assert!(resolve_native_options(Some(options(None, Some(isolation)))).is_err());
        }
    }
}

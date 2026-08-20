//! Node-API surface for embedding Deltabox in a Node.js host process.

use deltabox_core::changeset::{Change, ChangeKind};
use deltabox_core::coordinator::{CommandExecutionMode, CoordinatorOptions, SessionCoordinator};
use deltabox_core::protocol::{
    AbortResult, ActiveReview, CommitResult, DiffResult, DurabilityMode, ExecParams, ExecResult,
    FilesystemMode, ReviewBeginParams, ReviewKind, SessionInfo,
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
    #[napi(js_name = "resolved_fs_mode")]
    pub resolved_fs_mode: String,
    pub durability: String,
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

#[napi]
pub struct NativeDeltabox {
    coordinator: Arc<SessionCoordinator>,
}

#[napi]
impl NativeDeltabox {
    #[napi(constructor)]
    pub fn new(
        project_root: String,
        helper_binary: String,
        trusted: Option<bool>,
        state_dir: Option<String>,
        runtime_dir: Option<String>,
    ) -> napi::Result<Self> {
        let command_execution = if trusted.unwrap_or(true) {
            CommandExecutionMode::Trusted
        } else {
            CommandExecutionMode::Sandboxed
        };
        let coordinator = SessionCoordinator::new_with_options(
            Path::new(&project_root),
            CoordinatorOptions {
                helper_bin: Some(PathBuf::from(helper_binary)),
                command_execution,
                state_dir: state_dir.map(PathBuf::from),
                runtime_dir: runtime_dir.map(PathBuf::from),
                embedded_fast: true,
            },
        )
        .map_err(native_error)?;
        Ok(Self {
            coordinator: Arc::new(coordinator),
        })
    }

    #[napi]
    pub async fn start_session(
        &self,
        policy: Option<String>,
        durability: Option<String>,
        group_commit_ms: Option<f64>,
    ) -> napi::Result<NativeSessionInfo> {
        let coordinator = Arc::clone(&self.coordinator);
        let durability = parse_durability(durability.as_deref())?;
        let group_commit_ms = parse_nonnegative_integer("groupCommitMs", group_commit_ms)?;
        run_blocking(move || {
            coordinator
                .start_with_durability(
                    policy.as_deref().map(Path::new),
                    Some(FilesystemMode::CbcowThrough),
                    true,
                    durability,
                    group_commit_ms,
                )
                .map(session_info)
                .map_err(native_error)
        })
        .await
    }

    #[napi]
    pub async fn stop_session(&self, session_id: String) -> napi::Result<NativeSessionInfo> {
        let coordinator = Arc::clone(&self.coordinator);
        run_blocking(move || {
            coordinator
                .stop(&session_id)
                .map(session_info)
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

fn session_info(value: SessionInfo) -> NativeSessionInfo {
    NativeSessionInfo {
        session_id: value.session_id,
        project: value.project.to_string_lossy().into_owned(),
        resolved_fs_mode: filesystem_mode(value.filesystem.resolved_fs_mode),
        durability: value.filesystem.durability,
        mountpoint: value
            .filesystem
            .mountpoint
            .map(|path| path.to_string_lossy().into_owned()),
    }
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

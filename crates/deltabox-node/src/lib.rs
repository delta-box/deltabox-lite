//! Node-API surface for embedding Deltabox in a Node.js host process.

use deltabox_core::changeset::{Change, ChangeKind};
use deltabox_core::coordinator::{CommandExecutionMode, CoordinatorOptions, SessionCoordinator};
use deltabox_core::protocol::{
    AbortResult, ActiveReview, CommitResult, DiffResult, DurabilityMode, ExecParams, ExecResult,
    FilesystemMode, ReviewBeginParams, ReviewKind, SessionInfo,
};
use napi::bindgen_prelude::*;
use napi::{Env, Task};
use napi_derive::napi;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn native_error(error: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}

pub struct NativeTask<T> {
    operation: Option<Box<dyn FnOnce() -> napi::Result<T> + Send>>,
}

impl<T> NativeTask<T> {
    fn new(operation: impl FnOnce() -> napi::Result<T> + Send + 'static) -> Self {
        Self {
            operation: Some(Box::new(operation)),
        }
    }
}

impl<T> Task for NativeTask<T>
where
    T: ToNapiValue + TypeName + Send + 'static,
{
    type Output = T;
    type JsValue = T;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        self.operation.take().expect("native task is computed once")()
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct ReadFileTask {
    coordinator: Arc<SessionCoordinator>,
    session_id: String,
    path: String,
}

impl Task for ReadFileTask {
    type Output = Vec<u8>;
    type JsValue = Buffer;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        self.coordinator
            .read_file_bytes(&self.session_id, Path::new(&self.path))
            .map_err(native_error)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(output.into())
    }
}

#[napi(object)]
pub struct NativeEnvironmentEntry {
    pub key: String,
    pub value: String,
}

#[napi(object)]
pub struct NativeSessionInfo {
    pub session_id: String,
    pub project: String,
    pub resolved_fs_mode: String,
    pub durability: String,
    pub mountpoint: Option<String>,
}

#[napi(object)]
pub struct NativeExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: f64,
    pub truncated: bool,
}

#[napi(object)]
pub struct NativeActiveReview {
    pub checkpoint_id: String,
    pub kind: String,
    pub label: Option<String>,
    pub started_at_ms: f64,
}

#[napi(object)]
pub struct NativeChange {
    pub path: String,
    pub kind: String,
    pub old_size: Option<f64>,
    pub new_size: Option<f64>,
    pub is_binary: bool,
    pub added_lines: f64,
    pub removed_lines: f64,
    pub text_diff: Option<String>,
}

#[napi(object)]
pub struct NativeDiffResult {
    pub from: Option<String>,
    pub to: Option<String>,
    pub pending_changes: f64,
    pub changes: Vec<NativeChange>,
}

#[napi(object)]
pub struct NativeCommitResult {
    pub checkpoint_id: String,
    pub applied: f64,
}

#[napi(object)]
pub struct NativeAbortResult {
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
            },
        )
        .map_err(native_error)?;
        Ok(Self {
            coordinator: Arc::new(coordinator),
        })
    }

    #[napi]
    pub fn start_session(
        &self,
        policy: Option<String>,
        durability: Option<String>,
        group_commit_ms: Option<f64>,
    ) -> napi::Result<AsyncTask<NativeTask<NativeSessionInfo>>> {
        let coordinator = Arc::clone(&self.coordinator);
        let durability = parse_durability(durability.as_deref())?;
        let group_commit_ms = parse_nonnegative_integer("groupCommitMs", group_commit_ms)?;
        Ok(AsyncTask::new(NativeTask::new(move || {
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
        })))
    }

    #[napi]
    pub fn stop_session(&self, session_id: String) -> AsyncTask<NativeTask<NativeSessionInfo>> {
        let coordinator = Arc::clone(&self.coordinator);
        AsyncTask::new(NativeTask::new(move || {
            coordinator
                .stop(&session_id)
                .map(session_info)
                .map_err(native_error)
        }))
    }

    #[napi]
    pub fn exec(
        &self,
        session_id: String,
        argv: Vec<String>,
        cwd: Option<String>,
        environment: Option<Vec<NativeEnvironmentEntry>>,
        label: Option<String>,
    ) -> AsyncTask<NativeTask<NativeExecResult>> {
        let coordinator = Arc::clone(&self.coordinator);
        AsyncTask::new(NativeTask::new(move || {
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
        }))
    }

    #[napi]
    pub fn read_file(&self, session_id: String, path: String) -> AsyncTask<ReadFileTask> {
        AsyncTask::new(ReadFileTask {
            coordinator: Arc::clone(&self.coordinator),
            session_id,
            path,
        })
    }

    #[napi]
    pub fn write_file(
        &self,
        session_id: String,
        path: String,
        content: Buffer,
    ) -> AsyncTask<NativeTask<()>> {
        let coordinator = Arc::clone(&self.coordinator);
        let content = content.to_vec();
        AsyncTask::new(NativeTask::new(move || {
            coordinator
                .write_file(&session_id, Path::new(&path), content)
                .map_err(native_error)
        }))
    }

    #[napi]
    pub fn begin_review(
        &self,
        session_id: String,
        kind: String,
        label: Option<String>,
    ) -> napi::Result<AsyncTask<NativeTask<NativeActiveReview>>> {
        let coordinator = Arc::clone(&self.coordinator);
        let kind = parse_review_kind(&kind)?;
        Ok(AsyncTask::new(NativeTask::new(move || {
            coordinator
                .begin_review(ReviewBeginParams {
                    session_id,
                    kind,
                    label,
                })
                .map(active_review)
                .map_err(native_error)
        })))
    }

    #[napi]
    pub fn review_diff(&self, session_id: String) -> AsyncTask<NativeTask<NativeDiffResult>> {
        let coordinator = Arc::clone(&self.coordinator);
        AsyncTask::new(NativeTask::new(move || {
            coordinator
                .review_diff(&session_id)
                .map(diff_result)
                .map_err(native_error)
        }))
    }

    #[napi]
    pub fn review_commit(&self, session_id: String) -> AsyncTask<NativeTask<NativeCommitResult>> {
        let coordinator = Arc::clone(&self.coordinator);
        AsyncTask::new(NativeTask::new(move || {
            coordinator
                .review_commit(&session_id)
                .map(commit_result)
                .map_err(native_error)
        }))
    }

    #[napi]
    pub fn review_abort(&self, session_id: String) -> AsyncTask<NativeTask<NativeAbortResult>> {
        let coordinator = Arc::clone(&self.coordinator);
        AsyncTask::new(NativeTask::new(move || {
            coordinator
                .review_abort(&session_id)
                .map(abort_result)
                .map_err(native_error)
        }))
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
        exit_code: value.exit_code,
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
        label: value.label,
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
        old_size: value.old_size.map(|size| size as f64),
        new_size: value.new_size.map(|size| size as f64),
        is_binary: value.is_binary,
        added_lines: value.added_lines as f64,
        removed_lines: value.removed_lines as f64,
        text_diff: value.text_diff,
    }
}

fn diff_result(value: DiffResult) -> NativeDiffResult {
    NativeDiffResult {
        from: value.from,
        to: value.to,
        pending_changes: value.pending_changes as f64,
        changes: value.changes.into_iter().map(change).collect(),
    }
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

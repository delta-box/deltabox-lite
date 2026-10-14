use crate::changeset::Change;
use crate::policy::Policy;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub method: Method,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Method {
    #[serde(rename = "session.start")]
    SessionStart(SessionStartParams),
    #[serde(rename = "session.stop")]
    SessionStop(SessionStopParams),
    #[serde(rename = "session.status")]
    SessionStatus(SessionStatusParams),
    #[serde(rename = "session.exec")]
    SessionExec(ExecParams),
    #[serde(rename = "fs.read")]
    FsRead(FsReadParams),
    #[serde(rename = "fs.write")]
    FsWrite(FsWriteParams),
    #[serde(rename = "checkpoint.create")]
    CheckpointCreate(CheckpointParams),
    #[serde(rename = "diff.list")]
    DiffList(DiffParams),
    #[serde(rename = "changes.commit")]
    ChangesCommit(CommitParams),
    #[serde(rename = "changes.abort")]
    ChangesAbort(AbortParams),
    #[serde(rename = "session.sync")]
    SessionSync(SyncParams),
    #[serde(rename = "review.begin")]
    ReviewBegin(ReviewBeginParams),
    #[serde(rename = "review.status")]
    ReviewStatus(ReviewStatusParams),
    #[serde(rename = "review.diff")]
    ReviewDiff(ReviewStatusParams),
    #[serde(rename = "review.commit")]
    ReviewCommit(ReviewStatusParams),
    #[serde(rename = "review.abort")]
    ReviewAbort(ReviewStatusParams),
    #[serde(rename = "review.skip")]
    ReviewSkip(ReviewStatusParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartParams {
    pub project: PathBuf,
    #[serde(default)]
    pub policy: Option<PathBuf>,
    #[serde(default)]
    pub fs_mode: Option<FilesystemMode>,
    #[serde(default)]
    pub require_fs_mode: bool,
    #[serde(default)]
    pub durability: DurabilityMode,
    #[serde(default)]
    pub group_commit_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStopParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatusParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecParams {
    pub session_id: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub label: Option<String>,
    /// Filesystem breadth granted to this command only.
    ///
    /// The session's isolation backend and protected deny set are unchanged:
    /// the workspace still routes through DeltaFS and the backing project,
    /// engine state, and helper endpoints stay denied.
    #[serde(default)]
    pub execution_profile: ExecutionProfile,
}

/// Filesystem breadth granted to one command execution.
///
/// Profiles widen what a command may reach; they never relax the protected
/// deny set that keeps the session and its rollback data authoritative.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionProfile {
    /// Write access limited to the session workspace.
    #[default]
    WorkspaceWrite,
    /// Workspace writes plus the rest of the filesystem.
    ReadWrite,
}

impl ExecutionProfile {
    /// For modes that isolate a backing project, reject a widening profile
    /// whose working directory overlaps that project. Direct through-mode
    /// writers do not use this check: the project is their tracked workspace.
    ///
    /// `read-write` grants the filesystem root, so the backend must keep
    /// denying the backing project to preserve the workspace guarantee. When
    /// the session's view lives inside that project, a deny on the project
    /// would also cover the view, so the profile cannot be expressed and fails
    /// closed instead of exposing the host tree.
    pub fn ensure_backing_project_is_deniable(
        self,
        view: &std::path::Path,
        host_project: &std::path::Path,
    ) -> crate::Result<()> {
        if self == Self::ReadWrite && view.starts_with(host_project) {
            return Err(crate::Error::Unsupported(format!(
                "read-write execution requires a working directory outside the denied backing project (working directory {} overlaps backing project {})",
                view.display(),
                host_project.display()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadParams {
    pub session_id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadResult {
    pub content_base64: String,
    pub size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsWriteParams {
    pub session_id: String,
    pub path: PathBuf,
    pub content_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointParams {
    pub session_id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffParams {
    pub session_id: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitParams {
    pub session_id: String,
    #[serde(default)]
    pub checkpoint: Option<String>,
    /// Pre-declared conflict policy: publications by sibling sessions may be
    /// overridden by this session's version without a prior sync, resolved
    /// inside the publish-lock window. Only digest-endorsed sibling content
    /// qualifies — changes made outside deltabox still fail closed.
    #[serde(default)]
    pub ours: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortParams {
    pub session_id: String,
    #[serde(default)]
    pub checkpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewBeginParams {
    pub session_id: String,
    pub kind: ReviewKind,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStatusParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Task,
    Tool,
    Risky,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveReview {
    pub checkpoint_id: String,
    pub kind: ReviewKind,
    pub label: Option<String>,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: i32,
    pub message: String,
    /// Structured error payload; today only commit conflicts populate it
    /// (`{"type":"project-conflict", ...ConflictReport}`). Absent otherwise so
    /// pre-existing consumers see byte-identical errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub project: PathBuf,
    /// Real host project when `project` is a private transaction or shadow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_project: Option<PathBuf>,
    pub store_dir: PathBuf,
    pub policy: Policy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_deny_paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project: PathBuf,
    pub started_at_ms: u64,
    pub status: SessionStatus,
    pub checkpoint_count: usize,
    pub pending_changes: usize,
    #[serde(flatten)]
    pub filesystem: FilesystemInfo,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemMode {
    CbcowThrough,
    CbcowPrivate,
    #[default]
    CopyPrivate,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DurabilityMode {
    #[default]
    Fast,
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemInfo {
    pub requested_fs_mode: FilesystemMode,
    pub resolved_fs_mode: FilesystemMode,
    pub host_visibility: String,
    pub durability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_commit_ms: Option<u64>,
    pub isolation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_provider: Option<String>,
    #[serde(default, alias = "fuse_provider")]
    pub state_provider: Option<String>,
    pub fallback_reason: Option<String>,
    pub rollback_guarantee: String,
    pub tainted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mountpoint: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_pid: Option<u32>,
    #[serde(default)]
    pub helper_restarts: u64,
    #[serde(default)]
    pub mount_namespace: bool,
    /// Landlock protections the running kernel cannot enforce (Linux only).
    /// Absent means full enforcement; consumers must not assume signal or
    /// abstract-socket confinement when this lists the matching scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_protections: Option<Vec<String>>,
}

impl Default for FilesystemInfo {
    fn default() -> Self {
        Self::copy_private(FilesystemMode::CopyPrivate, None)
    }
}

impl FilesystemInfo {
    pub fn with_durability(
        mut self,
        durability: DurabilityMode,
        group_commit_ms: Option<u64>,
    ) -> Self {
        self.durability = match durability {
            DurabilityMode::Fast => "process-crash",
            DurabilityMode::Strict => "strict",
        }
        .into();
        self.group_commit_ms = group_commit_ms;
        self
    }

    #[cfg(target_os = "linux")]
    fn linux_degraded() -> Option<Vec<String>> {
        crate::backend::linux_sandlock::degraded_protections()
    }

    #[cfg(not(target_os = "linux"))]
    fn linux_degraded() -> Option<Vec<String>> {
        None
    }

    pub fn copy_private(requested: FilesystemMode, fallback_reason: Option<String>) -> Self {
        Self {
            requested_fs_mode: requested,
            resolved_fs_mode: FilesystemMode::CopyPrivate,
            host_visibility: "on-commit".into(),
            durability: "process-crash".into(),
            group_commit_ms: None,
            isolation: "deltabox".into(),
            isolation_provider: Some(platform_isolation().into()),
            state_provider: None,
            fallback_reason,
            rollback_guarantee: "private-working-tree".into(),
            tainted: false,
            mountpoint: None,
            helper_pid: None,
            helper_restarts: 0,
            mount_namespace: false,
            degraded_protections: Self::linux_degraded(),
        }
    }

    pub fn cbcow_through(mountpoint: PathBuf, helper_pid: u32, restarts: u64) -> Self {
        Self {
            requested_fs_mode: FilesystemMode::CbcowThrough,
            resolved_fs_mode: FilesystemMode::CbcowThrough,
            host_visibility: "immediate".into(),
            durability: "process-crash".into(),
            group_commit_ms: None,
            isolation: "deltabox".into(),
            isolation_provider: Some(platform_isolation().into()),
            state_provider: Some("linux-fuse3".into()),
            fallback_reason: None,
            rollback_guarantee: "fuse-routed-writes-only".into(),
            tainted: false,
            mountpoint: Some(mountpoint),
            helper_pid: Some(helper_pid),
            helper_restarts: restarts,
            mount_namespace: false,
            degraded_protections: None,
        }
    }

    pub fn cbcow_private(mountpoint: PathBuf, helper_pid: u32, restarts: u64) -> Self {
        Self {
            requested_fs_mode: FilesystemMode::CbcowPrivate,
            resolved_fs_mode: FilesystemMode::CbcowPrivate,
            host_visibility: "on-commit".into(),
            durability: "process-crash".into(),
            group_commit_ms: None,
            isolation: "deltabox".into(),
            isolation_provider: Some(platform_isolation().into()),
            state_provider: Some("linux-fuse3".into()),
            fallback_reason: None,
            rollback_guarantee: "private-live-tree".into(),
            tainted: false,
            mountpoint: Some(mountpoint),
            helper_pid: Some(helper_pid),
            helper_restarts: restarts,
            mount_namespace: false,
            degraded_protections: None,
        }
    }

    pub fn macos_native(mode: FilesystemMode) -> Self {
        let private = mode == FilesystemMode::CbcowPrivate;
        Self {
            requested_fs_mode: mode,
            resolved_fs_mode: mode,
            host_visibility: if private { "on-commit" } else { "immediate" }.into(),
            durability: "process-crash".into(),
            group_commit_ms: None,
            isolation: "deltabox".into(),
            isolation_provider: Some("seatbelt".into()),
            state_provider: Some("macos-apfs-fsevents".into()),
            fallback_reason: None,
            rollback_guarantee: if private {
                "private-live-tree"
            } else {
                "exclusive-writer-apfs-base"
            }
            .into(),
            tainted: false,
            mountpoint: None,
            helper_pid: None,
            helper_restarts: 0,
            mount_namespace: false,
            degraded_protections: None,
        }
    }
}

fn platform_isolation() -> &'static str {
    if cfg!(target_os = "linux") {
        "sandlock"
    } else if cfg!(target_os = "macos") {
        "seatbelt"
    } else {
        "unsupported"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub id: String,
    pub label: Option<String>,
    pub created_at_ms: u64,
    pub change_count: usize,
    pub committed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiffResult {
    pub from: Option<String>,
    pub to: Option<String>,
    pub pending_changes: usize,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncParams {
    pub session_id: String,
}

/// One path where this session's pending work collides with a sibling's
/// publication. `theirs` names the materialized copy of the current host
/// content inside the session view (absent when the sibling deleted it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConflict {
    pub path: String,
    pub host_kind: crate::changeset::ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

/// Result of session.sync: the loser's one-command path back into the game.
/// Baseline advances over sibling publications; contested paths get their
/// `theirs` material; `stale_paths` are sibling-changed paths this session
/// never touched (its view of them is outdated but nothing blocks commit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub folded_publications: u64,
    pub stale_paths: Vec<String>,
    pub conflicts: Vec<SyncConflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResult {
    pub checkpoint_id: String,
    pub applied: usize,
    /// How many sibling publications this commit's baseline was fast-forwarded
    /// over. Non-zero means the host moved legitimately since this session's
    /// baseline: orchestrators must re-verify (build/test) after integration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folded_publications: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortResult {
    pub checkpoint_id: String,
    pub restored: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widening_a_private_workspace_keeps_the_backing_project_deniable() {
        let host = std::path::Path::new("/project");
        for view in [host, std::path::Path::new("/project/mount")] {
            let error = ExecutionProfile::ReadWrite
                .ensure_backing_project_is_deniable(view, host)
                .unwrap_err();
            assert!(error.to_string().contains("working directory"));
            ExecutionProfile::WorkspaceWrite
                .ensure_backing_project_is_deniable(view, host)
                .unwrap();
        }
        ExecutionProfile::ReadWrite
            .ensure_backing_project_is_deniable(std::path::Path::new("/state/mount"), host)
            .unwrap();
    }

    #[test]
    fn request_json_round_trip() {
        let request = Request {
            id: 7,
            method: Method::SessionStart(SessionStartParams {
                project: "/tmp/proj".into(),
                policy: None,
                fs_mode: None,
                require_fs_mode: false,
                durability: DurabilityMode::Fast,
                group_commit_ms: None,
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"method\":\"session.start\""));
        assert!(json.contains("\"project\":\"/tmp/proj\""));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed.method {
            Method::SessionStart(p) => assert_eq!(p.project, PathBuf::from("/tmp/proj")),
            other => panic!("unexpected method: {other:?}"),
        }
    }

    #[test]
    fn durability_defaults_and_strict_status_are_stable() {
        let request: Request = serde_json::from_str(
            r#"{"id":1,"method":"session.start","params":{"project":"/tmp/proj"}}"#,
        )
        .unwrap();
        let Method::SessionStart(params) = request.method else {
            panic!("unexpected method");
        };
        assert_eq!(params.durability, DurabilityMode::Fast);
        assert_eq!(params.group_commit_ms, None);

        let info = FilesystemInfo::cbcow_through(PathBuf::from("/tmp/mount"), 7, 0)
            .with_durability(DurabilityMode::Strict, Some(2));
        let value = serde_json::to_value(info).unwrap();
        assert_eq!(value["durability"], "strict");
        assert_eq!(value["group_commit_ms"], 2);
    }

    #[test]
    fn old_start_request_defaults_new_filesystem_fields() {
        let parsed: Request = serde_json::from_str(
            r#"{"id":7,"method":"session.start","params":{"project":"/tmp/proj"}}"#,
        )
        .unwrap();
        match parsed.method {
            Method::SessionStart(params) => {
                assert_eq!(params.fs_mode, None);
                assert!(!params.require_fs_mode);
            }
            other => panic!("unexpected method: {other:?}"),
        }
    }

    #[test]
    fn filesystem_protocol_uses_stable_mode_names() {
        let info = FilesystemInfo::cbcow_through("/run/deltafs".into(), 42, 3);
        let value = serde_json::to_value(info).unwrap();
        assert_eq!(value["requested_fs_mode"], "cbcow-through");
        assert_eq!(value["resolved_fs_mode"], "cbcow-through");
        assert_eq!(value["host_visibility"], "immediate");
        assert_eq!(value["durability"], "process-crash");
        assert_eq!(value["helper_restarts"], 3);
        assert_eq!(value["mount_namespace"], false);

        let private = FilesystemInfo::cbcow_private("/run/private".into(), 43, 1);
        let value = serde_json::to_value(private).unwrap();
        assert_eq!(value["requested_fs_mode"], "cbcow-private");
        assert_eq!(value["resolved_fs_mode"], "cbcow-private");
        assert_eq!(value["host_visibility"], "on-commit");
        assert_eq!(value["rollback_guarantee"], "private-live-tree");
    }

    #[test]
    fn response_round_trip_with_and_without_error() {
        let ok = Response {
            id: 1,
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(!json.contains("error"));
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert!(parsed.error.is_none());

        let err = Response {
            id: 2,
            result: None,
            error: Some(ProtocolError {
                data: None,
                code: -32000,
                message: "boom".into(),
            }),
        };
        let parsed: Response = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert!(parsed.result.is_none());
        assert_eq!(parsed.error.unwrap().message, "boom");
    }

    #[test]
    fn fs_methods_round_trip() {
        let read = Request {
            id: 10,
            method: Method::FsRead(FsReadParams {
                session_id: "s".into(),
                path: "/proj/a.txt".into(),
            }),
        };
        let json = serde_json::to_string(&read).unwrap();
        assert!(json.contains("\"method\":\"fs.read\""));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed.method {
            Method::FsRead(p) => assert_eq!(p.path, PathBuf::from("/proj/a.txt")),
            other => panic!("unexpected: {other:?}"),
        }

        let write = Request {
            id: 11,
            method: Method::FsWrite(FsWriteParams {
                session_id: "s".into(),
                path: "/proj/b.txt".into(),
                content_base64: "aGk=".into(),
            }),
        };
        let parsed: Request =
            serde_json::from_str(&serde_json::to_string(&write).unwrap()).unwrap();
        match parsed.method {
            Method::FsWrite(p) => assert_eq!(p.content_base64, "aGk="),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn review_methods_round_trip() {
        let begin = Request {
            id: 20,
            method: Method::ReviewBegin(ReviewBeginParams {
                session_id: "session".into(),
                kind: ReviewKind::Risky,
                label: Some("before install".into()),
            }),
        };
        let json = serde_json::to_string(&begin).unwrap();
        assert!(json.contains("\"method\":\"review.begin\""));
        assert!(json.contains("\"kind\":\"risky\""));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed.method {
            Method::ReviewBegin(params) => {
                assert_eq!(params.session_id, "session");
                assert_eq!(params.kind, ReviewKind::Risky);
            }
            other => panic!("unexpected: {other:?}"),
        }

        for method in ["status", "diff", "commit", "abort", "skip"] {
            let raw = format!(
                "{{\"id\":21,\"method\":\"review.{method}\",\"params\":{{\"session_id\":\"s\"}}}}"
            );
            let _: Request = serde_json::from_str(&raw).unwrap();
        }
    }
}

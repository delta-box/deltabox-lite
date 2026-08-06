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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub project: PathBuf,
    pub store_dir: PathBuf,
    pub policy: Policy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project: PathBuf,
    pub started_at_ms: u64,
    pub status: SessionStatus,
    pub checkpoint_count: usize,
    pub pending_changes: usize,
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
pub struct CommitResult {
    pub checkpoint_id: String,
    pub applied: usize,
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
    fn request_json_round_trip() {
        let request = Request {
            id: 7,
            method: Method::SessionStart(SessionStartParams {
                project: "/tmp/proj".into(),
                policy: None,
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

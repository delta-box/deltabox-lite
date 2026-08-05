//! In-sandbox worker protocol (used by the Linux/macOS session backends in
//! later phases). The worker is a confined process that owns the COW layer
//! and forks one child per exec, so no per-command sandbox teardown.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum WorkerCommand {
    Ping,
    Exec(WorkerExec),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerExec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum WorkerEvent {
    Ready,
    ExecResult(WorkerExecResult),
    Fatal(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_round_trip() {
        let cmd = WorkerCommand::Exec(WorkerExec {
            argv: vec!["npm".into(), "install".into()],
            cwd: None,
            env: vec![("PATH".into(), "/usr/bin".into())],
        });
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"exec\""));
        let parsed: WorkerCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            WorkerCommand::Exec(e) => assert_eq!(e.argv[0], "npm"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn worker_event_round_trip() {
        let event = WorkerEvent::ExecResult(WorkerExecResult {
            exit_code: Some(0),
            stdout: "ok".into(),
            stderr: String::new(),
            duration_ms: 5,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WorkerEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WorkerEvent::ExecResult(r) => assert_eq!(r.stdout, "ok"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}

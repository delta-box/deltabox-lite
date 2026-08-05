//! In-sandbox worker protocol and implementation.
//!
//! The worker runs inside the platform sandbox and owns the COW layer. It
//! forks one child per exec so a session keeps its filesystem state across
//! commands without re-creating the sandbox.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_EXEC_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum WorkerCommand {
    Ping { id: u64 },
    Exec { id: u64, exec: WorkerExec },
    Scan { id: u64 },
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
    Ok { id: u64, result: serde_json::Value },
    Err { id: u64, message: String },
    Fatal(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanEntryKind {
    File,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub kind: ScanEntryKind,
}

/// Directories excluded from project scans.
pub fn is_excluded_dir(name: &str) -> bool {
    name == ".deltabox" || name == ".git"
}

/// Run the worker loop until stdin reaches EOF.
pub fn run_worker(root: &Path) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{}", serde_json::to_string(&WorkerEvent::Ready)?)?;
    out.flush()?;

    let stdin = std::io::stdin();
    for line in BufReader::new(stdin.lock()).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let cmd: WorkerCommand = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                let event = WorkerEvent::Err {
                    id: 0,
                    message: format!("invalid command: {e}"),
                };
                writeln!(out, "{}", serde_json::to_string(&event)?)?;
                out.flush()?;
                continue;
            }
        };
        let event = match cmd {
            WorkerCommand::Ping { id } => WorkerEvent::Ok {
                id,
                result: serde_json::json!(true),
            },
            WorkerCommand::Exec { id, exec } => match run_exec(root, &exec) {
                Ok(result) => WorkerEvent::Ok {
                    id,
                    result: serde_json::to_value(&result)?,
                },
                Err(e) => WorkerEvent::Err {
                    id,
                    message: e.to_string(),
                },
            },
            WorkerCommand::Scan { id } => match scan_root(root) {
                Ok(entries) => WorkerEvent::Ok {
                    id,
                    result: serde_json::to_value(&entries)?,
                },
                Err(e) => WorkerEvent::Err {
                    id,
                    message: e.to_string(),
                },
            },
        };
        writeln!(out, "{}", serde_json::to_string(&event)?)?;
        out.flush()?;
    }
    Ok(())
}

fn run_exec(root: &Path, exec: &WorkerExec) -> Result<WorkerExecResult> {
    let argv = &exec.argv;
    if argv.is_empty() {
        return Err(Error::InvalidPolicy("empty argv".into()));
    }
    let cwd = exec.cwd.clone().unwrap_or_else(|| root.to_path_buf());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(&cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if !exec.env.is_empty() {
        let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
        for (k, v) in &exec.env {
            env.insert(k.clone(), v.clone());
        }
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    let start = now_ms();
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let err_thread = std::thread::spawn(move || read_capped(stderr));
    let (out_text, out_trunc) = read_capped(stdout);
    let (err_text, err_trunc) = err_thread
        .join()
        .map_err(|_| Error::Backend("output thread join failed".into()))?;
    let status = child.wait()?;
    Ok(WorkerExecResult {
        exit_code: status.code(),
        stdout: out_text,
        stderr: err_text,
        duration_ms: now_ms() - start,
        truncated: out_trunc || err_trunc,
    })
}

fn read_capped(mut reader: impl Read) -> (String, bool) {
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n <= MAX_EXEC_OUTPUT_BYTES {
                    buf.extend_from_slice(&chunk[..n]);
                } else if buf.len() < MAX_EXEC_OUTPUT_BYTES {
                    let room = MAX_EXEC_OUTPUT_BYTES - buf.len();
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

fn scan_root(root: &Path) -> Result<Vec<ScanEntry>> {
    let mut entries = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(&e.file_name().to_string_lossy()));
    for entry in walker {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let ft = entry.file_type();
        if ft.is_dir() {
            continue;
        }
        let kind = if ft.is_file() {
            ScanEntryKind::File
        } else if ft.is_symlink() {
            ScanEntryKind::Symlink
        } else {
            continue;
        };
        let meta = entry
            .metadata()
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::Backend("scan path escapes root".into()))?;
        entries.push(ScanEntry {
            path: rel.to_string_lossy().into_owned(),
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            kind,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_round_trip() {
        let ping = WorkerCommand::Ping { id: 1 };
        let json = serde_json::to_string(&ping).unwrap();
        assert!(json.contains("\"cmd\":\"ping\""));

        let cmd = WorkerCommand::Exec {
            id: 3,
            exec: WorkerExec {
                argv: vec!["npm".into(), "install".into()],
                cwd: None,
                env: vec![("PATH".into(), "/usr/bin".into())],
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"exec\""));
        let parsed: WorkerCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            WorkerCommand::Exec { id, exec } => {
                assert_eq!(id, 3);
                assert_eq!(exec.argv[0], "npm");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn worker_event_round_trip() {
        let event = WorkerEvent::Ok {
            id: 5,
            result: serde_json::json!({"exit_code": 0}),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"ok\""));
        let parsed: WorkerEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WorkerEvent::Ok { id, result } => {
                assert_eq!(id, 5);
                assert_eq!(result["exit_code"], 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn read_capped_truncates_large_output() {
        let big = vec![b'a'; MAX_EXEC_OUTPUT_BYTES + 100];
        let (text, truncated) = read_capped(big.as_slice());
        assert!(truncated);
        assert_eq!(text.len(), MAX_EXEC_OUTPUT_BYTES);
        let (small, truncated) = read_capped(b"hi".as_slice());
        assert!(!truncated);
        assert_eq!(small, "hi");
    }

    #[test]
    fn scan_skips_git_and_deltabox() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".deltabox")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "x").unwrap();
        std::fs::write(dir.path().join(".deltabox/state"), "x").unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let entries = scan_root(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a.txt");
    }
}

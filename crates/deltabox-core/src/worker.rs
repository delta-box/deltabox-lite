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
    Ping {
        id: u64,
    },
    Exec {
        id: u64,
        exec: WorkerExec,
    },
    Scan {
        id: u64,
    },
    /// List processes still alive after their `exec` returned.
    Processes {
        id: u64,
    },
    /// Terminate every such process and wait briefly for them to exit.
    KillProcesses {
        id: u64,
    },
    Write {
        id: u64,
        path: String,
        content_base64: String,
    },
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

/// A process that outlived the `exec` that started it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveProcess {
    pub pid: u32,
    /// Executable name as reported by the kernel (`comm`).
    pub name: String,
}

impl LiveProcess {
    /// One-line description for error messages: `sleep (pid 42), node (pid 57)`.
    pub fn describe(processes: &[LiveProcess]) -> String {
        processes
            .iter()
            .map(|p| format!("{} (pid {})", p.name, p.pid))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanEntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanFingerprint {
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub kind: ScanEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<ScanFingerprint>,
}

/// Directories excluded from project scans.
pub fn is_excluded_dir(name: &str) -> bool {
    name == ".deltabox" || name == ".git"
}

/// Run the worker loop until stdin reaches EOF.
pub fn run_worker(root: &Path, runtime: &Path) -> Result<()> {
    configure_parent_death_signal()?;
    configure_child_subreaper();
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
            WorkerCommand::Exec { id, exec } => match run_exec(root, runtime, &exec) {
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
                Ok(paths) => WorkerEvent::Ok {
                    id,
                    result: serde_json::to_value(&paths)?,
                },
                Err(e) => WorkerEvent::Err {
                    id,
                    message: e.to_string(),
                },
            },
            WorkerCommand::Processes { id } => WorkerEvent::Ok {
                id,
                result: serde_json::to_value(live_processes())?,
            },
            WorkerCommand::KillProcesses { id } => WorkerEvent::Ok {
                id,
                result: serde_json::to_value(kill_live_processes())?,
            },
            WorkerCommand::Write {
                id,
                path,
                content_base64,
            } => match write_file(root, &path, &content_base64) {
                Ok(()) => WorkerEvent::Ok {
                    id,
                    result: serde_json::json!(null),
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

#[cfg(target_os = "linux")]
fn configure_parent_death_signal() -> Result<()> {
    let parent = unsafe { libc::getppid() };
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::getppid() } != parent || parent == 1 {
        unsafe { libc::raise(libc::SIGKILL) };
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn configure_parent_death_signal() -> Result<()> {
    Ok(())
}

/// Make the worker adopt orphaned descendants. A command that backgrounds a
/// process and exits would otherwise hand it to init, out of the worker's
/// reach; as subreaper the worker can still find and stop it.
#[cfg(target_os = "linux")]
fn configure_child_subreaper() {
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

#[cfg(not(target_os = "linux"))]
fn configure_child_subreaper() {}

/// Every descendant of the worker as `(pid, state, comm)`. Every exec child
/// has been waited on by the time this runs, so descendants are processes a
/// command left behind: directly adopted orphans and whatever they spawned.
#[cfg(target_os = "linux")]
fn worker_descendants() -> Vec<(u32, char, String)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut table = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "<pid> (<comm>) <state> <ppid> ..."; comm may contain spaces and ')'.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let mut rest = stat[close + 1..].split_whitespace();
        let (Some(state), Some(ppid)) = (rest.next(), rest.next()) else {
            continue;
        };
        let Ok(ppid) = ppid.parse::<u32>() else {
            continue;
        };
        table.push((
            pid,
            ppid,
            state.chars().next().unwrap_or('?'),
            stat[open + 1..close].to_owned(),
        ));
    }
    let mut parents = vec![std::process::id()];
    let mut found = Vec::new();
    while let Some(parent) = parents.pop() {
        for (pid, ppid, state, name) in &table {
            if *ppid == parent {
                parents.push(*pid);
                found.push((*pid, *state, name.clone()));
            }
        }
    }
    found
}

/// Reap adopted orphans that have exited, then report the ones still alive.
#[cfg(target_os = "linux")]
fn live_processes() -> Vec<LiveProcess> {
    let mut live = Vec::new();
    for (pid, state, name) in worker_descendants() {
        if state == 'Z' {
            // Only adopted orphans are our children; for any other zombie this
            // fails with ECHILD and its own parent reaps it.
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        } else {
            live.push(LiveProcess { pid, name });
        }
    }
    live
}

#[cfg(not(target_os = "linux"))]
fn live_processes() -> Vec<LiveProcess> {
    Vec::new()
}

/// SIGTERM then SIGKILL every process a command left behind.
/// Returns whatever is still alive afterwards.
#[cfg(target_os = "linux")]
fn kill_live_processes() -> Vec<LiveProcess> {
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        let live = live_processes();
        if live.is_empty() {
            break;
        }
        for process in &live {
            unsafe { libc::kill(process.pid as libc::pid_t, signal) };
        }
        for _ in 0..50 {
            if live_processes().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    live_processes()
}

#[cfg(not(target_os = "linux"))]
fn kill_live_processes() -> Vec<LiveProcess> {
    Vec::new()
}

fn run_exec(root: &Path, runtime: &Path, exec: &WorkerExec) -> Result<WorkerExecResult> {
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
    cmd.env_clear();
    for key in [
        "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TERM", "TZ", "USER",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    let sandbox_home = runtime.join(crate::state_layout::SCRATCH_HOME);
    let sandbox_tmp = runtime.join(crate::state_layout::SCRATCH_TMP);
    cmd.env("HOME", &sandbox_home);
    cmd.env("TMPDIR", &sandbox_tmp);
    cmd.env("XDG_CACHE_HOME", sandbox_home.join(".cache"));
    cmd.env("XDG_CONFIG_HOME", sandbox_home.join(".config"));
    for (key, value) in &exec.env {
        cmd.env(key, value);
    }

    let start = now_ms();
    let mut child = cmd
        .spawn()
        .map_err(|error| Error::Backend(format!("exec spawn failed: {error}")))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let err_thread = std::thread::spawn(move || read_capped(stderr));
    let (out_text, out_trunc) = read_capped(stdout);
    let (err_text, err_trunc) = err_thread
        .join()
        .map_err(|_| Error::Backend("output thread join failed".into()))?;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| Error::Backend(format!("exec wait failed: {error}")))?
        {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    Ok(WorkerExecResult {
        exit_code: status.code(),
        stdout: out_text,
        stderr: err_text,
        duration_ms: now_ms() - start,
        truncated: out_trunc || err_trunc,
    })
}

pub(crate) fn read_capped(mut reader: impl Read) -> (String, bool) {
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

/// Walk the merged sandbox view and return relative paths of files and
/// symlinks. Metadata is built supervisor-side (native stats), so this scan
/// only pays interception cost for readdir calls.
pub(crate) fn scan_root(root: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
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
        if !(ft.is_file() || ft.is_symlink()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::Backend("scan path escapes root".into()))?;
        paths.push(rel.to_string_lossy().into_owned());
    }
    paths.sort();
    Ok(paths)
}

fn write_file(root: &Path, rel: &str, content_base64: &str) -> Result<()> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_base64)
        .map_err(|e| Error::InvalidPolicy(format!("bad base64: {e}")))?;
    let path = resolve_within(root, rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(())
}

fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if p.has_root() {
        return Err(Error::InvalidPolicy(
            "fs.write path must be relative".into(),
        ));
    }
    if p.components().any(|c| {
        matches!(
            c,
            std::path::Component::Prefix(_) | std::path::Component::ParentDir
        )
    }) {
        return Err(Error::InvalidPolicy(
            "fs.write path escapes the project".into(),
        ));
    }
    Ok(root.join(p))
}

pub(crate) fn now_ms() -> u64 {
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
        let paths = scan_root(dir.path()).unwrap();
        assert_eq!(paths, vec!["a.txt".to_string()]);
    }

    #[test]
    fn worker_write_creates_dirs_and_rejects_escape() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello");
        write_file(dir.path(), "a/b.txt", &b64).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b.txt")).unwrap(),
            "hello"
        );
        assert!(write_file(dir.path(), "../evil", &b64).is_err());
        assert!(write_file(dir.path(), "/abs", &b64).is_err());
    }
}

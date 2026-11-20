//! Linux backend powered by sandlock-core.
//!
//! Each command gets its own sandbox, built in the calling process from the
//! session policy: Landlock and seccomp confinement, a network proxy when the
//! policy needs one, and resource limits. The command runs as the sandbox's
//! confined child in its own process group, its output is read straight from
//! pipes, and the sandbox is dropped when the command ends. There is no
//! long-lived worker and no IPC.
//!
//! Commands write directly to the session's working tree; the session
//! managers (`cbcow`, `durable`) own all change tracking. The copy-on-write
//! operations of [`SandboxBackend`] are therefore unsupported here.

use super::SandboxBackend;
use crate::error::{Error, Result};
use crate::policy::{NetworkMode, Policy};
use crate::protocol::{AbortResult, CommitResult, ExecRequest, ExecResult, SessionSpec};
use crate::worker::{read_capped, ScanEntry};
use sandlock_core::sandbox::{ByteSize, StdioMode};
use sandlock_core::{Protection, Sandbox};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Environment variables a command inherits from the caller. Everything else
/// is cleared; the request adds its own explicit values on top.
const INHERITED_ENV: &[&str] = &[
    "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TERM", "TZ", "USER",
];

#[derive(Default)]
pub struct SandlockBackend {
    sessions: HashMap<String, SessionSpec>,
}

impl SandlockBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SandboxBackend for SandlockBackend {
    fn name(&self) -> &'static str {
        "sandlock"
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()> {
        if self.sessions.contains_key(session_id) {
            return Err(Error::SessionExists(session_id.to_string()));
        }
        spec.policy.validate()?;
        self.sessions.insert(session_id.to_string(), spec.clone());
        Ok(())
    }

    fn stop_session(&mut self, session_id: &str) -> Result<()> {
        self.sessions.remove(session_id);
        Ok(())
    }

    fn ping(&mut self, session_id: &str) -> Result<()> {
        self.session(session_id).map(|_| ())
    }

    fn exec(&mut self, session_id: &str, req: ExecRequest) -> Result<ExecResult> {
        let spec = self.session(session_id)?.clone();
        exec(spec, req)
    }

    fn read_file(&mut self, _session_id: &str, _path: &Path) -> Result<Option<Vec<u8>>> {
        Err(unsupported())
    }

    fn write_file(&mut self, _session_id: &str, _path: &Path, _content: Vec<u8>) -> Result<()> {
        Err(unsupported())
    }

    fn scan(&mut self, _session_id: &str) -> Result<Vec<ScanEntry>> {
        Err(unsupported())
    }

    fn current_upper(&mut self, _session_id: &str) -> Result<PathBuf> {
        Err(unsupported())
    }

    fn freeze_upper(
        &mut self,
        _session_id: &str,
        _dest_dir: &Path,
        _prev_upper: Option<&Path>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn commit_changes(
        &mut self,
        _session_id: &str,
        _checkpoint_id: &str,
        _head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<CommitResult> {
        Err(unsupported())
    }

    fn abort_changes(
        &mut self,
        _session_id: &str,
        _checkpoint_id: &str,
        _head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<AbortResult> {
        Err(unsupported())
    }
}

impl SandlockBackend {
    fn session(&self, session_id: &str) -> Result<&SessionSpec> {
        self.sessions
            .get(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))
    }
}

fn unsupported() -> Error {
    Error::Unsupported(
        "the Linux backend runs commands against the session's working tree; \
         change tracking belongs to the session manager"
            .into(),
    )
}

/// Run one command in a sandbox built for it alone.
fn exec(mut spec: SessionSpec, mut req: ExecRequest) -> Result<ExecResult> {
    if req.argv.is_empty() {
        return Err(Error::InvalidPolicy("empty argv".into()));
    }
    // A proxy per command keeps grants and denials attributed to it.
    let proxy = crate::network::NetworkProxy::prepare(&mut spec.policy)?;
    if let Some(proxy) = &proxy {
        proxy.environment(&mut req);
    }
    let cwd = req.cwd.clone().unwrap_or_else(|| spec.project.clone());
    let sandbox = build_sandbox(&spec)?.cwd(&cwd);
    let sandbox = command_environment(&spec, &req)
        .into_iter()
        .fold(sandbox, |sandbox, (key, value)| sandbox.env_var(key, value));
    let mut sandbox = sandbox
        .build()
        .map_err(|e| Error::Backend(format!("sandlock build: {e}")))?;

    // The seccomp-notify supervisor runs on this runtime for the command's
    // whole life. Its worker threads also outlive the command, which matters:
    // the child's PR_SET_PDEATHSIG fires when the thread that forked it exits.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| Error::Backend(format!("tokio runtime: {e}")))?;
    let started = Instant::now();
    let (exit_code, stdout, stderr) = runtime.block_on(async {
        let argv: Vec<&str> = req.argv.iter().map(String::as_str).collect();
        // stdin is never inherited: from a terminal, sandlock would make the
        // command the terminal's foreground process group.
        let mut process = sandbox
            .popen(&argv, StdioMode::Null, StdioMode::Piped, StdioMode::Piped)
            .await
            .map_err(|e| Error::Backend(format!("exec spawn failed: {e}")))?;
        // sandlock puts the command in its own process group.
        let pgid = process.pid().unwrap_or(0);
        let _cancel = crate::cancel::register(move || crate::cancel::kill_process_group(pgid));
        crate::fence::record(pgid);
        // Read the pipes on plain threads: blocking reads on the runtime would
        // starve the supervisor and deadlock the command.
        let stdout = process.take_stdout().map(read_pipe);
        let stderr = process.take_stderr().map(read_pipe);
        let status = process
            .wait()
            .await
            .map_err(|e| Error::Backend(format!("exec wait failed: {e}")))?;
        let collect = |reader: Option<std::thread::JoinHandle<(String, bool)>>| {
            reader.map_or_else(
                || Ok((String::new(), false)),
                |reader| {
                    reader
                        .join()
                        .map_err(|_| Error::Backend("output reader failed".into()))
                },
            )
        };
        Ok::<_, Error>((status.code(), collect(stdout)?, collect(stderr)?))
    })?;
    drop(sandbox);
    drop(runtime);
    Ok(ExecResult {
        exit_code,
        stdout: stdout.0,
        stderr: stderr.0,
        duration_ms: started.elapsed().as_millis() as u64,
        truncated: stdout.1 || stderr.1,
        network_denials: proxy.map(|proxy| proxy.denials()).unwrap_or_default(),
    })
}

fn read_pipe(fd: OwnedFd) -> std::thread::JoinHandle<(String, bool)> {
    std::thread::spawn(move || read_capped(std::fs::File::from(fd)))
}

/// The command's environment: a small allowlist from the caller, private
/// scratch `HOME` and `TMPDIR`, then the request's explicit values.
fn command_environment(spec: &SessionSpec, req: &ExecRequest) -> Vec<(String, String)> {
    let home = spec.store_dir.join(crate::state_layout::SCRATCH_HOME);
    let tmp = spec.store_dir.join(crate::state_layout::SCRATCH_TMP);
    INHERITED_ENV
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .chain([
            ("HOME".to_string(), home.display().to_string()),
            ("TMPDIR".to_string(), tmp.display().to_string()),
            (
                "XDG_CACHE_HOME".to_string(),
                home.join(".cache").display().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                home.join(".config").display().to_string(),
            ),
        ])
        .chain(req.env.iter().flatten().cloned())
        .collect()
}

/// Landlock ABI version of the running kernel (0 when unavailable). The
/// syscall number is 444 on every architecture Linux supports.
pub fn landlock_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    let abi = unsafe {
        libc::syscall(
            444,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    abi.max(0)
}

/// Protections b15389a downgrades to best-effort, keyed by the ABI that
/// first enforces them; anything the kernel cannot enforce is reported in
/// SessionInfo instead of silently dropped.
pub fn degraded_protections() -> Option<Vec<String>> {
    let abi = landlock_abi();
    let mut degraded = Vec::new();
    if abi < 5 {
        degraded.push("fs-ioctl-dev".to_string());
    }
    if abi < 6 {
        degraded.push("signal-scope".to_string());
        degraded.push("abstract-unix-socket-scope".to_string());
    }
    (!degraded.is_empty()).then_some(degraded)
}

/// The sandbox configuration a session's policy asks for. The caller adds the
/// command's working directory and environment, then builds it.
fn build_sandbox(spec: &SessionSpec) -> Result<sandlock_core::sandbox::SandboxBuilder> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    spec.project.hash(&mut hasher);
    let sandbox_name = format!("deltabox-{:016x}", hasher.finish());
    let sandbox_home = spec.store_dir.join(crate::state_layout::SCRATCH_HOME);
    let sandbox_tmp = spec.store_dir.join(crate::state_layout::SCRATCH_TMP);
    std::fs::create_dir_all(&sandbox_home)?;
    std::fs::create_dir_all(&sandbox_tmp)?;
    let mut builder = Sandbox::builder()
        .name(sandbox_name)
        .allow_degraded(Protection::FsIoctlDev)
        .allow_degraded(Protection::SignalScope)
        .allow_degraded(Protection::AbstractUnixSocketScope)
        .clean_env(true)
        .fs_read(&spec.project)
        .fs_write(&spec.project)
        .fs_read("/proc")
        .fs_read("/dev")
        .fs_write("/dev/null")
        .fs_write("/dev/full")
        .fs_read("/tmp")
        .fs_read_if_exists("/lib64")
        .fs_read_if_exists("/usr/local");
    match spec.policy.network.mode {
        NetworkMode::AllowAudit => {
            builder = builder.net_allow("*");
        }
        NetworkMode::DenyAll => {}
        NetworkMode::Allowlist => {
            for target in &spec.policy.network.allow {
                builder = builder.net_allow(target);
            }
        }
    }
    if !sandbox_home.starts_with(&spec.project) {
        builder = builder
            .fs_read(&sandbox_home)
            .fs_write(&sandbox_home)
            .fs_read(&sandbox_tmp)
            .fs_write(&sandbox_tmp);
    }
    // Landlock adds a rule by opening the ruled path, so one absent allowlist
    // entry aborts confinement for every command. Absent entries grant
    // nothing anyway (the default policy lists /lib64, which does not exist
    // on arm64 Debian/Ubuntu), so skipping them only tightens confinement.
    for p in resolve_read_paths(&spec.policy, &spec.project) {
        builder = builder.fs_read_if_exists(&p);
    }
    for p in resolve_write_paths(&spec.policy, &spec.project) {
        if p.exists() {
            builder = builder.fs_write(&p);
        }
    }
    let deny_paths = match &spec.resolved_deny_paths {
        Some(paths) => paths.clone(),
        None => resolve_deny_paths(&spec.policy, &spec.project)?,
    };
    for p in deny_paths {
        builder = builder.fs_deny(&p);
    }
    if let Some(host_project) = &spec.host_project {
        if host_project != &spec.project && !spec.project.starts_with(host_project) {
            builder = builder.fs_deny(host_project);
        }
    }
    if let Some(bytes) = spec.policy.limits.max_memory_bytes {
        builder = builder.max_memory(ByteSize::bytes(bytes));
    }
    if let Some(n) = spec.policy.limits.max_processes {
        builder = builder.max_processes(n);
    }
    if let Some(n) = spec.policy.limits.max_open_files {
        builder = builder.max_open_files(n);
    }
    if let Some(pct) = spec.policy.limits.max_cpu_percent {
        builder = builder.max_cpu(pct);
    }
    Ok(builder)
}

fn resolve_read_paths(policy: &Policy, project: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = policy
        .filesystem
        .read
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                project.join(p)
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn resolve_write_paths(policy: &Policy, project: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = policy
        .filesystem
        .write
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                project.join(p)
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(crate) fn resolve_deny_paths(policy: &Policy, project: &Path) -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut out: Vec<PathBuf> = Vec::new();
    for pattern in &policy.filesystem.deny {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        if let Some(rest) = pattern.strip_prefix("~/") {
            let candidate = home.join(rest);
            if candidate.exists() {
                out.push(candidate);
            }
            continue;
        }
        if pattern.starts_with('/') {
            let candidate = PathBuf::from(pattern);
            if candidate.exists() {
                out.push(candidate);
            }
            continue;
        }
        let home_candidate = home.join(pattern);
        if home_candidate.exists() {
            out.push(home_candidate);
        }
    }
    let walker = walkdir::WalkDir::new(project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !crate::worker::is_excluded_dir(&e.file_name().to_string_lossy()));
    for entry in walker {
        let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
        if !entry.file_type().is_dir() && policy.path_is_denied(entry.path()) {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_paths_include_home_and_project_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "secret").unwrap();
        std::fs::write(dir.path().join("ok.txt"), "fine").unwrap();
        let policy = Policy::default_agent();
        let paths = resolve_deny_paths(&policy, dir.path()).unwrap();
        assert!(paths.contains(&dir.path().join(".env")));
        assert!(!paths.contains(&dir.path().join("ok.txt")));
    }

    #[test]
    fn direct_backing_project_is_denied() {
        let root = tempfile::tempdir().unwrap();
        let backing = root.path().join("backing");
        let mounted = root.path().join("mounted");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&mounted).unwrap();
        let spec = SessionSpec {
            project: mounted.clone(),
            host_project: Some(backing.clone()),
            store_dir: runtime,
            policy: Policy::default_agent(),
            resolved_deny_paths: None,
        };
        let sandbox = build_sandbox(&spec).unwrap().build().unwrap();
        assert!(sandbox.fs_denied.contains(&backing));
    }

    #[test]
    fn nested_private_working_tree_remains_accessible() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let working = project.join(".deltabox/session/working");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&working).unwrap();
        let spec = SessionSpec {
            project: working,
            host_project: Some(project.clone()),
            store_dir: runtime,
            policy: Policy::default_agent(),
            resolved_deny_paths: None,
        };
        let sandbox = build_sandbox(&spec).unwrap().build().unwrap();
        assert!(!sandbox.fs_denied.contains(&project));
    }
}

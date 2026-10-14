//! macOS backend using Seatbelt confinement and APFS clone shadows.

use super::SandboxBackend;
use crate::changeset::ChangeKind;
use crate::error::{Error, Result};
use crate::pathmap::PathMapper;
use crate::policy::{NetworkMode, Policy};
use crate::protocol::{AbortResult, CommitResult, ExecRequest, ExecResult, SessionSpec};
use crate::snapshot::{
    build_scan_entries, clone_or_copy_file, diff_snapshots, preserve_symlink_times,
};
use crate::worker::{read_capped, scan_root, ScanEntry};
use base64::Engine;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

pub struct SeatbeltBackend {
    direct_write: bool,
    sessions: HashMap<String, SessionState>,
}

struct SessionState {
    spec: SessionSpec,
    root: PathBuf,
    runtime: PathBuf,
    mapper: PathMapper,
    profile: String,
    proxy: Option<ProxyHandle>,
    rustup_home: Option<PathBuf>,
}

struct ProxyHandle {
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ProxyHandle {
    fn start(allow: Vec<String>, max_connections: usize) -> Result<Self> {
        for target in &allow {
            split_target(target)?;
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let active = Arc::new(AtomicUsize::new(0));
        let thread_token = token.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if active.fetch_add(1, Ordering::Relaxed) >= max_connections {
                            active.fetch_sub(1, Ordering::Relaxed);
                            drop(stream);
                            continue;
                        }
                        let allow = allow.clone();
                        let token = thread_token.clone();
                        let active = Arc::clone(&active);
                        std::thread::spawn(move || {
                            if stream.set_nonblocking(false).is_ok() {
                                let _ = serve_proxy_connection(stream, &allow, &token);
                            }
                            active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            port,
            token,
            stop,
            thread: Some(thread),
        })
    }
}

fn serve_proxy_connection(mut client: TcpStream, allow: &[String], token: &str) -> Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    client.set_write_timeout(Some(Duration::from_secs(30)))?;
    let request = read_proxy_headers(&mut client)?;
    let split = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| Error::Backend("proxy request has incomplete headers".into()))?;
    let head = std::str::from_utf8(&request[..split])
        .map_err(|_| Error::Backend("proxy request headers are not UTF-8".into()))?;
    if !proxy_auth_matches(head, token) {
        client.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
        )?;
        return Ok(());
    }
    let first = head
        .lines()
        .next()
        .ok_or_else(|| Error::Backend("proxy request is empty".into()))?;
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_target(target)?;
        if !target_is_allowed(&host, port, allow) {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
        let mut upstream = TcpStream::connect((host.as_str(), port))?;
        proxy_debug(format_args!(
            "CONNECT {host}:{port} via {}",
            upstream.peer_addr()?
        ));
        upstream.set_read_timeout(None)?;
        upstream.set_write_timeout(None)?;
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        client.set_read_timeout(None)?;
        client.set_write_timeout(None)?;
        proxy_debug(format_args!(
            "CONNECT buffered tunnel bytes={}",
            request.len().saturating_sub(split)
        ));
        if request.len() > split {
            upstream.write_all(&request[split..])?;
        }
        relay_streams(client, upstream)?;
        return Ok(());
    }

    let (host, port, path) = parse_http_target(target)?;
    if !target_is_allowed(&host, port, allow) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(());
    }
    let mut upstream = TcpStream::connect((host.as_str(), port))?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    write!(upstream, "{method} {path} {version}\r\n")?;
    for line in head.lines().skip(1) {
        if line.is_empty()
            || line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:")
            || line.to_ascii_lowercase().starts_with("proxy-connection:")
        {
            continue;
        }
        write!(upstream, "{line}\r\n")?;
    }
    upstream.write_all(b"\r\n")?;
    if request.len() > split {
        upstream.write_all(&request[split..])?;
    }
    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    relay_streams(client, upstream)?;
    Ok(())
}

fn read_proxy_headers(stream: &mut TcpStream) -> Result<Vec<u8>> {
    const MAX_HEADERS: usize = 64 * 1024;
    let mut data = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    while data.len() < MAX_HEADERS {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..read]);
        if data.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(data);
        }
    }
    Err(Error::Backend("proxy request headers exceed 64 KiB".into()))
}

fn proxy_auth_matches(headers: &str, token: &str) -> bool {
    let expected = base64::engine::general_purpose::STANDARD.encode(format!("deltabox:{token}"));
    headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            key.eq_ignore_ascii_case("proxy-authorization")
                && value.trim() == format!("Basic {expected}")
        })
    })
}

fn parse_http_target(target: &str) -> Result<(String, u16, String)> {
    let rest = target
        .strip_prefix("http://")
        .ok_or_else(|| Error::Backend("proxy only accepts HTTP absolute-form requests".into()))?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    let (host, port) = if authority.starts_with('[') || authority.contains(':') {
        split_target(authority)?
    } else {
        (authority.to_ascii_lowercase(), 80)
    };
    Ok((host, port, path))
}

fn target_is_allowed(host: &str, port: u16, allow: &[String]) -> bool {
    allow.iter().any(|target| {
        split_target(target).is_ok_and(|(allowed_host, allowed_port)| {
            allowed_host
                .trim_end_matches('.')
                .eq_ignore_ascii_case(host.trim_end_matches('.'))
                && allowed_port == port
        })
    })
}

fn relay_streams(client: TcpStream, upstream: TcpStream) -> Result<()> {
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let left = std::thread::spawn(move || relay_one_way(&mut client_read, &mut upstream_write));
    let mut upstream_read = upstream;
    let mut client_write = client;
    let right = relay_one_way(&mut upstream_read, &mut client_write);
    let left = left
        .join()
        .map_err(|_| Error::Backend("proxy relay thread panicked".into()))?;
    proxy_debug(format_args!(
        "relay client->upstream={left:?} upstream->client={right:?}"
    ));
    left.1?;
    right.1?;
    Ok(())
}

fn relay_one_way(read: &mut TcpStream, write: &mut TcpStream) -> (u64, std::io::Result<()>) {
    let mut total = 0;
    let mut buffer = [0u8; 32 * 1024];
    loop {
        match read.read(&mut buffer) {
            Ok(0) => return (total, Ok(())),
            Ok(count) => {
                if let Err(error) = write.write_all(&buffer[..count]) {
                    return (total, Err(error));
                }
                if let Err(error) = write.flush() {
                    return (total, Err(error));
                }
                total += count as u64;
            }
            Err(error) => return (total, Err(error)),
        }
    }
}

fn proxy_debug(args: std::fmt::Arguments<'_>) {
    if cfg!(debug_assertions) && std::env::var_os("DELTABOX_PROXY_DEBUG").is_some() {
        eprintln!("deltabox proxy: {args}");
    }
}

impl SeatbeltBackend {
    pub fn new() -> Self {
        Self {
            direct_write: false,
            sessions: HashMap::new(),
        }
    }

    /// Run directly in a private tree owned by the durable transaction layer.
    pub fn new_direct() -> Self {
        Self {
            direct_write: true,
            sessions: HashMap::new(),
        }
    }

    fn state(&self, session_id: &str) -> Result<&SessionState> {
        self.sessions
            .get(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))
    }

    fn build_state(&self, spec: &SessionSpec, root: PathBuf) -> Result<SessionState> {
        let runtime = spec.store_dir.join("macos-runtime");
        std::fs::create_dir_all(runtime.join("home/.cache"))?;
        std::fs::create_dir_all(runtime.join("home/.config"))?;
        std::fs::create_dir_all(runtime.join("tmp"))?;
        let host = spec
            .host_project
            .clone()
            .unwrap_or_else(|| spec.project.clone());
        let mapper = PathMapper::new(host.clone(), root.clone());
        let rustup_home = host_rustup_home();
        let tool_roots: Vec<PathBuf> = rustup_home.iter().cloned().collect();
        let proxy = if spec.policy.network.mode == NetworkMode::Allowlist
            && spec
                .policy
                .network
                .allow
                .iter()
                .any(|target| !is_direct_network_target(target))
        {
            let max_connections =
                spec.policy.limits.max_processes.unwrap_or(32).clamp(1, 128) as usize;
            Some(ProxyHandle::start(
                spec.policy.network.allow.clone(),
                max_connections,
            )?)
        } else {
            None
        };
        let profile = build_profile(
            &spec.policy,
            &root,
            &runtime,
            &host,
            proxy.as_ref().map(|proxy| proxy.port),
            &tool_roots,
            spec.resolved_deny_paths.as_deref().unwrap_or_default(),
        )?;
        Ok(SessionState {
            spec: spec.clone(),
            root,
            runtime,
            mapper,
            profile,
            proxy,
            rustup_home,
        })
    }
}

impl Default for SeatbeltBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SeatbeltBackend {
    fn drop(&mut self) {
        for (_, state) in self.sessions.drain() {
            if !self.direct_write {
                let _ = remove_path(&state.root);
            }
        }
    }
}

impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn is_supported(&self) -> bool {
        Path::new(SANDBOX_EXEC).is_file()
    }

    fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()> {
        if self.sessions.contains_key(session_id) {
            return Err(Error::SessionExists(session_id.to_string()));
        }
        spec.policy.validate()?;
        if !self.is_supported() {
            return Err(Error::Unsupported(
                "/usr/bin/sandbox-exec is unavailable".into(),
            ));
        }
        let root = if self.direct_write {
            spec.project.clone()
        } else {
            let shadow = spec.store_dir.join("macos-shadow");
            clone_project_tree(&spec.project, &shadow)?;
            shadow
        };
        let state = self.build_state(spec, root)?;
        self.sessions.insert(session_id.to_string(), state);
        Ok(())
    }

    fn stop_session(&mut self, session_id: &str) -> Result<()> {
        let Some(state) = self.sessions.remove(session_id) else {
            return Ok(());
        };
        if !self.direct_write {
            remove_path(&state.root)?;
        }
        Ok(())
    }

    fn ping(&mut self, session_id: &str) -> Result<()> {
        let state = self.state(session_id)?;
        if !state.root.is_dir() || !Path::new(SANDBOX_EXEC).is_file() {
            return Err(Error::Backend(
                "macOS session runtime is unavailable".into(),
            ));
        }
        Ok(())
    }

    fn exec(&mut self, session_id: &str, req: ExecRequest) -> Result<ExecResult> {
        let state = self.state(session_id)?;
        if req.argv.is_empty() {
            return Err(Error::InvalidPolicy("empty argv".into()));
        }
        let argv: Vec<String> = req
            .argv
            .iter()
            .map(|arg| state.mapper.map_arg(arg))
            .collect();
        let cwd = req
            .cwd
            .as_deref()
            .map(|path| state.mapper.map_path(path))
            .unwrap_or_else(|| state.root.clone());
        if !cwd.starts_with(&state.root) {
            return Err(Error::InvalidPolicy(
                "exec cwd is outside the sandbox project".into(),
            ));
        }

        let mut command = Command::new(SANDBOX_EXEC);
        command
            .arg("-p")
            .arg(&state.profile)
            .arg(&argv[0])
            .args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for key in [
            "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "SHELL", "TERM", "TZ", "USER",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if let Ok(path) = std::env::var("PATH") {
            let mapped = state.mapper.map_env("PATH", &path)?;
            command.env("PATH", developer_tool_path(&mapped));
        }
        let home = state.runtime.join("home");
        command.env("HOME", &home);
        command.env("TMPDIR", state.runtime.join("tmp"));
        command.env("XDG_CACHE_HOME", home.join(".cache"));
        command.env("XDG_CONFIG_HOME", home.join(".config"));
        if let Some(rustup_home) = &state.rustup_home {
            command.env("RUSTUP_HOME", rustup_home);
        }
        if let Some(sdkroot) = host_sdkroot() {
            command.env("SDKROOT", sdkroot);
        }
        for (key, value) in req.env.unwrap_or_default() {
            let mapped = state.mapper.map_env(&key, &value)?;
            command.env(
                &key,
                if key == "PATH" {
                    developer_tool_path(&mapped)
                } else {
                    mapped
                },
            );
        }
        if let Some(proxy) = &state.proxy {
            let url = format!("http://deltabox:{}@127.0.0.1:{}", proxy.token, proxy.port);
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                command.env(key, &url);
            }
            let no_proxy = state
                .spec
                .policy
                .network
                .allow
                .iter()
                .filter_map(|target| split_target(target).ok())
                .filter(|(host, _)| is_local_host(host))
                .map(|(host, _)| host)
                .collect::<Vec<_>>()
                .join(",");
            command.env("NO_PROXY", &no_proxy);
            command.env("no_proxy", no_proxy);
        }
        apply_limits(&mut command, &state.spec.policy);

        let start = Instant::now();
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let out_thread = std::thread::spawn(move || read_capped(stdout));
        let err_thread = std::thread::spawn(move || read_capped(stderr));
        let Some(status) = child.wait_timeout(COMMAND_TIMEOUT)? else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Backend("session command timed out".into()));
        };
        let (stdout, out_truncated) = out_thread
            .join()
            .map_err(|_| Error::Backend("stdout reader failed".into()))?;
        let (stderr, err_truncated) = err_thread
            .join()
            .map_err(|_| Error::Backend("stderr reader failed".into()))?;
        Ok(ExecResult {
            exit_code: status.code(),
            stdout,
            stderr,
            duration_ms: start.elapsed().as_millis() as u64,
            truncated: out_truncated || err_truncated,
        })
    }

    fn read_file(&mut self, session_id: &str, path: &Path) -> Result<Option<Vec<u8>>> {
        let state = self.state(session_id)?;
        let target = project_target(state, path, "fs.read")?;
        if !target.exists() {
            return Ok(None);
        }
        let meta = std::fs::symlink_metadata(&target)?;
        if meta.file_type().is_symlink() {
            return Err(Error::Backend("fs.read refuses symlinks".into()));
        }
        let canonical = target.canonicalize()?;
        if !canonical.starts_with(state.root.canonicalize()?) {
            return Err(Error::Backend("fs.read symlink escapes project".into()));
        }
        let meta = std::fs::metadata(&canonical)?;
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() > 16 * 1024 * 1024 {
            return Err(Error::Backend("fs.read file exceeds 16 MiB limit".into()));
        }
        Ok(Some(std::fs::read(canonical)?))
    }

    fn write_file(&mut self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        let state = self.state(session_id)?;
        if state
            .spec
            .policy
            .limits
            .max_write_bytes
            .is_some_and(|limit| content.len() as u64 > limit)
        {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        let target = project_target(state, path, "fs.write")?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_path(&target)?;
        std::fs::write(target, content)?;
        Ok(())
    }

    fn scan(&mut self, session_id: &str) -> Result<Vec<ScanEntry>> {
        let state = self.state(session_id)?;
        let paths = scan_root(&state.root)?;
        Ok(build_scan_entries(&paths, &state.root, &state.root))
    }

    fn current_upper(&mut self, session_id: &str) -> Result<PathBuf> {
        Ok(self.state(session_id)?.root.clone())
    }

    fn freeze_upper(
        &mut self,
        session_id: &str,
        dest_dir: &Path,
        _prev_upper: Option<&Path>,
    ) -> Result<()> {
        let state = self.state(session_id)?;
        clone_project_tree(&state.root, &dest_dir.join("upper"))?;
        Ok(())
    }

    fn commit_changes(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        _head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<CommitResult> {
        let state = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))?;
        let applied = if self.direct_write {
            0
        } else {
            apply_tree_delta(&state.spec.project, &state.root, &state.spec.project)?
        };
        if !self.direct_write {
            remove_path(&state.root)?;
        }
        let spec = state.spec;
        self.start_session(session_id, &spec)?;
        Ok(CommitResult {
            checkpoint_id: checkpoint_id.to_string(),
            applied,
            folded_publications: None,
        })
    }

    fn materialize_changes(
        &mut self,
        session_id: &str,
        _head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<usize> {
        if !self.sessions.contains_key(session_id) {
            return Err(Error::SessionNotFound(session_id.to_string()));
        }
        Ok(0)
    }

    fn abort_changes(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        head_dir: &Path,
        _deleted: Vec<String>,
    ) -> Result<AbortResult> {
        let state = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))?;
        let checkpoint = head_dir.join("upper");
        let restored = if self.direct_write {
            0
        } else {
            let before = scan_entries(&state.root)?;
            let after = scan_entries(&checkpoint)?;
            let count = diff_snapshots(&before, &after).len();
            clone_project_tree(&checkpoint, &state.root)?;
            count
        };
        let root = state.root.clone();
        let spec = state.spec;
        let replacement = self.build_state(&spec, root)?;
        self.sessions.insert(session_id.to_string(), replacement);
        Ok(AbortResult {
            checkpoint_id: checkpoint_id.to_string(),
            restored,
        })
    }
}

fn project_target(state: &SessionState, path: &Path, operation: &str) -> Result<PathBuf> {
    let mapped = state.mapper.map_path(path);
    let rel = mapped
        .strip_prefix(&state.root)
        .map_err(|_| Error::InvalidPolicy(format!("{operation} outside project")))?;
    if rel
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
        || state.spec.policy.path_is_denied(rel)
    {
        return Err(Error::Backend("path denied by policy".into()));
    }
    Ok(state.root.join(rel))
}

fn scan_entries(root: &Path) -> Result<Vec<ScanEntry>> {
    let paths = scan_root(root)?;
    Ok(build_scan_entries(&paths, root, root))
}

fn clone_project_tree(src: &Path, dst: &Path) -> Result<usize> {
    remove_path(dst)?;
    std::fs::create_dir_all(dst)?;
    let mut copied = 0;
    let walker = walkdir::WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".deltabox");
    for entry in walker {
        let entry = entry.map_err(|error| Error::Io(std::io::Error::other(error)))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|_| Error::Backend("clone path escaped source".into()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
            preserve_symlink_times(entry.path(), &target)?;
            copied += 1;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            clone_or_copy_file(entry.path(), &target)?;
            std::fs::set_permissions(&target, std::fs::metadata(entry.path())?.permissions())?;
            File::options()
                .write(true)
                .open(&target)?
                .set_modified(std::fs::metadata(entry.path())?.modified()?)?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn apply_tree_delta(from: &Path, to: &Path, dest: &Path) -> Result<usize> {
    let diffs = diff_snapshots(&scan_entries(from)?, &scan_entries(to)?);
    for diff in &diffs {
        let target = dest.join(&diff.path);
        match diff.kind {
            ChangeKind::Deleted => remove_path(&target)?,
            ChangeKind::Added | ChangeKind::Modified => {
                let source = to.join(&diff.path);
                remove_path(&target)?;
                let meta = std::fs::symlink_metadata(&source)?;
                if meta.file_type().is_symlink() {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
                    preserve_symlink_times(&source, &target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    clone_or_copy_file(&source, &target)?;
                    std::fs::set_permissions(&target, meta.permissions())?;
                }
            }
        }
    }
    Ok(diffs.len())
}

fn remove_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn build_profile(
    policy: &Policy,
    root: &Path,
    runtime: &Path,
    host: &Path,
    proxy_port: Option<u16>,
    tool_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<String> {
    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow process-exec process-fork)".to_string(),
        "(allow process-info* signal (target same-sandbox))".to_string(),
        "(allow system-sched (target self))".to_string(),
        "(allow mach-priv-task-port (target same-sandbox))".to_string(),
        "(allow user-preference-read)".to_string(),
        "(allow ipc-posix-shm ipc-posix-sem ipc-sysv-shm ipc-sysv-sem)".to_string(),
        "(allow distributed-notification-post)".to_string(),
        "(allow iokit-get-properties)".to_string(),
        "(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))"
            .to_string(),
        "(allow mach-lookup (global-name \"com.apple.FontObjectsServer\") (global-name \"com.apple.fonts\") (global-name \"com.apple.logd\") (global-name \"com.apple.lsd.mapdb\") (global-name \"com.apple.system.logger\") (global-name \"com.apple.system.notification_center\") (global-name \"com.apple.system.opendirectoryd.libinfo\") (global-name \"com.apple.system.opendirectoryd.membership\") (global-name \"com.apple.bsd.dirhelper\") (global-name \"com.apple.securityd.xpc\") (global-name \"com.apple.SecurityServer\"))".to_string(),
        "(allow sysctl-read)".to_string(),
        "(allow file-read* (literal \"/\"))".to_string(),
        "(allow file-read-metadata (vnode-type DIRECTORY))".to_string(),
        "(allow file-read* (literal \"/etc\") (literal \"/tmp\") (literal \"/var\"))"
            .to_string(),
    ];

    for path in [
        "/System",
        "/Library",
        "/usr",
        "/bin",
        "/sbin",
        "/opt/homebrew",
        "/usr/local",
        "/etc",
        "/var/select",
        "/private/etc",
        "/private/var/db",
        "/private/var/select",
        "/dev",
    ] {
        lines.push(format!("(allow file-read* (subpath {}))", quote(path)));
    }
    for path in policy_paths(&policy.filesystem.read, root) {
        lines.push(format!(
            "(allow file-read* (subpath {}))",
            quote_path(&path)
        ));
    }
    for path in tool_roots {
        lines.push(format!("(allow file-read* (subpath {}))", quote_path(path)));
    }
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path).filter(|path| path.is_absolute()) {
            lines.push(format!(
                "(allow file-read* (subpath {}))",
                quote_path(&directory)
            ));
        }
    }
    lines.push(format!(
        "(allow file-read* (subpath {}))",
        quote_path(runtime)
    ));
    if host == root || root.starts_with(host) {
        lines.push(format!("(allow file-read* (subpath {}))", quote_path(host)));
    }

    for path in policy_paths(&policy.filesystem.write, root) {
        lines.push(format!(
            "(allow file-write* (subpath {}))",
            quote_path(&path)
        ));
    }
    lines.push(format!(
        "(allow file-write* (subpath {}))",
        quote_path(runtime)
    ));
    if host != root {
        if !root.starts_with(host) {
            lines.push(format!(
                "(deny file-read* file-write* (subpath {}))",
                quote_path(host)
            ));
        }
        lines.push(format!(
            "(allow file-write* (subpath {}))",
            quote_path(root)
        ));
        lines.push(format!(
            "(allow file-write* (subpath {}))",
            quote_path(runtime)
        ));
    }
    lines.push("(allow file-ioctl file-read-data file-write-data (literal \"/dev/null\"))".into());
    lines.push("(allow file-read-data (literal \"/dev/zero\") (literal \"/dev/random\") (literal \"/dev/urandom\"))".into());

    let deny_roots = if host == root {
        vec![root]
    } else {
        vec![root, host]
    };
    for pattern in &policy.filesystem.deny {
        for deny_root in &deny_roots {
            let filter = deny_filter(deny_root, pattern);
            lines.push(format!("(deny file-read* file-write* {filter})"));
            lines.push(format!(
                "(deny file-write-create file-write-unlink {filter})"
            ));
        }
    }

    // Broad filesystem grants must never expose authoritative session data.
    // A private LIVE tree and command scratch can be nested in engine state;
    // carve out only those writable trees, not the runtime/control directory.
    let scratch = [runtime.join("home"), runtime.join("tmp")];
    let mut anchors = BTreeSet::new();
    for protected in protected_paths {
        let exceptions: Vec<&Path> = std::iter::once(root)
            .chain(scratch.iter().map(PathBuf::as_path))
            .filter(|path| *path != protected && path.starts_with(protected))
            .collect();
        let mut filter = format!("(subpath {})", quote_path(protected));
        for exception in &exceptions {
            filter = format!(
                "(require-all {filter} (require-not (subpath {})))",
                quote_path(exception)
            );
        }
        lines.push(format!("(deny file-read* file-write* {filter})"));
        // Keep LIVE/scratch roots and every protected ancestor stable: a
        // parent rename could otherwise relocate state without opening it.
        for exception in &exceptions {
            anchors.extend(exception.ancestors());
        }
        anchors.extend(protected.ancestors());
        // Unix-domain sockets use network rules as well as filesystem rules.
        lines.push(format!(
            "(deny network* (local unix-socket (subpath {})))",
            quote_path(protected)
        ));
        lines.push(format!(
            "(deny network* (remote unix-socket (subpath {})))",
            quote_path(protected)
        ));
    }

    for anchor in anchors {
        lines.push(format!(
            "(deny file-write-unlink (literal {}))",
            quote_path(anchor)
        ));
    }

    match policy.network.mode {
        NetworkMode::AllowAudit => lines.push("(allow network*)".into()),
        NetworkMode::DenyAll => {}
        NetworkMode::Allowlist => {
            for target in &policy.network.allow {
                if !is_direct_network_target(target) {
                    continue;
                }
                let (_, port) = split_target(target)?;
                let seatbelt_target = format!("localhost:{port}");
                lines.push(format!(
                    "(allow network-outbound (remote ip {}))",
                    quote(&seatbelt_target)
                ));
                if is_local_target(target) {
                    lines.push(format!(
                        "(allow network-bind (local ip {}))",
                        quote(&seatbelt_target)
                    ));
                    lines.push(format!(
                        "(allow network-inbound (local ip {}))",
                        quote(&seatbelt_target)
                    ));
                }
            }
            if let Some(port) = proxy_port {
                lines.push(format!(
                    "(allow network-outbound (remote ip \"localhost:{port}\"))"
                ));
            }
        }
    }
    Ok(lines.join("\n"))
}

fn policy_paths(paths: &[PathBuf], root: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else if path == Path::new(".") {
                root.to_path_buf()
            } else {
                root.join(path)
            }
        })
        .collect()
}

fn deny_filter(root: &Path, pattern: &str) -> String {
    if pattern.contains('/') && !pattern.contains('*') && !pattern.contains('?') {
        let path = root.join(pattern.trim_end_matches('/'));
        return format!("(subpath {})", quote_path(&path));
    }
    let mut regex = regex_escape(&root.to_string_lossy());
    regex.push('/');
    if !pattern.trim_end_matches('/').contains('/') {
        regex.push_str("(.*/)?");
    }
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            _ => regex.push_str(&regex_escape(&ch.to_string())),
        }
    }
    regex.push_str("(/.*)?$");
    format!("(regex {})", quote(&format!("^{regex}")))
}

fn regex_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if matches!(
            ch,
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn is_local_target(target: &str) -> bool {
    split_target(target).is_ok_and(|(host, _)| is_local_host(&host))
}

fn is_direct_network_target(target: &str) -> bool {
    split_target(target)
        .map(|(host, _)| is_local_host(&host))
        .unwrap_or(false)
}

fn is_local_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn split_target(target: &str) -> Result<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or_else(|| {
            Error::InvalidPolicy(format!("network target must be host:port: {target}"))
        })?;
        let port = port
            .parse()
            .map_err(|_| Error::InvalidPolicy(format!("invalid network target port: {target}")))?;
        return Ok((host.to_ascii_lowercase(), port));
    }
    let (host, port) = target.rsplit_once(':').ok_or_else(|| {
        Error::InvalidPolicy(format!("network target must be host:port: {target}"))
    })?;
    if host.is_empty() {
        return Err(Error::InvalidPolicy(format!(
            "network target host is empty: {target}"
        )));
    }
    let port = port
        .parse()
        .map_err(|_| Error::InvalidPolicy(format!("invalid network target port: {target}")))?;
    Ok((host.to_ascii_lowercase(), port))
}

fn quote_path(path: &Path) -> String {
    quote(&path.to_string_lossy())
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

fn developer_tool_path(path: &str) -> String {
    const CLT_BIN: &str = "/Library/Developer/CommandLineTools/usr/bin";
    let mut entries = Vec::new();
    if Path::new(CLT_BIN).is_dir() {
        entries.push(PathBuf::from(CLT_BIN));
    }
    entries.extend(std::env::split_paths(path));
    entries.dedup();
    std::env::join_paths(entries)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

fn host_rustup_home() -> Option<PathBuf> {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))
        .filter(|path| path.is_dir())
}

fn host_sdkroot() -> Option<PathBuf> {
    std::env::var_os("SDKROOT")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            let path = PathBuf::from("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk");
            path.is_dir().then_some(path)
        })
}

fn apply_limits(command: &mut Command, policy: &Policy) {
    let limits = policy.limits.clone();
    unsafe {
        command.pre_exec(move || {
            set_limit(libc::RLIMIT_NOFILE, limits.max_open_files.map(u64::from))?;
            set_limit(libc::RLIMIT_AS, limits.max_memory_bytes)?;
            Ok(())
        });
    }
}

fn set_limit(resource: libc::c_int, value: Option<u64>) -> std::io::Result<()> {
    let Some(value) = value else { return Ok(()) };
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn proxy_connect_request(port: u16, token: &str, target: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("deltabox:{token}"));
        write!(
            stream,
            "CONNECT {target} HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
        )
        .unwrap();
        stream
    }

    #[test]
    fn profile_escapes_paths_and_denies_secrets() {
        let profile = build_profile(
            &Policy::default_agent(),
            Path::new("/tmp/a project"),
            Path::new("/tmp/runtime"),
            Path::new("/tmp/host"),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(profile.contains("/tmp/a project"));
        assert!(profile.contains("file-write-data"));
        assert!(profile.contains("(allow file-write* (subpath \"/tmp/a project\"))"));
        assert!(profile.contains("\\.env"));
        assert!(profile.contains("deny file-read* file-write* (subpath \"/tmp/host\")"));
    }

    #[test]
    fn profile_allows_a_shadow_nested_under_the_host() {
        let profile = build_profile(
            &Policy::default_agent(),
            Path::new("/tmp/host/.deltabox/shadow"),
            Path::new("/tmp/runtime"),
            Path::new("/tmp/host"),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(!profile.contains("deny file-read* file-write* (subpath \"/tmp/host\")"));
    }

    #[test]
    fn profile_denies_an_external_private_host() {
        let profile = build_profile(
            &Policy::default_agent(),
            Path::new("/tmp/private/live"),
            Path::new("/tmp/runtime"),
            Path::new("/tmp/host"),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(profile.contains("deny file-read* file-write* (subpath \"/tmp/host\")"));
    }

    #[test]
    fn project_clone_uses_cow_and_preserves_links_and_mode() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tool"), "hello").unwrap();
        std::fs::set_permissions(
            src.path().join("tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("tool", src.path().join("link")).unwrap();
        clone_project_tree(src.path(), &dst.path().join("shadow")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.path().join("shadow/tool")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::symlink_metadata(dst.path().join("shadow/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read_link(dst.path().join("shadow/link")).unwrap(),
            PathBuf::from("tool")
        );
    }

    #[test]
    fn authenticated_proxy_relays_only_allowlisted_targets() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let target_port = listener.local_addr().unwrap().port();
        let target = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut ping = [0u8; 4];
            stream.read_exact(&mut ping).unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"pong").unwrap();
        });
        let proxy = ProxyHandle::start(vec![format!("localhost:{target_port}")], 8).unwrap();
        let mut stream = proxy_connect_request(
            proxy.port,
            &proxy.token,
            &format!("localhost:{target_port}"),
        );
        let response = read_proxy_headers(&mut stream).unwrap();
        assert!(String::from_utf8_lossy(&response).contains("200 Connection Established"));
        stream.write_all(b"ping").unwrap();
        let mut pong = [0u8; 4];
        stream.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
        target.join().unwrap();

        let mut denied = proxy_connect_request(proxy.port, &proxy.token, "localhost:9");
        let response = read_proxy_headers(&mut denied).unwrap();
        assert!(String::from_utf8_lossy(&response).contains("403 Forbidden"));
    }

    #[test]
    fn hostname_allowlist_profile_exposes_only_the_proxy() {
        let mut policy = Policy::default_agent();
        policy.network.mode = NetworkMode::Allowlist;
        policy.network.allow = vec!["registry.npmjs.org:443".into()];
        let profile = build_profile(
            &policy,
            Path::new("/tmp/shadow"),
            Path::new("/tmp/runtime"),
            Path::new("/tmp/host"),
            Some(43210),
            &[],
            &[],
        )
        .unwrap();
        assert!(profile.contains("localhost:43210"));
        assert!(!profile.contains("registry.npmjs.org"));
    }
}

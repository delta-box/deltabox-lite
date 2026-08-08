//! Linux backend powered by sandlock-core.
//!
//! A confined worker process owns the COW upper layer across exec calls. The
//! supervisor (this process) drives the worker over piped stdio and performs
//! checkpoint freezing, commit merges, and abort restores on the COW layer.

use super::SandboxBackend;
use crate::error::{Error, Result};
use crate::policy::{NetworkMode, Policy};
use crate::protocol::{AbortResult, CommitResult, ExecRequest, ExecResult, SessionSpec};
use crate::snapshot::{copy_tree, move_paths_to_dir};
use crate::worker::{ScanEntry, WorkerCommand, WorkerEvent, WorkerExec};
use sandlock_core::sandbox::{BranchAction, ByteSize, Process, StdioMode};
use sandlock_core::Sandbox;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// Result type used for channels between the caller and the session runtime.
type ChanResult<T> = std::result::Result<T, String>;

pub struct SandlockBackend {
    worker_bin: PathBuf,
    direct_write: bool,
    sessions: HashMap<String, SessionRuntime>,
}

struct SessionRuntime {
    tx: tokio_mpsc::Sender<InnerCmd>,
    thread: Option<std::thread::JoinHandle<()>>,
    spec: SessionSpec,
}

enum InnerCmd {
    Exec {
        req: ExecRequest,
        resp: std_mpsc::Sender<ChanResult<ExecResult>>,
    },
    Scan {
        resp: std_mpsc::Sender<ChanResult<Vec<String>>>,
    },
    CurrentUpper {
        resp: std_mpsc::Sender<PathBuf>,
    },
    Freeze {
        dest: PathBuf,
        prev: Option<PathBuf>,
        resp: std_mpsc::Sender<ChanResult<()>>,
    },
    Commit {
        head_dir: PathBuf,
        deleted: Vec<String>,
        resp: std_mpsc::Sender<ChanResult<usize>>,
    },
    Abort {
        resp: std_mpsc::Sender<ChanResult<()>>,
    },
    Stop {
        resp: std_mpsc::Sender<ChanResult<()>>,
    },
    Ping {
        resp: std_mpsc::Sender<ChanResult<()>>,
    },
    Write {
        path: PathBuf,
        content: Vec<u8>,
        resp: std_mpsc::Sender<ChanResult<()>>,
    },
}

impl SandlockBackend {
    pub fn new(worker_bin: Option<PathBuf>) -> Self {
        let worker_bin = worker_bin.unwrap_or_else(|| {
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("deltabox"))
        });
        Self {
            worker_bin,
            direct_write: false,
            sessions: HashMap::new(),
        }
    }

    /// Run directly against a private, caller-owned transaction tree.
    /// Sandlock still provides Landlock, seccomp, network, and resource
    /// isolation, but its syscall-level COW layer is disabled.
    pub fn new_direct(worker_bin: Option<PathBuf>) -> Self {
        let mut backend = Self::new(worker_bin);
        backend.direct_write = true;
        backend
    }

    fn call<T: Send + 'static>(
        &self,
        session_id: &str,
        make: impl FnOnce(std_mpsc::Sender<ChanResult<T>>) -> InnerCmd,
        timeout: Duration,
    ) -> Result<T> {
        let runtime = self
            .sessions
            .get(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))?;
        let (tx, rx) = std_mpsc::channel();
        runtime
            .tx
            .blocking_send(make(tx))
            .map_err(|_| Error::Backend("session runtime closed".into()))?;
        let res = rx
            .recv_timeout(timeout)
            .map_err(|_| Error::Backend("session command timed out".into()))?;
        res.map_err(Error::Backend)
    }
}

impl Default for SandlockBackend {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Drop for SandlockBackend {
    fn drop(&mut self) {
        let sessions = std::mem::take(&mut self.sessions);
        for (session_id, _) in sessions {
            let _ = self.stop_session(&session_id);
        }
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
        let (tx, rx) = tokio_mpsc::channel(16);
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let thread_spec = spec.clone();
        let worker_bin = self.worker_bin.clone();
        let direct_write = self.direct_write;
        let sid = session_id.to_string();
        let thread = std::thread::spawn(move || {
            run_session(sid, thread_spec, worker_bin, direct_write, rx, ready_tx)
        });
        ready_rx
            .recv_timeout(READY_TIMEOUT)
            .map_err(|_| Error::Backend("session startup timed out".into()))?
            .map_err(Error::Backend)?;
        self.sessions.insert(
            session_id.to_string(),
            SessionRuntime {
                tx,
                thread: Some(thread),
                spec: spec.clone(),
            },
        );
        Ok(())
    }

    fn stop_session(&mut self, session_id: &str) -> Result<()> {
        let Some(runtime) = self.sessions.remove(session_id) else {
            return Ok(());
        };
        let (tx, rx) = std_mpsc::channel();
        let _ = runtime.tx.blocking_send(InnerCmd::Stop { resp: tx });
        let _ = rx.recv_timeout(Duration::from_secs(15));
        if let Some(thread) = runtime.thread {
            let _ = thread.join();
        }
        Ok(())
    }

    fn ping(&mut self, session_id: &str) -> Result<()> {
        self.call(
            session_id,
            |resp| InnerCmd::Ping { resp },
            Duration::from_secs(10),
        )
    }

    fn exec(&mut self, session_id: &str, req: ExecRequest) -> Result<ExecResult> {
        self.call(
            session_id,
            |resp| InnerCmd::Exec { req, resp },
            COMMAND_TIMEOUT,
        )
    }

    fn read_file(&mut self, session_id: &str, path: &Path) -> Result<Option<Vec<u8>>> {
        let Some(runtime) = self.sessions.get(session_id) else {
            return Err(Error::SessionNotFound(session_id.to_string()));
        };
        let project = runtime.spec.project.clone();
        let policy = runtime.spec.policy.clone();
        let rel = path
            .strip_prefix(&project)
            .map_err(|_| Error::InvalidPolicy("fs.read outside project".into()))?;
        if policy.path_is_denied(rel) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        let _ = runtime;
        let upper = self.current_upper(session_id)?;
        let candidates = [upper.join(rel), project.join(rel)];
        for candidate in candidates {
            if let Ok(link_meta) = std::fs::symlink_metadata(&candidate) {
                if link_meta.file_type().is_symlink() {
                    return Err(Error::Backend(
                        "fs.read refuses symlinks in the legacy backend".into(),
                    ));
                }
                let canonical = candidate.canonicalize()?;
                let allowed_root = if candidate.starts_with(&upper) {
                    &upper
                } else {
                    &project
                };
                if !canonical.starts_with(allowed_root.canonicalize()?) {
                    return Err(Error::Backend("fs.read symlink escapes project".into()));
                }
                let meta = std::fs::metadata(&canonical)?;
                if meta.is_file() {
                    if meta.len() > 16 * 1024 * 1024 {
                        return Err(Error::Backend("fs.read file exceeds 16 MiB limit".into()));
                    }
                    return Ok(Some(std::fs::read(canonical)?));
                }
            }
        }
        Ok(None)
    }

    fn write_file(&mut self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        let Some(runtime) = self.sessions.get(session_id) else {
            return Err(Error::SessionNotFound(session_id.to_string()));
        };
        let project = runtime.spec.project.clone();
        let policy = runtime.spec.policy.clone();
        let rel = path
            .strip_prefix(&project)
            .map_err(|_| Error::InvalidPolicy("fs.write outside project".into()))?;
        if policy.path_is_denied(rel) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        if policy
            .limits
            .max_write_bytes
            .is_some_and(|limit| content.len() as u64 > limit)
        {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        let _ = runtime;
        let path = rel.to_path_buf();
        self.call(
            session_id,
            move |resp| InnerCmd::Write {
                path,
                content,
                resp,
            },
            COMMAND_TIMEOUT,
        )
    }

    fn scan(&mut self, session_id: &str) -> Result<Vec<ScanEntry>> {
        let paths: Vec<String> =
            self.call(session_id, |resp| InnerCmd::Scan { resp }, COMMAND_TIMEOUT)?;
        let Some(runtime) = self.sessions.get(session_id) else {
            return Err(Error::SessionNotFound(session_id.to_string()));
        };
        let project = runtime.spec.project.clone();
        let upper = self.current_upper(session_id)?;
        Ok(crate::snapshot::build_scan_entries(
            &paths, &upper, &project,
        ))
    }

    fn current_upper(&mut self, session_id: &str) -> Result<PathBuf> {
        let runtime = self
            .sessions
            .get(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))?;
        let (tx, rx) = std_mpsc::channel();
        runtime
            .tx
            .blocking_send(InnerCmd::CurrentUpper { resp: tx })
            .map_err(|_| Error::Backend("session runtime closed".into()))?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| Error::Backend("current_upper timed out".into()))
    }

    fn freeze_upper(
        &mut self,
        session_id: &str,
        dest_dir: &Path,
        prev_upper: Option<&Path>,
    ) -> Result<()> {
        let dest = dest_dir.to_path_buf();
        let prev = prev_upper.map(|p| p.to_path_buf());
        self.call(
            session_id,
            move |resp| InnerCmd::Freeze { dest, prev, resp },
            Duration::from_secs(120),
        )
    }

    fn commit_changes(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        head_dir: &Path,
        deleted: Vec<String>,
    ) -> Result<CommitResult> {
        let Some(runtime) = self.sessions.remove(session_id) else {
            return Err(Error::SessionNotFound(session_id.to_string()));
        };
        let head_dir = head_dir.to_path_buf();
        let (tx, rx) = std_mpsc::channel();
        runtime
            .tx
            .blocking_send(InnerCmd::Commit {
                head_dir,
                deleted,
                resp: tx,
            })
            .map_err(|_| Error::Backend("session runtime closed".into()))?;
        let applied = rx
            .recv_timeout(Duration::from_secs(300))
            .map_err(|_| Error::Backend("commit timed out".into()))?
            .map_err(Error::Backend)?;
        if let Some(thread) = runtime.thread {
            let _ = thread.join();
        }
        self.start_session(session_id, &runtime.spec)?;
        Ok(CommitResult {
            checkpoint_id: checkpoint_id.to_string(),
            applied,
        })
    }

    fn materialize_changes(
        &mut self,
        session_id: &str,
        head_dir: &Path,
        deleted: Vec<String>,
    ) -> Result<usize> {
        let Some(runtime) = self.sessions.remove(session_id) else {
            return Err(Error::SessionNotFound(session_id.to_string()));
        };
        let (tx, rx) = std_mpsc::channel();
        runtime
            .tx
            .blocking_send(InnerCmd::Commit {
                head_dir: head_dir.to_path_buf(),
                deleted,
                resp: tx,
            })
            .map_err(|_| Error::Backend("session runtime closed".into()))?;
        let applied = rx
            .recv_timeout(Duration::from_secs(300))
            .map_err(|_| Error::Backend("materialize timed out".into()))?
            .map_err(Error::Backend)?;
        if let Some(thread) = runtime.thread {
            let _ = thread.join();
        }
        Ok(applied)
    }

    fn abort_changes(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        head_dir: &Path,
        deleted: Vec<String>,
    ) -> Result<AbortResult> {
        let Some(runtime) = self.sessions.remove(session_id) else {
            return Err(Error::SessionNotFound(session_id.to_string()));
        };
        let (tx, rx) = std_mpsc::channel();
        runtime
            .tx
            .blocking_send(InnerCmd::Abort { resp: tx })
            .map_err(|_| Error::Backend("session runtime closed".into()))?;
        rx.recv_timeout(Duration::from_secs(300))
            .map_err(|_| Error::Backend("abort timed out".into()))?
            .map_err(Error::Backend)?;
        if let Some(thread) = runtime.thread {
            let _ = thread.join();
        }
        let project = runtime.spec.project.clone();
        self.start_session(session_id, &runtime.spec)?;
        let upper = self.current_upper(session_id)?;
        let checkpoint_upper = head_dir.join("upper");
        let copied = if checkpoint_upper.exists() {
            copy_tree(&checkpoint_upper, &upper)?
        } else {
            0
        };
        let moved = move_paths_to_dir(&project, &deleted, &head_dir.join("hidden"))?;
        let restored = copied + moved;
        Ok(AbortResult {
            checkpoint_id: checkpoint_id.to_string(),
            restored,
        })
    }
}

fn run_session(
    session_id: String,
    spec: SessionSpec,
    worker_bin: PathBuf,
    direct_write: bool,
    rx: tokio_mpsc::Receiver<InnerCmd>,
    ready_tx: std_mpsc::Sender<ChanResult<()>>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
            return;
        }
    };
    let result = runtime.block_on(serve_loop(spec, worker_bin, direct_write, rx, ready_tx));
    if let Err(e) = result {
        eprintln!("deltabox: session {session_id} runtime error: {e}");
    }
}

async fn serve_loop(
    spec: SessionSpec,
    worker_bin: PathBuf,
    direct_write: bool,
    mut cmd_rx: tokio_mpsc::Receiver<InnerCmd>,
    ready_tx: std_mpsc::Sender<ChanResult<()>>,
) -> Result<()> {
    let fs_storage = spec.store_dir.join("cow");
    std::fs::create_dir_all(&fs_storage)?;
    let mut sandbox = build_sandbox(&spec, &worker_bin, direct_write)?;
    let mut pending: HashMap<u64, oneshot::Sender<ChanResult<serde_json::Value>>> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut fatal: Option<String> = None;

    let (mut process, current_upper, writer_tx, mut evt_rx) =
        match spawn_worker(&mut sandbox, &worker_bin, &spec, &fs_storage, direct_write).await {
            Ok((p, upper, wtx, erx)) => (Some(p), upper, Some(wtx), Some(erx)),
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return Err(e);
            }
        };
    let _ = ready_tx.send(Ok(()));
    let mut ready = true;

    loop {
        tokio::select! {
            evt = async { evt_rx.as_mut().expect("event receiver").recv().await } => {
                match evt {
                    Some(Some(event)) => {
                        handle_event(event, &mut pending, &mut ready, &mut fatal);
                    }
                    Some(None) | None => {
                        if fatal.is_none() {
                            fatal = Some("worker exited unexpectedly".into());
                            fail_all(&mut pending, "worker exited");
                        }
                        evt_rx = Some(dead_channel());
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    InnerCmd::Exec { req, resp } => {
                        if let Some(f) = &fatal {
                            let _ = resp.send(Err(f.clone()));
                            continue;
                        }
                        if !ready {
                            let _ = resp.send(Err("worker not ready".into()));
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        let (otx, orx) = oneshot::channel();
                        pending.insert(id, otx);
                        let line = serde_json::to_string(&WorkerCommand::Exec {
                            id,
                            exec: WorkerExec {
                                argv: req.argv,
                                cwd: req.cwd,
                                env: req.env.unwrap_or_default(),
                            },
                        })?;
                        if writer_tx.as_ref().expect("writer").send(line).is_err() {
                            pending.remove(&id);
                            let _ = resp.send(Err("worker pipe closed".into()));
                            continue;
                        }
                        tokio::spawn(async move {
                            let out = match orx.await {
                                Ok(Ok(value)) => {
                                    serde_json::from_value::<ExecResult>(value)
                                        .map_err(|e| e.to_string())
                                }
                                Ok(Err(e)) => Err(e),
                                Err(_) => Err("worker disconnected".into()),
                            };
                            let _ = resp.send(out);
                        });
                    }
                    InnerCmd::Scan { resp } => {
                        if let Some(f) = &fatal {
                            let _ = resp.send(Err(f.clone()));
                            continue;
                        }
                        if !ready {
                            let _ = resp.send(Err("worker not ready".into()));
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        let (otx, orx) = oneshot::channel();
                        pending.insert(id, otx);
                        let line = serde_json::to_string(&WorkerCommand::Scan { id })?;
                        if writer_tx.as_ref().expect("writer").send(line).is_err() {
                            pending.remove(&id);
                            let _ = resp.send(Err("worker pipe closed".into()));
                            continue;
                        }
                        tokio::spawn(async move {
                            let out = match orx.await {
                                Ok(Ok(value)) => serde_json::from_value::<Vec<String>>(value)
                                    .map_err(|e| e.to_string()),
                                Ok(Err(e)) => Err(e),
                                Err(_) => Err("worker disconnected".into()),
                            };
                            let _ = resp.send(out);
                        });
                    }
                    InnerCmd::CurrentUpper { resp } => {
                        let _ = resp.send(current_upper.clone());
                    }
                    InnerCmd::Freeze { dest, prev, resp } => {
                        let r = crate::snapshot::copy_tree_incremental(
                            &current_upper,
                            prev.as_deref(),
                            &dest.join("upper"),
                        )
                        .map(|_| ());
                        let _ = resp.send(r.map_err(|e| e.to_string()));
                    }
                    InnerCmd::Commit {
                        head_dir,
                        deleted,
                        resp,
                    } => {
                        if let Some(mut p) = process.take() {
                            let _ = p.kill();
                            let _ = p.wait().await;
                        }
                        fail_all(&mut pending, "session interrupted");
                        let result: crate::error::Result<usize> = (|| {
                            if direct_write {
                                return Ok(0);
                            }
                            let copied = copy_tree(&current_upper, &spec.project)?;
                            maybe_test_crash("materialize_after_copy");
                            let moved = move_paths_to_dir(
                                &spec.project,
                                &deleted,
                                &head_dir.join("hidden"),
                            )?;
                            if let Some(branch) = upper_branch_dir(&current_upper) {
                                let _ = std::fs::remove_dir_all(branch);
                            }
                            Ok(copied + moved)
                        })();
                        let _ = resp.send(result.map_err(|e| e.to_string()));
                        break;
                    }
                    InnerCmd::Abort { resp } => {
                        if let Some(mut p) = process.take() {
                            let _ = p.kill();
                            let _ = p.wait().await;
                        }
                        fail_all(&mut pending, "session interrupted");
                        if !direct_write {
                            if let Some(branch) = upper_branch_dir(&current_upper) {
                                let _ = std::fs::remove_dir_all(branch);
                            }
                        }
                        let _ = resp.send(Ok(()));
                        break;
                    }
                    InnerCmd::Stop { resp } => {
                        if let Some(mut p) = process.take() {
                            let _ = p.kill();
                            let _ = p.wait().await;
                        }
                        fail_all(&mut pending, "session interrupted");
                        if !direct_write {
                            if let Some(branch) = upper_branch_dir(&current_upper) {
                                let _ = std::fs::remove_dir_all(branch);
                            }
                        }
                        let _ = resp.send(Ok(()));
                        break;
                    }
                    InnerCmd::Ping { resp } => {
                        if !ready {
                            let _ = resp.send(Err("worker not ready".into()));
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        let (otx, orx) = oneshot::channel();
                        pending.insert(id, otx);
                        let line = serde_json::to_string(&WorkerCommand::Ping { id })?;
                        if writer_tx.as_ref().expect("writer").send(line).is_err() {
                            pending.remove(&id);
                            let _ = resp.send(Err("worker pipe closed".into()));
                            continue;
                        }
                        tokio::spawn(async move {
                            let out = match orx.await {
                                Ok(Ok(_)) => Ok(()),
                                Ok(Err(e)) => Err(e),
                                Err(_) => Err("worker disconnected".into()),
                            };
                            let _ = resp.send(out);
                        });
                    }
                    InnerCmd::Write {
                        path,
                        content,
                        resp,
                    } => {
                        use base64::Engine;
                        if !ready {
                            let _ = resp.send(Err("worker not ready".into()));
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        let (otx, orx) = oneshot::channel();
                        pending.insert(id, otx);
                        let line = serde_json::to_string(&WorkerCommand::Write {
                            id,
                            path: path.to_string_lossy().into_owned(),
                            content_base64: base64::engine::general_purpose::STANDARD
                                .encode(&content),
                        })?;
                        if writer_tx.as_ref().expect("writer").send(line).is_err() {
                            pending.remove(&id);
                            let _ = resp.send(Err("worker pipe closed".into()));
                            continue;
                        }
                        tokio::spawn(async move {
                            let out = match orx.await {
                                Ok(Ok(_)) => Ok(()),
                                Ok(Err(e)) => Err(e),
                                Err(_) => Err("worker disconnected".into()),
                            };
                            let _ = resp.send(out);
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

fn maybe_test_crash(point: &str) {
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_AT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

fn handle_event(
    event: WorkerEvent,
    pending: &mut HashMap<u64, oneshot::Sender<ChanResult<serde_json::Value>>>,
    ready: &mut bool,
    fatal: &mut Option<String>,
) {
    match event {
        WorkerEvent::Ready => *ready = true,
        WorkerEvent::Ok { id, result } => {
            if let Some(tx) = pending.remove(&id) {
                let _ = tx.send(Ok(result));
            }
        }
        WorkerEvent::Err { id, message } => {
            if let Some(tx) = pending.remove(&id) {
                let _ = tx.send(Err(message));
            }
        }
        WorkerEvent::Fatal(message) => {
            *fatal = Some(message);
            fail_all(pending, "worker fatal");
        }
    }
}

fn fail_all(
    pending: &mut HashMap<u64, oneshot::Sender<ChanResult<serde_json::Value>>>,
    message: &str,
) {
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(message.to_string()));
    }
}

fn dead_channel() -> tokio_mpsc::Receiver<Option<WorkerEvent>> {
    let (_, rx) = tokio_mpsc::channel(1);
    rx
}

fn upper_branch_dir(upper: &Path) -> Option<PathBuf> {
    upper.parent().map(|p| p.to_path_buf())
}

fn build_sandbox(spec: &SessionSpec, worker_bin: &Path, direct_write: bool) -> Result<Sandbox> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    spec.project.hash(&mut hasher);
    let sandbox_name = format!("deltabox-{:016x}", hasher.finish());
    let sandbox_home = spec.store_dir.join("home");
    let sandbox_tmp = spec.store_dir.join("tmp");
    std::fs::create_dir_all(&sandbox_home)?;
    std::fs::create_dir_all(&sandbox_tmp)?;
    let mut builder = Sandbox::builder()
        .name(sandbox_name)
        .cwd(&spec.project)
        .on_exit(BranchAction::Keep)
        .on_error(BranchAction::Keep)
        .fs_read(&spec.project)
        .fs_write(&spec.project)
        .fs_read("/proc")
        .fs_read("/dev")
        .fs_write("/dev/null")
        .fs_write("/dev/full")
        .fs_read("/tmp")
        .fs_read_if_exists("/lib64")
        .fs_read_if_exists("/usr/local");
    if !direct_write {
        builder = builder
            .workdir(&spec.project)
            .fs_storage(spec.store_dir.join("cow"));
    }
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
    if let Some(parent) = worker_bin.parent() {
        builder = builder.fs_read(parent);
    }
    for p in resolve_read_paths(&spec.policy, &spec.project) {
        builder = builder.fs_read(&p);
    }
    for p in resolve_write_paths(&spec.policy, &spec.project) {
        builder = builder.fs_write(&p);
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
    builder
        .build()
        .map_err(|e| Error::Backend(format!("sandlock build: {e}")))
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

async fn spawn_worker<'a>(
    sandbox: &'a mut Sandbox,
    worker_bin: &Path,
    spec: &SessionSpec,
    fs_storage: &Path,
    direct_write: bool,
) -> Result<(
    Process<'a>,
    PathBuf,
    std_mpsc::Sender<String>,
    tokio_mpsc::Receiver<Option<WorkerEvent>>,
)> {
    let before = list_branch_dirs(fs_storage)?;
    let argv = [
        worker_bin.to_string_lossy().into_owned(),
        "worker".to_string(),
        "--root".to_string(),
        spec.project.to_string_lossy().into_owned(),
        "--runtime".to_string(),
        spec.store_dir.to_string_lossy().into_owned(),
    ];
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let mut process = sandbox
        .popen(
            &argv_refs,
            StdioMode::Piped,
            StdioMode::Piped,
            StdioMode::Piped,
        )
        .await
        .map_err(|e| Error::Backend(format!("worker popen: {e}")))?;
    let stdin = std::fs::File::from(
        process
            .take_stdin()
            .ok_or_else(|| Error::Backend("worker stdin not piped".into()))?,
    );
    let stdout = process
        .take_stdout()
        .ok_or_else(|| Error::Backend("worker stdout not piped".into()))?;
    let stderr = process
        .take_stderr()
        .ok_or_else(|| Error::Backend("worker stderr not piped".into()))?;

    let (wtx, wrx) = std_mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut writer = stdin;
        while let Ok(line) = wrx.recv() {
            if writer.write_all(line.as_bytes()).is_err()
                || writer.write_all(b"\n").is_err()
                || writer.flush().is_err()
            {
                break;
            }
        }
    });

    let (etx, mut erx) = tokio_mpsc::channel::<Option<WorkerEvent>>(64);
    std::thread::spawn(move || {
        let file = std::fs::File::from(stdout);
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let event = serde_json::from_str::<WorkerEvent>(trimmed)
                        .unwrap_or_else(|e| WorkerEvent::Fatal(format!("bad worker event: {e}")));
                    if etx.blocking_send(Some(event)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = etx.blocking_send(None);
    });
    std::thread::spawn(move || {
        let file = std::fs::File::from(stderr);
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        eprintln!("deltabox-worker: {trimmed}");
                    }
                }
                Err(_) => break,
            }
        }
    });

    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            match erx.recv().await {
                Some(Some(WorkerEvent::Ready)) => break,
                Some(Some(WorkerEvent::Fatal(m))) => {
                    return Err(Error::Backend(format!("worker fatal: {m}")))
                }
                Some(Some(_)) => continue,
                Some(None) => return Err(Error::Backend("worker exited before ready".into())),
                None => return Err(Error::Backend("worker event channel closed".into())),
            }
        }
        Ok::<(), Error>(())
    })
    .await
    .map_err(|_| Error::Backend("worker ready timeout".into()))??;

    let upper = if direct_write {
        spec.project.clone()
    } else {
        let after = list_branch_dirs(fs_storage)?;
        let new_branch = after
            .into_iter()
            .find(|name| !before.contains(name))
            .ok_or_else(|| Error::Backend("COW branch not created".into()))?;
        fs_storage.join(new_branch).join("upper")
    };
    Ok((process, upper, wtx, erx))
}

fn list_branch_dirs(fs_storage: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !fs_storage.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(fs_storage)? {
        let entry = entry?;
        if entry.path().is_dir() {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
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
        let sandbox = build_sandbox(&spec, Path::new("/bin/sh"), true).unwrap();
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
        let sandbox = build_sandbox(&spec, Path::new("/bin/sh"), true).unwrap();
        assert!(!sandbox.fs_denied.contains(&project));
    }
}

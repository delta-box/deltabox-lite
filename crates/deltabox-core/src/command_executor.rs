//! Session-scoped command executor shared by independent CLI processes.
//!
//! The helper owns only warm, reconstructible sandbox runtime state. Durable
//! filesystem state remains authoritative in the platform transaction layer.

use crate::backend::SandboxBackend;
use crate::error::{Error, Result};
use crate::protocol::{ExecRequest, ExecResult, SessionSpec};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const EXEC_TIMEOUT: Duration = Duration::from_secs(615);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const LISTENER_POLL_TIMEOUT_MS: libc::c_int = 100;
const RUNTIME_ENVIRONMENT_KEYS: &[&str] = &["HOME", "PATH", "RUSTUP_HOME", "SDKROOT"];

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub ready_file: PathBuf,
    pub operation_lock: PathBuf,
}

impl Endpoint {
    pub fn new(socket: PathBuf, token_file: PathBuf, ready_file: PathBuf) -> Self {
        let operation_lock = ready_file.with_extension("lock");
        Self {
            socket,
            token_file,
            ready_file,
            operation_lock,
        }
    }
}

/// Held by state-changing manager operations and by the helper while a command
/// is live. This keeps an orphaned exec serialized after its CLI parent exits.
pub struct OperationLock(File);

impl OperationLock {
    fn acquire(path: &Path) -> Result<Self> {
        prepare_private_parent(path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn lock_operations(endpoint: &Endpoint) -> Result<OperationLock> {
    OperationLock::acquire(&endpoint.operation_lock)
}

/// Capture the small inherited environment allowlist at dispatch time. A
/// persistent helper may have been launched by an older CLI process, so these
/// values travel with each request; explicit values retain final precedence.
pub fn capture_environment(
    explicit: Option<Vec<(String, String)>>,
) -> Option<Vec<(String, String)>> {
    let mut environment = Vec::new();
    for key in [
        "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TERM", "TZ", "USER",
    ] {
        if let Ok(value) = std::env::var(key) {
            environment.push((key.to_owned(), value));
        }
    }
    environment.extend(explicit.unwrap_or_default());
    (!environment.is_empty()).then_some(environment)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub endpoint: Endpoint,
    pub worker_bin: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReadyRecord {
    pid: u32,
    process_start: ProcessStart,
    runtime_environment: Vec<(String, Vec<u8>)>,
    nonce: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ProcessStart {
    coarse: u64,
    fine: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    token: String,
    #[serde(flatten)]
    command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Command {
    Ping,
    Exec {
        session_id: String,
        generation: u64,
        spec: SessionSpec,
        request: ExecRequest,
    },
    Reset,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ResultValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StartConfirmation {
    start: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum ResultValue {
    Pong,
    Started,
    NetworkStarted { version: u32 },
    Exec(ExecResult),
    Reset,
    ShuttingDown,
}

trait CommandBackend: Send + 'static {
    fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()>;
    fn stop_session(&mut self, session_id: &str) -> Result<()>;
    fn exec(&mut self, session_id: &str, request: ExecRequest) -> Result<ExecResult>;
}

struct SandboxCommandBackend<T>(T);

impl<T: SandboxBackend + 'static> CommandBackend for SandboxCommandBackend<T> {
    fn start_session(&mut self, session_id: &str, spec: &SessionSpec) -> Result<()> {
        SandboxBackend::start_session(&mut self.0, session_id, spec)
    }

    fn stop_session(&mut self, session_id: &str) -> Result<()> {
        SandboxBackend::stop_session(&mut self.0, session_id)
    }

    fn exec(&mut self, session_id: &str, request: ExecRequest) -> Result<ExecResult> {
        SandboxBackend::exec(&mut self.0, session_id, request)
    }
}

struct ActiveSession {
    session_id: String,
    fingerprint: Vec<u8>,
}

struct PersistentExecutor<B: CommandBackend> {
    backend: B,
    active: Option<ActiveSession>,
}

impl<B: CommandBackend> PersistentExecutor<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            active: None,
        }
    }

    fn exec(
        &mut self,
        session_id: &str,
        generation: u64,
        spec: &SessionSpec,
        request: ExecRequest,
    ) -> Result<ExecResult> {
        let fingerprint = serde_json::to_vec(&(generation, spec))?;
        let matches = self.active.as_ref().is_some_and(|active| {
            active.session_id == session_id && active.fingerprint == fingerprint
        });
        if !matches {
            self.stop()?;
            self.backend.start_session(session_id, spec)?;
            self.active = Some(ActiveSession {
                session_id: session_id.to_owned(),
                fingerprint,
            });
        }

        match self.backend.exec(session_id, request) {
            Ok(result) => {
                if spec.policy.network.allow_approval {
                    self.stop()?;
                }
                Ok(result)
            }
            Err(error) => {
                // Never retry an exec automatically: the caller cannot know
                // whether a lost response followed observable side effects.
                // Drop the unhealthy warm runtime so the next request starts
                // from the durable filesystem view.
                if let Err(cleanup) = self.stop() {
                    return Err(Error::Backend(format!(
                        "{error}; command executor cleanup failed: {cleanup}"
                    )));
                }
                Err(error)
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        self.backend.stop_session(&active.session_id)
    }
}

impl<B: CommandBackend> Drop for PersistentExecutor<B> {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Start the helper process if the session does not already own a responsive
/// executor. The helper survives the invoking CLI process and is safe to
/// recreate because it owns no authoritative filesystem state.
pub fn ensure_running(endpoint: &Endpoint, helper_bin: &Path, log_file: &Path) -> Result<()> {
    let expected_environment = runtime_environment();
    // The next Exec request is itself a bounded, authenticated health check.
    // Trust the atomically-published process identity on the hot path instead
    // of paying for a separate Ping connection before every command.
    if helper_is_compatible(endpoint, &expected_environment)? {
        return Ok(());
    }
    if helper_is_alive(endpoint)? {
        shutdown(endpoint)?;
    }

    remove_if_exists(&endpoint.ready_file)?;
    remove_if_exists(&endpoint.socket)?;
    prepare_private_parent(&endpoint.socket)?;
    if let Some(parent) = log_file.parent() {
        fs::create_dir_all(parent)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_file)?;
    let mut command = ProcessCommand::new(helper_bin);
    command
        .arg("command-helper")
        .arg("--socket")
        .arg(&endpoint.socket)
        .arg("--token-file")
        .arg(&endpoint.token_file)
        .arg("--ready-file")
        .arg(&endpoint.ready_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    if let Some(runtime) = endpoint.socket.parent() {
        command.current_dir(runtime);
    }
    scrub_helper_environment(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| Error::Backend(format!("start command executor: {error}")))?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if helper_is_compatible(endpoint, &expected_environment)? {
            thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(Error::Backend(format!(
                "command executor exited during startup with {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Backend(
                "command executor did not become ready".into(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn exec(
    endpoint: &Endpoint,
    session_id: &str,
    generation: u64,
    spec: SessionSpec,
    exec_request: ExecRequest,
) -> Result<ExecResult> {
    match request(
        endpoint,
        Command::Exec {
            session_id: session_id.to_owned(),
            generation,
            spec,
            request: exec_request,
        },
    )? {
        ResultValue::Exec(result) => Ok(result),
        _ => Err(Error::Backend(
            "unexpected command executor response".into(),
        )),
    }
}

pub fn shutdown(endpoint: &Endpoint) -> Result<()> {
    match request(endpoint, Command::Shutdown) {
        Ok(ResultValue::ShuttingDown) => {}
        Ok(_) => {
            return Err(Error::Backend(
                "unexpected command executor shutdown response".into(),
            ))
        }
        Err(error) => {
            if helper_is_alive(endpoint)? {
                return Err(error);
            }
            remove_if_exists(&endpoint.ready_file)?;
            remove_if_exists(&endpoint.socket)?;
            return Ok(());
        }
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !helper_is_alive(endpoint)? {
            remove_if_exists(&endpoint.ready_file)?;
            remove_if_exists(&endpoint.socket)?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(Error::Backend(
        "command executor did not stop after shutdown".into(),
    ))
}

pub fn reset(endpoint: &Endpoint) -> Result<()> {
    match request(endpoint, Command::Reset) {
        Ok(ResultValue::Reset) => Ok(()),
        Ok(_) => Err(Error::Backend(
            "unexpected command executor reset response".into(),
        )),
        Err(error) => {
            if helper_is_alive(endpoint)? {
                Err(error)
            } else {
                Ok(())
            }
        }
    }
}

/// Best-effort removal of reconstructible control artifacts after the owning
/// session has become durably stopped. Callers still hold the session and
/// operation locks here, so unlinking the open lock file cannot split live
/// mutual exclusion.
pub fn cleanup_stopped(endpoint: &Endpoint) {
    if !matches!(helper_is_alive(endpoint), Ok(false)) {
        return;
    }
    let _ = fs::remove_file(&endpoint.ready_file);
    let _ = fs::remove_file(&endpoint.socket);
    let _ = fs::remove_file(&endpoint.operation_lock);
    if let Some(runtime) = endpoint.socket.parent() {
        let _ = fs::remove_dir(runtime);
    }
}

pub fn run(config: Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    let backend = crate::backend::linux_sandlock::SandlockBackend::new_direct(Some(
        config.worker_bin.clone(),
    ));
    #[cfg(target_os = "macos")]
    let backend = crate::backend::macos_seatbelt::SeatbeltBackend::new_direct();
    run_with_backend(config, SandboxCommandBackend(backend))
}

fn run_with_backend<B: CommandBackend>(config: Config, backend: B) -> Result<()> {
    let token = fs::read_to_string(&config.endpoint.token_file).map_err(|error| {
        Error::Backend(format!(
            "read command executor token {}: {error}",
            config.endpoint.token_file.display()
        ))
    })?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(Error::Backend("command executor token is empty".into()));
    }

    prepare_private_parent(&config.endpoint.socket)?;
    remove_if_exists(&config.endpoint.socket)?;
    let listener = UnixListener::bind(&config.endpoint.socket)?;
    fs::set_permissions(&config.endpoint.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let socket_metadata = fs::metadata(&config.endpoint.socket)?;
    let ready = ReadyRecord {
        pid: std::process::id(),
        process_start: process_start(std::process::id())
            .ok_or_else(|| Error::Backend("read command executor process start identity".into()))?,
        runtime_environment: runtime_environment(),
        nonce: uuid::Uuid::new_v4().to_string(),
    };
    write_ready(&config.endpoint.ready_file, &ready)?;
    let cleanup = ArtifactGuard {
        endpoint: config.endpoint.clone(),
        socket_dev: socket_metadata.dev(),
        socket_ino: socket_metadata.ino(),
        nonce: ready.nonce.clone(),
    };

    let executor = Arc::new(Mutex::new(PersistentExecutor::new(backend)));
    let running = Arc::new(AtomicBool::new(true));
    let mut connections = Vec::new();
    while running.load(Ordering::Acquire) && ready_matches(&config.endpoint.ready_file, &ready) {
        reap_connections(&mut connections);
        if !listener_has_connection(&listener)? {
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let token = token.clone();
                let executor = Arc::clone(&executor);
                let running = Arc::clone(&running);
                let operation_lock = config.endpoint.operation_lock.clone();
                connections.push(thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_connection(stream, &token, &operation_lock, &executor, &running)
                    }));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            eprintln!("deltabox command executor connection: {error}");
                        }
                        Err(_) => {
                            // A backend panic may leave its warm state
                            // inconsistent. Retire the whole helper; the next
                            // request can rebuild it from durable state.
                            running.store(false, Ordering::Release);
                            eprintln!("deltabox command executor backend panicked");
                        }
                    }
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.into()),
        }
    }
    running.store(false, Ordering::Release);
    for connection in connections {
        let _ = connection.join();
    }
    drop(executor);
    drop(cleanup);
    Ok(())
}

fn handle_connection<B: CommandBackend>(
    mut stream: UnixStream,
    token: &str,
    operation_lock: &Path,
    executor: &Arc<Mutex<PersistentExecutor<B>>>,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    // Accepted sockets inherit O_NONBLOCK from the listener on Darwin. The
    // line-oriented handshake relies on bounded blocking reads instead.
    stream.set_nonblocking(false)?;
    let Some(request) = authenticated_request(&mut stream, token)? else {
        return Ok(());
    };
    let result = match request.command {
        Command::Ping => Ok(ResultValue::Pong),
        Command::Exec {
            session_id,
            generation,
            spec,
            request,
        } => {
            let operation = OperationLock::acquire(operation_lock)?;
            // The prepared response begins the lock handoff. The caller must
            // confirm receipt before execution; after that confirmation, the
            // helper owns the operation lock until every observable side
            // effect has finished.
            let started = if spec.policy.network.allow_approval {
                ResultValue::NetworkStarted { version: 1 }
            } else {
                ResultValue::Started
            };
            write_response(&mut stream, Response::ok(started))?;
            if !read_start_confirmation(&mut stream)? {
                return Ok(());
            }
            let result = executor
                .lock()
                .map_err(|_| Error::Backend("command executor lock poisoned".into()))?
                .exec(&session_id, generation, &spec, request)
                .map(ResultValue::Exec);
            let retire = result.is_err();
            drop(operation);
            if retire {
                // A backend error can include a failed runtime cleanup. Retire
                // the process as well as its ActiveSession so a successful
                // health check cannot preserve partially torn-down state.
                running.store(false, Ordering::Release);
            }
            write_response(
                &mut stream,
                match result {
                    Ok(result) => Response::ok(result),
                    Err(error) => Response::error(error.to_string()),
                },
            )?;
            return Ok(());
        }
        Command::Shutdown => {
            let result = executor
                .lock()
                .map_err(|_| Error::Backend("command executor lock poisoned".into()))?
                .stop()
                .map(|()| ResultValue::ShuttingDown);
            if result.is_ok() {
                running.store(false, Ordering::Release);
            }
            result
        }
        Command::Reset => executor
            .lock()
            .map_err(|_| Error::Backend("command executor lock poisoned".into()))?
            .stop()
            .map(|()| ResultValue::Reset),
    };
    write_response(
        &mut stream,
        match result {
            Ok(result) => Response::ok(result),
            Err(error) => Response::error(error.to_string()),
        },
    )?;
    Ok(())
}

fn request(endpoint: &Endpoint, command: Command) -> Result<ResultValue> {
    let token = fs::read_to_string(&endpoint.token_file)?;
    let mut stream = UnixStream::connect(&endpoint.socket)?;
    let expects_start = matches!(&command, Command::Exec { .. });
    let needs_network = matches!(&command, Command::Exec { spec, .. } if spec.policy.network.allow_approval);
    let initial_timeout = match &command {
        // Only the post-Started response may legitimately take as long as the
        // command. A stalled helper must fail its initial handshake quickly.
        Command::Exec { .. } => CONNECT_TIMEOUT,
        Command::Shutdown => EXEC_TIMEOUT,
        Command::Reset => STOP_TIMEOUT,
        Command::Ping => CONNECT_TIMEOUT,
    };
    stream.set_read_timeout(Some(initial_timeout))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    serde_json::to_writer(
        &mut stream,
        &Request {
            token: token.trim().to_owned(),
            command,
        },
    )?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let result = read_result(&mut reader)?;
    if expects_start {
        let compatible = if needs_network {
            matches!(result, ResultValue::NetworkStarted { version: 1 })
        } else {
            matches!(result, ResultValue::Started)
        };
        if !compatible {
            return Err(Error::Backend(
                "command executor did not acknowledge the operation lock or required network approval protocol".into(),
            ));
        }
        // Once Started has crossed the wire the helper may observe command
        // side effects, so failures below this point must never be retried.
        reader.get_mut().set_read_timeout(Some(EXEC_TIMEOUT))?;
        serde_json::to_writer(reader.get_mut(), &StartConfirmation { start: true })?;
        reader.get_mut().write_all(b"\n")?;
        reader.get_mut().flush()?;
        return read_result(&mut reader);
    }
    Ok(result)
}

fn read_start_confirmation(stream: &mut UnixStream) -> Result<bool> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    if line.is_empty() {
        return Ok(false);
    }
    let confirmation: StartConfirmation = serde_json::from_str(&line)?;
    Ok(confirmation.start)
}

fn read_result(reader: &mut impl BufRead) -> Result<ResultValue> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(Error::Backend(
            "command executor closed the control connection".into(),
        ));
    }
    let response: Response = serde_json::from_str(&line)?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(error)) => Err(Error::Backend(format!("command executor: {error}"))),
        _ => Err(Error::Backend(
            "command executor returned an empty response".into(),
        )),
    }
}

fn reap_connections(connections: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < connections.len() {
        if connections[index].is_finished() {
            let connection = connections.swap_remove(index);
            let _ = connection.join();
        } else {
            index += 1;
        }
    }
}

fn listener_has_connection(listener: &UnixListener) -> Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut descriptor, 1, LISTENER_POLL_TIMEOUT_MS) };
        if ready > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                return Err(std::io::Error::from_raw_os_error(libc::EBADF).into());
            }
            return Ok(true);
        }
        if ready == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

fn authenticated_request(stream: &mut UnixStream, token: &str) -> Result<Option<Request>> {
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    if peer_uid(stream)? != unsafe { libc::geteuid() } {
        write_response(
            stream,
            Response::error("command executor peer is not the session owner"),
        )?;
        return Ok(None);
    }
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            write_response(stream, Response::error(error.to_string()))?;
            return Ok(None);
        }
    };
    if request.token != token {
        write_response(stream, Response::error("invalid command executor token"))?;
        return Ok(None);
    }
    Ok(Some(request))
}

fn write_response(stream: &mut UnixStream, response: Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

impl Response {
    fn ok(result: ResultValue) -> Self {
        Self {
            result: Some(result),
            error: None,
        }
    }

    fn error(error: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(error.into()),
        }
    }
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_ready(path: &Path, ready: &ReadyRecord) -> Result<()> {
    prepare_private_parent(path)?;
    let temporary = path.with_extension(format!("{}.tmp", ready.nonce));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, ready)?;
    file.write_all(b"\n")?;
    file.flush()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn ready_record(path: &Path) -> Result<Option<ReadyRecord>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn ready_matches(path: &Path, expected: &ReadyRecord) -> bool {
    ready_record(path).is_ok_and(|ready| {
        ready.is_some_and(|ready| ready.pid == expected.pid && ready.nonce == expected.nonce)
    })
}

fn helper_is_alive(endpoint: &Endpoint) -> Result<bool> {
    Ok(ready_record(&endpoint.ready_file)?.is_some_and(|ready| {
        process_start(ready.pid).is_some_and(|start| start == ready.process_start)
    }))
}

fn helper_is_compatible(
    endpoint: &Endpoint,
    expected_environment: &[(String, Vec<u8>)],
) -> Result<bool> {
    Ok(ready_record(&endpoint.ready_file)?.is_some_and(|ready| {
        ready.runtime_environment == expected_environment
            && process_start(ready.pid).is_some_and(|start| start == ready.process_start)
    }))
}

fn runtime_environment() -> Vec<(String, Vec<u8>)> {
    RUNTIME_ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| ((*key).to_owned(), value.as_os_str().as_bytes().to_vec()))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<ProcessStart> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    let mut fields = after_name.split_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    Some(ProcessStart {
        coarse: fields.nth(18)?.parse().ok()?,
        fine: 0,
    })
}

#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<ProcessStart> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(size).ok()?,
        )
    };
    if read != i32::try_from(size).ok()? {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(ProcessStart {
        coarse: info.pbi_start_tvsec,
        fine: info.pbi_start_tvusec,
    })
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn scrub_helper_environment(command: &mut ProcessCommand) {
    command.env_clear();
    for key in [
        "DEVELOPER_DIR",
        "HOME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LOGNAME",
        "PATH",
        "RUSTUP_HOME",
        "SDKROOT",
        "SHELL",
        "TERM",
        "TMPDIR",
        "TZ",
        "USER",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

struct ArtifactGuard {
    endpoint: Endpoint,
    socket_dev: u64,
    socket_ino: u64,
    nonce: String,
}

impl Drop for ArtifactGuard {
    fn drop(&mut self) {
        if fs::metadata(&self.endpoint.socket).is_ok_and(|metadata| {
            metadata.dev() == self.socket_dev && metadata.ino() == self.socket_ino
        }) {
            let _ = fs::remove_file(&self.endpoint.socket);
        }
        if ready_record(&self.endpoint.ready_file)
            .is_ok_and(|ready| ready.is_some_and(|ready| ready.nonce == self.nonce))
        {
            let _ = fs::remove_file(&self.endpoint.ready_file);
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result == 0 {
        Ok(credentials.uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counts {
        starts: AtomicUsize,
        stops: AtomicUsize,
        execs: AtomicUsize,
        fail_next: AtomicBool,
        panic_next: AtomicBool,
        block_exec: AtomicBool,
        exec_entered: AtomicBool,
    }

    struct FakeBackend {
        counts: Arc<Counts>,
    }

    impl CommandBackend for FakeBackend {
        fn start_session(&mut self, _session_id: &str, _spec: &SessionSpec) -> Result<()> {
            self.counts.starts.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn stop_session(&mut self, _session_id: &str) -> Result<()> {
            self.counts.stops.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn exec(&mut self, _session_id: &str, request: ExecRequest) -> Result<ExecResult> {
            self.counts.execs.fetch_add(1, Ordering::Relaxed);
            self.counts.exec_entered.store(true, Ordering::Release);
            while self.counts.block_exec.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            if self.counts.panic_next.swap(false, Ordering::Relaxed) {
                panic!("injected backend panic");
            }
            if self.counts.fail_next.swap(false, Ordering::Relaxed) {
                return Err(Error::Backend("injected exec failure".into()));
            }
            Ok(ExecResult {
                exit_code: Some(0),
                stdout: request.argv.join(" "),
                stderr: String::new(),
                duration_ms: 1,
                truncated: false,
                network_denials: Vec::new(),
            })
        }
    }

    fn spec(root: &Path) -> SessionSpec {
        SessionSpec {
            project: root.to_path_buf(),
            host_project: None,
            store_dir: root.join("runtime"),
            policy: Policy::default_agent(),
            resolved_deny_paths: None,
        }
    }

    fn exec_request(value: &str) -> ExecRequest {
        ExecRequest {
            argv: vec![value.into()],
            cwd: None,
            env: None,
        }
    }

    #[test]
    fn persistent_executor_reuses_matching_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let mut executor = PersistentExecutor::new(FakeBackend {
            counts: Arc::clone(&counts),
        });
        executor
            .exec("session", 1, &spec(directory.path()), exec_request("one"))
            .unwrap();
        executor
            .exec("session", 1, &spec(directory.path()), exec_request("two"))
            .unwrap();

        assert_eq!(counts.starts.load(Ordering::Relaxed), 1);
        assert_eq!(counts.execs.load(Ordering::Relaxed), 2);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 0);
        executor.stop().unwrap();
        assert_eq!(counts.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn network_approval_runtime_is_closed_after_each_exec() {
        let directory = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let mut executor = PersistentExecutor::new(FakeBackend { counts: Arc::clone(&counts) });
        let mut spec = spec(directory.path());
        spec.policy.network.mode = crate::policy::NetworkMode::Allowlist;
        spec.policy.network.allow_approval = true;
        for _ in 0..2 { executor.exec("session", 1, &spec, exec_request("one")).unwrap(); }
        assert_eq!(counts.starts.load(Ordering::Relaxed), 2);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 2);
        assert!(executor.active.is_none());
    }

    #[test]
    fn generation_change_restarts_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let mut executor = PersistentExecutor::new(FakeBackend {
            counts: Arc::clone(&counts),
        });
        executor
            .exec("session", 1, &spec(directory.path()), exec_request("one"))
            .unwrap();
        executor
            .exec("session", 2, &spec(directory.path()), exec_request("two"))
            .unwrap();

        assert_eq!(counts.starts.load(Ordering::Relaxed), 2);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_exec_is_not_retried_and_invalidates_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        counts.fail_next.store(true, Ordering::Relaxed);
        let mut executor = PersistentExecutor::new(FakeBackend {
            counts: Arc::clone(&counts),
        });
        assert!(executor
            .exec("session", 1, &spec(directory.path()), exec_request("one"))
            .is_err());
        assert_eq!(counts.execs.load(Ordering::Relaxed), 1);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 1);

        executor
            .exec("session", 1, &spec(directory.path()), exec_request("two"))
            .unwrap();
        assert_eq!(counts.starts.load(Ordering::Relaxed), 2);
        assert_eq!(counts.execs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn compatible_ready_record_skips_a_separate_ping() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("missing.sock"),
            directory.path().join("missing-token"),
            directory.path().join("ready"),
        );
        write_ready(
            &endpoint.ready_file,
            &ReadyRecord {
                pid: std::process::id(),
                process_start: process_start(std::process::id()).unwrap(),
                runtime_environment: runtime_environment(),
                nonce: "test-ready".into(),
            },
        )
        .unwrap();

        ensure_running(
            &endpoint,
            Path::new("helper-must-not-start"),
            &directory.path().join("helper.log"),
        )
        .unwrap();
    }

    #[test]
    fn listener_poll_reports_a_pending_connection() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("poll.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = UnixStream::connect(&socket).unwrap();

        assert!(listener_has_connection(&listener).unwrap());
        let (accepted, _) = listener.accept().unwrap();
        drop(accepted);
        drop(client);
    }

    #[test]
    fn socket_server_reuses_backend_across_connections() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("executor.sock"),
            directory.path().join("token"),
            directory.path().join("ready"),
        );
        fs::write(&endpoint.token_file, "test-token\n").unwrap();
        let counts = Arc::new(Counts::default());
        let server_counts = Arc::clone(&counts);
        let server_endpoint = endpoint.clone();
        let directory_path = directory.path().to_path_buf();
        let server = thread::spawn(move || {
            run_with_backend(
                Config {
                    endpoint: server_endpoint,
                    worker_bin: PathBuf::from("unused"),
                },
                FakeBackend {
                    counts: server_counts,
                },
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(request(&endpoint, Command::Ping), Ok(ResultValue::Pong)) {
            assert!(Instant::now() < deadline, "server did not become ready");
            thread::sleep(Duration::from_millis(10));
        }
        for value in ["one", "two"] {
            let result = exec(
                &endpoint,
                "session",
                1,
                spec(&directory_path),
                exec_request(value),
            )
            .unwrap();
            assert_eq!(result.stdout, value);
        }
        shutdown(&endpoint).unwrap();
        server.join().unwrap();
        assert_eq!(counts.starts.load(Ordering::Relaxed), 1);
        assert_eq!(counts.execs.load(Ordering::Relaxed), 2);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn network_execution_rejects_legacy_helper_before_start_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("executor.sock"),
            directory.path().join("token"),
            directory.path().join("ready"),
        );
        fs::write(&endpoint.token_file, "test-token\n").unwrap();
        let listener = UnixListener::bind(&endpoint.socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            write_response(reader.get_mut(), Response::ok(ResultValue::Started)).unwrap();
            line.clear();
            assert_eq!(reader.read_line(&mut line).unwrap(), 0, "must close without authorizing execution");
        });
        let mut network_spec = spec(directory.path());
        network_spec.policy.network.allow_approval = true;
        assert!(exec(&endpoint, "session", 1, network_spec, exec_request("must-not-run")).is_err());
        server.join().unwrap();
    }

    #[test]
    fn disconnected_client_before_confirmation_does_not_execute() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("executor.sock"),
            directory.path().join("token"),
            directory.path().join("ready"),
        );
        fs::write(&endpoint.token_file, "test-token\n").unwrap();
        let counts = Arc::new(Counts::default());
        let server_counts = Arc::clone(&counts);
        let server_endpoint = endpoint.clone();
        let root = directory.path().to_path_buf();
        let server = thread::spawn(move || {
            run_with_backend(
                Config {
                    endpoint: server_endpoint,
                    worker_bin: PathBuf::from("unused"),
                },
                FakeBackend {
                    counts: server_counts,
                },
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(request(&endpoint, Command::Ping), Ok(ResultValue::Pong)) {
            assert!(Instant::now() < deadline, "server did not become ready");
            thread::sleep(Duration::from_millis(10));
        }

        let mut stream = UnixStream::connect(&endpoint.socket).unwrap();
        serde_json::to_writer(
            &mut stream,
            &Request {
                token: "test-token".into(),
                command: Command::Exec {
                    session_id: "session".into(),
                    generation: 1,
                    spec: spec(&root),
                    request: exec_request("must-not-run"),
                },
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        assert!(matches!(
            read_result(&mut reader).unwrap(),
            ResultValue::Started
        ));
        drop(reader);

        let operation = lock_operations(&endpoint).unwrap();
        assert_eq!(counts.execs.load(Ordering::Relaxed), 0);
        drop(operation);
        shutdown(&endpoint).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn backend_error_retires_helper() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("executor.sock"),
            directory.path().join("token"),
            directory.path().join("ready"),
        );
        fs::write(&endpoint.token_file, "test-token\n").unwrap();
        let counts = Arc::new(Counts::default());
        counts.fail_next.store(true, Ordering::Relaxed);
        let server_counts = Arc::clone(&counts);
        let server_endpoint = endpoint.clone();
        let root = directory.path().to_path_buf();
        let server = thread::spawn(move || {
            run_with_backend(
                Config {
                    endpoint: server_endpoint,
                    worker_bin: PathBuf::from("unused"),
                },
                FakeBackend {
                    counts: server_counts,
                },
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(request(&endpoint, Command::Ping), Ok(ResultValue::Pong)) {
            assert!(Instant::now() < deadline, "server did not become ready");
            thread::sleep(Duration::from_millis(10));
        }

        assert!(exec(&endpoint, "session", 1, spec(&root), exec_request("fail"),).is_err());
        server.join().unwrap();
        assert_eq!(counts.execs.load(Ordering::Relaxed), 1);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 1);
        assert!(!endpoint.ready_file.exists());
        assert!(!endpoint.socket.exists());
    }

    #[test]
    fn backend_panic_retires_helper() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("executor.sock"),
            directory.path().join("token"),
            directory.path().join("ready"),
        );
        fs::write(&endpoint.token_file, "test-token\n").unwrap();
        let counts = Arc::new(Counts::default());
        counts.panic_next.store(true, Ordering::Relaxed);
        let server_counts = Arc::clone(&counts);
        let server_endpoint = endpoint.clone();
        let root = directory.path().to_path_buf();
        let server = thread::spawn(move || {
            run_with_backend(
                Config {
                    endpoint: server_endpoint,
                    worker_bin: PathBuf::from("unused"),
                },
                FakeBackend {
                    counts: server_counts,
                },
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(request(&endpoint, Command::Ping), Ok(ResultValue::Pong)) {
            assert!(Instant::now() < deadline, "server did not become ready");
            thread::sleep(Duration::from_millis(10));
        }

        assert!(exec(&endpoint, "session", 1, spec(&root), exec_request("panic"),).is_err());
        server.join().unwrap();
        assert_eq!(counts.execs.load(Ordering::Relaxed), 1);
        assert_eq!(counts.stops.load(Ordering::Relaxed), 1);
        assert!(!endpoint.ready_file.exists());
        assert!(!endpoint.socket.exists());
    }

    #[test]
    fn live_exec_holds_operation_lock_after_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            directory.path().join("executor.sock"),
            directory.path().join("token"),
            directory.path().join("ready"),
        );
        fs::write(&endpoint.token_file, "test-token\n").unwrap();
        let counts = Arc::new(Counts::default());
        counts.block_exec.store(true, Ordering::Relaxed);
        let server_counts = Arc::clone(&counts);
        let server_endpoint = endpoint.clone();
        let server = thread::spawn(move || {
            run_with_backend(
                Config {
                    endpoint: server_endpoint,
                    worker_bin: PathBuf::from("unused"),
                },
                FakeBackend {
                    counts: server_counts,
                },
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(request(&endpoint, Command::Ping), Ok(ResultValue::Pong)) {
            assert!(Instant::now() < deadline, "server did not become ready");
            thread::sleep(Duration::from_millis(10));
        }

        let client_endpoint = endpoint.clone();
        let root = directory.path().to_path_buf();
        let client = thread::spawn(move || {
            exec(
                &client_endpoint,
                "session",
                1,
                spec(&root),
                exec_request("blocked"),
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !counts.exec_entered.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "exec did not reach backend");
            thread::sleep(Duration::from_millis(1));
        }

        let lock_endpoint = endpoint.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            let _lock = lock_operations(&lock_endpoint).unwrap();
            acquired_tx.send(()).unwrap();
        });
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        counts.block_exec.store(false, Ordering::Release);
        client.join().unwrap();
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
        shutdown(&endpoint).unwrap();
        server.join().unwrap();
    }
}

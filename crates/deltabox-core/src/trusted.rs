//! Unsandboxed command execution for trusted, in-process integrations.
//!
//! This path deliberately inherits the embedding process environment.  It is
//! intended for callers that value minimum overhead and shared configuration
//! over process isolation; the default coordinator mode remains sandboxed.

use crate::error::{Error, Result};
use crate::protocol::{ExecRequest, ExecResult};
use crate::worker::{read_capped, MAX_EXEC_OUTPUT_BYTES};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Instant;
use uuid::Uuid;

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

struct OutputChunk {
    stream: OutputStream,
    bytes: Option<Vec<u8>>,
}

/// Long-lived Bash used by trusted embedded sessions. Commands execute in a
/// subshell, so `exit`, `exec`, `cd`, exports, and shell options cannot poison
/// the worker that serves the next request.
pub(crate) struct PersistentBash {
    child: Child,
    stdin: ChildStdin,
    output: Receiver<OutputChunk>,
    ambient_environment: BTreeMap<String, String>,
}

impl PersistentBash {
    fn start() -> Result<Self> {
        let ambient_environment = current_environment();
        let mut command = Command::new("bash");
        command
            .args(["--noprofile", "--norc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A separate process group lets a cancel take down the shell together
        // with every command it started.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|error| Error::Backend(format!("persistent bash spawn failed: {error}")))?;
        let stdin = child.stdin.take().expect("bash stdin is piped");
        let stdout = child.stdout.take().expect("bash stdout is piped");
        let stderr = child.stderr.take().expect("bash stderr is piped");
        // Bound queued output so a noisy command cannot allocate without limit
        // while the executor is applying its per-stream capture ceiling.
        let (sender, output) = mpsc::sync_channel(64);
        spawn_output_reader(stdout, OutputStream::Stdout, sender.clone());
        spawn_output_reader(stderr, OutputStream::Stderr, sender);
        Ok(Self {
            child,
            stdin,
            output,
            ambient_environment,
        })
    }

    fn supports(request: &ExecRequest) -> bool {
        request.argv.len() == 3
            && request.argv[1] == "-c"
            && Path::new(&request.argv[0])
                .file_name()
                .is_some_and(|name| name == "bash")
            && request
                .cwd
                .as_ref()
                .is_some_and(|path| path.to_str().is_some_and(|value| !value.contains('\0')))
            && request.env.as_ref().is_none_or(|values| {
                values.iter().all(|(key, value)| {
                    valid_environment_name(key) && !key.contains('\0') && !value.contains('\0')
                })
            })
            && !request.argv[2].contains('\0')
    }

    fn execute(&mut self, request: &ExecRequest) -> Result<ExecResult> {
        if !Self::supports(request) {
            return Err(Error::Backend(
                "persistent bash received an unsupported request".into(),
            ));
        }
        let token = format!("DELTABOX_{}", Uuid::new_v4().simple());
        let stdout_prefix = format!("\x1e{token}:").into_bytes();
        let stderr_marker = format!("\x1e{token}\x1f").into_bytes();
        let cwd = request
            .cwd
            .as_ref()
            .and_then(|path| path.to_str())
            .expect("supports validated cwd");
        let ambient_environment = current_environment();
        let mut script = String::new();
        for key in self
            .ambient_environment
            .keys()
            .filter(|key| valid_environment_name(key) && !ambient_environment.contains_key(*key))
        {
            script.push_str("unset -v ");
            script.push_str(key);
            script.push('\n');
        }
        for (key, value) in &ambient_environment {
            if valid_environment_name(key) && self.ambient_environment.get(key) != Some(value) {
                script.push_str("export ");
                script.push_str(&bash_quote(&format!("{key}={value}")));
                script.push('\n');
            }
        }
        script.push_str("(\ncd -- ");
        script.push_str(&bash_quote(cwd));
        script.push_str(" || exit 125\n");
        if let Some(environment) = &request.env {
            for (key, value) in environment {
                script.push_str("export ");
                script.push_str(&bash_quote(&format!("{key}={value}")));
                script.push('\n');
            }
        }
        script.push_str("eval -- ");
        script.push_str(&bash_quote(&request.argv[2]));
        script.push_str("\n)\n__deltabox_status=$?\nprintf '\\036%s:%d\\037' ");
        script.push_str(&bash_quote(&token));
        script.push_str(" \"$__deltabox_status\"\nprintf '\\036%s\\037' ");
        script.push_str(&bash_quote(&token));
        script.push_str(" >&2\n");

        let started = Instant::now();
        // The command's lifetime is bound to this call: a cancel kills the
        // shell's whole process group, which ends the wait below.
        #[cfg(unix)]
        let _cancel = {
            let pgid = self.child.id() as i32;
            crate::cancel::register(move || crate::cancel::kill_process_group(pgid))
        };
        self.stdin
            .write_all(script.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|error| Error::Backend(format!("persistent bash write failed: {error}")))?;
        self.ambient_environment = ambient_environment;

        let mut stdout = MarkerCollector::status(stdout_prefix);
        let mut stderr = MarkerCollector::exact(stderr_marker);
        while !stdout.done() || !stderr.done() {
            match self.output.recv() {
                Ok(OutputChunk {
                    stream,
                    bytes: Some(bytes),
                }) => match stream {
                    OutputStream::Stdout => stdout.push(&bytes)?,
                    OutputStream::Stderr => stderr.push(&bytes)?,
                },
                Ok(OutputChunk {
                    stream,
                    bytes: None,
                }) => {
                    let name = match stream {
                        OutputStream::Stdout => "stdout",
                        OutputStream::Stderr => "stderr",
                    };
                    return Err(Error::Backend(format!(
                        "persistent bash closed {name} before command completion"
                    )));
                }
                Err(_) => {
                    return Err(Error::Backend("persistent bash output closed".into()));
                }
            }
        }
        Ok(ExecResult {
            exit_code: stdout.status,
            stdout: String::from_utf8_lossy(&stdout.output).into_owned(),
            stderr: String::from_utf8_lossy(&stderr.output).into_owned(),
            duration_ms: started.elapsed().as_millis() as u64,
            truncated: stdout.truncated || stderr.truncated,
            network_denials: Vec::new(),
        })
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PersistentBash {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct MarkerCollector {
    marker: Vec<u8>,
    parse_status: bool,
    scan: Vec<u8>,
    output: Vec<u8>,
    status: Option<i32>,
    complete: bool,
    truncated: bool,
}

impl MarkerCollector {
    fn exact(marker: Vec<u8>) -> Self {
        Self {
            marker,
            parse_status: false,
            scan: Vec::new(),
            output: Vec::new(),
            status: None,
            complete: false,
            truncated: false,
        }
    }

    fn status(prefix: Vec<u8>) -> Self {
        Self {
            marker: prefix,
            parse_status: true,
            scan: Vec::new(),
            output: Vec::new(),
            status: None,
            complete: false,
            truncated: false,
        }
    }

    fn done(&self) -> bool {
        self.complete
    }

    fn push(&mut self, bytes: &[u8]) -> Result<()> {
        if self.complete {
            return Ok(());
        }
        self.scan.extend_from_slice(bytes);
        if let Some(position) = find_bytes(&self.scan, &self.marker) {
            if self.parse_status {
                let status_start = position + self.marker.len();
                let Some(status_end) = self.scan[status_start..]
                    .iter()
                    .position(|byte| *byte == 0x1f)
                    .map(|offset| status_start + offset)
                else {
                    if position > 0 {
                        let prefix = self.scan.drain(..position).collect::<Vec<_>>();
                        self.append_output(&prefix);
                    }
                    return Ok(());
                };
                let status = std::str::from_utf8(&self.scan[status_start..status_end])
                    .ok()
                    .and_then(|value| value.parse::<i32>().ok())
                    .ok_or_else(|| {
                        Error::Backend("persistent bash returned invalid status".into())
                    })?;
                let prefix = self.scan[..position].to_vec();
                self.append_output(&prefix);
                self.status = Some(status);
            } else {
                let prefix = self.scan[..position].to_vec();
                self.append_output(&prefix);
            }
            self.scan.clear();
            self.complete = true;
            return Ok(());
        }

        let keep = self.marker.len().saturating_sub(1).max(32);
        if self.scan.len() > keep {
            let emit = self.scan.len() - keep;
            let prefix = self.scan.drain(..emit).collect::<Vec<_>>();
            self.append_output(&prefix);
        }
        Ok(())
    }

    fn append_output(&mut self, bytes: &[u8]) {
        let room = MAX_EXEC_OUTPUT_BYTES.saturating_sub(self.output.len());
        self.output
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.truncated |= bytes.len() > room;
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

fn spawn_output_reader(
    mut reader: impl Read + Send + 'static,
    stream: OutputStream,
    sender: SyncSender<OutputChunk>,
) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    let _ = sender.send(OutputChunk {
                        stream,
                        bytes: None,
                    });
                    break;
                }
                Ok(length) => {
                    if sender
                        .send(OutputChunk {
                            stream,
                            bytes: Some(buffer[..length].to_vec()),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

fn current_environment() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Reuse Bash for the common `bash -c` trusted path and preserve the generic
/// direct-spawn behavior for all other argv shapes.
pub(crate) fn exec_reusing_bash(
    shell: &mut Option<PersistentBash>,
    request: ExecRequest,
) -> Result<ExecResult> {
    if !PersistentBash::supports(&request) {
        return exec(request);
    }
    if shell.is_none() {
        *shell = Some(PersistentBash::start()?);
    }
    let result = shell
        .as_mut()
        .expect("persistent bash was initialized")
        .execute(&request);
    if result.is_err() {
        *shell = None;
    }
    result
}

/// Execute a command directly as a child of the embedding process.
pub fn exec(request: ExecRequest) -> Result<ExecResult> {
    let Some(program) = request.argv.first() else {
        return Err(Error::InvalidPolicy("empty argv".into()));
    };

    let mut command = Command::new(program);
    command.args(&request.argv[1..]);
    if let Some(cwd) = request.cwd {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Do not clear the environment: sharing the host process configuration is
    // the defining contract of trusted embedding. Explicit values win.
    if let Some(environment) = request.env {
        command.envs(environment);
    }

    // Run the command in its own process group so a cancel reaches its
    // children, not just the direct child.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| Error::Backend(format!("exec spawn failed: {error}")))?;
    #[cfg(unix)]
    let _cancel = {
        let pgid = child.id() as i32;
        crate::cancel::register(move || crate::cancel::kill_process_group(pgid))
    };
    // Windows has no process groups; a job object plays the same role. If the
    // job cannot be created the command still runs, it just cannot be
    // cancelled as a tree.
    #[cfg(windows)]
    let _cancel = job::Job::containing(&child)
        .map(std::sync::Arc::new)
        .map(|job| crate::cancel::register(move || job.terminate()));
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || read_capped(stdout));
    let stderr_reader = thread::spawn(move || read_capped(stderr));

    // The command lives exactly as long as this call; there is no wall-clock
    // limit here. Callers end it through their cancel scope.
    let status = child
        .wait()
        .map_err(|error| Error::Backend(format!("exec wait failed: {error}")))?;

    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| Error::Backend("stdout reader failed".into()))?;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| Error::Backend("stderr reader failed".into()))?;
    Ok(ExecResult {
        exit_code: status.code(),
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis() as u64,
        truncated: stdout_truncated || stderr_truncated,
        network_denials: Vec::new(),
    })
}

/// A Windows job object that holds a command and its descendants so they can
/// be terminated together.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Create a job that kills its processes when closed and put `child` in it.
        pub fn containing(child: &std::process::Child) -> Option<Job> {
            // SAFETY: plain Win32 calls with valid arguments; the handle is
            // owned by the returned `Job` and closed exactly once in `Drop`.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return None;
                }
                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return None;
                }
                Some(job)
            }
        }

        pub fn terminate(&self) {
            // SAFETY: `self.0` is a live job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closes the handle this job owns, once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn inherits_process_environment_and_applies_overrides() {
        const INHERITED: &str = "DELTABOX_TRUSTED_INHERITED_TEST";
        std::env::set_var(INHERITED, "host-value");
        let result = exec(ExecRequest {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("printf '%s:%s' \"${INHERITED}\" \"$EXPLICIT_VALUE\""),
            ],
            cwd: None,
            env: Some(vec![("EXPLICIT_VALUE".into(), "override".into())]),
        })
        .unwrap();
        std::env::remove_var(INHERITED);

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "host-value:override");
    }

    #[test]
    fn persistent_bash_reuses_process_and_isolates_command_state() {
        let project = tempfile::tempdir().unwrap();
        let mut shell = None;
        let first = exec_reusing_bash(
            &mut shell,
            ExecRequest {
                argv: vec![
                    "bash".into(),
                    "-c".into(),
                    "printf '%s:%s' \"$$\" \"$VALUE\"; printf 'problem' >&2; exit 7".into(),
                ],
                cwd: Some(project.path().to_path_buf()),
                env: Some(vec![("VALUE".into(), "first value".into())]),
            },
        )
        .unwrap();
        let second = exec_reusing_bash(
            &mut shell,
            ExecRequest {
                argv: vec![
                    "/bin/bash".into(),
                    "-c".into(),
                    "printf '%s:%s' \"$$\" \"${VALUE-unset}\"".into(),
                ],
                cwd: Some(project.path().to_path_buf()),
                env: None,
            },
        )
        .unwrap();

        assert_eq!(first.exit_code, Some(7));
        assert_eq!(first.stderr, "problem");
        let (first_pid, first_value) = first.stdout.split_once(':').unwrap();
        let (second_pid, second_value) = second.stdout.split_once(':').unwrap();
        assert_eq!(first_pid, second_pid);
        assert_eq!(first_value, "first value");
        assert_eq!(second_value, "unset");
    }

    #[cfg(unix)]
    fn process_is_alive(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // A zombie has already been killed; it only awaits reaping.
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z"),
            Err(_) => unsafe { libc::kill(pid, 0) == 0 },
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_the_command_and_its_children_without_a_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("child.pid");
        let (_scope, handle) = crate::cancel::scope();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            handle.cancel();
        });
        let started = Instant::now();
        let result = exec(ExecRequest {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("sleep 60 & echo $! > '{}'; wait", pid_file.display()),
            ],
            cwd: None,
            env: None,
        })
        .unwrap();
        canceller.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(
            result.exit_code, None,
            "killed by signal, not a normal exit"
        );
        let child: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_alive(child) {
            assert!(Instant::now() < deadline, "grandchild survived the cancel");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_a_running_persistent_bash_command() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("child.pid");
        let mut bash = PersistentBash::start().unwrap();
        let (_scope, handle) = crate::cancel::scope();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            handle.cancel();
        });
        let result = bash.execute(&ExecRequest {
            argv: vec![
                "bash".into(),
                "-c".into(),
                format!("sleep 60 & echo $! > '{}'; wait", pid_file.display()),
            ],
            cwd: Some(directory.path().to_path_buf()),
            env: None,
        });
        canceller.join().unwrap();
        assert!(result.is_err(), "the shell was killed with the command");
        let child: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_alive(child) {
            assert!(Instant::now() < deadline, "grandchild survived the cancel");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn cancel_kills_the_command_tree_without_a_timeout() {
        let (_scope, handle) = crate::cancel::scope();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            handle.cancel();
        });
        let started = Instant::now();
        // `ping` is a grandchild that would hold the output pipes open for a
        // minute; `exec` only returns promptly if the whole job was killed.
        let result = exec(ExecRequest {
            argv: vec![
                "cmd".into(),
                "/c".into(),
                "ping -n 60 127.0.0.1 >nul".into(),
            ],
            cwd: None,
            env: None,
        })
        .unwrap();
        canceller.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "cancel did not end the command tree"
        );
        assert_ne!(result.exit_code, Some(0));
    }
}

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
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

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
        let mut child = Command::new("bash")
            .args(["--noprofile", "--norc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
        self.stdin
            .write_all(script.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|error| Error::Backend(format!("persistent bash write failed: {error}")))?;
        self.ambient_environment = ambient_environment;

        let mut stdout = MarkerCollector::status(stdout_prefix);
        let mut stderr = MarkerCollector::exact(stderr_marker);
        let deadline = started + COMMAND_TIMEOUT;
        while !stdout.done() || !stderr.done() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.terminate();
                return Err(Error::Backend("session command timed out".into()));
            }
            match self.output.recv_timeout(remaining) {
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
                Err(RecvTimeoutError::Timeout) => {
                    self.terminate();
                    return Err(Error::Backend("session command timed out".into()));
                }
                Err(RecvTimeoutError::Disconnected) => {
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

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| Error::Backend(format!("exec spawn failed: {error}")))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || read_capped(stdout));
    let stderr_reader = thread::spawn(move || read_capped(stderr));

    let deadline = started + COMMAND_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| Error::Backend(format!("exec wait failed: {error}")))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Backend("session command timed out".into()));
        }
        thread::sleep(Duration::from_millis(1));
    };

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

#[cfg(test)]
mod tests {
    use super::*;

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
}

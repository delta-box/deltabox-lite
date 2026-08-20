//! Unsandboxed command execution for trusted, in-process integrations.
//!
//! This path deliberately inherits the embedding process environment.  It is
//! intended for callers that value minimum overhead and shared configuration
//! over process isolation; the default coordinator mode remains sandboxed.

use crate::error::{Error, Result};
use crate::protocol::{ExecRequest, ExecResult};
use crate::worker::read_capped;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

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
}

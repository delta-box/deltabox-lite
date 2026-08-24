//! Linux command isolation for zero-FUSE git-backed sessions.

use crate::error::{Error, Result};
use crate::protocol::ExecResult;
use crate::providers::{canonicalize_allow_missing, IsolationProvider};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use uuid::Uuid;

/// Fixed image used by the native-only Docker isolation profile.
pub const DOCKER_IMAGE: &str = "ubuntu:22.04";

/// One command may run for at most this long.
const EXEC_TIMEOUT: Duration = Duration::from_secs(600);

/// Host paths bind-mounted read-only into every sandbox.
const READ_ONLY_HOST_PATHS: [&str; 5] = ["/usr", "/lib", "/lib64", "/bin", "/etc"];

/// Environment keys preserved from the host; everything else is scrubbed.
const ENV_ALLOWLIST: [&str; 9] = [
    "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TERM", "TZ", "USER",
];

/// Confines commands against the real project tree with bubblewrap.
pub struct BubblewrapIsolation {
    project: PathBuf,
}

impl BubblewrapIsolation {
    pub fn new(project: PathBuf) -> Self {
        Self { project }
    }
}

impl IsolationProvider for BubblewrapIsolation {
    fn available(&self) -> bool {
        // Smoke probe: a bare `bwrap true` would run in an empty root and fail
        // to find the binary, so bind the host root read-only first.
        match Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => child.wait().map(|status| status.success()).unwrap_or(false),
            Err(_) => false,
        }
    }

    fn exec(
        &self,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<ExecResult> {
        if argv.is_empty() {
            return Err(Error::InvalidPolicy("empty argv".into()));
        }
        let sandbox_cwd = map_sandbox_cwd(&self.project, cwd)?;
        let mut cmd = Command::new("bwrap");
        for path in READ_ONLY_HOST_PATHS {
            cmd.arg("--ro-bind").arg(path).arg(path);
        }
        cmd.arg("--proc").arg("/proc");
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--tmpfs").arg("/tmp");
        cmd.arg("--bind").arg(&self.project).arg("/workspace");
        cmd.arg("--chdir").arg(&sandbox_cwd);
        cmd.arg("--unshare-net");
        cmd.arg("--die-with-parent");
        cmd.arg("--");
        cmd.args(&argv);

        cmd.env_clear();
        for key in ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        // Fresh writable home inside the per-exec tmpfs.
        cmd.env("HOME", "/tmp");
        cmd.env("TMPDIR", "/tmp");
        for (key, value) in env.unwrap_or_default() {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        run_bwrap(cmd)
    }
}

/// Confines commands in a fresh Docker container against the real project.
pub struct DockerIsolation {
    project: PathBuf,
}

impl DockerIsolation {
    pub fn new(project: PathBuf) -> Self {
        Self { project }
    }

    fn command(
        &self,
        container_name: &str,
        argv: &[String],
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<Command> {
        if argv.is_empty() {
            return Err(Error::InvalidPolicy("empty argv".into()));
        }
        let project = self.project.to_string_lossy();
        if project.contains(',') {
            return Err(Error::InvalidPolicy(
                "Docker project path must not contain a comma".into(),
            ));
        }
        let sandbox_cwd = map_sandbox_cwd(&self.project, cwd)?;
        let user = format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
            libc::getegid()
        });
        let mount = format!("type=bind,src={project},dst=/workspace");
        let mut cmd = Command::new("docker");
        cmd.args([
            "run",
            "--rm",
            "--init",
            "--name",
            container_name,
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--user",
            &user,
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,exec,mode=1777",
            "--mount",
            &mount,
            "--workdir",
            &sandbox_cwd,
        ]);
        for key in ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(key) {
                cmd.arg("--env")
                    .arg(format!("{key}={}", value.to_string_lossy()));
            }
        }
        cmd.args(["--env", "HOME=/tmp", "--env", "TMPDIR=/tmp"]);
        for (key, value) in env.unwrap_or_default() {
            cmd.arg("--env").arg(format!("{key}={value}"));
        }
        cmd.arg(DOCKER_IMAGE);
        cmd.args(argv);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        Ok(cmd)
    }
}

impl IsolationProvider for DockerIsolation {
    fn available(&self) -> bool {
        Command::new("docker")
            .args(["image", "inspect", DOCKER_IMAGE])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn exec(
        &self,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<ExecResult> {
        let container_name = format!("deltabox-{}", Uuid::new_v4());
        let command = self.command(&container_name, &argv, cwd, env)?;
        run_docker(command, &container_name)
    }
}

/// Map a host project path to its `/workspace` sandbox path.
fn map_sandbox_cwd(project: &Path, cwd: Option<PathBuf>) -> Result<String> {
    let relative = match cwd {
        Some(path) => {
            let full = if path.is_absolute() {
                path
            } else {
                project.join(path)
            };
            canonicalize_allow_missing(&full)?
                .strip_prefix(project)
                .map_err(|_| Error::InvalidPolicy("cwd outside project".into()))?
                .to_path_buf()
        }
        None => PathBuf::new(),
    };
    let mut sandbox = "/workspace".to_string();
    for component in relative.components() {
        sandbox.push('/');
        sandbox.push_str(&component.as_os_str().to_string_lossy());
    }
    Ok(sandbox)
}

/// Run bwrap to completion, capping output at the shared 8 MiB budget and
/// killing the sandbox after `EXEC_TIMEOUT`.
fn run_bwrap(mut cmd: Command) -> Result<ExecResult> {
    let start = crate::session::now_ms();
    let mut child = cmd
        .spawn()
        .map_err(|error| Error::Backend(format!("bwrap exec spawn failed: {error}")))?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel::<ExecResult>();
    let worker = std::thread::spawn(move || {
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let err_thread = std::thread::spawn(move || crate::worker::read_capped(stderr));
        let (out_text, out_trunc) = crate::worker::read_capped(stdout);
        let (err_text, err_trunc) = err_thread.join().unwrap_or_default();
        let duration_ms = crate::session::now_ms() - start;
        let exit_code = child.wait().ok().and_then(|status| status.code());
        let _ = tx.send(ExecResult {
            exit_code,
            stdout: out_text,
            stderr: err_text,
            duration_ms,
            truncated: out_trunc || err_trunc,
        });
    });
    match rx.recv_timeout(EXEC_TIMEOUT) {
        Ok(result) => {
            let _ = worker.join();
            Ok(result)
        }
        Err(_) => {
            // The sandboxed process tree dies with bwrap (`--die-with-parent`).
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            let _ = rx.recv();
            let _ = worker.join();
            Err(Error::Backend(format!(
                "bwrap exec timed out after {}s",
                EXEC_TIMEOUT.as_secs()
            )))
        }
    }
}

fn run_docker(mut cmd: Command, container_name: &str) -> Result<ExecResult> {
    let start = crate::session::now_ms();
    let mut child = cmd
        .spawn()
        .map_err(|error| Error::Backend(format!("docker exec spawn failed: {error}")))?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel::<ExecResult>();
    let worker = std::thread::spawn(move || {
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let err_thread = std::thread::spawn(move || crate::worker::read_capped(stderr));
        let (out_text, out_trunc) = crate::worker::read_capped(stdout);
        let (err_text, err_trunc) = err_thread.join().unwrap_or_default();
        let duration_ms = crate::session::now_ms() - start;
        let exit_code = child.wait().ok().and_then(|status| status.code());
        let _ = tx.send(ExecResult {
            exit_code,
            stdout: out_text,
            stderr: err_text,
            duration_ms,
            truncated: out_trunc || err_trunc,
        });
    });
    match rx.recv_timeout(EXEC_TIMEOUT) {
        Ok(result) => {
            let _ = worker.join();
            Ok(result)
        }
        Err(_) => {
            let _ = Command::new("docker")
                .args(["rm", "--force", container_name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if rx.recv_timeout(Duration::from_secs(5)).is_err() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            let _ = worker.join();
            Err(Error::Backend(format!(
                "docker exec timed out after {}s",
                EXEC_TIMEOUT.as_secs()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bwrap_runs_on_host() -> bool {
        match Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => child.wait().map(|status| status.success()).unwrap_or(false),
            Err(_) => false,
        }
    }

    #[test]
    fn available_reflects_host_bubblewrap_capability() {
        let isolation = BubblewrapIsolation::new("/tmp".into());
        assert_eq!(isolation.available(), bwrap_runs_on_host());
    }

    #[test]
    fn exec_writes_workspace_and_keeps_host_paths_read_only() {
        if !bwrap_runs_on_host() {
            eprintln!("bwrap unavailable; skipping");
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let isolation = BubblewrapIsolation::new(project.path().to_path_buf());
        let result = isolation
            .exec(
                vec![
                    "sh".into(),
                    "-c".into(),
                    "echo sandbox > /workspace/out.txt; pwd; \
                     touch /etc/deltabox-ro-test 2>/dev/null && exit 9; exit 0"
                        .into(),
                ],
                None,
                None,
            )
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "/workspace");
        assert_eq!(
            std::fs::read_to_string(project.path().join("out.txt")).unwrap(),
            "sandbox\n"
        );
    }

    #[test]
    fn exec_maps_cwd_into_workspace() {
        if !bwrap_runs_on_host() {
            eprintln!("bwrap unavailable; skipping");
            return;
        }
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("sub")).unwrap();
        let isolation = BubblewrapIsolation::new(project.path().to_path_buf());
        let result = isolation
            .exec(vec!["pwd".into()], Some(project.path().join("sub")), None)
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "/workspace/sub");
    }

    #[test]
    fn exec_rejects_cwd_outside_project() {
        if !bwrap_runs_on_host() {
            eprintln!("bwrap unavailable; skipping");
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let isolation = BubblewrapIsolation::new(project.path().to_path_buf());
        let error = isolation
            .exec(vec!["pwd".into()], Some(outside.path().to_path_buf()), None)
            .unwrap_err();
        assert!(error.to_string().contains("cwd outside project"));
    }

    #[test]
    fn docker_command_has_fixed_image_and_confinement() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("sub")).unwrap();
        let isolation = DockerIsolation::new(project.path().to_path_buf());
        let command = isolation
            .command(
                "deltabox-test",
                &["sh".into(), "-c".into(), "pwd".into()],
                Some(project.path().join("sub")),
                Some(vec![("EXPLICIT_VALUE".into(), "yes".into())]),
            )
            .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(command.get_program(), "docker");
        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--workdir", "/workspace/sub"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--env", "EXPLICIT_VALUE=yes"]));
        assert_eq!(args[args.len() - 4], DOCKER_IMAGE);
        assert_eq!(&args[args.len() - 3..], ["sh", "-c", "pwd"]);
    }

    #[test]
    fn docker_command_rejects_project_path_with_mount_delimiter() {
        let isolation = DockerIsolation::new("/tmp/project,unsafe".into());
        let error = isolation
            .command("deltabox-test", &["true".into()], None, None)
            .unwrap_err();
        assert!(error.to_string().contains("must not contain a comma"));
    }
}

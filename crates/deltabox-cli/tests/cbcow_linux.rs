#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn available() -> bool {
    deltafs_fuse::doctor::inspect().mount_ready
}

struct Session {
    project: PathBuf,
    id: String,
}

impl Session {
    fn start(project: &Path) -> Self {
        Self::start_with_policy(project, None)
    }

    fn start_with_policy(project: &Path, policy: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command.args([
            "session",
            "start",
            project.to_str().unwrap(),
            "--fs-mode",
            "cbcow-through",
            "--require-fs-mode",
        ]);
        if let Some(policy) = policy {
            command.arg("--policy").arg(policy);
        }
        let output = command.current_dir(project).output().unwrap();
        assert_ok(&output);
        Self {
            project: project.to_path_buf(),
            id: String::from_utf8(output.stdout).unwrap().trim().into(),
        }
    }

    fn command<const N: usize>(&self, args: [&str; N]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command.args(args).arg("--project").arg(&self.project);
        command.output().unwrap()
    }

    fn status(&self) -> Value {
        let output = self.command(["session", "status", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn write(&self, path: &str, content: &str) {
        let output = self.command(["write", &self.id, path, "--content", content]);
        assert_ok(&output);
    }

    fn checkpoint(&self) -> String {
        let output = self.command(["checkpoint", &self.id]);
        assert_ok(&output);
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["id"]
            .as_str()
            .unwrap()
            .into()
    }

    fn abort(&self, checkpoint: Option<&str>) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command
            .args(["abort", &self.id])
            .arg("--project")
            .arg(&self.project);
        if let Some(checkpoint) = checkpoint {
            command.args(["--checkpoint", checkpoint]);
        }
        let output = command.output().unwrap();
        assert_ok(&output);
    }

    fn stop(&self) -> Value {
        let output = self.command(["session", "stop", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn exec_shell(&self, script: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .args(["exec", &self.id, "--project"])
            .arg(&self.project)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap()
    }

    fn exec_shell_with_path(&self, script: &str, path: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .args(["exec", &self.id, "--project"])
            .arg(&self.project)
            .args(["--", "sh", "-c", script, "sh"])
            .arg(path)
            .output()
            .unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.command(["session", "stop", &self.id]);
    }
}

#[test]
fn changes_and_history_cross_cli_processes() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("README.md"), "base\n").unwrap();
    let session = Session::start(project.path());
    let status = session.status();
    assert_eq!(status["resolved_fs_mode"], "cbcow-through");
    assert_eq!(status["host_visibility"], "immediate");
    assert_eq!(status["mount_namespace"], false);

    session.write("created.txt", "created");
    assert_eq!(
        std::fs::read_to_string(project.path().join("created.txt")).unwrap(),
        "created"
    );
    let checkpoint = session.checkpoint();
    session.write("README.md", "changed\n");
    session.abort(Some(&checkpoint));
    assert_eq!(
        std::fs::read_to_string(project.path().join("README.md")).unwrap(),
        "base\n"
    );
    assert!(project.path().join("created.txt").exists());

    let diff = session.command(["diff", &session.id, "--json"]);
    assert_ok(&diff);
    let diff: Value = serde_json::from_slice(&diff.stdout).unwrap();
    assert_eq!(diff["pending_changes"], 1);
    assert_eq!(diff["changes"][0]["path"], "created.txt");

    let commit = session.command(["commit", &session.id]);
    assert_ok(&commit);
    session.write("README.md", "pending\n");
    session.abort(None);
    assert_eq!(
        std::fs::read_to_string(project.path().join("README.md")).unwrap(),
        "base\n"
    );
    assert!(project.path().join("created.txt").exists());
}

#[test]
fn helper_restart_preserves_pending_undo() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());
    session.write("pending.txt", "pending");
    let before = session.status();
    let pid = before["helper_pid"].as_u64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);

    let after = session.status();
    assert_ne!(after["helper_pid"], before["helper_pid"]);
    assert_eq!(after["helper_restarts"], 1);
    assert_eq!(after["pending_changes"], 1);
    session.abort(None);
    assert!(!project.path().join("pending.txt").exists());
}

#[test]
fn mapped_paths_route_to_mount_and_state_is_denied() {
    if !available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let alias = root.path().join("backing-alias");
    std::os::unix::fs::symlink(&project, &alias).unwrap();
    let policy = project.join("policy.toml");
    std::fs::write(
        &policy,
        "[filesystem]\nread = [\"/\"]\nwrite = [\".\"]\ndeny = []\n",
    )
    .unwrap();
    let session = Session::start_with_policy(&project, Some(&policy));
    let output = session.exec_shell_with_path(
        "set -e; printf routed > \"$1\"; printf sink >/dev/null",
        &project.join("mapped.txt"),
    );
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(
        std::fs::read_to_string(project.join("mapped.txt")).unwrap(),
        "routed"
    );
    session.abort(None);
    assert!(!project.join("mapped.txt").exists());

    let record: Value = serde_json::from_slice(
        &std::fs::read(
            project
                .join(".deltabox")
                .join(&session.id)
                .join("mount.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let state = record["state_dir"].as_str().unwrap();
    let state_root = Path::new(state).parent().unwrap();
    let script = format!("cat '{}/token' >/dev/null", state_root.display());
    let output = session.exec_shell(&script);
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(result["exit_code"], 0);

    let script = format!("printf escaped > '{}/escaped.txt'", alias.display());
    let output = session.exec_shell(&script);
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(result["exit_code"], 0);
    assert!(!project.join("escaped.txt").exists());

    let script = format!("cat '{}/policy.toml' >/dev/null", alias.display());
    let output = session.exec_shell(&script);
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(result["exit_code"], 0);
}

#[test]
fn existing_policy_denies_are_enforced_in_the_mounted_view() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join(".env"), "secret\n").unwrap();
    let session = Session::start(project.path());

    let output = session.exec_shell("cat .env >/dev/null");
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(result["exit_code"], 0);
    assert_eq!(
        std::fs::read_to_string(project.path().join(".env")).unwrap(),
        "secret\n"
    );
}

#[test]
fn text_diff_uses_accepted_and_live_content() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("note.txt"), "before\n").unwrap();
    let session = Session::start(project.path());
    session.write("note.txt", "after\n");

    let output = session.command(["diff", &session.id, "--json"]);
    assert_ok(&output);
    let diff: Value = serde_json::from_slice(&output.stdout).unwrap();
    let text = diff["changes"][0]["text_diff"].as_str().unwrap();
    assert!(text.contains("-before"));
    assert!(text.contains("+after"));
}

#[test]
fn atomic_rename_remains_visible_in_the_same_exec() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());
    let output = session.exec_shell(concat!(
        "python3 -c 'import os\n",
        "os.mkdir(\"build\")\n",
        "directory = os.open(\"build\", os.O_RDONLY | os.O_DIRECTORY)\n",
        "output = os.open(\"output.tmp\", os.O_WRONLY | os.O_CREAT, ",
        "0o644, dir_fd=directory)\n",
        "os.write(output, b\"ready\")\n",
        "os.close(output)\n",
        "os.rename(\"output.tmp\", \"output.d\", ",
        "src_dir_fd=directory, dst_dir_fd=directory)\n",
        "result = os.open(\"output.d\", os.O_RDONLY, dir_fd=directory)\n",
        "assert os.read(result, 5) == b\"ready\"\n",
        "os.close(result)\n",
        "os.close(directory)'"
    ));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "rename result: {result}");
    assert_eq!(
        std::fs::read(project.path().join("build/output.d")).unwrap(),
        b"ready"
    );
}

#[test]
fn git_commit_can_be_aborted_with_repository_state() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("tracked.txt"), "accepted\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "tracked.txt"],
        vec![
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "user.name=deltabox-test",
            "commit",
            "-qm",
            "accepted",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(project.path())
            .status()
            .unwrap()
            .success());
    }
    let accepted_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project.path())
        .output()
        .unwrap()
        .stdout;

    let session = Session::start(project.path());
    let output = session.exec_shell(concat!(
        "printf 'pending\\n' > tracked.txt; ",
        "git add tracked.txt; ",
        "git -c user.email=test@example.invalid -c user.name=deltabox-test ",
        "commit -qm pending"
    ));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "git result: {result}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "pending\n"
    );

    session.abort(None);
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "accepted\n"
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(head.stdout, accepted_head);
}

#[test]
fn supervisor_writes_reject_symlink_escape() {
    if !available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let session = Session::start(&project);
    let output = session.exec_shell(&format!("ln -s '{}' escape", outside.display()));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0);

    let output = session.command([
        "write",
        &session.id,
        "escape/file.txt",
        "--content",
        "escaped",
    ]);
    assert!(!output.status.success());
    assert!(!outside.join("file.txt").exists());
}

#[test]
fn stop_reverts_pending_host_visible_changes() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());
    session.write("pending.txt", "pending");

    let stopped = session.stop();
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["pending_changes"], 0);
    assert!(stopped["helper_pid"].is_null());
    assert!(!project.path().join("pending.txt").exists());
}

#[test]
fn helper_start_failure_falls_back_only_after_cleanup() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let state = project.path().join("state-inside-project");
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["session", "start", project.path().to_str().unwrap()])
        .env("DELTABOX_STATE_DIR", &state)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_ok(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("using copy-private"));
    let session_id = String::from_utf8(output.stdout).unwrap();
    let session_id = session_id.trim();
    let status = run(
        project.path(),
        [
            "session",
            "status",
            session_id,
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
    assert_ok(&status);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["requested_fs_mode"], "cbcow-through");
    assert_eq!(status["resolved_fs_mode"], "copy-private");
    assert!(std::fs::read_dir(&state).unwrap().next().is_none());

    let stopped = run(
        project.path(),
        [
            "session",
            "stop",
            session_id,
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
    assert_ok(&stopped);

    let required_project = tempfile::tempdir().unwrap();
    let required_state = required_project.path().join("state-inside-project");
    let required = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args([
            "session",
            "start",
            required_project.path().to_str().unwrap(),
            "--fs-mode",
            "cbcow-through",
            "--require-fs-mode",
        ])
        .env("DELTABOX_STATE_DIR", &required_state)
        .current_dir(required_project.path())
        .output()
        .unwrap();
    assert!(!required.status.success());
    assert!(!String::from_utf8_lossy(&required.stderr).contains("using copy-private"));
    assert!(std::fs::read_dir(&required_state).unwrap().next().is_none());
}

#[test]
fn one_through_session_holds_the_project_lease() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let _session = Session::start(project.path());
    let output = run(
        project.path(),
        [
            "session",
            "start",
            project.path().to_str().unwrap(),
            "--fs-mode",
            "cbcow-through",
            "--require-fs-mode",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session busy"));
}

fn run<const N: usize>(project: &Path, args: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

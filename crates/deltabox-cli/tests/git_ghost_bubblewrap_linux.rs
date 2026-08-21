#![cfg(target_os = "linux")]

//! Zero-FUSE git-backed sessions over the CLI and the stdio protocol.
//!
//! These tests never touch /dev/fuse or fusermount: `--fs-mode git` routes to
//! bubblewrap + git ghost storage. They skip gracefully on hosts where
//! bubblewrap cannot create user namespaces.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_deltabox")
}

fn bwrap_available() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .and_then(|mut child| child.wait().ok())
        .map(|status| status.success())
        .unwrap_or(false)
}

fn git(args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One git-mode session backed by an isolated `DELTABOX_STATE_DIR`.
struct Session {
    project: PathBuf,
    state_dir: PathBuf,
    id: String,
}

impl Session {
    fn start(project: &Path, state_dir: &Path) -> Self {
        let output = Command::new(bin())
            .env("DELTABOX_STATE_DIR", state_dir)
            .args([
                "session",
                "start",
                project.to_str().unwrap(),
                "--fs-mode",
                "git",
                "--isolation",
                "bubblewrap",
                "--json",
            ])
            .current_dir(project)
            .output()
            .unwrap();
        assert_ok(&output);
        let info: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(info["requested_fs_mode"], "git");
        assert_eq!(info["resolved_fs_mode"], "git");
        assert_eq!(info["isolation"], "bubblewrap");
        assert_eq!(info["state_provider"], "git-ghost");
        assert_eq!(info["rollback_guarantee"], "byte-for-byte-restore");
        assert_eq!(info["mountpoint"], Value::Null);
        assert_eq!(info["helper_pid"], Value::Null);
        Self {
            project: project.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            id: info["session_id"].as_str().unwrap().to_string(),
        }
    }

    fn command(&self, args: &[&str]) -> Output {
        let mut command = Command::new(bin());
        // Every subcommand defaults `--project` to `.` and the coordinator
        // canonicalizes it, so the project is selected via the working
        // directory. `--project` must never be spliced into the trailing
        // `-- <argv>` command.
        command
            .env("DELTABOX_STATE_DIR", &self.state_dir)
            .current_dir(&self.project)
            .args(args);
        command.output().unwrap()
    }

    fn exec(&self, script: &str) -> Value {
        let output = self.command(&["exec", &self.id, "--", "sh", "-c", script]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn diff(&self) -> Value {
        let output = self.command(&["diff", &self.id, "--json"]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn checkpoint(&self) -> String {
        let output = self.command(&["checkpoint", &self.id]);
        assert_ok(&output);
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn commit(&self) -> Value {
        let output = self.command(&["commit", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn abort(&self) -> Value {
        let output = self.command(&["abort", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn review_begin(&self, kind: &str, label: Option<&str>) -> Value {
        let mut args = vec!["review", "begin", &self.id, "--kind", kind];
        if let Some(label) = label {
            args.extend(["--label", label]);
        }
        let output = self.command(&args);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn review_status(&self) -> Value {
        let output = self.command(&["review", "status", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn review_diff(&self) -> Value {
        let output = self.command(&["review", "diff", &self.id, "--json"]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn review_commit(&self) -> Value {
        let output = self.command(&["review", "commit", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn review_abort(&self) -> Value {
        let output = self.command(&["review", "abort", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn review_skip(&self) -> Value {
        let output = self.command(&["review", "skip", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn status(&self) -> Value {
        let output = self.command(&["session", "status", &self.id]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn stop(&self) {
        let output = self.command(&["session", "stop", &self.id]);
        assert_ok(&output);
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn temp_state() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn git_mode_full_lifecycle_without_fuse() {
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("baseline.txt"), "base\n").unwrap();
    let state = temp_state();
    let session = Session::start(project.path(), state.path());

    // exec creates a file in the real project through bubblewrap.
    let exec =
        session.exec("printf 'created\\n' > created.txt; printf 'mutated\\n' > baseline.txt");
    assert_eq!(exec["exit_code"], 0);
    assert_eq!(
        std::fs::read_to_string(project.path().join("created.txt")).unwrap(),
        "created\n"
    );

    let diff = session.diff();
    assert_eq!(diff["pending_changes"], 2, "diff: {diff}");
    let by_path: std::collections::HashMap<&str, &Value> = diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| (change["path"].as_str().unwrap(), change))
        .collect();
    assert_eq!(by_path["created.txt"]["kind"], "added");
    assert_eq!(by_path["baseline.txt"]["kind"], "modified");

    // checkpoint pins a ghost ref and keeps the diff contract.
    let checkpoint_id = session.checkpoint();
    assert!(
        checkpoint_id.starts_with("refs/deltabox/undo/"),
        "{checkpoint_id}"
    );

    // abort rewinds the workspace byte-for-byte and clears review-created files.
    let aborted = session.abort();
    assert!(
        aborted["checkpoint_id"]
            .as_str()
            .unwrap()
            .starts_with("refs/deltabox/undo/"),
        "abort target: {aborted}"
    );
    assert_eq!(aborted["restored"], 2);
    assert!(!project.path().join("created.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("baseline.txt")).unwrap(),
        "base\n"
    );

    // exec deletes a baseline file; commit accepts it and the diff goes empty.
    let exec = session.exec("rm baseline.txt; printf 'new\\n' > new.txt");
    assert_eq!(exec["exit_code"], 0);
    let committed = session.commit();
    assert_eq!(committed["applied"], 2);
    assert_eq!(session.diff()["pending_changes"], 0);
    assert!(!project.path().join("baseline.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("new.txt")).unwrap(),
        "new\n"
    );

    let status = session.status();
    assert_eq!(status["status"], "running");
    assert_eq!(status["pending_changes"], 0);
    session.stop();
    assert_eq!(session.status()["status"], "stopped");
}

#[test]
fn git_mode_serializes_cli_operations_and_releases_lock_after_kill() {
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let state = temp_state();
    let session = Session::start(project.path(), state.path());
    let marker = project.path().join("exec-started");

    let mut exec = Command::new(bin())
        .env("DELTABOX_STATE_DIR", state.path())
        .current_dir(project.path())
        .args([
            "exec",
            &session.id,
            "--",
            "sh",
            "-c",
            "printf started > exec-started; sleep 30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "long-running exec did not start");

    let mut write = Command::new(bin())
        .env("DELTABOX_STATE_DIR", state.path())
        .current_dir(project.path())
        .args(["write", &session.id, "queued.txt", "--content", "queued"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        write.try_wait().unwrap().is_none(),
        "concurrent write must wait for the session lock"
    );

    exec.kill().unwrap();
    exec.wait().unwrap();
    let output = write.wait_with_output().unwrap();
    assert_ok(&output);
    assert_eq!(
        std::fs::read_to_string(project.path().join("queued.txt")).unwrap(),
        "queued"
    );
    session.stop();
}

#[test]
fn git_mode_review_scope_is_cumulative_and_clears_at_boundaries() {
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("baseline.txt"), "base\n").unwrap();
    let state = temp_state();
    let session = Session::start(project.path(), state.path());

    let first = session.review_begin("task", Some("task-start"));
    let checkpoint = first["checkpoint_id"].as_str().unwrap().to_string();
    assert_eq!(first["kind"], "task");
    assert_eq!(first["label"], "task-start");
    assert_eq!(session.review_status()["checkpoint_id"], checkpoint);

    session.exec("printf 'first\n' > first.txt");
    let skipped = session.review_skip();
    assert_eq!(skipped["checkpoint_id"], checkpoint);

    // A later adapter cannot replace the earliest cumulative rollback point.
    let second = session.review_begin("tool", None);
    assert_eq!(second["checkpoint_id"], checkpoint);
    assert_eq!(second["kind"], "task");
    session.exec("printf 'second\n' > second.txt");
    assert_eq!(session.review_diff()["pending_changes"], 2);

    let aborted = session.review_abort();
    assert_eq!(aborted["checkpoint_id"], checkpoint);
    assert_eq!(aborted["restored"], 2);
    assert!(!project.path().join("first.txt").exists());
    assert!(!project.path().join("second.txt").exists());
    assert!(session.review_status().is_null());

    let committed_scope = session.review_begin("risky", None);
    session.exec("printf 'accepted\n' > accepted.txt");
    let committed = session.review_commit();
    assert_eq!(committed["applied"], 1);
    assert_eq!(committed["checkpoint_id"], "accepted");
    assert!(session.review_status().is_null());
    assert_eq!(session.diff()["pending_changes"], 0);
    assert_eq!(
        std::fs::read_to_string(project.path().join("accepted.txt")).unwrap(),
        "accepted\n"
    );
    assert_ne!(
        committed_scope["checkpoint_id"], committed["checkpoint_id"],
        "review commit returns the accepted boundary, not the consumed savepoint"
    );

    // Raw boundaries have the same scope-closing behavior as review.commit/abort.
    session.review_begin("task", None);
    session.exec("printf 'raw-commit\n' > raw-commit.txt");
    session.commit();
    assert!(session.review_status().is_null());

    session.review_begin("task", None);
    session.exec("printf 'raw-abort\n' > raw-abort.txt");
    session.abort();
    assert!(session.review_status().is_null());
    assert!(!project.path().join("raw-abort.txt").exists());

    session.stop();
}

#[test]
fn git_mode_preserves_project_git_repo() {
    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    git(&["init", project.path().to_str().unwrap()]);
    git(&[
        "-C",
        project.path().to_str().unwrap(),
        "config",
        "user.email",
        "t@t",
    ]);
    git(&[
        "-C",
        project.path().to_str().unwrap(),
        "config",
        "user.name",
        "t",
    ]);
    std::fs::write(project.path().join("tracked.txt"), "tracked\n").unwrap();
    git(&["-C", project.path().to_str().unwrap(), "add", "tracked.txt"]);
    git(&[
        "-C",
        project.path().to_str().unwrap(),
        "commit",
        "-m",
        "baseline",
    ]);

    let head_before = git(&["-C", project.path().to_str().unwrap(), "rev-parse", "HEAD"]);
    let log_before = git(&[
        "-C",
        project.path().to_str().unwrap(),
        "log",
        "--all",
        "--oneline",
    ]);

    let state = temp_state();
    let session = Session::start(project.path(), state.path());
    let exec = session.exec("printf 'mutated\\n' > tracked.txt; printf 'untracked\\n' > new.txt");
    assert_eq!(exec["exit_code"], 0);
    assert_eq!(session.diff()["pending_changes"], 2);

    // Abort rewinds the workspace; the project repository is never touched.
    session.abort();
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "tracked\n"
    );
    assert!(!project.path().join("new.txt").exists());
    assert_eq!(
        git(&["-C", project.path().to_str().unwrap(), "rev-parse", "HEAD"]),
        head_before
    );
    assert_eq!(
        git(&[
            "-C",
            project.path().to_str().unwrap(),
            "log",
            "--all",
            "--oneline"
        ]),
        log_before
    );
    assert_eq!(
        git(&["-C", project.path().to_str().unwrap(), "status", "--short"]),
        ""
    );
    session.stop();
}

#[test]
fn git_mode_requires_explicit_isolation_flag() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(bin())
        .args([
            "session",
            "start",
            project.path().to_str().unwrap(),
            "--fs-mode",
            "git",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--isolation bubblewrap"),
        "missing guidance: {stderr}"
    );

    let output = Command::new(bin())
        .args([
            "session",
            "start",
            project.path().to_str().unwrap(),
            "--isolation",
            "bubblewrap",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires fs_mode git"),
        "missing isolation-without-git error: {stderr}"
    );
}

#[test]
fn serve_protocol_keeps_old_start_compatible_and_git_sessions_usable() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("base.txt"), "base\n").unwrap();
    let state = temp_state();
    let mut serve = Command::new(bin())
        .arg("serve")
        .env("DELTABOX_STATE_DIR", state.path())
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = serve.stdin.take().unwrap();
    let mut output = BufReader::new(serve.stdout.take().unwrap());
    let mut rpc = |request: Value| -> Value {
        writeln!(input, "{}", request).unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "serve exited before responding");
        serde_json::from_str(&line).unwrap()
    };

    // Legacy request without the new `isolation` field still starts a session.
    let legacy = rpc(serde_json::json!({
        "id": 1,
        "method": "session.start",
        "params": {"project": project.path()},
    }));
    assert!(
        legacy["error"].is_null(),
        "legacy session.start failed: {legacy}"
    );
    let legacy_sid = legacy["result"]["session_id"].as_str().unwrap();
    let legacy_stop = rpc(serde_json::json!({
        "id": 2,
        "method": "session.stop",
        "params": {"session_id": legacy_sid},
    }));
    assert!(legacy_stop["error"].is_null());

    if !bwrap_available() {
        eprintln!("bwrap unavailable; skipping git session");
        drop(input);
        let _ = serve.wait();
        return;
    }

    let started = rpc(serde_json::json!({
        "id": 3,
        "method": "session.start",
        "params": {
            "project": project.path(),
            "fs_mode": "git",
            "isolation": "bubblewrap",
        },
    }));
    assert!(started["error"].is_null(), "git start failed: {started}");
    let result = &started["result"];
    assert_eq!(result["resolved_fs_mode"], "git");
    assert_eq!(result["isolation"], "bubblewrap");
    assert_eq!(result["state_provider"], "git-ghost");
    let sid = result["session_id"].as_str().unwrap().to_string();

    // exec / diff.list / fs.read / fs.write keep their protocol shapes.
    let exec = rpc(serde_json::json!({
        "id": 4,
        "method": "session.exec",
        "params": {
            "session_id": sid,
            "argv": ["sh", "-c", "printf 'via exec\\n' > exec.txt; printf 'via write\\n' > write.txt"],
        },
    }));
    assert_eq!(exec["result"]["exit_code"], 0);
    assert!(exec["result"]["stdout"].is_string());
    assert!(exec["result"]["stderr"].is_string());
    assert!(exec["result"]["duration_ms"].is_u64());
    assert!(exec["result"]["truncated"].is_boolean());

    let write = rpc(serde_json::json!({
        "id": 5,
        "method": "fs.write",
        "params": {
            "session_id": sid,
            "path": project.path().join("fs.txt"),
            "content_base64": base64_encode("via fs.write\n"),
        },
    }));
    assert!(write["error"].is_null(), "fs.write failed: {write}");

    let read = rpc(serde_json::json!({
        "id": 6,
        "method": "fs.read",
        "params": {"session_id": sid, "path": project.path().join("fs.txt")},
    }));
    assert!(read["error"].is_null(), "fs.read failed: {read}");
    assert_eq!(read["result"]["size"], "via fs.write\n".len());
    assert_eq!(
        read["result"]["content_base64"],
        base64_encode("via fs.write\n")
    );

    let diff = rpc(serde_json::json!({
        "id": 7,
        "method": "diff.list",
        "params": {"session_id": sid},
    }));
    assert!(diff["error"].is_null(), "diff.list failed: {diff}");
    assert_eq!(diff["result"]["pending_changes"], 3);
    assert!(diff["result"]["changes"].as_array().unwrap().len() == 3);

    let committed = rpc(serde_json::json!({
        "id": 8,
        "method": "changes.commit",
        "params": {"session_id": sid},
    }));
    assert!(
        committed["error"].is_null(),
        "changes.commit failed: {committed}"
    );
    assert_eq!(committed["result"]["applied"], 3);
    assert!(committed["result"]["checkpoint_id"].is_string());

    let empty_diff = rpc(serde_json::json!({
        "id": 9,
        "method": "diff.list",
        "params": {"session_id": sid},
    }));
    assert_eq!(empty_diff["result"]["pending_changes"], 0);

    // changes.abort after commit restores nothing.
    let aborted = rpc(serde_json::json!({
        "id": 10,
        "method": "changes.abort",
        "params": {"session_id": sid},
    }));
    assert!(
        aborted["error"].is_null(),
        "changes.abort failed: {aborted}"
    );
    assert_eq!(aborted["result"]["restored"], 0);

    // review.* uses the same ghost snapshots and keeps the protocol shapes.
    let review = rpc(serde_json::json!({
        "id": 11,
        "method": "review.begin",
        "params": {"session_id": sid, "kind": "task"},
    }));
    assert!(review["error"].is_null(), "review.begin failed: {review}");
    let review_checkpoint = review["result"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();

    let review_write = rpc(serde_json::json!({
        "id": 12,
        "method": "fs.write",
        "params": {
            "session_id": sid,
            "path": project.path().join("review.txt"),
            "content_base64": base64_encode("review\n"),
        },
    }));
    assert!(
        review_write["error"].is_null(),
        "review write failed: {review_write}"
    );

    let review_diff = rpc(serde_json::json!({
        "id": 13,
        "method": "review.diff",
        "params": {"session_id": sid},
    }));
    assert!(
        review_diff["error"].is_null(),
        "review.diff failed: {review_diff}"
    );
    assert_eq!(review_diff["result"]["pending_changes"], 1);

    let review_status = rpc(serde_json::json!({
        "id": 14,
        "method": "review.status",
        "params": {"session_id": sid},
    }));
    assert_eq!(review_status["result"]["checkpoint_id"], review_checkpoint);

    let review_abort = rpc(serde_json::json!({
        "id": 15,
        "method": "review.abort",
        "params": {"session_id": sid},
    }));
    assert!(
        review_abort["error"].is_null(),
        "review.abort failed: {review_abort}"
    );
    assert_eq!(review_abort["result"]["restored"], 1);
    assert!(!project.path().join("review.txt").exists());

    let review_status = rpc(serde_json::json!({
        "id": 16,
        "method": "review.status",
        "params": {"session_id": sid},
    }));
    assert!(review_status["result"].is_null());

    let stop = rpc(serde_json::json!({
        "id": 17,
        "method": "session.stop",
        "params": {"session_id": sid},
    }));
    assert!(stop["error"].is_null(), "git stop failed: {stop}");
    assert_eq!(stop["result"]["status"], "stopped");

    drop(input);
    let _ = serve.wait();
}

fn base64_encode(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}

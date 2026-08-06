#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_deltabox")
}

fn run(project: &Path, args: &[&str]) -> Output {
    let output = Command::new(bin()).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {:?}\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = project;
    output
}

fn start(project: &Path) -> String {
    run(project, &["session", "start", project.to_str().unwrap()])
        .stdout
        .split(|b| *b == b'\n')
        .next()
        .map(|s| String::from_utf8_lossy(s).trim().to_string())
        .unwrap()
}

fn write(project: &Path, sid: &str, path: &str, content: &str) {
    run(
        project,
        &[
            "write",
            sid,
            "--project",
            project.to_str().unwrap(),
            path,
            "--content",
            content,
        ],
    );
}

fn exec(project: &Path, sid: &str, script: &str) -> Value {
    serde_json::from_slice(
        &run(
            project,
            &[
                "exec",
                sid,
                "--project",
                project.to_str().unwrap(),
                "--",
                "sh",
                "-c",
                script,
            ],
        )
        .stdout,
    )
    .unwrap()
}

fn diff(project: &Path, sid: &str, from: Option<&str>) -> Value {
    let mut args = vec![
        "diff",
        sid,
        "--project",
        project.to_str().unwrap(),
        "--json",
    ];
    if let Some(from) = from {
        args.extend(["--from", from]);
    }
    serde_json::from_slice(&run(project, &args).stdout).unwrap()
}

#[test]
fn separate_processes_preserve_pending_state_and_default_abort_contract() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("README.md"), "accepted\n").unwrap();
    let sid = start(project.path());

    write(project.path(), &sid, "a.txt", "pending");
    exec(project.path(), &sid, "rm README.md");

    assert!(!project.path().join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("README.md")).unwrap(),
        "accepted\n"
    );
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 2);

    run(
        project.path(),
        &["abort", &sid, "--project", project.path().to_str().unwrap()],
    );
    let after = diff(project.path(), &sid, None);
    assert_eq!(after["pending_changes"], 0);
    assert!(!project.path().join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("README.md")).unwrap(),
        "accepted\n"
    );
}

#[test]
fn task_savepoint_aborts_multiple_tools_not_only_the_last_tool() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("README.md"), "accepted\n").unwrap();
    let sid = start(project.path());
    let checkpoint: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "checkpoint",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
                "--label",
                "task-start",
            ],
        )
        .stdout,
    )
    .unwrap();
    let checkpoint = checkpoint["id"].as_str().unwrap();

    write(project.path(), &sid, "first.txt", "one");
    write(project.path(), &sid, "second.txt", "two");
    exec(project.path(), &sid, "rm README.md");
    run(
        project.path(),
        &[
            "abort",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--checkpoint",
            checkpoint,
        ],
    );

    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);
    assert!(!project.path().join("first.txt").exists());
    assert!(!project.path().join("second.txt").exists());
    assert!(project.path().join("README.md").exists());
}

#[test]
fn commit_needs_no_checkpoint_and_historical_diff_keeps_immutable_bytes() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "v1\n").unwrap();
    let sid = start(project.path());
    let checkpoint: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "checkpoint",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
                "--label",
                "v1",
            ],
        )
        .stdout,
    )
    .unwrap();
    let checkpoint = checkpoint["id"].as_str().unwrap();

    write(project.path(), &sid, "a.txt", "v2\n");
    run(
        project.path(),
        &[
            "commit",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("a.txt")).unwrap(),
        "v2\n"
    );

    write(project.path(), &sid, "a.txt", "v3\n");
    let historical = diff(project.path(), &sid, Some(checkpoint));
    let text = historical["changes"][0]["text_diff"].as_str().unwrap();
    assert!(
        text.contains("-v1"),
        "historical diff must retain v1: {text}"
    );
    assert!(text.contains("+v3"), "historical diff must show v3: {text}");
}

#[test]
fn commit_rejects_external_host_modification() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "v1\n").unwrap();
    let sid = start(project.path());
    write(project.path(), &sid, "pending.txt", "pending");
    std::fs::write(project.path().join("a.txt"), "external change\n").unwrap();

    let output = Command::new(bin())
        .args([
            "commit",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("project changed outside deltabox"));
    assert!(!project.path().join("pending.txt").exists());
}

#[test]
fn live_lock_rejects_concurrency_and_stale_lock_recovers() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let lock = project
        .path()
        .join(".deltabox")
        .join(&sid)
        .join("session.lock");

    std::fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
    let busy = Command::new(bin())
        .args([
            "write",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "busy.txt",
            "--content",
            "blocked",
        ])
        .output()
        .unwrap();
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("session busy"));

    std::fs::write(&lock, "4294967294\n").unwrap();
    write(project.path(), &sid, "recovered.txt", "ok");
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 1);
}

#[test]
fn killed_cli_does_not_poison_session_or_publish_partial_work() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let mut child = Command::new(bin())
        .args([
            "exec",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "sleep 1; printf leaked > killed.txt",
        ])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(1100));

    write(project.path(), &sid, "after-kill.txt", "healthy");
    let pending = diff(project.path(), &sid, None);
    assert_eq!(pending["pending_changes"], 1);
    assert_eq!(pending["changes"][0]["path"], "after-kill.txt");
    assert!(!project.path().join("killed.txt").exists());
}

#[test]
fn per_session_metadata_survives_missing_global_registry() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    std::fs::remove_file(project.path().join(".deltabox/sessions.json")).unwrap();

    write(project.path(), &sid, "still-open.txt", "durable");
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 1);
}

#[test]
fn commit_does_not_publish_or_retain_shadow_git_metadata() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join(".git")).unwrap();
    std::fs::write(project.path().join(".git/marker"), "host\n").unwrap();
    let sid = start(project.path());

    exec(
        project.path(),
        &sid,
        "printf shadow > .git/marker; printf pending > source.txt",
    );
    run(
        project.path(),
        &[
            "commit",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ],
    );

    assert_eq!(
        std::fs::read_to_string(project.path().join(".git/marker")).unwrap(),
        "host\n"
    );
    assert_eq!(
        std::fs::read_to_string(
            project
                .path()
                .join(".deltabox")
                .join(&sid)
                .join("working/.git/marker")
        )
        .unwrap(),
        "host\n"
    );
}

#[test]
fn skipped_review_preserves_earliest_scope_and_aborts_all_tools() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let first: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "review",
                "begin",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
                "--kind",
                "task",
                "--label",
                "turn-start",
            ],
        )
        .stdout,
    )
    .unwrap();
    write(project.path(), &sid, "first.txt", "one");
    run(
        project.path(),
        &[
            "review",
            "skip",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ],
    );

    let second: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "review",
                "begin",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
                "--kind",
                "tool",
            ],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(first["checkpoint_id"], second["checkpoint_id"]);

    write(project.path(), &sid, "second.txt", "two");
    let review_diff: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "review",
                "diff",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
                "--json",
            ],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(review_diff["pending_changes"], 2);

    run(
        project.path(),
        &[
            "review",
            "abort",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);
    let status: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "review",
                "status",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
            ],
        )
        .stdout,
    )
    .unwrap();
    assert!(status.is_null());
}

#[test]
fn review_and_raw_boundaries_clear_active_scope_across_processes() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    for boundary in ["review", "raw"] {
        run(
            project.path(),
            &[
                "review",
                "begin",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
                "--kind",
                "task",
            ],
        );
        write(project.path(), &sid, &format!("{boundary}.txt"), boundary);
        if boundary == "review" {
            run(
                project.path(),
                &[
                    "review",
                    "commit",
                    &sid,
                    "--project",
                    project.path().to_str().unwrap(),
                ],
            );
        } else {
            run(
                project.path(),
                &[
                    "commit",
                    &sid,
                    "--project",
                    project.path().to_str().unwrap(),
                ],
            );
        }
        let status: Value = serde_json::from_slice(
            &run(
                project.path(),
                &[
                    "review",
                    "status",
                    &sid,
                    "--project",
                    project.path().to_str().unwrap(),
                ],
            )
            .stdout,
        )
        .unwrap();
        assert!(status.is_null(), "{boundary} commit retained review scope");
    }

    run(
        project.path(),
        &[
            "review",
            "begin",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--kind",
            "task",
        ],
    );
    write(project.path(), &sid, "aborted.txt", "pending");
    run(
        project.path(),
        &["abort", &sid, "--project", project.path().to_str().unwrap()],
    );
    let status: Value = serde_json::from_slice(
        &run(
            project.path(),
            &[
                "review",
                "status",
                &sid,
                "--project",
                project.path().to_str().unwrap(),
            ],
        )
        .stdout,
    )
    .unwrap();
    assert!(status.is_null(), "raw abort retained review scope");
}

#[test]
fn crash_during_working_swap_restores_previous_tree() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let output = Command::new(bin())
        .env("DELTABOX_TEST_CRASH_AT", "working_moved")
        .args([
            "write",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "partial.txt",
            "--content",
            "must-not-survive",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());

    let recovered = diff(project.path(), &sid, None);
    assert_eq!(
        recovered["pending_changes"], 0,
        "recovered diff: {recovered}"
    );
    assert!(!project
        .path()
        .join(".deltabox")
        .join(&sid)
        .join("working/partial.txt")
        .exists());
}

#[test]
fn crash_during_host_commit_rolls_back_host_and_keeps_pending_work() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("base.txt"), "base").unwrap();
    let sid = start(project.path());
    write(project.path(), &sid, "pending.txt", "pending");
    let output = Command::new(bin())
        .env("DELTABOX_TEST_CRASH_AT", "commit_host_applied")
        .args([
            "commit",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());

    let recovered = diff(project.path(), &sid, None);
    assert_eq!(recovered["pending_changes"], 1);
    assert!(!project.path().join("pending.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("base.txt")).unwrap(),
        "base"
    );
}

#[test]
fn crash_during_incremental_materialize_restores_every_changed_path() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "a-old").unwrap();
    std::fs::write(project.path().join("b.txt"), "b-old").unwrap();
    let sid = start(project.path());
    let output = Command::new(bin())
        .env("DELTABOX_TEST_CRASH_AT", "materialize_after_copy")
        .args([
            "exec",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "printf a-new > a.txt; rm b.txt; printf added > c.txt",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());

    let recovered = diff(project.path(), &sid, None);
    assert_eq!(
        recovered["pending_changes"], 0,
        "recovered diff: {recovered}"
    );
    let working = project.path().join(".deltabox").join(&sid).join("working");
    assert_eq!(
        std::fs::read_to_string(working.join("a.txt")).unwrap(),
        "a-old"
    );
    assert_eq!(
        std::fs::read_to_string(working.join("b.txt")).unwrap(),
        "b-old"
    );
    assert!(!working.join("c.txt").exists());
}

#[test]
fn exec_does_not_inherit_supervisor_secrets_but_accepts_explicit_env() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let output = Command::new(bin())
        .env("DELTABOX_PARENT_SECRET", "must-not-leak")
        .args([
            "exec",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--env",
            "EXPLICIT_VALUE=visible",
            "--",
            "sh",
            "-c",
            "test -z \"$DELTABOX_PARENT_SECRET\" && test \"$EXPLICIT_VALUE\" = visible",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0);
}

#[test]
fn representative_cli_tools_and_dev_null_work_inside_sandbox() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("tracked.txt"), "host").unwrap();
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
            "initial",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(project.path())
            .status()
            .unwrap()
            .success());
    }
    let sid = start(project.path());
    let cases = [
        ("dev-null", "printf sink-test > /dev/null"),
        (
            "git",
            concat!(
                "git status --short > /dev/null; ",
                "printf sandbox > tracked.txt; git add tracked.txt; ",
                "git -c user.email=test@example.invalid -c user.name=deltabox-test commit -qm sandbox"
            ),
        ),
        (
            "python",
            "python3 -c 'open(\"python.txt\", \"w\").write(\"ok\")'",
        ),
        (
            "node",
            "node -e 'require(\"fs\").writeFileSync(\"node.txt\", \"ok\")'",
        ),
        ("cargo", "cargo --version > /dev/null"),
        ("npm", "npm --version > /dev/null"),
    ];
    for (name, script) in cases {
        let result = exec(project.path(), &sid, script);
        assert_eq!(
            result["exit_code"], 0,
            "{name} compatibility failed: {result}"
        );
    }
    let pending = diff(project.path(), &sid, None);
    assert!(pending["pending_changes"].as_u64().unwrap() >= 3);
}

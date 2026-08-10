#![cfg(any(target_os = "linux", target_os = "macos"))]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    run(
        project,
        &[
            "session",
            "start",
            project.to_str().unwrap(),
            "--fs-mode",
            "copy-private",
            "--require-fs-mode",
        ],
    )
    .stdout
    .split(|b| *b == b'\n')
    .next()
    .map(|s| String::from_utf8_lossy(s).trim().to_string())
    .unwrap()
}

#[test]
fn json_start_reports_the_pinned_filesystem_contract() {
    let project = tempfile::tempdir().unwrap();
    let output = run(
        project.path(),
        &[
            "session",
            "start",
            project.path().to_str().unwrap(),
            "--json",
            "--fs-mode",
            "copy-private",
            "--require-fs-mode",
        ],
    );
    let info: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["requested_fs_mode"], "copy-private");
    assert_eq!(info["resolved_fs_mode"], "copy-private");
    assert_eq!(info["host_visibility"], "on-commit");
    assert_eq!(info["durability"], "process-crash");
    assert!(output.stderr.is_empty());

    run(
        project.path(),
        &[
            "session",
            "stop",
            info["session_id"].as_str().unwrap(),
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
}

fn start_with_policy(project: &Path, policy: &str) -> String {
    let policy_path = project.join("policy.toml");
    let policy = format!(
        concat!(
            "[filesystem]\n",
            "read = [\".\", \"/usr\", \"/bin\", \"/lib\", \"/lib64\", \"/etc\", \"/run\"]\n",
            "write = [\".\"]\n",
            "deny = [\".env\", \".env.*\", \"*.pem\", \"*.key\"]\n\n",
            "{}"
        ),
        policy
    );
    std::fs::write(&policy_path, policy).unwrap();
    run(
        project,
        &[
            "session",
            "start",
            project.to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
            "--fs-mode",
            "copy-private",
            "--require-fs-mode",
        ],
    )
    .stdout
    .split(|b| *b == b'\n')
    .next()
    .map(|s| String::from_utf8_lossy(s).trim().to_string())
    .unwrap()
}

fn raw_exec(project: &Path, sid: &str, script: &str) -> Output {
    Command::new(bin())
        .args([
            "exec",
            sid,
            "--project",
            project.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap()
}

fn tcp_server() -> (u16, std::thread::JoinHandle<bool>) {
    tcp_server_for(Duration::from_secs(30))
}

fn tcp_server_for(timeout: Duration) -> (u16, std::thread::JoinHandle<bool>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        }
        false
    });
    (port, thread)
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
fn separate_processes_preserve_pending_state_and_default_abort_behavior() {
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
fn separate_execs_keep_a_stable_working_path() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());

    let first = exec(project.path(), &sid, "pwd > recorded-working-path.txt");
    assert_eq!(first["exit_code"], 0, "first exec: {first}");
    let second = exec(
        project.path(),
        &sid,
        "test \"$(cat recorded-working-path.txt)\" = \"$PWD\"",
    );
    assert_eq!(second["exit_code"], 0, "second exec: {second}");
    assert!(!project.path().join("recorded-working-path.txt").exists());
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
fn crash_after_abort_swap_reconciles_journal_and_review_scope() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
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
    write(project.path(), &sid, "pending.txt", "pending");

    let crashed = Command::new(bin())
        .env("DELTABOX_TEST_CRASH_AT", "working_swapped")
        .args([
            "review",
            "abort",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!crashed.status.success());

    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);
    let review: Value = serde_json::from_slice(
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
    assert!(review.is_null());

    let journal = std::fs::read_to_string(
        project
            .path()
            .join(".deltabox")
            .join(&sid)
            .join("journal.jsonl"),
    )
    .unwrap();
    let entries: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(entries
        .iter()
        .any(|entry| { entry["kind"] == "abort" && entry["data"]["transaction_id"].is_string() }));
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
fn crash_during_exec_candidate_swap_restores_every_changed_path() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "a-old").unwrap();
    std::fs::write(project.path().join("b.txt"), "b-old").unwrap();
    let sid = start(project.path());
    let output = Command::new(bin())
        .env("DELTABOX_TEST_CRASH_AT", "exec_candidate_complete")
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
    let mut cases = vec![("dev-null", "printf sink-test > /dev/null")];
    #[cfg(target_os = "linux")]
    cases.push((
        "dev-full",
        concat!(
            "python3 -c 'import errno, os\n",
            "fd = os.open(\"/dev/full\", os.O_WRONLY)\n",
            "try:\n",
            "    os.write(fd, b\"x\")\n",
            "except OSError as error:\n",
            "    raise SystemExit(0 if error.errno == errno.ENOSPC else 1)\n",
            "raise SystemExit(1)'"
        ),
    ));
    cases.extend([
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
    ]);
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

#[test]
#[cfg(target_os = "linux")]
fn openat2_reads_a_file_created_in_the_same_exec() {
    let project = tempfile::tempdir().unwrap();
    let sid = start_with_policy(project.path(), "[network]\nmode = \"deny_all\"\n");
    let script = format!(
        concat!(
            "python3 -c 'import ctypes, os\n",
            "class OpenHow(ctypes.Structure):\n",
            "    _fields_ = [(\"flags\", ctypes.c_ulonglong), ",
            "(\"mode\", ctypes.c_ulonglong), (\"resolve\", ctypes.c_ulonglong)]\n",
            "open(\"openat2.txt\", \"wb\").write(b\"ok\")\n",
            "path = ctypes.create_string_buffer(b\"openat2.txt\")\n",
            "how = OpenHow(os.O_RDONLY, 0, 0)\n",
            "fd = ctypes.CDLL(None, use_errno=True).syscall(",
            "{}, -100, path, ctypes.byref(how), ctypes.sizeof(how))\n",
            "assert fd >= 0, ctypes.get_errno()\n",
            "assert os.read(fd, 2) == b\"ok\"\n",
            "os.close(fd)'"
        ),
        libc::SYS_openat2,
    );
    let result = exec(project.path(), &sid, &script);
    assert_eq!(result["exit_code"], 0, "openat2 result: {result}");
}

#[test]
fn renameat_keeps_an_atomic_output_visible_in_the_same_exec() {
    let project = tempfile::tempdir().unwrap();
    let sid = start_with_policy(project.path(), "[network]\nmode = \"deny_all\"\n");
    let script = concat!(
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
    );
    let result = exec(project.path(), &sid, script);
    assert_eq!(result["exit_code"], 0, "renameat result: {result}");
}

#[test]
fn access_sees_a_file_created_in_the_same_exec() {
    let project = tempfile::tempdir().unwrap();
    let sid = start_with_policy(project.path(), "[network]\nmode = \"deny_all\"\n");
    let result = exec(
        project.path(),
        &sid,
        concat!(
            "python3 -c 'open(\"access.txt\", \"w\").write(\"visible\")'; ",
            "test -r access.txt; test -f access.txt; cat access.txt"
        ),
    );
    assert_eq!(result["exit_code"], 0, "access result: {result}");
    assert_eq!(result["stdout"], "visible");
}

#[test]
fn network_modes_enforce_allow_audit_deny_all_and_allowlist() {
    let allow_project = tempfile::tempdir().unwrap();
    let allow_sid = start_with_policy(allow_project.path(), "[network]\nmode = \"allow_audit\"\n");
    let (allow_port, allow_thread) = tcp_server();
    let allow = exec(
        allow_project.path(),
        &allow_sid,
        &format!(
            "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {allow_port}), 1)'"
        ),
    );
    assert_eq!(allow["exit_code"], 0, "allow_audit result: {allow}");
    assert!(allow_thread.join().unwrap());

    let deny_project = tempfile::tempdir().unwrap();
    let deny_sid = start_with_policy(deny_project.path(), "[network]\nmode = \"deny_all\"\n");
    let deny = exec(
        deny_project.path(),
        &deny_sid,
        "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", 9), 1)'",
    );
    assert_ne!(
        deny["exit_code"], 0,
        "deny_all unexpectedly connected: {deny}"
    );

    let list_project = tempfile::tempdir().unwrap();
    let (listed_port, listed_thread) = tcp_server();
    let list_sid = start_with_policy(
        list_project.path(),
        &format!("[network]\nmode = \"allowlist\"\nallow = [\"127.0.0.1:{listed_port}\"]\n"),
    );
    let listed = exec(
        list_project.path(),
        &list_sid,
        &format!(
            "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {listed_port}), 1)'"
        ),
    );
    assert_eq!(listed["exit_code"], 0, "allowlist result: {listed}");
    assert!(listed_thread.join().unwrap());

    let (unlisted_port, unlisted_thread) = tcp_server_for(Duration::from_secs(1));
    let unlisted = exec(
        list_project.path(),
        &list_sid,
        &format!(
            "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {unlisted_port}), 1)'"
        ),
    );
    assert_ne!(
        unlisted["exit_code"], 0,
        "allowlist unexpectedly allowed an unlisted target: {unlisted}"
    );
    assert!(!unlisted_thread.join().unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_maps_path_arguments_and_denies_hardcoded_host_access() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.txt"), "visible").unwrap();
    std::fs::create_dir(project.path().join("config")).unwrap();
    std::fs::write(project.path().join("config/.env"), "SECRET=hidden").unwrap();
    std::os::unix::fs::symlink("/bin/sh", project.path().join("existing-tool-link")).unwrap();
    let sid = start(project.path());

    let mapped = Command::new(bin())
        .args([
            "exec",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--",
            "/bin/cat",
            project.path().join("hello.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(mapped.status.success());
    let mapped: Value = serde_json::from_slice(&mapped.stdout).unwrap();
    assert_eq!(mapped["exit_code"], 0);
    assert_eq!(mapped["stdout"], "visible");
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);

    let relative_secret = exec(project.path(), &sid, "cat config/.env");
    assert_ne!(relative_secret["exit_code"], 0);

    let real_project = project.path().canonicalize().unwrap();
    let absolute_secret = exec(
        project.path(),
        &sid,
        &format!("cat {}/config/.env", real_project.display()),
    );
    assert_ne!(absolute_secret["exit_code"], 0);
    assert!(!absolute_secret["stdout"]
        .as_str()
        .unwrap_or_default()
        .contains("SECRET"));

    let host_write = exec(
        project.path(),
        &sid,
        &format!("printf changed > {}/hello.txt", real_project.display()),
    );
    assert_ne!(host_write["exit_code"], 0);
    assert_eq!(
        std::fs::read_to_string(project.path().join("hello.txt")).unwrap(),
        "visible"
    );

    let unsafe_link = raw_exec(project.path(), &sid, "ln -s /etc/hosts new-leak");
    assert!(!unsafe_link.status.success());
    assert!(String::from_utf8_lossy(&unsafe_link.stderr).contains("absolute symlink"));
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);
}

#[test]
fn newly_created_denied_file_is_rejected_without_pending_state() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let output = raw_exec(project.path(), &sid, "printf secret > .env");
    if cfg!(target_os = "linux") {
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("path denied by policy"));
    } else {
        assert!(output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(result["exit_code"], 0);
        assert!(result["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("not permitted"));
    }
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);
    let working = project.path().join(".deltabox").join(&sid).join("working");
    assert!(!working.join(".env").exists());
}

#[test]
fn supervisor_read_and_diff_reject_external_symlinks() {
    let project = tempfile::tempdir().unwrap();
    let sid = start(project.path());
    let store = project.path().join(".deltabox").join(&sid);
    std::os::unix::fs::symlink("/etc/hosts", store.join("working/leak")).unwrap();
    std::fs::remove_file(store.join("working_scan.json")).unwrap();

    let read = Command::new(bin())
        .args([
            "read",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "leak",
        ])
        .output()
        .unwrap();
    assert!(!read.status.success());
    assert!(String::from_utf8_lossy(&read.stderr).contains("symlink"));
    assert!(
        read.stdout.is_empty(),
        "external bytes leaked through fs.read"
    );

    let diff = Command::new(bin())
        .args([
            "diff",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!diff.status.success());
    assert!(String::from_utf8_lossy(&diff.stderr).contains("symlink escapes project"));
    assert!(diff.stdout.is_empty(), "external bytes leaked through diff");
}

#[test]
fn direct_write_and_exec_storage_limits_are_enforced_atomically() {
    let project = tempfile::tempdir().unwrap();
    let sid = start_with_policy(
        project.path(),
        "[limits]\nmax_write_bytes = 4\nmax_storage_bytes = 8\n",
    );
    let write = Command::new(bin())
        .args([
            "write",
            &sid,
            "--project",
            project.path().to_str().unwrap(),
            "too-big.txt",
            "--content",
            "12345",
        ])
        .output()
        .unwrap();
    assert!(!write.status.success());
    assert!(String::from_utf8_lossy(&write.stderr).contains("max_write_bytes"));

    let exec = raw_exec(project.path(), &sid, "printf 123456789 > cow-big.txt");
    assert!(!exec.status.success());
    assert!(String::from_utf8_lossy(&exec.stderr).contains("max_storage_bytes"));
    assert_eq!(diff(project.path(), &sid, None)["pending_changes"], 0);
}

#[test]
fn optional_serve_recovers_after_worker_failure() {
    let project = tempfile::tempdir().unwrap();
    let mut serve = Command::new(bin())
        .arg("serve")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = serve.stdin.take().unwrap();
    let mut output = BufReader::new(serve.stdout.take().unwrap());
    {
        let mut rpc = |request: Value| -> Value {
            writeln!(input, "{}", request).unwrap();
            input.flush().unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert!(!line.is_empty(), "serve exited before responding");
            serde_json::from_str(&line).unwrap()
        };

        let started = rpc(serde_json::json!({
            "id": 1,
            "method": "session.start",
            "params": {"project": project.path()},
        }));
        let sid = started["result"]["session_id"].as_str().unwrap();
        let failed = rpc(serde_json::json!({
            "id": 2,
            "method": "session.exec",
            "params": {
                "session_id": sid,
                "argv": ["sh", "-c", if cfg!(target_os = "linux") { "kill -KILL $PPID" } else { "kill -KILL $$" }],
            },
        }));
        if cfg!(target_os = "linux") {
            assert!(
                failed["error"].is_object(),
                "worker failure was hidden: {failed}"
            );
        } else {
            assert!(
                failed["result"]["exit_code"].is_null(),
                "signalled command was hidden: {failed}"
            );
        }

        let recovered = rpc(serde_json::json!({
            "id": 3,
            "method": "session.exec",
            "params": {
                "session_id": sid,
                "argv": ["sh", "-c", "printf recovered > recovered.txt"],
            },
        }));
        assert_eq!(
            recovered["result"]["exit_code"], 0,
            "serve did not recover: {recovered}"
        );
        let pending = rpc(serde_json::json!({
            "id": 4,
            "method": "diff.list",
            "params": {"session_id": sid},
        }));
        assert_eq!(pending["result"]["pending_changes"], 1);
    }
    drop(input);
    assert!(serve.wait().unwrap().success());
}

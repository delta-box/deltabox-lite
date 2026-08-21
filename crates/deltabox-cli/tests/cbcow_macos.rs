#![cfg(target_os = "macos")]

use serde_json::Value;
use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_deltabox")
}

struct Session {
    project: tempfile::TempDir,
    state: tempfile::TempDir,
    id: String,
}

impl Session {
    fn start(mode: &str) -> Self {
        Self::start_with(mode, false)
    }

    fn start_with(mode: &str, durable: bool) -> Self {
        Self::start_with_policy(mode, durable, None)
    }

    fn start_with_policy(mode: &str, durable: bool, policy: Option<&str>) -> Self {
        let project = tempfile::Builder::new()
            .prefix("deltabox-macos-project-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let state = tempfile::Builder::new()
            .prefix("deltabox-macos-state-")
            .tempdir_in("/private/tmp")
            .unwrap();
        std::fs::write(project.path().join("a.txt"), "accepted").unwrap();
        let policy_path = policy.map(|policy| {
            let path = project.path().join("policy.toml");
            std::fs::write(&path, policy).unwrap();
            path
        });
        let mut args = vec![
            "session",
            "start",
            project.path().to_str().unwrap(),
            "--fs-mode",
            mode,
            "--require-fs-mode",
        ];
        if durable {
            args.push("--durable");
        }
        if let Some(path) = &policy_path {
            args.push("--policy");
            args.push(path.to_str().unwrap());
        }
        let output = command(project.path(), state.path(), args);
        assert_ok(&output);
        let id = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap()
            .to_string();
        Self { project, state, id }
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Output {
        command(self.project.path(), self.state.path(), args)
    }

    fn exec(&self, script: &str) -> Value {
        let output = self.run([
            "exec",
            &self.id,
            "--project",
            self.project.path().to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            script,
        ]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn status(&self) -> Value {
        let output = self.run([
            "session",
            "status",
            &self.id,
            "--project",
            self.project.path().to_str().unwrap(),
        ]);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn commit(&self) -> Output {
        self.run([
            "commit",
            &self.id,
            "--project",
            self.project.path().to_str().unwrap(),
        ])
    }

    fn stop(&self) {
        let output = self.run([
            "session",
            "stop",
            &self.id,
            "--project",
            self.project.path().to_str().unwrap(),
        ]);
        assert_ok(&output);
    }

    fn command_runtime(&self) -> PathBuf {
        let uid = std::fs::metadata(self.project.path()).unwrap().uid();
        Path::new("/private/tmp")
            .join(format!("deltabox-{uid}"))
            .join(&self.id)
    }
}

#[test]
fn fast_exec_defers_aggregate_quota_to_the_next_state_boundary() {
    let session = Session::start_with_policy(
        "cbcow-through",
        false,
        Some(
            r#"[filesystem]
read = ["."]
write = ["."]
deny = []

[network]
mode = "allow_audit"

[limits]
max_write_bytes = 1024
max_storage_bytes = 4
"#,
        ),
    );
    assert_eq!(session.exec("printf oversized > new.txt")["exit_code"], 0);
    let status = session.run([
        "session",
        "status",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
    ]);
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("exceeding max_storage_bytes"));
    let abort = session.run([
        "abort",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
    ]);
    assert_ok(&abort);
    assert_eq!(session.status()["pending_changes"], 0);
    session.stop();
}

fn command<I, S>(project: &Path, state: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(bin())
        .env("DELTABOX_STATE_DIR", state)
        .current_dir(project)
        .args(args)
        .output()
        .unwrap()
}

/// A project/state pair that several sessions can target, for multi-session
/// tests. `Session` above owns one directory pair per session and stays as-is.
struct SharedProject {
    project: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl SharedProject {
    fn new() -> Self {
        let project = tempfile::Builder::new()
            .prefix("deltabox-macos-shared-project-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let state = tempfile::Builder::new()
            .prefix("deltabox-macos-shared-state-")
            .tempdir_in("/private/tmp")
            .unwrap();
        std::fs::write(project.path().join("a.txt"), "accepted").unwrap();
        Self { project, state }
    }

    fn try_start(&self, mode: &str) -> Output {
        command(
            self.project.path(),
            self.state.path(),
            [
                "session",
                "start",
                self.project.path().to_str().unwrap(),
                "--fs-mode",
                mode,
                "--require-fs-mode",
            ],
        )
    }
}

fn sid_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap()
        .to_string()
}

// Mode-aware admission: private sessions coexist; through, which writes the
// host tree directly, excludes everything and is excluded by anything live.
#[test]
fn private_sessions_coexist_and_through_stays_exclusive() {
    let env = SharedProject::new();
    let a = env.try_start("cbcow-private");
    assert_ok(&a);
    let b = env.try_start("cbcow-private");
    assert_ok(&b);
    let through = env.try_start("cbcow-through");
    assert!(!through.status.success());
    assert!(
        String::from_utf8_lossy(&through.stderr).contains("session busy"),
        "{through:?}"
    );
}

#[test]
fn through_session_excludes_everyone() {
    let env = SharedProject::new();
    assert_ok(&env.try_start("cbcow-through"));
    let second = env.try_start("cbcow-through");
    assert!(!second.status.success());
    let private = env.try_start("cbcow-private");
    assert!(!private.status.success());
}

// Deterministic lock proof: while one commit holds the publish lock, a
// sibling's commit must block instead of interleaving host mutations.
#[test]
fn racing_commits_serialize_under_publish_lock() {
    let env = SharedProject::new();
    let a = sid_of(&{
        let out = env.try_start("cbcow-private");
        assert_ok(&out);
        out
    });
    let b = sid_of(&{
        let out = env.try_start("cbcow-private");
        assert_ok(&out);
        out
    });
    let project = env.project.path().to_str().unwrap().to_string();
    for (sid, path) in [(&a, "race-a.txt"), (&b, "race-b.txt")] {
        let out = command(
            env.project.path(),
            env.state.path(),
            ["write", sid, path, "--content", "x", "--project", &project],
        );
        assert_ok(&out);
    }

    let pause = tempfile::tempdir().unwrap();
    let mut commit_a = std::process::Command::new(bin());
    commit_a
        .env("DELTABOX_STATE_DIR", env.state.path())
        .env("DELTABOX_TEST_PAUSE_AT", "publish_lock_held")
        .env("DELTABOX_TEST_PAUSE_DIR", pause.path())
        .args(["commit", &a, "--project", &project]);
    let mut commit_a = commit_a.spawn().unwrap();
    let arrived = pause.path().join("publish_lock_held.arrived");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !arrived.exists() {
        assert!(std::time::Instant::now() < deadline, "commit A never took the lock");
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut commit_b = std::process::Command::new(bin());
    commit_b
        .env("DELTABOX_STATE_DIR", env.state.path())
        .args(["commit", &b, "--project", &project]);
    let mut commit_b = commit_b.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        commit_b.try_wait().unwrap().is_none(),
        "commit B finished while A held the publish lock"
    );

    std::fs::write(pause.path().join("publish_lock_held.release"), b"").unwrap();
    assert!(commit_a.wait().unwrap().success());
    assert!(commit_b.wait().unwrap().success());
    assert!(env.project.path().join("race-a.txt").exists());
    assert!(env.project.path().join("race-b.txt").exists());
}

// Stage-3 north star: the losing session recovers with one sync — merges
// against the materialized theirs copy and lands on the second attempt.
#[test]
fn conflicting_session_recovers_via_sync_and_lands() {
    let env = SharedProject::new();
    let a = sid_of(&{
        let out = env.try_start("cbcow-private");
        assert_ok(&out);
        out
    });
    let b = sid_of(&{
        let out = env.try_start("cbcow-private");
        assert_ok(&out);
        out
    });
    let project = env.project.path().to_str().unwrap().to_string();
    let run = |args: Vec<&str>| command(env.project.path(), env.state.path(), args);

    assert_ok(&run(vec!["write", &a, "shared.txt", "--content", "a-version", "--project", &project]));
    assert_ok(&run(vec!["write", &b, "shared.txt", "--content", "b-version", "--project", &project]));
    assert_ok(&run(vec!["write", &b, "b-only.txt", "--content", "from-b", "--project", &project]));

    assert_ok(&run(vec!["commit", &a, "--project", &project]));
    let rejected = run(vec!["commit", &b, "--project", &project]);
    assert!(!rejected.status.success());

    let sync = run(vec!["sync", &b, "--project", &project]);
    assert_ok(&sync);
    let sync: Value = serde_json::from_slice(&sync.stdout).unwrap();
    assert!(sync["folded_publications"].as_u64().unwrap() >= 1);
    assert_eq!(sync["conflicts"][0]["path"], "shared.txt");
    assert_eq!(sync["conflicts"][0]["theirs"], "shared.txt.deltabox-theirs");

    let theirs = run(vec!["read", &b, "shared.txt.deltabox-theirs", "--project", &project]);
    assert_ok(&theirs);

    assert_ok(&run(vec!["write", &b, "shared.txt", "--content", "merged: a-version + b-version", "--project", &project]));
    let cleanup = run(vec!["exec", &b, "--project", &project, "--", "rm", "shared.txt.deltabox-theirs"]);
    assert_ok(&cleanup);

    assert_ok(&run(vec!["commit", &b, "--project", &project]));
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("shared.txt")).unwrap(),
        "merged: a-version + b-version"
    );
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("b-only.txt")).unwrap(),
        "from-b"
    );
    assert!(!env.project.path().join("shared.txt.deltabox-theirs").exists());
}

// The multi-agent north star: disjoint private commits both land; an
// overlapping commit is rejected whole with a structured conflict listing.
#[test]
fn disjoint_private_commits_land_and_overlap_conflicts() {
    let env = SharedProject::new();
    let a = sid_of(&{
        let out = env.try_start("cbcow-private");
        assert_ok(&out);
        out
    });
    let b = sid_of(&{
        let out = env.try_start("cbcow-private");
        assert_ok(&out);
        out
    });
    let project = env.project.path().to_str().unwrap().to_string();
    let write = |sid: &str, path: &str, content: &str| {
        let out = command(
            env.project.path(),
            env.state.path(),
            ["write", sid, path, "--content", content, "--project", &project],
        );
        assert_ok(&out);
    };
    write(&a, "a-only.txt", "from-a");
    write(&a, "shared.txt", "a-version");
    write(&b, "b-only.txt", "from-b");
    write(&b, "shared.txt", "b-version");

    let commit_a = command(
        env.project.path(),
        env.state.path(),
        ["commit", &a, "--project", &project],
    );
    assert_ok(&commit_a);

    let commit_b = command(
        env.project.path(),
        env.state.path(),
        ["commit", &b, "--project", &project],
    );
    assert!(!commit_b.status.success());
    let stderr = String::from_utf8_lossy(&commit_b.stderr);
    assert!(stderr.contains("commit blocked"), "{stderr}");
    assert!(stderr.contains("shared.txt"), "{stderr}");

    assert_eq!(
        std::fs::read_to_string(env.project.path().join("a-only.txt")).unwrap(),
        "from-a"
    );
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("shared.txt")).unwrap(),
        "a-version"
    );
    assert!(
        !env.project.path().join("b-only.txt").exists(),
        "a rejected commit must publish nothing"
    );
}

#[test]
fn strict_private_mode_is_durable_across_cli_processes() {
    let session = Session::start_with("cbcow-private", true);
    let runtime = session.command_runtime();
    let ready = runtime.join("command.ready");
    let socket = runtime.join("command.sock");
    assert_eq!(session.status()["durability"], "strict");
    assert_eq!(session.exec("printf one > a.txt")["exit_code"], 0);
    let first_ready: Value = serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    let helper_pid = first_ready["pid"].as_u64().unwrap();
    let helper_nonce = first_ready["nonce"].as_str().unwrap().to_owned();
    assert!(socket.exists());
    let checkpoint = session.run([
        "checkpoint",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
    ]);
    assert_ok(&checkpoint);
    let checkpoint: Value = serde_json::from_slice(&checkpoint.stdout).unwrap();
    let checkpoint = checkpoint["id"].as_str().unwrap();
    assert_eq!(session.exec("printf two > a.txt")["exit_code"], 0);
    let second_ready: Value = serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    assert_eq!(second_ready["pid"].as_u64(), Some(helper_pid));
    let abort = session.run([
        "abort",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
        "--checkpoint",
        checkpoint,
    ]);
    assert_ok(&abort);
    assert_eq!(session.exec("cat a.txt")["stdout"], "one");
    assert_ok(&session.commit());
    assert_eq!(
        std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
        "one"
    );

    let changed_environment = Command::new(bin())
        .env("DELTABOX_STATE_DIR", session.state.path())
        .env("PATH", "/usr/bin:/bin")
        .current_dir(session.project.path())
        .args([
            "exec",
            &session.id,
            "--project",
            session.project.path().to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            ":",
        ])
        .output()
        .unwrap();
    assert_ok(&changed_environment);
    let changed_ready: Value = serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    assert_ne!(changed_ready["nonce"].as_str(), Some(helper_nonce.as_str()));

    session.stop();
    assert!(!ready.exists());
    assert!(!socket.exists());
    assert!(!runtime.exists());
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn through_mode_tracks_commands_and_rolls_back_namespaces() {
    let session = Session::start("cbcow-through");
    let result = session.exec(
        "printf one > a.txt; printf peer > linked; ln linked alias; mkdir empty; ln -s a.txt link; printf ok > /dev/null; test ! -r .deltabox/sessions.json",
    );
    assert_eq!(result["exit_code"], 0);
    assert_eq!(
        std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
        "one"
    );

    let checkpoint = session.run([
        "checkpoint",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
        "--label",
        "save",
    ]);
    assert_ok(&checkpoint);
    let checkpoint: Value = serde_json::from_slice(&checkpoint.stdout).unwrap();
    let checkpoint = checkpoint["id"].as_str().unwrap();
    let result =
        session.exec("mv linked moved; rm alias; rmdir empty; rm link; printf two > a.txt");
    assert_eq!(result["exit_code"], 0);

    let abort = session.run([
        "abort",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
        "--checkpoint",
        checkpoint,
    ]);
    assert_ok(&abort);
    assert_eq!(
        std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
        "one"
    );
    assert!(session.project.path().join("linked").exists());
    assert!(session.project.path().join("alias").exists());
    assert!(session.project.path().join("empty").is_dir());
    assert_eq!(
        std::fs::read_link(session.project.path().join("link")).unwrap(),
        PathBuf::from("a.txt")
    );

    let status = session.status();
    assert_eq!(status["state_provider"], "macos-apfs-fsevents");
    assert_eq!(status["tainted"], false);
    session.stop();
}

#[test]
fn through_mode_marks_external_writes_and_refuses_abort() {
    let session = Session::start_with("cbcow-through", true);
    assert_ok(&session.commit());
    std::fs::write(session.project.path().join("a.txt"), "external").unwrap();
    let status = session.status();
    assert_eq!(status["tainted"], true);
    let abort = session.run([
        "abort",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
    ]);
    assert!(!abort.status.success());
    assert!(String::from_utf8_lossy(&abort.stderr).contains("abort is refused"));
    assert_ok(&session.commit());
    session.stop();
}

#[test]
fn fast_through_mode_uses_an_exclusive_writer_contract() {
    let session = Session::start("cbcow-through");
    assert_ok(&session.commit());
    std::fs::write(session.project.path().join("a.txt"), "external").unwrap();
    let status = session.status();
    assert_eq!(status["tainted"], false);
    assert_eq!(status["pending_changes"], 1);
    assert_ok(&session.commit());
    session.stop();
}

#[test]
fn event_helper_restarts_from_the_persisted_cursor() {
    let session = Session::start("cbcow-through");
    let socket = Path::new("/private/tmp").join(format!("deltafs-{}.sock", session.id));
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    deltafs_fuse::macos::stop_event_helper(&socket).unwrap();
    for _ in 0..100 {
        if !socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(session.exec("printf changed > a.txt")["exit_code"], 0);
    assert_eq!(session.status()["pending_changes"], 1);
    assert!(socket.exists());
    session.stop();
}

#[test]
fn private_mode_hides_live_and_rejects_host_conflicts() {
    let session = Session::start("cbcow-private");
    let host = session.project.path().join("a.txt");
    let script = format!(
        "printf private > a.txt; cat a.txt; ! cat {}",
        host.display()
    );
    let result = session.exec(&script);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"], "private");
    assert_eq!(std::fs::read_to_string(&host).unwrap(), "accepted");
    assert_ok(&session.commit());
    assert_eq!(std::fs::read_to_string(&host).unwrap(), "private");

    let result = session.exec("printf pending > a.txt");
    assert_eq!(result["exit_code"], 0);
    std::fs::write(&host, "external").unwrap();
    let conflict = session.commit();
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("project changed outside deltabox"));
    assert_eq!(std::fs::read_to_string(&host).unwrap(), "external");
    session.stop();
    assert_eq!(std::fs::read_to_string(&host).unwrap(), "external");
}

#[test]
fn private_commit_recovers_process_crash_boundaries() {
    for point in [
        "macos_commit_prepared",
        "macos_commit_applied",
        "macos_commit_state_updated",
    ] {
        let session = Session::start("cbcow-private");
        let result = session.exec("printf new > a.txt");
        assert_eq!(result["exit_code"], 0);
        let crashed = Command::new(bin())
            .env("DELTABOX_STATE_DIR", session.state.path())
            .env("DELTABOX_TEST_CRASH_AT", point)
            .current_dir(session.project.path())
            .args([
                "commit",
                &session.id,
                "--project",
                session.project.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            !crashed.status.success(),
            "crash point {point} returned success"
        );
        let status = session.status();
        if point == "macos_commit_state_updated" {
            assert_eq!(status["pending_changes"], 0);
            assert_eq!(
                std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
                "new"
            );
        } else {
            assert_eq!(status["pending_changes"], 1);
            assert_eq!(
                std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
                "accepted"
            );
            assert_ok(&session.commit());
        }
        session.stop();
    }
}

// docs/protocol.md: session.sync requires cbcow-private. A through session
// must be rejected outright -- a silent empty result would mask client bugs.
#[test]
fn sync_rejects_through_sessions() {
    let session = Session::start("cbcow-through");
    let sync = session.run([
        "sync",
        &session.id,
        "--project",
        session.project.path().to_str().unwrap(),
    ]);
    assert!(!sync.status.success());
    assert!(
        String::from_utf8_lossy(&sync.stderr).contains("requires a cbcow-private session"),
        "{sync:?}"
    );
    session.stop();
}

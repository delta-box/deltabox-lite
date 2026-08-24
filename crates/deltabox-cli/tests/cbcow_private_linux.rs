#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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
        Self::start_with_options(project, policy, false)
    }

    fn start_durable(project: &Path) -> Self {
        Self::start_with_options(project, None, true)
    }

    fn start_with_options(project: &Path, policy: Option<&Path>, durable: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command.args([
            "session",
            "start",
            project.to_str().unwrap(),
            "--fs-mode",
            "cbcow-private",
            "--require-fs-mode",
        ]);
        if durable {
            command.arg("--durable");
        }
        if let Some(policy) = policy {
            command.arg("--policy").arg(policy);
        }
        let output = command.output().unwrap();
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

    fn command_with_env<const N: usize>(&self, args: [&str; N], key: &str, value: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command
            .args(args)
            .arg("--project")
            .arg(&self.project)
            .env(key, value);
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

    fn commit(&self) -> Output {
        self.command(["commit", &self.id])
    }

    fn commit_to(&self, checkpoint: &str) -> Output {
        self.command(["commit", &self.id, "--checkpoint", checkpoint])
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
        assert_ok(&command.output().unwrap());
    }

    fn exec_shell(&self, script: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .args(["exec", &self.id, "--project"])
            .arg(&self.project)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap()
    }

    fn mount_record(&self) -> Value {
        serde_json::from_slice(
            &std::fs::read(
                self.project
                    .join(".deltabox")
                    .join(&self.id)
                    .join("mount.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.command(["session", "stop", &self.id]);
    }
}

#[test]
fn private_view_commit_abort_and_checkpoints_cross_processes() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("README.md"), "base\n").unwrap();
    let session = Session::start(project.path());
    let status = session.status();
    assert_eq!(status["requested_fs_mode"], "cbcow-private");
    assert_eq!(status["resolved_fs_mode"], "cbcow-private");
    assert_eq!(status["host_visibility"], "on-commit");
    assert_eq!(status["rollback_guarantee"], "private-live-tree");

    session.write("created.txt", "private");
    assert!(!project.path().join("created.txt").exists());
    let checkpoint = session.checkpoint();
    session.write("README.md", "pending\n");
    session.abort(Some(&checkpoint));
    assert_eq!(
        std::fs::read_to_string(project.path().join("README.md")).unwrap(),
        "base\n"
    );
    let view = session
        .exec_shell("test \"$(cat created.txt)\" = private; test \"$(cat README.md)\" = base");
    assert_exec_ok(&view);

    assert_ok(&session.commit());
    assert_eq!(
        std::fs::read_to_string(project.path().join("created.txt")).unwrap(),
        "private"
    );
    session.write("README.md", "later\n");
    session.abort(None);
    assert_eq!(
        std::fs::read_to_string(project.path().join("README.md")).unwrap(),
        "base\n"
    );
}

#[test]
fn git_commit_and_worktree_publish_together() {
    if !available() || Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(project.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(project.path().join("tracked.txt"), "base\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(project.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.name=DeltaFS",
            "-c",
            "user.email=deltafs@example.invalid",
            "commit",
            "-qm",
            "base",
        ])
        .current_dir(project.path())
        .status()
        .unwrap()
        .success());
    let host_head = git_head(project.path());
    let session = Session::start(project.path());
    let output = session.exec_shell(
        "printf 'changed\\n' > tracked.txt; printf 'new\\n' > new.txt; git add tracked.txt new.txt; git -c user.name=DeltaFS -c user.email=deltafs@example.invalid commit -qm private",
    );
    assert_exec_ok(&output);
    assert_eq!(git_head(project.path()), host_head);
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert!(!project.path().join("new.txt").exists());

    assert_ok(&session.commit());
    assert_ne!(git_head(project.path()), host_head);
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("new.txt")).unwrap(),
        "new\n"
    );
    assert!(Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(project.path())
        .output()
        .unwrap()
        .stdout
        .is_empty());
}

#[test]
fn namespace_changes_stay_private_and_older_checkpoint_commit_discards_future() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("remove.txt"), "remove").unwrap();
    std::fs::write(project.path().join("rename.txt"), "rename").unwrap();
    let session = Session::start(project.path());
    let output =
        session.exec_shell("rm remove.txt; mv rename.txt moved.txt; printf first > version.txt");
    assert_exec_ok(&output);
    assert!(project.path().join("remove.txt").exists());
    assert!(project.path().join("rename.txt").exists());
    assert!(!project.path().join("moved.txt").exists());
    assert!(!project.path().join("version.txt").exists());

    let checkpoint = session.checkpoint();
    session.write("version.txt", "future");
    session.write("future.txt", "discarded");
    assert_ok(&session.commit_to(&checkpoint));
    assert!(!project.path().join("remove.txt").exists());
    assert!(!project.path().join("rename.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("moved.txt")).unwrap(),
        "rename"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("version.txt")).unwrap(),
        "first"
    );
    assert!(!project.path().join("future.txt").exists());
    assert_eq!(session.status()["pending_changes"], 0);
}

#[test]
fn conflicting_session_recovers_via_sync_and_lands() {
    if !available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("shared.txt"), "base").unwrap();
    let a = Session::start(&project);
    let b = Session::start(&project);

    a.write("shared.txt", "a-version");
    b.write("shared.txt", "b-version");
    b.write("b-only.txt", "from-b");

    let commit_a = a.commit();
    assert_ok(&commit_a);
    let rejected = b.commit();
    assert!(!rejected.status.success());

    let sync = b.command(["sync", &b.id]);
    assert_ok(&sync);
    let sync: Value = serde_json::from_slice(&sync.stdout).unwrap();
    assert!(sync["folded_publications"].as_u64().unwrap() >= 1);
    assert_eq!(sync["conflicts"][0]["path"], "shared.txt");

    b.write("shared.txt", "merged");

    // Commit with the draft still present: the tool, not the agent, keeps the
    // private `.deltabox-theirs` copy out of the host project.
    let landed = b.commit();
    assert_ok(&landed);
    assert_eq!(
        std::fs::read_to_string(project.join("shared.txt")).unwrap(),
        "merged"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("b-only.txt")).unwrap(),
        "from-b"
    );
    // The draft never crosses into the real project.
    assert!(!project.join("shared.txt.deltabox-theirs").exists());
}

#[test]
fn host_conflicts_preserve_external_and_private_states() {
    if !available() {
        return;
    }
    for scenario in ["modified", "deleted", "created", "parent", "subtree"] {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(project.join("dir")).unwrap();
        std::fs::write(project.join("dir/base.txt"), "base").unwrap();
        let session = Session::start(&project);
        match scenario {
            "modified" => {
                session.write("dir/base.txt", "private");
                std::fs::write(project.join("dir/base.txt"), "external").unwrap();
            }
            "deleted" => {
                session.write("dir/base.txt", "private");
                std::fs::remove_file(project.join("dir/base.txt")).unwrap();
            }
            "created" => {
                session.write("dir/new.txt", "private");
                std::fs::write(project.join("dir/new.txt"), "external").unwrap();
            }
            "parent" => {
                session.write("dir/base.txt", "private");
                std::fs::write(project.join("dir/external.txt"), "external").unwrap();
            }
            "subtree" => {
                assert_exec_ok(&session.exec_shell("rm -r dir"));
                std::fs::write(project.join("dir/base.txt"), "external").unwrap();
            }
            _ => unreachable!(),
        }
        let commit = session.commit();
        if scenario == "parent" {
            // Relaxed ancestor validation for concurrent sessions: an
            // unrelated entry appearing in a shared parent directory no
            // longer rejects the commit — the target's own fingerprint still
            // guards real conflicts, and both states must survive.
            assert!(commit.status.success(), "scenario parent was rejected");
            assert_eq!(
                std::fs::read_to_string(project.join("dir/base.txt")).unwrap(),
                "private"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("dir/external.txt")).unwrap(),
                "external"
            );
            continue;
        }
        assert!(!commit.status.success(), "scenario {scenario} committed");
        assert!(String::from_utf8_lossy(&commit.stderr).contains("project changed outside"));
        let diff = session.command(["diff", &session.id, "--json"]);
        assert_ok(&diff);
        assert!(
            serde_json::from_slice::<Value>(&diff.stdout).unwrap()["pending_changes"]
                .as_u64()
                .unwrap()
                > 0
        );
        match scenario {
            "modified" => assert_eq!(
                std::fs::read_to_string(project.join("dir/base.txt")).unwrap(),
                "external"
            ),
            "deleted" => assert!(!project.join("dir/base.txt").exists()),
            "created" => assert_eq!(
                std::fs::read_to_string(project.join("dir/new.txt")).unwrap(),
                "external"
            ),
            "subtree" => assert_eq!(
                std::fs::read_to_string(project.join("dir/base.txt")).unwrap(),
                "external"
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn helper_restart_preserves_private_undo_and_backing_is_denied() {
    if !available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let policy = project.join("policy.toml");
    std::fs::write(
        &policy,
        "[filesystem]\nread = [\"/\"]\nwrite = [\".\"]\ndeny = []\n",
    )
    .unwrap();
    std::fs::write(project.join("secret.txt"), "host").unwrap();
    let session = Session::start_with_policy(&project, Some(&policy));
    session.write("pending.txt", "private");
    let record = session.mount_record();
    let source = PathBuf::from(record["source"].as_str().unwrap());
    let alias = root.path().join("backing-alias");
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    let state = PathBuf::from(record["state_dir"].as_str().unwrap());
    let socket = PathBuf::from(record["socket"].as_str().unwrap());
    for script in [
        format!("cat '{}/secret.txt' >/dev/null", alias.display()),
        format!("cat '{}/session.json' >/dev/null", state.display()),
        format!("printf x > '{}'", socket.display()),
    ] {
        let output = session.exec_shell(&script);
        assert_ok(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(
            result["exit_code"], 0,
            "direct path was accessible: {script}"
        );
    }

    let pid = record["helper_pid"].as_u64().unwrap() as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let status = session.status();
    assert_eq!(status["helper_restarts"], 1);
    assert_eq!(status["pending_changes"], 1);
    session.abort(None);
    assert!(!project.join("pending.txt").exists());
}

#[test]
fn publication_recovers_each_process_crash_boundary() {
    if !available() {
        return;
    }
    for durable in [false, true] {
        for point in [
            "private_publish_prepared",
            "private_publish_path_applied",
            "private_publish_host_applied",
            "private_publish_engine_committed",
            "private_publish_state_updated",
        ] {
            let project = tempfile::tempdir().unwrap();
            std::fs::write(project.path().join("value.txt"), "old").unwrap();
            let session = if durable {
                Session::start_durable(project.path())
            } else {
                Session::start(project.path())
            };
            session.write("value.txt", "new");
            let crashed =
                session.command_with_env(["commit", &session.id], "DELTABOX_TEST_CRASH_AT", point);
            assert!(
                !crashed.status.success(),
                "crash point {point} returned success"
            );
            let status = session.status();
            assert_eq!(
                status["durability"],
                if durable { "strict" } else { "process-crash" }
            );
            if matches!(
                point,
                "private_publish_prepared" | "private_publish_path_applied"
            ) {
                assert_eq!(status["pending_changes"], 1);
                assert_eq!(
                    std::fs::read_to_string(project.path().join("value.txt")).unwrap(),
                    "old"
                );
                assert_ok(&session.commit());
            } else {
                assert_eq!(status["pending_changes"], 0);
            }
            assert_eq!(
                std::fs::read_to_string(project.path().join("value.txt")).unwrap(),
                "new"
            );
        }
    }
}

#[test]
fn publication_preserves_hardlinks_xattrs_and_sparse_layout() {
    if !available() {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("linked"), "base").unwrap();
    std::fs::hard_link(project.path().join("linked"), project.path().join("alias")).unwrap();
    xattr::set(project.path().join("linked"), "user.deltafs", b"base").unwrap();
    let session = Session::start(project.path());
    let output = session.exec_shell(
        "printf changed > linked; python3 -c \"import os; os.setxattr('linked', b'user.deltafs', b'changed')\"; setfacl -m u:65534:r linked; truncate -s 16777216 sparse.bin; printf tail | dd of=sparse.bin bs=1 seek=16777212 conv=notrunc status=none",
    );
    assert_exec_ok(&output);
    assert_ok(&session.commit());
    assert_eq!(
        std::fs::metadata(project.path().join("linked"))
            .unwrap()
            .ino(),
        std::fs::metadata(project.path().join("alias"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        xattr::get(project.path().join("linked"), "user.deltafs")
            .unwrap()
            .unwrap(),
        b"changed"
    );
    assert!(
        xattr::get(project.path().join("linked"), "system.posix_acl_access")
            .unwrap()
            .is_some()
    );
    let sparse = std::fs::metadata(project.path().join("sparse.bin")).unwrap();
    assert_eq!(sparse.len(), 16 * 1024 * 1024);
    assert!(sparse.blocks() * 512 < sparse.len());
}

#[test]
fn sqlite_database_publishes_and_retired_backups_are_reclaimed() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());
    let output = session.exec_shell(
        "python3 -c \"import sqlite3; db=sqlite3.connect('data.db'); db.execute('pragma journal_mode=wal'); db.execute('create table values_(value text)'); db.execute('insert into values_ values (\\\"private\\\")'); db.commit(); db.close()\"",
    );
    assert_exec_ok(&output);
    assert!(!project.path().join("data.db").exists());
    assert_ok(&session.commit());
    let output = Command::new("python3")
        .args([
            "-c",
            "import sqlite3,sys; sys.stdout.write(sqlite3.connect(sys.argv[1]).execute('select value from values_').fetchone()[0])",
        ])
        .arg(project.path().join("data.db"))
        .output()
        .unwrap();
    assert_ok(&output);
    assert_eq!(output.stdout, b"private");

    let reclaim = PathBuf::from(session.mount_record()["reclaim_dir"].as_str().unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && std::fs::read_dir(&reclaim).is_ok_and(|mut entries| entries.next().is_some())
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(std::fs::read_dir(reclaim).is_ok_and(|mut entries| entries.next().is_none()));
}

fn git_head(project: &Path) -> String {
    String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .into()
}

fn assert_exec_ok(output: &Output) {
    assert_ok(output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "exec result: {result}");
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// Multiagent start overlap: while one start is parked inside its shared-mode
// publish-lock materialization, a sibling start must run to completion
// instead of queueing behind it. Under the old exclusive hold, B would block
// until A released and this test would fail on the pause hook's 30s abort.
#[test]
fn concurrent_private_starts_overlap_scans() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "accepted").unwrap();
    let pause = tempfile::tempdir().unwrap();
    let mut start_a = Command::new(env!("CARGO_BIN_EXE_deltabox"));
    start_a
        .env("DELTABOX_TEST_PAUSE_AT", "start_scan_shared")
        .env("DELTABOX_TEST_PAUSE_DIR", pause.path())
        .args(["session", "start"])
        .arg(project.path())
        .args(["--fs-mode", "cbcow-private", "--require-fs-mode"])
        .stdout(std::process::Stdio::piped());
    let start_a = start_a.spawn().unwrap();
    let arrived = pause.path().join("start_scan_shared.arrived");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !arrived.exists() {
        assert!(Instant::now() < deadline, "start A never reached its scan");
        std::thread::sleep(Duration::from_millis(5));
    }

    let b = Session::start(project.path());

    std::fs::write(pause.path().join("start_scan_shared.release"), b"").unwrap();
    let out_a = start_a.wait_with_output().unwrap();
    assert!(out_a.status.success(), "{out_a:?}");
    let a_id = String::from_utf8_lossy(&out_a.stdout)
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert_ok(&b.command(["session", "status", &a_id]));
    assert_ok(&b.command(["session", "stop", &a_id]));
}

// The other direction of the shared/exclusive split: publication stays fully
// excluded from a start's materialization window, and the started session's
// baseline snapshot therefore predates the publication, which folds for it.
#[test]
fn start_scan_excludes_publication() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "accepted").unwrap();
    let a = Session::start(project.path());
    a.write("pending.txt", "x");

    let pause = tempfile::tempdir().unwrap();
    let mut start_b = Command::new(env!("CARGO_BIN_EXE_deltabox"));
    start_b
        .env("DELTABOX_TEST_PAUSE_AT", "start_scan_shared")
        .env("DELTABOX_TEST_PAUSE_DIR", pause.path())
        .args(["session", "start"])
        .arg(project.path())
        .args(["--fs-mode", "cbcow-private", "--require-fs-mode"])
        .stdout(std::process::Stdio::piped());
    let start_b = start_b.spawn().unwrap();
    let arrived = pause.path().join("start_scan_shared.arrived");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !arrived.exists() {
        assert!(Instant::now() < deadline, "start B never reached its scan");
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut commit_a = Command::new(env!("CARGO_BIN_EXE_deltabox"));
    commit_a
        .args(["commit", &a.id])
        .arg("--project")
        .arg(project.path());
    let mut commit_a = commit_a.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        commit_a.try_wait().unwrap().is_none(),
        "a publication landed inside a start's materialization window"
    );

    std::fs::write(pause.path().join("start_scan_shared.release"), b"").unwrap();
    let out_b = start_b.wait_with_output().unwrap();
    assert!(out_b.status.success(), "{out_b:?}");
    let b_id = String::from_utf8_lossy(&out_b.stdout)
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(commit_a.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(project.path().join("pending.txt")).unwrap(),
        "x"
    );

    let sync = a.command(["sync", &b_id]);
    assert_ok(&sync);
    let sync: Value = serde_json::from_slice(&sync.stdout).unwrap();
    assert_eq!(sync["folded_publications"], 1);
    assert_ok(&a.command(["session", "stop", &b_id]));
}

// A start killed inside its materialization window leaves its admission pin
// behind: a Running witness plus a lease, with no engine record. The zombie
// stays visible (the through rejection names it) and is clearable with one
// `session stop`; a living start is protected from that same stop.
#[test]
fn killed_start_leaves_stoppable_admission_pin() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "accepted").unwrap();
    let pause = tempfile::tempdir().unwrap();
    let mut start_a = Command::new(env!("CARGO_BIN_EXE_deltabox"));
    start_a
        .env("DELTABOX_TEST_PAUSE_AT", "start_scan_shared")
        .env("DELTABOX_TEST_PAUSE_DIR", pause.path())
        .args(["session", "start"])
        .arg(project.path())
        .args(["--fs-mode", "cbcow-private", "--require-fs-mode"])
        .stdout(std::process::Stdio::piped());
    let mut start_a = start_a.spawn().unwrap();
    let arrived = pause.path().join("start_scan_shared.arrived");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !arrived.exists() {
        assert!(Instant::now() < deadline, "start never reached its scan");
        std::thread::sleep(Duration::from_millis(5));
    }

    let raw = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .args(args)
            .arg("--project")
            .arg(project.path())
            .output()
            .unwrap()
    };
    let through_live = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["session", "start"])
        .arg(project.path())
        .args(["--fs-mode", "cbcow-through", "--require-fs-mode"])
        .output()
        .unwrap();
    assert!(!through_live.status.success());
    let stderr = String::from_utf8_lossy(&through_live.stderr).to_string();
    assert!(stderr.contains("session busy: "), "{stderr}");
    let sid = stderr
        .split("session busy: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let stop_live = raw(&["session", "stop", &sid]);
    assert!(!stop_live.status.success(), "stopped a live start");
    assert!(
        String::from_utf8_lossy(&stop_live.stderr).contains("session busy"),
        "{stop_live:?}"
    );

    start_a.kill().unwrap();
    start_a.wait().unwrap();

    let stop_dead = raw(&["session", "stop", &sid]);
    assert_ok(&stop_dead);
    let through = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["session", "start"])
        .arg(project.path())
        .args(["--fs-mode", "cbcow-through", "--require-fs-mode"])
        .output()
        .unwrap();
    assert_ok(&through);
    let through_id = String::from_utf8_lossy(&through.stdout)
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert_ok(&raw(&["session", "stop", &through_id]));
}

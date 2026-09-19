#![cfg(target_os = "linux")]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
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
        Self::start_with_options(project, policy, false, None)
    }

    fn start_durable(project: &Path, group_commit_ms: Option<u64>) -> Self {
        Self::start_with_options(project, None, true, group_commit_ms)
    }

    fn start_with_options(
        project: &Path,
        policy: Option<&Path>,
        durable: bool,
        group_commit_ms: Option<u64>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command.args([
            "session",
            "start",
            project.to_str().unwrap(),
            "--fs-mode",
            "cbcow-through",
            "--require-fs-mode",
        ]);
        if durable {
            command.arg("--durable");
            if let Some(milliseconds) = group_commit_ms {
                command
                    .arg("--group-commit-ms")
                    .arg(milliseconds.to_string());
            }
        }
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

    fn commit(&self) {
        let output = self.command(["commit", &self.id]);
        assert_ok(&output);
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
fn strict_mode_survives_helper_restart_before_and_after_commit() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("state.txt"), "old").unwrap();
    let session = Session::start_durable(project.path(), Some(2));
    let status = session.status();
    assert_eq!(status["durability"], "strict");
    assert_eq!(status["group_commit_ms"], 2);

    session.write("state.txt", "pending");
    kill_helper(&session.status());
    assert_eq!(session.status()["pending_changes"], 1);
    session.abort(None);
    assert_eq!(
        std::fs::read_to_string(project.path().join("state.txt")).unwrap(),
        "old"
    );

    session.write("state.txt", "accepted");
    session.commit();
    kill_helper(&session.status());
    assert_eq!(session.status()["pending_changes"], 0);
    assert_eq!(
        std::fs::read_to_string(project.path().join("state.txt")).unwrap(),
        "accepted"
    );
}

#[test]
fn abort_invalidates_cached_content_and_namespace() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("content.txt"), "accepted\n").unwrap();
    std::fs::write(project.path().join("original.txt"), "original\n").unwrap();
    let session = Session::start(project.path());

    let warm = session
        .exec_shell("test \"$(cat content.txt)\" = accepted; test -f original.txt; ls >/dev/null");
    assert_ok(&warm);
    let result: Value = serde_json::from_slice(&warm.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "cache warm result: {result}");
    let mutate = session.exec_shell(
        "printf 'pending\\n' > content.txt; printf 'created\\n' > created.txt; mv original.txt renamed.txt; cat content.txt renamed.txt >/dev/null; ls >/dev/null",
    );
    assert_ok(&mutate);
    let result: Value = serde_json::from_slice(&mutate.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "cache mutation result: {result}");

    session.abort(None);
    let restored = session.exec_shell(
        "test \"$(cat content.txt)\" = accepted; test -f original.txt; test ! -e renamed.txt; test ! -e created.txt; ls >/dev/null",
    );
    assert_ok(&restored);
    let result: Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(
        result["exit_code"], 0,
        "cache invalidation result: {result}"
    );
}

#[test]
fn abort_then_recreated_files_are_immediately_readable() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());

    for index in 0..8 {
        let name = format!("recreated-{index}.txt");
        let output = session.exec_shell(&format!(
            "printf recreated > {name}; test \"$(cat {name})\" = recreated"
        ));
        assert_ok(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["exit_code"], 0, "recreated file result: {result}");
        session.abort(None);
        assert!(!project.path().join(name).exists());
    }
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
fn helper_restarts_preserve_control_boundaries_and_reclaim_history() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("state.txt"), "accepted").unwrap();
    let session = Session::start(project.path());

    session.write("state.txt", "checkpointed");
    kill_helper(&session.status());
    let checkpoint = session.checkpoint();
    assert_eq!(session.status()["helper_restarts"], 1);

    session.write("state.txt", "committed");
    kill_helper(&session.status());
    session.commit();
    assert_eq!(session.status()["helper_restarts"], 2);
    assert_eq!(
        std::fs::read_to_string(project.path().join("state.txt")).unwrap(),
        "committed"
    );

    session.write("state.txt", "pending");
    kill_helper(&session.status());
    session.abort(None);
    assert_eq!(session.status()["helper_restarts"], 3);
    assert_eq!(
        std::fs::read_to_string(project.path().join("state.txt")).unwrap(),
        "committed"
    );
    assert_ne!(checkpoint, "0");

    let record: Value = serde_json::from_slice(
        &std::fs::read(
            project
                .path()
                .join(".deltabox")
                .join(&session.id)
                .join("mount.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let state = PathBuf::from(record["state_dir"].as_str().unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && std::fs::read_dir(state.join("gc"))
            .unwrap()
            .next()
            .is_some()
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(std::fs::read_dir(state.join("gc"))
        .unwrap()
        .next()
        .is_none());
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
fn inode_handles_and_atomic_rename_flags_round_trip() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("open.txt"), "original").unwrap();
    std::fs::write(project.path().join("source.txt"), "source").unwrap();
    std::fs::write(project.path().join("target.txt"), "target").unwrap();
    std::fs::write(project.path().join("first.txt"), "first").unwrap();
    std::fs::write(project.path().join("second.txt"), "second").unwrap();
    let session = Session::start(project.path());

    let output = session.exec_shell(concat!(
        "python3 -c 'import ctypes, errno, os\n",
        "fd = os.open(\"open.txt\", os.O_RDWR)\n",
        "os.unlink(\"open.txt\")\n",
        "os.pwrite(fd, b\"detached\", 0)\n",
        "assert os.pread(fd, 8, 0) == b\"detached\"\n",
        "os.ftruncate(fd, 4)\n",
        "os.fchmod(fd, 0o600)\n",
        "assert os.fstat(fd).st_size == 4\n",
        "assert os.fstat(fd).st_mode & 0o777 == 0o600\n",
        "open(\"open.txt\", \"wb\").write(b\"recreated\")\n",
        "assert os.pread(fd, 4, 0) == b\"deta\"\n",
        "os.close(fd)\n",
        "os.link(\"source.txt\", \"linked.txt\")\n",
        "assert os.stat(\"source.txt\").st_ino == os.stat(\"linked.txt\").st_ino\n",
        "target_fd = os.open(\"target.txt\", os.O_RDWR)\n",
        "os.rename(\"source.txt\", \"target.txt\")\n",
        "os.pwrite(target_fd, b\"old\", 0)\n",
        "assert os.pread(target_fd, 6, 0).startswith(b\"old\")\n",
        "assert open(\"target.txt\", \"rb\").read() == b\"source\"\n",
        "os.close(target_fd)\n",
        "libc = ctypes.CDLL(None, use_errno=True)\n",
        "result = libc.renameat2(-100, b\"first.txt\", -100, b\"second.txt\", 1)\n",
        "assert result == -1 and ctypes.get_errno() == errno.EEXIST\n",
        "assert libc.renameat2(-100, b\"first.txt\", -100, b\"second.txt\", 2) == 0\n",
        "assert open(\"first.txt\").read() == \"second\"\n",
        "assert open(\"second.txt\").read() == \"first\"'"
    ));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "filesystem result: {result}");

    session.abort(None);
    assert_eq!(
        std::fs::read_to_string(project.path().join("open.txt")).unwrap(),
        "original"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("source.txt")).unwrap(),
        "source"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("target.txt")).unwrap(),
        "target"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("first.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("second.txt")).unwrap(),
        "second"
    );
    assert!(!project.path().join("linked.txt").exists());
}

#[test]
fn mmap_metadata_sparse_and_sync_round_trip() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let sparse = project.path().join("sparse.bin");
    let file = std::fs::File::create(&sparse).unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, b"start", 0).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, b"end", 8 * 1024 * 1024 - 3).unwrap();
    let accepted_blocks = std::os::unix::fs::MetadataExt::blocks(&file.metadata().unwrap());
    std::fs::write(project.path().join("mapped.bin"), b"before").unwrap();
    let session = Session::start(project.path());

    let output = session.exec_shell(concat!(
        "python3 -c 'import mmap, os\n",
        "fd = os.open(\"mapped.bin\", os.O_RDWR)\n",
        "view = mmap.mmap(fd, 6, access=mmap.ACCESS_WRITE)\n",
        "view[:] = b\"mapped\"\n",
        "view.flush(); view.close(); os.fsync(fd); os.close(fd)\n",
        "os.chmod(\"mapped.bin\", 0o640)\n",
        "os.setxattr(\"mapped.bin\", b\"user.deltafs\", b\"value\")\n",
        "directory = os.open(\".\", os.O_RDONLY | os.O_DIRECTORY)\n",
        "os.fsync(directory); os.close(directory)\n",
        "data = os.open(\"sparse.bin\", os.O_RDONLY)\n",
        "assert os.lseek(data, 0, os.SEEK_DATA) == 0\n",
        "assert os.lseek(data, 4096, os.SEEK_HOLE) <= 8192\n",
        "os.close(data)\n",
        "os.unlink(\"sparse.bin\")'"
    ));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "filesystem result: {result}");
    assert_eq!(
        std::fs::read(project.path().join("mapped.bin")).unwrap(),
        b"mapped"
    );

    session.abort(None);
    assert_eq!(
        std::fs::read(project.path().join("mapped.bin")).unwrap(),
        b"before"
    );
    let restored = std::fs::metadata(&sparse).unwrap();
    assert_eq!(restored.len(), 8 * 1024 * 1024);
    assert_eq!(
        std::os::unix::fs::MetadataExt::blocks(&restored),
        accepted_blocks
    );
    let mounted = session.exec_shell(
        "test \"$(cat mapped.bin)\" = before; test -f sparse.bin; test \"$(stat -c %s sparse.bin)\" = 8388608",
    );
    assert_ok(&mounted);
    let result: Value = serde_json::from_slice(&mounted.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "restored mount result: {result}");
}

#[test]
fn access_control_lists_round_trip() {
    if !available()
        || !Command::new("sh")
            .args([
                "-c",
                "command -v setfacl >/dev/null && command -v getfacl >/dev/null",
            ])
            .status()
            .unwrap()
            .success()
    {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join("acl.txt");
    std::fs::write(&path, "content").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let accepted_mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&path).unwrap());
    let session = Session::start(project.path());

    let output = session.exec_shell(
        "setfacl -m u:65534:r-- acl.txt; getfacl -cpn acl.txt | grep -qx 'user:65534:r--'",
    );
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "ACL result: {result}");

    session.abort(None);
    let acl = Command::new("getfacl")
        .args(["-cpn", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ok(&acl);
    assert!(!String::from_utf8_lossy(&acl.stdout).contains("user:65534:r--"));
    assert_eq!(
        std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&path).unwrap()),
        accepted_mode
    );
}

#[test]
fn sparse_range_and_fallocate_operations_round_trip() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let sparse = project.path().join("sparse.bin");
    let sparse_file = std::fs::File::create(&sparse).unwrap();
    sparse_file.set_len(8 * 1024 * 1024).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&sparse_file, b"start", 0).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&sparse_file, b"end", 8 * 1024 * 1024 - 3).unwrap();
    std::fs::write(project.path().join("punch.bin"), vec![b'P'; 1024 * 1024]).unwrap();
    std::fs::write(project.path().join("zero.bin"), vec![b'Z'; 1024 * 1024]).unwrap();
    std::fs::write(
        project.path().join("copy-source.bin"),
        vec![b'S'; 1024 * 1024],
    )
    .unwrap();
    std::fs::write(
        project.path().join("copy-target.bin"),
        vec![b'T'; 1024 * 1024],
    )
    .unwrap();
    let accepted: Vec<(PathBuf, Vec<u8>, u64)> = [
        "sparse.bin",
        "punch.bin",
        "zero.bin",
        "copy-source.bin",
        "copy-target.bin",
    ]
    .into_iter()
    .map(|name| {
        let path = project.path().join(name);
        let metadata = std::fs::metadata(&path).unwrap();
        (
            path.clone(),
            std::fs::read(path).unwrap(),
            std::os::unix::fs::MetadataExt::blocks(&metadata),
        )
    })
    .collect();
    let session = Session::start(project.path());

    let output = session.exec_shell(concat!(
        "python3 -c 'import ctypes, errno, os\n",
        "libc = ctypes.CDLL(None, use_errno=True)\n",
        "sparse = os.open(\"sparse.bin\", os.O_RDWR)\n",
        "os.pwrite(sparse, b\"X\" * 4096, 1024 * 1024)\n",
        "os.posix_fallocate(sparse, 2 * 1024 * 1024, 64 * 1024)\n",
        "os.ftruncate(sparse, 4 * 1024 * 1024)\n",
        "os.close(sparse)\n",
        "punch = os.open(\"punch.bin\", os.O_RDWR)\n",
        "assert libc.fallocate(punch, 3, 256 * 1024, 128 * 1024) == 0\n",
        "assert os.pread(punch, 128 * 1024, 256 * 1024) == bytes(128 * 1024)\n",
        "os.close(punch)\n",
        "zero = os.open(\"zero.bin\", os.O_RDWR)\n",
        "assert libc.fallocate(zero, 17, 256 * 1024, 128 * 1024) == 0\n",
        "assert os.pread(zero, 128 * 1024, 256 * 1024) == bytes(128 * 1024)\n",
        "assert libc.fallocate(zero, 8, 0, 4096) == -1\n",
        "assert ctypes.get_errno() == errno.EOPNOTSUPP\n",
        "os.close(zero)\n",
        "source = os.open(\"copy-source.bin\", os.O_RDONLY)\n",
        "target = os.open(\"copy-target.bin\", os.O_RDWR)\n",
        "assert os.copy_file_range(source, target, 256 * 1024, 64 * 1024, 128 * 1024) == 256 * 1024\n",
        "assert os.pread(target, 256 * 1024, 128 * 1024) == b\"S\" * (256 * 1024)\n",
        "os.close(source); os.close(target)'"
    ));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "fallocate result: {result}");

    session.abort(None);
    for (path, bytes, blocks) in accepted {
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{}", path.display());
        assert_eq!(
            std::os::unix::fs::MetadataExt::blocks(&metadata),
            blocks,
            "{}",
            path.display()
        );
    }
}

#[test]
fn sqlite_wal_and_advisory_lock_are_rollback_safe() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());
    let output = session.exec_shell(concat!(
        "python3 -c 'import fcntl, os, sqlite3\n",
        "lock = open(\"lockfile\", \"w\")\n",
        "fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)\n",
        "db = sqlite3.connect(\"state.db\")\n",
        "db.execute(\"pragma journal_mode=wal\")\n",
        "db.execute(\"create table items(value text)\")\n",
        "db.execute(\"insert into items values (\\\"ready\\\")\")\n",
        "db.commit()\n",
        "assert db.execute(\"select value from items\").fetchone()[0] == \"ready\"\n",
        "db.close(); fcntl.flock(lock, fcntl.LOCK_UN); lock.close()'"
    ));
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "sqlite result: {result}");
    session.abort(None);
    assert!(!project.path().join("state.db").exists());
    assert!(!project.path().join("lockfile").exists());
}

#[test]
fn configured_storage_quota_rejects_before_mutation() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("large.bin"), vec![b'A'; 32 * 1024]).unwrap();
    let policy = project.path().join("policy.toml");
    std::fs::write(
        &policy,
        concat!(
            "[filesystem]\nread = [\"/\"]\nwrite = [\".\"]\ndeny = []\n\n",
            "[limits]\nmax_write_bytes = 1048576\nmax_storage_bytes = 8192\n"
        ),
    )
    .unwrap();
    let session = Session::start_with_policy(project.path(), Some(&policy));

    let output =
        session.exec_shell("python3 -c 'open(\"large.bin\", \"r+b\").write(b\"B\" * 16384)'");
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(result["exit_code"], 0);
    assert_eq!(
        std::fs::read(project.path().join("large.bin")).unwrap(),
        vec![b'A'; 32 * 1024]
    );
    session.write("small.txt", "within quota");
    session.abort(None);
    assert!(!project.path().join("small.txt").exists());
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
fn state_dir_inside_the_project_is_refused() {
    if !available() {
        return;
    }
    // A state base inside the project used to make copy-private fallback
    // clone its own state dir recursively until ENAMETOOLONG; admission now
    // refuses the layout outright, before any session artifacts exist.
    let project = tempfile::tempdir().unwrap();
    let state = project.path().join("state-inside-project");
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["session", "start", project.path().to_str().unwrap()])
        .env("DELTABOX_STATE_DIR", &state)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must live outside the project"),
        "{output:?}"
    );
    assert!(
        !state.exists() || std::fs::read_dir(&state).unwrap().next().is_none(),
        "refusal must not leave session artifacts behind"
    );
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

fn kill_helper(status: &Value) {
    let pid = status["helper_pid"].as_u64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
}

// docs/protocol.md: session.sync requires cbcow-private. A through session
// must be rejected outright -- a silent empty result would mask client bugs.
#[test]
fn sync_rejects_through_sessions() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let session = Session::start(project.path());
    let sync = session.command(["sync", &session.id]);
    assert!(!sync.status.success());
    assert!(
        String::from_utf8_lossy(&sync.stderr).contains("requires a cbcow-private session"),
        "{sync:?}"
    );
    let stop = session.command(["session", "stop", &session.id]);
    assert!(stop.status.success());
}

/// A per-command `read-write` profile widens host access without relaxing the
/// session's protected denies: the workspace still routes through DeltaFS and
/// the backing project stays unwritable.
#[test]
fn read_write_profile_widens_host_access_but_keeps_protected_denies() {
    if !available() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    let start = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args([
            "session",
            "start",
            project.path().to_str().unwrap(),
            "--fs-mode",
            "cbcow-through",
            "--require-fs-mode",
        ])
        .current_dir(project.path())
        .env("DELTABOX_STATE_DIR", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert_ok(&start);
    let session = String::from_utf8(start.stdout).unwrap().trim().to_string();

    let exec = |script: &str, profile: Option<&str>| -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command
            .args(["exec", &session, "--project"])
            .arg(project.path());
        if let Some(profile) = profile {
            command.args(["--execution-profile", profile]);
        }
        let output = command
            .args(["--", "sh", "-c", script])
            .current_dir(project.path())
            .env("DELTABOX_STATE_DIR", state.path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .output()
            .unwrap();
        // `exec` reports the command's status in its JSON body and the CLI
        // itself exits 0, so the result has to be parsed rather than inferred
        // from the process status.
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("exec did not return JSON: {error}"))
    };

    let escaped = outside.path().join("escaped.txt");
    let escape_script = format!("printf widened > '{}'", escaped.display());
    let denied = exec(&escape_script, None);
    assert!(
        denied["exit_code"].as_i64().unwrap_or(0) != 0,
        "workspace-write must not write outside the workspace: {denied}"
    );
    assert!(!escaped.exists(), "workspace-write created a host file");

    let widened = exec(&escape_script, Some("read-write"));
    assert_eq!(
        widened["exit_code"], 0,
        "read-write must reach host paths: {widened}"
    );
    assert_eq!(std::fs::read_to_string(&escaped).unwrap(), "widened");

    let tracked = exec("printf inside > tracked.txt", Some("read-write"));
    assert_eq!(tracked["exit_code"], 0, "workspace write: {tracked}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "inside"
    );

    let direct = project.path().join("direct.txt");
    let bypass = format!("printf hacked > '{}'", direct.display());
    let blocked = exec(&bypass, Some("read-write"));
    assert!(
        blocked["exit_code"].as_i64().unwrap_or(0) != 0,
        "the backing project must stay denied under read-write: {blocked}"
    );
    assert!(!direct.exists(), "read-write bypassed the backing deny");

    // Scratch is where the command's HOME and TMPDIR point. The helper creates
    // that tree when the first command starts the mount, so writing to it has
    // to keep working on the first command and on every later one.
    let scratch = "printf home > \"$HOME/probe\" && printf tmp > \"$TMPDIR/probe\"";
    for round in ["first", "second"] {
        let written = exec(scratch, Some("read-write"));
        assert_eq!(
            written["exit_code"], 0,
            "the {round} command must be able to write its HOME and TMPDIR: {written}"
        );
    }
    let home_probe = exec("cat \"$HOME/probe\" \"$TMPDIR/probe\"", Some("read-write"));
    assert_eq!(
        home_probe["exit_code"], 0,
        "scratch must persist: {home_probe}"
    );
    assert_eq!(home_probe["stdout"], "hometmp");

    // The control plane is closed in both profiles: none of these paths may be
    // opened for writing or renamed away, because they are how the manager
    // decides the session is alive. Appending is the meaningful probe for the
    // three regular files; for the two sockets it fails whatever the sandbox
    // does, so the rename is what carries the guarantee there.
    let session_runtime = runtime.path().join("deltabox-lite").join(&session);
    for name in [
        "ready",
        "control.sock",
        "command.sock",
        "command.ready",
        "command.lock",
    ] {
        let target = session_runtime.join(name);
        let moved = session_runtime.join(format!("{name}.moved"));
        assert!(
            target.exists(),
            "{} should exist while running",
            target.display()
        );
        for (label, profile) in [
            ("workspace-write", None),
            ("read-write", Some("read-write")),
        ] {
            let write = exec(&format!("printf x >> '{}'", target.display()), profile);
            assert!(
                write["exit_code"].as_i64().unwrap_or(0) != 0,
                "{label} must not write {}: {write}",
                target.display()
            );
            let rename = exec(
                &format!("mv '{}' '{}'", target.display(), moved.display()),
                profile,
            );
            assert!(
                rename["exit_code"].as_i64().unwrap_or(0) != 0,
                "{label} must not rename {}: {rename}",
                target.display()
            );
            assert!(
                target.exists() && !moved.exists(),
                "{label} moved {}",
                target.display()
            );
            // Reading is what the readiness marker was exposed to before the
            // deny existed, and only under `workspace-write`: `read-write`
            // grants the whole root, while `workspace-write` grants `/tmp`
            // read access, which covers a runtime directory that fell back to
            // `/tmp`. The sockets do not carry that guarantee — opening one
            // fails short of the deny — so this probe is for the plain files.
            if !name.ends_with(".sock") {
                let read = exec(&format!("cat '{}'", target.display()), profile);
                assert!(
                    read["exit_code"].as_i64().unwrap_or(0) != 0,
                    "{label} must not read {}: {read}",
                    target.display()
                );
            }
        }
    }

    let abort = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["abort", &session, "--project"])
        .arg(project.path())
        .current_dir(project.path())
        .env("DELTABOX_STATE_DIR", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert_ok(&abort);
    assert!(
        !project.path().join("tracked.txt").exists(),
        "abort must still roll back workspace writes made under read-write"
    );

    let stop = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["session", "stop", &session, "--project"])
        .arg(project.path())
        .current_dir(project.path())
        .env("DELTABOX_STATE_DIR", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert_ok(&stop);
}

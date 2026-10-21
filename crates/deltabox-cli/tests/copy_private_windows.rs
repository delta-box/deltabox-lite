#![cfg(windows)]
//! Windows copy-private through the CLI: session lifecycle, checkpoints,
//! commit/abort, deny policy, reparse points and crash recovery. Nothing here
//! runs a sandboxed command, so every test works on any Windows host without
//! WinFsp or MXC (building still needs the WinFsp SDK headers). Sandboxed
//! command execution is in `windows_mxc.rs`, network policy in
//! `network_approvals.rs`.

use serde_json::Value;
use std::{fs, path::{Path, PathBuf}, process::{Command, Output}};

struct Session { root: tempfile::TempDir, project: PathBuf, id: String }
impl Session {
    fn start(files: &[(&str, &str)]) -> Self {
        Self::start_with_policy(files, None)
    }
    /// `policy` is TOML written beside the project; None uses the default agent policy.
    fn start_with_policy(files: &[(&str, &str)], policy: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project with spaces");
        fs::create_dir(&project).unwrap();
        for (name, contents) in files {
            let path = project.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let mut session = Session { root, project, id: String::new() };
        let mut args = vec!["session", "start", session.project.to_str().unwrap(), "--fs-mode", "copy-private", "--require-fs-mode", "--json"];
        let policy_path = session.root.path().join("policy.toml");
        if let Some(policy) = policy {
            fs::write(&policy_path, policy).unwrap();
            args.extend(["--policy", policy_path.to_str().unwrap()]);
        }
        let info = session.json(&args);
        session.id = info["session_id"].as_str().unwrap().into();
        session
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        c.env("DELTABOX_STATE_DIR", self.root.path().join("state"))
            .env("DELTABOX_RUNTIME_DIR", self.root.path().join("runtime"));
        c
    }
    fn raw(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    /// Run an action with `DELTABOX_TEST_CRASH_POINT` set; debug builds exit with 86 there.
    fn crash(&self, action: &[&str], extra: &[&str], point: &str) {
        let out = self.command().env("DELTABOX_TEST_CRASH_POINT", point).args(self.args(action, extra)).output().unwrap();
        assert_eq!(out.status.code(), Some(86), "{action:?} did not stop at {point}: {}", String::from_utf8_lossy(&out.stderr));
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.raw(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn args<'a>(&'a self, action: &'a [&'a str], extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = action.to_vec();
        args.extend([self.id.as_str(), "--project", self.project.to_str().unwrap()]);
        args.extend_from_slice(extra);
        args
    }
    fn action(&self, action: &[&str], extra: &[&str]) -> Value { self.json(&self.args(action, extra)) }
    fn failure(&self, action: &[&str], extra: &[&str]) -> String {
        let out = self.raw(&self.args(action, extra));
        assert!(!out.status.success(), "{action:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn write(&self, name: &str, contents: &str) {
        let out = self.raw(&self.args(&["write"], &[name, "--content", contents]));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    fn read(&self, name: &str) -> String {
        let out = self.raw(&self.args(&["read"], &[name]));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn pending(&self) -> u64 { self.action(&["diff"], &["--json"])["pending_changes"].as_u64().unwrap() }
    fn host(&self, name: &str) -> Option<String> { fs::read_to_string(self.project.join(name)).ok() }
    fn live(&self) -> PathBuf { self.root.path().join("runtime").join("live").join(&self.id) }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.id.is_empty() {
            let _ = self.raw(&self.args(&["session", "stop"], &[]));
        }
    }
}

fn read_only(path: &Path) { let mut p = fs::metadata(path).unwrap().permissions(); p.set_readonly(true); fs::set_permissions(path, p).unwrap(); }

/// Make `link` a junction to `target`; no privilege is needed for junctions.
fn junction(link: &Path, target: &Path) {
    let status = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).status().unwrap();
    assert!(status.success(), "mklink /J failed");
}

/// The 8.3 short name of `name` inside `dir`, or None when the volume does
/// not create short names (then no alias exists to test).
fn short_name(dir: &Path, name: &str) -> Option<String> {
    let script = format!("(New-Object -ComObject Scripting.FileSystemObject).GetFile('{}').ShortName", dir.join(name).display());
    let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output().unwrap();
    let short = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!short.is_empty() && !short.eq_ignore_ascii_case(name)).then_some(short)
}

/// Set a directory's creation time (seconds after the Unix epoch).
fn set_created(path: &Path, seconds: u64) {
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    // FILE_WRITE_ATTRIBUTES, and FILE_FLAG_BACKUP_SEMANTICS to open a directory.
    let directory = fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(path).unwrap();
    let created = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
    directory.set_times(fs::FileTimes::new().set_created(created)).unwrap();
}

#[test]
fn start_reports_the_copy_private_contract_and_leaves_the_host_alone() {
    let s = Session::start(&[("a.txt", "v1")]);
    let info = s.action(&["session", "status"], &[]);
    assert_eq!(info["resolved_fs_mode"], "copy-private");
    assert_eq!(info["host_visibility"], "on-commit");
    assert_eq!(info["durability"], "process-crash");
    assert_eq!(info["pending_changes"], 0);
    assert_eq!(fs::read_to_string(s.live().join("a.txt")).unwrap(), "v1");

    let out = s.raw(&["session", "start", s.project.to_str().unwrap(), "--fs-mode", "copy-private", "--durable"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("strict durability requires a CB-CoW filesystem mode"));
}

#[test]
fn writes_stay_private_until_commit() {
    let s = Session::start(&[("a.txt", "v1"), ("dir/keep.txt", "keep")]);
    s.write("a.txt", "v2");
    s.write("dir/new.txt", "new");
    assert_eq!(s.host("a.txt").as_deref(), Some("v1"));
    assert_eq!(s.host("dir/new.txt"), None);
    assert_eq!(s.pending(), 2);

    let diff = s.action(&["diff"], &["--json"]);
    assert!(diff["changes"].as_array().unwrap().iter().any(|c| c["path"] == "dir/new.txt" && c["kind"] == "added"));

    let result = s.action(&["commit"], &[]);
    assert_eq!(result["applied"], 2);
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
    assert_eq!(s.host("dir/new.txt").as_deref(), Some("new"));
    assert_eq!(s.host("dir/keep.txt").as_deref(), Some("keep"));
    assert_eq!(s.pending(), 0);

    // A second round publishes a deletion too.
    fs::remove_file(s.live().join("dir/keep.txt")).unwrap();
    assert_eq!(s.pending(), 1);
    s.action(&["commit"], &[]);
    assert_eq!(s.host("dir/keep.txt"), None);
}

#[test]
fn abort_restores_the_accepted_tree() {
    let s = Session::start(&[("a.txt", "v1"), ("gone.txt", "restore me")]);
    s.write("a.txt", "v2");
    s.write("b.txt", "added");
    fs::remove_file(s.live().join("gone.txt")).unwrap();
    assert_eq!(s.pending(), 3);
    let result = s.action(&["abort"], &[]);
    assert_eq!(result["checkpoint_id"], "accepted");
    assert_eq!(s.read("a.txt"), "v1");
    assert_eq!(s.read("gone.txt"), "restore me");
    assert!(!s.live().join("b.txt").exists());
    assert_eq!(s.pending(), 0);
    assert_eq!(s.host("a.txt").as_deref(), Some("v1"));
}

#[test]
fn read_only_files_are_tracked_restored_and_writable() {
    let s = Session::start(&[("ro.txt", "read only")]);
    read_only(&s.live().join("ro.txt"));
    assert_eq!(s.pending(), 1, "an attribute change is a change");
    s.action(&["abort"], &[]);
    assert!(!fs::metadata(s.live().join("ro.txt")).unwrap().permissions().readonly());
    assert_eq!(s.pending(), 0);

    read_only(&s.live().join("ro.txt"));
    s.write("ro.txt", "replaced");
    assert_eq!(s.read("ro.txt"), "replaced");
}

#[test]
fn checkpoints_work_on_a_clean_tree_and_after_only_deletions() {
    let s = Session::start(&[("a.txt", "v1"), ("b.txt", "b")]);
    let clean = s.action(&["checkpoint"], &[]);
    assert_eq!(clean["change_count"], 0);
    fs::remove_file(s.live().join("b.txt")).unwrap();
    let deleted = s.action(&["checkpoint"], &[]);
    assert_eq!(deleted["change_count"], 1);
    s.write("c.txt", "c");
    let id = clean["id"].as_str().unwrap();
    assert_eq!(s.action(&["diff"], &["--json", "--from", id])["pending_changes"], 2);
    s.action(&["abort"], &["--checkpoint", deleted["id"].as_str().unwrap()]);
    assert!(!s.live().join("b.txt").exists());
    assert!(!s.live().join("c.txt").exists());
    s.action(&["abort"], &["--checkpoint", id]);
    assert_eq!(s.read("b.txt"), "b");
}

#[test]
fn checkpoints_restore_and_survive_a_commit() {
    let s = Session::start(&[("a.txt", "a1"), ("b.txt", "b1")]);
    s.write("a.txt", "a2");
    let checkpoint = s.action(&["checkpoint"], &["--label", "first"]);
    let id = checkpoint["id"].as_str().unwrap().to_owned();
    assert_eq!(checkpoint["change_count"], 1);

    s.write("a.txt", "a3");
    s.write("c.txt", "c");
    let since = s.action(&["diff"], &["--json", "--from", &id]);
    assert_eq!(since["pending_changes"], 2);

    // b.txt is unchanged at the checkpoint, so the checkpoint reads it from
    // the accepted copy. Committing b2 must not change what the checkpoint holds.
    s.write("b.txt", "b2");
    s.action(&["commit"], &[]);
    assert_eq!(s.host("b.txt").as_deref(), Some("b2"));

    let result = s.action(&["abort"], &["--checkpoint", &id]);
    assert_eq!(result["checkpoint_id"], id.as_str());
    assert_eq!(s.read("a.txt"), "a2");
    assert_eq!(s.read("b.txt"), "b1");
    assert!(!s.live().join("c.txt").exists());

    let missing = s.failure(&["abort"], &["--checkpoint", "00000000-0000-0000-0000-000000000000"]);
    assert!(missing.contains("checkpoint not found"), "{missing}");
}

#[test]
fn commit_rejects_any_external_host_change() {
    let s = Session::start(&[("a.txt", "v1")]);
    s.write("pending.txt", "pending");
    fs::write(s.project.join("a.txt"), "external change").unwrap();
    let error = s.failure(&["commit"], &[]);
    assert!(error.contains("project changed outside deltabox"), "{error}");
    assert_eq!(s.host("pending.txt"), None);
    assert_eq!(s.pending(), 1);
}

#[test]
fn denied_files_never_enter_the_private_copy() {
    let s = Session::start(&[("a.txt", "v1"), (".env", "secret"), ("config/id_rsa.pem", "key")]);
    assert!(!s.live().join(".env").exists());
    assert!(!s.live().join("config/id_rsa.pem").exists());
    assert!(s.failure(&["read"], &[".env"]).contains("path denied by policy"));
    assert!(s.failure(&["write"], &[".ENV", "--content", "x"]).contains("path denied by policy"));
    assert_eq!(s.pending(), 0);

    // Committing other work leaves the host's secrets in place.
    s.write("a.txt", "v2");
    s.action(&["commit"], &[]);
    assert_eq!(s.host(".env").as_deref(), Some("secret"));
    assert_eq!(s.host("config/id_rsa.pem").as_deref(), Some("key"));
}

#[test]
fn shadow_git_is_copied_but_never_published() {
    let s = Session::start(&[("a.txt", "v1"), (".git/HEAD", "ref: refs/heads/main\n")]);
    assert!(s.live().join(".git/HEAD").exists());
    fs::write(s.live().join(".git/HEAD"), "ref: refs/heads/agent\n").unwrap();
    assert_eq!(s.pending(), 0);
    s.write("a.txt", "v2");
    s.action(&["commit"], &[]);
    assert_eq!(s.host(".git/HEAD").as_deref(), Some("ref: refs/heads/main\n"));
    // Commit resets the shadow .git to the host's.
    assert_eq!(fs::read_to_string(s.live().join(".git/HEAD")).unwrap(), "ref: refs/heads/main\n");
}

#[test]
fn skipped_review_keeps_the_earliest_scope_and_aborts_everything() {
    let s = Session::start(&[]);
    let first = s.action(&["review", "begin"], &["--kind", "task", "--label", "turn-start"]);
    s.write("first.txt", "one");
    s.action(&["review", "skip"], &[]);
    let second = s.action(&["review", "begin"], &["--kind", "tool"]);
    assert_eq!(first["checkpoint_id"], second["checkpoint_id"]);
    s.write("second.txt", "two");
    assert_eq!(s.action(&["review", "diff"], &["--json"])["pending_changes"], 2);
    s.action(&["review", "abort"], &[]);
    assert_eq!(s.pending(), 0);
    assert!(s.action(&["review", "status"], &[]).is_null());
}

#[test]
fn copy_private_rejects_sync_and_keeps_work_when_stopped() {
    let s = Session::start(&[("a.txt", "v1")]);
    assert!(s.failure(&["sync"], &[]).contains("session.sync requires a cbcow-private session"));
    s.write("a.txt", "v2");
    let stopped = s.action(&["session", "stop"], &[]);
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(fs::read_to_string(s.live().join("a.txt")).unwrap(), "v2");
    assert!(s.failure(&["write"], &["b.txt", "--content", "x"]).contains("session stopped"));
}

#[test]
fn hidden_host_files_survive_directory_changes() {
    let s = Session::start(&[("config/app.txt", "v1"), ("config/.env", "secret"), ("secrets/readme.txt", "r"), ("secrets/.env", "secret")]);
    // A directory whose own metadata changed (as when it is recreated): the
    // host's hidden .env inside it must not turn into a conflict, and must
    // survive. Set the creation time explicitly; NTFS tunneling can give a
    // quickly recreated directory its old one.
    fs::write(s.live().join("config/app.txt"), "v2").unwrap();
    set_created(&s.live().join("config"), 1_000_000_000);
    let diff = s.action(&["diff"], &["--json"]);
    assert!(diff["changes"].as_array().unwrap().iter().any(|c| c["path"] == "config" && c["kind"] == "modified"), "{diff}");
    s.action(&["commit"], &[]);
    assert_eq!(s.host("config/app.txt").as_deref(), Some("v2"));
    assert_eq!(s.host("config/.env").as_deref(), Some("secret"));

    // Removing a directory would also remove what the session cannot see.
    fs::remove_dir_all(s.live().join("secrets")).unwrap();
    let error = s.failure(&["commit"], &[]);
    assert!(error.contains("cannot remove secrets"), "{error}");
    assert_eq!(s.host("secrets/.env").as_deref(), Some("secret"));
    assert_eq!(s.host("secrets/readme.txt").as_deref(), Some("r"));
    assert!(s.pending() > 0);
}

#[test]
fn absolute_paths_with_either_separator_resolve_into_the_session() {
    let s = Session::start(&[("dir/a.txt", "v1")]);
    // Canonical (long) spelling; a temp dir may use an 8.3 short name.
    let canonical = fs::canonicalize(&s.project).unwrap();
    let canonical = canonical.to_string_lossy();
    let canonical = canonical.strip_prefix(r"\\?\").unwrap_or(&canonical);
    let forward = format!("{canonical}/dir/b.txt").replace('\\', "/");
    s.write(&forward, "via forward slashes");
    assert_eq!(s.read("dir/b.txt"), "via forward slashes");
    assert_eq!(s.host("dir/b.txt"), None);
    assert!(s.failure(&["write"], &["../outside.txt", "--content", "x"]).contains("path outside project"));
    assert!(s.failure(&["write"], &["CON", "--content", "x"]).contains("invalid Windows path component"));
}

#[test]
fn a_new_path_that_aliases_a_hidden_host_file_is_refused() {
    let s = Session::start(&[("a.txt", "v1"), (".env", "secret")]);
    // NTFS answers to the short name of `.env` (usually ENV~1). In the
    // private copy that name is free, because `.env` was never copied.
    let Some(alias) = short_name(&s.project, ".env") else {
        eprintln!("skipped: the volume does not create 8.3 names");
        return;
    };
    fs::write(s.live().join(&alias), "leak").unwrap();
    assert_eq!(s.pending(), 1);
    let error = s.failure(&["commit"], &[]);
    assert!(error.contains("under another name"), "{error}");
    assert_eq!(s.host(".env").as_deref(), Some("secret"));
    assert!(fs::read_dir(&s.project).unwrap().all(|e| e.unwrap().file_name() != alias.as_str()));
    // Other work still commits once the alias is gone.
    fs::remove_file(s.live().join(&alias)).unwrap();
    s.write("a.txt", "v2");
    s.action(&["commit"], &[]);
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
}

#[test]
fn a_junction_in_place_of_the_shadow_git_is_refused_and_never_walked() {
    let s = Session::start(&[("a.txt", "v1"), (".git/HEAD", "ref: refs/heads/main\n")]);
    s.write("a.txt", "v2");
    let outside = s.root.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("victim.txt"), "keep me").unwrap();
    fs::remove_dir_all(s.live().join(".git")).unwrap();
    junction(&s.live().join(".git"), &outside);

    // Nothing that could walk or write through the junction runs; reads of
    // ordinary files still work.
    for (action, extra) in [(vec!["commit"], vec![]), (vec!["checkpoint"], vec![]), (vec!["write"], vec!["a.txt", "--content", "v3"]), (vec!["read"], vec![".git/HEAD"])] {
        let error = s.failure(&action, &extra);
        assert!(error.contains("reparse points"), "{action:?}: {error}");
    }
    assert_eq!(s.read("a.txt"), "v2");
    assert_eq!(fs::read_to_string(outside.join("victim.txt")).unwrap(), "keep me");

    // Abort removes the junction itself and rebuilds `.git` from the accepted copy.
    s.action(&["abort"], &[]);
    assert_eq!(fs::read_to_string(outside.join("victim.txt")).unwrap(), "keep me");
    assert_eq!(fs::read_to_string(s.live().join(".git/HEAD")).unwrap(), "ref: refs/heads/main\n");
    assert_eq!(s.read("a.txt"), "v1");
}

#[test]
fn a_missing_working_copy_is_reported_until_abort_rebuilds_it() {
    let s = Session::start(&[("a.txt", "v1"), ("dir/b.txt", "b")]);
    s.write("a.txt", "v2");
    fs::remove_dir_all(s.live()).unwrap();
    // Nothing recreates the copy by accident, not even a write to a path the
    // scans skip; the next commit would otherwise publish every deletion.
    for (action, extra) in [(vec!["write"], vec![".git/config", "--content", "x"]), (vec!["commit"], vec![]), (vec!["session", "status"], vec![])] {
        let error = s.failure(&action, &extra);
        assert!(error.contains("copy-private tree is missing"), "{action:?}: {error}");
        assert!(!s.live().exists(), "{action:?} recreated the working copy");
    }
    assert_eq!(s.host("a.txt").as_deref(), Some("v1"));
    s.action(&["abort"], &[]);
    assert_eq!(s.read("a.txt"), "v1");
    assert_eq!(s.read("dir/b.txt"), "b");
    assert_eq!(s.pending(), 0);
}

#[test]
fn a_denied_path_recreated_with_the_host_content_is_tolerated() {
    // `.env.example` matches the default `.env.*` pattern but is a tracked
    // file in many repositories, which git restore commands recreate.
    let s = Session::start(&[("a.txt", "v1"), (".env.example", "KEY=\n")]);
    assert!(!s.live().join(".env.example").exists());
    fs::write(s.live().join(".env.example"), "KEY=\n").unwrap();
    assert_eq!(s.pending(), 0, "an identical denied file is not a change");
    s.write("a.txt", "v2");
    s.action(&["commit"], &[]);
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
    assert_eq!(s.host(".env.example").as_deref(), Some("KEY=\n"));

    fs::write(s.live().join(".env.example"), "KEY=changed\n").unwrap();
    assert_eq!(s.pending(), 0, "denied paths never show in a diff");
    assert!(s.failure(&["commit"], &[]).contains("path denied by policy: .env.example"));
    assert_eq!(s.host(".env.example").as_deref(), Some("KEY=\n"));
    s.action(&["abort"], &[]);
    assert!(!s.live().join(".env.example").exists(), "abort removes the stray denied file");
}

#[test]
fn a_deny_pattern_with_a_slash_hides_the_whole_directory() {
    // `config/secrets` matches only the directory itself; its children are
    // hidden because their parent is, in the copy and in every check alike.
    let policy = "[filesystem]\nread = [\".\"]\nwrite = [\".\"]\ndeny = [\"config/secrets\"]\n\n[network]\nmode = \"allow_audit\"\n";
    let s = Session::start_with_policy(&[("a.txt", "v1"), ("config/app.toml", "x"), ("config/secrets/token", "t")], Some(policy));
    assert!(!s.live().join("config/secrets").exists());
    assert!(s.live().join("config/app.toml").exists());
    assert!(s.failure(&["read"], &["config/secrets/token"]).contains("path denied by policy"));
    fs::create_dir_all(s.live().join("config/secrets")).unwrap();
    fs::write(s.live().join("config/secrets/leak"), "x").unwrap();
    assert_eq!(s.pending(), 0);
    assert!(s.failure(&["commit"], &[]).contains("path denied by policy"));
    assert_eq!(s.host("config/secrets/token").as_deref(), Some("t"));
    assert_eq!(s.host("config/secrets/leak"), None);
}

#[test]
fn commit_validates_the_checkpoint_it_is_given() {
    let s = Session::start(&[("a.txt", "v1")]);
    assert!(s.failure(&["commit"], &["--checkpoint", "00000000-0000-0000-0000-000000000000"]).contains("checkpoint not found"));
    let first = s.action(&["checkpoint"], &[]);
    let latest = s.action(&["checkpoint"], &[]);
    let error = s.failure(&["commit"], &["--checkpoint", first["id"].as_str().unwrap()]);
    assert!(error.contains("latest checkpoint only"), "{error}");
    s.write("a.txt", "v2");
    let result = s.action(&["commit"], &["--checkpoint", latest["id"].as_str().unwrap()]);
    assert_eq!(result["checkpoint_id"], latest["id"]);
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
}

#[test]
fn start_refuses_reparse_points_and_leaves_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(project.join("real")).unwrap();
    junction(&project.join("link"), &project.join("real"));
    let out = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .env("DELTABOX_STATE_DIR", root.path().join("state"))
        .env("DELTABOX_RUNTIME_DIR", root.path().join("runtime"))
        .args(["session", "start", project.to_str().unwrap(), "--fs-mode", "copy-private", "--require-fs-mode"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reparse points"));
    let live = root.path().join("runtime").join("live");
    assert!(!live.exists() || fs::read_dir(&live).unwrap().next().is_none(), "a working copy was left behind");
}

#[test]
#[cfg(debug_assertions)] // the crash hooks exist only in debug builds
fn a_commit_interrupted_before_the_host_decision_rolls_the_host_back() {
    let s = Session::start(&[("a.txt", "v1")]);
    s.write("a.txt", "v2");
    s.crash(&["commit"], &[], "private_publish_path_applied");
    // The crash point sits after the host write, so the host really was changed.
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
    // The next operation lets the publication's own journal undo the host.
    assert_eq!(s.pending(), 1);
    assert_eq!(s.host("a.txt").as_deref(), Some("v1"));
    s.action(&["commit"], &[]);
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
}

#[test]
#[cfg(debug_assertions)] // the crash hooks exist only in debug builds
fn a_commit_interrupted_after_the_host_decision_finishes_on_the_next_operation() {
    let s = Session::start(&[("a.txt", "v1"), ("b.txt", "b1")]);
    let checkpoint = s.action(&["checkpoint"], &[]);
    s.write("a.txt", "v2");
    s.crash(&["commit"], &[], "copy_private_commit_published");
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
    assert_eq!(s.pending(), 0, "the accepted copy caught up with the host");
    // The checkpoint still holds the old content after recovery.
    s.action(&["abort"], &["--checkpoint", checkpoint["id"].as_str().unwrap()]);
    assert_eq!(s.read("a.txt"), "v1");
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
}

#[test]
#[cfg(debug_assertions)] // the crash hooks exist only in debug builds
fn an_interrupted_abort_finishes_and_closes_the_review_scope() {
    let s = Session::start(&[("a.txt", "v1")]);
    s.action(&["review", "begin"], &["--kind", "task"]);
    s.write("a.txt", "v2");
    s.crash(&["review", "abort"], &[], "copy_private_restore_marked");
    // The crash point sits before the restore, so recovery has to do it.
    assert_eq!(fs::read_to_string(s.live().join("a.txt")).unwrap(), "v2");
    assert_eq!(s.pending(), 0);
    assert_eq!(s.read("a.txt"), "v1");
    assert!(s.action(&["review", "status"], &[]).is_null());
}

#[test]
fn a_lost_checkpoint_is_dropped_instead_of_breaking_the_session() {
    let s = Session::start(&[("a.txt", "v1")]);
    let lost = s.action(&["checkpoint"], &[]);
    let lost = lost["id"].as_str().unwrap();
    let project_state = fs::read_dir(s.root.path().join("state/projects")).unwrap().next().unwrap().unwrap().path();
    fs::remove_dir_all(project_state.join(&s.id).join("checkpoints").join(lost)).unwrap();
    assert_eq!(s.action(&["session", "status"], &[])["checkpoint_count"], 0);
    assert!(s.failure(&["abort"], &["--checkpoint", lost]).contains("checkpoint not found"));
    s.write("a.txt", "v2");
    s.action(&["commit"], &[]);
    assert_eq!(s.host("a.txt").as_deref(), Some("v2"));
}

#[test]
fn sync_draft_names_are_refused() {
    let s = Session::start(&[]);
    let error = s.failure(&["write"], &["notes.deltabox-theirs", "--content", "x"]);
    assert!(error.contains("reserved for cbcow-private sync drafts"), "{error}");
    fs::write(s.live().join("other.deltabox-theirs"), "x").unwrap();
    assert!(s.failure(&["commit"], &[]).contains("reserved for cbcow-private sync drafts"));
}

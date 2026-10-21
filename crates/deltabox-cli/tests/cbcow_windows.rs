#![cfg(windows)]
//! Windows `cbcow-through` and `cbcow-private` sessions through the CLI: every
//! command-line operation (start, write, checkpoint, diff, commit, abort,
//! sync, stop) plus helper restarts and sibling sessions, in the same shape as
//! `cbcow_linux.rs`: one `Session` helper, one behavior per test.
//!
//! These need an installed WinFsp (the CI Windows job installs it) and, for
//! the PowerShell commands, native MXC PSEC. What the sandbox forbids is in
//! `windows_mxc.rs`; network policy in `network_approvals.rs`; copy-private in
//! `copy_private_windows.rs`.
use base64::Engine as _;
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    rc::Rc,
};

struct Session {
    root: Rc<tempfile::TempDir>,
    project: PathBuf,
    id: String,
}

impl Session {
    /// Start a session in `mode` over a fresh project holding `files`.
    /// Returns the session and the `session start` report.
    fn start(mode: &str, durable: bool, files: &[(&str, &str)]) -> (Self, Value) {
        let root = Rc::new(tempfile::tempdir().unwrap());
        let project = root.path().join("project with spaces");
        fs::create_dir(&project).unwrap();
        for (name, contents) in files {
            let path = project.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        Self::start_in(root, project, mode, durable)
    }

    /// Start another session on the same project and state.
    fn start_sibling(&self, mode: &str, durable: bool) -> (Self, Value) {
        Self::start_in(self.root.clone(), self.project.clone(), mode, durable)
    }

    fn start_in(root: Rc<tempfile::TempDir>, project: PathBuf, mode: &str, durable: bool) -> (Self, Value) {
        let mut session = Session { root, project, id: String::new() };
        let mut args = vec!["session", "start", session.project.to_str().unwrap(), "--fs-mode", mode, "--require-fs-mode", "--json"];
        if durable {
            args.push("--durable");
        }
        let info = session.json(&args);
        session.id = info["session_id"].as_str().unwrap().into();
        (session, info)
    }

    fn raw(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .env("DELTABOX_STATE_DIR", self.root.path().join("state"))
            .env("DELTABOX_RUNTIME_DIR", self.root.path().join("runtime"))
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.raw(args);
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// `deltabox <action> <id> --project <project> <extra...>`
    fn args<'a>(&'a self, action: &'a [&'a str], extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = action.to_vec();
        args.extend([self.id.as_str(), "--project", self.project.to_str().unwrap()]);
        args.extend_from_slice(extra);
        args
    }

    fn action(&self, action: &[&str], extra: &[&str]) -> Value {
        self.json(&self.args(action, extra))
    }

    /// Stderr of an action that must fail.
    fn failure(&self, action: &[&str], extra: &[&str]) -> String {
        let output = self.raw(&self.args(action, extra));
        assert!(!output.status.success(), "{action:?} unexpectedly succeeded");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn status(&self) -> Value {
        self.action(&["session", "status"], &[])
    }

    fn write(&self, path: &str, content: &str) {
        assert_ok(&self.raw(&self.args(&["write"], &[path, "--content", content])));
    }

    fn checkpoint(&self, label: &str) -> String {
        self.action(&["checkpoint"], &["--label", label])["id"].as_str().unwrap().into()
    }

    fn commit(&self) {
        self.action(&["commit"], &[]);
    }

    fn abort(&self, checkpoint: Option<&str>) {
        match checkpoint {
            Some(id) => self.action(&["abort"], &["--checkpoint", id]),
            None => self.action(&["abort"], &[]),
        };
    }

    fn pending(&self) -> u64 {
        self.action(&["diff"], &["--json"])["pending_changes"].as_u64().unwrap()
    }

    /// Run a PowerShell script in the sandbox; it must exit 0.
    fn powershell(&self, body: &str) -> Value {
        let script = format!("$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[Text.Encoding]::UTF8; {body}");
        let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let value = self.action(&["exec"], &["--", shell.to_str().unwrap(), "-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
        assert_eq!(value["exit_code"], 0, "{body}: {value}");
        value
    }

    fn host(&self, name: &str) -> Vec<u8> {
        fs::read(self.project.join(name)).unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.id.is_empty() {
            let _ = self.raw(&self.args(&["session", "stop"], &[]));
        }
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Kill the session's helper process; the next CLI call must restart it.
fn kill_helper(status: &Value) {
    let pid = status["helper_pid"].as_u64().unwrap().to_string();
    let killed = Command::new("taskkill.exe").args(["/F", "/PID", &pid]).output().unwrap();
    assert_ok(&killed);
}

// ---------------------------------------------------------------------------
// cbcow-through: the view is the real project, so changes are host-visible at once
// ---------------------------------------------------------------------------

#[test]
fn changes_and_history_cross_cli_processes() {
    let (s, info) = Session::start("cbcow-through", false, &[("README.md", "base\n")]);
    assert_eq!(info["host_visibility"], "immediate");
    assert_eq!(info["state_provider"], "windows-winfsp");

    s.write("created.txt", "created");
    assert_eq!(s.host("created.txt"), b"created");
    let checkpoint = s.checkpoint("saved");
    s.write("README.md", "changed\n");
    s.abort(Some(&checkpoint));
    assert_eq!(s.host("README.md"), b"base\n");
    assert!(s.project.join("created.txt").exists());
    assert_eq!(s.pending(), 1);

    s.commit();
    s.write("README.md", "pending\n");
    s.abort(None);
    assert_eq!(s.host("README.md"), b"base\n");
    assert!(s.project.join("created.txt").exists());
}

#[test]
fn powershell_file_operations_are_undone_by_abort() {
    let (s, _) = Session::start(
        "cbcow-through",
        false,
        &[
            ("a.txt", "abcdefgh"),
            ("deleted.txt", "restore me"),
            ("folder/child.txt", "child"),
            ("folder/existing/deeper/input.txt", "existing nested file"),
        ],
    );
    s.powershell(
        r#"
      if ((Get-Content -LiteralPath 'folder/existing/deeper/input.txt' -Raw) -ne 'existing nested file') {throw 'existing nested read failed'}
      Set-Content -LiteralPath 'folder/existing/deeper/new.txt' -Value 'existing nested write'
      if ((Get-Content -LiteralPath 'folder/existing/deeper/new.txt' -Raw).Trim() -ne 'existing nested write') {throw 'existing nested write failed'}
      $f=[IO.File]::Open('a.txt',[IO.FileMode]::Open,[IO.FileAccess]::Write,[IO.FileShare]::ReadWrite)
      try { $f.Position=2; $f.Write([Text.Encoding]::UTF8.GetBytes('XY'),0,2) } finally { $f.Dispose() }
      Set-Content -LiteralPath 'new.txt' -Value 'new'
      New-Item -ItemType Directory -Path 'sub' | Out-Null
      Set-Content -LiteralPath 'sub/中文.txt' -Value 'unicode'
      Move-Item -LiteralPath 'folder' -Destination 'moved'
      Remove-Item -LiteralPath 'deleted.txt'
      Rename-Item -LiteralPath 'a.txt' -NewName 'renamed.txt'
      if ((Get-Content -LiteralPath 'renamed.txt' -Raw) -ne 'abXYefgh') {throw 'unexpected range contents'}
    "#,
    );
    assert_eq!(s.host("renamed.txt"), b"abXYefgh");
    assert!(!s.project.join("deleted.txt").exists());
    assert!(s.pending() > 0);

    s.abort(None);
    assert_eq!(s.host("a.txt"), b"abcdefgh");
    assert_eq!(s.host("deleted.txt"), b"restore me");
    assert!(s.project.join("folder/child.txt").is_file());
    assert_eq!(s.host("folder/existing/deeper/input.txt"), b"existing nested file");
    assert!(!s.project.join("folder/existing/deeper/new.txt").exists());
    for name in ["new.txt", "sub", "moved", "renamed.txt"] {
        assert!(!s.project.join(name).exists(), "{name}");
    }
}

#[test]
fn abort_restores_a_checkpointed_file_after_truncate_and_append() {
    let (s, _) = Session::start("cbcow-through", false, &[("a.txt", "abcdefgh")]);
    s.write("a.txt", "checkpoint");
    let checkpoint = s.checkpoint("saved");
    s.powershell("$f=[IO.File]::OpenWrite('a.txt'); try {$f.SetLength(2)} finally {$f.Dispose()}; Add-Content -LiteralPath 'a.txt' -Value 'append'");
    s.abort(Some(&checkpoint));
    assert_eq!(s.host("a.txt"), b"checkpoint");
}

#[test]
fn helper_restart_preserves_pending_undo() {
    let (s, _) = Session::start("cbcow-through", false, &[("a.txt", "old")]);
    s.write(s.project.join("a.txt").to_str().unwrap(), "pending"); // an absolute project path works too
    let status = s.status();
    kill_helper(&status);
    let reopened = s.status();
    assert_ne!(reopened["helper_pid"], status["helper_pid"]);
    assert_eq!(s.host("a.txt"), b"pending");
    s.abort(None);
    assert_eq!(s.host("a.txt"), b"old");
}

#[test]
fn one_through_session_holds_the_project() {
    let (s, _) = Session::start("cbcow-through", false, &[("a.txt", "old")]);
    let error = s.raw(&["session", "start", s.project.to_str().unwrap(), "--fs-mode", "copy-private"]);
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("owns this project"));
    let error = s.raw(&["session", "start", s.project.to_str().unwrap(), "--durable"]);
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("strict durability"));
}

// ---------------------------------------------------------------------------
// cbcow-private: the view is a private live tree; commit publishes to the host
// ---------------------------------------------------------------------------

#[test]
fn private_session_reports_its_contract_and_keeps_writes_private() {
    let (s, info) = Session::start("cbcow-private", false, &[("a.txt", "old")]);
    assert_eq!(info["resolved_fs_mode"], "cbcow-private");
    assert_eq!(info["host_visibility"], "on-commit");
    assert_eq!(info["rollback_guarantee"], "private-live-tree");
    assert_eq!(info["durability"], "process-crash");

    let through = s.raw(&["session", "start", s.project.to_str().unwrap(), "--fs-mode", "cbcow-through"]);
    assert!(!through.status.success(), "a through session must not coexist with a private one");

    s.write("a.txt", "ours");
    let view = PathBuf::from(info["mountpoint"].as_str().unwrap());
    assert_eq!(fs::read(view.join("a.txt")).unwrap(), b"ours");
    assert_eq!(s.host("a.txt"), b"old");
}

#[test]
fn private_sessions_publish_disjoint_work_and_sync_conflicts() {
    private_siblings(false);
}

#[test]
fn strict_private_sessions_publish_disjoint_work_and_sync_conflicts() {
    private_siblings(true);
}

/// Two private sessions on one project: disjoint commits both land, an
/// overlapping one conflicts until `sync` brings in the sibling's content.
fn private_siblings(durable: bool) {
    let (ours, info) = Session::start("cbcow-private", durable, &[("a.txt", "old"), ("b.txt", "old")]);
    assert_eq!(info["durability"], if durable { "strict" } else { "process-crash" });
    let (sibling, _) = ours.start_sibling("cbcow-private", durable);
    let view = PathBuf::from(info["mountpoint"].as_str().unwrap());

    ours.write("a.txt", "ours");
    if durable {
        // Native namespace operations in the view must stay private as well.
        for script in [
            "[IO.File]::WriteAllText('native.txt','native strict')",
            "New-Item -ItemType Directory -Path 'native-dir' | Out-Null; [IO.File]::WriteAllText('native-dir/child.txt','child')",
            "Move-Item -LiteralPath 'native-dir' -Destination 'renamed-dir'",
            "Remove-Item -LiteralPath 'native.txt'",
            "[IO.File]::SetAttributes('a.txt',[IO.FileAttributes]::ReadOnly)",
            "[IO.File]::SetAttributes('a.txt',[IO.FileAttributes]::Normal)",
        ] {
            ours.powershell(script);
        }
        assert!(!ours.project.join("renamed-dir").exists());
    }
    assert_eq!(fs::read(view.join("a.txt")).unwrap(), b"ours");
    assert_eq!(ours.host("a.txt"), b"old");

    sibling.write("b.txt", "sibling");
    ours.commit();
    assert_eq!(sibling.action(&["commit"], &[])["folded_publications"], 1);
    assert_eq!(ours.host("a.txt"), b"ours");
    assert_eq!(ours.host("b.txt"), b"sibling");

    ours.write("b.txt", "overlap");
    assert!(ours.failure(&["commit"], &[]).contains("host paths changed"));
    let sync = ours.action(&["sync"], &[]);
    assert_eq!(sync["conflicts"][0]["path"], "b.txt");
    assert_eq!(fs::read(view.join("b.txt.deltabox-theirs")).unwrap(), b"sibling");
    ours.commit();
    assert_eq!(ours.host("b.txt"), b"overlap");
    assert!(!ours.project.join("b.txt.deltabox-theirs").exists());
}

#[test]
fn private_helper_restart_keeps_work_and_commit_ours_still_rejects_external_edits() {
    let (s, _) = Session::start("cbcow-private", false, &[("a.txt", "old")]);
    s.write("a.txt", "accepted");
    s.commit();
    s.write("a.txt", "pending");
    kill_helper(&s.status());
    s.abort(None);
    assert_eq!(s.host("a.txt"), b"accepted");
    let view = PathBuf::from(s.status()["mountpoint"].as_str().unwrap());
    assert_eq!(fs::read(view.join("a.txt")).unwrap(), b"accepted");

    // `--ours` may override recorded sibling content, never unrecognized external edits.
    fs::write(s.project.join("a.txt"), b"external").unwrap();
    s.write("a.txt", "should conflict");
    assert!(!s.raw(&s.args(&["commit"], &["--ours"])).status.success());
    s.abort(None);
}

#![cfg(windows)]
//! What the Microsoft MXC (PSEC) filesystem sandbox lets a command do, per
//! filesystem mode. Each test runs real commands in the sandbox and checks:
//!
//! * what they CAN do: read/write inside the session view, and
//! * what they CANNOT: read the host project, session state or denied files
//!   (`.env`, `.ssh`), or write outside the view.
//!
//! Needs native MXC PSEC filesystem-deny support, and WinFsp for the `cbcow-*`
//! modes (the CI Windows job installs WinFsp). Network policy is in
//! `network_approvals.rs`; lifecycle behavior (commit, abort, checkpoints,
//! recovery) is in `copy_private_windows.rs` and `cbcow_windows.rs`.
use base64::Engine as _;
use serde_json::Value;
use std::{fs, path::{Path, PathBuf}, process::{Command, Output}};

struct Session {
    root: tempfile::TempDir,
    project: PathBuf,
    id: String,
}

impl Session {
    /// Start a session in `mode` over a project holding `files`.
    fn start(mode: &str, files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project with spaces");
        fs::create_dir(&project).unwrap();
        for (name, contents) in files {
            let path = project.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let mut session = Session { root, project, id: String::new() };
        let info = session.json(&["session", "start", session.project.to_str().unwrap(), "--fs-mode", mode, "--require-fs-mode", "--json"]);
        session.id = info["session_id"].as_str().unwrap().into();
        session
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        c.env("DELTABOX_STATE_DIR", self.root.path().join("state"))
            .env("DELTABOX_RUNTIME_DIR", self.root.path().join("runtime"));
        c
    }

    fn raw(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.raw(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
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

    fn pending(&self) -> u64 {
        self.action(&["diff"], &["--json"])["pending_changes"].as_u64().unwrap()
    }

    fn host(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.project.join(name)).ok()
    }

    /// Where the private working copy lives, per mode.
    fn copy_private_live(&self) -> PathBuf {
        self.root.path().join("runtime").join("live").join(&self.id)
    }
    fn cbcow_private_live(&self) -> PathBuf {
        self.session_state().join("live")
    }
    fn session_state(&self) -> PathBuf {
        let projects = fs::read_dir(self.root.path().join("state/projects")).unwrap().next().unwrap().unwrap().path();
        projects.join(&self.id)
    }

    /// Run `body` through `cmd.exe` in the sandbox. cmd is used here rather
    /// than PowerShell because PowerShell fails to initialize (0xC0000142)
    /// under the sandbox in a non-interactive logon such as SSH.
    ///
    /// Programs started from this cmd must be written WITHOUT a path (`curl`,
    /// not `C:\Windows\System32\curl.exe`): with a path, cmd probes the parent
    /// directory first, and the sandbox forbids listing it ("Access denied").
    fn cmd(&self, body: &str) -> Output {
        let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe");
        self.raw(&self.args(&["exec"], &["--", shell.to_str().unwrap(), "/c", body]))
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
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.id.is_empty() {
            let _ = self.raw(&self.args(&["session", "stop"], &[]));
        }
    }
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// A PowerShell snippet that fails unless reading each path is denied.
fn expect_read_denied(paths: &[String]) -> String {
    format!(
        "foreach($p in @({})) {{ try {{[void][IO.File]::ReadAllBytes($p);throw \"unexpected read: $p\"}} catch [UnauthorizedAccessException] {{}} }}",
        paths.join(",")
    )
}

// ---------------------------------------------------------------------------
// copy-private: commands run in a plain private directory (no WinFsp)
// ---------------------------------------------------------------------------

/// CAN: read project files, create files in new and pre-existing directories
/// (the PSEC grant must reach directories copied in before launch).
/// CANNOT: leave a denied file behind; a command that creates one is undone
/// as a whole. A non-zero exit keeps its changes.
#[test]
fn copy_private_commands_run_in_the_private_copy_and_policy_violations_roll_back() {
    let s = Session::start("copy-private", &[("a.txt", "v1"), ("dir/existing.txt", "e"), (".env", "secret")]);

    let out = s.cmd("echo by command> made.txt & echo nested> dir\\deeper.txt & type a.txt");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["exit_code"], 0, "{result}");
    assert!(result["stdout"].as_str().unwrap().contains("v1"));
    assert_eq!(s.host("made.txt"), None, "the host project must stay untouched");
    assert_eq!(fs::read_to_string(s.copy_private_live().join("dir/deeper.txt")).unwrap().trim(), "nested");
    assert_eq!(s.pending(), 2);

    // A command that creates a denied file is undone as a whole.
    let out = s.cmd("echo x> other.txt & echo leak> .env");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("path denied by policy"));
    assert!(!s.copy_private_live().join("other.txt").exists());
    assert!(!s.copy_private_live().join(".env").exists());
    assert_eq!(s.pending(), 2);

    // A non-zero exit keeps its changes, as on Unix.
    let out = s.cmd("echo k> kept.txt & exit /b 3");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["exit_code"], 3);
    assert_eq!(s.pending(), 3);
}

/// A command started by the sandboxed cmd works when written without a path
/// and fails with "Access denied" when written with an absolute path.
#[test]
fn copy_private_cmd_starts_children_by_name_but_not_by_absolute_path() {
    let s = Session::start("copy-private", &[]);
    let run = |body: &str| -> Value { serde_json::from_slice(&s.cmd(body).stdout).unwrap() };

    let by_name = run("curl.exe --version");
    assert_eq!(by_name["exit_code"], 0, "{by_name}");
    assert!(by_name["stdout"].as_str().unwrap().starts_with("curl "));

    let by_path = run("C:\\Windows\\System32\\curl.exe --version");
    assert_ne!(by_path["exit_code"], 0, "an absolute-path child unexpectedly started: {by_path}");
}

// ---------------------------------------------------------------------------
// cbcow-through: WinFsp view over the real project
// ---------------------------------------------------------------------------

/// CAN: write inside the view (`allowed.txt` appears in the host project at once).
/// CANNOT: read the original project path, the session state, or denied files
/// (`.env`, `.ssh/id_rsa`); write outside the view and scratch space.
#[test]
fn cbcow_through_sandbox_denies_host_state_denied_files_and_outside_writes() {
    let s = Session::start("cbcow-through", &[("a.txt", "abcdefgh"), (".env", "secret"), (".ssh/id_rsa", "secret")]);
    let outside = s.root.path().join("outside.txt");
    let script = format!(
        "{} try {{[IO.File]::WriteAllText({},'escaped');throw 'unexpected outside write'}} catch [UnauthorizedAccessException] {{}}; Set-Content -LiteralPath 'allowed.txt' -Value 'inside'; Write-Output 'isolation-ok'",
        expect_read_denied(&[quoted(&s.project.join("a.txt")), quoted(&s.session_state().join("session.json")), "'.env'".into(), "'.ssh/id_rsa'".into()]),
        quoted(&outside),
    );
    assert!(s.powershell(&script)["stdout"].as_str().unwrap().contains("isolation-ok"));
    assert!(s.project.join("allowed.txt").exists());
    assert!(!outside.exists());
    s.action(&["abort"], &[]);
}

// ---------------------------------------------------------------------------
// cbcow-private: WinFsp view over a private live tree
// ---------------------------------------------------------------------------

/// CAN: write inside the view (stays private, host project untouched).
/// CANNOT: read the host project file or the private backing tree directly.
#[test]
fn cbcow_private_sandbox_denies_host_and_backing_tree() {
    let s = Session::start("cbcow-private", &[("a.txt", "old")]);
    let script = format!(
        "{} [IO.File]::WriteAllText('sandbox.txt','private')",
        expect_read_denied(&[quoted(&s.project.join("a.txt")), quoted(&s.cbcow_private_live().join("a.txt"))]),
    );
    s.powershell(&script);
    assert!(!s.project.join("sandbox.txt").exists(), "a private write reached the host project");
    s.action(&["abort"], &[]);
}

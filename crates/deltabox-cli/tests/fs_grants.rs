#![cfg(any(target_os = "linux", target_os = "macos", windows))]
//! Execution-scoped filesystem grants (`fs_read_allow` / `fs_write_allow`)
//! over `deltabox serve`, in every session mode, on Linux, macOS and Windows.
//!
//! An rwx grant is a directory tree: a command can create, write, rename and
//! delete files under it, including the atomic "write a temporary sibling and
//! rename it over the target" pattern. A grant on a file, or on a path that
//! does not exist yet, opens its directory (created first if missing).
//!
//! Every check looks at the real host disk, not just the exit code.
//!
//! On Windows these tests are ignored by default because hosted windows-2025
//! lacks usable PSEC; CI runs them on windows-11-arm with `--include-ignored`.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const MODES: [&str; 3] = ["copy-private", "cbcow-through", "cbcow-private"];

#[cfg(windows)]
const READ_ROOTS: &str = r#"["."]"#;
#[cfg(unix)]
const READ_ROOTS: &str = r#"[".", "/usr", "/bin", "/etc", "/lib", "/lib64", "/run"]"#;

fn policy(approval: bool) -> String {
    format!(
        "[filesystem]\nread = {READ_ROOTS}\nwrite = [\".\"]\ndeny = [\".env\", \".ssh\", \"*.pem\"]\nallow_approval = {approval}\n"
    )
}

/// The shell argv for one command line. Windows paths go to cmd unquoted:
/// argv quoting would turn embedded quotes into `\"`, which cmd rejects, so
/// the test directories must not contain spaces.
#[cfg(windows)]
fn shell(line: &str) -> Vec<String> {
    let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("cmd.exe");
    vec![cmd.to_string_lossy().into_owned(), "/c".into(), line.into()]
}
#[cfg(unix)]
fn shell(line: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), line.into()]
}

/// Shell snippets, one per platform.
struct Sh;
#[cfg(unix)]
impl Sh {
    fn write(text: &str, path: &Path) -> String {
        format!("printf {text} > '{}'", path.display())
    }
    fn read(path: &Path) -> String {
        format!("cat '{}'", path.display())
    }
    fn mkdir(path: &Path) -> String {
        format!("mkdir '{}'", path.display())
    }
    fn rename(from: &Path, to: &Path) -> String {
        format!("mv -f '{}' '{}'", from.display(), to.display())
    }
    fn remove(path: &Path) -> String {
        format!("rm '{}'", path.display())
    }
    fn and(a: &str, b: &str) -> String {
        format!("{a} && {b}")
    }
}
#[cfg(windows)]
impl Sh {
    fn write(text: &str, path: &Path) -> String {
        format!("echo {text}>{}", path.display())
    }
    fn read(path: &Path) -> String {
        format!("type {}", path.display())
    }
    fn mkdir(path: &Path) -> String {
        format!("mkdir {}", path.display())
    }
    fn rename(from: &Path, to: &Path) -> String {
        format!("move /y {} {}", from.display(), to.display())
    }
    fn remove(path: &Path) -> String {
        format!("del {}", path.display())
    }
    fn and(a: &str, b: &str) -> String {
        format!("{a} && {b}")
    }
}

/// A running `deltabox serve` process, driven with line-delimited JSON-RPC.
struct Server {
    root: tempfile::TempDir,
    project: PathBuf,
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn spawn() -> Self {
        // FSEvents reports canonical paths, so on macOS the session must not
        // live under the `/var/folders` alias of the system temp directory.
        let root = if cfg!(target_os = "macos") {
            tempfile::tempdir_in("/private/tmp").unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .arg("serve")
            .current_dir(&project)
            .env("DELTABOX_STATE_DIR", root.path().join("state"))
            .env("DELTABOX_RUNTIME_DIR", root.path().join("runtime"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { root, project, child, input: Some(input), output, next_id: 0 }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{}", json!({"id": self.next_id, "method": method, "params": params})).unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid RPC response {line:?}: {e}"))
    }

    fn start(&mut self, mode: &str, approval: bool) -> String {
        let path = self.root.path().join(format!("policy-{mode}-{approval}.toml"));
        std::fs::write(&path, policy(approval)).unwrap();
        let params = json!({"project": self.project, "policy": path, "fs_mode": mode, "require_fs_mode": true});
        let start = self.call("session.start", params);
        start["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{mode}: session.start failed: {start}"))
            .to_owned()
    }

    fn stop(&mut self, sid: &str) {
        let stopped = self.call("session.stop", json!({"session_id": sid}));
        assert!(stopped.get("error").is_none(), "{stopped}");
    }

    /// The raw `session.exec` response: `result` on success, `error` when the
    /// grants were rejected before anything ran.
    fn exec(&mut self, sid: &str, read: &[&Path], write: &[&Path], line: &str) -> Value {
        let mut params = json!({"session_id": sid, "argv": shell(line)});
        if !read.is_empty() {
            params["fs_read_allow"] = json!(read);
        }
        if !write.is_empty() {
            params["fs_write_allow"] = json!(write);
        }
        self.call("session.exec", params)
    }

    fn finish(mut self) {
        drop(self.input.take());
        assert!(self.child.wait().unwrap().success());
    }
}

/// A failed assertion must not leave `deltabox serve` and its sessions running:
/// on macOS a leftover cbcow session stalls the next one's FSEvents start.
impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Host ACLs for a failure message; Windows only.
fn acls(paths: &[&Path]) -> String {
    if !cfg!(windows) {
        return String::new();
    }
    paths
        .iter()
        .map(|path| {
            let out = Command::new("icacls").arg(path).output();
            out.map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
                .unwrap_or_else(|e| e.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn code(response: &Value) -> i64 {
    response["result"]["exit_code"].as_i64().unwrap_or(-1)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .trim()
        .to_owned()
}

/// A scratch tree outside the project: `out/existing.txt` = `old`, an
/// ungranted sibling `other/`, and nothing at `fresh/`. It lives under the
/// target directory rather than the system temp directory, because the Linux
/// sandbox always lets commands read `/tmp`.
fn scratch(test: &str, mode: &str) -> PathBuf {
    // One tree per test and mode: the tests run in parallel.
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("fs-grants-{test}-{mode}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("out")).unwrap();
    std::fs::create_dir_all(base.join("other")).unwrap();
    std::fs::write(base.join("out").join("existing.txt"), "old").unwrap();
    let base = dunce(std::fs::canonicalize(&base).unwrap());
    assert!(!base.to_string_lossy().contains(' '), "{}", base.display());
    base
}

/// `canonicalize` on Windows returns `\\?\C:\...`, which cmd cannot use.
fn dunce(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

#[test]
#[cfg_attr(windows, ignore = "requires native MXC PSEC; run explicitly on a supported Windows host")]
fn write_grants_can_create_rename_and_delete_files() {
    let mut server = Server::spawn();
    for mode in MODES {
        let base = scratch("create", mode);
        let out = base.join("out");
        let sid = server.start(mode, true);
        let ok = |response: &Value, what: &str| assert_eq!(code(response), 0, "{mode}: {what}: {response}");

        // 1. A new file in a granted directory.
        let r = server.exec(&sid, &[], &[&out], &Sh::write("hi", &out.join("new.txt")));
        ok(&r, "create a file in a granted directory");
        assert_eq!(read(&out.join("new.txt")), "hi", "{mode}");

        // 2. A grant on a file that does not exist yet opens its directory.
        let new2 = out.join("new2.txt");
        let r = server.exec(&sid, &[], &[&new2], &Sh::write("hi", &new2));
        ok(&r, "create the granted new file");
        assert_eq!(read(&new2), "hi", "{mode}");

        // 3. Missing directories above a new file are created, then granted.
        let fresh = base.join("fresh").join("cache");
        let r = server.exec(&sid, &[], &[&fresh.join("a.txt")], &Sh::write("hi", &fresh.join("a.txt")));
        ok(&r, "create a file in a directory that did not exist");
        assert_eq!(read(&fresh.join("a.txt")), "hi", "{mode}");

        // 4. New directories and files inside them.
        let sub = out.join("sub");
        let r = server.exec(&sid, &[], &[&out], &Sh::and(&Sh::mkdir(&sub), &Sh::write("x", &sub.join("b.txt"))));
        ok(&r, "mkdir and create inside it");
        assert_eq!(read(&sub.join("b.txt")), "x", "{mode}");

        // 5. Atomic replace through a temporary sibling, granted on the file:
        //    the write and the rename are separate execs so a failure says which.
        let existing = out.join("existing.txt");
        let tmp = out.join("existing.tmp");
        let r = server.exec(&sid, &[], &[&existing], &Sh::write("new", &tmp));
        ok(&r, "write a temporary sibling of a granted file");
        // On Windows both files already exist, so this needs the backend to
        // grant existing entries, not just the directory.
        let r = server.exec(&sid, &[], &[&existing], &Sh::rename(&tmp, &existing));
        assert_eq!(code(&r), 0, "{mode}: rename over a granted file: {r}\n{}", acls(&[&out, &existing, &tmp]));
        assert_eq!(read(&existing), "new", "{mode}");
        assert!(!tmp.exists(), "{mode}");

        // 6. Delete and rename.
        let renamed = out.join("renamed.txt");
        let r = server.exec(&sid, &[], &[&out], &Sh::and(&Sh::remove(&out.join("new.txt")), &Sh::rename(&existing, &renamed)));
        ok(&r, "delete and rename");
        assert!(!out.join("new.txt").exists(), "{mode}");
        assert_eq!(read(&renamed), "new", "{mode}");

        // 7. Read grant.
        let r = server.exec(&sid, &[&out], &[], &Sh::read(&renamed));
        ok(&r, "read a granted directory");
        assert!(r["result"]["stdout"].as_str().unwrap_or("").contains("new"), "{mode}: {r}");

        server.stop(&sid);
        std::fs::remove_dir_all(&base).unwrap();
    }
    server.finish();
}

#[test]
#[cfg_attr(windows, ignore = "requires native MXC PSEC; run explicitly on a supported Windows host")]
fn write_grants_stay_inside_their_directory() {
    let mut server = Server::spawn();
    for mode in MODES {
        let base = scratch("confine", mode);
        let out = base.join("out");
        let sid = server.start(mode, true);
        let denied = |response: &Value, what: &str| {
            assert!(response.get("error").is_none(), "{mode}: {what}: the exec itself failed: {response}");
            assert_ne!(code(response), 0, "{mode}: {what}: {response}");
        };
        let rejected = |response: &Value, what: &str| {
            assert!(response.get("error").is_some(), "{mode}: {what}: grant was accepted: {response}");
        };

        // 8. A sibling of the granted directory stays closed.
        let r = server.exec(&sid, &[], &[&out], &Sh::write("x", &base.join("other").join("x.txt")));
        denied(&r, "write to an ungranted sibling");
        assert!(!base.join("other").join("x.txt").exists(), "{mode}");

        // 9. Read access does not allow creating files.
        let r = server.exec(&sid, &[&out], &[], &Sh::write("x", &out.join("ro.txt")));
        denied(&r, "create under a read grant");
        assert!(!out.join("ro.txt").exists(), "{mode}");

        // 10. A grant whose directory is a secret path is refused, and nothing is created.
        let ssh = out.join(".ssh");
        rejected(&server.exec(&sid, &[], &[&ssh.join("id")], &Sh::write("x", &ssh.join("id"))), "grant into .ssh");
        assert!(!ssh.exists(), "{mode}");

        // 11. Directories too broad to open for writing.
        let mut broad: Vec<PathBuf> = if cfg!(windows) {
            [r"C:\", r"C:\Users", r"C:\new.txt"].map(PathBuf::from).to_vec()
        } else {
            ["/", "/etc", "/new.txt"].map(PathBuf::from).to_vec()
        };
        if let Some(home) = ["HOME", "USERPROFILE"].iter().find_map(std::env::var_os) {
            broad.push(PathBuf::from(&home));
            broad.push(PathBuf::from(&home).join("dbx-definitely-new.txt"));
        }
        for path in &broad {
            rejected(&server.exec(&sid, &[], &[path], &Sh::write("x", &out.join("never.txt"))), &path.display().to_string());
        }
        assert!(!out.join("never.txt").exists(), "{mode}");

        // 12. Grants never carry over to the next exec.
        let r = server.exec(&sid, &[], &[&out], &Sh::write("x", &out.join("once.txt")));
        assert_eq!(code(&r), 0, "{mode}: {r}");
        let r = server.exec(&sid, &[], &[], &Sh::write("x", &out.join("twice.txt")));
        denied(&r, "a grant must not persist");
        assert!(!out.join("twice.txt").exists(), "{mode}");
        server.stop(&sid);

        // 13. A session that forbids grants rejects them and creates nothing.
        let closed = server.start(mode, false);
        let fresh = base.join("fresh2").join("cache");
        let r = server.exec(&closed, &[], &[&fresh.join("a.txt")], &Sh::write("x", &fresh.join("a.txt")));
        rejected(&r, "grant in a session with allow_approval = false");
        assert!(!base.join("fresh2").exists(), "{mode}");
        server.stop(&closed);

        std::fs::remove_dir_all(&base).unwrap();
    }
    server.finish();
}

#![cfg(any(target_os = "linux", target_os = "macos", windows))]
//! All network-policy tests, over `deltabox serve`. The sandboxed command is
//! always curl, and what it tries to reach is always a local HTTP origin
//! (`127.0.0.1:<random port>`, run by the test; no public internet).
//!
//! * `approved_network_is_one_exec_only_and_direct_connections_stay_denied`:
//!   per-exec approval via `network_allow`.
//! * `static_network_modes_...`: the static `allow_audit`, `deny_all` and
//!   `allowlist` modes. Checks that hold everywhere run on every platform; the
//!   proxy-specific ones (denial reasons, `--noproxy` bypass, `localhost`) run
//!   on Windows only, where loopback allowlist entries go through the proxy.
//!
//! Each platform hardcodes its own curl path and filesystem read roots.
//! On Windows these tests are ignored by default because hosted CI lacks usable
//! PSEC. On a supported host, run:
//! `cargo test -p deltabox --test network_approvals -- --ignored --test-threads=1`.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
fn curl() -> String {
    format!("{}\\System32\\curl.exe", std::env::var("SystemRoot").unwrap())
}
#[cfg(windows)]
const READ_ROOTS: &str = r#"["."]"#;

#[cfg(unix)]
fn curl() -> String {
    "/usr/bin/curl".into()
}
#[cfg(unix)]
const READ_ROOTS: &str = r#"[".", "/usr", "/bin", "/etc", "/lib", "/lib64", "/run"]"#;

/// Policy TOML: read/write limited to the project, plus the given network rules.
/// `approval` lets a single exec request extra destinations (`network_allow`).
fn policy_toml(mode: &str, allow: &[String], approval: bool) -> String {
    let mut policy = format!("[filesystem]\nread = {READ_ROOTS}\nwrite = [\".\"]\n\n[network]\nmode = \"{mode}\"\n");
    if !allow.is_empty() {
        let entries: Vec<String> = allow.iter().map(|entry| format!("\"{entry}\"")).collect();
        policy.push_str(&format!("allow = [{}]\n", entries.join(", ")));
    }
    if approval {
        policy.push_str("allow_approval = true\nallow_private = true\n");
    }
    policy
}

/// A running `deltabox serve` process, driven with line-delimited JSON-RPC.
struct Server {
    root: tempfile::TempDir,
    project: PathBuf,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn spawn() -> Self {
        let root = tempfile::tempdir().unwrap();
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
        Self { root, project, child, input, output, next_id: 0 }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        writeln!(self.input, "{}", json!({"id": self.next_id, "method": method, "params": params})).unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid RPC response {line:?}: {e}"))
    }

    /// Start a copy-private session under `policy` (TOML text); returns its id.
    fn start_session(&mut self, policy: &str) -> String {
        let policy_path: &Path = &self.root.path().join("policy.toml");
        std::fs::write(policy_path, policy).unwrap();
        let params = json!({"project": self.project, "policy": policy_path, "fs_mode": "copy-private", "require_fs_mode": true});
        let start = self.call("session.start", params);
        start["result"]["session_id"].as_str().expect("session.start succeeded").to_owned()
    }

    fn stop_session(&mut self, sid: &str) {
        let stopped = self.call("session.stop", json!({"session_id": sid}));
        assert!(stopped.get("error").is_none(), "{stopped}");
    }

    /// Run curl inside the sandbox against `url` and return the exec result.
    /// `approve`: destinations approved for this single exec (`network_allow`).
    /// `direct`:  add `--noproxy *`, so curl ignores the sandbox's proxy
    ///            variables and opens the TCP connection itself.
    fn curl(&mut self, sid: &str, url: &str, approve: &[String], direct: bool) -> Value {
        let mut argv = vec![curl(), "--max-time".into(), "3".into(), "--silent".into(), "--show-error".into(), "--fail".into(), url.into()];
        if direct {
            argv.extend(["--noproxy".into(), "*".into()]);
        }
        let mut params = json!({"session_id": sid, "argv": argv});
        if !approve.is_empty() {
            params["network_allow"] = json!(approve);
        }
        let response = self.call("session.exec", params);
        assert!(response.get("error").is_none(), "session.exec failed: {response}");
        response["result"].clone()
    }

    fn finish(self) {
        let Server { input, mut child, .. } = self;
        drop(input);
        assert!(child.wait().unwrap().success());
    }
}

/// Accept exactly one connection, answer `200 ok`, and return.
fn serve_once(origin: &TcpListener) -> std::thread::JoinHandle<()> {
    let origin = origin.try_clone().unwrap();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            match origin.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => panic!("origin never contacted: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            stream.read_exact(&mut b).unwrap();
            header.push(b[0]);
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
    })
}

/// Bind a local origin that answers `200 ok` to every connection until the
/// test process ends. Returns its port.
fn serve_always() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let (mut header, mut byte) = (Vec::new(), [0]);
            while !header.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).is_err() {
                    break;
                }
                header.push(byte[0]);
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    port
}

/// Per-exec approval (`network_allow`):
///
/// Policy: `allowlist` with an EMPTY allowlist, so nothing is reachable by
/// default. The command tries to reach the local origin.
///
/// 1. no approval, via proxy        -> FORBIDDEN (`not-authorized`), origin untouched
/// 2. approved for this exec        -> ALLOWED, origin answers `ok`
/// 3. next exec, approval gone      -> FORBIDDEN again (approval is one-shot)
/// 4. approved but bypass the proxy -> FORBIDDEN (`--noproxy *` makes curl connect
///    directly; the OS sandbox must block it, only the proxy path is open)
#[test]
#[cfg_attr(windows, ignore = "requires native MXC PSEC; run explicitly on a supported Windows host")]
fn approved_network_is_one_exec_only_and_direct_connections_stay_denied() {
    let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    origin.set_nonblocking(true).unwrap();
    let target = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());
    let url = format!("http://{target}/");
    let approve = [target];

    let mut server = Server::spawn();
    assert_eq!(server.call("network.capabilities", json!({}))["result"]["version"], 1);
    let sid = server.start_session(&policy_toml("allowlist", &[], true));

    // 1. FORBIDDEN: curl goes through the proxy, which refuses the destination
    //    (403, recorded as `not-authorized`); origin never contacted.
    let denied = server.curl(&sid, &url, &[], false);
    assert_ne!(denied["exit_code"], 0, "{denied}");
    assert_eq!(denied["network_denials"][0]["reason"], "not-authorized", "{denied}");
    assert!(origin.accept().is_err(), "unapproved command reached the origin");

    // 2. ALLOWED: destination approved for this exec. The proxy forwards it and
    //    the origin (answering once with `ok`) is reached.
    let origin_thread = serve_once(&origin);
    let allowed = server.curl(&sid, &url, &approve, false);
    assert_eq!(allowed["exit_code"], 0, "{allowed}");
    assert_eq!(allowed["stdout"], "ok", "{allowed}");
    origin_thread.join().unwrap();

    // 3. FORBIDDEN: same request, no approval. The step-2 approval must not
    //    carry over to later execs.
    let after = server.curl(&sid, &url, &[], false);
    assert_eq!(after["network_denials"][0]["reason"], "not-authorized", "{after}");
    assert!(origin.accept().is_err(), "approval leaked into the next exec");

    // 4. FORBIDDEN: destination is approved, but curl bypasses the proxy and
    //    connects straight to 127.0.0.1. Approval only opens the proxy path, so
    //    the OS-level sandbox network rules must reject the direct connection.
    let direct = server.curl(&sid, &url, &approve, true);
    assert_ne!(direct["exit_code"], 0, "direct access bypassed proxy: {direct}");
    assert!(origin.accept().is_err(), "a direct connection reached the origin");

    server.stop_session(&sid);
    server.finish();
}

/// Static network modes, each in its own session. The command tries to reach
/// two local origins (`port`, `other_port`) that both answer `ok`.
///
/// | mode / allowlist             | `port`               | `other_port`      |
/// |------------------------------|----------------------|-------------------|
/// | `allow_audit`                | ALLOWED              | -                 |
/// | `deny_all`                   | FORBIDDEN            | -                 |
/// | `allowlist 127.0.0.1:port`   | ALLOWED              | FORBIDDEN         |
///
/// Windows additionally checks, because loopback entries go through the proxy
/// there: the refusal reason is `not-authorized`, a direct connection
/// (`--noproxy *`) past the proxy is FORBIDDEN, and a `localhost:port` entry
/// is refused by the private-address guard.
#[test]
#[cfg_attr(windows, ignore = "requires native MXC PSEC; run explicitly on a supported Windows host")]
fn static_network_modes_are_enforced() {
    let (port, other_port) = (serve_always(), serve_always());
    let local = format!("http://127.0.0.1:{port}/");
    let other = format!("http://127.0.0.1:{other_port}/");
    let mut server = Server::spawn();

    // allow_audit: unrestricted, loopback included, so even a direct connection works.
    let sid = server.start_session(&policy_toml("allow_audit", &[], false));
    let result = server.curl(&sid, &local, &[], true);
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["stdout"], "ok");
    server.stop_session(&sid);

    // deny_all: not even loopback.
    let sid = server.start_session(&policy_toml("deny_all", &[], false));
    let result = server.curl(&sid, &local, &[], true);
    assert_ne!(result["exit_code"], 0, "deny_all let a loopback connection through: {result}");
    server.stop_session(&sid);

    // allowlist with one loopback entry: the listed port answers, another is refused.
    let sid = server.start_session(&policy_toml("allowlist", &[format!("127.0.0.1:{port}")], false));
    let result = server.curl(&sid, &local, &[], false);
    assert_eq!(result["exit_code"], 0, "the listed port was unreachable: {result}");
    assert_eq!(result["stdout"], "ok");
    let result = server.curl(&sid, &other, &[], false);
    assert_ne!(result["exit_code"], 0, "an unlisted port was reachable: {result}");
    #[cfg(windows)]
    {
        assert_eq!(result["network_denials"][0]["reason"], "not-authorized", "{result}");
        let result = server.curl(&sid, &local, &[], true);
        assert_ne!(result["exit_code"], 0, "a connection went past the proxy: {result}");
    }
    server.stop_session(&sid);

    // Windows: a hostname entry is served by the proxy too; `localhost` stays
    // behind the private-address guard while the listed literal is reached.
    #[cfg(windows)]
    {
        let allow = [format!("localhost:{port}"), format!("127.0.0.1:{port}")];
        let sid = server.start_session(&policy_toml("allowlist", &allow, false));
        let result = server.curl(&sid, &local, &[], false);
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["stdout"], "ok");
        let result = server.curl(&sid, &format!("http://localhost:{port}/"), &[], false);
        assert_ne!(result["exit_code"], 0, "{result}");
        assert_eq!(result["network_denials"][0]["reason"], "private-address", "{result}");
        let result = server.curl(&sid, &local, &[], true);
        assert_ne!(result["exit_code"], 0, "a direct connection bypassed the proxy: {result}");
        server.stop_session(&sid);
    }

    server.finish();
}

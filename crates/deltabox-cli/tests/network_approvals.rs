#![cfg(any(target_os = "linux", target_os = "macos", windows))]
//! Network approvals end to end over `deltabox serve`.
//!
//! Policy under test: network mode is `allowlist` with an EMPTY allowlist, so
//! by default the sandboxed command may reach nothing. A caller can approve a
//! destination for ONE exec via `network_allow`.
//!
//! What the sandboxed command tries to reach: a local HTTP origin
//! (`127.0.0.1:<random port>`, run by this test; no public internet) using
//! curl. Each platform hardcodes its own curl path and filesystem read roots;
//! the checks are identical:
//!
//! 1. no approval, via proxy       -> FORBIDDEN (`not-authorized`), origin untouched
//! 2. approved for this exec       -> ALLOWED, origin answers `ok`
//! 3. next exec, approval gone     -> FORBIDDEN again (approval is one-shot)
//! 4. approved but bypass the proxy -> FORBIDDEN (`--noproxy *` makes curl connect
//!    directly; the OS sandbox must block it, only the proxy path is open)
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
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

/// Send one JSON-RPC request and return the response line.
fn rpc(input: &mut impl Write, output: &mut impl BufRead, id: u64, method: &str, params: Value) -> Value {
    writeln!(input, "{}", json!({"id": id, "method": method, "params": params})).unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid RPC response {line:?}: {e}"))
}

/// Accept one connection on `origin`, answer `200 ok`, and return.
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

#[test]
#[cfg_attr(windows, ignore = "requires native MXC PSEC networking; run explicitly on a supported Windows host")]
fn approved_network_is_one_exec_only_and_direct_connections_stay_denied() {
    // Fixture: project, policy (allowlist with per-exec approval), local origin.
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let policy = root.path().join("policy.toml");
    std::fs::write(
        &policy,
        format!("[filesystem]\nread = {READ_ROOTS}\nwrite = [\".\"]\n[network]\nmode = \"allowlist\"\nallow_approval = true\nallow_private = true\n"),
    )
    .unwrap();
    let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    origin.set_nonblocking(true).unwrap();
    let target = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());

    let mut server = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .arg("serve")
        .current_dir(&project)
        .env("DELTABOX_STATE_DIR", root.path().join("state"))
        .env("DELTABOX_RUNTIME_DIR", root.path().join("runtime"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = server.stdin.take().unwrap();
    let mut output = BufReader::new(server.stdout.take().unwrap());
    let mut next_id = 0;
    let mut call = |method: &str, params: Value| {
        next_id += 1;
        rpc(&mut input, &mut output, next_id, method, params)
    };

    assert_eq!(call("network.capabilities", json!({}))["result"]["version"], 1);
    let start = call("session.start", json!({"project": project, "policy": policy, "fs_mode": "copy-private", "require_fs_mode": true}));
    let sid = start["result"]["session_id"].as_str().expect("session.start succeeded").to_owned();

    // Run curl inside the sandbox against the local origin (`http://{target}/`).
    // `approve`: pass `network_allow=[target]`, i.e. approve that one destination
    //            for this single exec.
    // `direct`:  add `--noproxy *`, so curl ignores the sandbox's proxy variables
    //            and tries to open the TCP connection itself.
    let mut fetch = |approve: bool, direct: bool| {
        let mut argv = vec![curl(), "--max-time".into(), "3".into(), "--silent".into(), "--show-error".into(), "--fail".into(), format!("http://{target}/")];
        if direct {
            argv.extend(["--noproxy".into(), "*".into()]);
        }
        let mut params = json!({"session_id": sid, "argv": argv});
        if approve {
            params["network_allow"] = json!([target]);
        }
        call("session.exec", params)
    };

    // 1. FORBIDDEN: no approval. curl goes through the proxy, which refuses the
    //    destination (403, recorded as `not-authorized`); origin never contacted.
    let denied = fetch(false, false);
    assert_ne!(denied["result"]["exit_code"], 0, "{denied}");
    assert_eq!(denied["result"]["network_denials"][0]["reason"], "not-authorized", "{denied}");
    assert!(origin.accept().is_err(), "unapproved command reached the origin");

    // 2. ALLOWED: destination approved for this exec. The proxy forwards it and
    //    the origin (answering once with `ok`) is reached.
    let origin_thread = serve_once(&origin);
    let allowed = fetch(true, false);
    assert_eq!(allowed["result"]["exit_code"], 0, "{allowed}");
    assert_eq!(allowed["result"]["stdout"], "ok", "{allowed}");
    origin_thread.join().unwrap();

    // 3. FORBIDDEN: same request, no approval. The step-2 approval must not
    //    carry over to later execs.
    let after = fetch(false, false);
    assert_eq!(after["result"]["network_denials"][0]["reason"], "not-authorized", "{after}");
    assert!(origin.accept().is_err(), "approval leaked into the next exec");

    // 4. FORBIDDEN: destination is approved, but curl bypasses the proxy and
    //    connects straight to 127.0.0.1. Approval only opens the proxy path, so
    //    the OS-level sandbox network rules must reject the direct connection.
    let direct = fetch(true, true);
    assert_ne!(direct["result"]["exit_code"], 0, "direct access bypassed proxy: {direct}");
    assert!(origin.accept().is_err(), "a direct connection reached the origin");

    let stopped = call("session.stop", json!({"session_id": sid}));
    assert!(stopped.get("error").is_none(), "{stopped}");
    drop(input);
    assert!(server.wait().unwrap().success());
}

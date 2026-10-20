#![cfg(any(target_os = "linux", target_os = "macos", windows))]
//! End-to-end service-free RPC test with a local origin, no public internet.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

fn rpc(input: &mut impl Write, output: &mut impl BufRead, id: u64, method: &str, params: Value) -> Value {
    writeln!(input, "{}", json!({"id": id, "method": method, "params": params})).unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid RPC response {line:?}: {e}"))
}

#[test]
#[cfg_attr(windows, ignore = "requires native MXC PSEC networking; run explicitly on a supported Windows host")]
fn approved_network_is_one_exec_only_and_direct_connections_stay_denied() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let policy = root.path().join("policy.toml");
    let read = if cfg!(windows) { "[\".\"]" } else { "[\".\", \"/usr\", \"/bin\", \"/etc\", \"/lib\", \"/lib64\", \"/run\"]" };
    std::fs::write(&policy, format!("[filesystem]\nread = {read}\nwrite = [\".\"]\n[network]\nmode = \"allowlist\"\nallow_approval = true\nallow_private = true\n")).unwrap();
    let curl = if cfg!(windows) { format!("{}\\System32\\curl.exe", std::env::var("SystemRoot").unwrap()) } else { "/usr/bin/curl".to_string() };
    let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    origin.set_nonblocking(true).unwrap();
    let target = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());
    let mut server = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .arg("serve").current_dir(&project)
        .env("DELTABOX_STATE_DIR", root.path().join("state"))
        .env("DELTABOX_RUNTIME_DIR", root.path().join("runtime"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
    let mut input = server.stdin.take().unwrap();
    let mut output = BufReader::new(server.stdout.take().unwrap());
    assert_eq!(rpc(&mut input, &mut output, 1, "network.capabilities", json!({}))["result"]["version"], 1);
    let start = rpc(&mut input, &mut output, 2, "session.start", json!({"project": project, "policy": policy, "fs_mode": "copy-private", "require_fs_mode": true}));
    let sid = start["result"]["session_id"].as_str().expect("session.start succeeded").to_owned();
    let request = |extra: Value, direct: bool| {
        let mut params = json!({"session_id": sid, "argv": [curl, "--max-time", "3", "--silent", "--show-error", "--fail", format!("http://{target}/")]});
        if direct { params["argv"].as_array_mut().unwrap().extend([json!("--noproxy"), json!("*")]); }
        if !extra.is_null() { params["network_allow"] = extra; }
        params
    };
    let denied = rpc(&mut input, &mut output, 3, "session.exec", request(Value::Null, false));
    assert_ne!(denied["result"]["exit_code"], json!(0), "{denied}");
    assert_eq!(denied["result"]["network_denials"][0]["reason"], "not-authorized", "{denied}");
    assert!(origin.accept().is_err(), "denied command must not contact the origin");
    let origin_clone = origin.try_clone().unwrap();
    let origin_thread = std::thread::spawn(move || {
        let origin = origin_clone;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            match origin.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("origin never contacted: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") { let mut b = [0]; stream.read_exact(&mut b).unwrap(); header.push(b[0]); }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
    });
    let allowed = rpc(&mut input, &mut output, 4, "session.exec", request(json!([target]), false));
    assert_eq!(allowed["result"]["exit_code"], 0, "{allowed}");
    assert_eq!(allowed["result"]["stdout"], "ok", "{allowed}");
    origin_thread.join().unwrap();
    let after = rpc(&mut input, &mut output, 5, "session.exec", request(Value::Null, false));
    assert_eq!(after["result"]["network_denials"][0]["reason"], "not-authorized", "{after}");
    let direct = rpc(&mut input, &mut output, 6, "session.exec", request(json!([target]), true));
    assert_ne!(direct["result"]["exit_code"], json!(0), "direct access bypassed proxy: {direct}");
    assert!(origin.accept().is_err(), "a direct connection reached the origin despite network isolation");
    let stopped = rpc(&mut input, &mut output, 7, "session.stop", json!({"session_id": sid}));
    assert!(stopped.get("error").is_none(), "{stopped}");
    drop(input);
    assert!(server.wait().unwrap().success());
}

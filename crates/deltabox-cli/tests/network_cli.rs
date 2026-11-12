use std::process::Command;

#[test]
fn malformed_and_excessive_network_grants_fail_before_session_setup() {
    for subcommand in ["exec", "run"] {
        for target in ["*", "https://example.com", "example.com:0"] {
            let work = tempfile::tempdir().unwrap();
            let state = work.path().join("state");
            let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
            command
                .current_dir(work.path())
                .env("DELTABOX_STATE_DIR", &state)
                .arg(subcommand);
            if subcommand == "exec" {
                command.arg("unused");
            } else {
                command.arg("--abort");
            }
            let output = command
                .args(["--network-allow", target, "--", "unused"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&output.stderr).contains("host:port"));
            assert!(!state.exists());
        }
        let work = tempfile::tempdir().unwrap();
        let state = work.path().join("state");
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command
            .current_dir(work.path())
            .env("DELTABOX_STATE_DIR", &state)
            .arg(subcommand);
        if subcommand == "exec" {
            command.arg("unused");
            command.args(["--", "unused"]);
        } else {
            command.arg("--abort");
            command.args(["--", "unused"]);
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command
            .current_dir(work.path())
            .env("DELTABOX_STATE_DIR", &state)
            .arg(subcommand);
        if subcommand == "exec" {
            command.arg("unused");
        } else {
            command.arg("--abort");
        }
        for _ in 0..33 {
            command.args(["--network-allow", "example.com:443"]);
        }
        let output = command.args(["--", "unused"]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("at most 32"));
        assert!(!state.exists());
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::process::Output;
    use std::time::{Duration, Instant};

    struct Project {
        _work: tempfile::TempDir,
        project: PathBuf,
        state: PathBuf,
        policy: PathBuf,
        id: Option<String>,
    }
    impl Project {
        fn new(approval: bool) -> Self {
            let work = tempfile::tempdir().unwrap();
            let project = work.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let policy = work.path().join("policy.toml");
            let extra = if approval {
                "allow_approval = true\nallow_private = true\ndeny = [\"blocked.example:443\"]\n"
            } else {
                ""
            };
            std::fs::write(&policy, format!("[filesystem]\nread = [\".\", \"/usr\", \"/bin\", \"/lib\", \"/lib64\", \"/etc\", \"/run\"]\nwrite = [\".\"]\n[network]\nmode = \"allowlist\"\n{extra}")).unwrap();
            let state = work.path().join("state");
            Self {
                _work: work,
                project,
                state,
                policy,
                id: None,
            }
        }
        fn run(&self, args: &[&str]) -> Output {
            Command::new(env!("CARGO_BIN_EXE_deltabox"))
                .current_dir(&self.project)
                .env("DELTABOX_STATE_DIR", &self.state)
                .args(args)
                .output()
                .unwrap()
        }
        fn start(&mut self) -> String {
            let output = self.run(&[
                "session",
                "start",
                ".",
                "--policy",
                self.policy.to_str().unwrap(),
                "--fs-mode",
                "copy-private",
                "--require-fs-mode",
            ]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
            self.id = Some(id.clone());
            id
        }
        fn curl(&self, id: &str, url: &str, grant: Option<&str>) -> Value {
            let mut args = vec!["exec", id];
            if let Some(target) = grant {
                args.extend(["--network-allow", target]);
            }
            args.extend([
                "--",
                "/usr/bin/curl",
                "--silent",
                "--show-error",
                "--fail",
                "--max-time",
                "3",
                url,
            ]);
            let output = self.run(&args);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            if let Some(id) = &self.id {
                let _ = self.run(&["session", "stop", id]);
            }
        }
    }
    fn origin() -> (TcpListener, String, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = listener.local_addr().unwrap().to_string();
        let url = format!("http://{target}/");
        (listener, target, url)
    }
    fn answer_once(listener: &TcpListener) -> std::thread::JoinHandle<()> {
        let listener = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            start.elapsed() < Duration::from_secs(10),
                            "approved request never reached origin"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        })
    }
    #[test]
    fn exec_grant_expires_and_cannot_override_the_policy_deny_list() {
        let (listener, target, url) = origin();
        let mut project = Project::new(true);
        let policy_before = std::fs::read(&project.policy).unwrap();
        let id = project.start();
        assert_ne!(project.curl(&id, &url, None)["exit_code"], 0);
        assert!(listener.accept().is_err());
        let server = answer_once(&listener);
        let approved = project.curl(&id, &url, Some(&target));
        server.join().unwrap();
        assert_eq!(approved["exit_code"], 0, "{approved}");
        assert_eq!(approved["stdout"], "ok");
        assert_ne!(project.curl(&id, &url, None)["exit_code"], 0);
        assert!(listener.accept().is_err());
        let denied = project.run(&[
            "exec",
            &id,
            "--network-allow",
            "BLOCKED.EXAMPLE:443",
            "--",
            "sh",
            "-c",
            "touch must-not-run",
        ]);
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("prohibited"));
        assert_eq!(std::fs::read(&project.policy).unwrap(), policy_before);
        let diff = project.run(&["diff", &id, "--json"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&diff.stdout).unwrap()["pending_changes"],
            0
        );
        assert!(project.run(&["session", "stop", &id]).status.success());
    }
    #[test]
    fn run_accepts_grants_and_cleans_up_its_temporary_session() {
        let (listener, target, url) = origin();
        let project = Project::new(true);
        let server = answer_once(&listener);
        let output = project.run(&[
            "run",
            "--abort",
            "--fs-mode",
            "copy-private",
            "--require-fs-mode",
            "--policy",
            project.policy.to_str().unwrap(),
            "--network-allow",
            &target,
            "--",
            "/usr/bin/curl",
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "3",
            &url,
        ]);
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"ok");
        let listed = project.run(&["session", "list", "--json"]);
        assert!(listed.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&listed.stdout).unwrap(),
            serde_json::json!([])
        );
    }
    #[test]
    fn exec_refuses_grants_when_the_policy_has_not_opted_in() {
        let mut project = Project::new(false);
        let id = project.start();
        let output = project.run(&[
            "exec",
            &id,
            "--network-allow",
            "example.com:443",
            "--",
            "sh",
            "-c",
            "touch must-not-run",
        ]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("disabled"));
        let diff = project.run(&["diff", &id, "--json"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&diff.stdout).unwrap()["pending_changes"],
            0
        );
        assert!(project.run(&["session", "stop", &id]).status.success());
    }
}

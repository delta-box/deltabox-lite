use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_deltabox")
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "command failed ({}):\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn history_help_exposes_the_complete_workflow() {
    let output = Command::new(bin())
        .args(["history", "--help"])
        .output()
        .unwrap();
    assert_ok(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    for subcommand in [
        "capabilities",
        "capture",
        "list",
        "diff",
        "preview",
        "restore",
    ] {
        assert!(help.contains(subcommand), "missing {subcommand}: {help}");
    }

    let output = Command::new(bin())
        .args(["history", "preview", "--help"])
        .output()
        .unwrap();
    assert_ok(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--project"), "{help}");
    assert!(help.contains("--json"), "{help}");
    assert!(
        !help.contains("<SESSION>"),
        "preview must be session-free: {help}"
    );
}

#[test]
fn restore_requires_an_expected_version_before_opening_a_project() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let output = Command::new(bin())
        .current_dir(project.path())
        .env("DELTABOX_STATE_DIR", state.path())
        .args(["history", "restore", "unused-session", "unused-version"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--expected-version"), "{stderr}");
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(project.path()).unwrap().count(), 0);
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    struct Session {
        project: tempfile::TempDir,
        state: tempfile::TempDir,
        id: String,
        stopped: bool,
    }

    impl Session {
        fn start(mode: &str) -> Self {
            let project = tempfile::Builder::new()
                .prefix("deltabox-history-cli-project-")
                .tempdir_in("/private/tmp")
                .unwrap();
            let state = tempfile::Builder::new()
                .prefix("deltabox-history-cli-state-")
                .tempdir_in("/private/tmp")
                .unwrap();
            std::fs::write(project.path().join("a.txt"), "original\n").unwrap();
            std::fs::write(project.path().join(".gitignore"), "target/\n").unwrap();
            std::fs::create_dir(project.path().join("target")).unwrap();
            std::fs::write(project.path().join("target/output"), "generated before\n").unwrap();
            let output = Command::new(bin())
                .env("DELTABOX_STATE_DIR", state.path())
                .current_dir(project.path())
                .args([
                    "session",
                    "start",
                    ".",
                    "--fs-mode",
                    mode,
                    "--require-fs-mode",
                ])
                .output()
                .unwrap();
            assert_ok(&output);
            let id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
            Self {
                project,
                state,
                id,
                stopped: false,
            }
        }

        fn run(&self, args: &[&str]) -> Output {
            self.run_from(self.project.path(), args)
        }

        fn run_from(&self, cwd: &Path, args: &[&str]) -> Output {
            Command::new(bin())
                .env("DELTABOX_STATE_DIR", self.state.path())
                .current_dir(cwd)
                .args(args)
                .output()
                .unwrap()
        }

        fn json(&self, args: &[&str]) -> Value {
            parse_json(self.run(args))
        }

        fn capture(&self, label: &str) -> String {
            let output = self.run(&["history", "capture", &self.id, "--label", label]);
            assert_ok(&output);
            let stdout = String::from_utf8(output.stdout).unwrap();
            let id = stdout.trim();
            assert_eq!(
                stdout.lines().count(),
                1,
                "capture should print only its ID"
            );
            assert_eq!(id.len(), 36, "capture should print a UUID: {stdout}");
            id.to_owned()
        }

        fn write(&self, path: &str, contents: &str) {
            assert_ok(&self.run(&["write", &self.id, path, "--content", contents]));
        }

        fn stop(&mut self) {
            assert_ok(&self.run(&["session", "stop", &self.id]));
            self.stopped = true;
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if !self.stopped {
                // Clean up helpers even when an assertion unwinds. Successful
                // tests explicitly stop and check the result instead.
                let _ = self.run(&["session", "stop", &self.id]);
            }
        }
    }

    fn parse_json(output: Output) -> Value {
        assert_ok(&output);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn path_inventory(root: &Path) -> BTreeSet<PathBuf> {
        fn visit(root: &Path, directory: &Path, paths: &mut BTreeSet<PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                paths.insert(path.strip_prefix(root).unwrap().to_owned());
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &path, paths);
                }
            }
        }
        let mut paths = BTreeSet::new();
        visit(root, root, &mut paths);
        paths
    }

    #[test]
    fn history_roundtrip_restores_source_and_exposes_a_reusable_backup() {
        let mut session = Session::start("cbcow-through");
        let capabilities = session.json(&["history", "capabilities", &session.id]);
        assert_eq!(capabilities["supported"], true);
        assert_eq!(capabilities["version"], 1);
        assert_eq!(
            session.json(&["history", "list", &session.id, "--json"]),
            serde_json::json!([])
        );

        let original = session.capture("original source");
        session.write("a.txt", "changed\n");
        session.write("new.txt", "added\n");
        std::fs::write(
            session.project.path().join("target/output"),
            "generated after\n",
        )
        .unwrap();
        let current = session.json(&[
            "history",
            "capture",
            &session.id,
            "--label",
            "changed source",
            "--json",
        ]);
        let current_id = current["id"].as_str().unwrap();
        assert_eq!(current["session_id"], session.id);
        assert_eq!(current["label"], "changed source");
        assert!(current["entry_count"].as_u64().unwrap() >= 3);
        assert!(current["logical_bytes"].as_u64().unwrap() > 0);

        let versions = session.json(&["history", "list", &session.id, "--json"]);
        let versions = versions.as_array().unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.iter().any(|version| version["id"] == original));
        assert!(versions.iter().any(|version| version == &current));
        let listed = session.run(&["history", "list", &session.id]);
        assert_ok(&listed);
        let listed = String::from_utf8(listed.stdout).unwrap();
        assert!(listed.contains(&original), "{listed}");
        assert!(listed.contains("original source"), "{listed}");
        assert!(listed.contains("changed source"), "{listed}");

        let diff = session.json(&["history", "diff", &session.id, &original, "--json"]);
        assert_eq!(diff["to"], original);
        assert_eq!(diff["pending_changes"], 2);
        let changes = diff["changes"].as_array().unwrap();
        assert!(changes
            .iter()
            .any(|change| change["path"] == "a.txt" && change["kind"] == "modified"));
        assert!(changes
            .iter()
            .any(|change| change["path"] == "new.txt" && change["kind"] == "deleted"));
        let text = session.run(&["history", "diff", &session.id, &original]);
        assert_ok(&text);
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(text.contains("a.txt"), "{text}");
        assert!(text.contains("-changed"), "{text}");
        assert!(text.contains("+original"), "{text}");

        let restored = session.json(&[
            "history",
            "restore",
            &session.id,
            &original,
            "--expected-version",
            current_id,
        ]);
        assert_eq!(restored["version"]["id"], original);
        assert_eq!(restored["restored"], 2);
        let backup = restored["backup"]["id"].as_str().unwrap();
        assert_ne!(backup, current_id);
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
            "original\n"
        );
        assert!(!session.project.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("target/output")).unwrap(),
            "generated after\n"
        );
        assert_eq!(
            session.json(&["history", "diff", &session.id, &original, "--json"])["pending_changes"],
            0
        );

        let undone = session.json(&[
            "history",
            "restore",
            &session.id,
            backup,
            "--expected-version",
            &original,
        ]);
        assert_eq!(undone["version"]["id"], backup);
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("new.txt")).unwrap(),
            "added\n"
        );
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("target/output")).unwrap(),
            "generated after\n"
        );
        assert_eq!(
            session
                .json(&["history", "list", &session.id, "--json"])
                .as_array()
                .unwrap()
                .len(),
            4
        );
        session.stop();
    }

    #[test]
    fn stale_expected_version_rejects_restore_without_mutating_files_or_history() {
        let mut session = Session::start("cbcow-through");
        let original = session.capture("original");
        session.write("a.txt", "reviewed\n");
        let expected = session.capture("reviewed");
        session.write("a.txt", "new unreviewed edit\n");
        let versions = session.json(&["history", "list", &session.id, "--json"]);
        let output = session.run(&[
            "history",
            "restore",
            &session.id,
            &original,
            "--expected-version",
            &expected,
        ]);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("history restore conflict"), "{stderr}");
        assert!(stderr.contains("a.txt"), "{stderr}");
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
            "new unreviewed edit\n"
        );
        assert_eq!(
            session.json(&["history", "list", &session.id, "--json"]),
            versions
        );
        session.stop();
    }

    #[test]
    fn preview_after_stop_uses_explicit_project_without_creating_a_session() {
        let mut session = Session::start("cbcow-through");
        let original = session.capture("original");
        session.write("a.txt", "committed\n");
        assert_ok(&session.run(&["commit", &session.id]));
        session.stop();
        let other_cwd = tempfile::tempdir().unwrap();
        let project = session.project.path().to_str().unwrap();
        let state_before = path_inventory(session.state.path());
        let preview = parse_json(session.run_from(
            other_cwd.path(),
            &[
                "history",
                "preview",
                &original,
                "--project",
                project,
                "--json",
            ],
        ));
        assert_eq!(preview["to"], original);
        assert_eq!(preview["pending_changes"], 1);
        assert_eq!(preview["changes"][0]["path"], "a.txt");
        let text = session.run_from(
            other_cwd.path(),
            &["history", "preview", &original, "--project", project],
        );
        assert_ok(&text);
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(text.contains("-committed"), "{text}");
        assert!(text.contains("+original"), "{text}");
        assert_eq!(path_inventory(session.state.path()), state_before);
        assert_eq!(std::fs::read_dir(other_cwd.path()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_to_string(session.project.path().join("a.txt")).unwrap(),
            "committed\n"
        );
    }

    #[test]
    fn private_mode_reports_unsupported_history_without_capturing_a_version() {
        let mut session = Session::start("cbcow-private");
        let capabilities = session.json(&["history", "capabilities", &session.id]);
        assert_eq!(capabilities["supported"], false);
        assert!(capabilities["reason"]
            .as_str()
            .unwrap()
            .contains("macOS cbcow-through"));
        let state_before = path_inventory(session.state.path());
        let output = session.run(&["history", "capture", &session.id]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("macOS cbcow-through"));
        assert!(output.stdout.is_empty());
        assert_eq!(path_inventory(session.state.path()), state_before);
        session.stop();
    }
}

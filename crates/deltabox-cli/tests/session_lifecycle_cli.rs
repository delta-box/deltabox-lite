use std::process::Command;

#[test]
fn release_rejects_noncanonical_ids_before_opening_state() {
    let work = tempfile::tempdir().unwrap();
    let state = work.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .current_dir(work.path())
        .env("DELTABOX_STATE_DIR", &state)
        .args(["session", "release", "../outside"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("canonical UUID"));
    assert!(!state.exists());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::process::Output;

    struct Project {
        _work: tempfile::TempDir,
        project: PathBuf,
        state: PathBuf,
        sessions: Vec<String>,
    }

    impl Project {
        fn new() -> Self {
            let work = tempfile::tempdir().unwrap();
            let project = work.path().join("project");
            std::fs::create_dir(&project).unwrap();
            std::fs::write(project.join("file.txt"), "initial").unwrap();
            let state = work.path().join("state");
            Self {
                _work: work,
                project,
                state,
                sessions: Vec::new(),
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
        fn json(&self, args: &[&str]) -> Value {
            let output = self.run(args);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        }
        fn start(&mut self, mode: &str) -> String {
            let value = self.json(&[
                "session",
                "start",
                ".",
                "--fs-mode",
                mode,
                "--require-fs-mode",
                "--json",
            ]);
            let id = value["session_id"].as_str().unwrap().to_owned();
            self.sessions.push(id.clone());
            id
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            for id in &self.sessions {
                let _ = self.run(&["session", "stop", id]);
            }
        }
    }

    #[test]
    fn list_finds_sessions_and_filters_recorded_status_without_registry() {
        let mut project = Project::new();
        assert_eq!(
            project.json(&["session", "list", "--json"]),
            serde_json::json!([])
        );
        let first = project.start("copy-private");
        let second = project.start("copy-private");
        project.json(&["session", "stop", &first]);
        let roots = std::fs::read_dir(project.state.join("projects")).unwrap();
        for root in roots {
            std::fs::remove_file(root.unwrap().path().join("sessions.json")).unwrap();
        }
        let all = project.json(&["session", "list", "--json"]);
        assert_eq!(all.as_array().unwrap().len(), 2);
        let running = project.json(&["session", "list", "--status", "running", "--json"]);
        assert_eq!(running.as_array().unwrap().len(), 1);
        assert_eq!(running[0]["session_id"], second);
        let stopped = project.json(&["session", "list", "--status", "stopped", "--json"]);
        assert_eq!(stopped[0]["session_id"], first);
        assert_eq!(stopped[0]["resolved_fs_mode"], "copy-private");
        let table = project.run(&["session", "list"]);
        assert!(table.status.success());
        let table = String::from_utf8(table.stdout).unwrap();
        assert!(table.contains(&first) && table.contains(&second));
        assert!(table.contains("stopped") && table.contains("running"));
        let other = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .current_dir(other.path())
            .env("DELTABOX_STATE_DIR", &project.state)
            .args([
                "session",
                "list",
                "--project",
                project.project.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            all
        );
        project.json(&["session", "stop", &second]);
    }

    #[test]
    fn release_does_not_silently_stop_unsupported_private_mode() {
        let mut project = Project::new();
        let id = project.start("copy-private");
        let output = project.run(&["session", "release", &id]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires macOS cbcow-through"));
        assert_eq!(
            project.json(&["session", "status", &id])["status"],
            "running"
        );
        project.json(&["session", "stop", &id]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn release_preserves_pending_and_external_edits_for_the_next_session() {
        let mut project = Project::new();
        let id = project.start("cbcow-through");
        let write = project.run(&["write", &id, "file.txt", "--content", "pending"]);
        assert!(write.status.success());
        std::fs::write(project.project.join("external.txt"), "editor").unwrap();
        assert_eq!(
            project.json(&["session", "release", &id])["status"],
            "stopped"
        );
        assert_eq!(
            std::fs::read_to_string(project.project.join("file.txt")).unwrap(),
            "pending"
        );
        assert_eq!(
            std::fs::read_to_string(project.project.join("external.txt")).unwrap(),
            "editor"
        );
        let next = project.start("cbcow-through");
        assert_eq!(
            project.json(&["diff", &next, "--json"])["pending_changes"],
            0
        );
        project.json(&["session", "stop", &next]);
        assert_eq!(
            std::fs::read_to_string(project.project.join("file.txt")).unwrap(),
            "pending"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn release_requires_an_active_review_to_be_resolved() {
        let mut project = Project::new();
        let id = project.start("cbcow-through");
        project.json(&["review", "begin", &id, "--kind", "task"]);
        let output = project.run(&["session", "release", &id]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("active review"));
        assert_eq!(
            project.json(&["session", "status", &id])["status"],
            "running"
        );
        project.json(&["review", "abort", &id]);
        project.json(&["session", "release", &id]);
    }
}

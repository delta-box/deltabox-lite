use std::process::Command;

const DIFF_COMMANDS: &[&[&str]] = &[
    &["diff", "unused-session"],
    &["review", "diff", "unused-session"],
];

#[test]
fn all_diff_commands_expose_the_shared_output_options() {
    for args in DIFF_COMMANDS {
        let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .args(*args)
            .arg("--help")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        for option in ["--json", "--stat", "--name-only", "--path"] {
            assert!(
                help.contains(option),
                "{args:?} is missing {option}: {help}"
            );
        }
    }
}

#[test]
fn invalid_diff_paths_are_rejected_before_opening_or_recovering_a_project() {
    for args in DIFF_COMMANDS {
        for path in ["../outside", "/outside", "C:/outside", "src\\..\\outside"] {
            let project = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
                .args(*args)
                .args(["--path", path])
                .current_dir(project.path())
                .env("DELTABOX_STATE_DIR", state.path())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(2), "{args:?} {path:?}: {stderr}");
            assert!(stderr.contains("--path"), "{args:?}: {stderr}");
            assert!(output.stdout.is_empty());
            assert_eq!(std::fs::read_dir(project.path()).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
        }
    }
}

#[test]
fn conflicting_output_modes_are_rejected_before_project_access() {
    for args in DIFF_COMMANDS {
        for flags in [
            ["--json", "--stat"],
            ["--json", "--name-only"],
            ["--stat", "--name-only"],
        ] {
            let project = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
                .args(*args)
                .args(flags)
                .current_dir(project.path())
                .env("DELTABOX_STATE_DIR", state.path())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(2),
                "{args:?} {flags:?}: {stderr}"
            );
            assert!(stderr.contains("cannot be used with"), "{stderr}");
            assert!(output.stdout.is_empty());
            assert_eq!(std::fs::read_dir(project.path()).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use serde_json::Value;
    use std::process::Output;

    struct Session {
        project: tempfile::TempDir,
        state: tempfile::TempDir,
        id: String,
        stopped: bool,
    }

    impl Session {
        fn start() -> Self {
            let mut session = Self {
                project: tempfile::tempdir().unwrap(),
                state: tempfile::tempdir().unwrap(),
                id: String::new(),
                stopped: false,
            };
            for (path, contents) in [
                ("src/lib.rs", "old source\n"),
                ("src2/other.txt", "old other\n"),
            ] {
                let path = session.project.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, contents).unwrap();
            }
            session.id = session
                .text(&[
                    "session",
                    "start",
                    ".",
                    "--fs-mode",
                    "copy-private",
                    "--require-fs-mode",
                ])
                .trim()
                .to_owned();
            session
        }

        fn run(&self, args: &[&str]) -> Output {
            Command::new(env!("CARGO_BIN_EXE_deltabox"))
                .args(args)
                .current_dir(self.project.path())
                .env("DELTABOX_STATE_DIR", self.state.path())
                .output()
                .unwrap()
        }

        fn text(&self, args: &[&str]) -> String {
            let output = self.run(args);
            assert!(
                output.status.success(),
                "command {args:?} failed ({}):\nstdout: {}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            String::from_utf8(output.stdout).unwrap()
        }

        fn json(&self, args: &[&str]) -> Value {
            serde_json::from_str(&self.text(args)).unwrap()
        }

        fn stop(&mut self) {
            self.text(&["session", "stop", &self.id]);
            self.stopped = true;
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if !self.stopped && !self.id.is_empty() {
                let _ = self.run(&["session", "stop", &self.id]);
            }
        }
    }

    #[test]
    fn diff_filters_work_across_session_and_review_without_selective_commit() {
        let mut session = Session::start();
        let scope = session.json(&[
            "review",
            "begin",
            &session.id,
            "--kind",
            "task",
            "--label",
            "both files",
        ]);
        for (path, contents) in [
            ("src/lib.rs", "new source\n"),
            ("src2/other.txt", "new other\n"),
        ] {
            let full = session.project.path().join(path);
            session.text(&[
                "write",
                &session.id,
                full.to_str().unwrap(),
                "--content",
                contents,
            ]);
        }
        let full_before = session.json(&["diff", &session.id, "--json"]);
        assert_eq!(full_before["pending_changes"], 2);
        assert_eq!(full_before["changes"].as_array().unwrap().len(), 2);

        let text = session.text(&["diff", &session.id, "--path", "./src/"]);
        assert!(text.starts_with("1 file(s) changed\n"), "{text}");
        assert!(text.contains("src/lib.rs"), "{text}");
        assert!(text.contains("+new source"), "{text}");
        assert!(!text.contains("src2"), "{text}");
        assert!(!text.contains("new other"), "{text}");

        assert_eq!(
            session.text(&[
                "review",
                "diff",
                &session.id,
                "--path",
                "src",
                "--name-only"
            ]),
            "src/lib.rs\n"
        );
        let stat = session.text(&["review", "diff", &session.id, "--path", "src", "--stat"]);
        assert_eq!(
            stat,
            " M src/lib.rs | +1 -1\n1 file(s) changed, 1 insertion(s)(+), 1 deletion(s)(-)\n"
        );

        // Filtering is purely a view: both edits remain pending and the review
        // still refers to its original checkpoint until explicitly committed.
        assert_eq!(session.json(&["diff", &session.id, "--json"]), full_before);
        assert_eq!(session.json(&["review", "status", &session.id]), scope);
        let commit = session.json(&["review", "commit", &session.id]);
        assert_eq!(commit["applied"], 2);
        assert!(session.json(&["review", "status", &session.id]).is_null());
        assert_eq!(
            session.json(&["diff", &session.id, "--json"])["pending_changes"],
            0
        );
        session.stop();

        for (path, expected) in [
            ("src/lib.rs", "new source\n"),
            ("src2/other.txt", "new other\n"),
        ] {
            assert_eq!(
                std::fs::read_to_string(session.project.path().join(path)).unwrap(),
                expected
            );
        }
    }
}

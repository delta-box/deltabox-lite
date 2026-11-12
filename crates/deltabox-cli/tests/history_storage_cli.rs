#![cfg(target_os = "macos")]

use serde_json::Value;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Fixture {
    project: tempfile::TempDir,
    state: tempfile::TempDir,
    session: String,
    ids: Vec<String>,
}

fn ok(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

impl Fixture {
    fn new() -> Self {
        let project = tempfile::tempdir_in("/private/tmp").unwrap();
        let state = tempfile::tempdir_in("/private/tmp").unwrap();
        fs::write(project.path().join("source.txt"), "before\n").unwrap();
        let mut result = Self {
            project,
            state,
            session: String::new(),
            ids: Vec::new(),
        };
        result.session = String::from_utf8(
            ok(result.run(&[
                "session",
                "start",
                ".",
                "--fs-mode",
                "cbcow-through",
                "--require-fs-mode",
            ]))
            .stdout,
        )
        .unwrap()
        .trim()
        .into();
        for n in 0..3 {
            fs::write(
                result.project.path().join("source.txt"),
                format!("version {n}\n"),
            )
            .unwrap();
            let capture = result.json(&["history", "capture", &result.session, "--json"]);
            result.ids.push(capture["id"].as_str().unwrap().to_string());
            // Ensure stable temporal order even on coarse timestamp clocks.
            std::thread::sleep(Duration::from_millis(2));
        }
        ok(result.run(&["session", "release", &result.session]));
        result
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
        command
            .current_dir(self.project.path())
            .env("DELTABOX_STATE_DIR", self.state.path())
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&ok(self.run(args)).stdout).unwrap()
    }

    fn root(&self) -> PathBuf {
        fs::read_dir(self.state.path().join("macos-native/history-v1"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    fn plan(&self) -> Value {
        self.json(&["history", "prune", "--keep-last", "1", "--json"])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.session.is_empty() {
            let _ = self.run(&["session", "stop", &self.session]);
        }
    }
}

#[test]
fn project_retention_is_guarded_by_pins_and_exact_plan_without_touching_live_files() {
    let fixture = Fixture::new();
    let stats = fixture.json(&["history", "stats", "--json"]);
    assert_eq!(stats["versions"].as_array().unwrap().len(), 3);
    assert_eq!(stats["allocated_bytes_are_exclusive"], false);
    let before = fs::read(fixture.project.path().join("source.txt")).unwrap();
    let plan = fixture.plan();
    assert_eq!(plan["remove"].as_array().unwrap().len(), 2);
    assert_eq!(fixture.json(&["history", "stats", "--json"]), stats); // dry-run

    fixture.json(&["history", "pin", &fixture.ids[0], "--json"]);
    let stale = fixture.run(&[
        "history",
        "prune",
        "--keep-last",
        "1",
        "--apply",
        plan["plan_id"].as_str().unwrap(),
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale"));
    let plan = fixture.plan();
    assert_eq!(plan["remove"][0]["id"], fixture.ids[1]);
    assert_eq!(plan["remove"].as_array().unwrap().len(), 1);
    // Changing the policy also invalidates an otherwise unchanged catalog.
    let stale = fixture.run(&[
        "history",
        "prune",
        "--keep-last",
        "0",
        "--apply",
        plan["plan_id"].as_str().unwrap(),
    ]);
    assert!(!stale.status.success());
    let result = fixture.json(&[
        "history",
        "prune",
        "--keep-last",
        "1",
        "--apply",
        plan["plan_id"].as_str().unwrap(),
        "--json",
    ]);
    assert_eq!(
        result["removed_version_ids"],
        serde_json::json!([fixture.ids[1]])
    );
    assert_eq!(
        fs::read(fixture.project.path().join("source.txt")).unwrap(),
        before
    );
    assert_eq!(
        fixture.json(&["history", "verify", "--json"])["valid"],
        true
    );
    assert_eq!(
        fixture.json(&["history", "preview", &fixture.ids[0], "--json"])["to"],
        fixture.ids[0]
    );
    let budget = fixture.json(&[
        "history",
        "prune",
        "--keep-last",
        "0",
        "--max-logical-bytes",
        "0",
        "--json",
    ]);
    assert_eq!(budget["budget_satisfied"], false);
    assert_eq!(budget["retained"].as_array().unwrap().len(), 1);
    fixture.json(&["history", "pin", &fixture.ids[0], "--remove", "--json"]);
    assert_eq!(
        fixture.json(&["history", "stats", "--json"])["versions"][0]["pinned"],
        false
    );
}

#[test]
fn gc_process_crash_leaves_an_explicit_resumable_intent_and_collects_abandoned_capture() {
    let fixture = Fixture::new();
    let orphan_id = uuid::Uuid::new_v4().to_string();
    let orphan = fixture.root().join(format!(".capture-{orphan_id}"));
    fs::create_dir(&orphan).unwrap();
    fs::write(orphan.join("unpublished"), "incomplete capture").unwrap();
    let plan = fixture.plan();
    assert_eq!(
        plan["abandoned_capture_ids"],
        serde_json::json!([orphan_id])
    );
    let result = fixture
        .command(&[
            "history",
            "prune",
            "--keep-last",
            "1",
            "--apply",
            plan["plan_id"].as_str().unwrap(),
        ])
        .env("DELTABOX_TEST_CRASH_AT", "history_gc_renamed")
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stats = fixture.json(&["history", "stats", "--json"]);
    assert_eq!(stats["pending_gc"], true);
    assert!(stats["garbage"]["apparent_bytes"].as_u64().unwrap() > 0);
    assert!(!fixture
        .run(&["history", "pin", &fixture.ids[2]])
        .status
        .success());
    let result = fixture.json(&["history", "gc", "--json"]);
    assert_eq!(result["resumed"], true);
    assert_eq!(result["removed_version_ids"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["removed_capture_ids"],
        serde_json::json!([orphan_id])
    );
    let stats = fixture.json(&["history", "stats", "--json"]);
    assert_eq!(stats["pending_gc"], false);
    assert_eq!(stats["versions"].as_array().unwrap().len(), 1);
    assert!(!orphan.exists());
    assert_eq!(
        fixture.json(&["history", "gc", "--json"])["removed_version_ids"],
        serde_json::json!([])
    );
}

#[test]
fn scrub_reports_corruption_continues_other_versions_and_does_not_repair() {
    let fixture = Fixture::new();
    let corrupt = fixture.root().join(&fixture.ids[1]).join("tree/source.txt");
    fs::write(&corrupt, "corrupted\n").unwrap();
    let output = fixture.run(&["history", "verify", "--json"]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["valid"], false);
    assert_eq!(
        result["versions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["valid"] == true)
            .count(),
        2
    );
    assert_eq!(fs::read_to_string(&corrupt).unwrap(), "corrupted\n");
    assert_eq!(
        fixture.json(&["history", "verify", &fixture.ids[0], "--json"])["valid"],
        true
    );
}

#[test]
fn project_maintenance_waits_for_publication_lock_and_preview_reports_busy() {
    let fixture = Fixture::new();
    let records = fs::read_dir(fixture.state.path().join("projects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(records.join("publish.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let mut child = fixture
        .command(&["history", "prune", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "maintenance bypassed publication lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let preview = fixture.run(&["history", "preview", &fixture.ids[0], "--json"]);
    assert!(!preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stderr).contains("publication"));
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    ok(child.wait_with_output().unwrap());
}

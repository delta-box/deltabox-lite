#![cfg(any(target_os = "linux", target_os = "macos"))]

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn run(project: &Path, state: &Path, action: &str, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .env("DELTABOX_STATE_DIR", state)
        .args([
            "run",
            "--project",
            project.to_str().unwrap(),
            action,
            "--fs-mode",
            "copy-private",
            "--require-fs-mode",
            "--",
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap()
}

fn assert_no_sessions(project: &Path, state: &Path) {
    assert!(!project.join(".deltabox").exists());
    let mut projects = std::fs::read_dir(state.join("projects")).unwrap();
    let root = projects.next().expect("one project state dir").unwrap().path();
    assert!(projects.next().is_none(), "expected a single project state dir");
    let registry: Value =
        serde_json::from_slice(&std::fs::read(root.join("sessions.json")).unwrap()).unwrap();
    assert_eq!(registry["sessions"].as_array().unwrap().len(), 0);
    assert!(
        std::fs::read_dir(root)
            .unwrap()
            .all(|entry| !entry.unwrap().file_type().unwrap().is_dir()),
        "temporary session directory was not removed"
    );
}

#[test]
fn commit_publishes_changes_and_forwards_command_output() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let output = run(
        project.path(),
        state.path(),
        "--commit",
        "printf child-out; printf child-err >&2; printf committed > result.txt",
    );

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "child-out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("child-err"), "{stderr}");
    assert!(stderr.contains("A result.txt"), "{stderr}");
    assert!(stderr.contains("deltabox: committed 1 path(s)"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("result.txt")).unwrap(),
        "committed"
    );
    assert_no_sessions(project.path(), state.path());
}

#[test]
fn abort_discards_changes() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let output = run(project.path(), state.path(), "--abort", "printf discarded > result.txt");

    assert!(output.status.success());
    assert!(!project.path().join("result.txt").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("A result.txt"), "{stderr}");
    assert!(stderr.contains("deltabox: aborted 1 change(s)"), "{stderr}");
    assert_no_sessions(project.path(), state.path());
}

#[test]
fn nonzero_command_is_committed_before_its_status_is_returned() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let output = run(
        project.path(),
        state.path(),
        "--commit",
        "printf kept > result.txt; exit 7",
    );

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        std::fs::read_to_string(project.path().join("result.txt")).unwrap(),
        "kept"
    );
    assert_no_sessions(project.path(), state.path());
}

#[test]
fn nonzero_command_is_aborted_before_its_status_is_returned() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let output = run(
        project.path(),
        state.path(),
        "--abort",
        "printf discarded > result.txt; exit 9",
    );

    assert_eq!(output.status.code(), Some(9));
    assert!(!project.path().join("result.txt").exists());
    assert_no_sessions(project.path(), state.path());
}

#[test]
fn noninteractive_run_requires_an_explicit_action_before_starting() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .env("DELTABOX_STATE_DIR", tempfile::tempdir().unwrap().path())
        .args([
            "run",
            "--project",
            project.path().to_str().unwrap(),
            "--fs-mode",
            "copy-private",
            "--",
            "true",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --commit or --abort"));
    assert!(!project.path().join(".deltabox").exists());
}

#[test]
fn commit_and_abort_are_mutually_exclusive() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .env("DELTABOX_STATE_DIR", tempfile::tempdir().unwrap().path())
        .args([
            "run",
            "--project",
            project.path().to_str().unwrap(),
            "--commit",
            "--abort",
            "--",
            "true",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    assert!(!project.path().join(".deltabox").exists());
}

#[test]
fn help_describes_the_temporary_transaction_and_argv_boundary() {
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["run", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("temporary transaction"), "{stdout}");
    assert!(stdout.contains("--commit"), "{stdout}");
    assert!(stdout.contains("--abort"), "{stdout}");
    assert!(stdout.contains("<COMMAND>..."), "{stdout}");
}

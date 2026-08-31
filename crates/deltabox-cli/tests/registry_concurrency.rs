#![cfg(any(target_os = "linux", target_os = "macos"))]

use serde_json::Value;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn start(project: &Path, state: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .env("DELTABOX_STATE_DIR", state)
        .args([
            "session",
            "start",
            project.to_str().unwrap(),
            "--fs-mode",
            "copy-private",
            "--require-fs-mode",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap()
        .to_string()
}

fn spawn_stop(project: &Path, state: &Path, sid: &str, pause: Option<&Path>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_deltabox"));
    command.env("DELTABOX_STATE_DIR", state).args([
        "session",
        "stop",
        sid,
        "--project",
        project.to_str().unwrap(),
    ]);
    if let Some(dir) = pause {
        command
            .env("DELTABOX_TEST_PAUSE_AT", "registry_loaded")
            .env("DELTABOX_TEST_PAUSE_DIR", dir);
    }
    command.spawn().unwrap()
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Two sessions mutate sessions.json concurrently. Session A's stop pauses
/// inside the locked registry update; session B's stop must block on
/// registry.lock instead of overwriting A's in-flight update. Without the
/// lock, A resumes with a stale registry view and B's stop is lost.
#[test]
fn concurrent_registry_updates_are_serialized_and_lossless() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let pause = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.txt"), "x").unwrap();

    let a = start(project.path(), state.path());
    let b = start(project.path(), state.path());

    let mut stop_a = spawn_stop(project.path(), state.path(), &a, Some(pause.path()));
    wait_for(&pause.path().join("registry_loaded.arrived"));

    let mut stop_b = spawn_stop(project.path(), state.path(), &b, None);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        stop_b.try_wait().unwrap().is_none(),
        "stop B finished while A held registry.lock — writes are not serialized"
    );

    std::fs::write(pause.path().join("registry_loaded.release"), b"").unwrap();
    assert!(stop_a.wait().unwrap().success());
    assert!(stop_b.wait().unwrap().success());

    let projects_dir = state.path().join("projects");
    let root = std::fs::read_dir(&projects_dir)
        .unwrap()
        .next()
        .expect("one project state dir")
        .unwrap()
        .path();
    let registry: Value =
        serde_json::from_slice(&std::fs::read(root.join("sessions.json")).unwrap()).unwrap();
    let sessions = registry["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    for session in sessions {
        assert_eq!(
            session["status"], "stopped",
            "lost update: {session} still running in the registry"
        );
    }
}

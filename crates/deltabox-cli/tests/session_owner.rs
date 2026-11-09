//! Session ownership: an attached owner killed with SIGKILL leaves an orphan
//! that keeps its changes and its project lease until a new owner adopts it.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use deltabox_core::coordinator::{CoordinatorOptions, SessionCoordinator};
use deltabox_core::protocol::{AbortParams, CommitParams, FilesystemMode};
use deltabox_core::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CHILD_PROJECT: &str = "DELTABOX_OWNER_CHILD_PROJECT";
const CHILD_MODE: &str = "DELTABOX_OWNER_CHILD_MODE";
const CHILD_READY: &str = "DELTABOX_OWNER_CHILD_READY";

fn coordinator(project: &Path, attach: bool) -> SessionCoordinator {
    SessionCoordinator::new_with_options(
        project,
        CoordinatorOptions {
            helper_bin: Some(PathBuf::from(env!("CARGO_BIN_EXE_deltabox"))),
            attach_sessions: attach,
            ..CoordinatorOptions::default()
        },
    )
    .unwrap()
}

fn mode_name(mode: FilesystemMode) -> &'static str {
    match mode {
        FilesystemMode::CbcowThrough => "cbcow-through",
        FilesystemMode::CbcowPrivate => "cbcow-private",
        FilesystemMode::CopyPrivate => "copy-private",
    }
}

fn cbcow_available() -> bool {
    #[cfg(target_os = "linux")]
    return deltafs_fuse::doctor::inspect().mount_ready;
    #[cfg(target_os = "macos")]
    return true;
}

/// Child half: run only when re-executed by `spawn_owner`. Attaches a
/// session, leaves one pending change, reports the id, then waits to be killed.
#[test]
fn owner_child_process() {
    let Some(project) = std::env::var_os(CHILD_PROJECT) else {
        return;
    };
    let mode = match std::env::var(CHILD_MODE).unwrap().as_str() {
        "cbcow-through" => FilesystemMode::CbcowThrough,
        "cbcow-private" => FilesystemMode::CbcowPrivate,
        _ => FilesystemMode::CopyPrivate,
    };
    let coordinator = coordinator(Path::new(&project), true);
    let info = coordinator.start(None, Some(mode), true).unwrap();
    coordinator
        .write_file(&info.session_id, Path::new("agent.txt"), b"work in progress".to_vec())
        .unwrap();
    let ready = PathBuf::from(std::env::var_os(CHILD_READY).unwrap());
    std::fs::write(ready.with_extension("tmp"), &info.session_id).unwrap();
    std::fs::rename(ready.with_extension("tmp"), &ready).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn spawn_owner(project: &Path, mode: FilesystemMode) -> (Child, String) {
    let ready = project.parent().unwrap().join("owner-ready");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["owner_child_process", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_PROJECT, project)
        .env(CHILD_MODE, mode_name(mode))
        .env(CHILD_READY, &ready)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "owner child never became ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let id = std::fs::read_to_string(&ready).unwrap();
    std::fs::remove_file(&ready).unwrap();
    (child, id)
}

fn sigkill(mut child: Child) {
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();
}

fn cli_start(project: &Path, mode: FilesystemMode) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_deltabox"))
        .args(["session", "start", project.to_str().unwrap(), "--fs-mode", mode_name(mode), "--require-fs-mode"])
        .output()
        .unwrap()
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("tracked.txt"), "accepted").unwrap();
    (root, project.canonicalize().unwrap())
}

#[test]
fn killed_through_owner_leaves_an_adoptable_orphan_that_blocks_admission() {
    if !cbcow_available() {
        eprintln!("skipping: CB-CoW filesystem unavailable");
        return;
    }
    let (_root, project) = project();
    let (child, id) = spawn_owner(&project, FilesystemMode::CbcowThrough);

    // While the owner lives: plain busy, and nobody can adopt.
    let output = cli_start(&project, FilesystemMode::CbcowThrough);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session busy"));
    let adopter = coordinator(&project, true);
    assert!(matches!(adopter.adopt(&id), Err(Error::SessionBusy(_))));
    assert!(adopter.orphaned_sessions().unwrap().is_empty());

    sigkill(child);

    // After SIGKILL: admission names the orphan instead of a stale "busy".
    let output = cli_start(&project, FilesystemMode::CbcowThrough);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session orphaned") && stderr.contains(&id), "{stderr}");
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![id.clone()]);

    // Adopt keeps the dead owner's work; the new owner decides.
    let info = adopter.adopt(&id).unwrap();
    assert!(info.pending_changes >= 1, "{info:?}");
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
    assert!(matches!(coordinator(&project, true).adopt(&id), Err(Error::SessionBusy(_))));

    adopter
        .abort(AbortParams { session_id: id.clone(), checkpoint: None })
        .unwrap();
    adopter.stop(&id).unwrap();
    assert!(!project.join("agent.txt").exists(), "abort rolled the host back");

    // The lease is released: the project admits a new through session.
    let fresh = adopter.start(None, Some(FilesystemMode::CbcowThrough), true).unwrap();
    adopter.stop(&fresh.session_id).unwrap();
}

#[test]
fn adopted_orphan_can_commit_the_dead_owners_work() {
    let (_root, project) = project();
    let (child, id) = spawn_owner(&project, FilesystemMode::CopyPrivate);
    sigkill(child);

    let adopter = coordinator(&project, true);
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![id.clone()]);
    let info = adopter.adopt(&id).unwrap();
    assert_eq!(info.pending_changes, 1);
    adopter
        .commit(CommitParams { session_id: id.clone(), checkpoint: None, ours: false })
        .unwrap();
    adopter.stop(&id).unwrap();
    assert_eq!(std::fs::read_to_string(project.join("agent.txt")).unwrap(), "work in progress");
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
}

#[test]
fn detached_sessions_are_not_orphans_and_cannot_be_adopted() {
    let (_root, project) = project();
    let detached = coordinator(&project, false);
    let info = detached.start(None, Some(FilesystemMode::CopyPrivate), true).unwrap();
    drop(detached); // the CLI exits right after start: still a live session

    let adopter = coordinator(&project, true);
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
    assert!(matches!(adopter.adopt(&info.session_id), Err(Error::SessionBusy(_))));
    adopter.stop(&info.session_id).unwrap();
}

#[test]
fn clean_drop_of_an_attached_coordinator_orphans_its_sessions() {
    let (_root, project) = project();
    let owner = coordinator(&project, true);
    let info = owner.start(None, Some(FilesystemMode::CopyPrivate), true).unwrap();
    // Adopting a session this coordinator already owns is a no-op.
    assert_eq!(owner.adopt(&info.session_id).unwrap().session_id, info.session_id);
    drop(owner);

    let adopter = coordinator(&project, true);
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![info.session_id.clone()]);
    adopter.adopt(&info.session_id).unwrap();
    adopter.stop(&info.session_id).unwrap();
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
    assert!(matches!(adopter.adopt(&info.session_id), Err(Error::SessionStopped(_))));
}

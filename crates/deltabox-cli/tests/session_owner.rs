//! Session ownership, end to end, on every platform.
//!
//! An attached owner (the Node addon's role) that dies without stopping —
//! SIGKILL on Unix, TerminateProcess on Windows: `Child::kill` is both — leaves
//! an orphan. The orphan keeps its pending changes and its project lease until
//! a new owner adopts it and the user commits or aborts.
//!
//! The owner is this test binary re-executed as a child (`owner_child_process`).
//! On Windows the coordinator uses trusted execution (`isolation: "none"`), so
//! these tests need WinFsp but not MXC PSEC and run on hosted CI.
#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use deltabox_core::coordinator::{CoordinatorOptions, SessionCoordinator};
use deltabox_core::protocol::{AbortParams, CommitParams, FilesystemMode};
use deltabox_core::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use std::{fs, thread};

const CHILD_ROOT: &str = "DELTABOX_OWNER_CHILD_ROOT";
const CHILD_MODE: &str = "DELTABOX_OWNER_CHILD_MODE";

/// A coordinator over `<root>/project`. Attached coordinators own the
/// sessions they start or adopt; detached ones behave like the CLI.
fn coordinator(root: &Path, attach: bool) -> SessionCoordinator {
    #[cfg(windows)]
    let options = CoordinatorOptions {
        command_execution: deltabox_core::coordinator::CommandExecutionMode::Trusted,
        // Windows requires state and runtime outside the project.
        state_dir: Some(root.join("state")),
        runtime_dir: Some(root.join("runtime")),
        ..CoordinatorOptions::default()
    };
    #[cfg(not(windows))]
    let options = CoordinatorOptions::default();
    SessionCoordinator::new_with_options(
        &root.join("project"),
        CoordinatorOptions {
            helper_bin: Some(PathBuf::from(env!("CARGO_BIN_EXE_deltabox"))),
            attach_sessions: attach,
            ..options
        },
    )
    .unwrap()
}

/// CB-CoW needs FUSE on Linux; macOS and Windows (WinFsp in CI) always have it.
fn cbcow_available() -> bool {
    #[cfg(target_os = "linux")]
    return deltafs_fuse::doctor::inspect().mount_ready;
    #[cfg(not(target_os = "linux"))]
    return true;
}

fn mode_name(mode: FilesystemMode) -> &'static str {
    match mode {
        FilesystemMode::CbcowThrough => "cbcow-through",
        FilesystemMode::CbcowPrivate => "cbcow-private",
        FilesystemMode::CopyPrivate => "copy-private",
    }
}

fn parse_mode(name: &str) -> FilesystemMode {
    match name {
        "cbcow-through" => FilesystemMode::CbcowThrough,
        "cbcow-private" => FilesystemMode::CbcowPrivate,
        _ => FilesystemMode::CopyPrivate,
    }
}

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("project")).unwrap();
    fs::write(root.path().join("project").join("tracked.txt"), "accepted").unwrap();
    root
}

/// Child half: runs only when re-executed by `spawn_owner`. Attaches a
/// session, leaves one pending change, reports the id, then waits to be killed.
#[test]
fn owner_child_process() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let coordinator = coordinator(&root, true);
    let mode = parse_mode(&std::env::var(CHILD_MODE).unwrap());
    let info = coordinator.start(None, Some(mode), true).unwrap();
    coordinator
        .write_file(&info.session_id, Path::new("agent.txt"), b"work in progress".to_vec())
        .unwrap();
    fs::write(root.join("owner-ready.tmp"), &info.session_id).unwrap();
    fs::rename(root.join("owner-ready.tmp"), root.join("owner-ready")).unwrap();
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn spawn_owner(root: &Path, mode: FilesystemMode) -> (Child, String) {
    let ready = root.join("owner-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["owner_child_process", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ROOT, root)
        .env(CHILD_MODE, mode_name(mode))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("owner child exited before it was ready: {status}");
        }
        assert!(Instant::now() < deadline, "owner child never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    let id = fs::read_to_string(&ready).unwrap();
    fs::remove_file(&ready).unwrap();
    (child, id)
}

/// SIGKILL on Unix, TerminateProcess on Windows: no destructor, no stop.
fn kill(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Stops whatever sessions a test leaves running, even when it panics, so no
/// filesystem helper outlives the test or keeps its temporary directory open.
struct Cleanup<'a>(&'a SessionCoordinator, Vec<String>);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for id in &self.1 {
            let _ = self.0.adopt(id);
            let _ = self.0.abort(AbortParams { session_id: id.clone(), checkpoint: None });
            let _ = self.0.stop(id);
        }
    }
}

/// The mode for scenarios that only need *a* session. Windows trusted
/// execution has no copy-private, and Unix copy-private needs no FUSE.
fn any_mode() -> FilesystemMode {
    if cfg!(windows) {
        FilesystemMode::CbcowPrivate
    } else {
        FilesystemMode::CopyPrivate
    }
}

#[test]
fn killed_through_owner_leaves_an_adoptable_orphan_that_blocks_admission() {
    if !cbcow_available() {
        eprintln!("skipping: CB-CoW filesystem unavailable");
        return;
    }
    let root = project();
    let project = root.path().join("project");
    let (child, id) = spawn_owner(root.path(), FilesystemMode::CbcowThrough);
    let adopter = coordinator(root.path(), true);
    let _cleanup = Cleanup(&adopter, vec![id.clone()]);
    // Through mode writes the host at once.
    assert_eq!(fs::read(project.join("agent.txt")).unwrap(), b"work in progress");

    // While the owner lives: plain busy, and nobody can adopt.
    let error = adopter.start(None, Some(FilesystemMode::CbcowThrough), true).unwrap_err();
    assert!(matches!(error, Error::SessionBusy(_)), "{error}");
    assert!(matches!(adopter.adopt(&id), Err(Error::SessionBusy(_))));
    assert!(adopter.orphaned_sessions().unwrap().is_empty());

    kill(child);

    // After the kill: every new start names the orphan, whatever its mode,
    // instead of a stale "busy".
    for requested in [FilesystemMode::CbcowThrough, FilesystemMode::CbcowPrivate] {
        let error = coordinator(root.path(), false).start(None, Some(requested), true).unwrap_err();
        assert!(
            matches!(&error, Error::SessionOrphaned(orphan) if *orphan == id),
            "{requested:?}: {error}"
        );
        assert!(error.to_string().starts_with("session orphaned:"), "{error}");
    }
    // The CLI reports it the same way. (Windows CLI sessions are sandboxed
    // and need MXC PSEC before they even reach admission.)
    #[cfg(unix)]
    {
        let output = Command::new(env!("CARGO_BIN_EXE_deltabox"))
            .args(["session", "start", project.to_str().unwrap(), "--fs-mode", "cbcow-through", "--require-fs-mode"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("session orphaned") && stderr.contains(&id), "{stderr}");
    }
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![id.clone()]);

    // Adopt keeps the dead owner's work; a second adopter is refused.
    let info = adopter.adopt(&id).unwrap();
    assert!(info.pending_changes >= 1, "{info:?}");
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
    assert!(matches!(coordinator(root.path(), true).adopt(&id), Err(Error::SessionBusy(_))));

    // The user chose abort: the host rolls back and the lease is released.
    adopter.abort(AbortParams { session_id: id.clone(), checkpoint: None }).unwrap();
    adopter.stop(&id).unwrap();
    assert!(!project.join("agent.txt").exists(), "abort rolled the host back");
    let fresh = adopter.start(None, Some(FilesystemMode::CbcowThrough), true).unwrap();
    adopter.stop(&fresh.session_id).unwrap();
}

#[test]
fn killed_private_owner_is_adopted_and_its_work_committed() {
    if !cbcow_available() {
        eprintln!("skipping: CB-CoW filesystem unavailable");
        return;
    }
    let root = project();
    let project = root.path().join("project");
    let (child, id) = spawn_owner(root.path(), FilesystemMode::CbcowPrivate);
    let adopter = coordinator(root.path(), true);
    let _cleanup = Cleanup(&adopter, vec![id.clone()]);
    assert!(!project.join("agent.txt").exists(), "private work stays off the host");

    kill(child);

    // An orphaned private session does not block a private sibling...
    let sibling = adopter.start(None, Some(FilesystemMode::CbcowPrivate), true).unwrap();
    adopter.stop(&sibling.session_id).unwrap();
    // ...but it is listed, so its work is not silently forgotten.
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![id.clone()]);

    // The user chose commit: the dead owner's work reaches the host.
    assert!(adopter.adopt(&id).unwrap().pending_changes >= 1);
    adopter.commit(CommitParams { session_id: id.clone(), checkpoint: None, ours: false }).unwrap();
    adopter.stop(&id).unwrap();
    assert_eq!(fs::read(project.join("agent.txt")).unwrap(), b"work in progress");
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn killed_copy_private_owner_is_adopted_and_its_work_committed() {
    let root = project();
    let project = root.path().join("project");
    let (child, id) = spawn_owner(root.path(), FilesystemMode::CopyPrivate);
    kill(child);

    let adopter = coordinator(root.path(), true);
    let _cleanup = Cleanup(&adopter, vec![id.clone()]);
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![id.clone()]);
    assert_eq!(adopter.adopt(&id).unwrap().pending_changes, 1);
    adopter.commit(CommitParams { session_id: id.clone(), checkpoint: None, ours: false }).unwrap();
    adopter.stop(&id).unwrap();
    assert_eq!(fs::read(project.join("agent.txt")).unwrap(), b"work in progress");
}

#[test]
fn detached_sessions_are_not_orphans_and_cannot_be_adopted() {
    let root = project();
    let detached = coordinator(root.path(), false);
    let info = detached.start(None, Some(any_mode()), true).unwrap();
    drop(detached); // the CLI exits right after start: still a live session

    let adopter = coordinator(root.path(), true);
    let _cleanup = Cleanup(&adopter, vec![info.session_id.clone()]);
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
    assert!(matches!(adopter.adopt(&info.session_id), Err(Error::SessionBusy(_))));
    adopter.stop(&info.session_id).unwrap();
}

#[test]
fn clean_drop_of_an_attached_coordinator_orphans_its_sessions() {
    let root = project();
    let owner = coordinator(root.path(), true);
    let info = owner.start(None, Some(any_mode()), true).unwrap();
    // Adopting a session this coordinator already owns is a no-op.
    assert_eq!(owner.adopt(&info.session_id).unwrap().session_id, info.session_id);
    drop(owner);

    let adopter = coordinator(root.path(), true);
    let _cleanup = Cleanup(&adopter, vec![info.session_id.clone()]);
    assert_eq!(adopter.orphaned_sessions().unwrap(), vec![info.session_id.clone()]);
    adopter.adopt(&info.session_id).unwrap();
    adopter.stop(&info.session_id).unwrap();
    assert!(adopter.orphaned_sessions().unwrap().is_empty());
    assert!(matches!(adopter.adopt(&info.session_id), Err(Error::SessionStopped(_))));
}

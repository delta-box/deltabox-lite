#![cfg(target_os = "linux")]

use deltabox_core::backend::linux_sandlock::SandlockBackend;
use deltabox_core::protocol::{
    AbortParams, CheckpointParams, CommitParams, DiffParams, ExecParams,
};
use deltabox_core::session::SessionManager;
use std::path::PathBuf;

fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_deltabox"))
}

fn start_project() -> (tempfile::TempDir, SessionManager, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join("del.txt"), "delete me\n").unwrap();
    let backend = SandlockBackend::new(Some(worker_bin()));
    let mut manager = SessionManager::new(dir.path(), Box::new(backend)).unwrap();
    let info = manager.start(None).unwrap();
    (dir, manager, info.session_id)
}

fn exec_sh(manager: &mut SessionManager, session_id: &str, script: &str) -> (i32, String, String) {
    let result = manager
        .exec(ExecParams {
            session_id: session_id.to_string(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: None,
            env: None,
        })
        .unwrap();
    (result.exit_code.unwrap_or(-1), result.stdout, result.stderr)
}

#[test]
fn exec_captures_output_and_keeps_host_clean() {
    let (dir, mut manager, sid) = start_project();
    let (code, stdout, stderr) = exec_sh(
        &mut manager,
        &sid,
        "echo out; echo err 1>&2; printf x > created.txt",
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("out"));
    assert!(stderr.contains("err"));
    assert!(!dir.path().join("created.txt").exists());

    let diff = manager
        .diff(DiffParams {
            session_id: sid.clone(),
            from: None,
            to: None,
        })
        .unwrap();
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "created.txt");
    assert!(matches!(
        diff.changes[0].kind,
        deltabox_core::changeset::ChangeKind::Added
    ));

    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: Some("c1".into()),
        })
        .unwrap();
    manager
        .abort(AbortParams {
            session_id: sid,
            checkpoint: None,
        })
        .unwrap();
    assert!(!dir.path().join("created.txt").exists());
}

#[test]
fn checkpoint_abort_roundtrip() {
    let (dir, mut manager, sid) = start_project();
    exec_sh(&mut manager, &sid, "printf 'v1' > a.txt");
    let ckpt = manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: Some("before-v2".into()),
        })
        .unwrap();
    exec_sh(&mut manager, &sid, "printf 'v2' > a.txt; printf x > b.txt");

    let diff = manager
        .diff(DiffParams {
            session_id: sid.clone(),
            from: None,
            to: None,
        })
        .unwrap();
    assert_eq!(diff.changes.len(), 2);
    let a = diff.changes.iter().find(|c| c.path == "a.txt").unwrap();
    assert!(a.text_diff.is_some());

    manager
        .abort(AbortParams {
            session_id: sid.clone(),
            checkpoint: Some(ckpt.id),
        })
        .unwrap();
    let (code, out, _) = exec_sh(&mut manager, &sid, "cat a.txt");
    assert_eq!(code, 0);
    assert_eq!(out, "v1");
    let (code, _, _) = exec_sh(&mut manager, &sid, "test -e b.txt");
    assert_ne!(code, 0, "b.txt must be gone after abort");
}

#[test]
fn commit_applies_to_host() {
    let (dir, mut manager, sid) = start_project();
    exec_sh(&mut manager, &sid, "printf 'committed' > committed.txt");
    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: None,
        })
        .unwrap();
    let result = manager
        .commit(CommitParams {
            session_id: sid.clone(),
            checkpoint: None,
        })
        .unwrap();
    assert!(result.applied >= 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("committed.txt")).unwrap(),
        "committed"
    );
    assert_eq!(manager.status(&sid).unwrap().pending_changes, 0);
}

#[test]
fn delete_survives_checkpoint_and_abort() {
    let (dir, mut manager, sid) = start_project();
    exec_sh(&mut manager, &sid, "rm del.txt");
    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: Some("after-rm".into()),
        })
        .unwrap();
    exec_sh(&mut manager, &sid, "printf x > new.txt");
    manager
        .abort(AbortParams {
            session_id: sid.clone(),
            checkpoint: None,
        })
        .unwrap();
    assert!(!dir.path().join("del.txt").exists());
    assert!(!dir.path().join("new.txt").exists());
}

#[test]
fn secrets_are_hard_denied() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
    let backend = SandlockBackend::new(Some(worker_bin()));
    let mut manager = SessionManager::new(dir.path(), Box::new(backend)).unwrap();
    let info = manager.start(None).unwrap();
    let sid = info.session_id;
    let (code, _, _) = exec_sh(&mut manager, &sid, "cat .env");
    assert_ne!(code, 0, "reading .env must be denied");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".env")).unwrap(),
        "SECRET=1\n"
    );
}

#[test]
fn stop_discards_pending() {
    let (dir, mut manager, sid) = start_project();
    exec_sh(&mut manager, &sid, "printf x > pending.txt");
    manager.stop(&sid).unwrap();
    assert!(!dir.path().join("pending.txt").exists());
}

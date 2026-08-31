#![cfg(any(target_os = "linux", target_os = "macos"))]

#[cfg(target_os = "linux")]
use deltabox_core::backend::linux_sandlock::SandlockBackend;
#[cfg(target_os = "macos")]
use deltabox_core::backend::macos_seatbelt::SeatbeltBackend;
use deltabox_core::backend::SandboxBackend;
use deltabox_core::protocol::{
    AbortParams, CheckpointParams, CommitParams, DiffParams, ExecParams,
};
use deltabox_core::session::SessionManager;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_deltabox"))
}

fn native_backend() -> Box<dyn SandboxBackend> {
    #[cfg(target_os = "linux")]
    return Box::new(SandlockBackend::new(Some(worker_bin())));
    #[cfg(target_os = "macos")]
    return Box::new(SeatbeltBackend::new());
}

fn start_project() -> (tempfile::TempDir, SessionManager, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join("del.txt"), "delete me\n").unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
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
            label: None,
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
    let (_dir, mut manager, sid) = start_project();
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
            ours: false,
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
fn deletion_state_survives_checkpoint_and_abort() {
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
    let (deleted_code, _, _) = exec_sh(&mut manager, &sid, "test -e del.txt");
    let (new_code, _, _) = exec_sh(&mut manager, &sid, "test -e new.txt");
    assert_ne!(
        deleted_code, 0,
        "checkpoint deletion remains visible after abort"
    );
    assert_ne!(new_code, 0, "post-checkpoint file is removed by abort");
    #[cfg(target_os = "linux")]
    assert!(!dir.path().join("del.txt").exists());
    #[cfg(target_os = "macos")]
    assert!(
        dir.path().join("del.txt").exists(),
        "macOS keeps the real project untouched until commit"
    );
}

#[test]
fn secrets_are_hard_denied() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
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

#[test]
fn checkpoint_before_deletion_abort_restores() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
    std::fs::write(dir.path().join("sub/deep/a.txt"), "a").unwrap();
    std::fs::write(dir.path().join("sub/b.txt"), "b").unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
    let info = manager.start(None).unwrap();
    let sid = info.session_id;

    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: Some("before-rm".into()),
        })
        .unwrap();
    let (code, stdout, stderr) = exec_sh(&mut manager, &sid, "rm -rf sub");
    assert_eq!(
        code, 0,
        "rm -rf failed: stdout={stdout:?} stderr={stderr:?}"
    );
    for script in ["cat sub/b.txt", "ls sub/b.txt", "test -e sub/b.txt"] {
        let (code, stdout, stderr) = exec_sh(&mut manager, &sid, script);
        assert_ne!(
            code, 0,
            "{script:?} saw a file deleted inside the sandbox: stdout={stdout:?} stderr={stderr:?}"
        );
    }

    manager
        .abort(AbortParams {
            session_id: sid.clone(),
            checkpoint: None,
        })
        .unwrap();
    let (code, _, _) = exec_sh(&mut manager, &sid, "test -f sub/deep/a.txt");
    assert_eq!(code, 0, "abort restores files deleted after the checkpoint");
    assert!(dir.path().join("sub/deep/a.txt").exists());
}

#[test]
fn delete_then_checkpoint_then_commit_applies_deletion() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.txt"), "b").unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
    let info = manager.start(None).unwrap();
    let sid = info.session_id;

    exec_sh(&mut manager, &sid, "rm -rf sub");
    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: Some("after-rm".into()),
        })
        .unwrap();
    manager
        .commit(CommitParams {
            session_id: sid.clone(),
            checkpoint: None,
            ours: false,
        })
        .unwrap();
    assert!(
        !dir.path().join("sub/b.txt").exists(),
        "commit applies the deletion to the host project"
    );
}

#[test]
fn binary_edit_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join("bin.dat"), [1u8, 2, 3]).unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
    let info = manager.start(None).unwrap();
    let sid = info.session_id;

    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: Some("before-bin-edit".into()),
        })
        .unwrap();
    exec_sh(&mut manager, &sid, "printf '\\000\\002\\003' > bin.dat");
    let diff = manager
        .diff(DiffParams {
            session_id: sid.clone(),
            from: None,
            to: None,
        })
        .unwrap();
    let bin = diff.changes.iter().find(|c| c.path == "bin.dat").unwrap();
    assert!(bin.is_binary, "binary file is marked, not text-diffed");
    assert!(bin.text_diff.is_none());

    manager
        .abort(AbortParams {
            session_id: sid.clone(),
            checkpoint: None,
        })
        .unwrap();
    let (code, out, _) = exec_sh(&mut manager, &sid, "od -An -tx1 bin.dat | tr -d ' \\n'");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "010203", "abort restores binary content");
}

#[test]
fn exec_cwd_and_env() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
    let info = manager.start(None).unwrap();
    let sid = info.session_id;

    let result = manager
        .exec(ExecParams {
            session_id: sid.clone(),
            argv: vec![
                "sh".into(),
                "-c".into(),
                "pwd; printf '%s' \"$DBX_VAR\"".into(),
            ],
            cwd: Some(dir.path().join("sub")),
            env: Some(vec![("DBX_VAR".into(), "hello-env".into())]),
            label: Some("cwd-env".into()),
        })
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("sub"), "cwd passthrough");
    assert!(result.stdout.contains("hello-env"), "env override");
}

#[test]
fn fs_read_write_roundtrip() {
    let (dir, mut manager, sid) = start_project();
    let path = dir.path().join("new.txt");
    manager
        .write_file(&sid, &path, b"tracked content".to_vec())
        .unwrap();
    let result = manager.read_file(&sid, &path).unwrap();
    assert_eq!(result.size, 15);
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&result.content_base64)
        .unwrap();
    assert_eq!(bytes, b"tracked content");
    assert!(!path.exists(), "write stays in the COW layer until commit");

    let diff = manager
        .diff(DiffParams {
            session_id: sid.clone(),
            from: None,
            to: None,
        })
        .unwrap();
    assert!(diff.changes.iter().any(|c| c.path == "new.txt"));

    manager
        .checkpoint(CheckpointParams {
            session_id: sid.clone(),
            label: None,
        })
        .unwrap();
    manager
        .commit(CommitParams {
            session_id: sid.clone(),
            checkpoint: None,
            ours: false,
        })
        .unwrap();
    assert!(path.exists());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "tracked content");
}

#[test]
fn fs_ops_deny_secrets_and_outside_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
    let mut manager = SessionManager::new(dir.path(), native_backend()).unwrap();
    let info = manager.start(None).unwrap();
    let sid = info.session_id;

    let read = manager.read_file(&sid, &dir.path().join(".env"));
    assert!(read.is_err(), "fs.read must deny secret paths");
    let write = manager.write_file(&sid, &dir.path().join(".env"), b"x".to_vec());
    assert!(write.is_err(), "fs.write must deny secret paths");
    let outside = manager.read_file(&sid, &PathBuf::from("/etc/hostname"));
    assert!(outside.is_err(), "fs.read outside project must fail");
}

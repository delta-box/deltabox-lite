use super::engine::*;
use crate::policy::Policy;
use crate::protocol::{CheckpointInfo, DurabilityMode, SessionStatus};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};

struct Fixture { root: tempfile::TempDir, engine: Engine }
impl Fixture {
    fn new() -> Self {
        let root=tempfile::tempdir().unwrap();
        let source=root.path().join("source");fs::create_dir(&source).unwrap();
        let checkpoint="initial".to_owned();
        let manifest=Manifest {version:1,session_id:uuid::Uuid::new_v4().to_string(),source:source.canonicalize().unwrap(),fs_mode:crate::protocol::FilesystemMode::CbcowThrough,trusted:false,durability:DurabilityMode::Fast,live:None,view:root.path().join("view"),scratch:root.path().join("scratch"),token:"test-token".into(),started_at_ms:now_ms(),status:SessionStatus::Running,policy:Policy::default_agent(),next_id:1,accepted:checkpoint.clone(),checkpoints:vec![Savepoint {info:CheckpointInfo {id:checkpoint,label:None,created_at_ms:now_ms(),change_count:0,committed:true},cursor:0}],review:None,rollback:None};
        let engine=Engine::create(&root.path().join("state"),manifest).unwrap();
        Self {root,engine}
    }
    fn source(&self,name:&str)->std::path::PathBuf {self.engine.manifest.source.join(name)}
    fn write(&mut self,name:&str,offset:u64,data:&[u8]) {
        let record=self.engine.prepare(WRITE,name,"",offset,data.len() as u64).unwrap();
        let mut f=OpenOptions::new().write(true).open(self.engine.path(name,true).unwrap()).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();f.write_all(data).unwrap();drop(f);
        self.engine.finish(record,0).unwrap();
    }
}

impl Fixture {
    fn private(files: &[(&str, &[u8])]) -> Self {
        Self::private_with(files, DurabilityMode::Fast)
    }
    fn private_with(files: &[(&str, &[u8])], durability: DurabilityMode) -> Self {
        let mut fixture = Self::new();
        for (name, bytes) in files {
            let path = fixture.source(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let mut manifest = fixture.engine.manifest.clone();
        manifest.fs_mode = crate::protocol::FilesystemMode::CbcowPrivate;
        manifest.durability = durability;
        fixture.engine = Engine::create(&fixture.root.path().join("private"), manifest).unwrap();
        fixture
    }
    fn sibling(&self) -> Engine {
        let mut manifest = self.engine.manifest.clone();
        manifest.session_id = uuid::Uuid::new_v4().to_string();
        manifest.live = None;
        Engine::create(&self.root.path().join(&manifest.session_id), manifest).unwrap()
    }
    fn live(&self, name: &str) -> std::path::PathBuf { self.engine.manifest.live().join(name) }
    fn create(&mut self, name: &str, bytes: &[u8]) {
        let record = self.engine.prepare(CREATE, name, "", 0, 0).unwrap();
        fs::write(self.live(name), bytes).unwrap();
        self.engine.finish(record, 0).unwrap();
    }
    fn rename(&mut self, old: &str, new: &str) {
        let record = self.engine.prepare(RENAME, old, new, 0, 0).unwrap();
        fs::rename(self.live(old), self.live(new)).unwrap();
        self.engine.finish(record, 0).unwrap();
    }
}

#[test]
fn private_writes_abort_and_commit_stay_isolated_across_restarts() {
    let mut f = Fixture::private(&[("a", b"old")]);
    f.write("a", 0, b"new");
    f.create("new.txt", b"created");
    assert_eq!(fs::read(f.source("a")).unwrap(), b"old");
    assert!(!f.source("new.txt").exists());
    f.engine = Engine::open(&f.engine.state).unwrap();
    assert_eq!(f.engine.diff(None, None).unwrap().pending_changes, 2);
    f.engine.abort(None).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    assert!(!f.live("new.txt").exists());
    f.write("a", 0, b"new");
    let (_, applied, _) = f.engine.commit_private(None, false).unwrap();
    assert_eq!(applied, 1);
    assert_eq!(fs::read(f.source("a")).unwrap(), b"new");
    f.write("a", 0, b"bad");
    f.engine = Engine::open(&f.engine.state).unwrap();
    f.engine.abort(None).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"new");
    assert_eq!(fs::read(f.source("a")).unwrap(), b"new");
}

#[test]
fn private_checkpoint_commit_publishes_the_selected_version_and_keeps_later_undo() {
    let mut f = Fixture::private(&[("a", b"old")]);
    f.write("a", 0, b"one");
    let checkpoint = f.engine.checkpoint(None).unwrap();
    f.write("a", 0, b"two");
    f.engine.commit(Some(&checkpoint.id)).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(), b"one");
    assert_eq!(fs::read(f.live("a")).unwrap(), b"two");
    f.engine = Engine::open(&f.engine.state).unwrap();
    f.engine.abort(None).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"one");
}

#[test]
fn private_disjoint_commits_preserve_sibling_work_and_overlap_conflicts() {
    let mut f = Fixture::private(&[("dir/a", b"old"), ("dir/b", b"old")]);
    let mut sibling = f.sibling();
    f.write("dir\\a", 0, b"one");
    sibling.write_draft("dir\\b", b"two").unwrap();
    f.engine.commit(None).unwrap();
    let (_, _, folded) = sibling.commit_private(None, false).unwrap();
    assert_eq!(folded, 1);
    assert_eq!(fs::read(f.source("dir/a")).unwrap(), b"one");
    assert_eq!(fs::read(f.source("dir/b")).unwrap(), b"two");
    f.write("dir\\b", 0, b"bad");
    let error = f.engine.commit(None).unwrap_err();
    assert!(matches!(error, crate::Error::ProjectConflict(_)), "{error}");
    assert_eq!(fs::read(f.source("dir/b")).unwrap(), b"two");
    assert_eq!(fs::read(f.live("dir/b")).unwrap(), b"bad");
}

#[test]
fn private_sync_materializes_rollbackable_drafts_and_never_publishes_them() {
    let mut f = Fixture::private(&[("a", b"old"), ("b", b"old")]);
    let mut sibling = f.sibling();
    sibling.write_draft("a", b"theirs").unwrap();
    sibling.write_draft("b", b"stale").unwrap();
    sibling.commit(None).unwrap();
    f.write("a", 0, b"our");
    let sync = f.engine.sync().unwrap();
    assert_eq!(sync.folded_publications, 1);
    assert_eq!(sync.stale_paths, ["b"]);
    assert_eq!(sync.conflicts.len(), 1);
    assert_eq!(sync.conflicts[0].theirs.as_deref(), Some("a.deltabox-theirs"));
    assert_eq!(fs::read(f.live("a.deltabox-theirs")).unwrap(), b"theirs");
    f.engine.commit(None).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(), b"our");
    assert_eq!(fs::read(f.source("b")).unwrap(), b"stale");
    assert!(!f.source("a.deltabox-theirs").exists());
    assert_eq!(f.engine.sync().unwrap().folded_publications, 0);
}

#[test]
fn private_ours_accepts_only_endorsed_sibling_content() {
    let mut f = Fixture::private(&[("a", b"old")]);
    let mut sibling = f.sibling();
    sibling.write_draft("a", b"sibling").unwrap();
    sibling.commit(None).unwrap();
    f.write("a", 0, b"our");
    f.engine.commit_private(None, true).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(), b"our");
    fs::write(f.source("a"), b"external").unwrap();
    f.write("a", 0, b"bad");
    assert!(matches!(f.engine.commit_private(None, true), Err(crate::Error::ProjectConflict(_))));
    assert_eq!(fs::read(f.source("a")).unwrap(), b"external");
}

#[test]
fn private_conflicts_use_content_even_when_size_and_timestamps_match() {
    let mut f = Fixture::private(&[("a", b"old")]);
    let metadata = Metadata::read(&f.source("a")).unwrap();
    f.write("a", 0, b"our");
    fs::write(f.source("a"), b"bad").unwrap();
    metadata.restore(&f.source("a")).unwrap();
    assert!(matches!(f.engine.commit(None), Err(crate::Error::ProjectConflict(_))));
    assert_eq!(fs::read(f.source("a")).unwrap(), b"bad");
}

#[test]
fn private_commit_preserves_timestamp_only_changes() {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::SetFileTime;
    let mut f = Fixture::private(&[("a", b"old")]);
    let old = fs::metadata(f.source("a")).unwrap().last_write_time();
    let changed = old - 100_000_000;
    let record = f.engine.prepare(METADATA, "a", "", 0, 0).unwrap();
    let file = OpenOptions::new().write(true).open(f.live("a")).unwrap();
    let time = FILETIME { dwLowDateTime: changed as u32, dwHighDateTime: (changed >> 32) as u32 };
    assert_ne!(unsafe { SetFileTime(file.as_raw_handle(), std::ptr::null(), std::ptr::null(), &time) }, 0);
    drop(file);
    f.engine.finish(record, 0).unwrap();
    assert_eq!(f.engine.diff(None, None).unwrap().pending_changes, 1);
    assert_eq!(fs::metadata(f.source("a")).unwrap().last_write_time(), old);
    f.engine.commit(None).unwrap();
    assert_eq!(fs::metadata(f.source("a")).unwrap().last_write_time(), changed);
    assert_eq!(fs::read(f.source("a")).unwrap(), b"old");
}

#[test]
fn private_directory_rename_checks_descendants_and_publishes_only_its_subtree() {
    let mut f = Fixture::private(&[("dir/a", b"old"), ("unrelated", b"old")]);
    f.rename("dir", "moved");
    fs::write(f.source("dir/external"), b"new child").unwrap();
    assert!(matches!(f.engine.commit(None), Err(crate::Error::ProjectConflict(_))));
    assert!(f.source("dir/external").exists());
    fs::remove_file(f.source("dir/external")).unwrap();
    fs::write(f.source("unrelated"), b"external").unwrap();
    f.engine.commit(None).unwrap();
    assert!(!f.source("dir").exists());
    assert_eq!(fs::read(f.source("moved/a")).unwrap(), b"old");
    assert_eq!(fs::read(f.source("unrelated")).unwrap(), b"external");
}

#[test]
fn private_drafts_inside_new_directories_never_cross_the_host_boundary() {
    let mut f = Fixture::private(&[]);
    let record = f.engine.prepare(CREATE, "new", "", 0, 0).unwrap();
    fs::create_dir(f.live("new")).unwrap();
    f.engine.finish(record, 0).unwrap();
    f.create("new\\file", b"publish");
    f.create("new\\file.deltabox-theirs", b"keep private");
    f.engine.commit(None).unwrap();
    assert_eq!(fs::read(f.source("new/file")).unwrap(), b"publish");
    assert!(!f.source("new/file.deltabox-theirs").exists());
    assert!(f.live("new/file.deltabox-theirs").exists());
}

#[test]
fn private_publish_crash_child() {
    let Some(state) = std::env::var_os("DELTABOX_TEST_PRIVATE_STATE") else { return; };
    Engine::open(std::path::Path::new(&state)).unwrap().commit(None).unwrap();
    panic!("the requested crash point was not reached");
}

#[test]
fn private_publication_recovers_both_sides_of_the_commit_decision() {
    for point in ["private_publish_path_applied", "private_publish_committed"] {
        let mut f = Fixture::private(&[("a", b"old"), ("b", b"old")]);
        f.write("a", 0, b"new");
        f.write("b", 0, b"new");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "windows::engine_tests::private_publish_crash_child", "--nocapture"])
            .env("DELTABOX_TEST_PRIVATE_STATE", &f.engine.state)
            .env("DELTABOX_TEST_CRASH_POINT", point).output().unwrap();
        assert_eq!(output.status.code(), Some(86), "{}", String::from_utf8_lossy(&output.stderr));
        f.engine = Engine::open(&f.engine.state).unwrap();
        let committed = point == "private_publish_committed";
        for name in ["a", "b"] {
            assert_eq!(fs::read(f.source(name)).unwrap(), if committed { b"new" } else { b"old" });
            assert_eq!(fs::read(f.live(name)).unwrap(), b"new");
        }
        assert_eq!(f.engine.diff(None, None).unwrap().pending_changes, if committed { 0 } else { 2 });
        f.engine.abort(None).unwrap();
        assert!(!f.root.path().join("publish.json").exists());
    }
}

#[test]
fn legacy_windows_manifests_default_to_through_mode() {
    let f = Fixture::new();
    let mut value = serde_json::to_value(&f.engine.manifest).unwrap();
    value.as_object_mut().unwrap().remove("fs_mode");
    value.as_object_mut().unwrap().remove("durability");
    let legacy: Manifest = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(legacy.durability, DurabilityMode::Fast);
    value.as_object_mut().unwrap().remove("live");
    let manifest: Manifest = serde_json::from_value(value).unwrap();
    assert_eq!(manifest.fs_mode, crate::protocol::FilesystemMode::CbcowThrough);
    assert_eq!(manifest.live(), manifest.source);
}

#[test]
fn small_write_to_large_file_preserves_only_the_overwritten_range() {
    let mut f=Fixture::new();let file=File::create(f.source("large.bin")).unwrap();file.set_len(64*1024*1024).unwrap();drop(file);
    f.write("large.bin",32*1024*1024,&[42;4096]);
    assert_eq!(f.engine.preimage_bytes(),4096);
    f.engine.abort(None).unwrap();
    let mut file=File::open(f.source("large.bin")).unwrap();file.seek(SeekFrom::Start(32*1024*1024)).unwrap();
    let mut bytes=[1;4096];std::io::Read::read_exact(&mut file,&mut bytes).unwrap();assert_eq!(bytes,[0;4096]);
    assert_eq!(file.metadata().unwrap().len(),64*1024*1024);
}

#[test]
fn incomplete_overlapping_write_recovers_the_last_completed_operation() {
    let mut f=Fixture::new();fs::write(f.source("a"),b"abcdefgh").unwrap();
    f.write("a",2,b"XY");
    let _record=f.engine.prepare(WRITE,"a","",3,3).unwrap();
    let mut file=OpenOptions::new().write(true).open(f.source("a")).unwrap();file.seek(SeekFrom::Start(3)).unwrap();file.write_all(b"!!").unwrap();drop(file);
    let mut reopened=Engine::open(&f.root.path().join("state")).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(),b"abXYefgh");
    reopened.abort(None).unwrap();assert_eq!(fs::read(f.source("a")).unwrap(),b"abcdefgh");
}

#[test]
fn checkpoint_survives_restart_and_truncation_restores_the_tail() {
    let mut f=Fixture::new();fs::write(f.source("a"),b"abcdefgh").unwrap();f.write("a",0,b"XY");
    let checkpoint=f.engine.checkpoint(Some("saved".into())).unwrap();
    let record=f.engine.prepare(RESIZE,"a","",3,0).unwrap();OpenOptions::new().write(true).open(f.source("a")).unwrap().set_len(3).unwrap();f.engine.finish(record,0).unwrap();
    f.write("a",3,b"appended");
    let mut reopened=Engine::open(&f.root.path().join("state")).unwrap();reopened.abort(Some(&checkpoint.id)).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(),b"XYcdefgh");
    reopened.abort(None).unwrap();assert_eq!(fs::read(f.source("a")).unwrap(),b"abcdefgh");
}

#[test]
fn replacement_rename_recovery_is_repeatable() {
    let f=Fixture::new();fs::write(f.source("old"),b"source").unwrap();fs::write(f.source("new"),b"destination").unwrap();
    let mut e=f.engine;
    let _record=e.prepare(RENAME,"old","new",0,0).unwrap();
    fs::rename(e.manifest.source.join("old"),e.manifest.source.join("new")).unwrap();
    for _ in 0..2 {
        e=Engine::open(&f.root.path().join("state")).unwrap();
        assert_eq!(fs::read(e.manifest.source.join("old")).unwrap(),b"source");
        assert_eq!(fs::read(e.manifest.source.join("new")).unwrap(),b"destination");
    }
}

#[test]
fn failed_create_does_not_delete_an_existing_file() {
    let mut f=Fixture::new();fs::write(f.source("a"),b"keep").unwrap();
    assert!(f.engine.prepare(CREATE,"a","",0,0).is_err());assert_eq!(fs::read(f.source("a")).unwrap(),b"keep");
}

#[test]
fn commit_accepts_state_and_later_abort_retains_it() {
    let mut f=Fixture::new();fs::write(f.source("a"),b"old").unwrap();f.write("a",0,b"new");
    let (_,applied)=f.engine.commit(None).unwrap();assert_eq!(applied,1);assert_eq!(f.engine.preimage_bytes(),0);
    f.write("a",0,b"bad");
    let mut e=Engine::open(&f.root.path().join("state")).unwrap();e.abort(None).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(),b"new");assert_eq!(e.diff(None,None).unwrap().pending_changes,0);
}

#[test]
fn quota_rejection_precedes_backup_and_mutation() {
    let mut f=Fixture::new();f.engine.manifest.policy.limits.max_storage_bytes=Some(2);fs::write(f.source("a"),b"123456").unwrap();
    assert!(f.engine.prepare(WRITE,"a","",0,3).is_err());
    assert_eq!(fs::read_dir(f.engine.state.join("preimages")).unwrap().count(),0);
    assert_eq!(fs::read(f.source("a")).unwrap(),b"123456");
}

#[test]
fn rejects_traversal_streams_secrets_and_metadata_writes() {
    let f=Fixture::new();
    for path in ["..\\outside","a:stream",".ENV","sub\\secret.PEM"] { assert!(f.engine.path(path,false).is_err(),"{path}"); }
    assert!(f.engine.path(".git\\config",true).is_err());assert!(f.engine.path(".git\\config",false).is_ok());
}

#[test]
fn torn_final_line_is_removed_before_new_records_are_appended() {
    let mut f=Fixture::new();fs::write(f.source("a"),b"old").unwrap();f.write("a",0,b"new");
    OpenOptions::new().append(true).open(f.engine.state.join("journal.jsonl")).unwrap().write_all(b"{torn").unwrap();
    let mut e=Engine::open(&f.root.path().join("state")).unwrap();e.abort(None).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(),b"old");
    Engine::open(&f.root.path().join("state")).unwrap();
}

#[test]
fn strict_private_lifecycle_persists_mode_and_selected_checkpoint() {
    let mut f = Fixture::private_with(&[("a", b"old"), ("dir/child", b"child")], DurabilityMode::Strict);
    f.write("a", 0, b"one");
    f.rename("dir", "moved");
    f.create("created", b"new");
    let checkpoint = f.engine.checkpoint(None).unwrap();
    f.write("a", 0, b"two");
    f.engine.commit(Some(&checkpoint.id)).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(), b"one");
    assert_eq!(fs::read(f.source("moved/child")).unwrap(), b"child");
    assert!(!f.source("dir").exists());
    assert_eq!(f.engine.preimage_bytes(), 3);
    f.engine = Engine::open(&f.engine.state).unwrap();
    let info = super::helper::session_info(&f.engine, None).unwrap();
    assert_eq!(info.filesystem.durability, "strict");
    assert_eq!(info.filesystem.group_commit_ms, None);
    f.engine.abort(None).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"one");
    assert_eq!(fs::read(f.source("created")).unwrap(), b"new");
    assert_eq!(f.engine.preimage_bytes(), 0);
}

#[test]
fn strict_readonly_metadata_is_flushed_then_restored_on_abort() {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    let mut f = Fixture::private_with(&[("a", b"old")], DurabilityMode::Strict);
    let record = f.engine.prepare(METADATA, "a", "", 0, 0).unwrap();
    assert_ne!(unsafe { SetFileAttributesW(wide(&f.live("a")).as_ptr(), 1) }, 0);
    f.engine.finish(record, 0).unwrap();
    f.engine.commit(None).unwrap();
    assert_ne!(fs::metadata(f.source("a")).unwrap().file_attributes() & 1, 0);
    // Clear READONLY as a routed metadata change, then recover the accepted bit.
    let record = f.engine.prepare(METADATA, "a", "", 0, 0).unwrap();
    assert_ne!(unsafe { SetFileAttributesW(wide(&f.live("a")).as_ptr(), 0x80) }, 0);
    f.engine.finish(record, 0).unwrap();
    f.engine.abort(None).unwrap();
    assert_ne!(fs::metadata(f.live("a")).unwrap().file_attributes() & 1, 0);
    remove(&f.source("a")).unwrap();
    remove(&f.live("a")).unwrap();
}

#[test]
fn strict_mutation_barriers_fail_closed_and_recover_ready() {
    for point in ["flush_file", "flush_directory", "mutation_barrier", "applied_barrier"] {
        let mut f = Fixture::private_with(&[("a", b"old")], DurabilityMode::Strict);
        let record = f.engine.prepare(WRITE, "a", "", 0, 3).unwrap();
        fs::write(f.live("a"), b"new").unwrap();
        super::durability::FAIL.with(|fail| *fail.borrow_mut() = Some(point.into()));
        assert!(f.engine.finish(record, 0).is_err(), "{point}");
        assert!(f.engine.checkpoint(None).is_err(), "{point}");
        assert!(f.engine.prepare(WRITE, "a", "", 0, 3).is_err(), "{point}");
        assert_eq!(f.engine.preimage_bytes(), if point == "applied_barrier" {3} else {0});
        assert!(fs::read_dir(f.engine.state.join("preimages")).unwrap().next().is_some());
        f.engine = Engine::open(&f.engine.state).unwrap();
        assert_eq!(fs::read(f.live("a")).unwrap(), if point == "applied_barrier" {b"new"} else {b"old"});
        f.engine.abort(None).unwrap();
        assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    }
}

#[test]
fn strict_publish_flush_errors_retain_backups_and_never_report_success() {
    for point in ["publish_stage_barrier", "publish_prepared_barrier", "publish_path_barrier",
        "publish_host_barrier", "publish_decision_barrier", "publish_retire_before", "publish_retire_removed", "publish_retire_barrier"] {
        let mut f = Fixture::private_with(&[("a", b"old"), ("b", b"old")], DurabilityMode::Strict);
        f.write("a", 0, b"new");
        f.write("b", 0, b"new");
        super::durability::FAIL.with(|fail| *fail.borrow_mut() = Some(point.into()));
        assert!(f.engine.commit(None).is_err(), "{point}");
        assert!(f.engine.prepare(WRITE, "a", "", 0, 3).is_err(), "{point}");
        let stages: Vec<_> = fs::read_dir(&f.engine.state).unwrap().map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("publish-")).collect();
        assert_eq!(stages.len(), 1, "{point}");
        assert_eq!(fs::read(stages[0].join("backup/0")).unwrap(), b"old", "{point}");
        f.engine = Engine::open(&f.engine.state).unwrap();
        let committed = matches!(point, "publish_decision_barrier" | "publish_retire_before" | "publish_retire_removed" | "publish_retire_barrier");
        for name in ["a", "b"] { assert_eq!(fs::read(f.source(name)).unwrap(), if committed {b"new"} else {b"old"}, "{point}"); }
        f.engine.abort(None).unwrap();
    }
}

#[test]
fn strict_rollback_error_keeps_undo_until_recovery_completes() {
    let mut f = Fixture::private_with(&[("a", b"old")], DurabilityMode::Strict);
    f.write("a", 0, b"new");
    super::durability::FAIL.with(|fail| *fail.borrow_mut() = Some("rollback_barrier".into()));
    assert!(f.engine.abort(None).is_err());
    assert_eq!(f.engine.preimage_bytes(), 3);
    assert!(f.engine.prepare(WRITE, "a", "", 0, 3).is_err());
    f.engine = Engine::open(&f.engine.state).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    assert_eq!(f.engine.preimage_bytes(), 0);
}

#[test]
fn strict_publication_crashes_recover_before_and_after_the_durable_decision() {
    for point in ["publish_stage_barrier", "publish_prepared_barrier", "private_publish_path_applied",
        "publish_host_barrier", "publish_decision_barrier", "private_publish_committed", "publish_retire_removed", "publish_retire_barrier"] {
        let mut f = Fixture::private_with(&[("a", b"old"), ("b", b"old")], DurabilityMode::Strict);
        f.write("a", 0, b"new");
        f.write("b", 0, b"new");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "windows::engine_tests::private_publish_crash_child", "--nocapture"])
            .env("DELTABOX_TEST_PRIVATE_STATE", &f.engine.state)
            .env("DELTABOX_TEST_CRASH_POINT", point).output().unwrap();
        assert_eq!(output.status.code(), Some(86), "{point}: {}", String::from_utf8_lossy(&output.stderr));
        f.engine = Engine::open(&f.engine.state).unwrap();
        let committed = matches!(point, "publish_decision_barrier" | "private_publish_committed" | "publish_retire_removed" | "publish_retire_barrier");
        for name in ["a", "b"] {
            assert_eq!(fs::read(f.source(name)).unwrap(), if committed { b"new" } else { b"old" }, "{point}");
            assert_eq!(fs::read(f.live(name)).unwrap(), b"new", "{point}");
        }
        assert_eq!(f.engine.diff(None, None).unwrap().pending_changes, if committed {0} else {2}, "{point}");
    }
}

#[test]
fn strict_readonly_initialization_and_unfinished_rename_recover_metadata() {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    let mut f = Fixture::private_with(&[("a", b"old")], DurabilityMode::Strict);
    assert_ne!(unsafe { SetFileAttributesW(wide(&f.source("a")).as_ptr(), 1) }, 0);
    let mut manifest = f.engine.manifest.clone();
    manifest.session_id = uuid::Uuid::new_v4().to_string();
    f.engine = Engine::create(&f.root.path().join("readonly"), manifest).unwrap();
    assert_ne!(fs::metadata(f.live("a")).unwrap().file_attributes() & 1, 0);
    let record = f.engine.prepare(WRITE, "a", "", 0, 3).unwrap();
    assert!(fs::write(f.live("a"), b"bad").is_err());
    assert!(f.engine.finish(record, -1).unwrap() < 0);
    assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    let _record = f.engine.prepare(RENAME, "a", "renamed", 0, 0).unwrap();
    fs::rename(f.live("a"), f.live("renamed")).unwrap();
    f.engine = Engine::open(&f.engine.state).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    assert_ne!(fs::metadata(f.live("a")).unwrap().file_attributes() & 1, 0);
    assert!(!f.live("renamed").exists());
    // A completed rename uses the retained writable handle, then abort restores it.
    f.rename("a", "renamed");
    f.engine.abort(None).unwrap();
    assert_ne!(fs::metadata(f.live("a")).unwrap().file_attributes() & 1, 0);
    remove(&f.source("a")).unwrap();
    remove(&f.live("a")).unwrap();
}

#[test]
fn strict_mutation_crash_child() {
    let Some(state) = std::env::var_os("DELTABOX_TEST_MUTATION_STATE") else { return; };
    let mut engine = Engine::open(std::path::Path::new(&state)).unwrap();
    let record = engine.prepare(WRITE, "a", "", 0, 3).unwrap();
    fs::write(engine.manifest.live().join("a"), b"new").unwrap();
    engine.finish(record, 0).unwrap();
    panic!("the requested crash point was not reached");
}

#[test]
fn strict_mutation_crashes_recover_at_each_log_boundary() {
    for point in ["undo_barrier", "ready_barrier", "mutation_barrier", "applied_barrier"] {
        let mut f = Fixture::private_with(&[("a", b"old")], DurabilityMode::Strict);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "windows::engine_tests::strict_mutation_crash_child", "--nocapture"])
            .env("DELTABOX_TEST_MUTATION_STATE", &f.engine.state)
            .env("DELTABOX_TEST_CRASH_POINT", point).output().unwrap();
        assert_eq!(output.status.code(), Some(86), "{point}: {}", String::from_utf8_lossy(&output.stderr));
        f.engine = Engine::open(&f.engine.state).unwrap();
        assert_eq!(fs::read(f.live("a")).unwrap(), if point == "applied_barrier" {b"new"} else {b"old"}, "{point}");
        assert_eq!(fs::read(f.source("a")).unwrap(), b"old");
        f.engine.abort(None).unwrap();
        assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    }
}

#[test]
fn strict_recovery_retries_failed_host_restore_without_reclaiming_backup() {
    let mut f = Fixture::private_with(&[("a", b"old")], DurabilityMode::Strict);
    f.write("a", 0, b"new");
    super::durability::FAIL.with(|fail| *fail.borrow_mut() = Some("publish_path_barrier".into()));
    assert!(f.engine.commit(None).is_err());
    super::durability::FAIL.with(|fail| *fail.borrow_mut() = Some("publish_undo_barrier".into()));
    assert!(Engine::open(&f.engine.state).is_err());
    assert!(f.root.path().join("publish.json").exists());
    f.engine = Engine::open(&f.engine.state).unwrap();
    assert_eq!(fs::read(f.source("a")).unwrap(), b"old");
    assert_eq!(fs::read(f.live("a")).unwrap(), b"new");
    assert_eq!(f.engine.preimage_bytes(), 3);
}

#[test]
fn strict_abort_restores_a_renamed_then_deleted_file_after_restart() {
    let mut f = Fixture::private_with(&[("a", b"old"), ("b", b"replaced")], DurabilityMode::Strict);
    // Replacement rename, followed by deletion, necessarily recreates the
    // renamed object with a different NTFS ID while undoing the newer delete.
    let record = f.engine.prepare(RENAME, "a", "b", 0, 0).unwrap();
    remove(&f.live("b")).unwrap();
    fs::rename(f.live("a"), f.live("b")).unwrap();
    f.engine.finish(record, 0).unwrap();
    let record = f.engine.prepare(DELETE, "b", "", 0, 0).unwrap();
    fs::remove_file(f.live("b")).unwrap();
    f.engine.finish(record, 0).unwrap();
    f.engine = Engine::open(&f.engine.state).unwrap();
    f.engine.abort(None).unwrap();
    assert_eq!(fs::read(f.live("a")).unwrap(), b"old");
    assert_eq!(fs::read(f.live("b")).unwrap(), b"replaced");
    f.engine = Engine::open(&f.engine.state).unwrap();
    assert_eq!(f.engine.diff(None, None).unwrap().pending_changes, 0);
}

use super::engine::*;
use crate::policy::Policy;
use crate::protocol::{CheckpointInfo, SessionStatus};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};

struct Fixture { root: tempfile::TempDir, engine: Engine }
impl Fixture {
    fn new() -> Self {
        let root=tempfile::tempdir().unwrap();
        let source=root.path().join("source");fs::create_dir(&source).unwrap();
        let checkpoint="initial".to_owned();
        let manifest=Manifest {version:1,session_id:uuid::Uuid::new_v4().to_string(),source:source.canonicalize().unwrap(),view:root.path().join("view"),scratch:root.path().join("scratch"),token:"test-token".into(),started_at_ms:now_ms(),status:SessionStatus::Running,policy:Policy::default_agent(),next_id:1,accepted:checkpoint.clone(),checkpoints:vec![Savepoint {info:CheckpointInfo {id:checkpoint,label:None,created_at_ms:now_ms(),change_count:0,committed:true},cursor:0}],review:None,rollback:None};
        let engine=Engine::create(&root.path().join("state"),manifest).unwrap();
        Self {root,engine}
    }
    fn source(&self,name:&str)->std::path::PathBuf {self.engine.manifest.source.join(name)}
    fn write(&mut self,name:&str,offset:u64,data:&[u8]) {
        let record=self.engine.prepare(WRITE,name,"",offset,data.len() as u64).unwrap();
        let mut f=OpenOptions::new().write(true).open(self.source(name)).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();f.write_all(data).unwrap();drop(f);
        self.engine.finish(record,0).unwrap();
    }
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

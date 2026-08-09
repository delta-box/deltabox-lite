use deltafs_fuse::engine::{
    Change, ChangeKind, Checkpoint, Durability, EngineError, EngineOptions, FaultInjector,
    FaultPoint, StateEngine,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

#[derive(Serialize, Deserialize)]
struct ManifestForTest {
    session_id: String,
    live_root: String,
    state_root: String,
    durability: Durability,
    quota_limit: u64,
    active_epoch: u64,
    accepted_checkpoint: u64,
    checkpoints: Vec<Checkpoint>,
    next_epoch_id: u64,
    next_checkpoint_id: u64,
    next_record_id: u64,
    next_vinode_id: u64,
    next_commit_sequence: u64,
}

fn rewind_manifest_counters(path: &Path) {
    let mut envelope: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let mut manifest: ManifestForTest =
        serde_json::from_value(envelope["payload"].clone()).unwrap();
    manifest.next_epoch_id = 2;
    manifest.next_checkpoint_id = 1;
    manifest.next_record_id = 1;
    manifest.next_vinode_id = 1;
    let payload = serde_json::to_vec(&manifest).unwrap();
    envelope["payload_length"] = serde_json::json!(payload.len());
    envelope["checksum"] = serde_json::json!(crc32fast::hash(&payload));
    envelope["payload"] = serde_json::to_value(manifest).unwrap();
    fs::write(path, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
}

struct Fixture {
    _root: TempDir,
    live: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let state = root.path().join("state");
        fs::create_dir(&live).unwrap();
        Self {
            _root: root,
            live,
            state,
        }
    }

    fn engine(&self) -> StateEngine {
        StateEngine::create(&self.live, &self.state, EngineOptions::default()).unwrap()
    }

    fn engine_with_quota(&self, quota_bytes: u64) -> StateEngine {
        StateEngine::create(&self.live, &self.state, EngineOptions { quota_bytes }).unwrap()
    }

    fn reopen(&self) -> StateEngine {
        StateEngine::open(&self.live, &self.state).unwrap()
    }
}

fn read(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn preimage_count(state: &Path) -> usize {
    fs::read_dir(state.join("epochs/00000001/preimages"))
        .unwrap()
        .count()
}

fn set_directory_time(path: &Path, time: SystemTime) {
    let times = fs::FileTimes::new().set_accessed(time).set_modified(time);
    fs::File::open(path).unwrap().set_times(times).unwrap();
}

fn mtime(path: &Path) -> (i64, i64) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.mtime(), metadata.mtime_nsec())
}

fn changes(items: &[(&str, ChangeKind)]) -> Vec<(String, ChangeKind)> {
    items
        .iter()
        .map(|(path, kind)| ((*path).to_owned(), kind.clone()))
        .collect()
}

fn change_kinds(changes: Vec<Change>) -> Vec<(String, ChangeKind)> {
    changes
        .into_iter()
        .map(|change| (change.path, change.kind))
        .collect()
}

#[test]
fn create_returns_the_writable_descriptor_for_a_read_only_file() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    let file = engine
        .create_file_open(Path::new("object"), 0o444, libc::O_WRONLY)
        .unwrap();
    engine
        .write_file(Path::new("object"), &file, 0, b"contents")
        .unwrap();
    drop(file);

    assert_eq!(read(fixture.live.join("object")), b"contents");
    assert_eq!(
        fs::metadata(fixture.live.join("object")).unwrap().mode() & 0o777,
        0o444
    );
    engine.rollback(None).unwrap();
    assert!(!fixture.live.join("object").exists());
}

#[test]
fn create_modify_checkpoint_and_rollback_sequences() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("base"), b"v0").unwrap();
    let mut engine = fixture.engine();

    engine.create_file(Path::new("new"), b"n0", 0o640).unwrap();
    let first = engine.checkpoint(Some("first".into())).unwrap();
    engine.write(Path::new("new"), 1, b"1").unwrap();
    engine.append(Path::new("base"), b"-v1").unwrap();
    assert_eq!(read(fixture.live.join("new")), b"n1");
    assert_eq!(read(fixture.live.join("base")), b"v0-v1");

    engine.rollback(Some(first.id)).unwrap();
    assert_eq!(read(fixture.live.join("new")), b"n0");
    assert_eq!(read(fixture.live.join("base")), b"v0");

    engine.rollback(None).unwrap();
    assert!(!fixture.live.join("new").exists());
    assert_eq!(read(fixture.live.join("base")), b"v0");
    let remaining = engine.diff(None).unwrap();
    assert!(remaining.is_empty(), "remaining changes: {remaining:?}");
}

#[test]
fn commit_to_older_checkpoint_discards_future_and_starts_new_history() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"v0").unwrap();
    let mut engine = fixture.engine();

    engine.write(Path::new("a"), 0, b"v1").unwrap();
    let first = engine.checkpoint(Some("v1".into())).unwrap();
    engine.write(Path::new("a"), 0, b"v2").unwrap();
    let second = engine.checkpoint(Some("v2".into())).unwrap();
    engine.write(Path::new("a"), 0, b"v3").unwrap();

    let accepted = engine.commit(Some(first.id)).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"v1");
    assert_eq!(engine.status().accepted_checkpoint, accepted.id);
    assert_eq!(engine.status().checkpoints, vec![accepted.clone()]);
    assert!(matches!(
        engine.rollback(Some(second.id)),
        Err(EngineError::CheckpointNotFound(id)) if id == second.id
    ));
    assert!(engine.diff(None).unwrap().is_empty());

    engine.write(Path::new("a"), 0, b"v4").unwrap();
    assert_eq!(
        change_kinds(engine.diff(None).unwrap()),
        changes(&[("a", ChangeKind::Modified)])
    );
    engine.rollback(None).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"v1");
}

#[test]
fn committed_epochs_move_out_of_recovery_history_before_deletion() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"old").unwrap();
    let mut engine = fixture.engine();
    engine.write(Path::new("a"), 0, b"new").unwrap();
    engine.commit(None).unwrap();

    let garbage = engine.prepare_gc().unwrap();
    assert_eq!(garbage.len(), 1);
    assert!(!fixture.state.join("epochs/00000001").exists());
    assert_eq!(garbage[0].file_name().unwrap(), "00000001");
    assert_eq!(garbage[0].parent().unwrap().file_name().unwrap(), "gc");
    fs::remove_dir_all(&garbage[0]).unwrap();
    drop(engine);

    let reopened = fixture.reopen();
    assert!(reopened.diff(None).unwrap().is_empty());
    assert_eq!(read(fixture.live.join("a")), b"new");
}

#[test]
fn delete_recreate_rename_replacement_and_rollback() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    fs::write(fixture.live.join("b"), b"B").unwrap();
    let mut engine = fixture.engine();

    let checkpoint = engine.checkpoint(Some("base".into())).unwrap();
    engine.unlink(Path::new("a")).unwrap();
    engine.create_file(Path::new("a"), b"A2", 0o600).unwrap();
    engine.rename(Path::new("a"), Path::new("b")).unwrap();
    assert!(!fixture.live.join("a").exists());
    assert_eq!(read(fixture.live.join("b")), b"A2");

    engine.rollback(Some(checkpoint.id)).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(fixture.live.join("b")), b"B");
    assert!(engine.diff(Some(checkpoint.id)).unwrap().is_empty());
}

#[test]
fn rename_replacement_across_checkpoints_restores_each_generation() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    fs::write(fixture.live.join("b"), b"B").unwrap();
    fs::write(fixture.live.join("c"), b"C").unwrap();
    let mut engine = fixture.engine();

    engine.rename(Path::new("a"), Path::new("b")).unwrap();
    let first = engine.checkpoint(Some("a-over-b".into())).unwrap();
    engine.rename(Path::new("b"), Path::new("c")).unwrap();
    let second = engine.checkpoint(Some("a-over-c".into())).unwrap();
    engine.write(Path::new("c"), 0, b"Z").unwrap();

    engine.rollback(Some(second.id)).unwrap();
    assert_eq!(read(fixture.live.join("c")), b"A");
    engine.rollback(Some(first.id)).unwrap();
    assert_eq!(read(fixture.live.join("b")), b"A");
    assert_eq!(read(fixture.live.join("c")), b"C");
    engine.rollback(None).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(fixture.live.join("b")), b"B");
    assert_eq!(read(fixture.live.join("c")), b"C");
}

#[test]
fn rename_of_two_names_for_the_same_inode_is_a_noop() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"same").unwrap();
    fs::hard_link(fixture.live.join("a"), fixture.live.join("b")).unwrap();
    let mut engine = fixture.engine();

    engine.rename(Path::new("a"), Path::new("b")).unwrap();

    assert!(fixture.live.join("a").exists());
    assert!(fixture.live.join("b").exists());
    assert!(engine.diff(None).unwrap().is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn rename_flags_are_atomic_and_recoverable() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    fs::write(fixture.live.join("b"), b"B").unwrap();
    let mut engine = fixture.engine();

    assert!(engine
        .rename_noreplace(Path::new("a"), Path::new("b"))
        .is_err());
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(fixture.live.join("b")), b"B");

    engine.set_fault_injector(FaultInjector::once(FaultPoint::AfterReady));
    assert!(matches!(
        engine.rename_exchange(Path::new("a"), Path::new("b")),
        Err(EngineError::InjectedFault(FaultPoint::AfterReady))
    ));
    drop(engine);
    let mut engine = fixture.reopen();
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(fixture.live.join("b")), b"B");

    engine
        .rename_exchange(Path::new("a"), Path::new("b"))
        .unwrap();
    assert_eq!(read(fixture.live.join("a")), b"B");
    assert_eq!(read(fixture.live.join("b")), b"A");
    engine.rollback(None).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(fixture.live.join("b")), b"B");
}

#[cfg(target_os = "linux")]
#[test]
fn exchange_during_mutation_recovers_original_names() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    fs::write(fixture.live.join("b"), b"B").unwrap();
    let mut engine = fixture.engine();
    engine.set_fault_injector(FaultInjector::once(FaultPoint::DuringMutation));

    assert!(matches!(
        engine.rename_exchange(Path::new("a"), Path::new("b")),
        Err(EngineError::InjectedFault(FaultPoint::DuringMutation))
    ));
    drop(engine);
    fixture.reopen();
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(fixture.live.join("b")), b"B");
}

#[test]
fn hardlink_writes_through_alternating_names_restore_bytes_and_identity() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"0123456789").unwrap();
    let mut engine = fixture.engine();

    engine.hard_link(Path::new("a"), Path::new("b")).unwrap();
    let checkpoint = engine.checkpoint(Some("linked".into())).unwrap();
    engine.write(Path::new("a"), 2, b"AA").unwrap();
    engine.write(Path::new("b"), 4, b"BB").unwrap();
    engine.unlink(Path::new("a")).unwrap();
    assert_eq!(read(fixture.live.join("b")), b"01AABB6789");

    engine.rollback(Some(checkpoint.id)).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"0123456789");
    assert_eq!(read(fixture.live.join("b")), b"0123456789");
    let a = fs::metadata(fixture.live.join("a")).unwrap();
    let b = fs::metadata(fixture.live.join("b")).unwrap();
    assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
}

#[test]
fn diff_detects_hardlink_topology_changes_with_identical_files() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"same").unwrap();
    fs::hard_link(fixture.live.join("a"), fixture.live.join("b")).unwrap();
    let original = fs::metadata(fixture.live.join("b")).unwrap();
    let mut engine = fixture.engine();

    engine.unlink(Path::new("b")).unwrap();
    engine
        .create_file(Path::new("b"), b"same", original.mode() & 0o7777)
        .unwrap();
    engine
        .set_times(
            Path::new("b"),
            original.accessed().unwrap(),
            original.modified().unwrap(),
        )
        .unwrap();

    assert_eq!(
        change_kinds(engine.diff(None).unwrap()),
        changes(&[("a", ChangeKind::Modified), ("b", ChangeKind::Modified),])
    );
    engine.rollback(None).unwrap();
    let a = fs::metadata(fixture.live.join("a")).unwrap();
    let b = fs::metadata(fixture.live.join("b")).unwrap();
    assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
}

#[test]
fn external_hardlink_alias_is_rejected_without_mutation() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    let external = fixture.live.parent().unwrap().join("external-link");
    fs::hard_link(fixture.live.join("a"), &external).unwrap();
    let mut engine = fixture.engine();

    assert!(matches!(
        engine.unlink(Path::new("a")),
        Err(EngineError::UnresolvedHardlink(path)) if path == Path::new("a")
    ));
    assert_eq!(read(fixture.live.join("a")), b"A");
    assert_eq!(read(&external), b"A");
    let a = fs::metadata(fixture.live.join("a")).unwrap();
    let external = fs::metadata(external).unwrap();
    assert_eq!((a.dev(), a.ino()), (external.dev(), external.ino()));
    assert_eq!(preimage_count(&fixture.state), 0);
}

#[test]
fn diff_tracks_only_effective_touched_path_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    fs::write(fixture.live.join("gone"), b"G").unwrap();
    let a_metadata = fs::metadata(fixture.live.join("a")).unwrap();
    let gone_metadata = fs::metadata(fixture.live.join("gone")).unwrap();
    let mut engine = fixture.engine();

    engine.write(Path::new("a"), 0, b"B").unwrap();
    engine.create_file(Path::new("new"), b"N", 0o600).unwrap();
    engine.unlink(Path::new("gone")).unwrap();
    let diff = engine.diff(None).unwrap();
    let modified = diff.iter().find(|change| change.path == "a").unwrap();
    assert_eq!(modified.old_bytes.as_deref(), Some(b"A".as_slice()));
    assert_eq!(modified.new_bytes.as_deref(), Some(b"B".as_slice()));
    assert_eq!(modified.old_size, Some(1));
    assert_eq!(modified.new_size, Some(1));
    assert_eq!(
        change_kinds(diff),
        changes(&[
            ("a", ChangeKind::Modified),
            ("gone", ChangeKind::Deleted),
            ("new", ChangeKind::Added),
        ])
    );

    engine.write(Path::new("a"), 0, b"A").unwrap();
    engine.unlink(Path::new("new")).unwrap();
    engine
        .create_file(Path::new("gone"), b"G", gone_metadata.mode() & 0o7777)
        .unwrap();
    engine
        .set_times(
            Path::new("a"),
            a_metadata.accessed().unwrap(),
            a_metadata.modified().unwrap(),
        )
        .unwrap();
    engine
        .set_times(
            Path::new("gone"),
            gone_metadata.accessed().unwrap(),
            gone_metadata.modified().unwrap(),
        )
        .unwrap();
    let recreated = fs::metadata(fixture.live.join("gone")).unwrap();
    assert_eq!(
        recreated.mode(),
        gone_metadata.mode(),
        "recreated mode differs"
    );
    assert_eq!(
        (recreated.uid(), recreated.gid()),
        (gone_metadata.uid(), gone_metadata.gid()),
        "recreated owner differs"
    );
    assert_eq!(
        (recreated.mtime(), recreated.mtime_nsec()),
        (gone_metadata.mtime(), gone_metadata.mtime_nsec()),
        "recreated mtime differs"
    );
    let remaining = engine.diff(None).unwrap();
    assert!(remaining.is_empty(), "remaining changes: {remaining:?}");
}

#[test]
fn diff_handles_removed_nested_created_tree() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    engine.create_dir(Path::new("build"), 0o755).unwrap();
    engine
        .create_dir(Path::new("build/temporary"), 0o755)
        .unwrap();
    engine
        .create_file(Path::new("build/temporary/output"), b"output", 0o644)
        .unwrap();
    engine.unlink(Path::new("build/temporary/output")).unwrap();
    engine.unlink(Path::new("build/temporary")).unwrap();
    engine.unlink(Path::new("build")).unwrap();
    engine
        .create_file(Path::new("output.part"), b"temporary", 0o644)
        .unwrap();
    engine
        .rename(Path::new("output.part"), Path::new("output"))
        .unwrap();
    engine.unlink(Path::new("output")).unwrap();

    assert!(engine.diff(None).unwrap().is_empty());
}

#[test]
fn truncate_append_and_overlapping_ranges_restore_original() {
    let fixture = Fixture::new();
    let original: Vec<u8> = (0..=255).cycle().take(64 * 1024).collect();
    fs::write(fixture.live.join("large"), &original).unwrap();
    let mut engine = fixture.engine();

    engine
        .write(Path::new("large"), 4096, &vec![1; 4096])
        .unwrap();
    let used_after_first = engine.status().quota_used;
    engine
        .write(Path::new("large"), 6144, &vec![2; 4096])
        .unwrap();
    let additional = engine.status().quota_used - used_after_first;
    assert!(
        additional < 4 * 1024,
        "overlap recaptured too much: {additional}"
    );
    let used_before_append = engine.status().quota_used;
    engine.append(Path::new("large"), b"tail").unwrap();
    let used_after_append = engine.status().quota_used;
    assert!(used_after_append - used_before_append < 1024);
    engine.truncate(Path::new("large"), 1000).unwrap();
    engine.truncate(Path::new("large"), 80 * 1024).unwrap();

    engine.rollback(None).unwrap();
    assert_eq!(read(fixture.live.join("large")), original);
}

#[test]
fn small_overwrite_has_small_preimage_and_quota_failure_is_atomic() {
    let fixture = Fixture::new();
    let original = vec![7_u8; 1024 * 1024];
    fs::write(fixture.live.join("large"), &original).unwrap();
    let mut engine = fixture.engine_with_quota(32 * 1024);

    engine
        .write(Path::new("large"), 128 * 1024, &vec![9; 4096])
        .unwrap();
    let preimages = fixture.state.join("epochs/00000001/preimages");
    let stored: u64 = fs::read_dir(preimages)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert_eq!(stored, 4096);
    engine.rollback(None).unwrap();
    assert_eq!(read(fixture.live.join("large")), original);

    let fixture = Fixture::new();
    let original = vec![3_u8; 64 * 1024];
    fs::write(fixture.live.join("large"), &original).unwrap();
    let mut engine = fixture.engine_with_quota(12 * 1024);
    assert!(matches!(
        engine.write(Path::new("large"), 0, &vec![4; 16 * 1024]),
        Err(EngineError::QuotaExceeded { .. })
    ));
    assert_eq!(read(fixture.live.join("large")), original);
    assert_eq!(engine.status().quota_used, 0);
    assert_eq!(preimage_count(&fixture.state), 0);
}

#[test]
fn quota_rejection_precedes_whole_object_materialization() {
    let fixture = Fixture::new();
    let original = vec![3_u8; 64 * 1024];
    fs::write(fixture.live.join("large"), &original).unwrap();
    let mut engine = fixture.engine_with_quota(12 * 1024);
    assert!(matches!(
        engine.truncate(Path::new("large"), 0),
        Err(EngineError::QuotaExceeded { .. })
    ));
    assert_eq!(read(fixture.live.join("large")), original);
    assert_eq!(preimage_count(&fixture.state), 0);

    let fixture = Fixture::new();
    let original = vec![4_u8; 64 * 1024];
    fs::write(fixture.live.join("large"), &original).unwrap();
    let mut engine = fixture.engine_with_quota(12 * 1024);
    assert!(matches!(
        engine.unlink(Path::new("large")),
        Err(EngineError::QuotaExceeded { .. })
    ));
    assert_eq!(read(fixture.live.join("large")), original);
    assert_eq!(preimage_count(&fixture.state), 0);

    let fixture = Fixture::new();
    fs::write(fixture.live.join("source"), b"source").unwrap();
    let target = vec![5_u8; 64 * 1024];
    fs::write(fixture.live.join("target"), &target).unwrap();
    let mut engine = fixture.engine_with_quota(12 * 1024);
    assert!(matches!(
        engine.rename(Path::new("source"), Path::new("target")),
        Err(EngineError::QuotaExceeded { .. })
    ));
    assert_eq!(read(fixture.live.join("source")), b"source");
    assert_eq!(read(fixture.live.join("target")), target);
    assert_eq!(preimage_count(&fixture.state), 0);
}

#[test]
fn quota_usage_counts_journal_and_is_stable_after_reopen() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("large"), vec![1_u8; 64 * 1024]).unwrap();
    let mut engine = fixture.engine();
    engine
        .write(Path::new("large"), 4096, &vec![2_u8; 4096])
        .unwrap();
    engine.append(Path::new("large"), b"tail").unwrap();

    let journal = fixture.state.join("epochs/00000001/journal.log");
    let journal_bytes = fs::metadata(journal).unwrap().len();
    let accounted_preimage = 4096 + 1024;
    let used = engine.status().quota_used;
    assert!(used > journal_bytes + accounted_preimage);
    drop(engine);

    let engine = fixture.reopen();
    assert_eq!(engine.status().quota_used, used);
}

#[test]
fn xattr_quota_failure_happens_before_mutation() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    let xattr_name = if cfg!(target_os = "macos") {
        "com.deltafs.large"
    } else {
        "user.deltafs.large"
    };
    let original = vec![7_u8; 1024];
    xattr::set(fixture.live.join("a"), xattr_name, &original).unwrap();
    let mut engine = fixture.engine_with_quota(5 * 1024);

    assert!(matches!(
        engine.set_xattr(Path::new("a"), xattr_name, b"small"),
        Err(EngineError::QuotaExceeded { .. })
    ));
    assert_eq!(
        xattr::get(fixture.live.join("a"), xattr_name).unwrap(),
        Some(original)
    );
    assert_eq!(engine.status().quota_used, 0);
}

#[test]
fn metadata_symlink_directory_and_xattr_undo() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    fs::set_permissions(fixture.live.join("a"), fs::Permissions::from_mode(0o640)).unwrap();
    let original_time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut engine = fixture.engine();

    engine.create_dir(Path::new("dir"), 0o750).unwrap();
    engine
        .symlink(Path::new("../a"), Path::new("dir/link"))
        .unwrap();
    engine.set_mode(Path::new("a"), 0o600).unwrap();
    engine
        .set_times(
            Path::new("a"),
            original_time + Duration::from_secs(10),
            original_time + Duration::from_secs(20),
        )
        .unwrap();
    let metadata = fs::metadata(fixture.live.join("a")).unwrap();
    engine
        .set_owner(Path::new("a"), metadata.uid(), metadata.gid())
        .unwrap();
    let xattr_name = if cfg!(target_os = "macos") {
        "com.deltafs.engine-test"
    } else {
        "user.deltafs.engine-test"
    };
    engine
        .set_xattr(Path::new("a"), xattr_name, b"value")
        .unwrap();
    engine.remove_xattr(Path::new("a"), xattr_name).unwrap();

    engine.rollback(None).unwrap();
    assert!(!fixture.live.join("dir").exists());
    assert_eq!(
        fs::metadata(fixture.live.join("a")).unwrap().mode() & 0o777,
        0o640
    );
    assert_eq!(
        xattr::get(fixture.live.join("a"), xattr_name).unwrap(),
        None
    );
}

#[test]
fn unlink_restores_existing_symlink_and_empty_directory() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink("missing-target", fixture.live.join("link")).unwrap();
    fs::create_dir(fixture.live.join("empty")).unwrap();
    fs::set_permissions(
        fixture.live.join("empty"),
        fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    let mut engine = fixture.engine();

    engine.unlink(Path::new("link")).unwrap();
    engine.unlink(Path::new("empty")).unwrap();
    engine.rollback(None).unwrap();

    assert_eq!(
        fs::read_link(fixture.live.join("link")).unwrap(),
        Path::new("missing-target")
    );
    assert!(fixture.live.join("empty").is_dir());
    assert_eq!(
        fs::metadata(fixture.live.join("empty")).unwrap().mode() & 0o777,
        0o750
    );
}

#[test]
fn namespace_rollback_restores_parent_directory_times() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.live.join("left")).unwrap();
    fs::create_dir(fixture.live.join("right")).unwrap();
    fs::write(fixture.live.join("left/a"), b"A").unwrap();
    fs::write(fixture.live.join("right/b"), b"B").unwrap();
    let root_time = UNIX_EPOCH + Duration::from_secs(1_600_000_001);
    let left_time = UNIX_EPOCH + Duration::from_secs(1_600_000_002);
    let right_time = UNIX_EPOCH + Duration::from_secs(1_600_000_003);
    set_directory_time(&fixture.live, root_time);
    set_directory_time(&fixture.live.join("left"), left_time);
    set_directory_time(&fixture.live.join("right"), right_time);
    let expected_root = mtime(&fixture.live);
    let expected_left = mtime(&fixture.live.join("left"));
    let expected_right = mtime(&fixture.live.join("right"));
    let mut engine = fixture.engine();

    engine
        .create_file(Path::new("root-file"), b"root", 0o600)
        .unwrap();
    engine
        .hard_link(Path::new("left/a"), Path::new("right/link"))
        .unwrap();
    engine
        .rename(Path::new("left/a"), Path::new("right/b"))
        .unwrap();
    engine.rollback(None).unwrap();

    assert!(!fixture.live.join("root-file").exists());
    assert!(!fixture.live.join("right/link").exists());
    assert_eq!(read(fixture.live.join("left/a")), b"A");
    assert_eq!(read(fixture.live.join("right/b")), b"B");
    assert_eq!(mtime(&fixture.live), expected_root);
    assert_eq!(mtime(&fixture.live.join("left")), expected_left);
    assert_eq!(mtime(&fixture.live.join("right")), expected_right);
}

#[test]
fn reopen_rebuilds_truncate_range_and_touched_indexes() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"0123456789").unwrap();
    let mut engine = fixture.engine();
    engine.truncate(Path::new("a"), 4).unwrap();
    drop(engine);

    let mut engine = fixture.reopen();
    engine.write(Path::new("a"), 0, b"ABCD").unwrap();
    assert_eq!(
        change_kinds(engine.diff(None).unwrap()),
        changes(&[("a", ChangeKind::Modified)])
    );
    engine.rollback(None).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"0123456789");
}

#[test]
fn state_and_journal_corruption_are_rejected() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    let mut engine = fixture.engine();
    engine.write(Path::new("a"), 0, b"B").unwrap();
    drop(engine);

    let session = fixture.state.join("session.json");
    let original_session = fs::read(&session).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original_session).unwrap();
    value["checksum"] = serde_json::json!(0);
    fs::write(&session, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(
        StateEngine::open(&fixture.live, &fixture.state),
        Err(EngineError::CorruptState(_))
    ));

    fs::write(&session, &original_session).unwrap();
    let journal = fixture.state.join("epochs/00000001/journal.log");
    let mut bytes = fs::read(&journal).unwrap();
    bytes.pop();
    fs::write(&journal, bytes).unwrap();
    assert!(matches!(
        StateEngine::open(&fixture.live, &fixture.state),
        Err(EngineError::CorruptJournal(_))
    ));
}

#[test]
fn create_rejects_overlapping_or_nonempty_state_without_writes() {
    let fixture = Fixture::new();
    let nested = fixture.live.join("nested/state");
    assert!(matches!(
        StateEngine::create(&fixture.live, &nested, EngineOptions::default()),
        Err(EngineError::OverlappingRoots)
    ));
    assert!(!fixture.live.join("nested").exists());

    let fixture = Fixture::new();
    fs::create_dir(&fixture.state).unwrap();
    let evidence = fixture.state.join("evidence");
    fs::write(&evidence, b"preserve").unwrap();
    assert!(matches!(
        StateEngine::create(&fixture.live, &fixture.state, EngineOptions::default()),
        Err(EngineError::StateNotEmpty)
    ));
    assert_eq!(read(evidence), b"preserve");
    assert_eq!(fs::read_dir(&fixture.state).unwrap().count(), 1);
}

#[test]
fn corrupt_preimage_stops_rollback_and_preserves_evidence() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"old-value").unwrap();
    let mut engine = fixture.engine();
    engine.write(Path::new("a"), 0, b"new-value").unwrap();
    let preimage = fs::read_dir(fixture.state.join("epochs/00000001/preimages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&preimage, b"bad-value").unwrap();

    assert!(matches!(
        engine.diff(None),
        Err(EngineError::CorruptState(_))
    ));
    assert!(matches!(
        engine.rollback(None),
        Err(EngineError::CorruptState(_))
    ));
    assert_eq!(read(fixture.live.join("a")), b"new-value");
    assert!(preimage.exists());
    assert!(matches!(
        engine.diff(None),
        Err(EngineError::ReopenRequired)
    ));
    drop(engine);

    assert!(matches!(
        StateEngine::open(&fixture.live, &fixture.state),
        Err(EngineError::CorruptState(_))
    ));
    assert!(preimage.exists());
}

#[test]
fn incompatible_state_version_is_rejected() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    drop(engine);
    let session = fixture.state.join("session.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&session).unwrap()).unwrap();
    value["format_version"] = serde_json::json!(999);
    fs::write(&session, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(
        StateEngine::open(&fixture.live, &fixture.state),
        Err(EngineError::UnsupportedFormat {
            found: 999,
            supported: 1
        })
    ));
}

#[test]
fn reopen_rebuilds_monotonic_ids_from_durable_artifacts() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"v0").unwrap();
    let mut engine = fixture.engine();
    engine.write(Path::new("a"), 0, b"v1").unwrap();
    let first = engine.checkpoint(Some("first".into())).unwrap();
    engine.write(Path::new("a"), 0, b"v2").unwrap();
    drop(engine);

    rewind_manifest_counters(&fixture.state.join("session.json"));

    let mut engine = fixture.reopen();
    let second = engine.checkpoint(Some("second".into())).unwrap();
    assert!(second.id > first.id);
    assert_eq!(engine.status().active_epoch, 3);
    engine.write(Path::new("a"), 0, b"v3").unwrap();
    engine.rollback(Some(first.id)).unwrap();
    assert_eq!(read(fixture.live.join("a")), b"v1");
}

fn mutation_fault_expected(point: FaultPoint) -> &'static [u8] {
    match point {
        FaultPoint::AfterApplied => b"new-value",
        _ => b"old-value",
    }
}

#[test]
fn every_mutation_fault_recovers_to_its_allowed_state() {
    for point in [
        FaultPoint::AfterPreimage,
        FaultPoint::AfterReady,
        FaultPoint::DuringMutation,
        FaultPoint::AfterMutation,
        FaultPoint::AfterApplied,
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.live.join("a"), b"old-value").unwrap();
        let mut engine = fixture.engine();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.write(Path::new("a"), 0, b"new-value"),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);

        let engine = fixture.reopen();
        assert_eq!(read(fixture.live.join("a")), mutation_fault_expected(point));
        if point == FaultPoint::AfterApplied {
            assert_eq!(
                change_kinds(engine.diff(None).unwrap()),
                changes(&[("a", ChangeKind::Modified)])
            );
        } else {
            assert!(engine.diff(None).unwrap().is_empty());
        }
    }
}

fn mutation_fault_points() -> [FaultPoint; 5] {
    [
        FaultPoint::AfterPreimage,
        FaultPoint::AfterReady,
        FaultPoint::DuringMutation,
        FaultPoint::AfterMutation,
        FaultPoint::AfterApplied,
    ]
}

#[test]
fn mutation_faults_cover_namespace_and_metadata_records() {
    for point in mutation_fault_points() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.create_file(Path::new("new"), b"N", 0o600),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);
        let engine = fixture.reopen();
        assert_eq!(
            fixture.live.join("new").exists(),
            point == FaultPoint::AfterApplied
        );
        drop(engine);

        let fixture = Fixture::new();
        fs::write(fixture.live.join("old"), b"O").unwrap();
        let mut engine = fixture.engine();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.unlink(Path::new("old")),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);
        let engine = fixture.reopen();
        assert_eq!(
            fixture.live.join("old").exists(),
            point != FaultPoint::AfterApplied
        );
        drop(engine);

        let fixture = Fixture::new();
        fs::write(fixture.live.join("a"), b"A").unwrap();
        fs::write(fixture.live.join("b"), b"B").unwrap();
        let mut engine = fixture.engine();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.rename(Path::new("a"), Path::new("b")),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);
        let engine = fixture.reopen();
        if point == FaultPoint::AfterApplied {
            assert!(!fixture.live.join("a").exists());
            assert_eq!(read(fixture.live.join("b")), b"A");
        } else {
            assert_eq!(read(fixture.live.join("a")), b"A");
            assert_eq!(read(fixture.live.join("b")), b"B");
        }
        drop(engine);

        let fixture = Fixture::new();
        fs::write(fixture.live.join("a"), b"A").unwrap();
        fs::set_permissions(fixture.live.join("a"), fs::Permissions::from_mode(0o640)).unwrap();
        let mut engine = fixture.engine();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.set_mode(Path::new("a"), 0o600),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);
        let engine = fixture.reopen();
        let expected = if point == FaultPoint::AfterApplied {
            0o600
        } else {
            0o640
        };
        assert_eq!(
            fs::metadata(fixture.live.join("a")).unwrap().mode() & 0o777,
            expected
        );
        drop(engine);
    }
}

#[test]
fn checkpoint_rollback_and_commit_faults_resume_idempotently() {
    for point in [
        FaultPoint::DuringCheckpointSeal,
        FaultPoint::AfterCheckpointSeal,
    ] {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.checkpoint(Some("fault".into())),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);
        let engine = fixture.reopen();
        let expected = if point == FaultPoint::AfterCheckpointSeal {
            2
        } else {
            1
        };
        assert_eq!(engine.status().checkpoints.len(), expected);
    }

    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"old").unwrap();
    fs::write(fixture.live.join("b"), b"old").unwrap();
    let mut engine = fixture.engine();
    engine.write(Path::new("a"), 0, b"new").unwrap();
    engine.write(Path::new("b"), 0, b"new").unwrap();
    engine.set_fault_injector(FaultInjector::once(FaultPoint::DuringRollback));
    assert!(matches!(
        engine.rollback(None),
        Err(EngineError::InjectedFault(FaultPoint::DuringRollback))
    ));
    drop(engine);
    let engine = fixture.reopen();
    assert_eq!(read(fixture.live.join("a")), b"old");
    assert_eq!(read(fixture.live.join("b")), b"old");
    assert!(engine.diff(None).unwrap().is_empty());

    for point in [
        FaultPoint::BeforeCommitMarker,
        FaultPoint::AfterCommitMarker,
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.live.join("a"), b"old").unwrap();
        let mut engine = fixture.engine();
        engine.write(Path::new("a"), 0, b"new").unwrap();
        engine.set_fault_injector(FaultInjector::once(point));
        assert!(matches!(
            engine.commit(None),
            Err(EngineError::InjectedFault(found)) if found == point
        ));
        drop(engine);
        let engine = fixture.reopen();
        assert_eq!(read(fixture.live.join("a")), b"new");
        if point == FaultPoint::BeforeCommitMarker {
            assert_eq!(
                change_kinds(engine.diff(None).unwrap()),
                changes(&[("a", ChangeKind::Modified)])
            );
        } else {
            assert!(engine.diff(None).unwrap().is_empty());
            assert_ne!(engine.status().accepted_checkpoint, 0);
        }
    }
}

#[test]
fn mutation_io_failure_requires_reopen_and_recovers_ready_record() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.live.join("source")).unwrap();
    fs::create_dir(fixture.live.join("target")).unwrap();
    fs::write(fixture.live.join("target/child"), b"keep").unwrap();
    let mut engine = fixture.engine();

    assert!(matches!(
        engine.rename(Path::new("source"), Path::new("target")),
        Err(EngineError::Io(_))
    ));
    assert!(matches!(
        engine.diff(None),
        Err(EngineError::ReopenRequired)
    ));
    drop(engine);

    let engine = fixture.reopen();
    assert!(fixture.live.join("source").is_dir());
    assert_eq!(read(fixture.live.join("target/child")), b"keep");
    assert!(engine.diff(None).unwrap().is_empty());
}

#[derive(Clone, Debug)]
enum ModelOp {
    Write(u8, u8, Vec<u8>),
    Append(u8, Vec<u8>),
    Truncate(u8, u8),
    Create(u8, Vec<u8>),
    Unlink(u8),
    Rename(u8, u8),
    Checkpoint,
    Rollback(u8),
    Commit,
}

fn model_op() -> impl Strategy<Value = ModelOp> {
    prop_oneof![
        (0_u8..5, 0_u8..20, prop::collection::vec(any::<u8>(), 0..12))
            .prop_map(|(path, offset, data)| ModelOp::Write(path, offset, data)),
        (0_u8..5, prop::collection::vec(any::<u8>(), 0..12))
            .prop_map(|(path, data)| ModelOp::Append(path, data)),
        (0_u8..5, 0_u8..32).prop_map(|(path, size)| ModelOp::Truncate(path, size)),
        (0_u8..5, prop::collection::vec(any::<u8>(), 0..12))
            .prop_map(|(path, data)| ModelOp::Create(path, data)),
        (0_u8..5).prop_map(ModelOp::Unlink),
        (0_u8..5, 0_u8..5).prop_map(|(old, new)| ModelOp::Rename(old, new)),
        Just(ModelOp::Checkpoint),
        (0_u8..8).prop_map(ModelOp::Rollback),
        Just(ModelOp::Commit),
    ]
}

fn model_path(index: u8) -> String {
    format!("f{}", index % 5)
}

fn assert_model(live: &Path, model: &BTreeMap<String, Vec<u8>>) {
    for index in 0..5 {
        let path = model_path(index);
        match model.get(&path) {
            Some(expected) => assert_eq!(read(live.join(path)), *expected),
            None => assert!(!live.join(path).exists()),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn randomized_sequences_match_full_copy_oracle(ops in prop::collection::vec(model_op(), 1..80)) {
        let fixture = Fixture::new();
        fs::write(fixture.live.join("f0"), b"seed").unwrap();
        let mut engine = fixture.engine();
        let mut model = BTreeMap::from([("f0".to_owned(), b"seed".to_vec())]);
        let mut checkpoints = vec![(0_u64, model.clone())];

        for op in ops {
            match op {
                ModelOp::Write(index, offset, data) => {
                    let path = model_path(index);
                    if let Some(file) = model.get_mut(&path) {
                        let offset = usize::from(offset);
                        if !data.is_empty() {
                            if offset > file.len() {
                                file.resize(offset, 0);
                            }
                            if offset + data.len() > file.len() {
                                file.resize(offset + data.len(), 0);
                            }
                            file[offset..offset + data.len()].copy_from_slice(&data);
                        }
                        engine.write(Path::new(&path), offset as u64, &data).unwrap();
                    }
                }
                ModelOp::Append(index, data) => {
                    let path = model_path(index);
                    if let Some(file) = model.get_mut(&path) {
                        file.extend_from_slice(&data);
                        engine.append(Path::new(&path), &data).unwrap();
                    }
                }
                ModelOp::Truncate(index, size) => {
                    let path = model_path(index);
                    if let Some(file) = model.get_mut(&path) {
                        file.resize(usize::from(size), 0);
                        engine.truncate(Path::new(&path), u64::from(size)).unwrap();
                    }
                }
                ModelOp::Create(index, data) => {
                    let path = model_path(index);
                    if let Entry::Vacant(entry) = model.entry(path) {
                        engine
                            .create_file(Path::new(entry.key()), &data, 0o600)
                            .unwrap();
                        entry.insert(data);
                    }
                }
                ModelOp::Unlink(index) => {
                    let path = model_path(index);
                    if model.remove(&path).is_some() {
                        engine.unlink(Path::new(&path)).unwrap();
                    }
                }
                ModelOp::Rename(old, new) => {
                    let old = model_path(old);
                    let new = model_path(new);
                    if old != new {
                        if let Some(contents) = model.remove(&old) {
                            engine.rename(Path::new(&old), Path::new(&new)).unwrap();
                            model.insert(new, contents);
                        }
                    }
                }
                ModelOp::Checkpoint => {
                    let checkpoint = engine.checkpoint(None).unwrap();
                    checkpoints.push((checkpoint.id, model.clone()));
                }
                ModelOp::Rollback(selector) => {
                    let position = usize::from(selector) % checkpoints.len();
                    let (id, snapshot) = checkpoints[position].clone();
                    engine.rollback(Some(id)).unwrap();
                    model = snapshot;
                    checkpoints.truncate(position + 1);
                }
                ModelOp::Commit => {
                    let checkpoint = engine.commit(None).unwrap();
                    checkpoints = vec![(checkpoint.id, model.clone())];
                }
            }
            assert_model(&fixture.live, &model);
            drop(engine);
            engine = fixture.reopen();
            assert_model(&fixture.live, &model);
        }
    }
}

#[test]
fn empty_control_operations_do_not_depend_on_tree_size() {
    for entries in [10, 1_000, 10_000] {
        let fixture = Fixture::new();
        for index in 0..entries {
            fs::write(fixture.live.join(format!("file-{index:05}")), b"x").unwrap();
        }
        let mut engine = fixture.engine();
        assert!(engine.diff(None).unwrap().is_empty());
        let checkpoint = engine.checkpoint(Some(format!("tree-{entries}"))).unwrap();
        assert!(engine.diff(Some(checkpoint.id)).unwrap().is_empty());
        engine.commit(Some(checkpoint.id)).unwrap();
        assert!(engine.diff(None).unwrap().is_empty());
    }
}

#[test]
fn set_times_round_trip_uses_stable_precision() {
    let fixture = Fixture::new();
    fs::write(fixture.live.join("a"), b"A").unwrap();
    let before = fs::metadata(fixture.live.join("a")).unwrap().mtime();
    let mut engine = fixture.engine();
    engine
        .set_times(
            Path::new("a"),
            SystemTime::now(),
            UNIX_EPOCH + Duration::from_secs(1_600_000_000),
        )
        .unwrap();
    engine.rollback(None).unwrap();
    assert_eq!(
        fs::metadata(fixture.live.join("a")).unwrap().mtime(),
        before
    );
}

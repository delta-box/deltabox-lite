mod fault;
mod journal;
mod model;
mod quota;
mod ranges;
mod snapshot;
mod storage;

pub use fault::{FaultInjector, FaultPoint};
pub use model::{CheckpointMeta as Checkpoint, Durability, VInodeId};
pub use ranges::{ByteRange, RangeSet};

use model::{
    ApplyOutcome, BackingIdentity, CommitMarker, EpochIndex, EpochMeta, EpochStatus, InodeEntry,
    InodeRegistry, JournalBody, JournalEvent, ObjectSnapshot, OperationKind, ReadyRecord,
    RecoveryAction, RecoveryState, SessionManifest, UndoAction,
};
use quota::{QuotaReservation, QuotaState};
use serde::{Deserialize, Serialize};
use snapshot::{
    apply_undo, checked_path, copy_path_state, materialize_object, materialize_segment,
    metadata_snapshot, normalize_relative, path_state, plan_object, plan_segment,
    set_owner as apply_owner, set_times as apply_times, ObjectCapturePlan, PathState,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const JOURNAL_RESERVATION: u64 = 4096;
const RECORD_RECOVERY_ALLOWANCE: u64 = 64;
const DEFAULT_QUOTA: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("state directory already contains a session")]
    SessionExists,
    #[error("state directory does not contain a session")]
    SessionMissing,
    #[error("state directory is not empty")]
    StateNotEmpty,
    #[error("state and LIVE directories must not overlap")]
    OverlappingRoots,
    #[error("invalid relative path: {0}")]
    InvalidPath(PathBuf),
    #[error("path escapes LIVE: {0}")]
    PathEscape(PathBuf),
    #[error("unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),
    #[error("hardlink alias cannot be resolved inside LIVE: {0}")]
    UnresolvedHardlink(PathBuf),
    #[error("checkpoint {0} does not exist in the linear history")]
    CheckpointNotFound(u64),
    #[error("quota exceeded: requested {requested} bytes, available {available} bytes")]
    QuotaExceeded { requested: u64, available: u64 },
    #[error("unsupported state format {found}; supported format is {supported}")]
    UnsupportedFormat { found: u32, supported: u32 },
    #[error("corrupt state: {0}")]
    CorruptState(String),
    #[error("corrupt journal: {0}")]
    CorruptJournal(String),
    #[error("recovery failed: {0}")]
    Recovery(String),
    #[error("engine invariant failed: {0}")]
    Invariant(String),
    #[error("injected fault at {0:?}")]
    InjectedFault(FaultPoint),
    #[error("engine must be reopened after an injected fault")]
    ReopenRequired,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, Copy)]
pub struct EngineOptions {
    pub quota_bytes: u64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            quota_bytes: DEFAULT_QUOTA,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
    Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_bytes: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_bytes: Option<Vec<u8>>,
}

const DIFF_CONTENT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineStatus {
    pub durability: Durability,
    pub active_epoch: u64,
    pub accepted_checkpoint: u64,
    pub checkpoints: Vec<Checkpoint>,
    pub quota_limit: u64,
    pub quota_used: u64,
    pub touched_paths: usize,
}

#[derive(Debug)]
struct EpochRuntime {
    meta: EpochMeta,
    index: EpochIndex,
    loaded: journal::LoadedJournal,
}

#[derive(Debug)]
pub struct StateEngine {
    live: PathBuf,
    state: PathBuf,
    manifest: SessionManifest,
    inodes: InodeRegistry,
    epochs: BTreeMap<u64, EpochRuntime>,
    fault: FaultInjector,
    poisoned: bool,
}

impl StateEngine {
    pub fn create(live: &Path, state: &Path, options: EngineOptions) -> Result<Self> {
        let live = live.canonicalize()?;
        if !live.is_dir() {
            return Err(EngineError::InvalidPath(live));
        }
        let resolved_state = resolve_destination(state)?;
        if live.starts_with(&resolved_state) || resolved_state.starts_with(&live) {
            return Err(EngineError::OverlappingRoots);
        }
        fs::create_dir_all(&resolved_state)?;
        let state = resolved_state.canonicalize()?;
        if state.join("session.json").exists() {
            return Err(EngineError::SessionExists);
        }
        if fs::read_dir(&state)?.next().is_some() {
            return Err(EngineError::StateNotEmpty);
        }
        for directory in ["epochs", "commits", "recovery", "gc"] {
            fs::create_dir_all(state.join(directory))?;
        }

        let initial_checkpoint = Checkpoint {
            id: 0,
            label: Some("initial".into()),
            after_epoch: None,
        };
        let manifest = SessionManifest {
            session_id: session_id(),
            live_root: path_string(&live)?,
            state_root: path_string(&state)?,
            durability: Durability::ProcessCrash,
            quota_limit: options.quota_bytes,
            active_epoch: 1,
            accepted_checkpoint: 0,
            checkpoints: vec![initial_checkpoint],
            next_epoch_id: 2,
            next_checkpoint_id: 1,
            next_record_id: 1,
            next_vinode_id: 1,
            next_commit_sequence: 1,
        };
        let meta = EpochMeta {
            id: 1,
            base_checkpoint: 0,
            end_checkpoint: None,
            status: EpochStatus::Open,
            quota: QuotaState::new(options.quota_bytes),
        };
        create_epoch_files(&state, &meta)?;
        storage::write_state(&state.join("session.json"), &manifest)?;
        storage::write_state(&state.join("inodes.idx"), &InodeRegistry::default())?;
        storage::write_state(&state.join("touched.idx"), &BTreeSet::<String>::new())?;

        let loaded = journal::load(&journal_path(&state, 1), 1)?;
        Ok(Self {
            live,
            state,
            manifest,
            inodes: InodeRegistry::default(),
            epochs: BTreeMap::from([(
                1,
                EpochRuntime {
                    meta,
                    index: EpochIndex::default(),
                    loaded,
                },
            )]),
            fault: FaultInjector::default(),
            poisoned: false,
        })
    }

    pub fn open(live: &Path, state: &Path) -> Result<Self> {
        let live = live.canonicalize()?;
        let state = state.canonicalize()?;
        if live.starts_with(&state) || state.starts_with(&live) {
            return Err(EngineError::OverlappingRoots);
        }
        let session_path = state.join("session.json");
        if !session_path.exists() {
            return Err(EngineError::SessionMissing);
        }
        let manifest: SessionManifest = storage::read_state(&session_path)?;
        if manifest.live_root != path_string(&live)? || manifest.state_root != path_string(&state)?
        {
            return Err(EngineError::CorruptState(
                "session roots do not match the requested roots".into(),
            ));
        }
        let inodes = if state.join("inodes.idx").exists() {
            storage::read_state(&state.join("inodes.idx"))?
        } else {
            InodeRegistry::default()
        };
        let mut engine = Self {
            live,
            state,
            manifest,
            inodes,
            epochs: BTreeMap::new(),
            fault: FaultInjector::default(),
            poisoned: false,
        };
        engine.load_epochs()?;
        engine.recover()?;
        Ok(engine)
    }

    pub fn set_fault_injector(&mut self, injector: FaultInjector) {
        self.fault = injector;
    }

    pub fn live_root(&self) -> &Path {
        &self.live
    }

    pub fn state_root(&self) -> &Path {
        &self.state
    }

    pub fn status(&self) -> EngineStatus {
        let active = self.active_epoch();
        EngineStatus {
            durability: self.manifest.durability.clone(),
            active_epoch: self.manifest.active_epoch,
            accepted_checkpoint: self.manifest.accepted_checkpoint,
            checkpoints: self.manifest.checkpoints.clone(),
            quota_limit: active.meta.quota.limit,
            quota_used: active.meta.quota.used,
            touched_paths: self.touched_since(self.manifest.accepted_checkpoint).len(),
        }
    }

    pub fn prepare_gc(&mut self) -> Result<Vec<PathBuf>> {
        self.ensure_usable()?;
        let gc = self.state.join("gc");
        fs::create_dir_all(&gc)?;
        let retired: Vec<u64> = self
            .epochs
            .iter()
            .filter_map(|(id, epoch)| (epoch.meta.status == EpochStatus::Retired).then_some(*id))
            .collect();
        for id in retired {
            let source = epoch_dir(&self.state, id);
            let destination = gc.join(format!("{id:08}"));
            fs::rename(source, &destination)?;
            self.epochs.remove(&id);
        }
        let mut paths = fs::read_dir(gc)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_type().ok()?.is_dir().then_some(entry.path()))
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    fn load_epochs(&mut self) -> Result<()> {
        let root = self.state.join("epochs");
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let meta_path = entry.path().join("epoch.json");
            let meta: EpochMeta = storage::read_state(&meta_path)?;
            let loaded = journal::load(&entry.path().join("journal.log"), meta.id)?;
            let index = rebuild_index(&loaded);
            self.epochs.insert(
                meta.id,
                EpochRuntime {
                    meta,
                    index,
                    loaded,
                },
            );
        }
        if !self.epochs.contains_key(&self.manifest.active_epoch) {
            return Err(EngineError::CorruptState(format!(
                "active epoch {} is missing",
                self.manifest.active_epoch
            )));
        }
        let max_epoch = self.epochs.keys().copied().max().unwrap_or(0);
        let max_checkpoint = self
            .manifest
            .checkpoints
            .iter()
            .map(|checkpoint| checkpoint.id)
            .chain(self.epochs.values().filter_map(|epoch| {
                epoch
                    .meta
                    .end_checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.id)
            }))
            .max()
            .unwrap_or(0);
        let max_record = self
            .epochs
            .values()
            .flat_map(|epoch| epoch.loaded.records.iter().map(|record| record.record_id))
            .max()
            .unwrap_or(0);
        let max_vinode = self.inodes.entries.keys().map(|id| id.0).max().unwrap_or(0);
        self.manifest.next_epoch_id = self.manifest.next_epoch_id.max(max_epoch.saturating_add(1));
        self.manifest.next_checkpoint_id = self
            .manifest
            .next_checkpoint_id
            .max(max_checkpoint.saturating_add(1));
        self.manifest.next_record_id = self.manifest.next_record_id.max(max_record + 1);
        self.manifest.next_vinode_id = self
            .manifest
            .next_vinode_id
            .max(max_vinode.saturating_add(1));
        Ok(())
    }

    fn active_epoch(&self) -> &EpochRuntime {
        self.epochs
            .get(&self.manifest.active_epoch)
            .expect("active epoch checked on open")
    }

    fn active_epoch_mut(&mut self) -> &mut EpochRuntime {
        self.epochs
            .get_mut(&self.manifest.active_epoch)
            .expect("active epoch checked on open")
    }

    fn ensure_usable(&self) -> Result<()> {
        if self.poisoned {
            Err(EngineError::ReopenRequired)
        } else {
            Ok(())
        }
    }

    fn inject(&mut self, point: FaultPoint) -> Result<()> {
        match self.fault.check(point) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    fn allocate_record(&mut self) -> u64 {
        let id = self.manifest.next_record_id;
        self.manifest.next_record_id += 1;
        id
    }

    fn reserve(&self, bytes: u64) -> Result<QuotaReservation> {
        let bytes = bytes
            .checked_add(JOURNAL_RESERVATION)
            .ok_or_else(|| EngineError::Invariant("quota reservation overflow".into()))?;
        self.active_epoch().meta.quota.reserve(bytes)
    }

    fn record_storage_bytes(&self, record_id: u64, record: &ReadyRecord) -> Result<u64> {
        let epoch = self.manifest.active_epoch;
        let sequence = self.active_epoch().loaded.next_sequence;
        let ready = journal::encoded_len(JournalBody {
            epoch,
            sequence,
            record_id,
            event: JournalEvent::Ready {
                record: record.clone(),
            },
        })?;
        let applied = journal::encoded_len(JournalBody {
            epoch,
            sequence: sequence.saturating_add(1),
            record_id,
            event: JournalEvent::Applied {
                outcome: ApplyOutcome::Applied,
            },
        })?;
        let index = serde_json::to_vec(record)?.len() as u64;
        record
            .preimage_bytes
            .checked_add(ready)
            .and_then(|bytes| bytes.checked_add(applied))
            .and_then(|bytes| bytes.checked_add(index))
            .and_then(|bytes| bytes.checked_add(RECORD_RECOVERY_ALLOWANCE))
            .ok_or_else(|| EngineError::Invariant("record storage size overflow".into()))
    }

    fn reserve_record(&self, record_id: u64, record: &ReadyRecord) -> Result<QuotaReservation> {
        self.reserve(self.record_storage_bytes(record_id, record)?)
    }

    fn persist_manifest(&self) -> Result<()> {
        storage::write_state(&self.state.join("session.json"), &self.manifest)
    }

    fn persist_inodes(&self) -> Result<()> {
        storage::write_state(&self.state.join("inodes.idx"), &self.inodes)
    }

    fn persist_epoch(&self, id: u64) -> Result<()> {
        let epoch = self
            .epochs
            .get(&id)
            .ok_or_else(|| EngineError::Invariant(format!("epoch {id} is missing")))?;
        storage::write_state(&epoch_meta_path(&self.state, id), &epoch.meta)
    }

    fn poison_on_error<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
}

impl StateEngine {
    fn resolve_inode(&mut self, relative: &str) -> Result<VInodeId> {
        let path = checked_path(&self.live, relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        let backing = BackingIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let existing = self.inodes.entries.iter().find_map(|(id, entry)| {
            (entry.backing == backing
                && entry.bindings.iter().any(|binding| {
                    fs::symlink_metadata(self.live.join(binding)).is_ok_and(|metadata| {
                        metadata.dev() == backing.device && metadata.ino() == backing.inode
                    })
                }))
            .then_some(*id)
        });
        if let Some(id) = existing {
            let entry = self
                .inodes
                .entries
                .get_mut(&id)
                .expect("inode entry selected from this registry");
            entry.bindings.insert(relative.to_owned());
            return Ok(entry.id);
        }
        let generation = self
            .inodes
            .entries
            .values()
            .filter(|entry| entry.backing == backing)
            .map(|entry| entry.generation)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let id = VInodeId(self.manifest.next_vinode_id);
        self.manifest.next_vinode_id += 1;
        self.inodes.entries.insert(
            id,
            InodeEntry {
                id,
                backing,
                generation,
                bindings: BTreeSet::from([relative.to_owned()]),
            },
        );
        Ok(id)
    }

    fn known_alias(&self, inode: VInodeId, except: &str) -> Option<String> {
        self.inodes.entries.get(&inode).and_then(|entry| {
            entry
                .bindings
                .iter()
                .filter(|binding| binding.as_str() != except)
                .find(|binding| fs::symlink_metadata(self.live.join(binding)).is_ok())
                .cloned()
        })
    }

    fn remove_binding(&mut self, inode: VInodeId, path: &str) {
        if let Some(entry) = self.inodes.entries.get_mut(&inode) {
            entry.bindings.remove(path);
        }
    }

    fn rename_binding(&mut self, inode: VInodeId, old: &str, new: &str) {
        if let Some(entry) = self.inodes.entries.get_mut(&inode) {
            entry.bindings.remove(old);
            entry.bindings.insert(new.to_owned());
        }
    }

    fn append_ready(&mut self, record_id: u64, record: ReadyRecord) -> Result<()> {
        let epoch_id = self.manifest.active_epoch;
        let sequence = self.active_epoch().loaded.next_sequence;
        journal::append(
            &journal_path(&self.state, epoch_id),
            JournalBody {
                epoch: epoch_id,
                sequence,
                record_id,
                event: JournalEvent::Ready {
                    record: record.clone(),
                },
            },
        )?;
        let epoch = self.active_epoch_mut();
        epoch.loaded.records.push(journal::LoadedRecord {
            record_id,
            ready: record,
            outcome: None,
        });
        epoch.loaded.next_sequence += 1;
        epoch.index = rebuild_index(&epoch.loaded);
        Ok(())
    }

    fn append_applied(&mut self, record_id: u64, outcome: ApplyOutcome) -> Result<()> {
        let epoch_id = self.manifest.active_epoch;
        let sequence = self.active_epoch().loaded.next_sequence;
        journal::append(
            &journal_path(&self.state, epoch_id),
            JournalBody {
                epoch: epoch_id,
                sequence,
                record_id,
                event: JournalEvent::Applied { outcome },
            },
        )?;
        let epoch = self.active_epoch_mut();
        let record = epoch
            .loaded
            .records
            .iter_mut()
            .find(|record| record.record_id == record_id)
            .ok_or_else(|| EngineError::Invariant(format!("record {record_id} disappeared")))?;
        record.outcome = Some(outcome);
        epoch.loaded.next_sequence += 1;
        epoch.index = rebuild_index(&epoch.loaded);
        Ok(())
    }

    fn run_record<T>(
        &mut self,
        record_id: u64,
        reservation: QuotaReservation,
        record: ReadyRecord,
        mutation: impl FnOnce(bool) -> Result<T>,
    ) -> Result<T> {
        let actual = self.record_storage_bytes(record_id, &record)?;
        if !reservation.covers(actual) {
            self.cleanup_orphan_preimages()?;
            return Err(EngineError::Invariant(format!(
                "planned quota reservation does not cover {actual} bytes"
            )));
        }
        let result = (|| {
            self.inject(FaultPoint::AfterPreimage)?;
            self.append_ready(record_id, record.clone())?;
            self.inject(FaultPoint::AfterReady)?;

            let partial = self.fault.take(FaultPoint::DuringMutation);
            let value = mutation(partial)?;
            if partial {
                return Err(EngineError::InjectedFault(FaultPoint::DuringMutation));
            }
            self.inject(FaultPoint::AfterMutation)?;
            self.append_applied(record_id, ApplyOutcome::Applied)?;

            self.active_epoch_mut()
                .meta
                .quota
                .commit(reservation, actual)?;
            let epoch_id = self.manifest.active_epoch;
            self.persist_epoch(epoch_id)?;
            self.persist_inodes()?;
            self.persist_manifest()?;
            storage::write_state(
                &self.state.join("touched.idx"),
                &self.touched_since(self.manifest.accepted_checkpoint),
            )?;
            self.inject(FaultPoint::AfterApplied)?;
            Ok(value)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn plan_object_for_record(
        &mut self,
        relative: &str,
        record_id: u64,
    ) -> Result<ObjectCapturePlan> {
        let inode = self.resolve_inode(relative)?;
        let path = checked_path(&self.live, relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        let alias = if metadata.file_type().is_file() && metadata.nlink() > 1 {
            let backing = BackingIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            Some(
                self.known_alias(inode, relative)
                    .or(find_backing_alias(&self.live, relative, backing)?)
                    .ok_or_else(|| EngineError::UnresolvedHardlink(PathBuf::from(relative)))?,
            )
        } else {
            None
        };
        if let Some(alias) = &alias {
            self.inodes
                .entries
                .get_mut(&inode)
                .expect("inode resolved immediately above")
                .bindings
                .insert(alias.clone());
        }
        plan_object(&path, record_id, inode, alias)
    }

    fn namespace_parent_actions(&mut self, paths: &[&str]) -> Result<Vec<UndoAction>> {
        let mut captured = BTreeSet::new();
        let mut actions = Vec::new();
        for relative in paths {
            let parent = Path::new(relative)
                .parent()
                .and_then(Path::to_str)
                .ok_or_else(|| EngineError::InvalidPath(PathBuf::from(relative)))?;
            let inode = self.resolve_inode(parent)?;
            if self.active_epoch().index.times.contains(&inode) || !captured.insert(inode) {
                continue;
            }
            let snapshot = metadata_snapshot(&checked_path(&self.live, parent)?)?;
            actions.push(UndoAction::RestoreTimes {
                path: parent.to_owned(),
                inode,
                atime_ns: snapshot.atime_ns,
                mtime_ns: snapshot.mtime_ns,
            });
        }
        Ok(actions)
    }
}

impl StateEngine {
    pub fn create_file(&mut self, path: &Path, contents: &[u8], mode: u32) -> Result<()> {
        self.create_file_record(path, |destination, partial| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(mode)
                .open(destination)?;
            let length = if partial {
                contents.len().div_ceil(2)
            } else {
                contents.len()
            };
            file.write_all(&contents[..length])?;
            Ok(())
        })
    }

    pub fn create_file_open(&mut self, path: &Path, mode: u32, flags: i32) -> Result<File> {
        self.create_file_record(path, |destination, _partial| {
            let access = flags & libc::O_ACCMODE;
            let mut options = OpenOptions::new();
            options
                .read(access == libc::O_RDONLY || access == libc::O_RDWR)
                .write(access == libc::O_WRONLY || access == libc::O_RDWR)
                .append(flags & libc::O_APPEND != 0)
                .create_new(true)
                .mode(mode)
                .custom_flags(
                    flags
                        & !(libc::O_ACCMODE
                            | libc::O_APPEND
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_TRUNC),
                )
                .open(destination)
                .map_err(EngineError::from)
        })
    }

    fn create_file_record<T>(
        &mut self,
        path: &Path,
        mutation: impl FnOnce(&Path, bool) -> Result<T>,
    ) -> Result<T> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into());
        }
        let record_id = self.allocate_record();
        let mut actions = self.namespace_parent_actions(&[&relative])?;
        actions.push(UndoAction::RemoveCreated {
            path: relative.clone(),
        });
        let record = ReadyRecord {
            operation: OperationKind::Create,
            actions,
            touched: BTreeSet::from([relative.clone()]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        let value = self.run_record(record_id, reservation, record, |partial| {
            mutation(&destination, partial)
        })?;
        let result = (|| {
            self.resolve_inode(&relative)?;
            self.persist_inodes()?;
            self.persist_manifest()
        })();
        self.poison_on_error(result)?;
        Ok(value)
    }

    pub fn create_dir(&mut self, path: &Path, mode: u32) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let record_id = self.allocate_record();
        let mut actions = self.namespace_parent_actions(&[&relative])?;
        actions.push(UndoAction::RemoveCreated {
            path: relative.clone(),
        });
        let record = ReadyRecord {
            operation: OperationKind::CreateDir,
            actions,
            touched: BTreeSet::from([relative.clone()]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            fs::create_dir(&destination)?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
            Ok(())
        })?;
        let result = (|| {
            self.resolve_inode(&relative)?;
            self.persist_inodes()?;
            self.persist_manifest()
        })();
        self.poison_on_error(result)
    }

    pub fn symlink(&mut self, target: &Path, link: &Path) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(link)?;
        let destination = checked_path(&self.live, &relative)?;
        let record_id = self.allocate_record();
        let mut actions = self.namespace_parent_actions(&[&relative])?;
        actions.push(UndoAction::RemoveCreated {
            path: relative.clone(),
        });
        let record = ReadyRecord {
            operation: OperationKind::Symlink,
            actions,
            touched: BTreeSet::from([relative.clone()]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            std::os::unix::fs::symlink(target, destination)?;
            Ok(())
        })?;
        let result = (|| {
            self.resolve_inode(&relative)?;
            self.persist_inodes()?;
            self.persist_manifest()
        })();
        self.poison_on_error(result)
    }

    pub fn write(&mut self, path: &Path, offset: u64, data: &[u8]) -> Result<()> {
        self.write_internal(
            path,
            offset,
            data,
            OperationKind::Write,
            |destination, offset, data, partial| {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(destination)?;
                file.seek(SeekFrom::Start(offset))?;
                let length = mutation_length(data, partial);
                file.write_all(&data[..length])?;
                Ok(())
            },
        )
    }

    pub fn write_file(&mut self, path: &Path, file: &File, offset: u64, data: &[u8]) -> Result<()> {
        self.write_internal(
            path,
            offset,
            data,
            OperationKind::Write,
            |_destination, offset, data, partial| {
                let length = mutation_length(data, partial);
                file.write_all_at(&data[..length], offset)?;
                Ok(())
            },
        )
    }

    pub fn append(&mut self, path: &Path, data: &[u8]) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let size = fs::metadata(checked_path(&self.live, &relative)?)?.len();
        self.write_internal(
            Path::new(&relative),
            size,
            data,
            OperationKind::Append,
            |destination, offset, data, partial| {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(destination)?;
                file.seek(SeekFrom::Start(offset))?;
                let length = mutation_length(data, partial);
                file.write_all(&data[..length])?;
                Ok(())
            },
        )
    }

    fn write_internal(
        &mut self,
        path: &Path,
        offset: u64,
        data: &[u8],
        operation: OperationKind,
        mutation: impl FnOnce(&Path, u64, &[u8], bool) -> Result<()>,
    ) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let metadata = fs::metadata(&destination)?;
        if !metadata.file_type().is_file() {
            return Err(EngineError::UnsupportedFileType(destination));
        }
        let inode = self.resolve_inode(&relative)?;
        let old_size = metadata.len();
        let created = self.active_epoch().index.created.contains(&relative);
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| EngineError::Invariant("write range overflow".into()))?;
        let capture_end = end.min(old_size);
        let requested = ByteRange::new(offset.min(capture_end), capture_end);
        let uncovered = if created || self.active_epoch().index.whole_inodes.contains(&inode) {
            Vec::new()
        } else if let Some(requested) = requested {
            self.active_epoch()
                .index
                .ranges
                .get(&inode)
                .cloned()
                .unwrap_or_default()
                .uncovered(requested)
        } else {
            Vec::new()
        };
        let record_id = self.allocate_record();
        let mut planned_segments = Vec::new();
        let mut preimage_bytes = 0_u64;
        for (index, range) in uncovered.iter().enumerate() {
            let (segment, bytes) = plan_segment(record_id, index, range.start, range.len())?;
            planned_segments.push(segment);
            preimage_bytes = preimage_bytes
                .checked_add(bytes)
                .ok_or_else(|| EngineError::Invariant("preimage size overflow".into()))?;
        }
        let capture_size_field =
            !created && end > old_size && !self.active_epoch().index.sizes.contains(&inode);
        let mut actions = Vec::new();
        if !created && !self.active_epoch().index.times.contains(&inode) {
            let snapshot = metadata_snapshot(&destination)?;
            actions.push(UndoAction::RestoreTimes {
                path: relative.clone(),
                inode,
                atime_ns: snapshot.atime_ns,
                mtime_ns: snapshot.mtime_ns,
            });
        }
        if !planned_segments.is_empty() || capture_size_field {
            actions.push(UndoAction::RestoreRanges {
                path: relative.clone(),
                inode,
                old_size: capture_size_field.then_some(old_size),
                segments: planned_segments.clone(),
            });
        }
        let mut record = ReadyRecord {
            operation,
            actions,
            touched: BTreeSet::from([relative]),
            preimage_bytes,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        let mut segments = Vec::with_capacity(planned_segments.len());
        for plan in &planned_segments {
            match materialize_segment(
                &destination,
                &preimage_dir(&self.state, self.manifest.active_epoch),
                plan,
            ) {
                Ok(segment) => segments.push(segment),
                Err(error) => {
                    self.cleanup_orphan_preimages()?;
                    return Err(error);
                }
            }
        }
        if let Some(UndoAction::RestoreRanges {
            segments: record_segments,
            ..
        }) = record
            .actions
            .iter_mut()
            .find(|action| matches!(action, UndoAction::RestoreRanges { .. }))
        {
            *record_segments = segments;
        }
        self.run_record(record_id, reservation, record, |partial| {
            mutation(&destination, offset, data, partial)
        })
    }

    pub fn truncate(&mut self, path: &Path, size: u64) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let metadata = fs::metadata(&destination)?;
        let inode = self.resolve_inode(&relative)?;
        let old_size = metadata.len();
        let needs_capture = !self.active_epoch().index.whole_inodes.contains(&inode);
        let record_id = self.allocate_record();
        let mut actions = Vec::new();
        let mut preimage_bytes = 0_u64;
        if !self.active_epoch().index.times.contains(&inode) {
            let snapshot = metadata_snapshot(&destination)?;
            actions.push(UndoAction::RestoreTimes {
                path: relative.clone(),
                inode,
                atime_ns: snapshot.atime_ns,
                mtime_ns: snapshot.mtime_ns,
            });
        }
        if needs_capture {
            let mut segments = Vec::new();
            if old_size > 0 {
                let (segment, bytes) = plan_segment(record_id, 0, 0, old_size)?;
                segments.push(segment);
                preimage_bytes = bytes;
            }
            actions.push(UndoAction::RestoreRanges {
                path: relative.clone(),
                inode,
                old_size: Some(old_size),
                segments,
            });
        }
        let mut record = ReadyRecord {
            operation: OperationKind::Truncate,
            actions,
            touched: BTreeSet::from([relative]),
            preimage_bytes,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        if let Some(UndoAction::RestoreRanges { segments, .. }) = record
            .actions
            .iter_mut()
            .find(|action| matches!(action, UndoAction::RestoreRanges { .. }))
        {
            for segment in segments {
                match materialize_segment(
                    &destination,
                    &preimage_dir(&self.state, self.manifest.active_epoch),
                    segment,
                ) {
                    Ok(materialized) => *segment = materialized,
                    Err(error) => {
                        self.cleanup_orphan_preimages()?;
                        return Err(error);
                    }
                }
            }
        }
        self.run_record(record_id, reservation, record, |_partial| {
            OpenOptions::new()
                .write(true)
                .open(destination)?
                .set_len(size)?;
            Ok(())
        })
    }

    pub fn unlink(&mut self, path: &Path) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let record_id = self.allocate_record();
        let plan = self.plan_object_for_record(&relative, record_id)?;
        let inode = plan.snapshot.inode;
        let mut touched = BTreeSet::from([relative.clone()]);
        if let model::ObjectSnapshotKind::Hardlink { source } = &plan.snapshot.kind {
            touched.insert(source.clone());
        }
        let mut actions = self.namespace_parent_actions(&[&relative])?;
        actions.push(UndoAction::RestoreObject {
            path: relative.clone(),
            snapshot: plan.snapshot.clone(),
        });
        let mut record = ReadyRecord {
            operation: OperationKind::Unlink,
            actions,
            touched,
            preimage_bytes: plan.preimage_bytes,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        let snapshot = match materialize_object(
            &destination,
            &preimage_dir(&self.state, self.manifest.active_epoch),
            &plan,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.cleanup_orphan_preimages()?;
                return Err(error);
            }
        };
        if let Some(UndoAction::RestoreObject {
            snapshot: record_snapshot,
            ..
        }) = record
            .actions
            .iter_mut()
            .find(|action| matches!(action, UndoAction::RestoreObject { .. }))
        {
            *record_snapshot = snapshot;
        }
        self.run_record(record_id, reservation, record, |_partial| {
            let metadata = fs::symlink_metadata(&destination)?;
            if metadata.file_type().is_dir() {
                fs::remove_dir(&destination)?;
            } else {
                fs::remove_file(&destination)?;
            }
            Ok(())
        })?;
        self.remove_binding(inode, &relative);
        let result = self.persist_inodes();
        self.poison_on_error(result)
    }

    pub fn rename(&mut self, old: &Path, new: &Path) -> Result<()> {
        self.ensure_usable()?;
        let old_relative = normalize_relative(old)?;
        let new_relative = normalize_relative(new)?;
        let old_path = checked_path(&self.live, &old_relative)?;
        let new_path = checked_path(&self.live, &new_relative)?;
        let source_inode = self.resolve_inode(&old_relative)?;
        let target_exists = fs::symlink_metadata(&new_path).is_ok();
        let record_id = self.allocate_record();
        let replacement_plan = if target_exists {
            Some(self.plan_object_for_record(&new_relative, record_id)?)
        } else {
            None
        };
        let replaced = replacement_plan.as_ref().map(|plan| plan.snapshot.clone());
        let preimage_bytes = replacement_plan
            .as_ref()
            .map_or(0, |plan| plan.preimage_bytes);
        let replaced_inode = replacement_plan.as_ref().map(|plan| plan.snapshot.inode);
        let mut touched = BTreeSet::from([old_relative.clone(), new_relative.clone()]);
        if let Some(ObjectSnapshot {
            kind: model::ObjectSnapshotKind::Hardlink { source },
            ..
        }) = &replaced
        {
            touched.insert(source.clone());
        }
        let mut actions = self.namespace_parent_actions(&[&old_relative, &new_relative])?;
        actions.push(UndoAction::UndoRename {
            old_path: old_relative.clone(),
            new_path: new_relative.clone(),
            source_inode,
            replaced,
        });
        let mut record = ReadyRecord {
            operation: OperationKind::Rename,
            actions,
            touched,
            preimage_bytes,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        if let Some(plan) = &replacement_plan {
            let snapshot = match materialize_object(
                &new_path,
                &preimage_dir(&self.state, self.manifest.active_epoch),
                plan,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.cleanup_orphan_preimages()?;
                    return Err(error);
                }
            };
            if let Some(UndoAction::UndoRename { replaced, .. }) = record
                .actions
                .iter_mut()
                .find(|action| matches!(action, UndoAction::UndoRename { .. }))
            {
                *replaced = Some(snapshot);
            }
        }
        self.run_record(record_id, reservation, record, |_partial| {
            fs::rename(&old_path, &new_path)?;
            Ok(())
        })?;
        self.rename_binding(source_inode, &old_relative, &new_relative);
        if let Some(inode) = replaced_inode {
            self.remove_binding(inode, &new_relative);
        }
        let result = self.persist_inodes();
        self.poison_on_error(result)
    }

    pub fn hard_link(&mut self, source: &Path, target: &Path) -> Result<()> {
        self.ensure_usable()?;
        let source_relative = normalize_relative(source)?;
        let target_relative = normalize_relative(target)?;
        let source_path = checked_path(&self.live, &source_relative)?;
        let target_path = checked_path(&self.live, &target_relative)?;
        let source_inode = self.resolve_inode(&source_relative)?;
        let record_id = self.allocate_record();
        let mut actions = self.namespace_parent_actions(&[&target_relative])?;
        actions.push(UndoAction::RemoveCreated {
            path: target_relative.clone(),
        });
        let record = ReadyRecord {
            operation: OperationKind::Hardlink,
            actions,
            touched: BTreeSet::from([source_relative, target_relative.clone()]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            fs::hard_link(source_path, target_path)?;
            Ok(())
        })?;
        let result = (|| {
            self.resolve_inode(&target_relative)?;
            if let Some(entry) = self.inodes.entries.get_mut(&source_inode) {
                entry.bindings.insert(target_relative);
            }
            self.persist_inodes()
        })();
        self.poison_on_error(result)
    }
}

impl StateEngine {
    pub fn set_mode(&mut self, path: &Path, mode: u32) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let metadata = fs::symlink_metadata(&destination)?;
        let inode = self.resolve_inode(&relative)?;
        let actions = if self.active_epoch().index.mode.contains(&inode) {
            Vec::new()
        } else {
            vec![UndoAction::RestoreMode {
                path: relative.clone(),
                inode,
                mode: metadata.mode(),
            }]
        };
        let record_id = self.allocate_record();
        let record = ReadyRecord {
            operation: OperationKind::SetMode,
            actions,
            touched: BTreeSet::from([relative]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
            Ok(())
        })
    }

    pub fn set_owner(&mut self, path: &Path, uid: u32, gid: u32) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let metadata = fs::symlink_metadata(&destination)?;
        let inode = self.resolve_inode(&relative)?;
        let actions = if self.active_epoch().index.owner.contains(&inode) {
            Vec::new()
        } else {
            vec![UndoAction::RestoreOwner {
                path: relative.clone(),
                inode,
                uid: metadata.uid(),
                gid: metadata.gid(),
            }]
        };
        let record_id = self.allocate_record();
        let record = ReadyRecord {
            operation: OperationKind::SetOwner,
            actions,
            touched: BTreeSet::from([relative]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            apply_owner(&destination, uid, gid)
        })
    }

    pub fn set_times(&mut self, path: &Path, atime: SystemTime, mtime: SystemTime) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let inode = self.resolve_inode(&relative)?;
        let metadata = metadata_snapshot(&destination)?;
        let actions = if self.active_epoch().index.times.contains(&inode) {
            Vec::new()
        } else {
            vec![UndoAction::RestoreTimes {
                path: relative.clone(),
                inode,
                atime_ns: metadata.atime_ns,
                mtime_ns: metadata.mtime_ns,
            }]
        };
        let atime_ns = system_time_ns(atime)?;
        let mtime_ns = system_time_ns(mtime)?;
        let record_id = self.allocate_record();
        let record = ReadyRecord {
            operation: OperationKind::SetTimes,
            actions,
            touched: BTreeSet::from([relative]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            apply_times(&destination, atime_ns, mtime_ns)
        })
    }

    pub fn set_xattr(&mut self, path: &Path, name: &str, value: &[u8]) -> Result<()> {
        self.xattr_mutation(path, name, Some(value), OperationKind::SetXattr)
    }

    pub fn remove_xattr(&mut self, path: &Path, name: &str) -> Result<()> {
        self.xattr_mutation(path, name, None, OperationKind::RemoveXattr)
    }

    fn xattr_mutation(
        &mut self,
        path: &Path,
        name: &str,
        value: Option<&[u8]>,
        operation: OperationKind,
    ) -> Result<()> {
        self.ensure_usable()?;
        let relative = normalize_relative(path)?;
        let destination = checked_path(&self.live, &relative)?;
        let inode = self.resolve_inode(&relative)?;
        let key = (inode, name.to_owned());
        let actions = if self.active_epoch().index.xattrs.contains(&key) {
            Vec::new()
        } else {
            vec![UndoAction::RestoreXattr {
                path: relative.clone(),
                inode,
                name: name.to_owned(),
                value: xattr::get(&destination, name)?,
            }]
        };
        let record_id = self.allocate_record();
        let record = ReadyRecord {
            operation,
            actions,
            touched: BTreeSet::from([relative]),
            preimage_bytes: 0,
        };
        let reservation = self.reserve_record(record_id, &record)?;
        self.run_record(record_id, reservation, record, |_partial| {
            if let Some(value) = value {
                xattr::set(&destination, name, value)?;
            } else {
                xattr::remove(&destination, name)?;
            }
            Ok(())
        })
    }
}

impl StateEngine {
    pub fn checkpoint(&mut self, label: Option<String>) -> Result<Checkpoint> {
        self.ensure_usable()?;
        let result = (|| {
            self.inject(FaultPoint::DuringCheckpointSeal)?;
            let checkpoint = Checkpoint {
                id: self.manifest.next_checkpoint_id,
                label,
                after_epoch: Some(self.manifest.active_epoch),
            };
            self.manifest.next_checkpoint_id += 1;
            {
                let epoch = self.active_epoch_mut();
                if epoch.meta.status != EpochStatus::Open {
                    return Err(EngineError::Invariant("active epoch is not open".into()));
                }
                epoch.meta.status = EpochStatus::Sealed;
                epoch.meta.end_checkpoint = Some(checkpoint.clone());
            }
            self.persist_epoch(self.manifest.active_epoch)?;
            self.inject(FaultPoint::AfterCheckpointSeal)?;
            self.finish_sealed_checkpoint()?;
            Ok(checkpoint)
        })();
        self.poison_on_error(result)
    }

    pub fn rollback(&mut self, target: Option<u64>) -> Result<()> {
        self.ensure_usable()?;
        let target = target.unwrap_or(self.manifest.accepted_checkpoint);
        self.require_checkpoint(target)?;
        let result = (|| {
            storage::write_state(
                &self.state.join("recovery/active.json"),
                &RecoveryState {
                    action: RecoveryAction::Rollback {
                        target_checkpoint: target,
                    },
                },
            )?;
            self.resume_rollback(target)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    pub fn commit(&mut self, target: Option<u64>) -> Result<Checkpoint> {
        self.ensure_usable()?;
        if let Some(target) = target {
            self.require_checkpoint(target)?;
            let latest = self
                .manifest
                .checkpoints
                .last()
                .map(|checkpoint| checkpoint.id)
                .ok_or_else(|| EngineError::Invariant("checkpoint stack is empty".into()))?;
            if target != latest || !self.active_epoch().index.touched.is_empty() {
                self.rollback(Some(target))?;
            }
        }
        let sequence = self.manifest.next_commit_sequence;
        let checkpoint = Checkpoint {
            id: self.manifest.next_checkpoint_id,
            label: Some(format!("accepted-{sequence}")),
            after_epoch: Some(self.manifest.active_epoch),
        };
        let retired_epochs = self.active_history_epoch_ids(self.manifest.accepted_checkpoint);
        let marker = CommitMarker {
            sequence,
            checkpoint: checkpoint.clone(),
            retired_epochs,
        };
        let result = (|| {
            storage::write_state(
                &self.state.join("recovery/active.json"),
                &RecoveryState {
                    action: RecoveryAction::Commit {
                        marker_sequence: sequence,
                    },
                },
            )?;
            self.inject(FaultPoint::BeforeCommitMarker)?;
            storage::write_state(&commit_marker_path(&self.state, sequence), &marker)?;
            self.inject(FaultPoint::AfterCommitMarker)?;
            self.finalize_commit(marker)?;
            Ok(checkpoint)
        })();
        self.poison_on_error(result)
    }

    pub fn diff(&self, from: Option<u64>) -> Result<Vec<Change>> {
        self.ensure_usable()?;
        let target = from.unwrap_or(self.manifest.accepted_checkpoint);
        self.require_checkpoint(target)?;
        let epoch_ids = self.active_history_epoch_ids(target);
        let touched = self.touched_for_epochs(&epoch_ids);
        if touched.is_empty() {
            return Ok(Vec::new());
        }

        let scratch = self.state.join("recovery").join(format!(
            "diff-{}-{}",
            std::process::id(),
            self.manifest.next_record_id
        ));
        if scratch.exists() {
            fs::remove_dir_all(&scratch)?;
        }
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        let result = (|| {
            for path in &touched {
                let mut parent = Path::new(path).parent();
                while let Some(path) = parent {
                    if !path.as_os_str().is_empty() {
                        fs::create_dir_all(scratch.join(path))?;
                    }
                    parent = path.parent();
                }
            }
            let mut paths: Vec<&String> = touched.iter().collect();
            paths.sort_by_key(|path| Path::new(path).components().count());
            let mut copied_inodes: BTreeMap<BackingIdentity, String> = BTreeMap::new();
            for path in paths {
                let source = checked_path(&self.live, path)?;
                let metadata = fs::symlink_metadata(&source).ok();
                let identity = metadata.as_ref().and_then(|metadata| {
                    metadata.file_type().is_file().then_some(BackingIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    })
                });
                if let Some(alias) = identity.and_then(|identity| copied_inodes.get(&identity)) {
                    fs::hard_link(scratch.join(alias), scratch.join(path))?;
                } else {
                    copy_path_state(&self.live, &scratch, path)?;
                    if let Some(identity) = identity {
                        copied_inodes.insert(identity, path.clone());
                    }
                }
            }

            for epoch_id in epoch_ids.iter().rev() {
                let epoch = self.epochs.get(epoch_id).ok_or_else(|| {
                    EngineError::Invariant(format!("epoch {epoch_id} is missing"))
                })?;
                for record in epoch.loaded.records.iter().rev() {
                    if record.outcome != Some(ApplyOutcome::Applied) {
                        continue;
                    }
                    for action in record.ready.actions.iter().rev() {
                        apply_undo(&scratch, &preimage_dir(&self.state, *epoch_id), action)?;
                    }
                }
            }

            let old_links = hardlink_peers(&scratch, &touched)?;
            let current_links = hardlink_peers(&self.live, &touched)?;
            let mut changes = Vec::new();
            for path in &touched {
                let old = path_state(&scratch, path)?;
                let current = path_state(&self.live, path)?;
                let kind = classify_change(&old, &current).or_else(|| {
                    (old_links.get(path) != current_links.get(path)).then_some(ChangeKind::Modified)
                });
                if let Some(kind) = kind {
                    let (old_size, old_bytes) = diff_content(&old);
                    let (new_size, new_bytes) = diff_content(&current);
                    changes.push(Change {
                        path: path.clone(),
                        kind,
                        old_size,
                        new_size,
                        old_bytes,
                        new_bytes,
                    });
                }
            }
            Ok(changes)
        })();
        let cleanup = fs::remove_dir_all(&scratch);
        match (result, cleanup) {
            (Ok(changes), Ok(())) => Ok(changes),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    fn require_checkpoint(&self, id: u64) -> Result<&Checkpoint> {
        self.manifest
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.id == id)
            .ok_or(EngineError::CheckpointNotFound(id))
    }

    fn finish_sealed_checkpoint(&mut self) -> Result<()> {
        let active_id = self.manifest.active_epoch;
        let checkpoint = self
            .active_epoch()
            .meta
            .end_checkpoint
            .clone()
            .ok_or_else(|| EngineError::Recovery("sealed epoch has no checkpoint".into()))?;
        if !self
            .manifest
            .checkpoints
            .iter()
            .any(|existing| existing.id == checkpoint.id)
        {
            self.manifest.checkpoints.push(checkpoint.clone());
        }
        self.manifest.next_checkpoint_id = self
            .manifest
            .next_checkpoint_id
            .max(checkpoint.id.saturating_add(1));
        let new_epoch = self.open_new_epoch(checkpoint.id)?;
        self.manifest.active_epoch = new_epoch;
        self.persist_manifest()?;
        self.persist_epoch(active_id)?;
        Ok(())
    }

    fn open_new_epoch(&mut self, base_checkpoint: u64) -> Result<u64> {
        if let Some((id, _)) = self.epochs.iter().find(|(_, epoch)| {
            epoch.meta.status == EpochStatus::Open
                && epoch.meta.base_checkpoint == base_checkpoint
                && epoch.meta.id != self.manifest.active_epoch
        }) {
            return Ok(*id);
        }
        let id = self.manifest.next_epoch_id;
        self.manifest.next_epoch_id += 1;
        let meta = EpochMeta {
            id,
            base_checkpoint,
            end_checkpoint: None,
            status: EpochStatus::Open,
            quota: QuotaState::new(self.manifest.quota_limit),
        };
        create_epoch_files(&self.state, &meta)?;
        let loaded = journal::load(&journal_path(&self.state, id), id)?;
        self.epochs.insert(
            id,
            EpochRuntime {
                meta,
                index: EpochIndex::default(),
                loaded,
            },
        );
        Ok(id)
    }

    fn active_history_epoch_ids(&self, target: u64) -> Vec<u64> {
        let checkpoint_ids: BTreeSet<u64> = self
            .manifest
            .checkpoints
            .iter()
            .skip_while(|checkpoint| checkpoint.id != target)
            .map(|checkpoint| checkpoint.id)
            .collect();
        self.epochs
            .iter()
            .filter_map(|(id, epoch)| {
                (checkpoint_ids.contains(&epoch.meta.base_checkpoint)
                    && matches!(
                        epoch.meta.status,
                        EpochStatus::Open | EpochStatus::Sealed | EpochStatus::RollingBack
                    ))
                .then_some(*id)
            })
            .collect()
    }

    fn touched_for_epochs(&self, epoch_ids: &[u64]) -> BTreeSet<String> {
        epoch_ids
            .iter()
            .filter_map(|id| self.epochs.get(id))
            .flat_map(|epoch| epoch.index.touched.iter().cloned())
            .collect()
    }

    fn touched_since(&self, checkpoint: u64) -> BTreeSet<String> {
        self.touched_for_epochs(&self.active_history_epoch_ids(checkpoint))
    }
}

impl StateEngine {
    fn recover(&mut self) -> Result<()> {
        if let Some(marker) = self.latest_commit_marker()? {
            if marker.checkpoint.id != self.manifest.accepted_checkpoint {
                self.finalize_commit(marker)?;
            }
        }
        self.recover_unapplied_records()?;

        if self.active_epoch().meta.status == EpochStatus::Sealed {
            self.finish_sealed_checkpoint()?;
        }

        let recovery_path = self.state.join("recovery/active.json");
        if recovery_path.exists() {
            let recovery: RecoveryState = storage::read_state(&recovery_path)?;
            match recovery.action {
                RecoveryAction::Rollback { target_checkpoint } => {
                    self.resume_rollback(target_checkpoint)?;
                }
                RecoveryAction::Commit { marker_sequence } => {
                    let marker_path = commit_marker_path(&self.state, marker_sequence);
                    if marker_path.exists() {
                        self.finalize_commit(storage::read_state(&marker_path)?)?;
                    } else {
                        storage::remove_if_exists(&recovery_path)?;
                    }
                }
            }
        }
        self.cleanup_orphan_preimages()?;
        self.rebuild_bindings()?;
        self.persist_manifest()?;
        self.persist_inodes()?;
        storage::write_state(
            &self.state.join("touched.idx"),
            &self.touched_since(self.manifest.accepted_checkpoint),
        )?;
        Ok(())
    }

    fn recover_unapplied_records(&mut self) -> Result<()> {
        let epoch_ids: Vec<u64> = self
            .epochs
            .iter()
            .filter_map(|(id, epoch)| {
                (!matches!(
                    epoch.meta.status,
                    EpochStatus::Retired | EpochStatus::RolledBack
                ))
                .then_some(*id)
            })
            .collect();
        for epoch_id in epoch_ids {
            let unresolved: Vec<usize> = self
                .epochs
                .get(&epoch_id)
                .expect("epoch id came from map")
                .loaded
                .records
                .iter()
                .enumerate()
                .filter_map(|(index, record)| record.outcome.is_none().then_some(index))
                .collect();
            if let Some(first) = unresolved.first().copied() {
                let has_later_applied = self
                    .epochs
                    .get(&epoch_id)
                    .expect("epoch id came from map")
                    .loaded
                    .records
                    .iter()
                    .skip(first + 1)
                    .any(|record| record.outcome == Some(ApplyOutcome::Applied));
                if has_later_applied {
                    return Err(EngineError::CorruptJournal(format!(
                        "epoch {epoch_id} has an applied record behind an unresolved record"
                    )));
                }
            }
            for index in unresolved.into_iter().rev() {
                let record = self
                    .epochs
                    .get(&epoch_id)
                    .expect("epoch id came from map")
                    .loaded
                    .records[index]
                    .clone();
                for action in record.ready.actions.iter().rev() {
                    apply_undo(&self.live, &preimage_dir(&self.state, epoch_id), action)?;
                }
                self.append_applied_to_epoch(epoch_id, record.record_id, ApplyOutcome::Recovered)?;
            }
            self.rebuild_epoch_quota(epoch_id)?;
        }
        Ok(())
    }

    fn append_applied_to_epoch(
        &mut self,
        epoch_id: u64,
        record_id: u64,
        outcome: ApplyOutcome,
    ) -> Result<()> {
        let sequence = self
            .epochs
            .get(&epoch_id)
            .ok_or_else(|| EngineError::Invariant(format!("epoch {epoch_id} is missing")))?
            .loaded
            .next_sequence;
        journal::append(
            &journal_path(&self.state, epoch_id),
            JournalBody {
                epoch: epoch_id,
                sequence,
                record_id,
                event: JournalEvent::Applied { outcome },
            },
        )?;
        let epoch = self
            .epochs
            .get_mut(&epoch_id)
            .expect("epoch existence checked above");
        epoch
            .loaded
            .records
            .iter_mut()
            .find(|record| record.record_id == record_id)
            .ok_or_else(|| EngineError::Invariant(format!("record {record_id} is missing")))?
            .outcome = Some(outcome);
        epoch.loaded.next_sequence += 1;
        epoch.index = rebuild_index(&epoch.loaded);
        Ok(())
    }

    fn resume_rollback(&mut self, target: u64) -> Result<()> {
        self.require_checkpoint(target)?;
        if self.active_epoch().meta.status == EpochStatus::Open
            && self.active_epoch().meta.base_checkpoint == target
            && self.active_epoch().loaded.records.is_empty()
            && self.epochs.values().all(|epoch| {
                epoch.meta.id == self.manifest.active_epoch
                    || !matches!(
                        epoch.meta.status,
                        EpochStatus::Open | EpochStatus::Sealed | EpochStatus::RollingBack
                    )
                    || epoch.meta.base_checkpoint < target
            })
        {
            storage::remove_if_exists(&self.state.join("recovery/active.json"))?;
            return Ok(());
        }
        let mut epoch_ids = self.active_history_epoch_ids(target);
        epoch_ids.sort_unstable_by(|left, right| right.cmp(left));
        for epoch_id in epoch_ids {
            let status = self
                .epochs
                .get(&epoch_id)
                .expect("history epoch must exist")
                .meta
                .status;
            if status == EpochStatus::RolledBack {
                continue;
            }
            self.epochs
                .get_mut(&epoch_id)
                .expect("history epoch must exist")
                .meta
                .status = EpochStatus::RollingBack;
            self.persist_epoch(epoch_id)?;

            let records = self
                .epochs
                .get(&epoch_id)
                .expect("history epoch must exist")
                .loaded
                .records
                .clone();
            for record in records.iter().rev() {
                if record.outcome != Some(ApplyOutcome::Applied) {
                    continue;
                }
                for action in record.ready.actions.iter().rev() {
                    apply_undo(&self.live, &preimage_dir(&self.state, epoch_id), action)?;
                    self.inject(FaultPoint::DuringRollback)?;
                }
            }
            self.epochs
                .get_mut(&epoch_id)
                .expect("history epoch must exist")
                .meta
                .status = EpochStatus::RolledBack;
            self.persist_epoch(epoch_id)?;
        }

        let target_position = self
            .manifest
            .checkpoints
            .iter()
            .position(|checkpoint| checkpoint.id == target)
            .ok_or(EngineError::CheckpointNotFound(target))?;
        self.manifest.checkpoints.truncate(target_position + 1);
        let new_epoch = self.open_new_epoch(target)?;
        self.manifest.active_epoch = new_epoch;
        self.persist_manifest()?;
        storage::remove_if_exists(&self.state.join("recovery/active.json"))?;
        self.rebuild_bindings()?;
        self.persist_inodes()?;
        storage::write_state(
            &self.state.join("touched.idx"),
            &self.touched_since(self.manifest.accepted_checkpoint),
        )?;
        Ok(())
    }

    fn finalize_commit(&mut self, marker: CommitMarker) -> Result<()> {
        if self.manifest.accepted_checkpoint == marker.checkpoint.id
            && self.active_epoch().meta.status == EpochStatus::Open
            && self.active_epoch().meta.base_checkpoint == marker.checkpoint.id
        {
            storage::remove_if_exists(&self.state.join("recovery/active.json"))?;
            return Ok(());
        }
        for epoch_id in &marker.retired_epochs {
            if let Some(epoch) = self.epochs.get_mut(epoch_id) {
                epoch.meta.status = EpochStatus::Retired;
            }
            if self.epochs.contains_key(epoch_id) {
                self.persist_epoch(*epoch_id)?;
            }
        }
        self.manifest.accepted_checkpoint = marker.checkpoint.id;
        self.manifest.checkpoints = vec![marker.checkpoint.clone()];
        self.manifest.next_checkpoint_id = self
            .manifest
            .next_checkpoint_id
            .max(marker.checkpoint.id.saturating_add(1));
        self.manifest.next_commit_sequence = self
            .manifest
            .next_commit_sequence
            .max(marker.sequence.saturating_add(1));
        let new_epoch = self.open_new_epoch(marker.checkpoint.id)?;
        self.manifest.active_epoch = new_epoch;
        self.persist_manifest()?;
        storage::write_state(&self.state.join("touched.idx"), &BTreeSet::<String>::new())?;
        storage::remove_if_exists(&self.state.join("recovery/active.json"))?;
        Ok(())
    }

    fn latest_commit_marker(&self) -> Result<Option<CommitMarker>> {
        let mut markers = Vec::new();
        for entry in fs::read_dir(self.state.join("commits"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                markers.push(storage::read_state::<CommitMarker>(&entry.path())?);
            }
        }
        markers.sort_by_key(|marker| marker.sequence);
        Ok(markers.pop())
    }

    fn rebuild_epoch_quota(&mut self, epoch_id: u64) -> Result<()> {
        let preimages: u64 = self
            .epochs
            .get(&epoch_id)
            .ok_or_else(|| EngineError::Invariant(format!("epoch {epoch_id} is missing")))?
            .loaded
            .records
            .iter()
            .map(|record| record.ready.preimage_bytes)
            .sum();
        let journal_bytes = fs::metadata(journal_path(&self.state, epoch_id))?.len();
        let records = &self
            .epochs
            .get(&epoch_id)
            .expect("epoch existence checked above")
            .loaded
            .records;
        let index_bytes = records.iter().try_fold(0_u64, |total, record| {
            let bytes = serde_json::to_vec(&record.ready)?.len() as u64;
            total
                .checked_add(bytes)
                .ok_or_else(|| EngineError::Invariant("quota index usage overflow".into()))
        })?;
        let record_count = records.len() as u64;
        let used = preimages
            .checked_add(journal_bytes)
            .and_then(|bytes| bytes.checked_add(index_bytes))
            .and_then(|bytes| {
                record_count
                    .checked_mul(RECORD_RECOVERY_ALLOWANCE)
                    .and_then(|allowance| bytes.checked_add(allowance))
            })
            .ok_or_else(|| EngineError::Invariant("quota usage overflow".into()))?;
        let epoch = self
            .epochs
            .get_mut(&epoch_id)
            .expect("epoch existence checked above");
        epoch.meta.quota.used = used;
        self.persist_epoch(epoch_id)
    }

    fn cleanup_orphan_preimages(&self) -> Result<()> {
        for (epoch_id, epoch) in &self.epochs {
            let mut referenced = BTreeSet::new();
            for record in &epoch.loaded.records {
                for action in &record.ready.actions {
                    match action {
                        UndoAction::RestoreRanges { segments, .. } => {
                            referenced.extend(segments.iter().map(|segment| segment.file.clone()));
                        }
                        UndoAction::RestoreObject {
                            snapshot:
                                ObjectSnapshot {
                                    kind: model::ObjectSnapshotKind::Regular { data, .. },
                                    ..
                                },
                            ..
                        }
                        | UndoAction::UndoRename {
                            replaced:
                                Some(ObjectSnapshot {
                                    kind: model::ObjectSnapshotKind::Regular { data, .. },
                                    ..
                                }),
                            ..
                        } => {
                            referenced.insert(data.clone());
                        }
                        _ => {}
                    }
                }
            }
            let directory = preimage_dir(&self.state, *epoch_id);
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| EngineError::CorruptState("non-UTF-8 preimage name".into()))?;
                if !referenced.contains(name) && entry.file_type()?.is_file() {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }

    fn rebuild_bindings(&mut self) -> Result<()> {
        let candidates: BTreeSet<String> = self
            .inodes
            .entries
            .values()
            .flat_map(|entry| entry.bindings.iter().cloned())
            .chain(
                self.epochs
                    .values()
                    .flat_map(|epoch| epoch.index.touched.iter().cloned()),
            )
            .collect();
        self.inodes = InodeRegistry::default();
        for relative in candidates {
            if fs::symlink_metadata(self.live.join(&relative)).is_ok() {
                self.resolve_inode(&relative)?;
            }
        }
        Ok(())
    }
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| EngineError::InvalidPath(path.to_path_buf()))
}

fn resolve_destination(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| EngineError::InvalidPath(absolute.clone()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| EngineError::InvalidPath(absolute.clone()))?;
    }
    let mut resolved = existing.canonicalize()?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{nanos:x}", std::process::id())
}

fn mutation_length(data: &[u8], partial: bool) -> usize {
    if partial && !data.is_empty() {
        data.len().div_ceil(2)
    } else {
        data.len()
    }
}

fn epoch_dir(state: &Path, id: u64) -> PathBuf {
    state.join("epochs").join(format!("{id:08}"))
}

fn epoch_meta_path(state: &Path, id: u64) -> PathBuf {
    epoch_dir(state, id).join("epoch.json")
}

fn journal_path(state: &Path, id: u64) -> PathBuf {
    epoch_dir(state, id).join("journal.log")
}

fn preimage_dir(state: &Path, id: u64) -> PathBuf {
    epoch_dir(state, id).join("preimages")
}

fn create_epoch_files(state: &Path, meta: &EpochMeta) -> Result<()> {
    let directory = epoch_dir(state, meta.id);
    fs::create_dir_all(directory.join("preimages"))?;
    File::create(directory.join("journal.log"))?;
    storage::write_state(&directory.join("epoch.json"), meta)?;
    Ok(())
}

fn rebuild_index(loaded: &journal::LoadedJournal) -> EpochIndex {
    let mut index = EpochIndex::default();
    for record in &loaded.records {
        if record.outcome == Some(ApplyOutcome::Recovered) {
            continue;
        }
        if record.outcome == Some(ApplyOutcome::Applied) {
            index.touched.extend(record.ready.touched.iter().cloned());
        }
        for action in &record.ready.actions {
            match action {
                UndoAction::RestoreRanges {
                    inode,
                    old_size,
                    segments,
                    ..
                } => {
                    let ranges = index.ranges.entry(*inode).or_default();
                    for segment in segments {
                        if let Some(range) = ByteRange::new(
                            segment.offset,
                            segment.offset.saturating_add(segment.length),
                        ) {
                            ranges.insert(range);
                        }
                    }
                    if old_size.is_some() {
                        index.sizes.insert(*inode);
                    }
                    if record.ready.operation == OperationKind::Truncate {
                        index.whole_inodes.insert(*inode);
                    }
                }
                UndoAction::RestoreMode { inode, .. } => {
                    index.mode.insert(*inode);
                }
                UndoAction::RestoreOwner { inode, .. } => {
                    index.owner.insert(*inode);
                }
                UndoAction::RestoreTimes { inode, .. } => {
                    index.times.insert(*inode);
                }
                UndoAction::RestoreXattr { inode, name, .. } => {
                    index.xattrs.insert((*inode, name.clone()));
                }
                UndoAction::RestoreObject { snapshot, .. } => {
                    index.whole_inodes.insert(snapshot.inode);
                }
                UndoAction::RemoveCreated { path } => {
                    index.created.insert(path.clone());
                }
                UndoAction::UndoRename {
                    old_path, new_path, ..
                } => {
                    if index.created.remove(old_path) {
                        index.created.insert(new_path.clone());
                    }
                }
            }
        }
    }
    index
}

fn commit_marker_path(state: &Path, sequence: u64) -> PathBuf {
    state.join("commits").join(format!("{sequence:08}.marker"))
}

fn system_time_ns(time: SystemTime) -> Result<i64> {
    let (negative, magnitude) = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (false, duration.as_nanos()),
        Err(error) => (true, error.duration().as_nanos()),
    };
    let magnitude = i64::try_from(magnitude).map_err(|_| {
        EngineError::Invariant("requested timestamp is outside i64 nanoseconds".into())
    })?;
    Ok(if negative { -magnitude } else { magnitude })
}

fn classify_change(old: &PathState, current: &PathState) -> Option<ChangeKind> {
    match (old, current) {
        (PathState::Absent, PathState::Absent) => None,
        (PathState::Absent, _) => Some(ChangeKind::Added),
        (_, PathState::Absent) => Some(ChangeKind::Deleted),
        (
            PathState::Regular {
                data: old_data,
                metadata: old_metadata,
            },
            PathState::Regular {
                data: current_data,
                metadata: current_metadata,
            },
        ) => {
            if old_data != current_data {
                Some(ChangeKind::Modified)
            } else if old_metadata != current_metadata {
                Some(ChangeKind::Metadata)
            } else {
                None
            }
        }
        (
            PathState::Directory {
                metadata: old_metadata,
            },
            PathState::Directory {
                metadata: current_metadata,
            },
        ) => (old_metadata != current_metadata).then_some(ChangeKind::Metadata),
        (
            PathState::Symlink {
                target: old_target,
                metadata: old_metadata,
            },
            PathState::Symlink {
                target: current_target,
                metadata: current_metadata,
            },
        ) => {
            if old_target != current_target {
                Some(ChangeKind::Modified)
            } else if old_metadata != current_metadata {
                Some(ChangeKind::Metadata)
            } else {
                None
            }
        }
        _ => Some(ChangeKind::Modified),
    }
}

fn diff_content(state: &PathState) -> (Option<u64>, Option<Vec<u8>>) {
    let bytes = match state {
        PathState::Regular { data, .. } => data.as_slice(),
        PathState::Symlink { target, .. } => target.as_bytes(),
        PathState::Absent | PathState::Directory { .. } => return (None, None),
    };
    (
        Some(bytes.len() as u64),
        (bytes.len() <= DIFF_CONTENT_LIMIT).then(|| bytes.to_vec()),
    )
}

fn hardlink_peers(
    root: &Path,
    paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut groups: BTreeMap<BackingIdentity, BTreeSet<String>> = BTreeMap::new();
    for relative in paths {
        let path = checked_path(root, relative)?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        groups
            .entry(BackingIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
            .or_default()
            .insert(relative.clone());
    }
    let mut peers = BTreeMap::new();
    for group in groups.into_values() {
        for relative in &group {
            peers.insert(relative.clone(), group.clone());
        }
    }
    Ok(peers)
}

fn find_backing_alias(
    root: &Path,
    except: &str,
    backing: BackingIdentity,
) -> Result<Option<String>> {
    let mut directories = vec![root.to_path_buf()];
    let mut visited = BTreeSet::new();
    while let Some(directory) = directories.pop() {
        let directory_metadata = fs::symlink_metadata(&directory)?;
        if !visited.insert((directory_metadata.dev(), directory_metadata.ino())) {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let relative = path
                .strip_prefix(root)
                .map_err(|_| EngineError::PathEscape(path.clone()))?
                .to_str()
                .ok_or_else(|| EngineError::InvalidPath(path.clone()))?;
            if relative != except
                && metadata.dev() == backing.device
                && metadata.ino() == backing.inode
            {
                return Ok(Some(relative.to_owned()));
            }
            if metadata.file_type().is_dir() {
                directories.push(path);
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn backing_inode_reuse_gets_a_new_virtual_generation() {
        let root = tempdir().unwrap();
        let live = root.path().join("live");
        let state = root.path().join("state");
        fs::create_dir(&live).unwrap();
        fs::write(live.join("a"), b"A").unwrap();
        let mut engine = StateEngine::create(&live, &state, EngineOptions::default()).unwrap();

        let first = engine.resolve_inode("a").unwrap();
        let backing = engine.inodes.entries[&first].backing;
        engine
            .inodes
            .entries
            .get_mut(&first)
            .unwrap()
            .bindings
            .clear();
        let second = engine.resolve_inode("a").unwrap();

        assert_ne!(first, second);
        assert_eq!(engine.inodes.entries[&second].backing, backing);
        assert_eq!(engine.inodes.entries[&second].generation, 2);
    }
}

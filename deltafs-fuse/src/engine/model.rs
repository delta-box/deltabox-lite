use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::quota::QuotaState;
use super::ranges::RangeSet;

pub const FORMAT_VERSION: u32 = 2;
pub const LEGACY_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VInodeId(pub u64);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackingIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InodeEntry {
    pub id: VInodeId,
    pub backing: BackingIdentity,
    pub generation: u64,
    pub bindings: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InodeRegistry {
    pub entries: BTreeMap<VInodeId, InodeEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    ProcessCrash,
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointMeta {
    pub id: u64,
    pub label: Option<String>,
    pub after_epoch: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionManifest {
    pub session_id: String,
    pub live_root: String,
    pub state_root: String,
    pub durability: Durability,
    #[serde(default)]
    pub group_commit_ms: Option<u64>,
    pub quota_limit: u64,
    pub active_epoch: u64,
    pub accepted_checkpoint: u64,
    pub checkpoints: Vec<CheckpointMeta>,
    pub next_epoch_id: u64,
    pub next_checkpoint_id: u64,
    pub next_record_id: u64,
    pub next_vinode_id: u64,
    pub next_commit_sequence: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpochStatus {
    Open,
    Sealed,
    RollingBack,
    RolledBack,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpochMeta {
    pub id: u64,
    pub base_checkpoint: u64,
    pub end_checkpoint: Option<CheckpointMeta>,
    pub status: EpochStatus,
    pub quota: QuotaState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentRef {
    pub offset: u64,
    pub length: u64,
    pub file: String,
    pub checksum: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetadataSnapshot {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub xattrs: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectSnapshotKind {
    Regular {
        data: String,
        checksum: u32,
        #[serde(default)]
        sparse: Option<SparseLayout>,
    },
    Directory,
    Symlink {
        target: String,
    },
    Hardlink {
        source: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SparseLayout {
    pub size: u64,
    pub extents: Vec<DataExtent>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataExtent {
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectSnapshot {
    pub inode: VInodeId,
    pub kind: ObjectSnapshotKind,
    pub metadata: MetadataSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UndoAction {
    RemoveCreated {
        path: String,
    },
    RestoreObject {
        path: String,
        snapshot: ObjectSnapshot,
    },
    RestoreRanges {
        path: String,
        inode: VInodeId,
        old_size: Option<u64>,
        segments: Vec<SegmentRef>,
        #[serde(default)]
        holes: Vec<DataExtent>,
    },
    UndoRename {
        old_path: String,
        new_path: String,
        source_inode: VInodeId,
        replaced: Option<ObjectSnapshot>,
    },
    UndoExchange {
        first_path: String,
        second_path: String,
        first_backing: BackingIdentity,
        second_backing: BackingIdentity,
    },
    RestoreMode {
        path: String,
        inode: VInodeId,
        mode: u32,
    },
    RestoreOwner {
        path: String,
        inode: VInodeId,
        uid: u32,
        gid: u32,
    },
    RestoreTimes {
        path: String,
        inode: VInodeId,
        atime_ns: i64,
        mtime_ns: i64,
    },
    RestoreXattr {
        path: String,
        inode: VInodeId,
        name: String,
        value: Option<Vec<u8>>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Create,
    CreateDir,
    Symlink,
    Write,
    CopyFileRange,
    Append,
    Truncate,
    Fallocate,
    Unlink,
    Rename,
    RenameExchange,
    Hardlink,
    SetMode,
    SetOwner,
    SetTimes,
    SetXattr,
    RemoveXattr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadyRecord {
    pub operation: OperationKind,
    pub actions: Vec<UndoAction>,
    pub touched: BTreeSet<String>,
    pub preimage_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcome {
    Applied,
    Recovered,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JournalEvent {
    Ready { record: ReadyRecord },
    Applied { outcome: ApplyOutcome },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalBody {
    pub epoch: u64,
    pub sequence: u64,
    pub record_id: u64,
    pub event: JournalEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalEnvelope {
    pub format_version: u32,
    #[serde(default)]
    pub kind: Option<String>,
    pub payload_length: u64,
    pub checksum: u32,
    pub payload: JournalBody,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpochIndex {
    pub created: BTreeSet<String>,
    pub ranges: BTreeMap<VInodeId, RangeSet>,
    pub sizes: BTreeSet<VInodeId>,
    pub whole_inodes: BTreeSet<VInodeId>,
    pub mode: BTreeSet<VInodeId>,
    pub owner: BTreeSet<VInodeId>,
    pub times: BTreeSet<VInodeId>,
    pub xattrs: BTreeSet<(VInodeId, String)>,
    pub touched: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecoveryAction {
    Rollback {
        target_checkpoint: u64,
        #[serde(default)]
        current_epoch: Option<u64>,
        #[serde(default)]
        next_record_id: Option<u64>,
    },
    Commit {
        marker_sequence: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryState {
    pub action: RecoveryAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitMarker {
    pub sequence: u64,
    pub checkpoint: CheckpointMeta,
    pub retired_epochs: Vec<u64>,
}

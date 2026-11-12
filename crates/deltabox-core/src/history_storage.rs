//! Project-scoped history maintenance API. Byte budgets are logical, not an
//! estimate of exclusive APFS extents or the bytes an unlink will reclaim.
use crate::protocol::HistoryVersion;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryStorageUsage {
    /// Regular-file lengths, counted once per directory entry (includes manifests).
    pub apparent_bytes: u64,
    /// st_blocks * 512, counted once per (device, inode). Reflinks may overlap.
    pub allocated_bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryVersionStorage {
    pub version: HistoryVersion,
    pub pinned: bool,
    pub usage: HistoryStorageUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryStorageStats {
    pub versions: Vec<HistoryVersionStorage>,
    pub logical_bytes: u64,
    /// Entire history namespace, including manifests and maintenance files.
    pub total: HistoryStorageUsage,
    pub abandoned_captures: HistoryStorageUsage,
    pub garbage: HistoryStorageUsage,
    pub pending_gc: bool,
    /// False: APFS clone-shared extents cannot be attributed by stat(2).
    pub allocated_bytes_are_exclusive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPinParams {
    pub version_id: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HistoryRetentionPolicy {
    /// Always protect the newest N versions, in addition to explicit pins.
    pub keep_last: usize,
    /// Also protect versions at or newer than this absolute Unix millisecond time.
    pub older_than_ms: Option<u64>,
    /// Remove oldest eligible versions until retained logical bytes fit, or
    /// candidates run out. Without a budget, remove every eligible version.
    pub max_logical_bytes: Option<u64>,
}

impl Default for HistoryRetentionPolicy {
    fn default() -> Self {
        Self {
            keep_last: 1,
            older_than_ms: None,
            max_logical_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPrunePlan {
    /// Digest binds the complete catalog, pins, abandoned captures and policy.
    pub plan_id: String,
    pub policy: HistoryRetentionPolicy,
    pub remove: Vec<HistoryVersion>,
    pub retained: Vec<HistoryVersion>,
    pub pinned: Vec<String>,
    pub abandoned_capture_ids: Vec<String>,
    pub removed_logical_bytes: u64,
    pub retained_logical_bytes: u64,
    pub budget_satisfied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPruneApplyParams {
    pub policy: HistoryRetentionPolicy,
    pub expected_plan_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryGcResult {
    /// IDs in the completed durable intent, including work completed before a crash.
    pub removed_version_ids: Vec<String>,
    pub removed_capture_ids: Vec<String>,
    pub removed_logical_bytes: u64,
    pub resumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryVerifyEntry {
    pub version_id: String,
    pub valid: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryVerifyResult {
    pub versions: Vec<HistoryVerifyEntry>,
    pub valid: bool,
}

use crate::changeset::{describe_change, Change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{ConflictReport, Error, PathConflict, Result};
use crate::test_hooks::maybe_pause;
use crate::macos_tree::{
    clone_path, clone_tree, copy_metadata, link_entry, read_acl, remove_path, replace_path,
    restore_times, sync_dir, sync_tree, tracked_xattr, unlock_entry, unlock_parent,
};
use crate::snapshot::read_capped_file;
use deltafs_fuse::macos::{
    EventBatch, ITEM_CLONED, ITEM_CREATED, ITEM_IS_HARDLINK, ITEM_REMOVED, ITEM_RENAMED,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const FORMAT_VERSION: u32 = 2;
const LEGACY_FORMAT_VERSION: u32 = 1;
const MAX_INDEX_DELTAS: usize = 64;
const MIN_INDEX_DELTA_COMPACTION_ENTRIES: usize = 1024;
const METADATA_READERS: usize = 4;
const MANIFEST_FILE: &str = "state.json";
const TRANSACTION_FILE: &str = "transaction.json";
const BASE_DIR: &str = "base";
const LIVE_DIR: &str = "live";
const CHECKPOINTS_DIR: &str = "checkpoints";
const INDEXES_DIR: &str = "indexes";
const TRANSACTIONS_DIR: &str = "transactions";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode {
    Through,
    Private,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    mode: Mode,
    accepted_id: u64,
    next_id: u64,
    checkpoints: Vec<Checkpoint>,
    pending_paths: BTreeSet<String>,
    epoch_paths: BTreeSet<String>,
    event_id: u64,
    device_uuid: String,
    index_generation: String,
    #[serde(default)]
    index_deltas: Vec<String>,
    durable: bool,
    tainted: bool,
    last_transaction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    id: u64,
    label: Option<String>,
    created_at_ms: u64,
    paths: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TreeIndex {
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TreeIndexDelta {
    entries: BTreeMap<String, Option<Entry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    kind: EntryKind,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_ns: i64,
    ctime_ns: i64,
    device: u64,
    inode: u64,
    links: u64,
    flags: u32,
    acl: Vec<u8>,
    target: Option<Vec<u8>>,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotManifest {
    entries: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotEntry {
    path: String,
    present: bool,
    kind: Option<EntryKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Transaction {
    id: String,
    mode: Mode,
    paths: BTreeSet<String>,
}

pub(crate) struct State {
    project: PathBuf,
    root: PathBuf,
    manifest: Manifest,
    index: Option<TreeIndex>,
}

pub(crate) struct Status {
    pub mode: Mode,
    pub checkpoints: usize,
    pub pending: usize,
    pub event_id: u64,
    pub device_uuid: String,
    pub tainted: bool,
}

impl State {
    pub fn create(
        project: &Path,
        root: &Path,
        mode: Mode,
        durable: bool,
        event_id: u64,
        device_uuid: String,
    ) -> Result<Self> {
        fs::create_dir_all(root)?;
        fs::create_dir_all(root.join(CHECKPOINTS_DIR))?;
        fs::create_dir_all(root.join(INDEXES_DIR))?;
        fs::create_dir_all(root.join(TRANSACTIONS_DIR))?;
        let scanned = materialize(project, &root.join(BASE_DIR), durable)
            .map_err(|error| Error::Backend(format!("create accepted APFS base: {error}")))?;
        maybe_pause("macos_base_materialized");
        let index = match mode {
            // The through-mode live tree is the project itself, which
            // materializing the base has just scanned.
            Mode::Through => scanned,
            Mode::Private => {
                materialize(project, &root.join(LIVE_DIR), durable).map_err(|error| {
                    Error::Backend(format!("create private APFS live tree: {error}"))
                })?;
                TreeIndex::scan(&root.join(LIVE_DIR))
                    .map_err(|error| Error::Backend(format!("index initial live tree: {error}")))?
            }
        };
        let manifest = Manifest {
            version: FORMAT_VERSION,
            mode,
            accepted_id: 0,
            next_id: 1,
            checkpoints: Vec::new(),
            pending_paths: BTreeSet::new(),
            epoch_paths: BTreeSet::new(),
            event_id,
            device_uuid,
            index_generation: String::new(),
            index_deltas: Vec::new(),
            durable,
            tainted: false,
            last_transaction: None,
        };
        let mut state = Self {
            project: project.to_path_buf(),
            root: root.to_path_buf(),
            manifest,
            index: Some(index),
        };
        state
            .save_with_index()
            .map_err(|error| Error::Backend(format!("save initial macOS state: {error}")))?;
        Ok(state)
    }

    pub fn open(project: &Path, root: &Path) -> Result<Self> {
        recover_transaction(project, root)?;
        let mut manifest: Manifest = read_json(&root.join(MANIFEST_FILE))?;
        if manifest.version != LEGACY_FORMAT_VERSION && manifest.version != FORMAT_VERSION {
            return Err(Error::Backend(format!(
                "unsupported macOS state format: {}",
                manifest.version
            )));
        }
        let migrated = manifest.version == LEGACY_FORMAT_VERSION;
        manifest.version = FORMAT_VERSION;
        let state = Self {
            project: project.to_path_buf(),
            root: root.to_path_buf(),
            manifest,
            index: None,
        };
        if migrated {
            state.save_manifest()?;
        }
        state.cleanup_orphans()?;
        Ok(state)
    }

    pub fn pending_set(&self) -> std::collections::BTreeSet<String> {
        self.manifest.pending_paths.clone()
    }

    pub fn live(&self) -> PathBuf {
        match self.manifest.mode {
            Mode::Through => self.project.clone(),
            Mode::Private => self.root.join(LIVE_DIR),
        }
    }

    pub fn event_socket(&self) -> PathBuf {
        let session_id = self
            .root
            .parent()
            .and_then(Path::file_name)
            .expect("macOS state root has a session directory");
        Path::new("/private/tmp").join(format!("deltafs-{}.sock", session_id.to_string_lossy()))
    }

    pub fn status(&self) -> Status {
        Status {
            mode: self.manifest.mode,
            checkpoints: self.manifest.checkpoints.len(),
            pending: self.manifest.pending_paths.len(),
            event_id: self.manifest.event_id,
            device_uuid: self.manifest.device_uuid.clone(),
            tainted: self.manifest.tainted,
        }
    }

    pub fn durable(&self) -> bool {
        self.manifest.durable
    }

    pub fn set_event_boundary(&mut self, event_id: u64, device_uuid: String) -> Result<()> {
        if self.manifest.event_id == event_id && self.manifest.device_uuid == device_uuid {
            return Ok(());
        }
        self.manifest.event_id = event_id;
        self.manifest.device_uuid = device_uuid;
        self.save_manifest()
    }

    pub fn reconcile(
        &mut self,
        batch: &EventBatch,
        historical: bool,
        device_uuid: String,
    ) -> Result<usize> {
        let previous_event_id = self.manifest.event_id;
        let previous_device_uuid = self.manifest.device_uuid.clone();
        let was_tainted = self.manifest.tainted;
        let live = self.live();
        let has_project_events = batch.events.iter().any(|event| {
            event
                .path
                .strip_prefix(&live)
                .is_ok_and(|path| !path.as_os_str().is_empty() && !excluded(path))
        });
        if !batch.must_scan && !batch.root_changed && !has_project_events {
            self.manifest.event_id = self.manifest.event_id.max(batch.latest_id);
            self.manifest.device_uuid = device_uuid;
            if self.manifest.event_id != previous_event_id
                || self.manifest.device_uuid != previous_device_uuid
            {
                self.save_manifest()?;
            }
            return Ok(0);
        }
        self.load_index()?;
        let replace_index = batch.must_scan || batch.root_changed;
        let mut paths = BTreeSet::new();
        if replace_index {
            let next = TreeIndex::scan(&self.live())?;
            paths.extend(changed_index_paths(
                self.index.as_ref().expect("index loaded"),
                &next,
            ));
            self.index = Some(next);
        } else {
            {
                let before = self.index.as_ref().expect("index loaded");
                let mut parents = BTreeSet::new();
                for event in &batch.events {
                    let Ok(relative) = event.path.strip_prefix(&live) else {
                        continue;
                    };
                    if relative.as_os_str().is_empty() || excluded(relative) {
                        continue;
                    }
                    let path = relative_string(relative)?;
                    if event.flags & ITEM_CLONED != 0 && indexed_path_matches(before, &live, &path)?
                    {
                        continue;
                    }
                    paths.insert(path.clone());
                    if event.flags & (ITEM_CREATED | ITEM_REMOVED | ITEM_RENAMED | ITEM_IS_HARDLINK)
                        != 0
                    {
                        parents.insert(parent_string(&path)?);
                    }
                    if event.flags & ITEM_IS_HARDLINK != 0 {
                        for peer in before.hardlink_peers(&path) {
                            paths.insert(peer.clone());
                            parents.insert(parent_string(&peer)?);
                        }
                    }
                }
                for parent in parents {
                    paths.extend(before.children(&parent));
                    paths.extend(current_children(&live, &parent)?);
                }
                expand_subtrees(before, &live, &mut paths)?;
            }

            let before_entries = paths
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        self.index
                            .as_ref()
                            .expect("index loaded")
                            .entries
                            .get(path)
                            .cloned(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            self.index
                .as_mut()
                .expect("index loaded")
                .refresh(&live, &paths)?;

            // FSEvents paths are candidates, not proof that the current tree still
            // differs. In particular, a long-lived stream can deliver events from
            // an abort/commit after that manager operation has refreshed the
            // persisted index. Drop those stale notifications, along with
            // unchanged siblings introduced by parent expansion.
            let after = self.index.as_ref().expect("index loaded");
            paths.retain(|path| {
                before_entries.get(path).and_then(Option::as_ref) != after.entries.get(path)
            });
        }

        if historical && self.manifest.mode == Mode::Through && !paths.is_empty() {
            self.manifest.tainted = true;
        }
        self.manifest.pending_paths.extend(paths.iter().cloned());
        self.manifest.epoch_paths.extend(paths.iter().cloned());
        self.manifest.event_id = self.manifest.event_id.max(batch.latest_id);
        self.manifest.device_uuid = device_uuid;
        if paths.is_empty()
            && (self.manifest.event_id != previous_event_id
                || self.manifest.device_uuid != previous_device_uuid
                || self.manifest.tainted != was_tainted)
        {
            self.save_manifest()?;
        } else if !paths.is_empty() && replace_index {
            self.save_with_index()?;
        } else if !paths.is_empty() {
            self.save_index_delta(&paths)?;
        }
        Ok(paths.len())
    }

    /// Record exact paths changed by a trusted embedded writer without waiting
    /// for the asynchronous FSEvents stream to rediscover them.
    pub(crate) fn mark_paths(&mut self, paths: impl IntoIterator<Item = String>) -> Result<()> {
        let mut paths: BTreeSet<String> = paths.into_iter().collect();
        let live = self.live();
        self.load_index()?;
        expand_subtrees(
            self.index.as_ref().expect("index loaded"),
            &live,
            &mut paths,
        )?;
        self.index
            .as_mut()
            .expect("index loaded")
            .refresh(&live, &paths)?;
        self.manifest.pending_paths.extend(paths.iter().cloned());
        self.manifest.epoch_paths.extend(paths.iter().cloned());
        self.save_index_delta(&paths)
    }

    pub fn checkpoint(&mut self, label: Option<String>) -> Result<(u64, usize)> {
        let id = self.manifest.next_id;
        self.manifest.next_id += 1;
        let paths = self.manifest.epoch_paths.clone();
        let temporary = self.root.join(CHECKPOINTS_DIR).join(format!(".{id}.tmp"));
        let destination = self.root.join(CHECKPOINTS_DIR).join(id.to_string());
        remove_path(&temporary)?;
        capture_snapshot(&self.live(), &temporary, &paths, self.manifest.durable)?;
        fs::rename(&temporary, &destination)?;
        if self.manifest.durable {
            sync_dir(&self.root.join(CHECKPOINTS_DIR))?;
        }
        self.manifest.checkpoints.push(Checkpoint {
            id,
            label,
            created_at_ms: crate::session::now_ms(),
            paths: paths.clone(),
        });
        self.manifest.epoch_paths.clear();
        self.save_manifest()?;
        Ok((id, paths.len()))
    }

    pub fn diff(&self, from: Option<u64>, to: Option<u64>) -> Result<Vec<Change>> {
        let from = from.unwrap_or(self.manifest.accepted_id);
        self.require_checkpoint(from)?;
        if let Some(to) = to {
            self.require_checkpoint(to)?;
        }
        let paths = self.diff_paths(from, to);
        let mut changes = Vec::new();
        for path in paths {
            let old = self.resolve(from, &path)?;
            let new = match to {
                Some(id) => self.resolve(id, &path)?,
                None => existing(self.live().join(&path)),
            };
            if paths_equal(old.as_deref(), new.as_deref())? {
                continue;
            }
            let old_entry = old.as_deref().map(Entry::read).transpose()?;
            let new_entry = new.as_deref().map(Entry::read).transpose()?;
            let kind = match (&old_entry, &new_entry) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            let old_bytes = old.as_deref().map(read_capped_file).transpose()?.flatten();
            let new_bytes = new.as_deref().map(read_capped_file).transpose()?.flatten();
            changes.push(describe_change(
                &path,
                kind,
                old_entry.as_ref().map(|entry| entry.size),
                new_entry.as_ref().map(|entry| entry.size),
                old_bytes.as_deref(),
                new_bytes.as_deref(),
                DEFAULT_TEXT_SIZE_LIMIT,
            ));
        }
        Ok(changes)
    }

    pub fn abort(&mut self, target: Option<u64>) -> Result<(u64, usize)> {
        if self.manifest.tainted && self.manifest.mode == Mode::Through {
            return Err(Error::Backend(
                "through-mode session was modified outside a command; abort is refused".into(),
            ));
        }
        let target = target.unwrap_or(self.manifest.accepted_id);
        self.require_checkpoint(target)?;
        let paths = self.paths_after(target);
        let restore = self.root.join("restore");
        remove_path(&restore)?;
        capture_resolved(self, target, &restore, &paths)?;
        apply_snapshot(&restore, &self.live(), self.manifest.durable)?;
        remove_path(&restore)?;
        self.manifest
            .checkpoints
            .retain(|checkpoint| checkpoint.id <= target);
        self.manifest.pending_paths = self
            .manifest
            .checkpoints
            .iter()
            .flat_map(|checkpoint| checkpoint.paths.iter().cloned())
            .collect();
        self.manifest.epoch_paths.clear();
        let live = self.live();
        self.load_index()?;
        self.index
            .as_mut()
            .expect("index loaded")
            .refresh(&live, &paths)?;
        self.save_index_delta(&paths)?;
        self.cleanup_orphans()?;
        Ok((target, paths.len()))
    }

    pub fn commit(
        &mut self,
        target: Option<u64>,
        endorsed_until: u64,
    ) -> Result<(u64, Vec<crate::publog::PublogChange>)> {
        if let Some(target) = target {
            self.abort(Some(target))?;
        }
        // `.deltabox-theirs` drafts that `sync` materialized stay in the live
        // view for the agent to merge against, but never cross into the host or
        // the accepted baseline: dropping them from the publish set here excludes
        // them from staging, both applies (host and base), and the publog at once.
        // `pending_paths` itself is cleared below, so they simply stop being
        // pending — they are not deleted from the working tree.
        let paths: BTreeSet<String> = self
            .manifest
            .pending_paths
            .iter()
            .filter(|path| crate::publog::is_publishable(path.as_str()))
            .cloned()
            .collect();
        if self.manifest.mode == Mode::Private {
            validate_paths(
                &self.root.join(BASE_DIR),
                &self.project,
                &paths,
                endorsed_until,
            )?;
        }
        // Describe what this commit publishes while the trees are still
        // pre-apply: staged-vs-base presence yields the change kind, and the
        // pre-apply host (base for through mode, whose host already carries
        // the live view) yields `existed`. Sampling after apply would misread
        // additions as modifications and cannot see checkpoint-target aborts.
        let live_root = self.live();
        let base_root = self.root.join(BASE_DIR);
        let mut published = Vec::new();
        for path in &paths {
            let staged = fs::symlink_metadata(live_root.join(path)).ok();
            let base = fs::symlink_metadata(base_root.join(path)).ok();
            let host = if self.manifest.mode == Mode::Private {
                fs::symlink_metadata(self.project.join(path)).ok()
            } else {
                base.clone()
            };
            let kind = match (&staged, &base) {
                (Some(_), None) => crate::changeset::ChangeKind::Added,
                (Some(_), Some(_)) => crate::changeset::ChangeKind::Modified,
                (None, _) => crate::changeset::ChangeKind::Deleted,
            };
            let namespace_changed = matches!(
                (&staged, &base),
                (Some(staged), Some(base)) if staged.file_type() != base.file_type()
            );
            published.push(crate::publog::PublogChange {
                path: path.clone(),
                kind,
                existed: host.is_some(),
                namespace_changed,
            });
        }
        let id = Uuid::new_v4().to_string();
        let transaction_root = self.root.join(TRANSACTIONS_DIR).join(&id);
        fs::create_dir_all(&transaction_root)?;
        capture_snapshot(
            &self.live(),
            &transaction_root.join("staged"),
            &paths,
            self.manifest.durable,
        )?;
        capture_snapshot(
            &self.root.join(BASE_DIR),
            &transaction_root.join("base-backup"),
            &paths,
            self.manifest.durable,
        )?;
        if self.manifest.mode == Mode::Private {
            capture_snapshot(
                &self.project,
                &transaction_root.join("host-backup"),
                &paths,
                self.manifest.durable,
            )?;
        }
        save_json(
            &self.root.join(TRANSACTION_FILE),
            &Transaction {
                id: id.clone(),
                mode: self.manifest.mode,
                paths: paths.clone(),
            },
            self.manifest.durable,
        )?;
        maybe_crash("macos_commit_prepared");
        if self.manifest.mode == Mode::Private {
            apply_snapshot(
                &transaction_root.join("staged"),
                &self.project,
                self.manifest.durable,
            )?;
        }
        maybe_pause("publish_host_partially_applied");
        apply_snapshot(
            &transaction_root.join("staged"),
            &self.root.join(BASE_DIR),
            self.manifest.durable,
        )?;
        maybe_crash("macos_commit_applied");

        let checkpoint_id = self.manifest.next_id;
        self.manifest.next_id += 1;
        self.manifest.accepted_id = checkpoint_id;
        self.manifest.checkpoints.clear();
        self.manifest.pending_paths.clear();
        self.manifest.epoch_paths.clear();
        self.manifest.tainted = false;
        self.manifest.last_transaction = Some(id.clone());
        // Commit changes the accepted/base trees, but the live tree and its
        // already-persisted index are unchanged.
        self.save_manifest()?;
        maybe_crash("macos_commit_state_updated");
        remove_path(&self.root.join(TRANSACTION_FILE))?;
        remove_path(&transaction_root)?;
        self.cleanup_orphans()?;
        Ok((checkpoint_id, published))
    }

    fn require_checkpoint(&self, id: u64) -> Result<()> {
        if id == self.manifest.accepted_id
            || self
                .manifest
                .checkpoints
                .iter()
                .any(|checkpoint| checkpoint.id == id)
        {
            Ok(())
        } else {
            Err(Error::CheckpointNotFound(id.to_string()))
        }
    }

    fn resolve(&self, checkpoint_id: u64, path: &str) -> Result<Option<PathBuf>> {
        if checkpoint_id == self.manifest.accepted_id {
            return Ok(existing(self.root.join(BASE_DIR).join(path)));
        }
        for checkpoint in self.manifest.checkpoints.iter().rev() {
            if checkpoint.id > checkpoint_id || !checkpoint.paths.contains(path) {
                continue;
            }
            let snapshot = self
                .root
                .join(CHECKPOINTS_DIR)
                .join(checkpoint.id.to_string());
            let manifest: SnapshotManifest = read_json(&snapshot.join("snapshot.json"))?;
            let entry = manifest
                .entries
                .into_iter()
                .find(|entry| entry.path == path)
                .ok_or_else(|| Error::Backend(format!("checkpoint entry is missing: {path}")))?;
            return Ok(entry.present.then(|| snapshot.join("objects").join(path)));
        }
        Ok(existing(self.root.join(BASE_DIR).join(path)))
    }

    fn diff_paths(&self, from: u64, to: Option<u64>) -> BTreeSet<String> {
        let mut paths = self
            .manifest
            .checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.id > from && to.is_none_or(|to| checkpoint.id <= to))
            .flat_map(|checkpoint| checkpoint.paths.iter().cloned())
            .collect::<BTreeSet<_>>();
        if to.is_none() {
            paths.extend(self.manifest.epoch_paths.iter().cloned());
        }
        paths
    }

    fn paths_after(&self, target: u64) -> BTreeSet<String> {
        self.manifest
            .checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.id > target)
            .flat_map(|checkpoint| checkpoint.paths.iter().cloned())
            .chain(self.manifest.epoch_paths.iter().cloned())
            .collect()
    }

    fn save_with_index(&mut self) -> Result<()> {
        let Some(index) = &self.index else {
            return self.save_manifest();
        };
        let old = self.manifest.index_generation.clone();
        let old_deltas = self.manifest.index_deltas.clone();
        let generation = Uuid::new_v4().to_string();
        save_binary(
            &self
                .root
                .join(INDEXES_DIR)
                .join(format!("{generation}.bin")),
            index,
            self.manifest.durable,
        )?;
        self.manifest.version = FORMAT_VERSION;
        self.manifest.index_generation = generation;
        self.manifest.index_deltas.clear();
        self.save_manifest()?;
        if !old.is_empty() {
            remove_path(&self.root.join(INDEXES_DIR).join(format!("{old}.bin")))?;
        }
        for delta in old_deltas {
            remove_path(&self.root.join(INDEXES_DIR).join(format!("{delta}.delta")))?;
        }
        Ok(())
    }

    fn save_index_delta(&mut self, paths: &BTreeSet<String>) -> Result<()> {
        if paths.is_empty() {
            return self.save_manifest();
        }
        let Some(index) = &self.index else {
            return self.save_manifest();
        };
        let large_delta = paths.len() >= MIN_INDEX_DELTA_COMPACTION_ENTRIES
            && paths.len().saturating_mul(2) >= index.entries.len();
        if self.manifest.index_deltas.len() >= MAX_INDEX_DELTAS || large_delta {
            return self.save_with_index();
        }

        let delta = TreeIndexDelta {
            entries: paths
                .iter()
                .map(|path| (path.clone(), index.entries.get(path).cloned()))
                .collect(),
        };
        let generation = Uuid::new_v4().to_string();
        save_binary(
            &self
                .root
                .join(INDEXES_DIR)
                .join(format!("{generation}.delta")),
            &delta,
            self.manifest.durable,
        )?;
        self.manifest.version = FORMAT_VERSION;
        self.manifest.index_deltas.push(generation);
        self.save_manifest()
    }

    fn load_index(&mut self) -> Result<()> {
        if self.index.is_none() {
            let mut index: TreeIndex = read_binary(
                &self
                    .root
                    .join(INDEXES_DIR)
                    .join(format!("{}.bin", self.manifest.index_generation)),
            )?;
            for generation in &self.manifest.index_deltas {
                let delta: TreeIndexDelta = read_binary(
                    &self
                        .root
                        .join(INDEXES_DIR)
                        .join(format!("{generation}.delta")),
                )?;
                index.apply(delta);
            }
            self.index = Some(index);
        }
        Ok(())
    }

    fn save_manifest(&self) -> Result<()> {
        save_json(
            &self.root.join(MANIFEST_FILE),
            &self.manifest,
            self.manifest.durable,
        )
    }

    fn cleanup_orphans(&self) -> Result<()> {
        let checkpoints: BTreeSet<String> = self
            .manifest
            .checkpoints
            .iter()
            .map(|checkpoint| checkpoint.id.to_string())
            .collect();
        cleanup_children(&self.root.join(CHECKPOINTS_DIR), &checkpoints)?;
        let mut indexes = BTreeSet::from([format!("{}.bin", self.manifest.index_generation)]);
        indexes.extend(
            self.manifest
                .index_deltas
                .iter()
                .map(|generation| format!("{generation}.delta")),
        );
        cleanup_children(&self.root.join(INDEXES_DIR), &indexes)?;
        let active = fs::read(self.root.join(TRANSACTION_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Transaction>(&bytes).ok())
            .map(|transaction| transaction.id);
        cleanup_children(
            &self.root.join(TRANSACTIONS_DIR),
            &active.into_iter().collect(),
        )
    }
}

impl TreeIndex {
    fn scan(root: &Path) -> Result<Self> {
        let paths = walk(root)?;
        let entries = read_in_parallel(&paths, |path| Entry::read(path))?;
        let mut index = Self::default();
        for (path, entry) in paths.iter().zip(entries) {
            let relative =
                relative_string(path.strip_prefix(root).expect("walk stays below root"))?;
            index.entries.insert(relative, entry);
        }
        Ok(index)
    }

    fn apply(&mut self, delta: TreeIndexDelta) {
        for (path, entry) in delta.entries {
            match entry {
                Some(entry) => {
                    self.entries.insert(path, entry);
                }
                None => {
                    self.entries.remove(&path);
                }
            }
        }
    }

    fn refresh(&mut self, root: &Path, paths: &BTreeSet<String>) -> Result<()> {
        // Callers expand directory roots to their old and current descendants
        // before refreshing. Updating exact keys avoids K full-map retain
        // passes and repeated subtree walks for a K-path event batch.
        for path in paths {
            self.entries.remove(path);
            let absolute = root.join(path);
            match fs::symlink_metadata(&absolute) {
                Ok(_) => {
                    self.entries.insert(path.clone(), Entry::read(&absolute)?);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn children(&self, parent: &str) -> BTreeSet<String> {
        let prefix = if parent.is_empty() {
            String::new()
        } else {
            format!("{parent}/")
        };
        self.entries
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .filter_map(|(path, _)| {
                let relative = path.strip_prefix(&prefix).expect("range shares prefix");
                (!relative.contains('/')).then(|| path.clone())
            })
            .collect()
    }

    fn descendants(&self, parent: &str) -> Vec<String> {
        let prefix = format!("{parent}/");
        self.entries
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn hardlink_peers(&self, path: &str) -> Vec<String> {
        let Some(entry) = self.entries.get(path).filter(|entry| entry.links > 1) else {
            return Vec::new();
        };
        self.entries
            .iter()
            .filter(|(_, candidate)| {
                candidate.kind == EntryKind::File
                    && candidate.device == entry.device
                    && candidate.inode == entry.inode
            })
            .map(|(path, _)| path.clone())
            .collect()
    }
}

impl Entry {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        let kind = if metadata.is_file() {
            EntryKind::File
        } else if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.file_type().is_symlink() {
            EntryKind::Symlink
        } else {
            return Err(Error::Backend(format!(
                "unsupported project entry: {}",
                path.display()
            )));
        };
        let mut xattrs = Vec::new();
        for name in xattr::list(path)?.filter(|name| tracked_xattr(name)) {
            xattrs.push((
                name.as_bytes().to_vec(),
                xattr::get(path, &name)?.unwrap_or_default(),
            ));
        }
        xattrs.sort();
        Ok(Self {
            kind,
            size: metadata.len(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime_ns: timestamp_ns(metadata.mtime(), metadata.mtime_nsec()),
            ctime_ns: timestamp_ns(metadata.ctime(), metadata.ctime_nsec()),
            device: metadata.dev(),
            inode: metadata.ino(),
            links: metadata.nlink(),
            flags: metadata.st_flags(),
            acl: read_acl(path)?,
            target: (kind == EntryKind::Symlink)
                .then(|| fs::read_link(path).map(|path| path.as_os_str().as_bytes().to_vec()))
                .transpose()?,
            xattrs,
        })
    }

    fn semantic_eq(&self, other: &Self) -> bool {
        self.mtime_ns == other.mtime_ns && self.same_attributes(other)
    }

    /// `semantic_eq` without the modification time.
    fn same_attributes(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.size == other.size
            && self.mode == other.mode
            && self.uid == other.uid
            && self.gid == other.gid
            && self.flags == other.flags
            && self.acl == other.acl
            && self.target == other.target
            && self.xattrs == other.xattrs
    }

    /// Whether both entries hold the same data, so that only metadata can
    /// differ. A clone shares the data blocks of its source, so a file whose
    /// size and modification time still match was not rewritten since.
    fn same_content(&self, other: &Self) -> bool {
        self.kind == other.kind
            && match self.kind {
                EntryKind::File => self.size == other.size && self.mtime_ns == other.mtime_ns,
                EntryKind::Symlink => self.target == other.target,
                EntryKind::Directory => true,
            }
    }

    /// Whether a file was left alone since `since_ns`. Size and modification
    /// time can be restored by a writer; the change time cannot, so a file
    /// changed after a clone started is one whose clone must not be trusted.
    fn untouched_since(&self, since_ns: i64) -> bool {
        self.kind != EntryKind::File || self.ctime_ns < since_ns
    }
}

fn capture_snapshot(
    source: &Path,
    destination: &Path,
    paths: &BTreeSet<String>,
    durable: bool,
) -> Result<()> {
    remove_path(destination)?;
    fs::create_dir_all(destination.join("objects"))?;
    let mut entries = Vec::new();
    let mut links = HashMap::new();
    let mut directories = Vec::new();
    for path in paths {
        let source_path = source.join(path);
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entries.push(SnapshotEntry {
                    path: path.clone(),
                    present: false,
                    kind: None,
                });
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let kind = entry_kind(&metadata)?;
        let target = destination.join("objects").join(path);
        if kind == EntryKind::Directory {
            fs::create_dir_all(&target)?;
            directories.push((source_path, target));
        } else {
            clone_path(&source_path, &target, &mut links)?;
        }
        entries.push(SnapshotEntry {
            path: path.clone(),
            present: true,
            kind: Some(kind),
        });
    }
    for (source, target) in directories.into_iter().rev() {
        copy_metadata(&source, &target)?;
    }
    save_json(
        &destination.join("snapshot.json"),
        &SnapshotManifest { entries },
        durable,
    )?;
    if durable {
        sync_tree(destination)?;
    }
    Ok(())
}

fn capture_resolved(
    state: &State,
    checkpoint: u64,
    destination: &Path,
    paths: &BTreeSet<String>,
) -> Result<()> {
    remove_path(destination)?;
    fs::create_dir_all(destination.join("objects"))?;
    let mut entries = Vec::new();
    let mut links = HashMap::new();
    let mut directories = Vec::new();
    for path in paths {
        let Some(source) = state.resolve(checkpoint, path)? else {
            entries.push(SnapshotEntry {
                path: path.clone(),
                present: false,
                kind: None,
            });
            continue;
        };
        let metadata = fs::symlink_metadata(&source)?;
        let kind = entry_kind(&metadata)?;
        let target = destination.join("objects").join(path);
        if kind == EntryKind::Directory {
            fs::create_dir_all(&target)?;
            directories.push((source, target));
        } else {
            clone_path(&source, &target, &mut links)?;
        }
        entries.push(SnapshotEntry {
            path: path.clone(),
            present: true,
            kind: Some(kind),
        });
    }
    for (source, target) in directories.into_iter().rev() {
        copy_metadata(&source, &target)?;
    }
    save_json(
        &destination.join("snapshot.json"),
        &SnapshotManifest { entries },
        state.manifest.durable,
    )
}

fn apply_snapshot(snapshot: &Path, target: &Path, durable: bool) -> Result<()> {
    let manifest: SnapshotManifest = read_json(&snapshot.join("snapshot.json"))?;
    let mut entries = manifest.entries;
    entries.sort_by_key(|entry| depth(&entry.path));
    for entry in entries.iter().rev() {
        let destination = target.join(&entry.path);
        if !entry.present {
            remove_path(&destination)?;
            continue;
        }
        let current_kind = fs::symlink_metadata(&destination)
            .ok()
            .and_then(|metadata| entry_kind(&metadata).ok());
        if current_kind != entry.kind {
            remove_path(&destination)?;
        }
    }
    let mut links = HashMap::new();
    let mut directories = Vec::new();
    for entry in &entries {
        if !entry.present {
            continue;
        }
        let source = snapshot.join("objects").join(&entry.path);
        let destination = target.join(&entry.path);
        if entry.kind == Some(EntryKind::Directory) {
            fs::create_dir_all(&destination)?;
            directories.push((source, destination));
        } else {
            replace_path(&source, &destination, &mut links)?;
        }
    }
    for (source, destination) in directories.into_iter().rev() {
        copy_metadata(&source, &destination)?;
    }
    if durable {
        sync_tree(target)?;
    }
    Ok(())
}

fn validate_paths(
    base: &Path,
    host: &Path,
    paths: &BTreeSet<String>,
    endorsed_until: u64,
) -> Result<()> {
    // Digests of publications this session has ACKNOWLEDGED (its publog
    // cursor covers them, i.e. it synced or started after they landed). A
    // mismatching host file whose content is such a publication is accepted
    // baseline movement. Publications past the cursor stay conflicts: the
    // session has not seen them, so first-committer-wins must hold. Anything
    // without a digest match (dirs, symlinks, strangers) fails closed.
    let mut endorsed = std::collections::BTreeMap::new();
    for entry in crate::publog::entries_since(host, 0)? {
        if entry.seq > endorsed_until {
            continue;
        }
        for (path, digest) in entry.digests {
            endorsed.insert(path, digest);
        }
    }
    let mut conflicts = Vec::new();
    for path in paths {
        let base_entry = existing(base.join(path));
        let host_entry = existing(host.join(path));
        if paths_equal(base_entry.as_deref(), host_entry.as_deref())? {
            continue;
        }
        let host_path = host.join(path);
        if host_path.is_file() {
            if let Some(expected) = endorsed.get(path) {
                if crate::publog::file_digest(&host_path)? == *expected {
                    continue;
                }
            }
        }
        {
            let host_change = match (&base_entry, &host_entry) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(_), None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            conflicts.push(PathConflict {
                path: path.clone(),
                host_change,
            });
        }
    }
    if conflicts.is_empty() {
        return Ok(());
    }
    let message = format!(
        "host path changed after session start: {}",
        conflicts[0].path
    );
    Err(Error::ProjectConflict(ConflictReport::new(message, conflicts)))
}

fn paths_equal(left: Option<&Path>, right: Option<&Path>) -> Result<bool> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(left.is_none() && right.is_none());
    };
    let left_entry = Entry::read(left)?;
    let right_entry = Entry::read(right)?;
    if !left_entry.semantic_eq(&right_entry) {
        return Ok(false);
    }
    if left_entry.kind != EntryKind::File {
        return Ok(true);
    }
    files_equal(left, right)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let mut left = File::open(left)?;
    let mut right = File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_count = left.read(&mut left_buffer)?;
        let right_count = right.read(&mut right_buffer)?;
        if left_count != right_count || left_buffer[..left_count] != right_buffer[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn recover_transaction(project: &Path, root: &Path) -> Result<()> {
    let path = root.join(TRANSACTION_FILE);
    if !path.exists() {
        return Ok(());
    }
    let transaction: Transaction = read_json(&path)?;
    let state: Manifest = read_json(&root.join(MANIFEST_FILE))?;
    let transaction_root = root.join(TRANSACTIONS_DIR).join(&transaction.id);
    if state.last_transaction.as_deref() != Some(&transaction.id) {
        apply_snapshot(
            &transaction_root.join("base-backup"),
            &root.join(BASE_DIR),
            state.durable,
        )?;
        if transaction.mode == Mode::Private {
            apply_snapshot(
                &transaction_root.join("host-backup"),
                project,
                state.durable,
            )?;
        }
    }
    remove_path(&path)?;
    remove_path(&transaction_root)
}

/// Copy `source` into `destination` exactly, and return the scan of `source`
/// the copy was checked against.
fn materialize(source: &Path, destination: &Path, durable: bool) -> Result<TreeIndex> {
    remove_path(destination)?;
    fs::create_dir_all(destination)?;
    // Cloning entry by entry and replaying every attribute costs hundreds of
    // microseconds per entry, which made the first command in a project with
    // node_modules or target wait tens of seconds. One clonefile(2) per
    // top-level entry copies whole subtrees in the kernel instead. That copy
    // is only approximate (see `clone_tree`), so the scan taken after it is
    // the reference every entry is checked and repaired against. A failed
    // clone only leaves more entries to repair.
    let cloned_since = now_ns();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() != ".deltabox" {
            let _ = clone_tree(&entry.path(), &destination.join(entry.file_name()));
        }
    }
    let index = mirror(source, destination, cloned_since)?;
    if durable {
        sync_tree(destination)?;
    }
    Ok(index)
}

/// Repair `destination`, a copy of `source` whose clones were taken no
/// earlier than `cloned_since`, until it matches a scan of `source` exactly,
/// and return that scan.
fn mirror(source: &Path, destination: &Path, cloned_since: i64) -> Result<TreeIndex> {
    let index = TreeIndex::scan(source)?;
    // Where the scan has a directory and the copy has something else, put a
    // directory there first, parents before children: the source replaced a
    // file or symlink with a directory after the clone, or the clone failed.
    // Every read and repair below a directory then stays inside the copy
    // instead of following a stale symlink out of it.
    for (path, expected) in &index.entries {
        if expected.kind != EntryKind::Directory {
            continue;
        }
        let target = destination.join(path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => {
                unlock_parent(&target)?;
                unlock_entry(&target)?;
                remove_path(&target)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => unlock_parent(&target)?,
            Err(error) => return Err(error.into()),
        }
        fs::create_dir(&target)?;
    }
    // Checking the clone only reads it, and every read starts with a cold
    // lookup of an inode the clone just created, so it runs on a few threads.
    // The rare repairs then run in order.
    let paths = index.entries.keys().collect::<Vec<_>>();
    let cloned = read_in_parallel(&paths, |path| {
        let target = destination.join(path);
        let current = read_existing(&target)?;
        let extras = match &current {
            Some(current) if current.kind == EntryKind::Directory => {
                extra_entries(&target, path, &index)?
            }
            _ => Vec::new(),
        };
        Ok((current, extras))
    })?;
    let mut links = HashMap::new();
    // A path sorts after its parent, so walking the scan backwards settles
    // everything inside a directory before the directory itself, and the
    // directory times restored there stay final.
    for ((path, expected), (current, extras)) in index.entries.iter().zip(cloned).rev() {
        let target = destination.join(path);
        settle_extras(destination, path, extras, &index)?;
        let current = if expected.kind == EntryKind::Directory {
            // Settling the children changed the directory, or created it.
            read_existing(&target)?
        } else {
            current
        };
        mirror_entry(
            &source.join(path),
            &target,
            expected,
            current,
            cloned_since,
            &mut links,
        )?;
    }
    let extras = extra_entries(destination, "", &index)?;
    settle_extras(destination, "", extras, &index)?;
    copy_metadata(source, destination)?;
    Ok(index)
}

/// Repair `target`, a clone of `source` currently described by `current`,
/// until it matches the scanned entry.
fn mirror_entry(
    source: &Path,
    target: &Path,
    expected: &Entry,
    current: Option<Entry>,
    cloned_since: i64,
    links: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    // The kernel clone splits hard links. The first name of an inode reached
    // here keeps its clone, and later names are linked back to it, sharing
    // the metadata it is repaired to.
    let identity = (expected.kind == EntryKind::File && expected.links > 1)
        .then_some((expected.device, expected.inode));
    if let Some(primary) = identity.and_then(|identity| links.get(&identity)) {
        if !same_inode(primary, target)? {
            unlock_parent(target)?;
            unlock_entry(target)?;
            remove_path(target)?;
            link_entry(primary, target)?;
        }
        return Ok(());
    }
    match current {
        Some(current)
            if expected.untouched_since(cloned_since) && current.semantic_eq(expected) => {}
        Some(current)
            if expected.untouched_since(cloned_since) && current.same_content(expected) =>
        {
            // Directory times are what the clone resets on every tree, so
            // restore them alone when nothing else differs. Other attributes
            // are copied from the source, which must still be what the scan
            // saw: a replaced symlink would otherwise be written through.
            if current.flags == 0 && current.same_attributes(expected) {
                restore_times(
                    target,
                    expected.mtime_ns,
                    current.kind == EntryKind::Symlink,
                )?;
            } else if entry_kind_at(source)? == Some(expected.kind) {
                copy_metadata(source, target)?;
            } else {
                rebuild(source, target, expected)?;
            }
        }
        _ => rebuild(source, target, expected)?,
    }
    if let Some(identity) = identity {
        links.insert(identity, target.to_path_buf());
    }
    Ok(())
}

/// Replace `target` with a fresh clone of `source`: the clone is missing, or
/// the source changed after the clone. Children were settled first, so a
/// directory only needs itself. A source that vanished after the scan leaves
/// the copy alone; the next reconcile sees that deletion.
fn rebuild(source: &Path, target: &Path, expected: &Entry) -> Result<()> {
    if entry_kind_at(source)?.is_none() {
        return Ok(());
    }
    unlock_parent(target)?;
    unlock_entry(target)?;
    remove_path(target)?;
    if expected.kind == EntryKind::Directory {
        fs::create_dir(target)?;
        copy_metadata(source, target)?;
    } else {
        clone_path(source, target, &mut HashMap::new())?;
    }
    Ok(())
}

/// Remove the entries of a copied directory that the scan does not have:
/// the source deleted them after the clone. One that the volume still finds
/// under an expected sibling's name was renamed instead, by case or Unicode
/// normalization only, which APFS ignores in lookups; it takes that spelling.
fn settle_extras(
    destination: &Path,
    parent: &str,
    extras: Vec<PathBuf>,
    index: &TreeIndex,
) -> Result<()> {
    if extras.is_empty() {
        return Ok(());
    }
    let directory = if parent.is_empty() {
        destination.to_path_buf()
    } else {
        destination.join(parent)
    };
    unlock_parent(&extras[0])?;
    let present = fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<HashSet<OsString>>>()?;
    let mut renamed = Vec::new();
    for sibling in index.children(parent) {
        let name = Path::new(&sibling)
            .file_name()
            .expect("indexed path has a file name");
        if present.contains(name) {
            continue;
        }
        if let Ok(metadata) = fs::symlink_metadata(destination.join(&sibling)) {
            renamed.push((sibling, (metadata.dev(), metadata.ino())));
        }
    }
    for extra in extras {
        let metadata = fs::symlink_metadata(&extra)?;
        let identity = (metadata.dev(), metadata.ino());
        if let Some(position) = renamed.iter().position(|(_, found)| *found == identity) {
            let (sibling, _) = renamed.swap_remove(position);
            fs::rename(&extra, destination.join(sibling))?;
            continue;
        }
        unlock_entry(&extra)?;
        remove_path(&extra)?;
    }
    Ok(())
}

fn entry_kind_at(path: &Path) -> Result<Option<EntryKind>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => entry_kind(&metadata).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as i64)
}

/// Entries of a cloned directory that the scan does not have: the source
/// deleted them between the clone and the scan.
fn extra_entries(directory: &Path, relative: &str, index: &TreeIndex) -> Result<Vec<PathBuf>> {
    let mut extras = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = Path::new(relative).join(entry.file_name());
        if !path
            .to_str()
            .is_some_and(|path| index.entries.contains_key(path))
        {
            extras.push(entry.path());
        }
    }
    Ok(extras)
}

fn read_existing(path: &Path) -> Result<Option<Entry>> {
    match fs::symlink_metadata(path) {
        Ok(_) => Entry::read(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Map `items` through `read` on a few threads, keeping their order. Meant
/// for independent metadata reads: on a large or freshly cloned tree most of
/// their time is lookups that proceed concurrently, and beyond four threads
/// APFS stopped getting faster in measurements.
fn read_in_parallel<T, R>(items: &[T], read: impl Fn(&T) -> Result<R> + Sync) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
{
    let threads = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .min(METADATA_READERS);
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let readers = items
            .chunks(chunk)
            .map(|chunk| scope.spawn(|| chunk.iter().map(&read).collect::<Result<Vec<_>>>()))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(items.len());
        for reader in readers {
            results.extend(reader.join().expect("metadata reader panicked")?);
        }
        Ok(results)
    })
}

fn same_inode(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::symlink_metadata(left)?;
    Ok(fs::symlink_metadata(right)
        .is_ok_and(|right| (right.dev(), right.ino()) == (left.dev(), left.ino())))
}

fn expand_subtrees(before: &TreeIndex, live: &Path, paths: &mut BTreeSet<String>) -> Result<()> {
    let roots: Vec<String> = paths.iter().cloned().collect();
    for root in roots {
        if before
            .entries
            .get(&root)
            .is_some_and(|entry| entry.kind == EntryKind::Directory)
        {
            paths.extend(before.descendants(&root));
        }
        let absolute = live.join(&root);
        if fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.is_dir()) {
            for entry in walk(&absolute)? {
                paths.insert(relative_string(
                    entry.strip_prefix(live).expect("subtree stays below live"),
                )?);
            }
        }
    }
    Ok(())
}

fn current_children(live: &Path, parent: &str) -> Result<BTreeSet<String>> {
    let directory = if parent.is_empty() {
        live.to_path_buf()
    } else {
        live.join(parent)
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error.into()),
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| !parent.is_empty() || entry.file_name() != ".deltabox")
        .map(|entry| {
            relative_string(
                entry
                    .path()
                    .strip_prefix(live)
                    .expect("directory entry stays below live"),
            )
        })
        .collect()
}

fn changed_index_paths(before: &TreeIndex, after: &TreeIndex) -> BTreeSet<String> {
    before
        .entries
        .keys()
        .chain(after.entries.keys())
        .filter(|path| before.entries.get(*path) != after.entries.get(*path))
        .cloned()
        .collect()
}

fn indexed_path_matches(index: &TreeIndex, live: &Path, path: &str) -> Result<bool> {
    let current = match fs::symlink_metadata(live.join(path)) {
        Ok(_) => Some(Entry::read(&live.join(path))?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(match (index.entries.get(path), current.as_ref()) {
        (Some(before), Some(current)) => before.semantic_eq(current),
        (None, None) => true,
        _ => false,
    })
}

fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.depth() != 1 || entry.file_name() != ".deltabox")
        .map(|entry| {
            entry
                .map(|entry| entry.path().to_path_buf())
                .map_err(|error| Error::Io(std::io::Error::other(error)))
        })
        .collect()
}

fn entry_kind(metadata: &fs::Metadata) -> Result<EntryKind> {
    if metadata.is_file() {
        Ok(EntryKind::File)
    } else if metadata.is_dir() {
        Ok(EntryKind::Directory)
    } else if metadata.file_type().is_symlink() {
        Ok(EntryKind::Symlink)
    } else {
        Err(Error::Backend("unsupported filesystem entry".into()))
    }
}

pub(crate) fn relative_string(path: &Path) -> Result<String> {
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Backend("invalid relative path".into()));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Backend("non-UTF-8 project path".into()))
}

fn parent_string(path: &str) -> Result<String> {
    let parent = Path::new(path)
        .parent()
        .ok_or_else(|| Error::Backend("relative path has no parent".into()))?;
    if parent.as_os_str().is_empty() {
        Ok(String::new())
    } else {
        relative_string(parent)
    }
}

fn excluded(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == ".deltabox")
}

fn depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    fs::symlink_metadata(&path).is_ok().then_some(path)
}

fn timestamp_ns(seconds: i64, nanos: i64) -> i64 {
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn read_binary<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    bincode::deserialize(&fs::read(path)?)
        .map_err(|error| Error::Backend(format!("decode macOS index: {error}")))
}

fn save_json(path: &Path, value: &impl Serialize, durable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    if durable {
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    if durable {
        sync_dir(path.parent().expect("state file has a parent"))?;
    }
    Ok(())
}

fn save_binary(path: &Path, value: &impl Serialize, durable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let bytes = bincode::serialize(value)
        .map_err(|error| Error::Backend(format!("encode macOS index: {error}")))?;
    let mut file = File::create(&temporary)?;
    file.write_all(&bytes)?;
    if durable {
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    if durable {
        sync_dir(path.parent().expect("state file has a parent"))?;
    }
    Ok(())
}

fn cleanup_children(directory: &Path, keep: &BTreeSet<String>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !keep.contains(&name) {
            remove_path(&entry.path())?;
        }
    }
    Ok(())
}

fn maybe_crash(point: &str) {
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_AT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltafs_fuse::macos::Event;
    use std::os::unix::fs::{FileExt, PermissionsExt};
    use std::process::Command;

    fn state(mode: Mode) -> (tempfile::TempDir, State) {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("a.txt"), "accepted").unwrap();
        fs::create_dir(project.path().join("empty")).unwrap();
        let root = project.path().join(".deltabox/test/native");
        let state = State::create(project.path(), &root, mode, false, 1, "device".into()).unwrap();
        (project, state)
    }

    fn command(program: &str, args: &[&str], path: &Path) {
        let status = Command::new(program).args(args).arg(path).status().unwrap();
        assert!(status.success(), "{program} failed for {}", path.display());
    }

    #[test]
    fn incremental_index_delta_survives_reopen_and_commit_reuses_the_base() {
        let (project, mut state) = state(Mode::Through);
        let root = state.root.clone();
        let base_generation = state.manifest.index_generation.clone();
        fs::write(project.path().join("a.txt"), "pending").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();

        assert_eq!(state.manifest.index_generation, base_generation);
        assert_eq!(state.manifest.index_deltas.len(), 1);
        let delta_generation = state.manifest.index_deltas[0].clone();
        assert!(root
            .join(INDEXES_DIR)
            .join(format!("{delta_generation}.delta"))
            .is_file());

        drop(state);
        let mut reopened = State::open(project.path(), &root).unwrap();
        reopened.load_index().unwrap();
        assert_eq!(
            reopened.index.as_ref().unwrap().entries.get("a.txt"),
            Some(&Entry::read(&project.path().join("a.txt")).unwrap())
        );
        reopened.commit(None, 0).unwrap();
        assert_eq!(reopened.manifest.index_generation, base_generation);
        assert_eq!(
            reopened.manifest.index_deltas,
            vec![delta_generation.clone()]
        );

        drop(reopened);
        let mut reopened = State::open(project.path(), &root).unwrap();
        reopened.load_index().unwrap();
        assert_eq!(
            reopened.index.as_ref().unwrap().entries.get("a.txt"),
            Some(&Entry::read(&project.path().join("a.txt")).unwrap())
        );
    }

    #[test]
    fn full_scan_compacts_deltas_and_legacy_manifest_migrates() {
        let (project, mut state) = state(Mode::Through);
        let root = state.root.clone();
        fs::write(project.path().join("a.txt"), "first").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        let old_base = state.manifest.index_generation.clone();
        let old_delta = state.manifest.index_deltas[0].clone();

        fs::write(project.path().join("a.txt"), "second").unwrap();
        state
            .reconcile(
                &EventBatch {
                    latest_id: 2,
                    history_done: true,
                    must_scan: true,
                    ..EventBatch::default()
                },
                false,
                "device".into(),
            )
            .unwrap();
        assert_ne!(state.manifest.index_generation, old_base);
        assert!(state.manifest.index_deltas.is_empty());
        assert!(!root
            .join(INDEXES_DIR)
            .join(format!("{old_base}.bin"))
            .exists());
        assert!(!root
            .join(INDEXES_DIR)
            .join(format!("{old_delta}.delta"))
            .exists());

        let manifest_path = root.join(MANIFEST_FILE);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        legacy["version"] = LEGACY_FORMAT_VERSION.into();
        legacy.as_object_mut().unwrap().remove("index_deltas");
        fs::write(&manifest_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        drop(state);
        let migrated = State::open(project.path(), &root).unwrap();
        assert_eq!(migrated.manifest.version, FORMAT_VERSION);
        assert!(migrated.manifest.index_deltas.is_empty());
        let persisted: Manifest = read_json(&manifest_path).unwrap();
        assert_eq!(persisted.version, FORMAT_VERSION);
    }

    #[test]
    fn through_checkpoint_abort_and_commit() {
        let (project, mut state) = state(Mode::Through);
        fs::write(project.path().join("a.txt"), "v1").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        let (checkpoint, _) = state.checkpoint(Some("one".into())).unwrap();
        fs::write(project.path().join("a.txt"), "v2").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        state.abort(Some(checkpoint)).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "v1"
        );
        state.commit(None, 0).unwrap();
        fs::write(project.path().join("a.txt"), "pending").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        state.abort(None).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "v1"
        );
    }

    #[test]
    fn private_commit_publishes_and_abort_stays_private() {
        let (project, mut state) = state(Mode::Private);
        fs::write(state.live().join("a.txt"), "pending").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "accepted"
        );
        state.abort(None).unwrap();
        assert_eq!(
            fs::read_to_string(state.live().join("a.txt")).unwrap(),
            "accepted"
        );
        fs::write(state.live().join("a.txt"), "committed").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        state.commit(None, 0).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "committed"
        );
    }

    #[test]
    fn namespace_and_hardlinks_round_trip() {
        let (project, mut state) = state(Mode::Through);
        fs::hard_link(
            project.path().join("a.txt"),
            project.path().join("peer.txt"),
        )
        .unwrap();
        state
            .mark_paths(["a.txt".into(), "peer.txt".into()])
            .unwrap();
        state.commit(None, 0).unwrap();
        fs::remove_file(project.path().join("a.txt")).unwrap();
        fs::remove_file(project.path().join("peer.txt")).unwrap();
        state
            .mark_paths(["a.txt".into(), "peer.txt".into()])
            .unwrap();
        state.abort(None).unwrap();
        assert_eq!(
            fs::metadata(project.path().join("a.txt")).unwrap().ino(),
            fs::metadata(project.path().join("peer.txt")).unwrap().ino()
        );
    }

    #[test]
    fn metadata_and_sparse_files_round_trip() {
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("metadata.txt");
        let directory = project.path().join("metadata-dir");
        let sparse = project.path().join("sparse.bin");
        fs::write(&file, "accepted").unwrap();
        fs::create_dir(&directory).unwrap();
        let sparse_file = File::create(&sparse).unwrap();
        sparse_file.set_len(64 * 1024 * 1024).unwrap();
        sparse_file
            .write_all_at(b"tail", 64 * 1024 * 1024 - 4)
            .unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
        xattr::set(&file, "com.deltabox.test", b"file-xattr").unwrap();
        xattr::set(&directory, "com.deltabox.test", b"dir-xattr").unwrap();
        let user = String::from_utf8(
            Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let acl = format!("{} allow read", user.trim());
        command("/bin/chmod", &["+a", &acl], &file);
        command("/bin/chmod", &["+a", &acl], &directory);
        command("/usr/bin/chflags", &["hidden"], &file);
        command("/usr/bin/chflags", &["hidden"], &directory);
        command("/usr/bin/touch", &["-t", "202001020304.05"], &file);
        command("/usr/bin/touch", &["-t", "202001020304.05"], &directory);

        let expected_file = Entry::read(&file).unwrap();
        let expected_directory = Entry::read(&directory).unwrap();
        let expected_sparse = Entry::read(&sparse).unwrap();
        let root = project.path().join(".deltabox/test/native");
        let mut state = State::create(
            project.path(),
            &root,
            Mode::Through,
            false,
            1,
            "device".into(),
        )
        .unwrap();

        fs::write(&file, "modified").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        xattr::remove(&file, "com.deltabox.test").unwrap();
        command("/bin/chmod", &["-N"], &file);
        command("/usr/bin/chflags", &["nohidden"], &file);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        xattr::remove(&directory, "com.deltabox.test").unwrap();
        xattr::set(&directory, "com.deltabox.extra", b"remove-me").unwrap();
        command("/bin/chmod", &["-N"], &directory);
        command("/usr/bin/chflags", &["nohidden"], &directory);
        fs::write(&sparse, vec![1_u8; 1024 * 1024]).unwrap();
        state
            .mark_paths([
                "metadata.txt".into(),
                "metadata-dir".into(),
                "sparse.bin".into(),
            ])
            .unwrap();
        state.abort(None).unwrap();

        assert!(expected_file.semantic_eq(&Entry::read(&file).unwrap()));
        assert!(expected_directory.semantic_eq(&Entry::read(&directory).unwrap()));
        assert!(expected_sparse.semantic_eq(&Entry::read(&sparse).unwrap()));
        assert_eq!(fs::read(&file).unwrap(), b"accepted");
        let sparse_metadata = fs::metadata(&sparse).unwrap();
        assert!(sparse_metadata.blocks() * 512 < sparse_metadata.len() / 100);
        assert!(xattr::get(&directory, "com.deltabox.extra")
            .unwrap()
            .is_none());
    }

    /// Assert that `copy` holds exactly the tree of `source`: the same entries
    /// with the same metadata and content, and the same hard links.
    fn assert_mirrors(source: &Path, copy: &Path) {
        let expected = TreeIndex::scan(source).unwrap();
        let actual = TreeIndex::scan(copy).unwrap();
        assert_eq!(
            expected.entries.keys().collect::<Vec<_>>(),
            actual.entries.keys().collect::<Vec<_>>()
        );
        let mut copies = HashMap::new();
        let mut sources = HashMap::new();
        for (path, entry) in &expected.entries {
            let copied = &actual.entries[path];
            assert!(entry.semantic_eq(copied), "{path}: {entry:?} != {copied:?}");
            if entry.kind == EntryKind::File {
                assert!(files_equal(&source.join(path), &copy.join(path)).unwrap());
                let identity = (entry.device, entry.inode);
                assert_eq!(
                    *copies.entry(identity).or_insert(copied.inode),
                    copied.inode
                );
                assert_eq!(*sources.entry(copied.inode).or_insert(identity), identity);
            }
        }
    }

    fn other_group(path: &Path) -> Option<u32> {
        let current = fs::metadata(path).unwrap().gid();
        let groups = Command::new("/usr/bin/id")
            .arg("-G")
            .output()
            .unwrap()
            .stdout;
        String::from_utf8(groups)
            .unwrap()
            .split_whitespace()
            .filter_map(|group| group.parse().ok())
            .find(|group| *group != current)
    }

    #[test]
    fn materialize_restores_what_directory_clones_drop() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let nested = root.join("tree/nested");
        let links = root.join("tree/links");
        let locked = root.join("tree/aa-read-only");
        fs::create_dir_all(nested.join("empty")).unwrap();
        fs::create_dir_all(&links).unwrap();
        fs::create_dir_all(&locked).unwrap();
        fs::write(nested.join("acl.txt"), "acl").unwrap();
        fs::write(nested.join("setuid.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(nested.join("setuid.sh"), fs::Permissions::from_mode(0o4755)).unwrap();
        fs::write(nested.join("hidden.txt"), "hidden").unwrap();
        xattr::set(nested.join("hidden.txt"), "com.deltabox.test", b"nested").unwrap();
        xattr::set(&nested, "com.deltabox.test", b"nested-dir").unwrap();
        std::os::unix::fs::symlink("acl.txt", nested.join("link")).unwrap();
        if let Some(group) = other_group(root) {
            fs::write(nested.join("group.txt"), "group").unwrap();
            std::os::unix::fs::chown(nested.join("group.txt"), None, Some(group)).unwrap();
        }
        // The read-only directory sorts first, so relinking its name of the
        // inode has to open the directory up and restore it afterwards.
        fs::write(links.join("primary.txt"), "shared").unwrap();
        fs::hard_link(links.join("primary.txt"), links.join("peer.txt")).unwrap();
        fs::hard_link(links.join("primary.txt"), locked.join("peer.txt")).unwrap();
        let user = String::from_utf8(
            Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        command(
            "/bin/chmod",
            &["+a", &format!("{} allow read", user.trim())],
            &nested.join("acl.txt"),
        );
        command(
            "/bin/chmod",
            &["+a", &format!("{} allow list", user.trim())],
            &nested,
        );
        command("/usr/bin/chflags", &["hidden"], &nested.join("hidden.txt"));
        command("/usr/bin/chflags", &["hidden"], &nested.join("empty"));
        for directory in [nested.join("empty"), nested.clone(), links, locked.clone()] {
            command("/usr/bin/touch", &["-t", "202001020304.05"], &directory);
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        for mode in [Mode::Through, Mode::Private] {
            let state_root = root.join(format!(".deltabox/test/{mode:?}"));
            let state = State::create(root, &state_root, mode, false, 1, "device".into()).unwrap();
            assert_mirrors(root, &state_root.join(BASE_DIR));
            if mode == Mode::Private {
                assert_mirrors(root, &state.live());
            }
            assert_eq!(
                state.index.as_ref().unwrap().entries,
                TreeIndex::scan(&state.live()).unwrap().entries
            );
        }
        command("/bin/chmod", &["-R", "u+w"], root);
    }

    #[test]
    fn parallel_reads_keep_their_order() {
        let items = (0..1001).collect::<Vec<u64>>();
        let doubled = read_in_parallel(&items, |item| Ok(item * 2)).unwrap();
        assert_eq!(
            doubled,
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        );
        assert!(read_in_parallel(&items, |item| if *item == 700 {
            Err(Error::Backend("unreadable".into()))
        } else {
            Ok(*item)
        })
        .is_err());
    }

    #[test]
    fn mirror_repairs_entries_changed_after_the_clone() {
        let source = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        let tree = source.path();
        fs::create_dir_all(tree.join("gone/inner")).unwrap();
        fs::write(tree.join("gone/inner/file.txt"), "nested").unwrap();
        fs::create_dir(tree.join("kept")).unwrap();
        fs::write(tree.join("kept/rewritten.txt"), "original").unwrap();
        fs::write(tree.join("kept/became-dir"), "file").unwrap();
        fs::create_dir(tree.join("kept/became-file")).unwrap();
        fs::write(tree.join("kept/became-file/inner.txt"), "inner").unwrap();
        fs::write(tree.join("kept/primary.txt"), "linked").unwrap();
        fs::hard_link(tree.join("kept/primary.txt"), tree.join("kept/peer.txt")).unwrap();
        std::os::unix::fs::symlink("rewritten.txt", tree.join("kept/link")).unwrap();
        let destination = copy.path().join("tree");
        materialize(tree, &destination, false).unwrap();
        assert_mirrors(tree, &destination);

        // A clone that partly failed, or raced with writers to the source.
        fs::remove_dir_all(destination.join("gone")).unwrap();
        fs::write(
            destination.join("kept/rewritten.txt"),
            "rewritten after the clone",
        )
        .unwrap();
        fs::remove_file(destination.join("kept/became-dir")).unwrap();
        fs::create_dir_all(destination.join("kept/became-dir/child")).unwrap();
        fs::remove_dir_all(destination.join("kept/became-file")).unwrap();
        fs::write(destination.join("kept/became-file"), "was a directory").unwrap();
        fs::remove_file(destination.join("kept/peer.txt")).unwrap();
        fs::write(destination.join("kept/peer.txt"), "linked").unwrap();
        fs::remove_file(destination.join("kept/link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", destination.join("kept/link")).unwrap();
        fs::write(destination.join("extra.txt"), "deleted from the source").unwrap();
        fs::create_dir_all(destination.join("kept/extra-dir/inner")).unwrap();
        fs::set_permissions(destination.join("kept"), fs::Permissions::from_mode(0o500)).unwrap();

        mirror(tree, &destination, now_ns()).unwrap();
        assert_mirrors(tree, &destination);
    }

    #[test]
    fn mirror_never_reaches_through_a_stale_symlink_in_the_copy() {
        let source = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("child.txt"), "outside").unwrap();
        fs::create_dir(outside.path().join("sub")).unwrap();
        fs::write(outside.path().join("sub/keep.txt"), "outside").unwrap();
        // At clone time `dir` was a symlink to a directory outside the project.
        let tree = source.path();
        std::os::unix::fs::symlink(outside.path(), tree.join("dir")).unwrap();
        let destination = copy.path().join("tree");
        materialize(tree, &destination, false).unwrap();
        assert!(fs::symlink_metadata(destination.join("dir"))
            .unwrap()
            .file_type()
            .is_symlink());

        // Before the scan, the project replaced it with a real directory.
        fs::remove_file(tree.join("dir")).unwrap();
        fs::create_dir_all(tree.join("dir/sub")).unwrap();
        fs::write(tree.join("dir/child.txt"), "inside").unwrap();
        mirror(tree, &destination, now_ns()).unwrap();
        assert_mirrors(tree, &destination);
        assert_eq!(
            fs::read_to_string(outside.path().join("child.txt")).unwrap(),
            "outside"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("sub/keep.txt")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn mirror_renames_entries_the_volume_still_finds_under_their_old_name() {
        let source = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        let tree = source.path();
        fs::create_dir(tree.join("Docs")).unwrap();
        fs::write(tree.join("Docs/Readme.md"), "readme").unwrap();
        let destination = copy.path().join("tree");
        materialize(tree, &destination, false).unwrap();

        // Renamed by case only after the clone: APFS still finds the old
        // names, so the copy must take the new spelling, not lose the entries.
        fs::rename(tree.join("Docs/Readme.md"), tree.join("Docs/README.md")).unwrap();
        fs::rename(tree.join("Docs"), tree.join("DOCS")).unwrap();
        mirror(tree, &destination, now_ns()).unwrap();
        assert_mirrors(tree, &destination);
        let names = |directory: &Path| {
            let mut names = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(&destination), ["DOCS"]);
        assert_eq!(names(&destination.join("DOCS")), ["README.md"]);
        assert_eq!(
            fs::read_to_string(destination.join("DOCS/README.md")).unwrap(),
            "readme"
        );
    }

    #[test]
    fn mirror_rebuilds_files_rewritten_without_changing_size_or_mtime() {
        let source = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        let file = source.path().join("same.txt");
        fs::write(&file, "aaaa").unwrap();
        command("/usr/bin/touch", &["-t", "202001020304.05"], &file);
        let destination = copy.path().join("tree");
        materialize(source.path(), &destination, false).unwrap();

        // A writer that restores the modification time, as `cp -p` or
        // `rsync -t` do, still moves the change time.
        let cloned_since = now_ns();
        fs::write(&file, "bbbb").unwrap();
        command("/usr/bin/touch", &["-t", "202001020304.05"], &file);
        mirror(source.path(), &destination, cloned_since).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("same.txt")).unwrap(),
            "bbbb"
        );
        assert_mirrors(source.path(), &destination);
    }

    #[test]
    fn materialize_relinks_locked_hard_links() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        fs::write(root.join("a.txt"), "locked").unwrap();
        fs::hard_link(root.join("a.txt"), root.join("b.txt")).unwrap();
        command("/usr/bin/chflags", &["uchg"], &root.join("a.txt"));
        let destination = root.join(".deltabox/base");
        let mirrored = materialize(root, &destination, false).map(|_| ());
        let groups = mirrored.as_ref().ok().map(|_| {
            fs::metadata(destination.join("a.txt")).unwrap().ino()
                == fs::metadata(destination.join("b.txt")).unwrap().ino()
        });
        command("/usr/bin/chflags", &["-R", "nouchg"], root);
        mirrored.unwrap();
        assert_eq!(groups, Some(true));
    }

    #[test]
    fn repairs_never_write_through_a_symlink_whose_source_changed() {
        let source = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        fs::write(&victim, "victim").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let link = source.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let expected = Entry::read(&link).unwrap();
        let target = copy.path().join("link");
        std::os::unix::fs::symlink(&victim, &target).unwrap();
        // The clone differs only in a flag, so it is due for a metadata repair.
        command("/usr/bin/chflags", &["-h", "hidden"], &target);
        let current = Entry::read(&target).unwrap();
        assert!(!current.semantic_eq(&expected) && current.same_content(&expected));

        // Between the scan and the repair, the source replaced the symlink.
        fs::remove_file(&link).unwrap();
        fs::write(&link, "regular").unwrap();
        fs::set_permissions(&link, fs::Permissions::from_mode(0o600)).unwrap();
        mirror_entry(
            &link,
            &target,
            &expected,
            Some(current),
            now_ns(),
            &mut HashMap::new(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim");
        assert_eq!(fs::metadata(&victim).unwrap().mode() & 0o777, 0o644);
        assert!(fs::symlink_metadata(&target).unwrap().is_file());
    }

    #[test]
    fn commit_to_older_checkpoint_accepts_its_exact_view() {
        let (project, mut state) = state(Mode::Through);
        fs::write(project.path().join("a.txt"), "one").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        let (one, _) = state.checkpoint(Some("one".into())).unwrap();
        fs::write(project.path().join("a.txt"), "two").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        state.checkpoint(Some("two".into())).unwrap();
        fs::write(project.path().join("a.txt"), "three").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();

        state.commit(Some(one), 0).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "one"
        );
        assert!(state.diff(None, None).unwrap().is_empty());
        fs::write(project.path().join("a.txt"), "pending").unwrap();
        state.mark_paths(["a.txt".into()]).unwrap();
        state.abort(None).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn dropped_events_reconcile_current_tree_and_taint_through_mode() {
        let (project, mut state) = state(Mode::Through);
        fs::write(project.path().join("new.txt"), "new").unwrap();
        let batch = EventBatch {
            latest_id: 9,
            history_done: true,
            must_scan: true,
            ..EventBatch::default()
        };
        assert_eq!(state.reconcile(&batch, true, "device".into()).unwrap(), 1);
        assert_eq!(state.status().pending, 1);
        assert!(state.status().tainted);
    }

    #[test]
    fn root_change_reconciles_the_tree_and_updates_device_identity() {
        let (project, mut state) = state(Mode::Through);
        fs::write(project.path().join("a.txt"), "changed").unwrap();
        let batch = EventBatch {
            latest_id: 11,
            history_done: true,
            root_changed: true,
            ..EventBatch::default()
        };
        assert_eq!(
            state
                .reconcile(&batch, false, "replacement-device".into())
                .unwrap(),
            1
        );
        let status = state.status();
        assert_eq!(status.pending, 1);
        assert_eq!(status.event_id, 11);
        assert_eq!(status.device_uuid, "replacement-device");
        assert!(!status.tainted);
        state.abort(None).unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("a.txt")).unwrap(),
            "accepted"
        );
    }

    #[test]
    fn unchanged_clone_source_event_is_not_a_pending_write() {
        let (project, mut state) = state(Mode::Through);
        let batch = EventBatch {
            events: vec![Event {
                path: project.path().join("a.txt"),
                flags: ITEM_CLONED | ITEM_CREATED,
                id: 2,
            }],
            latest_id: 2,
            history_done: true,
            ..EventBatch::default()
        };
        assert_eq!(state.reconcile(&batch, true, "device".into()).unwrap(), 0);
        assert_eq!(state.status().pending, 0);
        assert!(!state.status().tainted);
    }

    #[test]
    fn delayed_abort_event_does_not_reopen_pending_changes() {
        let (project, mut state) = state(Mode::Through);
        let path = project.path().join("new.txt");
        fs::write(&path, "pending").unwrap();
        let created = EventBatch {
            events: vec![Event {
                path: path.clone(),
                flags: ITEM_CREATED,
                id: 2,
            }],
            latest_id: 2,
            history_done: true,
            ..EventBatch::default()
        };
        assert_eq!(
            state.reconcile(&created, false, "device".into()).unwrap(),
            1
        );
        state.abort(None).unwrap();
        assert_eq!(state.status().pending, 0);

        let delayed = EventBatch {
            events: vec![Event {
                path,
                flags: ITEM_REMOVED,
                id: 3,
            }],
            latest_id: 3,
            history_done: true,
            ..EventBatch::default()
        };
        assert_eq!(
            state.reconcile(&delayed, false, "device".into()).unwrap(),
            0
        );
        assert_eq!(state.status().pending, 0);
    }
}

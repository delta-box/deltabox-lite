use crate::changeset::ChangeKind as HostChangeKind;
use crate::error::{ConflictReport, Error, PathConflict, Result};
use crate::snapshot::clone_or_copy_file;
use crate::test_hooks::maybe_pause;
use deltafs_fuse::engine::{Change, ChangeKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub const ACCEPTED_INDEX: &str = "accepted";
pub const PUBLICATION_STATE: &str = "publication.json";
pub const LIVE_DIR: &str = "live";
pub const PUBLISH_DIR: &str = "publish";
pub const RETIRED_DIR: &str = "retired";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedState {
    root: Fingerprint,
    entries: BTreeMap<String, Fingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcceptedManifest {
    root: Fingerprint,
    root_generation: String,
    directories: BTreeMap<String, DirectoryStamp>,
    hardlinks: BTreeMap<String, BTreeSet<String>>,
}

pub struct AcceptedIndex {
    directory: PathBuf,
    manifest: AcceptedManifest,
    durable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AcceptedEntry {
    path: String,
    parent_generation: String,
    generation: Option<String>,
    fingerprint: Fingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DirectoryStamp {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_ns: i64,
    ctime_ns: i64,
    inode: u64,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum Fingerprint {
    File {
        mode: u32,
        uid: u32,
        gid: u32,
        mtime_ns: i64,
        ctime_ns: i64,
        device: u64,
        inode: u64,
        links: u64,
        size: u64,
        digest: String,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
    Directory {
        mode: u32,
        uid: u32,
        gid: u32,
        mtime_ns: i64,
        ctime_ns: i64,
        inode: u64,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
    Symlink {
        uid: u32,
        gid: u32,
        mtime_ns: i64,
        ctime_ns: i64,
        inode: u64,
        target: Vec<u8>,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublicationPhase {
    Prepared,
    HostApplied,
    EngineCommitted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publication {
    pub id: String,
    pub phase: PublicationPhase,
    pub engine_accepted_checkpoint: u64,
    pub changes: Vec<PublishChange>,
    #[serde(default)]
    durable: bool,
    /// publog head at prepare time: restore() must not roll back a path a
    /// sibling republished after this publication (its backup is stale).
    #[serde(default)]
    pub base_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishChange {
    pub path: String,
    pub kind: ChangeKind,
    pub existed: bool,
    pub namespace_changed: bool,
}

impl AcceptedState {
    pub fn capture(project: &Path) -> Result<Self> {
        let root = fingerprint(project, true)?
            .ok_or_else(|| Error::Backend("project root is unavailable".into()))?;
        let mut entries = BTreeMap::new();
        for entry in project_walk(project) {
            let entry = entry?;
            let relative = relative_string(project, entry.path())?;
            let value = fingerprint(entry.path(), false)?
                .ok_or_else(|| Error::Backend(format!("path disappeared: {relative}")))?;
            entries.insert(relative, value);
        }
        Ok(Self { root, entries })
    }
}

impl AcceptedIndex {
    #[cfg(test)]
    pub fn create(directory: &Path, state: AcceptedState) -> Result<Self> {
        Self::create_with_durability(directory, state, false)
    }

    pub fn create_with_durability(
        directory: &Path,
        state: AcceptedState,
        durable: bool,
    ) -> Result<Self> {
        remove_path(directory)?;
        fs::create_dir_all(directory.join("entries"))?;
        let mut hardlinks: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut directories = BTreeMap::new();
        let root_generation = Uuid::new_v4().to_string();
        let mut generations = BTreeMap::new();
        generations.insert(String::new(), root_generation.clone());
        directories.insert(
            String::new(),
            state
                .root
                .directory_stamp()
                .expect("project root is a directory"),
        );
        for (path, fingerprint) in state.entries {
            let parent = path_string(
                Path::new(&path)
                    .parent()
                    .expect("relative path has a parent"),
            )?;
            let parent_generation = generations
                .get(&parent)
                .expect("captured parent precedes its child")
                .clone();
            let generation = fingerprint
                .is_directory()
                .then(|| Uuid::new_v4().to_string());
            if let Some(identity) = fingerprint.hardlink_identity() {
                hardlinks.entry(identity).or_default().insert(path.clone());
            }
            if let Some(stamp) = fingerprint.directory_stamp() {
                directories.insert(path.clone(), stamp);
            }
            if let Some(generation) = &generation {
                generations.insert(path.clone(), generation.clone());
            }
            save_entry(
                directory,
                &path,
                &parent_generation,
                generation,
                &fingerprint,
                durable,
            )?;
        }
        hardlinks.retain(|_, paths| paths.len() > 1);
        let index = Self {
            directory: directory.to_path_buf(),
            manifest: AcceptedManifest {
                root: state.root,
                root_generation,
                directories,
                hardlinks,
            },
            durable,
        };
        index.save_manifest()?;
        if durable {
            sync_dir(&directory.join("entries"))?;
            sync_dir(directory)?;
            if let Some(parent) = directory.parent() {
                sync_dir(parent)?;
            }
        }
        Ok(index)
    }

    pub fn open_with_durability(directory: &Path, durable: bool) -> Result<Self> {
        Ok(Self {
            directory: directory.to_path_buf(),
            manifest: serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?,
            durable,
        })
    }

    /// `validate_with_endorsement` without an endorsement set, kept for unit
    /// tests; production validation always flows through `complete`.
    #[cfg(test)]
    pub fn validate(&self, project: &Path, paths: impl IntoIterator<Item = String>) -> Result<()> {
        self.validate_with_endorsement(project, paths, None)
    }

    /// `validate`, with an optional pre-declared endorsement set: a conflict
    /// on a path whose current host content digest-matches an endorsed publog
    /// digest is accepted baseline movement (a sibling publication the caller
    /// has chosen to override). Anything without a matching digest — dirs,
    /// symlinks, deletions, oversized files, strangers — keeps failing closed.
    pub fn validate_with_endorsement(
        &self,
        project: &Path,
        paths: impl IntoIterator<Item = String>,
        endorsed: Option<&BTreeMap<String, String>>,
    ) -> Result<()> {
        let mut checked = BTreeSet::new();
        let mut targets = Vec::new();
        checked.insert(String::new());
        for path in paths {
            validate_relative(&path)?;
            targets.push(path.clone());
            checked.insert(path.clone());
            let mut parent = Path::new(&path).parent();
            while let Some(value) = parent {
                if value.as_os_str().is_empty() {
                    break;
                }
                checked.insert(path_string(value)?);
                parent = value.parent();
            }
        }
        let target_set: BTreeSet<&str> = targets.iter().map(String::as_str).collect();
        let mut conflicts = Vec::new();
        let mut probe_error = None;
        for relative in &checked {
            let strict = target_set.contains(relative.as_str());
            match self.validate_path(project, relative, strict) {
                Ok(found) => conflicts.extend(found),
                Err(error) => {
                    // A drifting host can make later probes fail outright;
                    // already-confirmed conflicts are the more useful answer.
                    probe_error = Some(error);
                    break;
                }
            }
        }
        if let (Some(error), true) = (probe_error, conflicts.is_empty()) {
            return Err(error);
        }
        for target in targets {
            if self
                .entry(&target)?
                .is_some_and(|entry| entry.is_directory())
            {
                for entry in fs::read_dir(self.directory.join("entries"))? {
                    let entry: AcceptedEntry = serde_json::from_slice(&fs::read(entry?.path())?)?;
                    if is_ancestor(&target, &entry.path) {
                        conflicts.extend(self.validate_path(project, &entry.path, true)?);
                    }
                }
            }
        }
        if let Some(endorsed) = endorsed {
            let mut kept = Vec::new();
            for conflict in conflicts {
                let host_path = project.join(&conflict.path);
                let overridden = host_path.is_file()
                    && endorsed.get(&conflict.path).is_some_and(|expected| {
                        crate::publog::file_digest(&host_path)
                            .map(|digest| digest == *expected)
                            .unwrap_or(false)
                    });
                if !overridden {
                    kept.push(conflict);
                }
            }
            conflicts = kept;
        }
        if conflicts.is_empty() {
            return Ok(());
        }
        conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        conflicts.dedup_by(|a, b| a.path == b.path);
        let message = format!(
            "host path changed after session start: {}",
            conflicts[0].path
        );
        Err(Error::ProjectConflict(ConflictReport::new(message, conflicts)))
    }

    fn validate_path(
        &self,
        project: &Path,
        relative: &str,
        strict: bool,
    ) -> Result<Option<PathConflict>> {
        let expected = if relative.is_empty() {
            Some(self.manifest.root.clone())
        } else {
            self.entry(relative)?
        };
        let actual = fingerprint_shallow(
            if relative.is_empty() {
                project.to_path_buf()
            } else {
                project.join(relative)
            }
            .as_path(),
            relative.is_empty(),
        )?;
        let matches = match (&expected, &actual) {
            // A shared parent that did not exist at this session's baseline
            // was created by a sibling's publication; both sessions must be
            // able to introduce files under it. Targets never take this arm.
            (None, Some(Fingerprint::Directory { .. })) if !strict => true,
            (
                Some(Fingerprint::Directory {
                    mode: expected_mode,
                    uid: expected_uid,
                    gid: expected_gid,
                    xattrs: expected_xattrs,
                    ..
                }),
                Some(Fingerprint::Directory {
                    mode: actual_mode,
                    uid: actual_uid,
                    gid: actual_gid,
                    xattrs: actual_xattrs,
                    ..
                }),
            ) if !strict =>
            {
                // Ancestor directories: sibling publications legitimately bump
                // membership and timestamps; identity, permissions, and ACLs
                // (xattrs) are load-bearing. Targets never take this arm.
                expected_mode == actual_mode
                    && expected_uid == actual_uid
                    && expected_gid == actual_gid
                    && expected_xattrs == actual_xattrs
            }
            (Some(Fingerprint::Directory { .. }), Some(actual)) => self
                .manifest
                .directories
                .get(relative)
                .is_some_and(|expected| actual.directory_stamp().as_ref() == Some(expected)),
            (Some(expected), Some(actual)) => expected == actual,
            (None, None) => true,
            _ => false,
        };
        if matches {
            Ok(None)
        } else {
            let host_change = match (&expected, &actual) {
                (None, Some(_)) => HostChangeKind::Added,
                (Some(_), None) => HostChangeKind::Deleted,
                _ => HostChangeKind::Modified,
            };
            let label = if relative.is_empty() { "." } else { relative };
            Ok(Some(PathConflict {
                path: label.to_string(),
                host_change,
            }))
        }
    }

    pub fn refresh(&mut self, project: &Path, changes: &[PublishChange]) -> Result<()> {
        let mut parents = BTreeSet::new();
        let mut namespace_parents = BTreeSet::new();
        for change in changes {
            for paths in self.manifest.hardlinks.values_mut() {
                paths.retain(|path| {
                    path != &change.path
                        && !path
                            .strip_prefix(&change.path)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                });
            }
            self.remove_subtree(&change.path)?;
            let path = project.join(&change.path);
            if fs::symlink_metadata(&path).is_ok() {
                self.capture_path(project, &path)?;
            }
            let parent = path_string(
                Path::new(&change.path)
                    .parent()
                    .expect("relative path has a parent"),
            )?;
            parents.insert(parent.clone());
            if change.namespace_changed {
                namespace_parents.insert(parent);
            }
        }
        for parent in parents {
            let path = if parent.is_empty() {
                project.to_path_buf()
            } else {
                project.join(&parent)
            };
            if namespace_parents.contains(&parent) {
                if parent.is_empty() {
                    self.manifest.root = fingerprint(project, true)?
                        .ok_or_else(|| Error::Backend("project root is unavailable".into()))?;
                } else {
                    // A namespace change means this directory's CONTENTS
                    // changed, not its identity. Removing and re-adding the
                    // entry would issue a fresh generation and silently
                    // invalidate every descendant entry (untouched baseline
                    // subtrees included, and ones captured moments ago in the
                    // loop above); a later add_entry under such an orphaned
                    // subtree then found no indexed parent and panicked.
                    // Update the entry in place so descendants stay valid;
                    // only a vanished directory or a type flip tears it down.
                    let prior_is_dir = {
                        let file = entry_path(&self.directory, &parent);
                        file.exists()
                            && serde_json::from_slice::<AcceptedEntry>(&fs::read(&file)?)
                                .ok()
                                .is_some_and(|e| {
                                    e.path == parent && e.fingerprint.is_directory()
                                })
                    };
                    match fingerprint(&path, false)? {
                        Some(value) => {
                            if !(prior_is_dir && value.is_directory()) {
                                self.remove_entry(&parent)?;
                            }
                            self.add_entry(&parent, value)?;
                        }
                        None => self.remove_entry(&parent)?,
                    }
                }
            }
            if let Some(stamp) = fingerprint_shallow(&path, parent.is_empty())?
                .and_then(|fingerprint| fingerprint.directory_stamp())
            {
                self.manifest.directories.insert(parent, stamp);
            } else if !parent.is_empty() {
                self.manifest.directories.remove(&parent);
            }
        }
        self.manifest.hardlinks.retain(|_, paths| paths.len() > 1);
        self.save_manifest()
    }

    fn entry(&self, path: &str) -> Result<Option<Fingerprint>> {
        Ok(self.stored_entry(path)?.map(|entry| entry.fingerprint))
    }

    fn stored_entry(&self, path: &str) -> Result<Option<AcceptedEntry>> {
        let relative = Path::new(path);
        let parent = relative.parent().expect("relative path has a parent");
        let parent_generation = if parent.as_os_str().is_empty() {
            Some(self.manifest.root_generation.clone())
        } else {
            self.stored_entry(&path_string(parent)?)?
                .filter(|entry| entry.fingerprint.is_directory())
                .and_then(|entry| entry.generation)
        };
        let file = entry_path(&self.directory, path);
        if !file.exists() {
            return Ok(None);
        }
        let entry: AcceptedEntry = serde_json::from_slice(&fs::read(file)?)?;
        if entry.path != path {
            return Err(Error::Backend("accepted fingerprint path mismatch".into()));
        }
        if parent_generation.as_deref() != Some(entry.parent_generation.as_str()) {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    fn hardlink_peers(&self, path: &str) -> Result<Vec<String>> {
        let Some(identity) = self
            .entry(path)?
            .and_then(|entry| entry.hardlink_identity())
        else {
            return Ok(Vec::new());
        };
        Ok(self
            .manifest
            .hardlinks
            .get(&identity)
            .map(|paths| paths.iter().cloned().collect())
            .unwrap_or_default())
    }

    fn capture_path(&mut self, project: &Path, path: &Path) -> Result<()> {
        let relative = relative_string(project, path)?;
        let value = fingerprint(path, false)?.expect("captured path exists");
        self.add_entry(&relative, value)?;
        if fs::symlink_metadata(path)?.is_dir() {
            for entry in walkdir::WalkDir::new(path).min_depth(1).follow_links(false) {
                let entry = entry.map_err(|error| Error::Io(std::io::Error::other(error)))?;
                let relative = relative_string(project, entry.path())?;
                let value = fingerprint(entry.path(), false)?.expect("walked path exists");
                self.add_entry(&relative, value)?;
            }
        }
        Ok(())
    }

    fn add_entry(&mut self, path: &str, fingerprint: Fingerprint) -> Result<()> {
        if let Some(identity) = fingerprint.hardlink_identity() {
            self.manifest
                .hardlinks
                .entry(identity)
                .or_default()
                .insert(path.into());
        }
        if let Some(stamp) = fingerprint.directory_stamp() {
            self.manifest.directories.insert(path.into(), stamp);
        }
        let parent = Path::new(path)
            .parent()
            .expect("relative path has a parent");
        let parent_generation = if parent.as_os_str().is_empty() {
            self.manifest.root_generation.clone()
        } else {
            self.stored_entry(&path_string(parent)?)?
                .filter(|entry| entry.fingerprint.is_directory())
                .and_then(|entry| entry.generation)
                .ok_or_else(|| {
                    Error::Backend(format!(
                        "accepted index has no valid parent entry for {path}"
                    ))
                })?
        };
        let generation = if fingerprint.is_directory() {
            // Updating a directory entry must not change its identity: the
            // generation is what keeps every descendant entry valid, and a
            // content change (new or removed children) does not make it a
            // different directory. Reuse the stored generation when the entry
            // file still describes a directory; only a truly torn-down entry
            // (removed via remove_subtree, or a type flip) gets a fresh one.
            // Raw read on purpose: an ancestor may have been re-captured, and
            // we only need this directory's own identity here.
            let prior = {
                let file = entry_path(&self.directory, path);
                if file.exists() {
                    serde_json::from_slice::<AcceptedEntry>(&fs::read(&file)?)
                        .ok()
                        .filter(|e| e.path == path && e.fingerprint.is_directory())
                        .and_then(|e| e.generation)
                } else {
                    None
                }
            };
            Some(prior.unwrap_or_else(|| Uuid::new_v4().to_string()))
        } else {
            None
        };
        save_entry(
            &self.directory,
            path,
            &parent_generation,
            generation,
            &fingerprint,
            self.durable,
        )
    }

    fn remove_subtree(&mut self, path: &str) -> Result<()> {
        self.remove_entry(path)
    }

    fn remove_entry(&mut self, path: &str) -> Result<()> {
        if let Some(value) = self.entry(path)? {
            if let Some(identity) = value.hardlink_identity() {
                if let Some(paths) = self.manifest.hardlinks.get_mut(&identity) {
                    paths.remove(path);
                }
            }
            fs::remove_file(entry_path(&self.directory, path))?;
            if self.durable {
                sync_dir(&self.directory.join("entries"))?;
            }
        }
        self.manifest.directories.remove(path);
        Ok(())
    }

    fn save_manifest(&self) -> Result<()> {
        save_json(
            &self.directory.join("manifest.json"),
            &self.manifest,
            self.durable,
        )
    }
}

/// A publication frozen before the publish lock: the collapsed publish set,
/// its staged tree under `publish/<id>/staged`, and digests of that frozen
/// copy. Everything here is session-private on disk. `complete` performs the
/// in-lock half; `discard` releases one that never publishes.
pub struct StagedPublication {
    id: String,
    changes: Vec<StagedChange>,
    /// sha256 per staged regular file at or below the digest cap — the
    /// publog digests for this publication. `apply` renames these exact
    /// files into the host, so a staged digest is a digest of what lands.
    digests: BTreeMap<String, String>,
    durable: bool,
}

struct StagedChange {
    path: String,
    kind: ChangeKind,
    namespace_changed: bool,
}

impl Publication {
    /// Session-private half of a publication, safe to run without the
    /// publish lock: collapses the publish set and freezes it as the staged
    /// tree, digesting that frozen copy. Reads only the live tree and the
    /// accepted index. Failures clean up after themselves; a staged
    /// publication that never publishes must be released with `discard`.
    pub fn stage(
        store: &Path,
        live: &Path,
        accepted: &AcceptedIndex,
        changes: &[Change],
        durable: bool,
    ) -> Result<StagedPublication> {
        let collapsed = collapse_changes(changes, accepted, live)?;
        let id = Uuid::new_v4().to_string();
        sweep_stale_transactions(store, &id);
        let transaction = store.join(PUBLISH_DIR).join(&id);
        match stage_changes(&transaction, live, accepted, &collapsed, durable) {
            Ok((changes, digests)) => {
                maybe_crash("private_publish_staged");
                maybe_pause("commit_staged");
                Ok(StagedPublication {
                    id,
                    changes,
                    digests,
                    durable,
                })
            }
            Err(error) => {
                let _ = remove_path(&transaction);
                Err(error)
            }
        }
    }

    /// Both halves back to back, for unit tests that need no lock between
    /// them. Production runs `stage` before the publish lock and `complete`
    /// under it.
    #[cfg(test)]
    pub fn prepare(
        store: &Path,
        project: &Path,
        live: &Path,
        accepted: &AcceptedIndex,
        changes: &[Change],
        endorsed: Option<&BTreeMap<String, String>>,
        engine_accepted_checkpoint: u64,
        durable: bool,
    ) -> Result<Self> {
        let staged = Self::stage(store, live, accepted, changes, durable)?;
        match staged.complete(
            store,
            project,
            live,
            accepted,
            changes,
            endorsed,
            engine_accepted_checkpoint,
        ) {
            Ok(publication) => Ok(publication),
            Err(error) => {
                staged.discard(store);
                Err(error)
            }
        }
    }

    pub fn load(store: &Path) -> Result<Option<Self>> {
        let path = store.join(PUBLICATION_STATE);
        if path.exists() {
            Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
        } else {
            Ok(None)
        }
    }

    pub fn save(&self, store: &Path) -> Result<()> {
        save_json(&store.join(PUBLICATION_STATE), self, self.durable)
    }

    pub fn apply(&self, store: &Path, project: &Path) -> Result<()> {
        let staged = store.join(PUBLISH_DIR).join(&self.id).join("staged");
        let mut deletions: Vec<&PublishChange> = self
            .changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Deleted)
            .collect();
        deletions.sort_by_key(|change| std::cmp::Reverse(depth(&change.path)));
        for change in deletions {
            remove_path(&project.join(&change.path))?;
            maybe_crash("private_publish_path_applied");
            maybe_pause("publish_host_partially_applied");
        }
        let mut writes: Vec<&PublishChange> = self
            .changes
            .iter()
            .filter(|change| change.kind != ChangeKind::Deleted)
            .collect();
        writes.sort_by_key(|change| depth(&change.path));
        for change in writes {
            let target = project.join(&change.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            remove_path(&target)?;
            fs::rename(staged.join(&change.path), target)?;
            maybe_crash("private_publish_path_applied");
            maybe_pause("publish_host_partially_applied");
        }
        if self.durable {
            sync_changes(project, &self.changes)?;
        }
        Ok(())
    }

    pub fn restore(&self, store: &Path, project: &Path) -> Result<()> {
        let backup = transaction_path(store, &self.id).join("backup");
        // Paths a sibling republished after this publication carry NEWER data
        // than this backup; rolling them back would silently destroy the
        // sibling's accepted work. Leave them to the sibling's own history.
        let mut superseded = std::collections::BTreeSet::new();
        for entry in crate::publog::entries_since(project, self.base_seq)? {
            for change in entry.changes {
                superseded.insert(change.path);
            }
        }
        let mut changes: Vec<&PublishChange> = self
            .changes
            .iter()
            .filter(|change| !superseded.contains(&change.path))
            .collect();
        changes.sort_by_key(|change| depth(&change.path));
        for change in &changes {
            remove_path(&project.join(&change.path))?;
        }
        let mut links = HashMap::new();
        for change in changes {
            if change.existed {
                clone_path(
                    &backup.join(&change.path),
                    &project.join(&change.path),
                    &mut links,
                )?;
            }
        }
        if self.durable {
            sync_changes(project, &self.changes)?;
        }
        Ok(())
    }

    pub fn retire(&self, store: &Path) -> Result<PathBuf> {
        let publish = store.join(PUBLISH_DIR);
        let retired = publish.join(RETIRED_DIR);
        fs::create_dir_all(&retired)?;
        let destination = retired.join(&self.id);
        let source = publish.join(&self.id);
        if source.exists() {
            fs::rename(source, &destination)?;
        }
        fs::remove_file(store.join(PUBLICATION_STATE))?;
        if self.durable {
            sync_dir(&publish)?;
            sync_dir(store)?;
        }
        Ok(destination)
    }
}

pub fn retire_orphans(store: &Path) -> Result<bool> {
    let publish = store.join(PUBLISH_DIR);
    if !publish.exists() {
        return Ok(false);
    }
    let retired = publish.join(RETIRED_DIR);
    fs::create_dir_all(&retired)?;
    let mut found = false;
    for entry in fs::read_dir(&publish)? {
        let entry = entry?;
        if entry.file_name() != RETIRED_DIR {
            fs::rename(entry.path(), retired.join(entry.file_name()))?;
            found = true;
        }
    }
    Ok(found)
}

#[cfg(test)]
pub fn materialize(project: &Path, live: &Path) -> Result<AcceptedState> {
    materialize_with_durability(project, live, false)
}

pub fn materialize_with_durability(
    project: &Path,
    live: &Path,
    durable: bool,
) -> Result<AcceptedState> {
    remove_path(live)?;
    fs::create_dir_all(live)?;
    let mut links = HashMap::new();
    for entry in fs::read_dir(project)? {
        let entry = entry?;
        if entry.file_name() != ".deltabox" {
            clone_path(&entry.path(), &live.join(entry.file_name()), &mut links)?;
        }
    }
    copy_metadata(project, live)?;
    if durable {
        sync_tree(live)?;
        if let Some(parent) = live.parent() {
            sync_dir(parent)?;
        }
    }
    AcceptedState::capture(project)
}

impl StagedPublication {
    /// In-lock half of a publication; the caller must hold the publish lock.
    /// Recomputes the publish set against the (by now fast-forwarded)
    /// accepted index and refuses to publish if it drifted from the staged
    /// set: collapse expands hardlink peers, and a sibling publication
    /// landing between staging and this lock can break a peer relationship,
    /// so publishing the stale expansion could override work this session
    /// never saw. Then validates against the host, captures the host
    /// backups, and records the prepared publication.
    pub fn complete(
        &self,
        store: &Path,
        project: &Path,
        live: &Path,
        accepted: &AcceptedIndex,
        changes: &[Change],
        endorsed: Option<&BTreeMap<String, String>>,
        engine_accepted_checkpoint: u64,
    ) -> Result<Publication> {
        let fresh = collapse_changes(changes, accepted, live)?;
        let staged: BTreeMap<&str, ChangeKind> = self
            .changes
            .iter()
            .map(|change| (change.path.as_str(), change.kind.clone()))
            .collect();
        let current: BTreeMap<&str, ChangeKind> = fresh
            .iter()
            .map(|change| (change.path.as_str(), change.kind.clone()))
            .collect();
        if staged != current {
            let conflicts = staged
                .keys()
                .chain(current.keys())
                .filter(|path| staged.get(**path) != current.get(**path))
                .map(|path| PathConflict {
                    path: (*path).to_string(),
                    host_change: crate::changeset::ChangeKind::Modified,
                })
                .collect();
            return Err(Error::ProjectConflict(ConflictReport::new(
                "publish set changed while staging; retry the commit",
                conflicts,
            )));
        }
        accepted.validate_with_endorsement(
            project,
            self.changes.iter().map(|change| change.path.clone()),
            endorsed,
        )?;
        let base_seq = crate::publog::head_seq(project)?;
        let transaction = store.join(PUBLISH_DIR).join(&self.id);
        let backup = transaction.join("backup");
        fs::create_dir_all(&backup)?;
        let mut backup_links = HashMap::new();
        let mut plan = Vec::new();
        for change in &self.changes {
            let host = project.join(&change.path);
            let existed = fs::symlink_metadata(&host).is_ok();
            if existed {
                clone_path(&host, &backup.join(&change.path), &mut backup_links)?;
            }
            plan.push(PublishChange {
                path: change.path.clone(),
                kind: change.kind.clone(),
                existed,
                namespace_changed: change.namespace_changed,
            });
        }
        let publication = Publication {
            id: self.id.clone(),
            phase: PublicationPhase::Prepared,
            engine_accepted_checkpoint,
            changes: plan,
            durable: self.durable,
            base_seq,
        };
        if self.durable {
            sync_tree(&backup)?;
            sync_dir(&transaction)?;
            sync_dir(&store.join(PUBLISH_DIR))?;
        }
        publication.save(store)?;
        Ok(publication)
    }

    /// The publog digests computed from the staged tree, consumed once the
    /// publication lands.
    pub fn take_digests(&mut self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.digests)
    }

    /// Best-effort release of a staged publication that will not publish.
    /// If `publication.json` names this id, the in-lock publication already
    /// began and recovery owns the directory — leave it alone.
    pub fn discard(&self, store: &Path) {
        let owned = Publication::load(store)
            .ok()
            .flatten()
            .is_some_and(|publication| publication.id == self.id);
        if !owned {
            let _ = remove_path(&store.join(PUBLISH_DIR).join(&self.id));
        }
    }
}

fn stage_changes(
    transaction: &Path,
    live: &Path,
    accepted: &AcceptedIndex,
    changes: &[Change],
    durable: bool,
) -> Result<(Vec<StagedChange>, BTreeMap<String, String>)> {
    let staged_dir = transaction.join("staged");
    fs::create_dir_all(&staged_dir)?;
    let mut links = HashMap::new();
    let mut staged = Vec::new();
    for change in changes {
        validate_relative(&change.path)?;
        let namespace_changed = accepted.entry(&change.path)?.map(|entry| entry.path_kind())
            != path_kind(&live.join(&change.path))?;
        if change.kind != ChangeKind::Deleted {
            clone_path(
                &live.join(&change.path),
                &staged_dir.join(&change.path),
                &mut links,
            )?;
        }
        staged.push(StagedChange {
            path: change.path.clone(),
            kind: change.kind.clone(),
            namespace_changed,
        });
    }
    // Digest the frozen staged copy, not the live tree: a live write after
    // hashing would otherwise record a digest for content that never lands.
    // Regular files only — the publog documents that directories and
    // symlinks carry no digest and fail closed.
    let mut digests = BTreeMap::new();
    for change in &staged {
        if change.kind == ChangeKind::Deleted {
            continue;
        }
        let path = staged_dir.join(&change.path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            continue;
        }
        if let Some(digest) = crate::publog::file_digest_capped(&path)? {
            digests.insert(change.path.clone(), digest);
        }
    }
    if durable {
        sync_tree(&staged_dir)?;
        sync_dir(transaction)?;
    }
    Ok((staged, digests))
}

/// Removes transaction directories no publication references: leftovers of a
/// crash between staging and the publish lock. The store is session-private
/// and the caller holds the session lock, so nothing else owns these. The
/// active transaction (named by `publication.json`) and the one being staged
/// are kept.
fn sweep_stale_transactions(store: &Path, staging_id: &str) {
    let active = Publication::load(store)
        .ok()
        .flatten()
        .map(|publication| publication.id);
    let Ok(entries) = fs::read_dir(store.join(PUBLISH_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name != staging_id && active.as_deref() != Some(name) {
            let _ = remove_path(&entry.path());
        }
    }
}

fn collapse_changes(
    changes: &[Change],
    accepted: &AcceptedIndex,
    live: &Path,
) -> Result<Vec<Change>> {
    let mut expanded: BTreeMap<String, Change> = changes
        .iter()
        .cloned()
        .map(|change| (change.path.clone(), change))
        .collect();
    for change in changes {
        for path in accepted.hardlink_peers(&change.path)? {
            // Peer expansion must not smuggle unpublishable paths into the
            // publish set (git object stores hardlink aggressively).
            if !crate::publog::is_publishable(&path) {
                continue;
            }
            expanded.entry(path.clone()).or_insert_with(|| Change {
                path: path.clone(),
                kind: ChangeKind::Modified,
                old_size: None,
                new_size: None,
                old_bytes: None,
                new_bytes: None,
            });
        }
    }
    let mut ordered: Vec<Change> = expanded.into_values().collect();
    ordered.sort_by_key(|change| depth(&change.path));
    let mut result: Vec<Change> = Vec::new();
    for change in ordered {
        let mut covered = false;
        for parent in &result {
            if is_ancestor(&parent.path, &change.path)
                && (is_directory(&live.join(&parent.path))
                    || accepted
                        .entry(&parent.path)?
                        .is_some_and(|entry| entry.is_directory()))
            {
                covered = true;
                break;
            }
        }
        if !covered {
            result.push(change);
        }
    }
    Ok(result)
}

impl Fingerprint {
    fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }

    fn hardlink_identity(&self) -> Option<String> {
        match self {
            Self::File {
                device,
                inode,
                links,
                ..
            } if *links > 1 => Some(format!("{device}:{inode}")),
            _ => None,
        }
    }

    fn path_kind(&self) -> u8 {
        match self {
            Self::File { .. } => 1,
            Self::Directory { .. } => 2,
            Self::Symlink { .. } => 3,
        }
    }

    fn directory_stamp(&self) -> Option<DirectoryStamp> {
        match self {
            Self::Directory {
                mode,
                uid,
                gid,
                mtime_ns,
                ctime_ns,
                inode,
                xattrs,
                ..
            } => Some(DirectoryStamp {
                mode: *mode,
                uid: *uid,
                gid: *gid,
                mtime_ns: *mtime_ns,
                ctime_ns: *ctime_ns,
                inode: *inode,
                xattrs: xattrs.clone(),
            }),
            _ => None,
        }
    }
}

fn project_walk(root: &Path) -> impl Iterator<Item = Result<walkdir::DirEntry>> {
    walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            // `.git` is session-private and never published, so the accepted
            // baseline has no use for its entries — and hashing a repository's
            // object store (often larger than the working tree) would dominate
            // session start for nothing.
            (entry.depth() != 1 || entry.file_name() != ".deltabox")
                && entry.file_name() != ".git"
        })
        .map(|entry| entry.map_err(|error| Error::Io(std::io::Error::other(error))))
}

fn fingerprint(path: &Path, _project_root: bool) -> Result<Option<Fingerprint>> {
    fingerprint_inner(path)
}

fn fingerprint_shallow(path: &Path, _project_root: bool) -> Result<Option<Fingerprint>> {
    fingerprint_inner(path)
}

fn fingerprint_inner(path: &Path) -> Result<Option<Fingerprint>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let xattrs = read_xattrs(path)?;
    let mtime_ns = timestamp_ns(metadata.mtime(), metadata.mtime_nsec());
    let ctime_ns = timestamp_ns(metadata.ctime(), metadata.ctime_nsec());
    if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        Ok(Some(Fingerprint::File {
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime_ns,
            ctime_ns,
            device: metadata.dev(),
            inode: metadata.ino(),
            links: metadata.nlink(),
            size: metadata.len(),
            digest: format!("{:x}", hash.finalize()),
            xattrs,
        }))
    } else if metadata.is_dir() {
        Ok(Some(Fingerprint::Directory {
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime_ns,
            ctime_ns,
            inode: metadata.ino(),
            xattrs,
        }))
    } else if metadata.file_type().is_symlink() {
        Ok(Some(Fingerprint::Symlink {
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime_ns,
            ctime_ns,
            inode: metadata.ino(),
            target: fs::read_link(path)?.into_os_string().into_vec(),
            xattrs,
        }))
    } else {
        Err(Error::Backend(format!(
            "unsupported project entry: {}",
            path.display()
        )))
    }
}

fn path_kind(path: &Path) -> Result<Option<u8>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(file_type_kind(&metadata.file_type()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn file_type_kind(file_type: &fs::FileType) -> u8 {
    if file_type.is_file() {
        1
    } else if file_type.is_dir() {
        2
    } else if file_type.is_symlink() {
        3
    } else {
        4
    }
}

fn clone_path(
    source: &Path,
    destination: &Path,
    links: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if metadata.is_file() {
        let identity = (metadata.dev(), metadata.ino());
        if let Some(existing) = links.get(&identity) {
            fs::hard_link(existing, destination)?;
        } else {
            clone_or_copy_file(source, destination)?;
            links.insert(identity, destination.to_path_buf());
        }
        copy_metadata(source, destination)?;
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            clone_path(&entry.path(), &destination.join(entry.file_name()), links)?;
        }
        copy_metadata(source, destination)?;
    } else if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
        copy_metadata(source, destination)?;
    } else {
        return Err(Error::Backend(format!(
            "unsupported project entry: {}",
            source.display()
        )));
    }
    Ok(())
}

fn copy_metadata(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    set_owner(
        destination,
        metadata.uid(),
        metadata.gid(),
        metadata.file_type().is_symlink(),
    )?;
    if !metadata.file_type().is_symlink() {
        fs::set_permissions(destination, fs::Permissions::from_mode(metadata.mode()))?;
    }
    for name in xattr::list(source)? {
        if let Some(value) = xattr::get(source, &name)? {
            if xattr::get(destination, &name)?.as_deref() != Some(value.as_slice()) {
                xattr::set(destination, &name, &value)?;
            }
        }
    }
    set_times(
        destination,
        timestamp_ns(metadata.atime(), metadata.atime_nsec()),
        timestamp_ns(metadata.mtime(), metadata.mtime_nsec()),
        metadata.file_type().is_symlink(),
    )
}

fn read_xattrs(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut values = Vec::new();
    for name in xattr::list(path)? {
        let value = xattr::get(path, &name)?.unwrap_or_default();
        values.push((name.into_vec(), value));
    }
    values.sort();
    Ok(values)
}

fn set_owner(path: &Path, uid: u32, gid: u32, symlink: bool) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Backend("path contains a null byte".into()))?;
    let result = unsafe {
        if symlink {
            libc::lchown(path.as_ptr(), uid, gid)
        } else {
            libc::chown(path.as_ptr(), uid, gid)
        }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn set_times(path: &Path, atime_ns: i64, mtime_ns: i64, symlink: bool) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Backend("path contains a null byte".into()))?;
    let times = [timespec(atime_ns), timespec(mtime_ns)];
    let flags = if symlink {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn timespec(ns: i64) -> libc::timespec {
    libc::timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: ns % 1_000_000_000,
    }
}

fn timestamp_ns(seconds: i64, nanos: i64) -> i64 {
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn save_json(path: &Path, value: &impl Serialize, durable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(value)?)?;
    if durable {
        file.sync_all()?;
    }
    fs::rename(temporary, path)?;
    if durable {
        sync_dir(path.parent().expect("saved path has a parent"))?;
    }
    Ok(())
}

fn save_entry(
    directory: &Path,
    path: &str,
    parent_generation: &str,
    generation: Option<String>,
    fingerprint: &Fingerprint,
    durable: bool,
) -> Result<()> {
    save_json(
        &entry_path(directory, path),
        &AcceptedEntry {
            path: path.into(),
            parent_generation: parent_generation.into(),
            generation,
            fingerprint: fingerprint.clone(),
        },
        durable,
    )
}

fn sync_tree(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| Error::Io(std::io::Error::other(error)))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file() {
            File::open(entry.path())?.sync_all()?;
        } else if metadata.file_type().is_dir() {
            directories.push(entry.path().to_path_buf());
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_dir(&directory)?;
    }
    Ok(())
}

fn sync_changes(project: &Path, changes: &[PublishChange]) -> Result<()> {
    let mut directories = BTreeSet::from([project.to_path_buf()]);
    for change in changes {
        let path = project.join(&change.path);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_file() {
                File::open(&path)?.sync_all()?;
            } else if metadata.file_type().is_dir() {
                directories.insert(path.clone());
            }
        }
        let mut parent = path.parent();
        while let Some(path) = parent {
            if !path.starts_with(project) {
                break;
            }
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    directories.insert(path.to_path_buf());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if path == project {
                break;
            }
            parent = path.parent();
        }
    }
    let mut directories: Vec<PathBuf> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_dir(&directory)?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn entry_path(directory: &Path, path: &str) -> PathBuf {
    let digest = Sha256::digest(path.as_bytes());
    directory.join("entries").join(format!("{digest:x}"))
}

fn validate_relative(path: &str) -> Result<()> {
    let value = Path::new(path);
    if value.as_os_str().is_empty()
        || value
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || value
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".deltabox")
    {
        return Err(Error::Backend(format!("invalid publication path: {path}")));
    }
    Ok(())
}

fn relative_string(root: &Path, path: &Path) -> Result<String> {
    path_string(
        path.strip_prefix(root)
            .map_err(|_| Error::Backend("path escaped project".into()))?,
    )
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::Backend(format!("path is not UTF-8: {}", path.display())))
}

fn is_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn is_ancestor(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn maybe_crash(point: &str) {
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_AT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

fn transaction_path(store: &Path, id: &str) -> PathBuf {
    let active = store.join(PUBLISH_DIR).join(id);
    if active.exists() {
        active
    } else {
        store.join(PUBLISH_DIR).join(RETIRED_DIR).join(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, kind: ChangeKind) -> Change {
        Change {
            path: path.into(),
            kind,
            old_size: None,
            new_size: None,
            old_bytes: None,
            new_bytes: None,
        }
    }


    #[test]
    fn refresh_namespace_change_keeps_sibling_subtrees_valid() {
        // Regression: refreshing a namespace-changed path used to rebuild the
        // parent directory's entry with a fresh generation, implicitly
        // invalidating every existing descendant entry (untouched baseline
        // subtrees included). Validation then reported those paths as host
        // changes, and a later refresh beneath such an orphaned subtree found
        // no valid parent entry and panicked. Two sessions each running
        // `git commit` hit exactly this shape through `.git/objects/`.
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let index = root.path().join("accepted");
        fs::create_dir_all(project.join("dir/existing")).unwrap();
        fs::write(project.join("dir/existing/keep.txt"), "base").unwrap();
        let state = AcceptedState::capture(&project).unwrap();
        let mut accepted = AcceptedIndex::create(&index, state).unwrap();

        // A sibling publication adds a new subdirectory next to `existing`.
        fs::create_dir(project.join("dir/new")).unwrap();
        fs::write(project.join("dir/new/n.txt"), "sibling").unwrap();
        accepted
            .refresh(
                &project,
                &[PublishChange {
                    path: "dir/new".into(),
                    kind: ChangeKind::Added,
                    existed: false,
                    namespace_changed: true,
                }],
            )
            .unwrap();

        // The untouched sibling subtree must still validate cleanly...
        accepted
            .validate(&project, ["dir/existing/keep.txt".into()])
            .expect("untouched sibling subtree stays valid after a fold");

        // ...and refreshing a path beneath it must succeed rather than fail
        // over a missing parent entry.
        fs::write(project.join("dir/existing/keep.txt"), "next").unwrap();
        accepted
            .refresh(
                &project,
                &[PublishChange {
                    path: "dir/existing/keep.txt".into(),
                    kind: ChangeKind::Modified,
                    existed: true,
                    namespace_changed: false,
                }],
            )
            .expect("refresh under an existing sibling subtree succeeds");
    }

    #[test]
    fn materialization_is_private_and_preserves_hardlinks() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let live = root.path().join("live");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a"), "base").unwrap();
        fs::hard_link(project.join("a"), project.join("b")).unwrap();
        fs::create_dir(project.join(".deltabox")).unwrap();
        fs::write(project.join(".deltabox/secret"), "state").unwrap();

        materialize(&project, &live).unwrap();
        fs::write(live.join("a"), "private").unwrap();

        assert_eq!(fs::read_to_string(project.join("a")).unwrap(), "base");
        assert_eq!(
            fs::metadata(live.join("a")).unwrap().ino(),
            fs::metadata(live.join("b")).unwrap().ino()
        );
        assert!(!live.join(".deltabox").exists());
    }

    #[test]
    fn strong_validation_detects_same_size_host_edit() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let index = root.path().join("accepted");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a"), "one").unwrap();
        let accepted = AcceptedState::capture(&project).unwrap();
        let accepted = AcceptedIndex::create(&index, accepted).unwrap();
        fs::write(project.join("a"), "two").unwrap();
        let metadata = fs::metadata(project.join("a")).unwrap();
        set_times(
            &project.join("a"),
            timestamp_ns(metadata.atime(), metadata.atime_nsec()),
            timestamp_ns(metadata.mtime(), metadata.mtime_nsec()),
            false,
        )
        .unwrap();
        assert!(matches!(
            accepted.validate(&project, ["a".into()]),
            Err(Error::ProjectConflict(_))
        ));
    }

    #[test]
    fn publication_restores_or_applies_touched_paths() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let live = root.path().join("live");
        let store = root.path().join("store");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&store).unwrap();
        fs::write(project.join("a"), "old").unwrap();
        fs::write(project.join("gone"), "old").unwrap();
        let accepted = materialize(&project, &live).unwrap();
        let accepted = AcceptedIndex::create(&store.join(ACCEPTED_INDEX), accepted).unwrap();
        fs::write(live.join("a"), "new").unwrap();
        fs::write(live.join("added"), "new").unwrap();
        fs::remove_file(live.join("gone")).unwrap();
        let changes = vec![
            change("a", ChangeKind::Modified),
            change("added", ChangeKind::Added),
            change("gone", ChangeKind::Deleted),
        ];
        let publication =
            Publication::prepare(&store, &project, &live, &accepted, &changes, None, 0, false)
                .unwrap();
        publication.apply(&store, &project).unwrap();
        assert_eq!(fs::read_to_string(project.join("a")).unwrap(), "new");
        assert!(project.join("added").exists());
        assert!(!project.join("gone").exists());
        publication.restore(&store, &project).unwrap();
        assert_eq!(fs::read_to_string(project.join("a")).unwrap(), "old");
        assert!(!project.join("added").exists());
        assert_eq!(fs::read_to_string(project.join("gone")).unwrap(), "old");
    }

    // Digests come from the frozen staged copy, cover regular files only, and
    // the split halves still publish end to end.
    #[test]
    fn staged_digests_cover_regular_files_only() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let live = root.path().join("live");
        let store = root.path().join("store");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&store).unwrap();
        fs::write(project.join("f"), "old").unwrap();
        fs::write(project.join("gone"), "old").unwrap();
        let accepted = materialize(&project, &live).unwrap();
        let accepted = AcceptedIndex::create(&store.join(ACCEPTED_INDEX), accepted).unwrap();
        fs::write(live.join("f"), "new-content").unwrap();
        std::os::unix::fs::symlink("f", live.join("ln")).unwrap();
        fs::remove_file(live.join("gone")).unwrap();
        let changes = vec![
            change("f", ChangeKind::Modified),
            change("ln", ChangeKind::Added),
            change("gone", ChangeKind::Deleted),
        ];
        let mut staged = Publication::stage(&store, &live, &accepted, &changes, false).unwrap();
        let expected = crate::publog::file_digest(&live.join("f")).unwrap();
        assert_eq!(
            staged.digests,
            std::collections::BTreeMap::from([("f".to_string(), expected)]),
            "one digest for the regular file; none for the symlink or deletion"
        );
        let publication = staged
            .complete(&store, &project, &live, &accepted, &changes, None, 0)
            .unwrap();
        publication.apply(&store, &project).unwrap();
        assert_eq!(fs::read_to_string(project.join("f")).unwrap(), "new-content");
        assert!(project.join("ln").is_symlink());
        assert!(!project.join("gone").exists());
        assert_eq!(staged.take_digests().len(), 1);
        assert!(staged.take_digests().is_empty());
    }

    // Collapse expands hardlink peers from the accepted index. If a sibling
    // publication rewrites a peer between staging and the publish lock, the
    // staged expansion is stale and publishing it would override work this
    // session never saw — complete must refuse instead.
    #[test]
    fn complete_refuses_a_drifted_publish_set() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let live = root.path().join("live");
        let store = root.path().join("store");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&store).unwrap();
        fs::write(project.join("a"), "linked").unwrap();
        fs::hard_link(project.join("a"), project.join("b")).unwrap();
        let accepted = materialize(&project, &live).unwrap();
        let mut accepted = AcceptedIndex::create(&store.join(ACCEPTED_INDEX), accepted).unwrap();
        fs::write(live.join("a"), "rewritten").unwrap();
        let changes = vec![change("a", ChangeKind::Modified)];
        let staged = Publication::stage(&store, &live, &accepted, &changes, false).unwrap();
        assert_eq!(
            staged
                .changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "staging expanded the hardlink peer"
        );
        // A sibling publication replaces the peer with an independent file
        // and the in-lock fast-forward refreshes its index entry.
        fs::remove_file(project.join("b")).unwrap();
        fs::write(project.join("b"), "sibling").unwrap();
        accepted
            .refresh(
                &project,
                &[PublishChange {
                    path: "b".into(),
                    kind: ChangeKind::Modified,
                    existed: true,
                    namespace_changed: false,
                }],
            )
            .unwrap();
        let drifted = staged.complete(&store, &project, &live, &accepted, &changes, None, 0);
        assert!(
            matches!(drifted, Err(Error::ProjectConflict(_))),
            "stale hardlink expansion must be refused: {drifted:?}"
        );
        staged.discard(&store);
        assert_eq!(
            fs::read_dir(store.join(PUBLISH_DIR)).unwrap().count(),
            0,
            "discard removed the staged transaction"
        );
        assert_eq!(fs::read_to_string(project.join("b")).unwrap(), "sibling");
    }
}

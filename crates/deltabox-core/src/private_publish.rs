use crate::error::{Error, Result};
use crate::snapshot::clone_or_copy_file;
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
    pub fn create(directory: &Path, state: AcceptedState) -> Result<Self> {
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
        };
        index.save_manifest()?;
        Ok(index)
    }

    pub fn open(directory: &Path) -> Result<Self> {
        Ok(Self {
            directory: directory.to_path_buf(),
            manifest: serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?,
        })
    }

    pub fn validate(&self, project: &Path, paths: impl IntoIterator<Item = String>) -> Result<()> {
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
        for relative in checked {
            self.validate_path(project, &relative)?;
        }
        for target in targets {
            if self
                .entry(&target)?
                .is_some_and(|entry| entry.is_directory())
            {
                for entry in fs::read_dir(self.directory.join("entries"))? {
                    let entry: AcceptedEntry = serde_json::from_slice(&fs::read(entry?.path())?)?;
                    if is_ancestor(&target, &entry.path) {
                        self.validate_path(project, &entry.path)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_path(&self, project: &Path, relative: &str) -> Result<()> {
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
            Ok(())
        } else {
            let label = if relative.is_empty() { "." } else { relative };
            Err(Error::ProjectConflict(format!(
                "host path changed after session start: {label}"
            )))
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
                    self.remove_entry(&parent)?;
                    if let Some(value) = fingerprint(&path, false)? {
                        self.add_entry(&parent, value)?;
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
                .and_then(|entry| entry.generation)
                .expect("captured path has an indexed parent")
        };
        let generation = fingerprint
            .is_directory()
            .then(|| Uuid::new_v4().to_string());
        save_entry(
            &self.directory,
            path,
            &parent_generation,
            generation,
            &fingerprint,
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
        }
        self.manifest.directories.remove(path);
        Ok(())
    }

    fn save_manifest(&self) -> Result<()> {
        save_json(&self.directory.join("manifest.json"), &self.manifest)
    }
}

impl Publication {
    pub fn prepare(
        store: &Path,
        project: &Path,
        live: &Path,
        accepted: &AcceptedIndex,
        changes: &[Change],
        engine_accepted_checkpoint: u64,
    ) -> Result<Self> {
        let changes = collapse_changes(changes, accepted, live)?;
        accepted.validate(project, changes.iter().map(|change| change.path.clone()))?;
        let id = Uuid::new_v4().to_string();
        let transaction = store.join(PUBLISH_DIR).join(&id);
        let backup = transaction.join("backup");
        let staged = transaction.join("staged");
        fs::create_dir_all(&backup)?;
        fs::create_dir_all(&staged)?;
        let mut links = HashMap::new();
        let mut backup_links = HashMap::new();
        let mut plan = Vec::new();
        for change in changes {
            validate_relative(&change.path)?;
            let host = project.join(&change.path);
            let existed = fs::symlink_metadata(&host).is_ok();
            let namespace_changed = accepted.entry(&change.path)?.map(|entry| entry.path_kind())
                != path_kind(&live.join(&change.path))?;
            if existed {
                clone_path(&host, &backup.join(&change.path), &mut backup_links)?;
            }
            if change.kind != ChangeKind::Deleted {
                clone_path(
                    &live.join(&change.path),
                    &staged.join(&change.path),
                    &mut links,
                )?;
            }
            plan.push(PublishChange {
                path: change.path,
                kind: change.kind,
                existed,
                namespace_changed,
            });
        }
        let publication = Self {
            id,
            phase: PublicationPhase::Prepared,
            engine_accepted_checkpoint,
            changes: plan,
        };
        publication.save(store)?;
        Ok(publication)
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
        save_json(&store.join(PUBLICATION_STATE), self)
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
        }
        Ok(())
    }

    pub fn restore(&self, store: &Path, project: &Path) -> Result<()> {
        let backup = transaction_path(store, &self.id).join("backup");
        let mut changes: Vec<&PublishChange> = self.changes.iter().collect();
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

pub fn materialize(project: &Path, live: &Path) -> Result<AcceptedState> {
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
    AcceptedState::capture(project)
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
        .filter_entry(|entry| entry.depth() != 1 || entry.file_name() != ".deltabox")
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
            xattr::set(destination, &name, &value)?;
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

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn save_entry(
    directory: &Path,
    path: &str,
    parent_generation: &str,
    generation: Option<String>,
    fingerprint: &Fingerprint,
) -> Result<()> {
    save_json(
        &entry_path(directory, path),
        &AcceptedEntry {
            path: path.into(),
            parent_generation: parent_generation.into(),
            generation,
            fingerprint: fingerprint.clone(),
        },
    )
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
            Publication::prepare(&store, &project, &live, &accepted, &changes, 0).unwrap();
        publication.apply(&store, &project).unwrap();
        assert_eq!(fs::read_to_string(project.join("a")).unwrap(), "new");
        assert!(project.join("added").exists());
        assert!(!project.join("gone").exists());
        publication.restore(&store, &project).unwrap();
        assert_eq!(fs::read_to_string(project.join("a")).unwrap(), "old");
        assert!(!project.join("added").exists());
        assert_eq!(fs::read_to_string(project.join("gone")).unwrap(), "old");
    }
}

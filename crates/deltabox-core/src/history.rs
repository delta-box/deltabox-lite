//! Durable, immutable project-file versions. Retention is manual: no automatic
//! pruning is performed. APFS clone-exclusive bytes cannot be inferred by stat.
//! All access is serialized by the manager's session and project publication
//! locks; captures additionally check selected files and ignore rules before and after cloning.
use crate::changeset::{describe_change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{ConflictReport, Error, PathConflict, Result};
use crate::history_scope::SourceScope;
use crate::macos_tree::{clone_path, copy_metadata, read_acl, sync_dir, sync_tree, tracked_xattr};
use crate::protocol::{DiffResult, HistoryVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) mod incremental;

/// Opt-in stage timings on stderr; never changes the RPC response or durability.
pub(crate) struct RestoreTimings(Option<std::time::Instant>);

impl RestoreTimings {
    pub(crate) fn new() -> Self {
        Self((std::env::var("DELTABOX_HISTORY_TIMINGS").as_deref() == Ok("1"))
            .then(std::time::Instant::now))
    }

    pub(crate) fn mark(&mut self, stage: &str) {
        if let Some(last) = self.0.as_mut() {
            eprintln!("{}", serde_json::json!({"history_restore_stage": stage,
                "elapsed_ms": last.elapsed().as_secs_f64() * 1000.0}));
            *last = std::time::Instant::now();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    kind: String,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    flags: u32,
    acl: Vec<u8>,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    digest: String,
    /// Lexicographically first in-project path in a multi-link equivalence
    /// class. Raw inode numbers differ between immutable clones and live files.
    link_group: Option<String>,
}

type Index = BTreeMap<String, Entry>;

fn manifest_logical_bytes(entries: &Index) -> Result<u64> {
    entries.values().filter(|entry| entry.kind == "file")
        .try_fold(0u64, |total, entry| total.checked_add(entry.size)
            .ok_or_else(|| Error::Backend("history logical byte count overflow".into())))
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    project: PathBuf,
    version: HistoryVersion,
    entries: Index,
    #[serde(default)]
    scope: Option<SourceScope>,
}

pub(crate) struct HistoryStore {
    root: PathBuf,
    project: PathBuf,
    durable: bool,
}

impl HistoryStore {
    #[cfg(test)]
    pub fn open(base: &Path, project: &Path) -> Result<Self> {
        Self::open_with_durability(base, project, true)
    }

    pub fn open_with_durability(base: &Path, project: &Path, durable: bool) -> Result<Self> {
        let project = project.canonicalize()?;
        // Hash raw canonical bytes rather than accepting a caller-supplied path ID.
        let key = format!("{:x}", Sha256::digest(project.as_os_str().as_bytes()));
        fs::create_dir_all(base)?;
        let base = base.canonicalize()?;
        if base.starts_with(&project) {
            return Err(Error::Backend(
                "history storage must be outside the project".into(),
            ));
        }
        let root = base.join("history-v1").join(key);
        fs::create_dir_all(&root)?;
        if root.canonicalize()? != root {
            return Err(Error::Backend(
                "history storage must not contain symlinks".into(),
            ));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        if durable {
            sync_dir(&root)?;
            sync_dir(root.parent().expect("history namespace"))?;
            sync_dir(&base)?;
        }
        Ok(Self {
            root,
            project,
            durable,
        })
    }

    /// Open only published history; a cold preview creates no history or engine state.
    pub fn open_existing(base: &Path, project: &Path) -> Result<Self> {
        let project = project.canonicalize()?;
        let base = base.canonicalize()?;
        if base.starts_with(&project) {
            return Err(Error::Backend("history storage must be outside the project".into()));
        }
        let key = format!("{:x}", Sha256::digest(project.as_os_str().as_bytes()));
        let root = base.join("history-v1").join(key);
        if root.canonicalize()? != root || !fs::metadata(&root)?.is_dir() {
            return Err(Error::Backend("history storage must be a directory without symlinks".into()));
        }
        Ok(Self { root, project, durable: false })
    }

    pub fn capture(&self, session_id: &str, label: Option<String>) -> Result<HistoryVersion> {
        let scope = SourceScope::capture(&self.project)?;
        let before = scan_scoped(&self.project, &scope)?;
        let id = Uuid::new_v4().to_string();
        let staging = self.root.join(format!(".capture-{id}"));
        fs::create_dir(&staging)?;
        let captured = (|| {
            let tree = staging.join("tree");
            // Publish once after validating the clone. Flushing every file both
            // here and below doubled strict-mode IO and made fast-mode history
            // unexpectedly pay a power-loss durability cost on every step.
            clone_selected(&self.project, &tree, &before)?;
            let entries = scan(&tree)?;
            let after = scan_scoped(&self.project, &scope)?;
            if entries != before || after != before || SourceScope::capture(&self.project)? != scope
            {
                let changed: Vec<_> = changed_paths(&before, &entries)
                    .union(&changed_paths(&before, &after))
                    .take(8)
                    .cloned()
                    .collect();
                return Err(Error::SessionBusy(
                    format!("project changed during history capture; retry once external writers are idle; changed paths: {}", changed.join(", ")),
                ));
            }
            let version = HistoryVersion {
                id: id.clone(),
                session_id: session_id.into(),
                label,
                created_at_ms: crate::session::now_ms(),
                entry_count: entries.len().saturating_sub(1),
                logical_bytes: entries
                    .values()
                    .filter(|entry| entry.kind == "file")
                    .map(|entry| entry.size)
                    .sum(),
            };
            let manifest = Manifest {
                format: 2,
                project: self.project.clone(),
                version: version.clone(),
                entries,
                scope: Some(scope),
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(staging.join("manifest.json"))?;
            file.write_all(&serde_json::to_vec(&manifest)?)?;
            if self.durable {
                file.sync_all()?;
                sync_tree(&staging)?;
            }
            drop(file);
            fs::rename(&staging, self.root.join(&id))?;
            if self.durable {
                sync_dir(&self.root)?;
            }
            Ok(version)
        })();
        if captured.is_err() {
            let _ = crate::macos_tree::remove_path(&staging);
        }
        captured
    }

    pub fn list(&self) -> Result<Vec<HistoryVersion>> {
        let mut versions = Vec::new();
        for item in fs::read_dir(&self.root)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            } // incomplete captures never become versions
            versions.push(self.manifest(&name)?.version);
        }
        versions.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then(a.id.cmp(&b.id)));
        Ok(versions)
    }

    fn version_dir(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        let path = self.root.join(id);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            return Err(Error::Backend(format!("history version not found: {id}")));
        }
        Ok(path)
    }

    fn manifest(&self, id: &str) -> Result<Manifest> {
        let dir = self.version_dir(id)?;
        let manifest_path = dir.join("manifest.json");
        if !fs::symlink_metadata(&manifest_path)?.is_file() {
            return Err(Error::Backend(
                "history manifest must be a regular file without symlinks".into(),
            ));
        }
        let mut manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
        if !matches!(manifest.format, 1 | 2)
            || manifest.project != self.project
            || manifest.version.id != id
            || (manifest.format == 2) != manifest.scope.is_some()
        {
            return Err(Error::Backend("history manifest identity mismatch".into()));
        }
        if !manifest.entries.get(".").is_some_and(|entry| entry.kind == "directory") {
            return Err(Error::Backend("history manifest is missing its root directory".into()));
        }
        for (name, entry) in &manifest.entries {
            if !matches!(entry.kind.as_str(), "file" | "directory" | "symlink") {
                return Err(Error::Backend("unknown history manifest entry kind".into()));
            }
            let path = Path::new(name);
            if name == "." {
                continue;
            }
            if name.is_empty()
                || path
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
                || path
                    .components()
                    .next()
                    .is_some_and(|part| part.as_os_str() == ".deltabox")
            {
                return Err(Error::Backend("unsafe history manifest path".into()));
            }
        }
        if manifest.format == 1 {
            // Keep legacy snapshots intact on disk. Their project files remain
            // usable, while generated files and Git metadata stay live at restore.
            let tree = dir.join("tree");
            for (path, expected) in &manifest.entries {
                if Path::new(path)
                    .file_name()
                    .is_some_and(|name| name == ".gitignore")
                    || path == ".git/index"
                    || path == ".git/config"
                {
                    let (_, mut actual, _) = scan_entry(&tree, &tree.join(path))?;
                    actual.link_group = expected.link_group.clone();
                    if &actual != expected {
                        return Err(Error::Backend(
                            "legacy history scope integrity check failed".into(),
                        ));
                    }
                }
            }
            let scope = SourceScope::capture(&tree)?;
            let matcher = scope.matcher(&tree)?;
            let mut selected = BTreeSet::from([".".to_string()]);
            for (path, entry) in &manifest.entries {
                if entry.kind == "directory" || !matcher.includes(&tree.join(path), false) {
                    continue;
                }
                selected.insert(path.clone());
                let mut parent = Path::new(path).parent();
                while let Some(path) = parent {
                    if path.as_os_str().is_empty() {
                        break;
                    }
                    selected.insert(path.to_str().expect("validated manifest path").into());
                    parent = path.parent();
                }
            }
            manifest.entries.retain(|path, _| selected.contains(path));
            normalize_links(&mut manifest.entries);
            manifest.scope = Some(scope);
            manifest.version.entry_count = manifest.entries.len().saturating_sub(1);
            manifest.version.logical_bytes = manifest_logical_bytes(&manifest.entries)?;
        }
        if manifest.version.entry_count != manifest.entries.len() - 1
            || manifest.version.logical_bytes != manifest_logical_bytes(&manifest.entries)?
        {
            return Err(Error::Backend("history manifest aggregate counters mismatch".into()));
        }
        manifest
            .scope
            .as_ref()
            .expect("source scope")
            .matcher(&self.project)?;
        Ok(manifest)
    }

    pub fn scope(&self, id: &str) -> Result<SourceScope> {
        Ok(self.manifest(id)?.scope.expect("source scope"))
    }

    pub fn get(&self, id: &str) -> Result<HistoryVersion> {
        Ok(self.manifest(id)?.version)
    }

    pub fn tree(&self, id: &str) -> Result<PathBuf> {
        let manifest = self.manifest(id)?;
        let path = self.version_dir(id)?.join("tree");
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Backend(format!(
                "history snapshot integrity check failed: {id}"
            )));
        }
        let actual = if manifest.format == 2 {
            // A scoped capture contains only its manifest entries. Verify the
            // entire stored tree so injected ignored files, empty directories,
            // and engine/Git paths cannot disappear behind the source filter.
            let paths = walkdir::WalkDir::new(&path)
                .follow_links(false)
                .follow_root_links(false)
                .into_iter()
                .map(|item| {
                    item.map(|entry| entry.into_path())
                        .map_err(|error| Error::Io(std::io::Error::other(error)))
                })
                .collect::<Result<Vec<_>>>()?;
            scan_paths(&path, paths, 8)?
        } else {
            // Legacy full-tree captures intentionally retain ignored data and
            // must continue to verify only the projected source-file version.
            scan_scoped(&path, manifest.scope.as_ref().expect("source scope"))?
        };
        if actual != manifest.entries {
            return Err(Error::Backend(format!(
                "history snapshot integrity check failed: {id}"
            )));
        }
        Ok(path)
    }

    pub fn verify_expected(&self, id: &str) -> Result<()> {
        let manifest = self.manifest(id)?;
        let live = scan_scoped(
            &self.project,
            manifest.scope.as_ref().expect("source scope"),
        )?;
        let expected = manifest.entries;
        let conflicts: Vec<_> = changed_paths(&expected, &live)
            .into_iter()
            .map(|path| PathConflict {
                host_change: change_kind(expected.contains_key(&path), live.contains_key(&path)),
                path,
            })
            .collect();
        if !conflicts.is_empty() {
            return Err(Error::ProjectConflict(ConflictReport::new(
                format!("history restore conflict: live project differs from expected version {id}; no files were restored"), conflicts)));
        }
        Ok(())
    }

    pub fn diff(&self, id: &str) -> Result<DiffResult> {
        let manifest = self.manifest(id)?;
        let tree = self.tree(id)?;
        let live = scan_scoped(
            &self.project,
            manifest.scope.as_ref().expect("source scope"),
        )?;
        let target = manifest.entries;
        let changes = changed_paths(&live, &target)
            .into_iter()
            .map(|path| {
                let old = live.get(&path);
                let new = target.get(&path);
                let old_bytes = diff_bytes(&self.project.join(&path), old)?;
                let new_bytes = diff_bytes(&tree.join(&path), new)?;
                Ok(describe_change(
                    &path,
                    change_kind(old.is_some(), new.is_some()),
                    old.map(|entry| entry.size),
                    new.map(|entry| entry.size),
                    old_bytes.as_deref(),
                    new_bytes.as_deref(),
                    DEFAULT_TEXT_SIZE_LIMIT,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DiffResult {
            from: None,
            to: Some(id.into()),
            pending_changes: changes.len(),
            changes,
        })
    }
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id) {
        Ok(())
    } else {
        Err(Error::Backend(
            "invalid history/session ID: expected a canonical UUID".into(),
        ))
    }
}

fn changed_paths(before: &Index, after: &Index) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect()
}

/// Recovery must not overwrite edits made after the owner died. Known complete
/// pre/post trees can be rolled back automatically; an ambiguous partial tree
/// is kept intact for inspection along with the durable undo transaction.
pub(crate) fn verify_recovery_live(project: &Path, before: &Path, staged: &Path) -> Result<()> {
    let live = scan(project)?;
    let before = scan(before)?;
    let staged = scan(staged)?;
    if live == before || live == staged {
        return Ok(());
    }
    let conflicts = changed_paths(&before, &live)
        .into_iter()
        .map(|path| PathConflict {
            host_change: change_kind(before.contains_key(&path), live.contains_key(&path)),
            path,
        })
        .collect();
    Err(Error::ProjectConflict(ConflictReport::new(
        "history recovery conflict: live project matches neither the pre-restore nor target tree; preserve and reconcile post-crash edits before retrying; current files and the durable recovery transaction were left intact",
        conflicts,
    )))
}

fn change_kind(before: bool, after: bool) -> ChangeKind {
    match (before, after) {
        (false, true) => ChangeKind::Added,
        (true, false) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    }
}

fn diff_bytes(path: &Path, entry: Option<&Entry>) -> Result<Option<Vec<u8>>> {
    match entry {
        Some(entry) if entry.kind == "file" && entry.size <= DEFAULT_TEXT_SIZE_LIMIT => {
            Ok(Some(fs::read(path)?))
        }
        Some(entry) if entry.kind == "symlink" => {
            Ok(Some(fs::read_link(path)?.as_os_str().as_bytes().to_vec()))
        }
        _ => Ok(None),
    }
}

fn scan(root: &Path) -> Result<Index> {
    scan_with_workers(
        root,
        std::thread::available_parallelism()
            .map_or(1, |count| count.get())
            .min(8),
    )
}

fn scan_with_workers(root: &Path, workers: usize) -> Result<Index> {
    let paths = walkdir::WalkDir::new(root)
        .min_depth(0)
        .follow_links(false)
        .into_iter()
        .filter_entry(|item| item.depth() != 1 || item.file_name() != ".deltabox")
        .map(|item| {
            item.map(|entry| entry.into_path())
                .map_err(|error| Error::Io(std::io::Error::other(error)))
        })
        .collect::<Result<Vec<_>>>()?;
    scan_paths(root, paths, workers)
}

fn scan_scoped(root: &Path, scope: &SourceScope) -> Result<Index> {
    scan_paths(root, scope.paths(root)?, 8)
}

fn scan_paths(root: &Path, paths: Vec<PathBuf>, workers: usize) -> Result<Index> {
    // Bound concurrent opens on large workspaces. Every pass still hashes all
    // bytes and validates the same metadata, including the cloned snapshot.
    let workers = workers.max(1).min(paths.len().max(1));
    let entries = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|worker| {
                let paths = &paths;
                scope.spawn(move || {
                    paths
                        .iter()
                        .skip(worker)
                        .step_by(workers)
                        .map(|path| scan_entry(root, path))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| Error::Backend("history scan worker panicked".into()))?
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let mut result = Index::new();
    let mut hardlinks: HashMap<(u64, u64), Vec<String>> = HashMap::new();
    for (relative, entry, identity) in entries.into_iter().flatten() {
        if let Some(identity) = identity {
            hardlinks
                .entry(identity)
                .or_default()
                .push(relative.clone());
        }
        result.insert(relative, entry);
    }
    for paths in hardlinks.values_mut().filter(|paths| paths.len() > 1) {
        paths.sort();
        let representative = paths[0].clone();
        for path in paths {
            result.get_mut(path).expect("indexed file").link_group = Some(representative.clone());
        }
    }
    Ok(result)
}

fn scan_entry(root: &Path, path: &Path) -> Result<(String, Entry, Option<(u64, u64)>)> {
    let relative = path
        .strip_prefix(root)
        .expect("walk stays below root")
        .to_str()
        .ok_or_else(|| Error::Unsupported("history requires UTF-8 project paths".into()))?;
    let relative = if relative.is_empty() { "." } else { relative }.to_string();
    let metadata = fs::symlink_metadata(path)?;
    if metadata.st_flags()
        & (libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND)
        != 0
    {
        return Err(Error::Unsupported(format!(
            "history v1 cannot capture or restore immutable/append-only flags: {relative}; no history mutation was started",
        )));
    }
    let (kind, size, digest) = if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        ("file", metadata.len(), format!("{:x}", hasher.finalize()))
    } else if metadata.is_dir() {
        ("directory", 0, String::new())
    } else if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        (
            "symlink",
            target.as_os_str().as_bytes().len() as u64,
            format!("{:x}", Sha256::digest(target.as_os_str().as_bytes())),
        )
    } else {
        return Err(Error::Unsupported(format!(
            "history cannot capture special file: {relative}"
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
    let entry = Entry {
        kind: kind.into(),
        size,
        digest,
        link_group: None,
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        flags: metadata.st_flags(),
        acl: read_acl(path)?,
        xattrs,
    };
    Ok((
        relative,
        entry,
        metadata.is_file().then(|| (metadata.dev(), metadata.ino())),
    ))
}

fn normalize_links(entries: &mut Index) {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, entry) in entries.iter_mut() {
        if let Some(group) = entry.link_group.take() {
            groups.entry(group).or_default().push(path.clone());
        }
    }
    for paths in groups.values().filter(|paths| paths.len() > 1) {
        for path in paths {
            entries.get_mut(path).unwrap().link_group = Some(paths[0].clone());
        }
    }
}

fn clone_selected(source: &Path, destination: &Path, entries: &Index) -> Result<()> {
    fs::create_dir(destination)?;
    let mut links = HashMap::new();
    for (path, entry) in entries {
        if path == "." {
            continue;
        }
        if entry.kind == "directory" {
            fs::create_dir(destination.join(path))?;
        } else {
            clone_path(&source.join(path), &destination.join(path), &mut links)?;
        }
    }
    for (path, entry) in entries.iter().rev() {
        if entry.kind == "directory" {
            copy_metadata(&source.join(path), &destination.join(path))?;
        }
    }
    Ok(())
}

/// Merge only versioned paths into a private full-tree staging copy. Ignored
/// children are never recursively removed, even during directory/file swaps.
pub(crate) fn apply_source_tree(
    source: &Path,
    destination: &Path,
    scope: &SourceScope,
) -> Result<()> {
    let target = scan_scoped(source, scope)?;
    let live = scan_scoped(destination, scope)?;
    for (path, entry) in &live {
        if entry.kind == "directory" {
            fs::set_permissions(
                destination.join(path),
                fs::Permissions::from_mode(entry.mode | 0o700),
            )?;
        }
    }
    for (path, entry) in live.iter().rev() {
        if path == "." {
            continue;
        }
        let wanted = target.get(path);
        if entry.kind == "directory" {
            if wanted.is_some_and(|entry| entry.kind == "directory") {
                continue;
            }
            match fs::remove_dir(destination.join(path)) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::DirectoryNotEmpty
                        && wanted.is_none() =>
                {
                    // The container now holds only excluded data; retain its metadata.
                    fs::set_permissions(
                        destination.join(path),
                        fs::Permissions::from_mode(entry.mode),
                    )?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    return Err(Error::Backend(format!("history restore would remove ignored data inside {path}; current project was left intact")));
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            fs::remove_file(destination.join(path))?;
        }
    }
    let mut links = HashMap::new();
    for (path, entry) in &target {
        if path == "." {
            continue;
        }
        let dest = destination.join(path);
        if entry.kind == "directory" {
            match fs::symlink_metadata(&dest) {
                Ok(metadata) if metadata.is_dir() => {
                    fs::set_permissions(
                        &dest,
                        fs::Permissions::from_mode(metadata.mode() | 0o700),
                    )?;
                }
                Ok(_) => {
                    return Err(Error::Backend(format!(
                        "history restore conflicts with excluded path: {path}"
                    )))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&dest)?
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            // An ignored-only directory is absent from the managed index.
            if fs::symlink_metadata(&dest).is_ok() {
                return Err(Error::Backend(format!("history restore conflicts with excluded data: {path}; current project was left intact")));
            }
            clone_path(&source.join(path), &dest, &mut links)?;
        }
    }
    for (path, entry) in target.iter().rev() {
        if entry.kind == "directory" {
            copy_metadata(&source.join(path), &destination.join(path))?;
        }
    }
    if scan_scoped(destination, scope)? != target {
        return Err(Error::Backend(
            "source history staging verification failed".into(),
        ));
    }
    Ok(())
}

pub(crate) fn clone_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut timings = RestoreTimings::new();
    clone_tree_contents(source, destination)?;
    timings.mark("clone_tree.copy");
    sync_tree(destination)?;
    timings.mark("clone_tree.sync");
    Ok(())
}

fn clone_tree_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    let mut links = HashMap::new();
    for item in fs::read_dir(source)? {
        let item = item?;
        if item.file_name() != ".deltabox" {
            clone_path(
                &item.path(),
                &destination.join(item.file_name()),
                &mut links,
            )?;
        }
    }
    copy_metadata(source, destination)
}

/// Replace only top-level entries, never walking through live symlink parents.
/// The engine directory is excluded from both replacement and deletion.
pub(crate) fn apply_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut timings = RestoreTimings::new();
    // Root permissions may themselves be part of the restore (including a
    // previous read-only root). Make the owner able to replace its entries;
    // the final source metadata below restores the requested exact permissions.
    let mut permissions = fs::metadata(destination)?.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    fs::set_permissions(destination, permissions)?;
    for item in fs::read_dir(destination)? {
        let item = item?;
        if item.file_name() != ".deltabox" {
            match fs::symlink_metadata(source.join(item.file_name())) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    crate::macos_tree::remove_path(&item.path())?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut links = HashMap::new();
    for item in fs::read_dir(source)? {
        let item = item?;
        if item.file_name() != ".deltabox" {
            crate::macos_tree::replace_path(
                &item.path(),
                &destination.join(item.file_name()),
                &mut links,
            )?;
        }
    }
    copy_metadata(source, destination)?;
    timings.mark("apply_tree.replace");
    sync_tree(destination)?;
    timings.mark("apply_tree.sync");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_open_does_not_create_missing_history_and_checks_snapshot_integrity() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        assert!(HistoryStore::open_existing(storage.path(), project.path()).is_err());
        assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
        fs::write(project.path().join("source"), "old").unwrap();
        let writer = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = writer.capture("test", None).unwrap();
        fs::write(project.path().join("source"), "new").unwrap();
        let reader = HistoryStore::open_existing(storage.path(), project.path()).unwrap();
        assert_eq!(reader.diff(&version.id).unwrap().pending_changes, 1);
        fs::write(writer.root.join(&version.id).join("tree/source"), "corrupt").unwrap();
        assert!(reader.diff(&version.id).unwrap_err().to_string().contains("integrity"));
        assert_eq!(fs::read_to_string(project.path().join("source")).unwrap(), "new");
    }

    #[test]
    fn source_restore_preserves_excluded_data_and_keeps_untracked_source() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = project.path();
        fs::write(root.join(".gitignore"), "target/\n*.log\n.env\n").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "original ref").unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/artifact"), "original build").unwrap();
        fs::write(root.join("source.rs"), "original source").unwrap();
        let store = HistoryStore::open(storage.path(), root).unwrap();
        let version = store.capture("test", None).unwrap();
        assert_eq!(version.entry_count, 2);
        assert!(!store.tree(&version.id).unwrap().join("target").exists());
        fs::write(root.join("target/artifact"), "new build").unwrap();
        fs::write(root.join(".git/HEAD"), "new ref").unwrap();
        fs::write(root.join(".env"), "local secret").unwrap();
        store.verify_expected(&version.id).unwrap();
        fs::write(root.join("source.rs"), "modified").unwrap();
        fs::create_dir(root.join("mixed")).unwrap();
        fs::write(root.join("mixed/new.rs"), "remove this").unwrap();
        fs::write(root.join("mixed/output.log"), "retain this").unwrap();
        fs::write(root.join(".gitignore"), "source.rs\n").unwrap();
        let tree = store.tree(&version.id).unwrap();
        apply_source_tree(&tree, root, &store.scope(&version.id).unwrap()).unwrap();
        store.verify_expected(&version.id).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("source.rs")).unwrap(),
            "original source"
        );
        assert_eq!(
            fs::read_to_string(root.join("target/artifact")).unwrap(),
            "new build"
        );
        assert_eq!(
            fs::read_to_string(root.join(".git/HEAD")).unwrap(),
            "new ref"
        );
        assert_eq!(
            fs::read_to_string(root.join(".env")).unwrap(),
            "local secret"
        );
        assert_eq!(
            fs::read_to_string(root.join("mixed/output.log")).unwrap(),
            "retain this"
        );
        assert!(!root.join("mixed/new.rs").exists());
    }

    #[test]
    fn source_restore_rejects_type_swap_over_ignored_children() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(project.path().join("node"), "old file").unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = store.capture("test", None).unwrap();
        fs::remove_file(project.path().join("node")).unwrap();
        fs::create_dir(project.path().join("node")).unwrap();
        fs::write(project.path().join("node/output.log"), "excluded").unwrap();
        let staging = storage.path().join("staging");
        clone_tree(project.path(), &staging).unwrap();
        let error = apply_source_tree(
            &store.tree(&version.id).unwrap(),
            &staging,
            &store.scope(&version.id).unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("excluded data"));
        assert_eq!(
            fs::read_to_string(project.path().join("node/output.log")).unwrap(),
            "excluded"
        );
    }

    #[test]
    fn legacy_full_versions_are_projected_without_rewriting_the_original() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join(".gitignore"), "target/\n").unwrap();
        fs::write(project.path().join("source.rs"), "v1").unwrap();
        fs::create_dir(project.path().join("target")).unwrap();
        fs::write(project.path().join("target/build"), "old build").unwrap();
        fs::hard_link(
            project.path().join("source.rs"),
            project.path().join("target/alias"),
        )
        .unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = store.capture("test", None).unwrap();
        let dir = store.version_dir(&version.id).unwrap();
        let mut legacy = store.manifest(&version.id).unwrap();
        crate::macos_tree::remove_path(&dir.join("tree")).unwrap();
        clone_tree(project.path(), &dir.join("tree")).unwrap();
        legacy.entries = scan(&dir.join("tree")).unwrap();
        legacy.format = 1;
        legacy.scope = None;
        let bytes = serde_json::to_vec(&legacy).unwrap();
        fs::write(dir.join("manifest.json"), &bytes).unwrap();
        fs::write(project.path().join("target/build"), "new build").unwrap();
        fs::write(project.path().join("source.rs"), "v2").unwrap();
        apply_source_tree(
            &store.tree(&version.id).unwrap(),
            project.path(),
            &store.scope(&version.id).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(project.path().join("source.rs")).unwrap(),
            "v1"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("target/build")).unwrap(),
            "new build"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("target/alias")).unwrap(),
            "v2"
        );
        store.verify_expected(&version.id).unwrap();
        assert_eq!(fs::read(dir.join("manifest.json")).unwrap(), bytes);
        assert_eq!(
            fs::read_to_string(dir.join("tree/target/build")).unwrap(),
            "old build"
        );
        // A missing managed legacy file must not be silently filtered away.
        fs::remove_file(dir.join("tree/source.rs")).unwrap();
        assert!(store.tree(&version.id).is_err());
    }

    #[test]
    fn parallel_scans_preserve_serial_semantics_and_reject_special_files() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("nested/.deltabox")).unwrap();
        fs::create_dir(project.path().join(".deltabox")).unwrap();
        fs::write(project.path().join(".deltabox/ignored"), "engine").unwrap();
        for index in 0..65 {
            fs::write(
                project.path().join(format!("nested/file-{index}")),
                vec![index as u8; 4096],
            )
            .unwrap();
        }
        let source = project.path().join("nested/file-0");
        fs::hard_link(&source, project.path().join("peer")).unwrap();
        xattr::set(&source, "com.deltabox.history-test", b"preserved").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("missing", project.path().join("link")).unwrap();
        let serial = scan_with_workers(project.path(), 1).unwrap();
        assert_eq!(serial, scan_with_workers(project.path(), 8).unwrap());
        assert!(serial.contains_key("nested/.deltabox"));
        assert!(!serial.contains_key(".deltabox"));
        fs::write(&source, "changed bytes").unwrap();
        assert_ne!(serial, scan_with_workers(project.path(), 8).unwrap());
        let _socket =
            std::os::unix::net::UnixListener::bind(project.path().join("socket")).unwrap();
        for workers in [1, 8] {
            assert!(matches!(
                scan_with_workers(project.path(), workers),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn fast_capture_remains_readable_after_reopening_with_either_durability() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join("a"), b"binary\0contents").unwrap();
        let store =
            HistoryStore::open_with_durability(storage.path(), project.path(), false).unwrap();
        let version = store.capture("sid", None).unwrap();
        drop(store);
        for durable in [false, true] {
            let reopened =
                HistoryStore::open_with_durability(storage.path(), project.path(), durable)
                    .unwrap();
            assert_eq!(reopened.list().unwrap().len(), 1);
            assert_eq!(reopened.diff(&version.id).unwrap().pending_changes, 0);
            assert_eq!(
                fs::read(reopened.tree(&version.id).unwrap().join("a")).unwrap(),
                b"binary\0contents"
            );
        }
    }

    #[test]
    fn hardlink_topology_is_semantic_across_clones_and_conflicts_after_unlink() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join("a"), "same bytes").unwrap();
        fs::hard_link(project.path().join("a"), project.path().join("b")).unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = store.capture("sid", None).unwrap();
        store.verify_expected(&version.id).unwrap();
        fs::remove_file(project.path().join("b")).unwrap();
        fs::write(project.path().join("b"), "same bytes").unwrap();
        assert!(matches!(
            store.verify_expected(&version.id),
            Err(Error::ProjectConflict(_))
        ));
        let diff = store.diff(&version.id).unwrap();
        assert!(!diff.changes.is_empty());
        assert!(diff.changes.iter().any(|change| change.path == "b"));
        apply_tree(&store.tree(&version.id).unwrap(), project.path()).unwrap();
        store.verify_expected(&version.id).unwrap();
    }

    #[test]
    fn root_directory_permissions_and_xattrs_are_checked_and_restored() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::set_permissions(project.path(), fs::Permissions::from_mode(0o750)).unwrap();
        xattr::set(
            project.path(),
            "com.deltabox.history-test",
            b"root-original",
        )
        .unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = store.capture("sid", None).unwrap();
        assert_eq!(version.entry_count, 0);
        fs::set_permissions(project.path(), fs::Permissions::from_mode(0o700)).unwrap();
        xattr::set(project.path(), "com.deltabox.history-test", b"root-changed").unwrap();
        assert!(matches!(
            store.verify_expected(&version.id),
            Err(Error::ProjectConflict(_))
        ));
        assert_eq!(store.diff(&version.id).unwrap().changes[0].path, ".");
        apply_tree(&store.tree(&version.id).unwrap(), project.path()).unwrap();
        store.verify_expected(&version.id).unwrap();
        assert_eq!(fs::metadata(project.path()).unwrap().mode() & 0o777, 0o750);
        assert_eq!(
            xattr::get(project.path(), "com.deltabox.history-test")
                .unwrap()
                .unwrap(),
            b"root-original"
        );
        // Restore remains usable after restoring a read-only project root.
        fs::write(project.path().join("a"), "read-only tree").unwrap();
        fs::set_permissions(project.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let read_only = store.capture("sid", None).unwrap();
        apply_tree(&store.tree(&version.id).unwrap(), project.path()).unwrap();
        assert!(!project.path().join("a").exists());
        apply_tree(&store.tree(&read_only.id).unwrap(), project.path()).unwrap();
        store.verify_expected(&read_only.id).unwrap();
        assert_eq!(fs::metadata(project.path()).unwrap().mode() & 0o777, 0o500);
        fs::set_permissions(project.path(), fs::Permissions::from_mode(0o700)).unwrap();
        // Allow the temporary test store to remove its read-only fixture.
        fs::set_permissions(
            store.tree(&read_only.id).unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    #[test]
    fn restrictive_flags_fail_before_capture_without_leaking_a_version() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = project.path().join("locked");
        fs::write(&file, "unchanged").unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("uchg")
            .arg(&file)
            .status()
            .unwrap()
            .success());
        let captured = store.capture("sid", None);
        let leaked = fs::read_dir(&store.root).unwrap().count();
        let bytes = fs::read(&file).unwrap();
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("nouchg")
            .arg(&file)
            .status()
            .unwrap()
            .success());
        assert!(matches!(captured, Err(Error::Unsupported(_))));
        assert_eq!(leaked, 0);
        assert_eq!(bytes, b"unchanged");
    }

    #[test]
    fn capture_detects_external_bytes_modes_and_metadata_and_never_prunes() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = project.path().join("binary");
        fs::write(&file, [0, 1, 2, 3]).unwrap();
        let history = HistoryStore::open(storage.path(), project.path()).unwrap();
        let initial = history.capture("sid", None).unwrap();
        fs::write(&file, [0, 1, 2, 4]).unwrap();
        assert!(matches!(
            history.verify_expected(&initial.id),
            Err(Error::ProjectConflict(_))
        ));
        fs::write(&file, [0, 1, 2, 3]).unwrap();
        history.verify_expected(&initial.id).unwrap();
        let permissions = fs::metadata(&file).unwrap().permissions();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(history.verify_expected(&initial.id).is_err());
        fs::set_permissions(&file, permissions).unwrap();
        xattr::set(&file, "com.deltabox.history-test", b"external").unwrap();
        assert!(history.verify_expected(&initial.id).is_err());
        xattr::remove(&file, "com.deltabox.history-test").unwrap();
        for _ in 0..102 {
            history.capture("new-session", None).unwrap();
        }
        assert_eq!(history.list().unwrap().len(), 103);
        assert!(history.tree(&initial.id).unwrap().join("binary").exists());
    }

    #[test]
    fn store_rejects_inside_project_symlink_aliases_unsafe_ids_and_tampering() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        assert!(HistoryStore::open(&project.path().join("inside"), project.path()).is_err());
        let alias = storage.path().join("alias");
        std::os::unix::fs::symlink(project.path(), &alias).unwrap();
        assert!(HistoryStore::open(&alias, project.path()).is_err());
        fs::write(project.path().join("a"), "hello").unwrap();
        let history = HistoryStore::open(storage.path(), project.path()).unwrap();
        let v = history.capture("sid", None).unwrap();
        for id in ["../outside", "/tmp", "", "a/b"] {
            assert!(history.get(id).is_err());
        }
        fs::write(history.root.join(&v.id).join("tree/a"), "tampered").unwrap();
        assert!(history.tree(&v.id).is_err());
        fs::create_dir(history.root.join(".capture-interrupted")).unwrap();
        assert_eq!(history.list().unwrap().len(), 1);
        let mut manifest = history.manifest(&v.id).unwrap();
        let entry = manifest.entries.remove("a").unwrap();
        manifest.entries.insert("../../outside".into(), entry);
        fs::write(
            history.root.join(v.id).join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(history.list().is_err());
    }

    #[test]
    fn full_restore_preserves_binary_links_permissions_and_engine_state() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("untouched"), "outside").unwrap();
        fs::create_dir(project.path().join("folder")).unwrap();
        fs::write(project.path().join("folder/binary"), [0, 255, 42]).unwrap();
        fs::hard_link(
            project.path().join("folder/binary"),
            project.path().join("peer"),
        )
        .unwrap();
        fs::set_permissions(
            project.path().join("folder/binary"),
            fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        xattr::set(
            project.path().join("folder/binary"),
            "com.deltabox.history-test",
            b"keep",
        )
        .unwrap();
        std::os::unix::fs::symlink("missing", project.path().join("dangling")).unwrap();
        fs::create_dir(project.path().join(".deltabox")).unwrap();
        fs::write(project.path().join(".deltabox/engine"), "original").unwrap();
        let history = HistoryStore::open(storage.path(), project.path()).unwrap();
        let v = history.capture("sid", None).unwrap();
        assert_eq!(v.entry_count, 4);
        assert!(!history.tree(&v.id).unwrap().join(".deltabox").exists());
        fs::remove_dir_all(project.path().join("folder")).unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join("folder")).unwrap();
        fs::remove_file(project.path().join("dangling")).unwrap();
        fs::create_dir(project.path().join("dangling")).unwrap();
        fs::write(project.path().join("dangling/extra"), "delete").unwrap();
        fs::write(project.path().join("extra"), "delete").unwrap();
        fs::write(project.path().join(".deltabox/engine"), "latest").unwrap();
        apply_tree(&history.tree(&v.id).unwrap(), project.path()).unwrap();
        history.verify_expected(&v.id).unwrap();
        assert_eq!(
            fs::read(project.path().join("folder/binary")).unwrap(),
            [0, 255, 42]
        );
        assert_eq!(
            fs::metadata(project.path().join("folder/binary"))
                .unwrap()
                .ino(),
            fs::metadata(project.path().join("peer")).unwrap().ino()
        );
        assert_eq!(
            fs::read_link(project.path().join("dangling")).unwrap(),
            Path::new("missing")
        );
        assert!(!project.path().join("extra").exists());
        assert_eq!(
            fs::read_to_string(outside.path().join("untouched")).unwrap(),
            "outside"
        );
        assert_eq!(
            fs::read_to_string(project.path().join(".deltabox/engine")).unwrap(),
            "latest"
        );
    }
}

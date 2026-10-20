//! Windows copy-private: commands run in a private copy of the project.
//!
//! Layout. The session store `<state>\projects\<key>\<id>` holds `session.json`
//! (a [`Manifest`] with `fs_mode = CopyPrivate`), `baseline.json` (the host
//! fingerprints shared with `private.rs`), `accepted\` (a pristine copy of the
//! accepted tree that commands never see), `accepted.json` (its scan) and
//! `checkpoints\<id>\`. The private working copy lives outside the state tree
//! at `<runtime>\live\<id>`, so the sandbox can deny the whole state tree
//! without a writable grant nested inside it.
//!
//! There is no write interception. Changes are found by scanning and comparing
//! content fingerprints. Commit publishes through the cbcow-private transaction
//! in `private.rs`. Exec, abort and commit write `pending.json` before they
//! change anything, and every operation first finishes or reverts an
//! interrupted step.
//!
//! Differences from Unix copy-private, on purpose:
//! - Files matching deny patterns and `.deltabox` are never copied out of the
//!   project, so commands cannot read them (tracked ones are still in the
//!   shadow `.git` objects, as on Unix). A denied path that a command
//!   recreates with exactly the host's content, as `git checkout` does for a
//!   tracked `.env.example`, is tolerated but never diffed or published.
//! - Reparse points (symlinks, junctions, and cloud placeholders such as
//!   OneDrive online-only files) are refused, as in cbcow-private. That
//!   includes one put in place of `.git` or `.deltabox`, which the scans
//!   otherwise skip.
//! - `commit` with a checkpoint publishes the working tree as it is, and only
//!   the latest checkpoint may be named.
//! - A working copy that goes missing is rebuilt only by `abort`; every other
//!   operation reports it missing rather than treating it as empty.
//! - The shadow `.git` is copied so git works in the private tree. It is never
//!   compared or published, and it follows the host's `.git` after a commit.
//!   Checkpoints and exec rollback restore the root `.git` without
//!   `objects\`; objects deleted by `git gc` or `git prune` are not brought
//!   back, and a nested `.git` (a submodule) is neither snapshotted nor
//!   restored.
//! - A case-only rename of a directory is applied to the private copies but
//!   not to the host, where the publication keeps the existing spelling.
//! - Comparisons between the private copies ignore attribute bits a copy
//!   cannot reproduce (compression, sparseness, encryption).
//! - Names ending in `.deltabox-theirs` are reserved for cbcow-private sync
//!   drafts and refused, because the publication never publishes them.
//! - `fs.write` onto an existing directory is refused instead of replacing it.
//! - There is no journal and no publog; publications are recorded for sibling
//!   cbcow-private sessions by `private.rs`.
//! - Like cbcow-private, a running copy-private session and a through session
//!   exclude each other.
use super::engine::{now_ms, remove, wide, write_json, Manifest, Metadata, Savepoint};
use super::helper::lock;
use super::private::{self, Baseline, Fingerprint};
use crate::changeset::{describe_change, Change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{ConflictReport, PathConflict};
use crate::policy::Policy;
use crate::protocol::{
    AbortResult, ActiveReview, CheckpointInfo, CommitResult, DiffResult, ExecParams, ExecResult,
    FilesystemInfo, FilesystemMode, FsReadResult, ReviewKind, SessionInfo, SessionStatus,
};
use crate::{Error, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

const PENDING: &str = "pending.json";
const MAX_FS_READ_BYTES: u64 = 16 * 1024 * 1024;
const CHECKPOINT_CAP: usize = 100;
const DEFAULT_STORAGE_LIMIT: u64 = 1024 * 1024 * 1024;
const DEFAULT_WRITE_LIMIT: u64 = 16 * 1024 * 1024;
/// Attribute bits a copy can reproduce with SetFileAttributesW. Compression,
/// sparseness and encryption follow the destination folder instead.
const RESTORABLE_ATTRIBUTES: u32 = 0x1 | 0x2 | 0x4 | 0x10 | 0x20 | 0x100 | 0x2000;
/// Marks a reparse point (symlink or junction) in a scan.
const REPARSE_DIGEST: &str = "reparse-point";

/// Exit at a named point in debug builds, so tests can interrupt a step.
fn crash_point(point: &str) {
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_POINT").as_deref() == Ok(point) {
        std::process::exit(86);
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// One scanned path: its original-case relative spelling and its fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    path: String,
    fingerprint: Fingerprint,
}

/// A scan keyed by the lower-case `\` key that `private.rs` uses.
type Tree = BTreeMap<String, Entry>;

/// Never compared, published or restored as ordinary files: DeltaBox
/// metadata and the shadow `.git`.
fn skipped(key: &str) -> bool {
    key.split('\\').any(|part| part == ".deltabox" || part == ".git")
}

fn draft_error(path: &str) -> Error {
    Error::InvalidPolicy(format!("names ending in .deltabox-theirs are reserved for cbcow-private sync drafts: {}", slash(path)))
}

/// Deny patterns compare case-insensitively with `/` separators on Windows.
/// A path under a denied directory is denied too: the scans prune whole
/// subtrees, so every other check must see the same set.
fn denied(policy: &Policy, key: &str) -> bool {
    let path = key.replace('\\', "/");
    let patterns: Vec<String> = policy.filesystem.deny.iter().map(|pattern| pattern.replace('\\', "/").to_lowercase()).collect();
    let mut end = path.len();
    loop {
        let prefix = &path[..end];
        if patterns.iter().any(|pattern| crate::policy::deny_matches(pattern, prefix)) {
            return true;
        }
        match prefix.rfind('/') {
            Some(index) => end = index,
            None => return false,
        }
    }
}

/// The view that diffs, conflicts and publication compare.
fn tracked(policy: &Policy) -> impl Fn(&str) -> bool + '_ {
    move |key| skipped(key) || denied(policy, key)
}

/// The paths users see in diffs and conflicts use `/`.
fn slash(path: &str) -> String {
    path.replace('\\', "/")
}

fn relative_spelling(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|_| Error::Backend("scan escaped its root".into()))?;
    relative
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Unsupported(format!("non-Unicode Windows path: {}", relative.display())))
}

/// Like `private::fingerprint`, but a reparse point becomes a marker entry
/// instead of an error. A command can create one in the working copy; the
/// scan must still succeed so the command can be rejected and undone.
fn entry_fingerprint(path: &Path) -> Result<Fingerprint> {
    let m = fs::symlink_metadata(path)?;
    if m.file_attributes() & 0x400 != 0 {
        return Ok(Fingerprint {
            directory: false,
            attributes: m.file_attributes(),
            creation: m.creation_time(),
            write: m.last_write_time(),
            size: 0,
            digest: REPARSE_DIGEST.into(),
        });
    }
    private::fingerprint(path)
}

/// Fingerprint every path under `root`, skipping the subtrees `prune` names.
/// `exact` keeps every attribute bit, as `private.rs` compares host scans;
/// otherwise bits a copy cannot reproduce are cleared. A missing root is an
/// error: an empty scan of a vanished working copy would otherwise look like
/// the deletion of every file.
fn scan_with(root: &Path, prune: &dyn Fn(&str) -> bool, exact: bool) -> Result<Tree> {
    plain_root(root)?;
    let mut out = Tree::new();
    let walker = walkdir::WalkDir::new(root).min_depth(1).follow_links(false).into_iter().filter_entry(|entry| {
        match relative_spelling(root, entry.path()) {
            Ok(path) => !prune(&private::key(&path)),
            Err(_) => true, // surfaced below as an error
        }
    });
    for entry in walker {
        let entry = entry.map_err(|e| Error::Backend(e.to_string()))?;
        let path = relative_spelling(root, entry.path())?;
        let mut fingerprint = entry_fingerprint(entry.path())?;
        if !exact {
            fingerprint.attributes &= RESTORABLE_ATTRIBUTES;
        }
        out.insert(private::key(&path), Entry { fingerprint, path });
    }
    Ok(out)
}

/// A scan of one of the private copies.
fn scan(root: &Path, prune: &dyn Fn(&str) -> bool) -> Result<Tree> {
    scan_with(root, prune, false)
}

/// `root` must be a plain directory: present, and not a reparse point. A
/// missing working copy must never scan as empty (that would look like the
/// deletion of every file), and a junction must never be walked through.
fn plain_root(root: &Path) -> Result<()> {
    let m = fs::symlink_metadata(root)
        .map_err(|e| Error::Backend(format!("copy-private tree is missing: {} ({e})", root.display())))?;
    if !m.is_dir() || m.file_attributes() & 0x400 != 0 {
        return Err(Error::Backend(format!("copy-private tree is not a plain directory: {}", root.display())));
    }
    Ok(())
}

/// Whether `path` exists and is a reparse point.
fn is_reparse(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(m.file_attributes() & 0x400 != 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn reparse_error(path: &str) -> Error {
    Error::Unsupported(format!("reparse points (symlinks, junctions) are not supported in Windows copy-private: {}", slash(path)))
}

/// A metadata-only scan of the shadow `.git` under `root` (empty when there
/// is none). Without `objects`, the `objects` directory is left out.
fn scan_git(root: &Path, objects: bool) -> Result<Tree> {
    let mut out = Tree::new();
    let git = root.join(".git");
    let Ok(m) = fs::symlink_metadata(&git) else {
        return Ok(out);
    };
    // walkdir follows a root link by default. A junction put in place of
    // `.git` must never be walked, let alone converged through.
    if m.file_attributes() & 0x400 != 0 {
        return Err(reparse_error(".git"));
    }
    let walker = walkdir::WalkDir::new(&git).follow_links(false).follow_root_links(false).into_iter().filter_entry(|entry| {
        // Object stores at any depth: `objects`, `modules\*\objects`, `lfs\objects`.
        objects || !(entry.file_type().is_dir() && entry.file_name().eq_ignore_ascii_case("objects"))
    });
    for entry in walker {
        let entry = entry.map_err(|e| Error::Backend(e.to_string()))?;
        let m = fs::symlink_metadata(entry.path())?;
        let path = relative_spelling(root, entry.path())?;
        let reparse = m.file_attributes() & 0x400 != 0;
        let fingerprint = Fingerprint {
            directory: m.is_dir() && !reparse,
            attributes: m.file_attributes() & RESTORABLE_ATTRIBUTES,
            creation: 0,
            write: if m.is_dir() { 0 } else { m.last_write_time() },
            size: if m.is_dir() { 0 } else { m.len() },
            digest: if reparse { REPARSE_DIGEST.into() } else { String::new() },
        };
        out.insert(private::key(&path), Entry { path, fingerprint });
    }
    Ok(out)
}

/// The same tree with the attribute bits a copy cannot reproduce cleared.
fn masked(tree: &Tree) -> Tree {
    tree.iter()
        .map(|(key, entry)| {
            let mut entry = entry.clone();
            entry.fingerprint.attributes &= RESTORABLE_ATTRIBUTES;
            (key.clone(), entry)
        })
        .collect()
}

/// Paths whose entries differ between two trees, except denied ones.
fn changed_keys(policy: &Policy, before: &Tree, after: &Tree) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key) && !denied(policy, key))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Copying and converging trees
// ---------------------------------------------------------------------------

fn reject_reparse(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_attributes() & 0x400 != 0 {
        return Err(Error::Unsupported(format!(
            "reparse points (symlinks, junctions, and cloud placeholders such as OneDrive online-only files) are not supported in Windows copy-private: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Copy `source` to `target`, skipping the children `prune` names (by their
/// key relative to the copy root). Times and attributes are restored after
/// the content, deepest first, exactly like `engine::copy_tree`, so the copy
/// fingerprints equal the source.
fn copy_filtered(source: &Path, target: &Path, relative: &Path, prune: &dyn Fn(&str) -> bool) -> Result<()> {
    let m = fs::symlink_metadata(source)?;
    reject_reparse(source, &m)?;
    if m.is_dir() {
        fs::create_dir_all(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            let child_relative = relative.join(child.file_name());
            let spelling = child_relative
                .to_str()
                .ok_or_else(|| Error::Unsupported(format!("non-Unicode Windows path: {}", child_relative.display())))?;
            if prune(&private::key(spelling)) {
                continue;
            }
            copy_filtered(&child.path(), &target.join(child.file_name()), &child_relative, prune)?;
        }
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }
    Metadata::read(source)?.restore(target)
}

/// Give an existing directory the exact spelling `target` asks for. NTFS
/// renames a directory to a different case of the same name in place.
fn match_case(target: &Path) -> Result<()> {
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Ok(());
    };
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let actual = entry.file_name();
        if actual != name && actual.to_string_lossy().to_lowercase() == name.to_string_lossy().to_lowercase() {
            fs::rename(entry.path(), target)?;
            break;
        }
    }
    Ok(())
}

/// Put one entry (a file, or a directory without its children) at `target`.
fn place(source: &Path, target: &Path) -> Result<()> {
    let m = fs::symlink_metadata(source)?;
    reject_reparse(source, &m)?;
    if m.is_dir() {
        match fs::symlink_metadata(target) {
            Ok(t) if t.file_attributes() & 0x10 != 0 && t.file_attributes() & 0x400 == 0 => match_case(target)?,
            Ok(_) => {
                remove(target)?;
                fs::create_dir_all(target)?;
            }
            Err(_) => fs::create_dir_all(target)?,
        }
    } else {
        remove(target)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }
    Metadata::read(source)?.restore(target)
}

/// Make `root` hold `desired` wherever it differs from `current`.
/// `source_of` names the file that holds the desired version of a path.
/// Returns how many paths differed.
fn converge(
    root: &Path,
    current: &Tree,
    desired: &Tree,
    source_of: &dyn Fn(&str, &Entry) -> Result<PathBuf>,
) -> Result<usize> {
    let keys: BTreeSet<&String> = current.keys().chain(desired.keys()).collect();
    let differing: Vec<&String> = keys.into_iter().filter(|key| current.get(*key) != desired.get(*key)).collect();
    // Removals first, children before parents. Removing a directory removes
    // its children too; removing a missing path is a no-op.
    for key in differing.iter().rev() {
        if let Some(entry) = current.get(*key) {
            let wanted = desired.get(*key);
            let replaced_kind = wanted.is_some_and(|w| w.fingerprint.directory != entry.fingerprint.directory);
            if wanted.is_none() || replaced_kind || entry.fingerprint.digest == REPARSE_DIGEST {
                remove(&root.join(&entry.path))?;
            }
        }
    }
    // Then placements, parents before children. Creating a child does not
    // change a directory's creation time or attributes, so the metadata a
    // directory receives here survives.
    for key in &differing {
        if let Some(entry) = desired.get(*key) {
            place(&source_of(key, entry)?, &root.join(&entry.path))?;
        }
    }
    Ok(differing.len())
}

/// Make the shadow `.git` under `to` match the one under `from`. Reparse
/// points are never copied, and any under `to` are removed.
fn mirror_git(from: &Path, to: &Path, objects: bool) -> Result<()> {
    // A junction left in place of the target's `.git` is removed as a
    // whole, never walked; the mirror below rebuilds the directory.
    let target = to.join(".git");
    if is_reparse(&target)? {
        remove(&target)?;
    }
    let desired: Tree = scan_git(from, objects)?
        .into_iter()
        .filter(|(_, entry)| entry.fingerprint.digest != REPARSE_DIGEST)
        .collect();
    let current = scan_git(to, objects)?;
    converge(to, &current, &desired, &|_, entry| Ok(from.join(&entry.path)))?;
    Ok(())
}

/// Count the entries under host directory `relative` that the session never
/// sees (denied files, `.git`, `.deltabox`), without hashing anything.
fn hidden_entries(source: &Path, relative: &str, policy: &Policy) -> Result<usize> {
    let root = source.join(relative);
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in walkdir::WalkDir::new(&root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|e| Error::Backend(e.to_string()))?;
        let key = private::key(&relative_spelling(source, entry.path())?);
        if skipped(&key) || denied(policy, &key) {
            count += 1;
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Persistent records
// ---------------------------------------------------------------------------

/// A checkpoint stores the working-tree scan plus copies of only the paths
/// that differed from the accepted tree at that moment (in `tree\`), and the
/// shadow `.git` without `objects` (in `.git\`). Every other path's content is
/// the accepted copy's, which commit preserves into the checkpoint before it
/// changes.
#[derive(Serialize, Deserialize)]
struct CheckpointRecord {
    tree: Tree,
    delta: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CommitPhase {
    /// The host publication may or may not have happened.
    Publishing,
    /// Published; checkpoints may still rely on the old accepted content.
    Published,
    /// Checkpoints are self-contained; the accepted copy may be stale.
    CheckpointsPreserved,
}

/// The step that was in flight when the last process stopped.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Pending {
    /// A command ran. `before` is the working tree before it; `backup` lists
    /// the paths whose pre-command content is in `exec-backup`, which also
    /// holds the shadow `.git` without `objects`.
    Exec { before: Tree, backup: BTreeSet<String> },
    /// The working tree was being restored to a checkpoint, or to the
    /// accepted tree when `target` is None.
    Restore { target: Option<String> },
    /// A commit of `changes`. `staged` holds the published entries; their
    /// content is in `stage\tree`, copy-private's own copy, which outlives
    /// the publication's stage. `decision` is the `next_id` that session.json
    /// carries once `private::publish` has committed.
    Commit { stage: PathBuf, staged: Tree, changes: BTreeSet<String>, decision: u64, phase: CommitPhase },
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Version {
    Accepted,
    Checkpoint(String),
    Live,
}

/// An open copy-private session. Holding it holds the session's
/// `operation.lock`, so operations on one session never overlap.
pub(crate) struct Session {
    store: PathBuf,
    manifest: Manifest,
    _operation: File,
}

impl Session {
    /// Lock the session, reload its manifest, and finish any interrupted step.
    pub(crate) fn open(store: &Path) -> Result<Self> {
        let mut session = Self::load(store)?;
        session.recover()?;
        Ok(session)
    }

    fn load(store: &Path) -> Result<Self> {
        let operation = lock(&store.join("operation.lock"), Duration::from_secs(660))?;
        let mut manifest: Manifest = read_json(&store.join("session.json"))?;
        check_layout(&manifest)?;
        if is_reparse(manifest.live())? {
            return Err(Error::Backend("corrupt copy-private session: working copy is a reparse point".into()));
        }
        // A running record without accepted.json belongs to a start that died
        // before its copy finished (a live start holds the operation lock we
        // now hold). It owns nothing worth keeping; stop it so status, stop
        // and removal give clear answers instead of missing-file errors.
        if manifest.status != SessionStatus::Stopped && !store.join("accepted.json").is_file() {
            manifest.status = SessionStatus::Stopped;
            write_json(&store.join("session.json"), &manifest)?;
        }
        Ok(Self { store: store.to_path_buf(), manifest, _operation: operation })
    }

    fn live(&self) -> &Path {
        self.manifest.live()
    }
    fn accepted_dir(&self) -> PathBuf {
        self.store.join("accepted")
    }
    fn checkpoint_dir(&self, id: &str) -> PathBuf {
        self.store.join("checkpoints").join(id)
    }
    fn save(&self) -> Result<()> {
        write_json(&self.store.join("session.json"), &self.manifest)
    }
    fn accepted_tree(&self) -> Result<Tree> {
        read_json(&self.store.join("accepted.json"))
    }
    /// The host's fingerprints, hidden paths included, kept by `private.rs`.
    fn baseline(&self) -> Result<Baseline> {
        read_json(&self.store.join("baseline.json"))
    }
    /// `.git` and `.deltabox` are pruned from every scan, so a junction put
    /// in their place by a command is checked here, before anything walks
    /// into them or writes under them.
    fn hidden_roots_plain(&self) -> Result<()> {
        for relative in [".git", ".deltabox", ".deltabox\\writes"] {
            if is_reparse(&self.live().join(relative))? {
                return Err(reparse_error(relative));
            }
        }
        Ok(())
    }
    /// Every directory between the working copy and `relative` must be a
    /// plain directory; a junction there would lead outside the copy.
    fn check_ancestors(&self, relative: &str) -> Result<()> {
        plain_root(self.live())?;
        let parts: Vec<&str> = relative.split('\\').collect();
        let mut directory = self.live().to_path_buf();
        for part in &parts[..parts.len() - 1] {
            directory.push(part);
            if is_reparse(&directory)? {
                return Err(reparse_error(&parts[..parts.len() - 1].join("\\")));
            }
        }
        Ok(())
    }
    fn live_tree(&self) -> Result<Tree> {
        scan(self.live(), &tracked(&self.manifest.policy))
    }
    fn running(&self) -> Result<()> {
        if self.manifest.status == SessionStatus::Stopped {
            return Err(Error::SessionStopped(self.manifest.session_id.clone()));
        }
        Ok(())
    }

    fn version(&self, id: Option<&str>, default: Version) -> Result<Version> {
        match id {
            None => Ok(default),
            Some(id) if self.manifest.checkpoints.iter().any(|c| c.info.id == id) => Ok(Version::Checkpoint(id.to_owned())),
            Some(id) => Err(Error::CheckpointNotFound(id.to_owned())),
        }
    }

    fn checkpoint_record(&self, id: &str) -> Result<CheckpointRecord> {
        read_json(&self.checkpoint_dir(id).join("index.json"))
    }

    fn record_of(&self, version: &Version) -> Result<Option<CheckpointRecord>> {
        match version {
            Version::Checkpoint(id) => self.checkpoint_record(id).map(Some),
            _ => Ok(None),
        }
    }

    fn tree_of(&self, version: &Version, record: Option<&CheckpointRecord>) -> Result<Tree> {
        match (version, record) {
            (Version::Accepted, _) => self.accepted_tree(),
            (Version::Checkpoint(_), Some(record)) => Ok(record.tree.clone()),
            (Version::Checkpoint(id), None) => Ok(self.checkpoint_record(id)?.tree),
            (Version::Live, _) => self.live_tree(),
        }
    }

    /// Where the content of `entry` lives in `version`.
    fn content_of(&self, version: &Version, record: Option<&CheckpointRecord>, key: &str, entry: &Entry) -> PathBuf {
        match version {
            Version::Live => self.live().join(&entry.path),
            Version::Checkpoint(id) if record.is_some_and(|r| r.delta.contains(key)) => {
                self.checkpoint_dir(id).join("tree").join(&entry.path)
            }
            _ => self.accepted_dir().join(&entry.path),
        }
    }

    /// Describe the paths in `keys` from one version to another.
    fn describe(
        &self,
        (from, from_record, before): (&Version, Option<&CheckpointRecord>, &Tree),
        (to, to_record, after): (&Version, Option<&CheckpointRecord>, &Tree),
        keys: &BTreeSet<String>,
    ) -> Vec<Change> {
        let read = |version: &Version, record: Option<&CheckpointRecord>, key: &str, entry: Option<&Entry>| -> Option<Vec<u8>> {
            let entry = entry.filter(|e| !e.fingerprint.directory && e.fingerprint.size <= DEFAULT_TEXT_SIZE_LIMIT)?;
            fs::read(self.content_of(version, record, key, entry)).ok()
        };
        keys.iter()
            .map(|key| {
                let (a, b) = (before.get(key), after.get(key));
                let kind = match (a, b) {
                    (None, _) => ChangeKind::Added,
                    (_, None) => ChangeKind::Deleted,
                    _ => ChangeKind::Modified,
                };
                let path = slash(&b.or(a).map(|e| e.path.clone()).unwrap_or_else(|| key.clone()));
                describe_change(
                    &path,
                    kind,
                    a.map(|e| e.fingerprint.size),
                    b.map(|e| e.fingerprint.size),
                    read(from, from_record, key, a).as_deref(),
                    read(to, to_record, key, b).as_deref(),
                    DEFAULT_TEXT_SIZE_LIMIT,
                )
            })
            .collect()
    }

    fn pending_count(&self) -> Result<usize> {
        Ok(changed_keys(&self.manifest.policy, &self.accepted_tree()?, &self.live_tree()?).len())
    }

    // -----------------------------------------------------------------------
    // Recovery
    // -----------------------------------------------------------------------

    fn recover(&mut self) -> Result<()> {
        let marker = self.store.join(PENDING);
        // A publication may have committed while no process of this session
        // was looking: a sibling settles the project journal and writes the
        // decision into session.json. Settle and reload it before anything
        // here saves the manifest, or the decision would be written over.
        if marker.exists() {
            if let Ok(Pending::Commit { phase: CommitPhase::Publishing, .. }) = read_json::<Pending>(&marker) {
                private::recover(&self.store)?;
                self.manifest = read_json(&self.store.join("session.json"))?;
            }
        }
        // Drop dangling checkpoints first, so a restore can rely on its target.
        self.reconcile_checkpoints()?;
        if marker.exists() {
            match read_json::<Pending>(&marker)? {
                Pending::Exec { before, backup } => self.revert_exec(&before, &backup)?,
                Pending::Restore { target } => {
                    // An abort always ends with the review scope cleared.
                    let version = self.version(target.as_deref(), Version::Accepted).unwrap_or(Version::Accepted);
                    self.restore_live(&version)?;
                    self.manifest.review = None;
                    self.save()?;
                }
                Pending::Commit { stage, staged, changes, decision, phase } => {
                    self.finish_commit(&stage, &staged, &changes, decision, phase)?
                }
            }
            self.clear_mark()?;
        }
        // Leftovers that no marker references any more, swept best effort: one
        // held open by a scanner must not make every operation fail, and the
        // next sweep retries. A publication stage holds host backups while the
        // project's journal names this store, so stages wait for that journal.
        let _ = remove(&self.store.join("exec-backup"));
        if !private::journal_names(&self.store)? {
            for entry in fs::read_dir(&self.store)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("publish-") || name.starts_with("commit-") {
                    let _ = remove(&entry.path());
                }
            }
        }
        // `.deltabox` is where fs.write stages content. A junction a command
        // put there is removed whole, never walked; a missing working copy is
        // left missing (only a restore rebuilds it).
        let metadata = self.live().join(".deltabox");
        if is_reparse(&metadata)? {
            remove(&metadata)?;
        } else {
            let _ = remove(&metadata.join("writes"));
        }
        Ok(())
    }

    fn mark(&self, pending: &Pending) -> Result<()> {
        write_json(&self.store.join(PENDING), pending)
    }

    /// A marker left behind would undo or redo a finished step at the next
    /// operation, so a removal blocked by a scanner holding the file open is
    /// retried for a moment before it counts as a failure.
    fn clear_mark(&self) -> Result<()> {
        let marker = self.store.join(PENDING);
        let mut attempts = 0;
        loop {
            match fs::remove_file(&marker) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(_) if attempts < 50 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Drop manifest entries without a record and directories without a
    /// manifest entry (half-built `.tmp` checkpoints included).
    fn reconcile_checkpoints(&mut self) -> Result<()> {
        let before = self.manifest.checkpoints.len();
        let root = self.store.join("checkpoints");
        self.manifest.checkpoints.retain(|c| root.join(&c.info.id).join("index.json").is_file());
        if self.manifest.checkpoints.len() != before {
            let ids: BTreeSet<&str> = self.manifest.checkpoints.iter().map(|c| c.info.id.as_str()).collect();
            if self.manifest.review.as_ref().is_some_and(|r| !ids.contains(r.checkpoint_id.as_str())) {
                self.manifest.review = None;
            }
            self.save()?;
        }
        if root.exists() {
            let known: BTreeSet<String> = self.manifest.checkpoints.iter().map(|c| c.info.id.clone()).collect();
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                if !known.contains(entry.file_name().to_string_lossy().as_ref()) {
                    remove(&entry.path())?;
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    pub(crate) fn info(&self) -> Result<SessionInfo> {
        let pending_changes = if self.manifest.status == SessionStatus::Running { self.pending_count()? } else { 0 };
        let mut filesystem = FilesystemInfo::copy_private(FilesystemMode::CopyPrivate, None);
        filesystem.isolation_provider = Some("mxc-psec-filesystem".into());
        filesystem.degraded_protections = Some(vec!["resource-limits".into()]);
        Ok(SessionInfo {
            session_id: self.manifest.session_id.clone(),
            project: self.manifest.source.clone(),
            started_at_ms: self.manifest.started_at_ms,
            status: self.manifest.status.clone(),
            checkpoint_count: self.manifest.checkpoints.len(),
            pending_changes,
            filesystem,
        })
    }

    // -----------------------------------------------------------------------
    // Exec
    // -----------------------------------------------------------------------

    pub(crate) fn exec(&mut self, params: ExecParams) -> Result<ExecResult> {
        self.running()?;
        // Cheap checks first, before the backup below.
        if params.argv.is_empty() {
            return Err(Error::InvalidPolicy("empty argv".into()));
        }
        super::sandbox::available()?;
        let accepted = self.accepted_tree()?;
        let baseline = self.baseline()?.entries;
        self.hidden_roots_plain()?;
        // Include denied paths so a command that creates one is caught.
        let before = scan(self.live(), &skipped)?;
        let backup_root = self.store.join("exec-backup");
        remove(&backup_root)?;
        fs::create_dir_all(&backup_root)?;
        let mut backup = BTreeSet::new();
        for (key, entry) in &before {
            if accepted.get(key) != Some(entry) {
                place(&self.live().join(&entry.path), &backup_root.join(&entry.path))?;
                backup.insert(key.clone());
            }
        }
        mirror_git(self.live(), &backup_root, false)?;
        self.mark(&Pending::Exec { before: before.clone(), backup: backup.clone() })?;
        crash_point("copy_private_exec_marked");
        let outcome = super::sandbox::execute(&self.manifest, params, &self.store).and_then(|result| {
            self.hidden_roots_plain()?;
            validate(&self.manifest.policy, &accepted, &baseline, &scan(self.live(), &skipped)?)?;
            Ok(result)
        });
        // The backup is swept later if something holds it open now.
        match outcome {
            Ok(result) => {
                self.clear_mark()?;
                let _ = remove(&backup_root);
                Ok(result)
            }
            Err(error) => {
                // Undo the whole command, so pending state never contains a
                // denied path, a reparse point, or more than the storage limit.
                // If undoing fails, the marker stays and the next operation
                // retries it; report both errors.
                if let Err(undo) = self.revert_exec(&before, &backup) {
                    return Err(Error::Backend(format!("{error}; undoing the command also failed: {undo}")));
                }
                self.clear_mark()?;
                let _ = remove(&backup_root);
                Err(error)
            }
        }
    }

    fn revert_exec(&self, before: &Tree, backup: &BTreeSet<String>) -> Result<()> {
        fs::create_dir_all(self.live())?;
        let current = scan(self.live(), &skipped)?;
        let backup_root = self.store.join("exec-backup");
        let accepted = self.accepted_dir();
        converge(self.live(), &current, before, &|key, entry| {
            Ok(if backup.contains(key) { backup_root.join(&entry.path) } else { accepted.join(&entry.path) })
        })?;
        mirror_git(&backup_root, self.live(), false)
    }

    // -----------------------------------------------------------------------
    // Direct file access
    // -----------------------------------------------------------------------

    /// Resolve a caller path (absolute in the host project or the private
    /// copy, or project-relative) to its relative spelling; empty for the root.
    fn relative(&self, path: &Path) -> Result<String> {
        let path = PathBuf::from(path.to_string_lossy().replace('/', "\\"));
        let relative = if path.is_absolute() {
            [self.manifest.source.as_path(), self.live()]
                .iter()
                .find_map(|root| strip_prefix_ignoring_case(&path, root))
                .ok_or_else(|| Error::InvalidPolicy("path outside project".into()))?
        } else {
            path
        };
        let mut parts: Vec<String> = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => {
                    let part = part.to_str().ok_or_else(|| Error::Unsupported("non-Unicode Windows path".into()))?;
                    check_component(part)?;
                    parts.push(part.to_owned());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if parts.pop().is_none() {
                        return Err(Error::InvalidPolicy("path outside project".into()));
                    }
                }
                _ => return Err(Error::InvalidPolicy("path outside project".into())),
            }
        }
        Ok(parts.join("\\"))
    }

    pub(crate) fn read_file(&mut self, path: &Path) -> Result<FsReadResult> {
        self.running()?;
        let relative = self.relative(path)?;
        if denied(&self.manifest.policy, &private::key(&relative)) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        self.check_ancestors(&relative)?;
        let target = self.live().join(&relative);
        let m = match fs::symlink_metadata(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FsReadResult { content_base64: String::new(), size: 0 });
            }
            other => other?,
        };
        if m.file_attributes() & 0x400 != 0 || !m.is_file() {
            return Err(Error::Backend("fs.read target is not a regular file".into()));
        }
        if m.len() > MAX_FS_READ_BYTES {
            return Err(Error::Backend("fs.read file exceeds 16 MiB limit".into()));
        }
        let bytes = fs::read(&target)?;
        Ok(FsReadResult { size: bytes.len(), content_base64: base64::engine::general_purpose::STANDARD.encode(bytes) })
    }

    pub(crate) fn write_file(&mut self, path: &Path, content: &[u8]) -> Result<()> {
        self.running()?;
        let relative = self.relative(path)?;
        let key = private::key(&relative);
        let policy = &self.manifest.policy;
        if denied(policy, &key) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        if private::is_draft(&key) {
            return Err(draft_error(&relative));
        }
        if content.len() as u64 > policy.limits.max_write_bytes.unwrap_or(DEFAULT_WRITE_LIMIT) {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        // The working copy must exist (only a restore rebuilds a lost one),
        // and nothing on the way to the target may be a junction.
        self.check_ancestors(&relative)?;
        self.hidden_roots_plain()?;
        let target = self.live().join(&relative);
        let is_directory = |m: &fs::Metadata| m.file_attributes() & 0x10 != 0 && m.file_attributes() & 0x400 == 0;
        if relative.is_empty() || fs::symlink_metadata(&target).is_ok_and(|m| is_directory(&m)) {
            return Err(Error::Backend("fs.write target is a directory".into()));
        }
        // Check the storage limit as if the write had happened.
        if !skipped(&key) {
            let others = pending_bytes(&self.accepted_tree()?, &self.live_tree()?, Some(&key));
            let total = others + content.len() as u64;
            if total > policy.limits.max_storage_bytes.unwrap_or(DEFAULT_STORAGE_LIMIT) {
                return Err(Error::Backend(format!("pending changes use {total} bytes, exceeding max_storage_bytes")));
            }
        }
        // Write beside the tree, then move into place. `.deltabox` is never
        // scanned, and recovery sweeps leftovers from an interrupted write.
        let scratch = self.live().join(".deltabox").join("writes");
        fs::create_dir_all(&scratch)?;
        let temporary = scratch.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        fs::write(&temporary, content)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // Replace, never follow: a reparse point is removed first. Read-only,
        // hidden and system attributes can make the rename fail, so they are
        // cleared; hidden and system go back onto the new file.
        let mut keep = 0;
        match fs::symlink_metadata(&target) {
            Ok(m) if m.file_attributes() & 0x400 != 0 => remove(&target)?,
            Ok(m) if m.file_attributes() & 0x7 != 0 => {
                keep = m.file_attributes() & 0x6;
                set_attributes(&target, m.file_attributes() & !0x7)?;
            }
            _ => {}
        }
        fs::rename(&temporary, &target)?;
        if keep != 0 {
            set_attributes(&target, 0x20 | keep)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Checkpoints and diff
    // -----------------------------------------------------------------------

    pub(crate) fn checkpoint(&mut self, label: Option<String>) -> Result<CheckpointInfo> {
        self.running()?;
        let accepted = self.accepted_tree()?;
        let tree = self.live_tree()?;
        let id = uuid::Uuid::new_v4().to_string();
        let building = self.store.join("checkpoints").join(format!("{id}.tmp"));
        remove(&building)?;
        fs::create_dir_all(building.join("tree"))?;
        let mut delta = BTreeSet::new();
        for (key, entry) in &tree {
            if accepted.get(key) != Some(entry) {
                place(&self.live().join(&entry.path), &building.join("tree").join(&entry.path))?;
                delta.insert(key.clone());
            }
        }
        mirror_git(self.live(), &building, false)?;
        let change_count = changed_keys(&self.manifest.policy, &accepted, &tree).len();
        write_json(&building.join("index.json"), &CheckpointRecord { tree, delta })?;
        fs::rename(&building, self.checkpoint_dir(&id))?;
        let info = CheckpointInfo { id, label, created_at_ms: now_ms(), change_count, committed: false };
        self.manifest.checkpoints.push(Savepoint { info: info.clone(), cursor: 0 });
        // Persist the shorter list before deleting anything, so a crash leaves
        // only orphan directories, which recovery removes.
        let pruned = self.prune_checkpoints();
        self.save()?;
        for id in pruned {
            let _ = remove(&self.checkpoint_dir(&id));
        }
        Ok(info)
    }

    /// Keep at most `CHECKPOINT_CAP`, dropping the oldest first but never the
    /// active review's checkpoint. Returns the ids dropped from the manifest.
    fn prune_checkpoints(&mut self) -> Vec<String> {
        let protected = self.manifest.review.as_ref().map(|r| r.checkpoint_id.clone());
        let mut pruned = Vec::new();
        while self.manifest.checkpoints.len() > CHECKPOINT_CAP {
            let Some(index) = self.manifest.checkpoints.iter().position(|c| Some(&c.info.id) != protected.as_ref()) else {
                break;
            };
            pruned.push(self.manifest.checkpoints.remove(index).info.id);
        }
        pruned
    }

    pub(crate) fn diff(&mut self, from: Option<&str>, to: Option<&str>) -> Result<DiffResult> {
        self.running()?;
        let (from_version, to_version) = (self.version(from, Version::Accepted)?, self.version(to, Version::Live)?);
        let (from_record, to_record) = (self.record_of(&from_version)?, self.record_of(&to_version)?);
        let before = self.tree_of(&from_version, from_record.as_ref())?;
        let after = self.tree_of(&to_version, to_record.as_ref())?;
        let keys = changed_keys(&self.manifest.policy, &before, &after);
        let changes = self.describe(
            (&from_version, from_record.as_ref(), &before),
            (&to_version, to_record.as_ref(), &after),
            &keys,
        );
        Ok(DiffResult { from: from.map(str::to_owned), to: to.map(str::to_owned), pending_changes: changes.len(), changes })
    }

    // -----------------------------------------------------------------------
    // Commit
    // -----------------------------------------------------------------------

    pub(crate) fn commit(&mut self, checkpoint: Option<String>) -> Result<CommitResult> {
        self.running()?;
        // An unknown checkpoint is an error, and only the latest one may be
        // named: what gets published is the working tree as it is now.
        if let Some(id) = &checkpoint {
            self.version(Some(id), Version::Accepted)?;
            if self.manifest.checkpoints.last().map(|c| c.info.id.as_str()) != Some(id.as_str()) {
                return Err(Error::Unsupported("commit currently supports the latest checkpoint only".into()));
            }
        }
        let checkpoint_id = checkpoint.unwrap_or_else(|| "accepted".into());
        let policy = self.manifest.policy.clone();
        let accepted = self.accepted_tree()?;
        self.hidden_roots_plain()?;
        // One scan decides everything below; the stage is checked against it.
        let full = scan(self.live(), &skipped)?;
        validate(&policy, &accepted, &self.baseline()?.entries, &full)?;
        let live: Tree = full.into_iter().filter(|(key, _)| !denied(&policy, key)).collect();
        let changed = changed_keys(&policy, &accepted, &live);
        if changed.is_empty() {
            mirror_git(&self.manifest.source, &self.accepted_dir(), true)?;
            mirror_git(&self.manifest.source, self.live(), true)?;
            self.manifest.review = None;
            self.save()?;
            return Ok(CommitResult { checkpoint_id, applied: 0, folded_publications: None });
        }
        let changes = self.describe((&Version::Accepted, None, &accepted), (&Version::Live, None, &live), &changed);

        // Stage exactly the scanned versions, then make sure nothing changed
        // under us: an edit during the commit must never publish a mix.
        let stage = self.store.join(format!("commit-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(stage.join("tree"))?;
        let staged: Tree = changed.iter().filter_map(|key| live.get(key).map(|e| (key.clone(), e.clone()))).collect();
        for entry in staged.values() {
            place(&self.live().join(&entry.path), &stage.join("tree").join(&entry.path))?;
        }
        let check = scan(&stage.join("tree"), &|_| false)?;
        if staged.iter().any(|(key, entry)| check.get(key) != Some(entry)) {
            remove(&stage)?;
            return Err(Error::Backend("the working copy changed during commit; nothing was published, retry the commit".into()));
        }
        // `private::publish` removes its stage when the publication settles,
        // so it gets its own copy; `stage` stays to update the accepted copy.
        let publication = self.store.join(format!("publish-{}", uuid::Uuid::new_v4()));
        copy_filtered(&stage.join("tree"), &publication.join("tree"), Path::new(""), &|_| false)?;

        // session.json carries `decision` exactly when the publication committed.
        let decision = self.manifest.next_id + 1;
        let mut next = self.manifest.clone();
        next.review = None;
        next.next_id = decision;
        let mark = |phase| Pending::Commit { stage: stage.clone(), staged: staged.clone(), changes: changed.clone(), decision, phase };
        self.mark(&mark(CommitPhase::Publishing))?;

        // Runs under publish.lock, so no sibling publication slips in between
        // this check and the publication. As on Unix, any outside change to
        // the project blocks the commit, not only changes to touched paths.
        let (source, store) = (self.manifest.source.clone(), self.store.clone());
        let removed_directories: Vec<&Entry> = changed
            .iter()
            .filter_map(|key| {
                accepted
                    .get(key)
                    .filter(|a| a.fingerprint.directory && !staged.get(key).is_some_and(|s| s.fingerprint.directory))
            })
            .collect();
        let check = || -> Result<()> {
            let baseline: Baseline = read_json(&store.join("baseline.json"))?;
            let host = scan_with(&source, &tracked(&policy), true)?;
            let keys: BTreeSet<&String> = baseline.entries.keys().chain(host.keys()).collect();
            let drift: Vec<PathConflict> = keys
                .into_iter()
                .filter(|key| !skipped(key) && !denied(&policy, key))
                .filter(|key| baseline.entries.get(*key) != host.get(*key).map(|e| &e.fingerprint))
                .map(|key| PathConflict {
                    path: slash(&host.get(key).or_else(|| accepted.get(key)).map(|e| e.path.clone()).unwrap_or_else(|| key.clone())),
                    host_change: match (baseline.entries.get(key), host.get(key)) {
                        (None, _) => ChangeKind::Added,
                        (_, None) => ChangeKind::Deleted,
                        _ => ChangeKind::Modified,
                    },
                })
                .collect();
            if !drift.is_empty() {
                return Err(Error::ProjectConflict(ConflictReport::new("real project no longer matches the accepted baseline", drift)));
            }
            // A path this session added must not exist on the host under
            // another name. NTFS resolves 8.3 short names (`.env` answers to
            // `ENV~1`, `.git` to `GIT~1`), and the publication would replace
            // that hidden file with the new one.
            for (key, entry) in &staged {
                if host.contains_key(key) {
                    continue;
                }
                match fs::symlink_metadata(source.join(&entry.path)) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(Error::ProjectConflict(ConflictReport::new(
                            "a new path already exists in the real project under another name (a short name or a hidden path)",
                            vec![PathConflict { path: slash(&entry.path), host_change: ChangeKind::Added }],
                        )))
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Removing a directory on the host would also remove what this
            // session never saw inside it (secrets, a nested repository).
            for entry in &removed_directories {
                let hidden = hidden_entries(&source, &entry.path, &policy)?;
                if hidden > 0 {
                    return Err(Error::Backend(format!(
                        "cannot remove {} from the project: it contains {hidden} entries this session cannot see (denied files or .git); move them first or keep the directory",
                        slash(&entry.path)
                    )));
                }
            }
            Ok(())
        };
        let result = CommitResult { checkpoint_id, applied: changes.len(), folded_publications: None };
        if let Err(error) = private::publish(&self.store, &self.manifest, &next, &changes, &publication, false, &check) {
            // Settle the host now rather than at some later operation: finish
            // a publication that committed, or let `private.rs` undo one that
            // did not. If settling fails, the marker and both stages stay for
            // the next try.
            if self.finish_commit(&stage, &staged, &changed, decision, CommitPhase::Publishing).is_err() {
                return Err(error);
            }
            let _ = remove(&publication);
            if self.manifest.next_id == decision {
                // Published after all. A marker left behind only makes the
                // next operation repeat the private bookkeeping.
                let _ = self.clear_mark();
                return Ok(result);
            }
            self.clear_mark()?;
            return Err(error);
        }
        let _ = remove(&publication);
        crash_point("copy_private_commit_published");
        self.manifest = next;
        // The host has the changes, and that is the outcome reported. What is
        // left updates private copies only; if it fails here, the marker makes
        // the next operation finish it.
        let _ = self
            .mark(&mark(CommitPhase::Published))
            .and_then(|()| self.finish_commit(&stage, &staged, &changed, decision, CommitPhase::Published))
            .and_then(|()| self.clear_mark());
        Ok(result)
    }

    /// Complete a commit from `phase`. Every step can be repeated.
    fn finish_commit(
        &mut self,
        stage: &Path,
        staged: &Tree,
        changed: &BTreeSet<String>,
        decision: u64,
        mut phase: CommitPhase,
    ) -> Result<()> {
        let mark = |phase| Pending::Commit { stage: stage.to_path_buf(), staged: staged.clone(), changes: changed.clone(), decision, phase };
        if phase == CommitPhase::Publishing {
            // Let `private.rs` finish or undo its host transaction, then read
            // the decision it recorded.
            private::recover(&self.store)?;
            let current: Manifest = read_json(&self.store.join("session.json"))?;
            if current.next_id != decision {
                remove(stage)?;
                return Ok(());
            }
            self.manifest = current;
            phase = CommitPhase::Published;
            self.mark(&mark(phase))?;
        }
        if phase == CommitPhase::Published {
            // Checkpoints read unchanged paths from the accepted copy. Give
            // them their own copy of every path the commit is about to change.
            let accepted = self.accepted_tree()?;
            let mut lost = Vec::new();
            for savepoint in self.manifest.checkpoints.clone() {
                let id = &savepoint.info.id;
                // A missing record is dropped by reconciliation; never fail
                // here, after the host already has the publication.
                let Ok(mut record) = self.checkpoint_record(id) else { continue };
                let mut dirty = false;
                for key in changed {
                    if record.tree.contains_key(key) && !record.delta.contains(key) {
                        if let Some(entry) = accepted.get(key) {
                            let source = self.accepted_dir().join(&entry.path);
                            // An accepted file that vanished (quarantined, or
                            // deleted by a cleanup tool) cannot be preserved.
                            // The checkpoint that needs it is dropped rather
                            // than wedging every later operation.
                            if fs::symlink_metadata(&source).is_err() {
                                lost.push(id.clone());
                                break;
                            }
                            place(&source, &self.checkpoint_dir(id).join("tree").join(&entry.path))?;
                            record.delta.insert(key.clone());
                            dirty = true;
                        }
                    }
                }
                if dirty {
                    write_json(&self.checkpoint_dir(id).join("index.json"), &record)?;
                }
            }
            if !lost.is_empty() {
                self.manifest.checkpoints.retain(|c| !lost.contains(&c.info.id));
                if self.manifest.review.as_ref().is_some_and(|r| lost.contains(&r.checkpoint_id)) {
                    self.manifest.review = None;
                }
                self.save()?;
                for id in &lost {
                    let _ = remove(&self.checkpoint_dir(id));
                }
            }
            phase = CommitPhase::CheckpointsPreserved;
            self.mark(&mark(phase))?;
        }
        // The accepted copy receives exactly the published paths. Nothing else
        // in it is touched, so outside damage to an unrelated file cannot
        // block this step, and repeating it after a crash is harmless.
        let accepted = self.accepted_tree()?;
        let current: Tree = changed.iter().filter_map(|key| accepted.get(key).map(|e| (key.clone(), e.clone()))).collect();
        let published: Tree = changed.iter().filter_map(|key| staged.get(key).map(|e| (key.clone(), e.clone()))).collect();
        converge(&self.accepted_dir(), &current, &published, &|_, entry| Ok(stage.join("tree").join(&entry.path)))?;
        let mut desired = accepted;
        for key in changed {
            match staged.get(key) {
                Some(entry) => desired.insert(key.clone(), entry.clone()),
                None => desired.remove(key),
            };
        }
        write_json(&self.store.join("accepted.json"), &desired)?;
        // The shadow `.git` follows the host, as a Unix commit recopies it. A
        // working copy that went missing stays missing: only a restore
        // rebuilds it, never a step that would leave it almost empty.
        mirror_git(&self.manifest.source, &self.accepted_dir(), true)?;
        if fs::symlink_metadata(self.live()).is_ok() {
            plain_root(self.live())?;
            mirror_git(&self.manifest.source, self.live(), true)?;
        }
        let _ = remove(stage);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Abort
    // -----------------------------------------------------------------------

    pub(crate) fn abort(&mut self, checkpoint: Option<String>) -> Result<AbortResult> {
        self.running()?;
        let target = self.version(checkpoint.as_deref(), Version::Accepted)?;
        // Read the target before marking, so a bad checkpoint fails here and
        // never leaves a marker that recovery cannot complete.
        let record = self.record_of(&target)?;
        self.tree_of(&target, record.as_ref())?;
        self.mark(&Pending::Restore { target: checkpoint.clone() })?;
        crash_point("copy_private_restore_marked");
        let restored = self.restore_live(&target)?;
        self.manifest.review = None;
        self.save()?;
        self.clear_mark()?;
        Ok(AbortResult { checkpoint_id: checkpoint.unwrap_or_else(|| "accepted".into()), restored })
    }

    /// Make the working tree equal `target`. Denied paths are scanned too, so
    /// any stray one is removed.
    fn restore_live(&self, target: &Version) -> Result<usize> {
        let record = self.record_of(target)?;
        let desired = self.tree_of(target, record.as_ref())?;
        fs::create_dir_all(self.live())?;
        let current = scan(self.live(), &skipped)?;
        let restored = converge(self.live(), &current, &desired, &|key, entry| {
            Ok(self.content_of(target, record.as_ref(), key, entry))
        })?;
        match target {
            Version::Checkpoint(id) => mirror_git(&self.checkpoint_dir(id), self.live(), false)?,
            _ => mirror_git(&self.accepted_dir(), self.live(), true)?,
        }
        Ok(restored)
    }

    // -----------------------------------------------------------------------
    // Review scopes
    // -----------------------------------------------------------------------

    fn active_review(&self) -> Result<ActiveReview> {
        self.manifest.review.clone().ok_or_else(|| Error::ReviewNotActive(self.manifest.session_id.clone()))
    }

    /// Idempotent: an existing scope is returned unchanged.
    pub(crate) fn begin_review(&mut self, kind: ReviewKind, label: Option<String>) -> Result<ActiveReview> {
        self.running()?;
        if let Some(review) = &self.manifest.review {
            return Ok(review.clone());
        }
        let checkpoint = self.checkpoint(label.clone())?;
        let review = ActiveReview { checkpoint_id: checkpoint.id, kind, label, started_at_ms: now_ms() };
        self.manifest.review = Some(review.clone());
        self.save()?;
        Ok(review)
    }

    pub(crate) fn review_status(&self) -> Result<Option<ActiveReview>> {
        self.running()?;
        Ok(self.manifest.review.clone())
    }

    pub(crate) fn review_diff(&mut self) -> Result<DiffResult> {
        self.running()?;
        let review = self.active_review()?;
        self.diff(Some(&review.checkpoint_id), None)
    }

    pub(crate) fn review_commit(&mut self) -> Result<CommitResult> {
        self.running()?;
        self.active_review()?;
        self.commit(None)
    }

    pub(crate) fn review_abort(&mut self) -> Result<AbortResult> {
        self.running()?;
        let review = self.active_review()?;
        self.abort(Some(review.checkpoint_id))
    }

    /// Skipping keeps the scope open, as on Unix.
    pub(crate) fn review_skip(&mut self) -> Result<ActiveReview> {
        self.running()?;
        self.active_review()
    }
}

// ---------------------------------------------------------------------------
// Stop and removal
// ---------------------------------------------------------------------------

/// Whether the host may still need this store: exactly while the project's
/// publication journal names it, since its stage holds the host backups a
/// rollback needs. Before the journal exists nothing has touched the host,
/// and later commit phases only update private copies.
fn commit_unsettled(store: &Path) -> Result<bool> {
    private::journal_names(store)
}

/// Stop a session; the pending work stays, as on Unix. If finishing an
/// interrupted exec or abort keeps failing, the session still stops so it can
/// be removed. An unsettled commit does not: the host publication may still
/// need this store.
pub(crate) fn stop(store: &Path) -> Result<SessionInfo> {
    let mut session = Session::load(store)?;
    if let Err(error) = session.recover() {
        if commit_unsettled(store)? {
            return Err(error);
        }
    }
    session.manifest.status = SessionStatus::Stopped;
    session.save()?;
    session.info()
}

/// Refuse to remove a session whose store a host publication still needs, or
/// whose working copy is not where copy-private puts it.
pub(crate) fn check_removable(store: &Path, manifest: &Manifest, runtime: &Path) -> Result<()> {
    check_layout(manifest)?;
    if commit_unsettled(store)? {
        return Err(Error::SessionBusy(format!(
            "{}: an interrupted commit still needs this session; run `session status` once the project is no longer locked, then retry",
            manifest.session_id
        )));
    }
    let under = |path: &Path, root: &str| {
        strip_prefix_ignoring_case(path, &runtime.join(root)).is_some_and(|rest| !rest.as_os_str().is_empty())
    };
    if !manifest.live.as_deref().is_some_and(|live| under(live, "live")) || !under(&manifest.scratch, "scratch") {
        return Err(Error::Backend("corrupt copy-private session: working copy outside the runtime directory".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/// Copy the project into the accepted copy and the working copy, and record
/// the baseline. The copies run without the project's `publish.lock`, so
/// sibling sessions are not held up; the lock is taken only to confirm the
/// host is still exactly what was copied and to record the baseline.
pub(crate) fn initialize(store: &Path, manifest: &Manifest) -> Result<()> {
    check_layout(manifest)?;
    let parent = store.parent().ok_or_else(|| Error::Backend("session store has no parent".into()))?;
    let source = &manifest.source;
    let policy = &manifest.policy;
    let accepted_dir = store.join("accepted");
    let live_dir = manifest.live();
    let changed = || Error::ProjectConflict(ConflictReport::new("project changed while copying the private working tree", Vec::new()));
    let result = (|| {
        // Every host path, exactly as `private::publish` will scan it. A
        // reparse point anywhere would fail every later publication, so it
        // is refused now.
        let host = scan_with(source, &|_| false, true)?;
        if let Some(entry) = host.values().find(|e| e.fingerprint.digest == REPARSE_DIGEST) {
            return Err(Error::Unsupported(format!(
                "reparse points (symlinks, junctions, and cloud placeholders such as OneDrive online-only files) are not supported in Windows copy-private: {}",
                slash(&entry.path)
            )));
        }
        let visible: Tree = masked(&host).into_iter().filter(|(key, _)| !tracked(policy)(key)).collect();
        // Denied files and DeltaBox metadata never leave the project.
        copy_filtered(source, &accepted_dir, Path::new(""), &|key| {
            key.split('\\').any(|part| part == ".deltabox") || denied(policy, key)
        })?;
        copy_filtered(&accepted_dir, live_dir, Path::new(""), &|_| false)?;
        let accepted = scan(&accepted_dir, &tracked(policy))?;
        if visible != accepted || accepted != scan(live_dir, &tracked(policy))? {
            return Err(changed());
        }
        // Under the lock, the host must still match the first scan exactly.
        // The baseline describes the whole host, hidden paths included, so the
        // publication's own per-path checks see no false changes.
        // accepted.json is written last: it marks a finished start.
        let _publish = lock(&parent.join("publish.lock"), Duration::from_secs(660))?;
        private::recover_locked(parent)?;
        if host != scan_with(source, &|_| false, true)? {
            return Err(changed());
        }
        let entries = host.into_iter().map(|(key, entry)| (key, entry.fingerprint)).collect();
        write_json(&store.join("baseline.json"), &Baseline { entries, seen: private::publication_count(parent)? })?;
        write_json(&store.join("accepted.json"), &accepted)
    })();
    if result.is_err() {
        let _ = remove(&accepted_dir);
        let _ = remove(live_dir);
    }
    result
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The working copy must exist apart from the host project; a manifest that
/// lost it must never fall back to `Manifest::live()`'s host default.
fn check_layout(manifest: &Manifest) -> Result<()> {
    let live = manifest.live.as_ref().ok_or_else(|| Error::Backend("corrupt copy-private session: no working copy".into()))?;
    if strip_prefix_ignoring_case(live, &manifest.source).is_some() || strip_prefix_ignoring_case(&manifest.source, live).is_some() {
        return Err(Error::Backend("corrupt copy-private session: working copy overlaps the project".into()));
    }
    Ok(())
}

/// Bytes of new content in pending files, optionally ignoring one path.
fn pending_bytes(accepted: &Tree, live: &Tree, except: Option<&str>) -> u64 {
    live.iter()
        .filter(|(key, entry)| Some(key.as_str()) != except && !entry.fingerprint.directory && accepted.get(*key) != Some(*entry))
        .map(|(_, entry)| entry.fingerprint.size)
        .sum()
}

/// Reject a working tree that contains a new denied path, a reparse point,
/// a name Win32 tools cannot use, or more pending content than the storage
/// limit allows. `live` must be a scan that includes denied paths.
///
/// A denied path whose content equals the host's version in `baseline` is
/// tolerated: git recreates tracked files that match a deny pattern (such as
/// `.env.example`), and their content already sits in the shadow `.git`. It
/// is never diffed, published or counted against the storage limit.
fn validate(policy: &Policy, accepted: &Tree, baseline: &private::Index, live: &Tree) -> Result<()> {
    let mut used = 0u64;
    for (key, entry) in live {
        if accepted.get(key) == Some(entry) {
            continue;
        }
        if entry.fingerprint.digest == REPARSE_DIGEST {
            return Err(reparse_error(&entry.path));
        }
        if denied(policy, key) {
            let same_as_host = baseline.get(key).is_some_and(|host| {
                host.directory == entry.fingerprint.directory && host.size == entry.fingerprint.size && host.digest == entry.fingerprint.digest
            });
            if !same_as_host {
                return Err(Error::Backend(format!("path denied by policy: {}", slash(&entry.path))));
            }
            continue;
        }
        if private::is_draft(key) {
            return Err(draft_error(&entry.path));
        }
        for part in entry.path.split('\\') {
            check_component(part)?;
        }
        if !entry.fingerprint.directory {
            used += entry.fingerprint.size;
        }
    }
    // The publication never publishes a draft, deletions included.
    if let Some((_, entry)) = accepted.iter().find(|(key, _)| private::is_draft(key) && !live.contains_key(*key)) {
        return Err(draft_error(&entry.path));
    }
    if used > policy.limits.max_storage_bytes.unwrap_or(DEFAULT_STORAGE_LIMIT) {
        return Err(Error::Backend(format!("pending changes use {used} bytes, exceeding max_storage_bytes")));
    }
    Ok(())
}

/// Names Win32 tools cannot open as ordinary files: DOS device names,
/// trailing dots or spaces, and alternate data streams. A command can create
/// them through `\\?\` paths, so the scan is checked as well as fs.write.
fn check_component(part: &str) -> Result<()> {
    if part.contains(':') || part.ends_with(['.', ' ']) || reserved_name(part) {
        return Err(Error::InvalidPolicy(format!("invalid Windows path component: {part}")));
    }
    Ok(())
}

/// DOS device names that Win32 tools cannot open as ordinary files.
fn reserved_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or("").trim_end_matches([' ', '.']).to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.len() == 4 && stem.starts_with(prefix) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0'
    };
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}

/// `SetFileAttributesW`; zero means no attributes, which is NORMAL.
fn set_attributes(path: &Path, attributes: u32) -> Result<()> {
    let attributes = if attributes == 0 { 0x80 } else { attributes };
    if unsafe { SetFileAttributesW(wide(path).as_ptr(), attributes) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Strip `root` from `path`, ignoring ASCII case, `/` versus `\`, and the
/// `\\?\` and `\\?\UNC\` prefixes.
fn strip_prefix_ignoring_case(path: &Path, root: &Path) -> Option<PathBuf> {
    fn plain(path: &Path) -> String {
        let text = path.to_string_lossy().replace('/', "\\");
        let text = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text)
        };
        text.trim_end_matches('\\').to_owned()
    }
    let (path, root) = (plain(path), plain(root));
    // `get` returns None instead of panicking when `root.len()` falls inside
    // a multi-byte character.
    if !path.get(..root.len())?.eq_ignore_ascii_case(&root) {
        return None;
    }
    let rest = path.get(root.len()..)?;
    if rest.is_empty() {
        return Some(PathBuf::new());
    }
    rest.strip_prefix('\\').map(PathBuf::from)
}

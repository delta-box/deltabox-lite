//! Private LIVE materialization and serialized, recoverable host publication.
use super::engine::{copy_tree_durable, remove, write_json_durable, Engine, Manifest, Metadata};
use super::durability;
use super::helper::lock;
use crate::changeset::{Change, ChangeKind};
use crate::error::{ConflictReport, PathConflict};
use crate::protocol::{SyncConflict, SyncResult};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Fingerprint {
    pub(super) directory: bool,
    pub(super) attributes: u32,
    pub(super) creation: u64,
    pub(super) write: u64,
    pub(super) size: u64,
    pub(super) digest: String,
}
pub(super) type Index = BTreeMap<String, Fingerprint>;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Baseline { pub(super) entries: Index, pub(super) seen: usize }

#[derive(Clone, Serialize, Deserialize)]
struct Publication { id: String, session: String, changes: BTreeMap<String, Option<Fingerprint>> }

#[derive(Serialize, Deserialize)]
struct Operation {
    path: String,
    existed: bool,
    metadata_only: bool,
    old_metadata: Option<Metadata>,
}

#[derive(Serialize, Deserialize)]
struct Transaction {
    state: PathBuf,
    stage: PathBuf,
    operations: Vec<Operation>,
    manifest: Manifest,
    baseline: Baseline,
    publication: Publication,
    committed: bool,
}

pub(super) fn key(path: &str) -> String { path.replace('/', "\\").to_lowercase() }
fn under(path: &str, parent: &str) -> bool { path == parent || path.starts_with(&format!("{parent}\\")) }
pub(super) fn is_draft(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| part.to_lowercase().ends_with(".deltabox-theirs"))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn history(project_state: &Path) -> Result<Vec<Publication>> {
    let path = project_state.join("publications.json");
    if path.exists() { read_json(&path) } else { Ok(Vec::new()) }
}

/// How many publications this project has, for a new session's baseline.
pub(super) fn publication_count(project_state: &Path) -> Result<usize> {
    Ok(history(project_state)?.len())
}

pub(super) fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let m = fs::symlink_metadata(path)?;
    if m.file_attributes() & 0x400 != 0 {
        return Err(Error::Unsupported(format!("reparse points in Windows private trees: {}", path.display())));
    }
    let digest = if m.is_file() {
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 { break; }
            hash.update(&buffer[..count]);
        }
        format!("{:x}", hash.finalize())
    } else { String::new() };
    Ok(Fingerprint {
        directory: m.is_dir(), attributes: m.file_attributes(), creation: m.creation_time(),
        // Directory times change when a sibling creates an unrelated child.
        write: if m.is_dir() { 0 } else { m.last_write_time() },
        size: if m.is_dir() { 0 } else { m.len() }, digest,
    })
}

fn scan(root: &Path) -> Result<Index> {
    let mut out = Index::new();
    // Check the root too; WalkDir does not follow child reparse points.
    fingerprint(root)?;
    for entry in walkdir::WalkDir::new(root).min_depth(1) {
        let entry = entry.map_err(|e| Error::Backend(e.to_string()))?;
        let relative = entry.path().strip_prefix(root).unwrap().to_str()
            .ok_or_else(|| Error::Unsupported("non-Unicode Windows path".into()))?;
        out.insert(key(relative), fingerprint(entry.path())?);
    }
    Ok(out)
}

pub(super) fn initialize(state: &Path, manifest: &mut Manifest) -> Result<()> {
    let parent = state.parent().unwrap();
    let _publish = lock(&parent.join("publish.lock"), Duration::from_secs(660))?;
    recover_locked(parent)?;
    let entries = scan(&manifest.source)?;
    let live = state.join("live");
    copy_tree_durable(&manifest.source, &live, manifest.durable())?;
    if entries != scan(&live)? || entries != scan(&manifest.source)? {
        remove(&live)?;
        return Err(Error::ProjectConflict(ConflictReport::new("project changed while materializing private LIVE", Vec::new())));
    }
    manifest.live = Some(live.canonicalize()?);
    write_json_durable(&state.join("baseline.json"), &Baseline { entries, seen: history(parent)?.len() }, manifest.durable())
}

/// Whether the project's unfinished publication journal belongs to `state`.
/// Its stage (with the host backups a rollback needs) lives in that store, so
/// the store must not be removed while this is true.
pub(super) fn journal_names(state: &Path) -> Result<bool> {
    #[derive(Deserialize)]
    struct Owner { state: PathBuf }
    // A sibling may retire the journal at any moment; a missing one names nobody.
    let bytes = match fs::read(state.parent().unwrap().join("publish.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        other => other?,
    };
    let owner: Owner = serde_json::from_slice(&bytes)?;
    Ok(owner.state.to_string_lossy().eq_ignore_ascii_case(&state.to_string_lossy()))
}

pub(super) fn recover(state: &Path) -> Result<()> {
    let parent = state.parent().unwrap();
    let _publish = lock(&parent.join("publish.lock"), Duration::from_secs(660))?;
    recover_locked(parent)
}

pub(super) fn recover_locked(parent: &Path) -> Result<()> {
    let journal = parent.join("publish.json");
    if !journal.exists() { return Ok(()); }
    let tx: Transaction = read_json(&journal)?;
    let durable = tx.manifest.durable();
    if durable {
        durability::qualify(&tx.manifest.source)?;
        durability::qualify(&tx.state)?;
        // A previously failed decision-directory flush is ambiguous. Stabilize
        // the recovered decision before completing it or retiring its backups.
        durability::flush(&durability::open(&journal)?)?;
        durability::directory(parent)?;
    }
    if tx.committed {
        // The decision is authoritative. Every subsequent step is idempotent.
        write_json_durable(&tx.state.join("baseline.json"), &tx.baseline, durable)?;
        let mut entries = history(parent)?;
        if !tx.publication.changes.is_empty() && !entries.iter().any(|p| p.id == tx.publication.id) {
            entries.push(tx.publication.clone());
            write_json_durable(&parent.join("publications.json"), &entries, durable)?;
        }
        write_json_durable(&tx.state.join("session.json"), &tx.manifest, durable)?;
    } else {
        for (index, op) in tx.operations.iter().enumerate().rev() {
            let host = tx.manifest.source.join(&op.path);
            if op.metadata_only {
                op.old_metadata.as_ref().unwrap().restore_durable(&host, durable)?;
            } else {
                remove(&host)?;
                if op.existed { copy_tree_durable(&tx.stage.join("backup").join(index.to_string()), &host, durable)?; }
            }
            if durable { durability::parents(&host, &tx.manifest.source)?; durability::point("publish_undo_barrier")?; }
        }
    }
    if durable { durability::point("publish_retire_before")?; }
    fs::remove_file(journal)?;
    if durable { durability::point("publish_retire_removed")?; durability::directory(parent)?; durability::point("publish_retire_barrier")?; }
    let _ = remove(&tx.stage);
    Ok(())
}

fn kind(before: Option<&Fingerprint>, after: Option<&Fingerprint>) -> ChangeKind {
    if before.is_none() { ChangeKind::Added } else if after.is_none() { ChangeKind::Deleted } else { ChangeKind::Modified }
}

fn latest(entries: &[Publication]) -> BTreeMap<String, Option<Fingerprint>> {
    let mut out = BTreeMap::new();
    for publication in entries { out.extend(publication.changes.clone()); }
    out
}

fn copy_publication(source: &Path, target: &Path, durable: bool) -> Result<()> {
    if !source.exists() { return Ok(()); }
    if source.is_dir() {
        durability::create_directories(target, durable)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if !is_draft(&entry.file_name().to_string_lossy()) { copy_publication(&entry.path(), &target.join(entry.file_name()), durable)?; }
        }
        Metadata::read(source)?.restore_durable(target, durable)
    } else { copy_tree_durable(source, target, durable) }
}

/// `check` runs under `publish.lock`, after recovery and before anything is
/// read or changed. Copy-private uses it for its whole-project drift check.
pub(super) fn publish(state: &Path, manifest: &Manifest, next: &Manifest, changes: &[Change], stage: &Path, ours: bool, check: &dyn Fn() -> Result<()>) -> Result<u64> {
    let parent = state.parent().unwrap();
    let _publish = lock(&parent.join("publish.lock"), Duration::from_secs(660))?;
    recover_locked(parent)?;
    check()?;
    let durable = manifest.durable();
    let mut baseline: Baseline = read_json(&state.join("baseline.json"))?;
    let publications = history(parent)?;
    let folded = publications.len().saturating_sub(baseline.seen) as u64;
    let endorsed = latest(&publications);
    let host = scan(&manifest.source)?;
    let paths: BTreeSet<_> = changes.iter().filter(|c| !is_draft(&c.path)).map(|c| key(&c.path)).collect();
    let mut validate = paths.clone();
    for path in &paths {
        // Deleting/replacing a directory must not erase unobserved children.
        validate.extend(baseline.entries.keys().chain(host.keys()).filter(|p| under(p, path)).cloned());
        let mut ancestor = Path::new(path).parent();
        while let Some(p) = ancestor.filter(|p| !p.as_os_str().is_empty()) {
            validate.insert(p.to_string_lossy().into_owned());
            ancestor = p.parent();
        }
    }
    let conflicts: Vec<_> = validate.iter().filter(|p| {
        let (before, current) = (baseline.entries.get(*p), host.get(*p));
        before != current && !(ours && endorsed.get(*p).is_some_and(|v| v.as_ref() == current))
    }).map(|p| PathConflict { path: p.replace('\\', "/"), host_change: kind(baseline.entries.get(p), host.get(p)) }).collect();
    if !conflicts.is_empty() {
        return Err(Error::ProjectConflict(ConflictReport::new("host paths changed since the private baseline", conflicts)));
    }

    let incoming = stage.join("incoming");
    let backup = stage.join("backup");
    durability::create_directories(&incoming, durable)?;
    durability::create_directories(&backup, durable)?;
    // Structural directory operations own their entire subtree. Existing
    // directories receive only metadata so disjoint sibling files survive.
    let structural: BTreeSet<_> = paths.iter().filter(|p| {
        let before = host.get(*p);
        let after = stage.join("tree").join(p).is_dir();
        before.is_some_and(|f| f.directory) != after || (before.is_none() && after)
    }).cloned().collect();
    let mut operations = Vec::new();
    for change in changes.iter().filter(|c| !is_draft(&c.path)) {
        let path = key(&change.path);
        if structural.iter().any(|p| p != &path && under(&path, p)) { continue; }
        let source = manifest.source.join(&change.path);
        let target = stage.join("tree").join(&change.path);
        let metadata_only = host.get(&path).is_some_and(|f| f.directory) && target.is_dir();
        let index = operations.len().to_string();
        if source.exists() && !metadata_only { copy_tree_durable(&source, &backup.join(&index), durable)?; }
        if target.exists() && !metadata_only { copy_publication(&target, &incoming.join(&index), durable)?; }
        operations.push(Operation { path: change.path.clone(), existed: source.exists(), metadata_only,
            old_metadata: if metadata_only { Some(Metadata::read(&source)?) } else { None } });
    }
    let publication = Publication { id: uuid::Uuid::new_v4().to_string(), session: manifest.session_id.clone(), changes: BTreeMap::new() };
    let mut tx = Transaction { state: state.into(), stage: stage.into(), operations, manifest: next.clone(), baseline: baseline.clone(), publication, committed: false };
    let journal = parent.join("publish.json");
    if durable {
        durability::directory(stage)?;
        durability::directory(state)?;
        durability::point("publish_stage_barrier")?;
    }
    write_json_durable(&journal, &tx, durable)?;
    if durable { durability::point("publish_prepared_barrier")?; }
    let result: Result<()> = (|| {
        for (index, op) in tx.operations.iter().enumerate() {
            let target = manifest.source.join(&op.path);
            if op.metadata_only { continue; }
            remove(&target)?;
            let source = incoming.join(index.to_string());
            if source.exists() { copy_tree_durable(&source, &target, durable)?; }
            if durable { durability::parents(&target, &manifest.source)?; durability::point("publish_path_barrier")?; }
            crash_point("private_publish_path_applied");
        }
        for op in tx.operations.iter().filter(|op| op.metadata_only) {
            Metadata::read(&stage.join("tree").join(&op.path))?.restore_durable(&manifest.source.join(&op.path), durable)?;
        }
        if durable { durability::point("publish_host_barrier")?; }
        let after = scan(&manifest.source)?;
        for path in &validate {
            if let Some(value) = after.get(path) { baseline.entries.insert(path.clone(), value.clone()); }
            else { baseline.entries.remove(path); }
            if host.get(path) != after.get(path) { tx.publication.changes.insert(path.clone(), after.get(path).cloned()); }
        }
        baseline.seen = publications.len() + usize::from(!tx.publication.changes.is_empty());
        tx.baseline = baseline;
        tx.committed = true;
        write_json_durable(&journal, &tx, durable)?;
        if durable { durability::point("publish_decision_barrier")?; }
        crash_point("private_publish_committed");
        Ok(())
    })();
    // A visible decision after a failed barrier does not establish durability.
    // Leave recovery information intact and require reopen; never turn I/O
    // failure into a successful commit by rereading the cached decision.
    result?;
    recover_locked(parent)?;
    Ok(folded)
}

pub(super) fn sync(engine: &mut Engine, changes: &[Change]) -> Result<SyncResult> {
    let parent = engine.state.parent().unwrap().to_path_buf();
    let _publish = lock(&parent.join("publish.lock"), Duration::from_secs(660))?;
    recover_locked(&parent)?;
    let mut baseline: Baseline = read_json(&engine.state.join("baseline.json"))?;
    let publications = history(&parent)?;
    let updates = latest(&publications[baseline.seen.min(publications.len())..]);
    let host = scan(&engine.manifest.source)?;
    let mine: BTreeSet<_> = changes.iter().filter(|c| !is_draft(&c.path)).map(|c| key(&c.path)).collect();
    let mut result = SyncResult { folded_publications: publications.len().saturating_sub(baseline.seen) as u64, stale_paths: Vec::new(), conflicts: Vec::new() };
    for (path, value) in updates {
        // Sibling endorsement never blesses a subsequent external edit.
        if value.as_ref() != host.get(&path) { continue; }
        let before = baseline.entries.get(&path);
        if before == value.as_ref() { continue; }
        if mine.iter().any(|p| under(p, &path) || under(&path, p)) {
            let theirs = if value.as_ref().is_some_and(|f| !f.directory) {
                let draft = format!("{path}.deltabox-theirs");
                let bytes = fs::read(engine.manifest.source.join(&path))?;
                engine.write_draft(&draft, &bytes)?.then(|| draft.replace('\\', "/"))
            } else { None };
            result.conflicts.push(SyncConflict { path: path.replace('\\', "/"), host_kind: kind(before, value.as_ref()), theirs });
        } else { result.stale_paths.push(path.replace('\\', "/")); }
        if let Some(value) = value { baseline.entries.insert(path, value); } else { baseline.entries.remove(&path); }
    }
    baseline.seen = publications.len();
    write_json_durable(&engine.state.join("baseline.json"), &baseline, engine.manifest.durable())?;
    Ok(result)
}

fn crash_point(point: &str) {
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_POINT").as_deref() == Ok(point) { std::process::exit(86); }
}

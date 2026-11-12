//! Explicit retention, persistent roots (pins), and restartable reclamation.
//!
//! The project publication lock must cover each operation. A durable journal is
//! the commit point: after it is published, recovery rolls forward, never back.
//! Each directory first leaves the public namespace by rename, which is synced
//! before unlinking. The journal outlives every unlink and its directory sync.
use super::*;
use crate::history_storage::{
    HistoryGcResult, HistoryPinParams, HistoryPruneApplyParams, HistoryPrunePlan,
    HistoryRetentionPolicy,
};
use std::os::unix::fs::OpenOptionsExt;

const PINS: &str = ".pins.json";
const JOURNAL: &str = ".gc-journal.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pins {
    format: u32,
    project: PathBuf,
    versions: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Serialize)]
struct CatalogVersion {
    version: HistoryVersion,
    manifest_digest: String,
    identity: DirectoryIdentity,
}

#[derive(Serialize)]
struct CaptureIdentity {
    id: String,
    identity: DirectoryIdentity,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemKind {
    Version,
    Capture,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcItem {
    id: String,
    kind: ItemKind,
    identity: DirectoryIdentity,
    manifest_digest: Option<String>,
    logical_bytes: u64,
}

impl GcItem {
    fn source(&self, root: &Path) -> PathBuf {
        root.join(match self.kind {
            ItemKind::Version => self.id.clone(),
            ItemKind::Capture => format!(".capture-{}", self.id),
        })
    }

    fn trash(&self, root: &Path) -> PathBuf {
        root.join(match self.kind {
            ItemKind::Version => format!(".trash-{}", self.id),
            ItemKind::Capture => format!(".trash-capture-{}", self.id),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    format: u32,
    project: PathBuf,
    plan_id: String,
    items: Vec<GcItem>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    intent: Intent,
    checksum: String,
}

impl HistoryStore {
    pub(crate) fn pins(&self) -> Result<BTreeSet<String>> {
        let Some(bytes) = read_record(&self.root.join(PINS))? else {
            return Ok(BTreeSet::new());
        };
        let record: Pins = serde_json::from_slice(&bytes)?;
        if record.format != 1 || record.project != self.project {
            return Err(invalid("pin record identity mismatch"));
        }
        let mut pins = BTreeSet::new();
        for id in record.versions {
            validate_id(&id)?;
            if !pins.insert(id.clone()) {
                return Err(invalid("duplicate history pin"));
            }
            // A dangling root is an integrity error, not permission to silently
            // discard a user's retention guarantee.
            self.get(&id)?;
        }
        Ok(pins)
    }

    pub fn set_pin(&self, params: &HistoryPinParams) -> Result<Vec<String>> {
        self.gc_require_idle()?;
        validate_id(&params.version_id)?;
        self.get(&params.version_id)?;
        let mut pins = self.pins()?;
        if params.pinned {
            pins.insert(params.version_id.clone());
        } else {
            pins.remove(&params.version_id);
        }
        let versions: Vec<_> = pins.into_iter().collect();
        write_record(
            &self.root,
            PINS,
            &Pins {
                format: 1,
                project: self.project.clone(),
                versions: versions.clone(),
            },
        )?;
        Ok(versions)
    }

    pub fn prune_plan(&self, policy: &HistoryRetentionPolicy) -> Result<HistoryPrunePlan> {
        self.gc_require_idle()?;
        let (plan, _) = self.gc_plan(policy)?;
        Ok(plan)
    }

    pub fn prune_apply(&self, params: &HistoryPruneApplyParams) -> Result<HistoryGcResult> {
        self.gc_require_idle()?;
        let (plan, items) = self.gc_plan(&params.policy)?;
        if plan.plan_id != params.expected_plan_id {
            return Err(invalid(
                "stale history prune plan; generate and review a new plan",
            ));
        }
        if items.is_empty() {
            return Ok(HistoryGcResult::default());
        }
        let intent = Intent {
            format: 1,
            project: self.project.clone(),
            plan_id: plan.plan_id,
            items,
        };
        self.gc_preflight(&intent)?;
        gc_fault("history_gc_before_journal")?;
        let checksum = digest(&serde_json::to_vec(&intent)?);
        write_record(&self.root, JOURNAL, &Journal { intent, checksum })?;
        gc_fault("history_gc_journal")?;
        // Read back the actual durable record through the same strict decoder
        // used after a process restart.
        self.gc_finish(false)
    }

    pub fn gc_resume(&self) -> Result<HistoryGcResult> {
        if read_record(&self.root.join(JOURNAL))?.is_none() {
            self.gc_no_unowned_trash(&BTreeSet::new())?;
            return Ok(HistoryGcResult::default());
        }
        self.gc_finish(true)
    }

    fn gc_require_idle(&self) -> Result<()> {
        if fs::symlink_metadata(self.root.join(JOURNAL)).is_ok() {
            return Err(Error::SessionBusy(
                "history garbage collection is pending; run history gc to resume".into(),
            ));
        }
        // Permission and I/O failures must not masquerade as an absent intent.
        read_record(&self.root.join(JOURNAL))?;
        Ok(())
    }

    fn gc_plan(&self, policy: &HistoryRetentionPolicy) -> Result<(HistoryPrunePlan, Vec<GcItem>)> {
        self.gc_no_unowned_trash(&BTreeSet::new())?;
        let pins = self.pins()?;
        let mut catalog = Vec::new();
        for version in self.list()? {
            let directory = self.version_dir(&version.id)?;
            let identity = directory_identity(&directory)?.expect("validated version directory");
            let bytes = read_record(&directory.join("manifest.json"))?
                .ok_or_else(|| invalid("missing history manifest"))?;
            catalog.push(CatalogVersion {
                version,
                manifest_digest: digest(&bytes),
                identity,
            });
        }
        let captures = self.gc_abandoned_captures()?;
        #[derive(Serialize)]
        struct PlanIdentity<'a> {
            format: u32,
            project: &'a Path,
            policy: &'a HistoryRetentionPolicy,
            catalog: &'a [CatalogVersion],
            pins: &'a BTreeSet<String>,
            captures: &'a [CaptureIdentity],
        }
        let plan_id = digest(&serde_json::to_vec(&PlanIdentity {
            format: 1,
            project: &self.project,
            policy,
            catalog: &catalog,
            pins: &pins,
            captures: &captures,
        })?);
        let mut retained_bytes = 0u64;
        for entry in &catalog {
            retained_bytes = add_bytes(retained_bytes, entry.version.logical_bytes)?;
        }
        let newest_start = catalog.len().saturating_sub(policy.keep_last);
        let mut remove = Vec::new();
        let mut retained = Vec::new();
        let mut items = Vec::new();
        let mut removed_bytes = 0u64;
        for (index, entry) in catalog.into_iter().enumerate() {
            let version = entry.version;
            let protected = index >= newest_start
                || pins.contains(&version.id)
                || policy
                    .older_than_ms
                    .is_some_and(|time| version.created_at_ms >= time);
            let budget_reached = policy
                .max_logical_bytes
                .is_some_and(|limit| retained_bytes <= limit);
            if protected || budget_reached {
                retained.push(version);
                continue;
            }
            retained_bytes -= version.logical_bytes;
            removed_bytes = add_bytes(removed_bytes, version.logical_bytes)?;
            items.push(GcItem {
                id: version.id.clone(),
                kind: ItemKind::Version,
                identity: entry.identity,
                manifest_digest: Some(entry.manifest_digest),
                logical_bytes: version.logical_bytes,
            });
            remove.push(version);
        }
        let abandoned_capture_ids = captures.iter().map(|entry| entry.id.clone()).collect();
        for capture in captures {
            items.push(GcItem {
                id: capture.id,
                kind: ItemKind::Capture,
                identity: capture.identity,
                manifest_digest: None,
                logical_bytes: 0,
            });
        }
        let plan = HistoryPrunePlan {
            plan_id,
            policy: policy.clone(),
            remove,
            retained,
            pinned: pins.into_iter().collect(),
            abandoned_capture_ids,
            removed_logical_bytes: removed_bytes,
            retained_logical_bytes: retained_bytes,
            budget_satisfied: policy
                .max_logical_bytes
                .is_none_or(|limit| retained_bytes <= limit),
        };
        Ok((plan, items))
    }

    fn gc_abandoned_captures(&self) -> Result<Vec<CaptureIdentity>> {
        let mut captures = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_prefix(".capture-"))
            else {
                continue;
            };
            validate_id(id)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            let identity = directory_identity(&entry.path())?
                .ok_or_else(|| invalid("abandoned capture disappeared"))?;
            captures.push(CaptureIdentity {
                id: id.to_string(),
                identity,
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            });
        }
        captures.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(captures)
    }

    fn gc_load(&self) -> Result<Intent> {
        let bytes = read_record(&self.root.join(JOURNAL))?
            .ok_or_else(|| invalid("history GC journal disappeared"))?;
        let journal: Journal = serde_json::from_slice(&bytes)?;
        let intent = journal.intent;
        if intent.format != 1
            || intent.project != self.project
            || !is_digest(&intent.plan_id)
            || journal.checksum != digest(&serde_json::to_vec(&intent)?)
            || intent.items.is_empty()
        {
            return Err(invalid("history GC journal identity or checksum mismatch"));
        }
        let mut ids = BTreeSet::new();
        let mut logical_bytes = 0u64;
        for item in &intent.items {
            validate_id(&item.id)?;
            if !ids.insert(item.id.clone()) {
                return Err(invalid("duplicate history GC journal item"));
            }
            match item.kind {
                ItemKind::Version if item.manifest_digest.as_deref().is_some_and(is_digest) => {}
                ItemKind::Capture if item.manifest_digest.is_none() && item.logical_bytes == 0 => {}
                _ => return Err(invalid("invalid history GC journal item")),
            }
            logical_bytes = add_bytes(logical_bytes, item.logical_bytes)?;
        }
        Ok(intent)
    }

    fn gc_no_unowned_trash(&self, owned: &BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(name) = name.to_str() {
                if name.starts_with(".trash-") && !owned.contains(name) {
                    return Err(invalid(
                        "history trash has no matching GC intent; refusing to delete it",
                    ));
                }
            }
        }
        Ok(())
    }

    fn gc_preflight(&self, intent: &Intent) -> Result<()> {
        let pins = self.pins()?;
        let mut owned = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for item in &intent.items {
            validate_id(&item.id)?;
            if !ids.insert(&item.id) || pins.contains(&item.id) {
                return Err(invalid(
                    "history GC intent duplicates an ID or includes a pinned version",
                ));
            }
            owned.insert(
                item.trash(&self.root)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            let source = item.source(&self.root);
            let trash = item.trash(&self.root);
            let live = directory_identity(&source)?;
            let retired = directory_identity(&trash)?;
            if live.is_some() && retired.is_some() {
                return Err(invalid("history GC source and trash both exist"));
            }
            let Some(actual) = live.as_ref().or(retired.as_ref()) else {
                continue;
            };
            if actual.device != item.identity.device || actual.inode != item.identity.inode {
                return Err(invalid("history GC directory identity changed"));
            }
            if let Some(expected) = &item.manifest_digest {
                let directory = if live.is_some() { &source } else { &trash };
                let bytes = read_record(&directory.join("manifest.json"))?;
                if live.is_some() {
                    let version = self.get(&item.id)?;
                    if version.logical_bytes != item.logical_bytes {
                        return Err(invalid("history GC manifest logical size changed"));
                    }
                    if bytes.is_none() {
                        return Err(invalid("history GC live manifest is missing"));
                    }
                }
                // Recursive unlink may have removed the manifest before a crash.
                // The retained directory inode is the identity in that state.
                if bytes.is_some_and(|bytes| digest(&bytes) != *expected) {
                    return Err(invalid("history GC manifest digest changed"));
                }
            }
        }
        self.gc_no_unowned_trash(&owned)
    }

    fn gc_finish(&self, resumed: bool) -> Result<HistoryGcResult> {
        let intent = self.gc_load()?;
        self.gc_preflight(&intent)?;
        let mut result = HistoryGcResult {
            resumed,
            ..Default::default()
        };
        for item in &intent.items {
            let source = item.source(&self.root);
            let trash = item.trash(&self.root);
            if directory_identity(&source)?.is_some() {
                fs::rename(&source, &trash)?;
                sync_dir(&self.root)?;
                gc_fault("history_gc_renamed")?;
            }
            crate::macos_tree::remove_path(&trash)?;
            sync_dir(&self.root)?;
            gc_fault("history_gc_deleted")?;
            match item.kind {
                ItemKind::Version => {
                    result.removed_version_ids.push(item.id.clone());
                    result.removed_logical_bytes =
                        add_bytes(result.removed_logical_bytes, item.logical_bytes)?;
                }
                ItemKind::Capture => result.removed_capture_ids.push(item.id.clone()),
            }
        }
        fs::remove_file(self.root.join(JOURNAL))?;
        sync_dir(&self.root)?;
        Ok(result)
    }
}

fn invalid(message: &str) -> Error {
    Error::Backend(message.into())
}

fn add_bytes(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| invalid("history logical byte count overflow"))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn directory_identity(path: &Path) -> Result<Option<DirectoryIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(DirectoryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }))
        }
        Ok(_) => Err(invalid(
            "history GC target must be a directory without symlinks",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_record(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(invalid(
                "history maintenance record must be a regular file without symlinks",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(invalid("history maintenance record must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn write_record(root: &Path, name: &str, record: &impl Serialize) -> Result<()> {
    let temporary = root.join(format!("{name}-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(record)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, root.join(name))?;
        sync_dir(root)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn gc_fault(point: &str) -> Result<()> {
    if cfg!(debug_assertions) {
        if std::env::var("DELTABOX_TEST_CRASH_AT").as_deref() == Ok(point) {
            std::process::abort();
        }
        if std::env::var("DELTABOX_TEST_FAIL_AT").as_deref() == Ok(point) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    fn fixture(
        sizes: &[usize],
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        HistoryStore,
        Vec<String>,
    ) {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let mut ids = Vec::new();
        for (index, size) in sizes.iter().enumerate() {
            fs::write(
                project.path().join("content"),
                vec![b'a' + index as u8; *size],
            )
            .unwrap();
            let version = store
                .capture("test-session", Some(format!("version {index}")))
                .unwrap();
            // Deterministic ordering and exact age-boundary tests, independent
            // of wall clock granularity or test execution speed.
            let mut manifest = store.manifest(&version.id).unwrap();
            manifest.version.created_at_ms = (index as u64 + 1) * 1000;
            fs::write(
                store.root.join(&version.id).join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            ids.push(version.id);
        }
        (project, storage, store, ids)
    }

    fn all() -> HistoryRetentionPolicy {
        HistoryRetentionPolicy {
            keep_last: 0,
            older_than_ms: None,
            max_logical_bytes: None,
        }
    }

    fn apply(store: &HistoryStore, plan: &HistoryPrunePlan) -> Result<HistoryGcResult> {
        store.prune_apply(&HistoryPruneApplyParams {
            policy: plan.policy.clone(),
            expected_plan_id: plan.plan_id.clone(),
        })
    }

    fn ids(versions: &[HistoryVersion]) -> Vec<String> {
        versions.iter().map(|version| version.id.clone()).collect()
    }

    fn child(
        store: &HistoryStore,
        storage: &Path,
        crash: Option<&str>,
        fail: Option<&str>,
        resume: bool,
    ) -> std::process::Output {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "history::gc::tests::gc_child", "--nocapture"])
            .env("DELTABOX_GC_CHILD_PROJECT", &store.project)
            .env("DELTABOX_GC_CHILD_STORAGE", storage);
        if let Some(point) = crash {
            command.env("DELTABOX_TEST_CRASH_AT", point);
        }
        if let Some(point) = fail {
            command.env("DELTABOX_TEST_FAIL_AT", point);
        }
        if resume {
            command.env("DELTABOX_GC_CHILD_RESUME", "1");
        }
        command.output().unwrap()
    }

    #[test]
    fn gc_child() {
        let Ok(project) = std::env::var("DELTABOX_GC_CHILD_PROJECT") else {
            return;
        };
        unsafe {
            libc::setrlimit(
                libc::RLIMIT_CORE,
                &libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                },
            );
        }
        let storage = std::env::var("DELTABOX_GC_CHILD_STORAGE").unwrap();
        let store = HistoryStore::open(Path::new(&storage), Path::new(&project)).unwrap();
        let result = if std::env::var_os("DELTABOX_GC_CHILD_RESUME").is_some() {
            store.gc_resume()
        } else {
            apply(&store, &store.prune_plan(&all()).unwrap())
        };
        if std::env::var_os("DELTABOX_TEST_FAIL_AT").is_some() {
            assert!(result.unwrap_err().to_string().contains("No space left"));
        } else {
            result.unwrap();
            assert!(
                std::env::var_os("DELTABOX_TEST_CRASH_AT").is_none(),
                "crash hook did not fire"
            );
        }
    }

    #[test]
    fn retention_combines_pins_newest_and_age_with_honest_budget_failure() {
        let (_project, storage, store, versions) = fixture(&[10, 20, 30, 40]);
        store
            .set_pin(&HistoryPinParams {
                version_id: versions[0].clone(),
                pinned: true,
            })
            .unwrap();
        let plan = store
            .prune_plan(&HistoryRetentionPolicy {
                keep_last: 1,
                older_than_ms: Some(3000),
                max_logical_bytes: Some(20),
            })
            .unwrap();
        assert_eq!(ids(&plan.remove), vec![versions[1].clone()]);
        assert_eq!(
            ids(&plan.retained),
            vec![
                versions[0].clone(),
                versions[2].clone(),
                versions[3].clone()
            ]
        );
        assert_eq!(plan.removed_logical_bytes, 20);
        assert_eq!(plan.retained_logical_bytes, 80);
        assert!(!plan.budget_satisfied);
        assert_eq!(
            store.list().unwrap().len(),
            4,
            "planning must not mutate versions"
        );
        let result = apply(&store, &plan).unwrap();
        assert_eq!(result.removed_version_ids, vec![versions[1].clone()]);
        assert!(!result.resumed);
        let reopened = HistoryStore::open(storage.path(), &store.project).unwrap();
        assert_eq!(
            reopened.pins().unwrap(),
            BTreeSet::from([versions[0].clone()])
        );
        assert_eq!(reopened.list().unwrap().len(), 3);
        assert!(!store.root.join(JOURNAL).exists());
    }

    #[test]
    fn logical_budget_removes_oldest_only_until_met_and_zero_keep_is_explicit() {
        let (_project, _storage, store, versions) = fixture(&[10, 20, 30, 40]);
        let plan = store
            .prune_plan(&HistoryRetentionPolicy {
                keep_last: 1,
                older_than_ms: None,
                max_logical_bytes: Some(75),
            })
            .unwrap();
        assert_eq!(ids(&plan.remove), versions[..2]);
        assert_eq!(plan.retained_logical_bytes, 70);
        assert!(plan.budget_satisfied);
        let default = store
            .prune_plan(&HistoryRetentionPolicy::default())
            .unwrap();
        assert_eq!(ids(&default.retained), versions[3..]);
        let all_plan = store.prune_plan(&all()).unwrap();
        assert!(all_plan.retained.is_empty());
        assert_eq!(
            apply(&store, &all_plan).unwrap().removed_version_ids.len(),
            4
        );
        assert!(store.list().unwrap().is_empty());
        assert!(store.gc_resume().unwrap().removed_version_ids.is_empty());
    }

    #[test]
    fn stale_plan_rejects_catalog_pin_policy_manifest_and_capture_changes() {
        let (_project, _storage, store, versions) = fixture(&[10, 20]);
        let plan = store.prune_plan(&all()).unwrap();
        store
            .set_pin(&HistoryPinParams {
                version_id: versions[0].clone(),
                pinned: true,
            })
            .unwrap();
        assert!(apply(&store, &plan)
            .unwrap_err()
            .to_string()
            .contains("stale"));
        store
            .set_pin(&HistoryPinParams {
                version_id: versions[0].clone(),
                pinned: false,
            })
            .unwrap();
        let plan = store.prune_plan(&all()).unwrap();
        let mut other_policy = plan.clone();
        other_policy.policy.keep_last = 1;
        assert!(apply(&store, &other_policy).is_err());
        store.capture("new-session", None).unwrap();
        assert!(apply(&store, &plan).is_err());
        let plan = store.prune_plan(&all()).unwrap();
        let mut manifest = store.manifest(&versions[0]).unwrap();
        manifest.version.label = Some("changed manifest".into());
        fs::write(
            store.root.join(&versions[0]).join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(apply(&store, &plan).is_err());
        let plan = store.prune_plan(&all()).unwrap();
        let orphan = store.root.join(format!(".capture-{}", Uuid::new_v4()));
        fs::create_dir(&orphan).unwrap();
        assert!(apply(&store, &plan).is_err());
        let plan = store.prune_plan(&all()).unwrap();
        fs::write(orphan.join("late-write"), b"new").unwrap();
        assert!(apply(&store, &plan).is_err());
        assert_eq!(store.list().unwrap().len(), 3);
        assert!(!store.root.join(JOURNAL).exists());
    }

    #[test]
    fn abandoned_captures_and_readonly_trees_are_reclaimed_without_following_links() {
        let (project, _storage, store, versions) = fixture(&[10]);
        let orphan_id = Uuid::new_v4().to_string();
        let orphan = store.root.join(format!(".capture-{orphan_id}"));
        fs::create_dir(&orphan).unwrap();
        symlink(project.path(), orphan.join("external-project")).unwrap();
        fs::set_permissions(
            store.root.join(&versions[0]).join("tree"),
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        let plan = store.prune_plan(&all()).unwrap();
        assert_eq!(plan.abandoned_capture_ids, vec![orphan_id.clone()]);
        let result = apply(&store, &plan).unwrap();
        assert_eq!(result.removed_capture_ids, vec![orphan_id]);
        assert_eq!(
            fs::read(project.path().join("content")).unwrap(),
            vec![b'a'; 10]
        );
        assert!(!orphan.exists());
        assert!(!store.root.join(&versions[0]).exists());
    }

    #[test]
    fn rejects_malformed_pins_and_unsafe_capture_names_or_symlinks() {
        let (project, _storage, store, versions) = fixture(&[10]);
        for record in [
            serde_json::json!({"format":1,"project":store.project,"versions":["../escape"]}),
            serde_json::json!({"format":1,"project":store.project,"versions":[versions[0],versions[0]]}),
            serde_json::json!({"format":1,"project":store.project,"versions":[Uuid::new_v4().to_string()]}),
            serde_json::json!({"format":1,"project":"/wrong-project","versions":[]}),
        ] {
            fs::write(store.root.join(PINS), serde_json::to_vec(&record).unwrap()).unwrap();
            assert!(store.prune_plan(&all()).is_err());
        }
        fs::remove_file(store.root.join(PINS)).unwrap();
        symlink(project.path().join("content"), store.root.join(PINS)).unwrap();
        assert!(store.pins().is_err());
        fs::remove_file(store.root.join(PINS)).unwrap();
        let malformed = store.root.join(".capture-not-a-uuid");
        fs::create_dir(&malformed).unwrap();
        assert!(store.prune_plan(&all()).is_err());
        fs::remove_dir(malformed).unwrap();
        let link = store.root.join(format!(".capture-{}", Uuid::new_v4()));
        symlink(project.path(), &link).unwrap();
        assert!(store.prune_plan(&all()).is_err());
        assert!(project.path().join("content").exists());
    }

    #[test]
    fn nonregular_maintenance_records_do_not_block_and_journal_paths_fail_closed() {
        let (_project, storage, store, _versions) = fixture(&[10]);
        for name in [PINS, JOURNAL] {
            let path = store.root.join(name);
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            assert!(read_record(&path).is_err());
            fs::remove_file(path).unwrap();
        }
        assert!(!child(
            &store,
            storage.path(),
            Some("history_gc_journal"),
            None,
            false
        )
        .status
        .success());
        let original = fs::read(store.root.join(JOURNAL)).unwrap();
        for case in ["unsafe_id", "duplicate", "wrong_project", "wrong_kind"] {
            let mut journal: Journal = serde_json::from_slice(&original).unwrap();
            match case {
                "unsafe_id" => journal.intent.items[0].id = "../outside".into(),
                "duplicate" => journal.intent.items.push(journal.intent.items[0].clone()),
                "wrong_project" => journal.intent.project = PathBuf::from("/wrong-project"),
                "wrong_kind" => journal.intent.items[0].kind = ItemKind::Capture,
                _ => unreachable!(),
            }
            // Even with a consistent checksum, schema and path invariants
            // must be checked independently before touching filesystem data.
            journal.checksum = digest(&serde_json::to_vec(&journal.intent).unwrap());
            fs::write(
                store.root.join(JOURNAL),
                serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            assert!(store.gc_resume().is_err(), "{case}");
            assert_eq!(store.list().unwrap().len(), 1);
        }
        fs::write(store.root.join(JOURNAL), original).unwrap();
        store.gc_resume().unwrap();
    }

    #[test]
    fn journal_rename_and_delete_crashes_resume_idempotently_and_preserve_pins() {
        for point in [
            "history_gc_journal",
            "history_gc_renamed",
            "history_gc_deleted",
        ] {
            let (project, storage, store, versions) = fixture(&[10, 20, 30]);
            store
                .set_pin(&HistoryPinParams {
                    version_id: versions[1].clone(),
                    pinned: true,
                })
                .unwrap();
            let output = child(&store, storage.path(), Some(point), None, false);
            assert!(
                !output.status.success(),
                "{point} should terminate the worker"
            );
            assert!(store.root.join(JOURNAL).exists());
            assert!(store
                .prune_plan(&all())
                .unwrap_err()
                .to_string()
                .contains("pending"));
            assert!(store
                .set_pin(&HistoryPinParams {
                    version_id: versions[1].clone(),
                    pinned: false
                })
                .is_err());
            let newest = store.capture("capture-after-crash", None).unwrap();
            let reopened = HistoryStore::open(storage.path(), project.path()).unwrap();
            let result = reopened.gc_resume().unwrap();
            assert!(result.resumed);
            assert_eq!(
                result.removed_version_ids,
                vec![versions[0].clone(), versions[2].clone()]
            );
            assert_eq!(result.removed_logical_bytes, 40);
            assert_eq!(
                ids(&reopened.list().unwrap()),
                vec![versions[1].clone(), newest.id]
            );
            assert!(reopened.tree(&versions[1]).is_ok());
            assert!(!reopened.root.join(JOURNAL).exists());
            assert!(reopened.gc_resume().unwrap().removed_version_ids.is_empty());
            assert_eq!(
                fs::read(project.path().join("content")).unwrap(),
                vec![b'c'; 30]
            );
        }
    }

    #[test]
    fn recovery_can_crash_again_and_resume_after_partial_recursive_unlink() {
        let (_project, storage, store, versions) = fixture(&[10, 20]);
        assert!(!child(
            &store,
            storage.path(),
            Some("history_gc_journal"),
            None,
            false
        )
        .status
        .success());
        assert!(!child(
            &store,
            storage.path(),
            Some("history_gc_renamed"),
            None,
            true
        )
        .status
        .success());
        let trash = store.root.join(format!(".trash-{}", versions[0]));
        assert!(trash.exists());
        fs::remove_file(trash.join("manifest.json")).unwrap();
        fs::remove_file(trash.join("tree/content")).unwrap();
        assert!(!child(
            &store,
            storage.path(),
            Some("history_gc_deleted"),
            None,
            true
        )
        .status
        .success());
        assert!(store.root.join(JOURNAL).exists());
        assert_eq!(store.gc_resume().unwrap().removed_version_ids, versions);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn injected_io_failures_preserve_intent_and_are_retryable() {
        for point in [
            "history_gc_before_journal",
            "history_gc_journal",
            "history_gc_renamed",
            "history_gc_deleted",
        ] {
            let (_project, storage, store, versions) = fixture(&[10, 20]);
            let output = child(&store, storage.path(), None, Some(point), false);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if point == "history_gc_before_journal" {
                assert!(!store.root.join(JOURNAL).exists());
                assert_eq!(store.list().unwrap().len(), 2);
                apply(&store, &store.prune_plan(&all()).unwrap()).unwrap();
            } else {
                assert!(store.root.join(JOURNAL).exists());
                assert_eq!(store.gc_resume().unwrap().removed_version_ids, versions);
            }
            assert!(store.list().unwrap().is_empty());
        }
    }

    #[test]
    fn recovery_rejects_journal_corruption_source_replacement_and_both_names() {
        let (_project, storage, store, versions) = fixture(&[10, 20]);
        assert!(!child(
            &store,
            storage.path(),
            Some("history_gc_journal"),
            None,
            false
        )
        .status
        .success());
        let original = fs::read(store.root.join(JOURNAL)).unwrap();
        fs::write(store.root.join(JOURNAL), b"{broken").unwrap();
        assert!(store.gc_resume().is_err());
        fs::write(store.root.join(JOURNAL), &original).unwrap();
        let mut journal: Journal = serde_json::from_slice(&original).unwrap();
        journal.intent.items[0].id = versions[1].clone();
        fs::write(
            store.root.join(JOURNAL),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        assert!(
            store.gc_resume().is_err(),
            "checksum mismatch must fail closed"
        );
        fs::write(store.root.join(JOURNAL), &original).unwrap();
        let first = store.root.join(&versions[0]);
        let preserved = store.root.join(".preserved-version");
        fs::rename(&first, &preserved).unwrap();
        fs::create_dir(&first).unwrap();
        assert!(store
            .gc_resume()
            .unwrap_err()
            .to_string()
            .contains("identity changed"));
        fs::remove_dir(&first).unwrap();
        fs::rename(&preserved, &first).unwrap();
        let trash = store.root.join(format!(".trash-{}", versions[0]));
        fs::create_dir(&trash).unwrap();
        assert!(store
            .gc_resume()
            .unwrap_err()
            .to_string()
            .contains("both exist"));
        fs::remove_dir(&trash).unwrap();
        assert_eq!(
            store.list().unwrap().len(),
            2,
            "preflight must validate all before removal"
        );
        assert_eq!(store.gc_resume().unwrap().removed_version_ids, versions);
    }

    #[test]
    fn recovery_refuses_modified_live_manifest_and_unowned_trash() {
        let (_project, storage, store, versions) = fixture(&[10, 20]);
        assert!(!child(
            &store,
            storage.path(),
            Some("history_gc_journal"),
            None,
            false
        )
        .status
        .success());
        let path = store.root.join(&versions[1]).join("manifest.json");
        let original = fs::read(&path).unwrap();
        let mut manifest = store.manifest(&versions[1]).unwrap();
        manifest.version.label = Some("changed after committed plan".into());
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(store
            .gc_resume()
            .unwrap_err()
            .to_string()
            .contains("digest changed"));
        assert_eq!(store.list().unwrap().len(), 2);
        fs::write(path, original).unwrap();
        store.gc_resume().unwrap();
        let stray = store.root.join(format!(".trash-{}", Uuid::new_v4()));
        fs::create_dir(&stray).unwrap();
        assert!(store.prune_plan(&all()).is_err());
        assert!(store.gc_resume().is_err());
        assert!(stray.exists());
    }
}

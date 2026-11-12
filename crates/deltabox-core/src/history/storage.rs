//! Read-only accounting and integrity checks for the history namespace.
//!
//! `st_blocks` measures the blocks referenced by an inode. APFS clone-shared
//! extents can appear in multiple inodes, so these counters are deliberately not
//! a reclaimable-space estimate. Hard links, unlike clone-shared extents, can be
//! deduplicated exactly by (device, inode).
use super::*;
use crate::history_storage::{
    HistoryStorageStats, HistoryStorageUsage, HistoryVerifyEntry, HistoryVerifyResult,
    HistoryVersionStorage,
};
use std::collections::HashSet;

impl HistoryStore {
    pub fn storage_stats(&self) -> Result<HistoryStorageStats> {
        let pins = self.pins()?;
        let mut versions = Vec::new();
        let mut abandoned_captures = UsageAccumulator::default();
        let mut garbage = UsageAccumulator::default();
        let mut logical_bytes = 0;
        for item in fs::read_dir(&self.root)? {
            let item = item?;
            let name = item.file_name();
            let name = name.to_str().ok_or_else(|| {
                Error::Backend("history namespace contains a non-UTF-8 name".into())
            })?;
            if let Some(id) = name.strip_prefix(".capture-") {
                require_artifact_directory(&item.path(), id)?;
                abandoned_captures.add_tree(&item.path())?;
            } else if let Some(id) = name
                .strip_prefix(".trash-capture-")
                .or_else(|| name.strip_prefix(".trash-"))
            {
                require_artifact_directory(&item.path(), id)?;
                garbage.add_tree(&item.path())?;
            } else if !name.starts_with('.') {
                let version = self.get(name).map_err(|error| {
                    Error::Backend(format!("cannot account history version {name}: {error}"))
                })?;
                checked_add(&mut logical_bytes, version.logical_bytes)?;
                let mut usage = UsageAccumulator::default();
                usage.add_tree(&item.path())?;
                versions.push(HistoryVersionStorage {
                    pinned: pins.contains(&version.id),
                    version,
                    usage: usage.usage,
                });
            }
        }
        versions.sort_by(|a, b| {
            a.version
                .created_at_ms
                .cmp(&b.version.created_at_ms)
                .then(a.version.id.cmp(&b.version.id))
        });
        let mut total = UsageAccumulator::default();
        // A fresh accumulator is essential: hard links can cross versions and
        // maintenance directories, so summing per-version usage overcounts.
        total.add_tree(&self.root)?;
        let pending_gc = match fs::symlink_metadata(self.root.join(".gc-journal.json")) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        Ok(HistoryStorageStats {
            versions,
            logical_bytes,
            total: total.usage,
            abandoned_captures: abandoned_captures.usage,
            garbage: garbage.usage,
            pending_gc,
            allocated_bytes_are_exclusive: false,
        })
    }

    /// Verify the stored snapshot, not the current project. A broken version is
    /// retained in the report while verification continues for other versions.
    pub fn verify_versions(&self, version_id: Option<&str>) -> Result<HistoryVerifyResult> {
        let ids = if let Some(id) = version_id {
            validate_id(id)?;
            vec![id.to_string()]
        } else {
            let mut ids = Vec::new();
            for item in fs::read_dir(&self.root)? {
                let item = item?;
                let name = item.file_name();
                let name = name.to_str().ok_or_else(|| {
                    Error::Backend("history namespace contains a non-UTF-8 name".into())
                })?;
                if !name.starts_with('.') {
                    ids.push(name.to_string());
                }
            }
            ids.sort();
            ids
        };
        let versions: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let result = self.tree(&id);
                HistoryVerifyEntry {
                    version_id: id,
                    valid: result.is_ok(),
                    error: result.err().map(|error| error.to_string()),
                }
            })
            .collect();
        Ok(HistoryVerifyResult {
            valid: versions.iter().all(|version| version.valid),
            versions,
        })
    }
}

fn require_artifact_directory(path: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(Error::Backend(format!(
            "history maintenance artifact must be a directory without symlinks: {}",
            path.display()
        )));
    }
    Ok(())
}

fn checked_add(target: &mut u64, value: u64) -> Result<()> {
    *target = target
        .checked_add(value)
        .ok_or_else(|| Error::Backend("history storage counter overflow".into()))?;
    Ok(())
}

#[derive(Default)]
struct UsageAccumulator {
    usage: HistoryStorageUsage,
    inodes: HashSet<(u64, u64)>,
}

impl UsageAccumulator {
    fn add_tree(&mut self, path: &Path) -> Result<()> {
        // walkdir does not follow interior symlinks. Disable following the root
        // explicitly as well, since hidden, unrecognized entries may be links.
        for item in walkdir::WalkDir::new(path)
            .follow_links(false)
            .follow_root_links(false)
        {
            let item = item.map_err(|error| Error::Io(std::io::Error::other(error)))?;
            let metadata = fs::symlink_metadata(item.path())?;
            if metadata.is_file() {
                checked_add(&mut self.usage.files, 1)?;
                checked_add(&mut self.usage.apparent_bytes, metadata.len())?;
            } else if metadata.is_dir() {
                checked_add(&mut self.usage.directories, 1)?;
            } else if metadata.file_type().is_symlink() {
                checked_add(&mut self.usage.symlinks, 1)?;
            } else {
                return Err(Error::Unsupported(format!(
                    "unsupported history storage entry: {}",
                    item.path().display()
                )));
            }
            if self.inodes.insert((metadata.dev(), metadata.ino())) {
                let bytes = metadata
                    .blocks()
                    .checked_mul(512)
                    .ok_or_else(|| Error::Backend("history storage counter overflow".into()))?;
                checked_add(&mut self.usage.allocated_bytes, bytes)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn accounting_distinguishes_apparent_hardlink_and_clone_storage() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join("source"), vec![7; 65_536]).unwrap();
        fs::hard_link(project.path().join("source"), project.path().join("alias")).unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let first = store.capture("test", None).unwrap();
        let second = store.capture("test", None).unwrap();
        let before = store.storage_stats().unwrap();
        assert_eq!(before.logical_bytes, 4 * 65_536);
        assert!(!before.allocated_bytes_are_exclusive);
        for version in &before.versions {
            assert_eq!(version.usage.files, 3); // two paths plus manifest
            let dir = store.root.join(&version.version.id);
            let manifest = fs::symlink_metadata(dir.join("manifest.json")).unwrap();
            let source = fs::symlink_metadata(dir.join("tree/source")).unwrap();
            let tree = fs::symlink_metadata(dir.join("tree")).unwrap();
            let version_dir = fs::symlink_metadata(&dir).unwrap();
            assert_eq!(version.usage.apparent_bytes, 2 * 65_536 + manifest.len());
            assert_eq!(
                version.usage.allocated_bytes,
                (source.blocks() + manifest.blocks() + tree.blocks() + version_dir.blocks()) * 512
            );
        }

        // Share a physical inode across versions. Per-version usage is unchanged
        // but the whole-store total must count its blocks just once.
        let old_source = store.root.join(&second.id).join("tree/source");
        let old_blocks = fs::metadata(&old_source).unwrap().blocks() * 512;
        assert!(old_blocks > 0);
        let old_alias = store.root.join(&second.id).join("tree/alias");
        fs::remove_file(&old_source).unwrap();
        fs::remove_file(&old_alias).unwrap();
        fs::hard_link(store.root.join(&first.id).join("tree/source"), &old_source).unwrap();
        fs::hard_link(&old_source, &old_alias).unwrap();
        let after = store.storage_stats().unwrap();
        assert_eq!(after.total.apparent_bytes, before.total.apparent_bytes);
        assert_eq!(after.total.files, before.total.files);
        assert_eq!(
            after.total.allocated_bytes + old_blocks,
            before.total.allocated_bytes
        );
        assert_eq!(after.versions[0].usage, before.versions[0].usage);
        assert_eq!(after.versions[1].usage, before.versions[1].usage);
        assert!(store.verify_versions(None).unwrap().valid);
        store
            .set_pin(&crate::history_storage::HistoryPinParams {
                version_id: first.id.clone(),
                pinned: true,
            })
            .unwrap();
        let pinned = store.storage_stats().unwrap();
        assert_eq!(pinned.logical_bytes, after.logical_bytes);
        assert_eq!(pinned.total.files, after.total.files + 1);
        assert!(pinned.total.apparent_bytes > after.total.apparent_bytes);
        for version in &pinned.versions {
            assert_eq!(version.pinned, version.version.id == first.id);
        }
    }

    #[test]
    fn accounting_includes_sparse_artifacts_but_never_follows_symlinks() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), vec![8; 1_048_576]).unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let capture = store.root.join(format!(".capture-{}", Uuid::new_v4()));
        fs::create_dir(&capture).unwrap();
        let sparse = File::create(capture.join("partial")).unwrap();
        sparse.set_len(32 * 1_048_576).unwrap();
        symlink(outside.path(), capture.join("outside")).unwrap();
        symlink(outside.path(), store.root.join(".unknown-link")).unwrap();
        let trash = store.root.join(format!(".trash-{}", Uuid::new_v4()));
        fs::create_dir(&trash).unwrap();
        fs::write(trash.join("retired"), b"trash").unwrap();
        let capture_trash = store
            .root
            .join(format!(".trash-capture-{}", Uuid::new_v4()));
        fs::create_dir(&capture_trash).unwrap();
        fs::write(capture_trash.join("abandoned"), b"partial").unwrap();
        fs::write(store.root.join(".gc-journal.json"), b"pending intent").unwrap();

        let stats = store.storage_stats().unwrap();
        assert!(stats.versions.is_empty());
        assert_eq!(stats.logical_bytes, 0);
        assert!(stats.pending_gc);
        assert_eq!(stats.abandoned_captures.apparent_bytes, 32 * 1_048_576);
        assert!(stats.abandoned_captures.allocated_bytes < stats.abandoned_captures.apparent_bytes);
        assert_eq!(stats.abandoned_captures.files, 1);
        assert_eq!(stats.abandoned_captures.symlinks, 1);
        assert_eq!(stats.garbage.apparent_bytes, 12);
        assert_eq!(stats.garbage.files, 2);
        assert_eq!(stats.total.apparent_bytes, 32 * 1_048_576 + 12 + 14);
        assert_eq!(stats.total.files, 4);
        assert_eq!(stats.total.symlinks, 2);
        assert_eq!(
            fs::read(outside.path().join("sentinel")).unwrap(),
            vec![8; 1_048_576]
        );
    }

    #[test]
    fn accounting_rejects_malformed_artifacts_and_published_symlinks() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        fs::create_dir(store.root.join(".capture-not-a-version")).unwrap();
        assert!(store.storage_stats().is_err());
        fs::remove_dir(store.root.join(".capture-not-a-version")).unwrap();
        let artifact = store.root.join(format!(".capture-{}", Uuid::new_v4()));
        symlink(outside.path(), &artifact).unwrap();
        assert!(store.storage_stats().is_err());
        fs::remove_file(&artifact).unwrap();
        let published = Uuid::new_v4().to_string();
        symlink(outside.path(), store.root.join(&published)).unwrap();
        assert!(store.storage_stats().is_err());
        let checked = store.verify_versions(None).unwrap();
        assert!(!checked.valid);
        assert_eq!(checked.versions.len(), 1);
        assert_eq!(checked.versions[0].version_id, published);
        assert!(!checked.versions[0].valid);
    }

    #[test]
    fn scrub_rejects_unrecorded_ignored_and_empty_paths_in_scoped_snapshots() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join(".gitignore"), b"*.log\n").unwrap();
        fs::write(project.path().join("source"), b"source").unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = store.capture("test", None).unwrap();
        let tree = store.root.join(&version.id).join("tree");
        assert!(store.verify_versions(Some(&version.id)).unwrap().valid);
        for name in ["ignored.log", ".git", ".deltabox", "empty-directory"] {
            let injected = tree.join(name);
            if name == "ignored.log" {
                fs::write(&injected, b"injected bytes").unwrap();
            } else {
                fs::create_dir(&injected).unwrap();
            }
            let result = store.verify_versions(Some(&version.id)).unwrap();
            assert!(!result.valid, "unrecorded path must fail scrub: {name}");
            assert!(result.versions[0]
                .error
                .as_ref()
                .unwrap()
                .contains("integrity"));
            crate::macos_tree::remove_path(&injected).unwrap();
            assert!(store.verify_versions(Some(&version.id)).unwrap().valid);
        }
    }

    #[test]
    fn scrub_reports_independent_corruptions_and_does_not_repair_or_read_live_files() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join("source"), b"original").unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let valid = store.capture("test", Some("healthy".into())).unwrap();
        let body = store.capture("test", None).unwrap();
        let manifest = store.capture("test", None).unwrap();
        let metadata = store.capture("test", None).unwrap();
        let counter = store.capture("test", None).unwrap();
        fs::write(store.root.join(&body.id).join("tree/source"), b"tampered").unwrap();
        fs::write(
            store.root.join(&manifest.id).join("manifest.json"),
            b"{invalid json",
        )
        .unwrap();
        fs::set_permissions(
            store.root.join(&metadata.id).join("tree/source"),
            fs::Permissions::from_mode(0o711),
        )
        .unwrap();
        let counter_manifest = store.root.join(&counter.id).join("manifest.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&counter_manifest).unwrap()).unwrap();
        document["version"]["logical_bytes"] = 999.into();
        fs::write(&counter_manifest, serde_json::to_vec(&document).unwrap()).unwrap();
        // History scrub must not confuse current project divergence with damage
        // to stored history, even when the current file becomes a dangling link.
        fs::remove_file(project.path().join("source")).unwrap();
        symlink("absent", project.path().join("source")).unwrap();
        let corrupt_before = fs::read(store.root.join(&manifest.id).join("manifest.json")).unwrap();
        let names_before: BTreeSet<_> = fs::read_dir(&store.root)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();

        let result = store.verify_versions(None).unwrap();
        assert!(!result.valid);
        assert_eq!(result.versions.len(), 5);
        for item in result.versions {
            assert_eq!(item.valid, item.version_id == valid.id);
            assert_eq!(item.error.is_some(), !item.valid);
        }
        assert!(store.verify_versions(Some(&valid.id)).unwrap().valid);
        assert!(
            !store
                .verify_versions(Some(&Uuid::new_v4().to_string()))
                .unwrap()
                .valid
        );
        assert!(store.verify_versions(Some("../outside")).is_err());
        assert!(store.storage_stats().is_err());
        assert_eq!(
            fs::read(store.root.join(&manifest.id).join("manifest.json")).unwrap(),
            corrupt_before
        );
        let names_after: BTreeSet<_> = fs::read_dir(&store.root)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        assert_eq!(names_before, names_after);
        assert_eq!(
            fs::read_link(project.path().join("source")).unwrap(),
            Path::new("absent")
        );
    }

    #[test]
    fn legacy_accounting_reports_projected_logical_bytes_and_all_stored_bytes() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(project.path().join(".gitignore"), b"target/\n").unwrap();
        fs::write(project.path().join("source"), b"source").unwrap();
        fs::create_dir(project.path().join("target")).unwrap();
        fs::write(project.path().join("target/generated"), vec![9; 4096]).unwrap();
        let store = HistoryStore::open(storage.path(), project.path()).unwrap();
        let version = store.capture("test", None).unwrap();
        let dir = store.version_dir(&version.id).unwrap();
        let mut legacy = store.manifest(&version.id).unwrap();
        crate::macos_tree::remove_path(&dir.join("tree")).unwrap();
        clone_tree(project.path(), &dir.join("tree")).unwrap();
        legacy.entries = scan(&dir.join("tree")).unwrap();
        legacy.format = 1;
        legacy.scope = None;
        let original = serde_json::to_vec(&legacy).unwrap();
        fs::write(dir.join("manifest.json"), &original).unwrap();

        let stats = store.storage_stats().unwrap();
        assert_eq!(stats.logical_bytes, version.logical_bytes);
        assert_eq!(stats.logical_bytes, 14);
        assert_eq!(
            stats.versions[0].usage.apparent_bytes,
            14 + 4096 + original.len() as u64
        );
        assert!(store.verify_versions(None).unwrap().valid);
        assert_eq!(fs::read(dir.join("manifest.json")).unwrap(), original);
        assert_eq!(
            fs::read(dir.join("tree/target/generated")).unwrap(),
            vec![9; 4096]
        );
    }
}

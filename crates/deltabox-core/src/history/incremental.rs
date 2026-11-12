//! Conservative regular-file restore transactions. Keep full-tree fingerprints
//! for conflict detection, but never replace untouched entries. All mutations
//! are same-filesystem atomic renames from durable, private staging files.
use super::*;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd};

const PLAN: &str = "files-plan.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileRestorePlan {
    version: u32,
    before: Index,
    base_before: Index,
    after: Index,
    host_paths: BTreeSet<String>,
    base_paths: BTreeSet<String>,
}

impl FileRestorePlan {
    /// No mutations on rejection: structural/metadata/link changes retain the
    /// existing full-tree transaction. Baseline-only changes must qualify too.
    pub(crate) fn plan(
        project: &Path,
        base: &Path,
        target: &Path,
        scope: &SourceScope,
        transaction_parent: &Path,
    ) -> Result<Option<Self>> {
        let target_entries = scan_scoped(target, scope)?;
        let selected = scan_scoped(project, scope)?;
        let host_paths = changed_paths(&selected, &target_entries);
        if !ordinary_changes(&selected, &target_entries, &host_paths) {
            return Ok(None);
        }
        let before = scan(project)?;
        if selected
            .iter()
            .any(|(path, entry)| before.get(path) != Some(entry))
        {
            // A hard-link group can cross the filtered scope. The full-tree
            // path understands that topology; do not normalize it away here.
            return Ok(None);
        }
        let mut after = before.clone();
        for path in &host_paths {
            after.insert(path.clone(), target_entries[path].clone());
        }
        let base_before = scan(base)?;
        let base_paths = changed_paths(&base_before, &after);
        if !ordinary_changes(&base_before, &after, &base_paths) {
            return Ok(None);
        }
        let device = fs::metadata(transaction_parent)?.dev();
        if fs::metadata(project)?.dev() != device || fs::metadata(base)?.dev() != device {
            return Ok(None);
        }
        for (root, paths) in [(project, &host_paths), (base, &base_paths)] {
            for path in paths {
                if !ordinary_path(root, path, device)? {
                    return Ok(None);
                }
            }
        }
        for path in &host_paths {
            // Include out-of-project hard links, which aren't in semantic indexes.
            if fs::symlink_metadata(target.join(path))?.nlink() != 1 {
                return Ok(None);
            }
        }
        let plan = Self {
            version: 1,
            before,
            base_before,
            after,
            host_paths,
            base_paths,
        };
        plan.validate()?;
        Ok(Some(plan))
    }

    pub(crate) fn prepare(
        &self,
        project: &Path,
        base: &Path,
        target: &Path,
        tx: &Path,
    ) -> Result<()> {
        let mut timings = RestoreTimings::new();
        for path in &self.host_paths {
            clone_file(&project.join(path), &tx.join("host-backup").join(path))?;
            fault("macos_history_files_backup")?;
        }
        for path in &self.base_paths {
            clone_file(&base.join(path), &tx.join("base-backup").join(path))?;
        }
        for path in self.host_paths.union(&self.base_paths) {
            let source = if self.host_paths.contains(path) {
                target
            } else {
                project
            };
            clone_file(&source.join(path), &tx.join("after").join(path))?;
        }
        for (name, paths) in [
            ("host-ready", &self.host_paths),
            ("base-ready", &self.base_paths),
        ] {
            for path in paths {
                clone_file(&tx.join("after").join(path), &tx.join(name).join(path))?;
            }
        }
        timings.mark("files.copy_changed_files");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tx.join(PLAN))?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        sync_tree(tx)?;
        sync_dir(tx.parent().expect("transaction parent"))?;
        self.verify_backups(tx)?;
        timings.mark("files.sync_and_verify_backups");
        // Retain the full-transaction durability boundary for untouched data and
        // the accepted baseline too. Avoid new clones/fsyncs of every output;
        // do NOT weaken strict guarantees by merely dropping all tree flushes.
        sync_tree(project)?;
        timings.mark("files.sync_existing_host");
        sync_tree(base)?;
        timings.mark("files.sync_existing_base");
        fault("macos_history_files_before_verify")?;
        self.verify_before(project, base)
    }

    pub(crate) fn verify_before(&self, project: &Path, base: &Path) -> Result<()> {
        if scan(project)? != self.before || scan(base)? != self.base_before {
            return Err(conflict(
                "project or accepted baseline changed before file restore",
            ));
        }
        let device = fs::metadata(project)?.dev();
        for (root, paths) in [(project, &self.host_paths), (base, &self.base_paths)] {
            for path in paths {
                if !ordinary_path(root, path, device)? {
                    return Err(conflict("file restore path or hard-link topology changed"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn publish_host(&self, project: &Path, tx: &Path) -> Result<()> {
        self.publish(project, tx, "host-ready", &self.host_paths, "host")
    }

    pub(crate) fn publish_base(&self, base: &Path, tx: &Path) -> Result<()> {
        self.publish(base, tx, "base-ready", &self.base_paths, "base")
    }

    fn publish(
        &self,
        root: &Path,
        tx: &Path,
        name: &str,
        paths: &BTreeSet<String>,
        kind: &str,
    ) -> Result<()> {
        for (i, path) in paths.iter().enumerate() {
            replace_file(&tx.join(name).join(path), root, path)?;
            fault(&format!("macos_history_files_{kind}_{}", i + 1))?;
        }
        Ok(())
    }

    pub(crate) fn verify_after(&self, project: &Path, base: &Path) -> Result<()> {
        if scan(project)? != self.after || scan(base)? != self.after {
            return Err(conflict(
                "project or accepted baseline changed during file restore",
            ));
        }
        Ok(())
    }

    pub(crate) fn recover(project: &Path, base: &Path, tx: &Path) -> Result<()> {
        let plan: Self = serde_json::from_slice(&fs::read(tx.join(PLAN))?)?;
        plan.validate()?;
        plan.verify_backups(tx)?;
        // All untouched entries (including ignored files and Git metadata) must
        // still match. Touched paths may be old OR new after atomic renames.
        verify_partial(project, &plan.before, &plan.after, &plan.host_paths)?;
        verify_partial(base, &plan.base_before, &plan.after, &plan.base_paths)?;
        for (root, before, paths, backup, kind) in [
            (
                base,
                &plan.base_before,
                &plan.base_paths,
                "base-backup",
                "base",
            ),
            (
                project,
                &plan.before,
                &plan.host_paths,
                "host-backup",
                "host",
            ),
        ] {
            for (i, path) in paths.iter().enumerate() {
                let ready = tx.join(format!("recover-{kind}")).join(path);
                crate::macos_tree::remove_path(&ready)?;
                clone_file(&tx.join(backup).join(path), &ready)?;
                File::open(&ready)?.sync_all()?;
                // A crash before this rename changes no destination. Backups
                // remain durable and immutable for any number of recovery tries.
                replace_file(&ready, root, path)?;
                fault(&format!("macos_history_files_recovery_{kind}_{}", i + 1))?;
            }
            if scan(root)? != *before {
                return Err(conflict("project changed during file restore recovery"));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Backend("unknown file restore plan version".into()));
        }
        for index in [&self.before, &self.base_before, &self.after] {
            for path in index.keys() {
                validate_path(path)?;
            }
        }
        if self.host_paths != changed_paths(&self.before, &self.after)
            || self.base_paths != changed_paths(&self.base_before, &self.after)
            || !ordinary_changes(&self.before, &self.after, &self.host_paths)
            || !ordinary_changes(&self.base_before, &self.after, &self.base_paths)
        {
            return Err(Error::Backend(
                "invalid file restore transaction plan".into(),
            ));
        }
        Ok(())
    }

    fn verify_backups(&self, tx: &Path) -> Result<()> {
        for (dir, paths, expected) in [
            ("host-backup", &self.host_paths, &self.before),
            ("base-backup", &self.base_paths, &self.base_before),
        ] {
            for path in paths {
                verify_file(&tx.join(dir), path, &expected[path])?;
            }
        }
        for path in self.host_paths.union(&self.base_paths) {
            verify_file(&tx.join("after"), path, &self.after[path])?;
        }
        Ok(())
    }
}

fn ordinary_changes(before: &Index, after: &Index, paths: &BTreeSet<String>) -> bool {
    paths.iter().all(|path| {
        let (Some(a), Some(b)) = (before.get(path), after.get(path)) else {
            return false;
        };
        if a.kind != "file"
            || b.kind != "file"
            || a.link_group.is_some()
            || b.link_group.is_some()
            || a.uid != unsafe { libc::geteuid() }
            || a.flags != 0
            || !a.acl.is_empty()
            || !a.xattrs.is_empty()
            || a.mode & 0o7000 != 0
        {
            return false;
        }
        let mut metadata = a.clone();
        metadata.size = b.size;
        metadata.digest = b.digest.clone();
        &metadata == b
    })
}

fn ordinary_path(root: &Path, path: &str, device: u64) -> Result<bool> {
    validate_path(path)?;
    let file = fs::symlink_metadata(root.join(path))?;
    if !file.is_file() || file.nlink() != 1 || file.dev() != device {
        return Ok(false);
    }
    let mut directory = root.to_path_buf();
    let parent = Path::new(path).parent().expect("relative file parent");
    for component in std::iter::once(None).chain(parent.components().map(Some)) {
        if let Some(component) = component {
            directory.push(component);
        }
        let m = fs::symlink_metadata(&directory)?;
        if !m.is_dir()
            || m.dev() != device
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o700 != 0o700
            || m.st_flags() != 0
            || !read_acl(&directory)?.is_empty()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_file(root: &Path, path: &str, expected: &Entry) -> Result<()> {
    let (_, entry, _) = scan_entry(root, &root.join(path))?;
    if &entry != expected || fs::symlink_metadata(root.join(path))?.nlink() != 1 {
        return Err(Error::Backend(
            "file restore backup integrity check failed".into(),
        ));
    }
    Ok(())
}

fn verify_partial(
    root: &Path,
    before: &Index,
    after: &Index,
    touched: &BTreeSet<String>,
) -> Result<()> {
    let live = scan(root)?;
    if live.keys().ne(before.keys())
        || live.iter().any(|(path, entry)| {
            Some(entry) != before.get(path)
                && (!touched.contains(path) || Some(entry) != after.get(path))
        })
    {
        return Err(conflict(
            "live tree contains post-crash edits; current files and transaction retained",
        ));
    }
    for path in touched {
        if !ordinary_path(root, path, fs::metadata(root)?.dev())? {
            return Err(conflict(
                "post-crash file topology changed; transaction retained",
            ));
        }
    }
    Ok(())
}

fn clone_file(source: &Path, destination: &Path) -> Result<()> {
    clone_path(source, destination, &mut HashMap::new())
}

fn validate_path(path: &str) -> Result<()> {
    if path == "." {
        return Ok(());
    }
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || Path::new(path)
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == ".deltabox")
    {
        return Err(Error::Backend("unsafe file restore path".into()));
    }
    Ok(())
}

fn conflict(message: &str) -> Error {
    Error::ProjectConflict(ConflictReport::new(
        format!("history recovery conflict: {message}"),
        Vec::new(),
    ))
}

/// Follow no live symlink parents. The source is already durable on the same
/// device. Atomic rename never leaves the destination absent or partially written.
fn replace_file(source: &Path, root: &Path, path: &str) -> Result<()> {
    validate_path(path)?;
    let open_dir = |fd, name: &std::ffi::OsStr| -> Result<File> {
        let name = CString::new(name.as_bytes())
            .map_err(|_| Error::Backend("NUL in restore path".into()))?;
        let raw = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(raw) })
    };
    let mut parent = open_dir(libc::AT_FDCWD, root.as_os_str())?;
    let relative = Path::new(path);
    for component in relative.parent().expect("file parent").components() {
        parent = open_dir(parent.as_raw_fd(), component.as_os_str())?;
    }
    let name = CString::new(relative.file_name().expect("file name").as_bytes()).unwrap();
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_nlink != 1 {
        return Err(conflict(
            "restore destination is no longer an ordinary file",
        ));
    }
    let source_name = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| Error::Backend("NUL in staging path".into()))?;
    if unsafe {
        libc::renameat(
            libc::AT_FDCWD,
            source_name.as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    fault("macos_history_files_renamed")?;
    parent.sync_all()?;
    sync_dir(source.parent().expect("staging parent"))?;
    Ok(())
}

pub(crate) fn fault(point: &str) -> Result<()> {
    if cfg!(debug_assertions) {
        if std::env::var("DELTABOX_TEST_CRASH_AT").as_deref() == Ok(point) {
            std::process::abort();
        }
        if std::env::var("DELTABOX_TEST_FAIL_AT").as_deref() == Ok(point) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
        crate::test_hooks::maybe_pause(point);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_never_follows_a_live_symlink_parent() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file"), "external content").unwrap();
        fs::write(staging.path().join("ready"), "replacement").unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join("parent")).unwrap();
        assert!(replace_file(&staging.path().join("ready"), project.path(), "parent/file").is_err());
        assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"external content");
        assert!(staging.path().join("ready").exists());
    }
}

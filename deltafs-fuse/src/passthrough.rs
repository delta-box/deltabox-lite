use crate::engine::{EngineError, EngineOptions, StateEngine};
use crate::gate::WriteGate;
use anyhow::{bail, Context, Result};
use fuser::{
    AccessFlags, BackgroundSession, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};
#[cfg(target_os = "linux")]
use fuser::{InitFlags, KernelConfig};
use std::collections::{BTreeSet, HashMap};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(60);
const INTERNAL_DIR: &str = ".deltabox";

#[derive(Debug)]
struct InodeEntry {
    backing: (u64, u64),
    paths: BTreeSet<PathBuf>,
}

#[derive(Debug)]
struct InodeTable {
    next: u64,
    by_inode: HashMap<u64, InodeEntry>,
    by_backing: HashMap<(u64, u64), u64>,
    by_path: HashMap<PathBuf, u64>,
}

impl InodeTable {
    fn new(source: PathBuf) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&source)?;
        let backing = backing_identity(&metadata);
        let mut by_inode = HashMap::new();
        let mut by_backing = HashMap::new();
        let mut by_path = HashMap::new();
        by_inode.insert(
            INodeNo::ROOT.0,
            InodeEntry {
                backing,
                paths: BTreeSet::from([source.clone()]),
            },
        );
        by_backing.insert(backing, INodeNo::ROOT.0);
        by_path.insert(source, INodeNo::ROOT.0);
        Ok(Self {
            next: INodeNo::ROOT.0 + 1,
            by_inode,
            by_backing,
            by_path,
        })
    }

    fn path(&self, ino: INodeNo) -> io::Result<PathBuf> {
        self.by_inode
            .get(&ino.0)
            .and_then(|entry| entry.paths.first().cloned())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn intern(&mut self, path: PathBuf, metadata: &Metadata) -> INodeNo {
        let backing = backing_identity(metadata);
        if let Some(ino) = self.by_path.get(&path).copied() {
            if self
                .by_inode
                .get(&ino)
                .is_some_and(|entry| entry.backing == backing)
            {
                return INodeNo(ino);
            }
            self.detach_path(&path);
        }
        if let Some(ino) = self.by_backing.get(&backing).copied() {
            self.bind_path(ino, path);
            return INodeNo(ino);
        }
        let ino = self.next;
        self.next += 1;
        self.by_inode.insert(
            ino,
            InodeEntry {
                backing,
                paths: BTreeSet::new(),
            },
        );
        self.by_backing.insert(backing, ino);
        self.bind_path(ino, path);
        INodeNo(ino)
    }

    fn rename_prefix(&mut self, old: &Path, new: &Path) {
        let replacements: Vec<(u64, PathBuf, PathBuf)> = self
            .by_path
            .iter()
            .filter_map(|(path, ino)| {
                path.strip_prefix(old)
                    .ok()
                    .map(|suffix| (*ino, path.clone(), renamed_path(new, suffix)))
            })
            .collect();
        let replaced: Vec<PathBuf> = self
            .by_path
            .keys()
            .filter(|path| path.starts_with(new) && !path.starts_with(old))
            .cloned()
            .collect();
        for path in replaced {
            self.detach_path(&path);
        }
        for (ino, old_path, new_path) in replacements {
            self.detach_path(&old_path);
            self.bind_path(ino, new_path);
        }
    }

    fn exchange_prefix(&mut self, first: &Path, second: &Path) {
        let replacements: Vec<(u64, PathBuf, PathBuf)> = self
            .by_path
            .iter()
            .filter_map(|(path, ino)| {
                if let Ok(suffix) = path.strip_prefix(first) {
                    Some((*ino, path.clone(), renamed_path(second, suffix)))
                } else {
                    path.strip_prefix(second)
                        .ok()
                        .map(|suffix| (*ino, path.clone(), renamed_path(first, suffix)))
                }
            })
            .collect();
        for (_, path, _) in &replacements {
            self.detach_path(path);
        }
        for (ino, _, path) in replacements {
            self.bind_path(ino, path);
        }
    }

    fn unlink(&mut self, path: &Path) -> Option<(INodeNo, Option<PathBuf>)> {
        let ino = self.by_path.get(path).copied()?;
        self.detach_path(path);
        let alternate = self
            .by_inode
            .get(&ino)
            .and_then(|entry| entry.paths.first().cloned());
        Some((INodeNo(ino), alternate))
    }

    fn entries(&self) -> Vec<(INodeNo, PathBuf)> {
        self.by_inode
            .iter()
            .flat_map(|(inode, entry)| {
                entry
                    .paths
                    .iter()
                    .cloned()
                    .map(|path| (INodeNo(*inode), path))
            })
            .collect()
    }

    fn inode(&self, path: &Path) -> Option<INodeNo> {
        self.by_path.get(path).copied().map(INodeNo)
    }

    fn bind_path(&mut self, ino: u64, path: PathBuf) {
        let entry = self.by_inode.get_mut(&ino).expect("inode entry missing");
        self.by_backing.insert(entry.backing, ino);
        entry.paths.insert(path.clone());
        self.by_path.insert(path, ino);
    }

    fn detach_path(&mut self, path: &Path) {
        let Some(ino) = self.by_path.remove(path) else {
            return;
        };
        let entry = self.by_inode.get_mut(&ino).expect("inode entry missing");
        entry.paths.remove(path);
        if entry.paths.is_empty() {
            self.by_backing.remove(&entry.backing);
        }
    }

    fn reset_non_root(&mut self) {
        self.by_inode.retain(|inode, _| *inode == INodeNo::ROOT.0);
        self.by_backing.retain(|_, inode| *inode == INodeNo::ROOT.0);
        self.by_path.retain(|_, inode| *inode == INodeNo::ROOT.0);
    }
}

#[derive(Debug)]
struct OpenFile {
    file: File,
    ino: INodeNo,
    mutation_path: Option<PathBuf>,
}

#[derive(Debug)]
struct DirectoryEntry {
    attr: FileAttr,
    kind: FileType,
    name: OsString,
}

#[derive(Debug)]
struct OpenDirectory {
    file: File,
    ino: INodeNo,
    entries: Vec<DirectoryEntry>,
}

#[derive(Debug)]
struct HandleTable {
    next: u64,
    files: HashMap<u64, OpenFile>,
    directories: HashMap<u64, OpenDirectory>,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            next: 1,
            files: HashMap::new(),
            directories: HashMap::new(),
        }
    }

    fn insert(&mut self, file: File, ino: INodeNo, mutation_path: PathBuf) -> FileHandle {
        let handle = self.next;
        self.next += 1;
        self.files.insert(
            handle,
            OpenFile {
                file,
                ino,
                mutation_path: Some(mutation_path),
            },
        );
        FileHandle(handle)
    }

    fn rename_prefix(&mut self, old: &Path, new: &Path) {
        for open in self.files.values_mut() {
            if let Some(path) = &open.mutation_path {
                if let Ok(suffix) = path.strip_prefix(old) {
                    open.mutation_path = Some(renamed_path(new, suffix));
                }
            }
        }
    }

    fn exchange_prefix(&mut self, first: &Path, second: &Path) {
        for open in self.files.values_mut() {
            let Some(path) = &open.mutation_path else {
                continue;
            };
            if let Ok(suffix) = path.strip_prefix(first) {
                open.mutation_path = Some(renamed_path(second, suffix));
            } else if let Ok(suffix) = path.strip_prefix(second) {
                open.mutation_path = Some(renamed_path(first, suffix));
            }
        }
    }

    fn unlink(&mut self, path: &Path, alternate: Option<&Path>) {
        for open in self.files.values_mut() {
            if open.mutation_path.as_deref() == Some(path) {
                open.mutation_path = alternate.map(Path::to_path_buf);
            }
        }
    }

    fn insert_directory(
        &mut self,
        file: File,
        ino: INodeNo,
        entries: Vec<DirectoryEntry>,
    ) -> FileHandle {
        let handle = self.next;
        self.next += 1;
        self.directories
            .insert(handle, OpenDirectory { file, ino, entries });
        FileHandle(handle)
    }

    fn metadata_by_inode(&self, ino: INodeNo) -> io::Result<Metadata> {
        self.files
            .values()
            .find(|open| open.ino == ino && open.mutation_path.is_none())
            .map(|open| &open.file)
            .or_else(|| {
                self.directories
                    .values()
                    .find(|open| open.ino == ino)
                    .map(|open| &open.file)
            })
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?
            .metadata()
    }
}

#[derive(Debug)]
pub struct DeltaFs {
    source: PathBuf,
    engine: Arc<Mutex<StateEngine>>,
    gate: Arc<WriteGate>,
    inodes: Arc<Mutex<InodeTable>>,
    handles: Mutex<HandleTable>,
}

impl DeltaFs {
    fn new(source: PathBuf, engine: Arc<Mutex<StateEngine>>) -> io::Result<Self> {
        Ok(Self {
            inodes: Arc::new(Mutex::new(InodeTable::new(source.clone())?)),
            source,
            engine,
            gate: Arc::new(WriteGate::default()),
            handles: Mutex::new(HandleTable::new()),
        })
    }

    fn path(&self, ino: INodeNo) -> io::Result<PathBuf> {
        self.inodes.lock().expect("inode table poisoned").path(ino)
    }

    fn child_path(&self, parent: INodeNo, name: &OsStr) -> io::Result<PathBuf> {
        if name.as_bytes().contains(&b'/') || name == OsStr::new(".") || name == OsStr::new("..") {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        Ok(self.path(parent)?.join(name))
    }

    fn intern_attr(&self, path: PathBuf) -> io::Result<FileAttr> {
        let metadata = fs::symlink_metadata(&path)?;
        let ino = self
            .inodes
            .lock()
            .expect("inode table poisoned")
            .intern(path, &metadata);
        file_attr(ino, &metadata)
    }

    fn relative(&self, path: &Path) -> io::Result<PathBuf> {
        path.strip_prefix(&self.source)
            .map(Path::to_path_buf)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn mutate<T>(
        &self,
        operation: impl FnOnce(&mut StateEngine) -> crate::engine::Result<T>,
    ) -> io::Result<T> {
        let _entry = self.gate.enter();
        self.mutate_entered(operation)
    }

    /// `mutate` for callers that entered the write gate themselves, before
    /// taking the handle table: waiting at the gate while holding that table
    /// would also stall reads through open handles. The gate is not
    /// reentrant, so these callers must not use `mutate`.
    fn mutate_entered<T>(
        &self,
        operation: impl FnOnce(&mut StateEngine) -> crate::engine::Result<T>,
    ) -> io::Result<T> {
        let (result, barrier) = {
            let mut engine = self.engine.lock().expect("engine lock");
            engine.begin_grouped_return();
            let result = operation(&mut engine);
            (result, engine.finish_grouped_return())
        };
        if let Some(barrier) = barrier {
            barrier.wait().map_err(engine_io)?;
        }
        result.map_err(engine_io)
    }

    fn open_handle(&self, ino: INodeNo, flags: OpenFlags) -> io::Result<FileHandle> {
        let path = self.path(ino)?;
        let file = if flags.0 & libc::O_TRUNC != 0 {
            let relative = self.relative(&path)?;
            self.mutate(|engine| engine.truncate(&relative, 0))?;
            open_file(&path, flags.0 & !libc::O_TRUNC, false, 0)?
        } else {
            open_file(&path, flags.0, false, 0)?
        };
        Ok(self
            .handles
            .lock()
            .expect("handle table poisoned")
            .insert(file, ino, path))
    }

    fn directory_entries(&self, ino: INodeNo) -> io::Result<Vec<DirectoryEntry>> {
        let path = self.path(ino)?;
        let parent = if ino == INodeNo::ROOT {
            path.clone()
        } else {
            path.parent().unwrap_or(&path).to_path_buf()
        };
        let mut entries = vec![
            DirectoryEntry {
                attr: self.intern_attr(path.clone())?,
                kind: FileType::Directory,
                name: ".".into(),
            },
            DirectoryEntry {
                attr: self.intern_attr(parent)?,
                kind: FileType::Directory,
                name: "..".into(),
            },
        ];
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if path == self.source && entry.file_name() == OsStr::new(INTERNAL_DIR) {
                continue;
            }
            let kind = FileType::from_std(entry.file_type()?)
                .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
            entries.push(DirectoryEntry {
                attr: self.intern_attr(entry.path())?,
                kind,
                name: entry.file_name(),
            });
        }
        Ok(entries)
    }
}

#[derive(Debug)]
pub struct MountedEngine {
    session: BackgroundSession,
    inodes: Arc<Mutex<InodeTable>>,
    gate: Arc<WriteGate>,
}

impl MountedEngine {
    pub fn gate(&self) -> Arc<WriteGate> {
        self.gate.clone()
    }

    pub fn invalidate(&self) -> io::Result<()> {
        let notifier = self.session.notifier();
        let mut inodes = self.inodes.lock().expect("inode table poisoned");
        let entries = inodes.entries();
        for (inode, path) in entries {
            notifier.inval_inode(inode, 0, -1)?;
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                if let Some(parent_inode) = inodes.inode(parent) {
                    notifier.inval_entry(parent_inode, name)?;
                }
            }
        }
        // Rollback mutates the backing tree without passing through FUSE
        // callbacks, so path and backing aliases can refer to removed inodes.
        // Keep the root and force subsequent lookups to allocate fresh inodes.
        inodes.reset_non_root();
        Ok(())
    }

    pub fn shutdown(self) -> io::Result<()> {
        self.session.umount_and_join()
    }

    pub fn wait(self) -> io::Result<()> {
        self.session.join()
    }
}

impl Filesystem for DeltaFs {
    #[cfg(target_os = "linux")]
    fn init(&mut self, _request: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let readdirplus = InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO;
        let _ = config.add_capabilities(readdirplus);
        Ok(())
    }

    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent == INodeNo::ROOT && name == OsStr::new(INTERNAL_DIR) {
            reply.error(Errno::ENOENT);
            return;
        }
        let result = self
            .child_path(parent, name)
            .and_then(|path| self.intern_attr(path));
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error.into()),
        }
    }

    fn getattr(&self, _request: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let result = if let Some(fh) = fh {
            let handles = self.handles.lock().expect("handle table poisoned");
            handles
                .files
                .get(&fh.0)
                .map(|open| open.file.metadata())
                .or_else(|| {
                    handles
                        .directories
                        .get(&fh.0)
                        .map(|open| open.file.metadata())
                })
                .unwrap_or_else(|| Err(io::Error::from(io::ErrorKind::NotFound)))
                .and_then(|metadata| file_attr(ino, &metadata))
        } else if let Ok(path) = self.path(ino) {
            fs::symlink_metadata(path).and_then(|metadata| file_attr(ino, &metadata))
        } else {
            self.handles
                .lock()
                .expect("handle table poisoned")
                .metadata_by_inode(ino)
                .and_then(|metadata| file_attr(ino, &metadata))
        };
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error.into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _request: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
            {
                let handles = self.handles.lock().expect("handle table poisoned");
                let open = fh
                    .and_then(|fh| handles.files.get(&fh.0))
                    .filter(|open| open.mutation_path.is_none())
                    .or_else(|| {
                        handles
                            .files
                            .values()
                            .find(|open| open.ino == ino && open.mutation_path.is_none())
                    });
                if let Some(open) = open {
                    return setattr_detached(ino, &open.file, mode, uid, gid, size, atime, mtime);
                }
            }
            let path = self.path(ino)?;
            let relative = self.relative(&path)?;
            let metadata = fs::symlink_metadata(&path)?;
            if let Some(mode) = mode {
                self.mutate(|engine| engine.set_mode(&relative, mode))?;
            }
            if uid.is_some() || gid.is_some() {
                self.mutate(|engine| {
                    engine.set_owner(
                        &relative,
                        uid.unwrap_or(metadata.uid()),
                        gid.unwrap_or(metadata.gid()),
                    )
                })?;
            }
            if let Some(size) = size {
                self.mutate(|engine| engine.truncate(&relative, size))?;
            }
            if atime.is_some() || mtime.is_some() {
                let now = SystemTime::now();
                let old_atime = system_time(metadata.atime(), metadata.atime_nsec());
                let old_mtime = system_time(metadata.mtime(), metadata.mtime_nsec());
                self.mutate(|engine| {
                    engine.set_times(
                        &relative,
                        atime.map_or(old_atime, |value| resolve_time(value, now)),
                        mtime.map_or(old_mtime, |value| resolve_time(value, now)),
                    )
                })?;
            }
            let metadata = fs::symlink_metadata(&path)?;
            file_attr(ino, &metadata)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error.into()),
        }
    }

    fn readlink(&self, _request: &Request, ino: INodeNo, reply: ReplyData) {
        let result = self.path(ino).and_then(fs::read_link);
        match result {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(error) => reply.error(error.into()),
        }
    }

    fn mkdir(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            let path = self.child_path(parent, name)?;
            let relative = self.relative(&path)?;
            self.mutate(|engine| engine.create_dir(&relative, mode & !umask))?;
            self.intern_attr(path)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error.into()),
        }
    }

    fn unlink(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            let relative = self.relative(&path)?;
            self.mutate(|engine| engine.unlink(&relative))?;
            let alternate = self
                .inodes
                .lock()
                .expect("inode table poisoned")
                .unlink(&path)
                .and_then(|(_, alternate)| alternate);
            self.handles
                .lock()
                .expect("handle table poisoned")
                .unlink(&path, alternate.as_deref());
            Ok(())
        });
        finish_empty(result, reply);
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            let relative = self.relative(&path)?;
            self.mutate(|engine| engine.unlink(&relative))?;
            self.inodes
                .lock()
                .expect("inode table poisoned")
                .unlink(&path);
            Ok(())
        });
        finish_empty(result, reply);
    }

    fn symlink(
        &self,
        _request: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result: io::Result<FileAttr> = (|| {
            let path = self.child_path(parent, link_name)?;
            let relative = self.relative(&path)?;
            self.mutate(|engine| engine.symlink(target, &relative))?;
            self.intern_attr(path)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error.into()),
        }
    }

    fn rename(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let supported = flags.is_empty() || rename_noreplace(flags) || rename_exchange(flags);
        if !supported {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let result: io::Result<()> = (|| {
            let old = self.child_path(parent, name)?;
            let new = self.child_path(newparent, newname)?;
            if same_backing(&old, &new)? {
                return Ok(());
            }
            let old_relative = self.relative(&old)?;
            let new_relative = self.relative(&new)?;
            self.mutate(|engine| {
                if rename_noreplace(flags) {
                    engine.rename_noreplace(&old_relative, &new_relative)
                } else if rename_exchange(flags) {
                    engine.rename_exchange(&old_relative, &new_relative)
                } else {
                    engine.rename(&old_relative, &new_relative)
                }
            })?;
            if rename_exchange(flags) {
                self.inodes
                    .lock()
                    .expect("inode table poisoned")
                    .exchange_prefix(&old, &new);
                self.handles
                    .lock()
                    .expect("handle table poisoned")
                    .exchange_prefix(&old, &new);
                return Ok(());
            }
            let mut inodes = self.inodes.lock().expect("inode table poisoned");
            let target_alternate = inodes.unlink(&new).and_then(|(_, alternate)| alternate);
            inodes.rename_prefix(&old, &new);
            drop(inodes);
            let mut handles = self.handles.lock().expect("handle table poisoned");
            handles.unlink(&new, target_alternate.as_deref());
            handles.rename_prefix(&old, &new);
            Ok(())
        })();
        finish_empty(result, reply);
    }

    fn link(
        &self,
        _request: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let result: io::Result<FileAttr> = (|| {
            let source = self.path(ino)?;
            let target = self.child_path(newparent, newname)?;
            let source_relative = self.relative(&source)?;
            let target_relative = self.relative(&target)?;
            self.mutate(|engine| engine.hard_link(&source_relative, &target_relative))?;
            self.intern_attr(target)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error.into()),
        }
    }

    fn open(&self, _request: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = self.open_handle(ino, flags);
        match result {
            Ok(handle) => reply.opened(handle, FopenFlags::FOPEN_KEEP_CACHE),
            Err(error) => reply.error(error.into()),
        }
    }

    fn read(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result: io::Result<Vec<u8>> = (|| {
            let handles = self.handles.lock().expect("handle table poisoned");
            let open = handles
                .files
                .get(&fh.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let mut buffer = vec![0; size as usize];
            let read = open.file.read_at(&mut buffer, offset)?;
            buffer.truncate(read);
            Ok(buffer)
        })();
        match result {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(error.into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result: io::Result<usize> = (|| {
            let _entry = self.gate.enter();
            let handles = self.handles.lock().expect("handle table poisoned");
            let open = handles
                .files
                .get(&fh.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if let Some(path) = &open.mutation_path {
                let relative = self.relative(path)?;
                self.mutate_entered(|engine| {
                    engine.write_file(&relative, &open.file, offset, data)
                })?;
            } else {
                open.file.write_at(data, offset)?;
            }
            Ok(data.len())
        })();
        match result {
            Ok(size) => reply.written(size as u32),
            Err(error) => reply.error(error.into()),
        }
    }

    fn flush(
        &self,
        _request: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles
            .lock()
            .expect("handle table poisoned")
            .files
            .remove(&fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = self
            .handles
            .lock()
            .expect("handle table poisoned")
            .files
            .get(&fh.0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|open| {
                if datasync {
                    open.file.sync_data()
                } else {
                    open.file.sync_all()
                }
            });
        finish_empty(result, reply);
    }

    #[cfg(target_os = "linux")]
    fn fallocate(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        if !supported_fallocate(mode) {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let result = (|| {
            let _entry = self.gate.enter();
            let handles = self.handles.lock().expect("handle table poisoned");
            let open = handles
                .files
                .get(&fh.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if let Some(path) = &open.mutation_path {
                let relative = self.relative(path)?;
                self.mutate_entered(|engine| {
                    engine.fallocate_file(&relative, &open.file, offset, length, mode)
                })
            } else {
                let result = unsafe {
                    libc::fallocate(
                        open.file.as_raw_fd(),
                        mode,
                        offset as libc::off_t,
                        length as libc::off_t,
                    )
                };
                if result == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
        })();
        finish_empty(result, reply);
    }

    fn lseek(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let result = self
            .handles
            .lock()
            .expect("handle table poisoned")
            .files
            .get(&fh.0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|open| {
                let offset = unsafe { libc::lseek(open.file.as_raw_fd(), offset, whence) };
                if offset >= 0 {
                    Ok(offset)
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        match result {
            Ok(offset) => reply.offset(offset),
            Err(error) => reply.error(error.into()),
        }
    }

    fn copy_file_range(
        &self,
        _request: &Request,
        _ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        _ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        let result = (|| {
            let _entry = self.gate.enter();
            let handles = self.handles.lock().expect("handle table poisoned");
            let input = handles
                .files
                .get(&fh_in.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let output = handles
                .files
                .get(&fh_out.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if same_file(&input.file, &output.file)? && ranges_overlap(offset_in, offset_out, len) {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            if let Some(path) = &output.mutation_path {
                let relative = self.relative(path)?;
                self.mutate_entered(|engine| {
                    engine.copy_file_range_file(
                        &relative,
                        &output.file,
                        offset_out,
                        &input.file,
                        offset_in,
                        len,
                    )
                })
            } else {
                copy_file_bytes(&input.file, offset_in, &output.file, offset_out, len)
            }
        })();
        match result {
            Ok(written) => reply.written(written as u32),
            Err(error) => reply.error(error.into()),
        }
    }

    fn opendir(&self, _request: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let result = self.path(ino).and_then(|path| {
            let directory = File::open(path)?;
            let entries = self.directory_entries(ino)?;
            Ok(self
                .handles
                .lock()
                .expect("handle table poisoned")
                .insert_directory(directory, ino, entries))
        });
        match result {
            Ok(handle) => reply.opened(handle, FopenFlags::FOPEN_CACHE_DIR),
            Err(error) => reply.error(error.into()),
        }
    }

    fn readdir(
        &self,
        _request: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let handles = self.handles.lock().expect("handle table poisoned");
        let Some(directory) = handles.directories.get(&_fh.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        for (index, entry) in directory.entries.iter().enumerate().skip(offset as usize) {
            if reply.add(entry.attr.ino, (index + 1) as u64, entry.kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let handles = self.handles.lock().expect("handle table poisoned");
        let Some(directory) = handles.directories.get(&fh.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        for (index, entry) in directory.entries.iter().enumerate().skip(offset as usize) {
            if reply.add(
                entry.attr.ino,
                (index + 1) as u64,
                &entry.name,
                &TTL,
                &entry.attr,
                Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.handles
            .lock()
            .expect("handle table poisoned")
            .directories
            .remove(&fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _request: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = self
            .handles
            .lock()
            .expect("handle table poisoned")
            .directories
            .get(&fh.0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|directory| directory.file.sync_all());
        finish_empty(result, reply);
    }

    fn statfs(&self, _request: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let result = (|| {
            let path = c_path(&self.source)?;
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(stat)
        })();
        match result {
            Ok(stat) => {
                #[cfg(target_os = "macos")]
                let (blocks, bfree, bavail, files, ffree) = (
                    u64::from(stat.f_blocks),
                    u64::from(stat.f_bfree),
                    u64::from(stat.f_bavail),
                    u64::from(stat.f_files),
                    u64::from(stat.f_ffree),
                );
                #[cfg(not(target_os = "macos"))]
                let (blocks, bfree, bavail, files, ffree) = (
                    stat.f_blocks,
                    stat.f_bfree,
                    stat.f_bavail,
                    stat.f_files,
                    stat.f_ffree,
                );
                reply.statfs(
                    blocks,
                    bfree,
                    bavail,
                    files,
                    ffree,
                    stat.f_bsize as u32,
                    255,
                    stat.f_frsize as u32,
                )
            }
            Err(error) => reply.error(error.into()),
        }
    }

    fn setxattr(
        &self,
        _request: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        if position != 0 {
            reply.error(Errno::ENOSYS);
            return;
        }
        let result = (|| {
            let path = self.path(ino)?;
            let existing = xattr::get(&path, name)?;
            if flags & libc::XATTR_CREATE != 0 && existing.is_some() {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            if flags & libc::XATTR_REPLACE != 0 && existing.is_none() {
                return Err(io::Error::from_raw_os_error(no_xattr_errno()));
            }
            let relative = self.relative(&path)?;
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            self.mutate(|engine| engine.set_xattr(&relative, name, value))
        })();
        finish_empty(result, reply);
    }

    fn getxattr(
        &self,
        _request: &Request,
        ino: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let result = self.path(ino).and_then(|path| xattr::get(path, name));
        match result {
            Ok(Some(value)) if size == 0 => reply.size(value.len() as u32),
            Ok(Some(value)) if value.len() <= size as usize => reply.data(&value),
            Ok(Some(_)) => reply.error(Errno::ERANGE),
            Ok(None) => reply.error(Errno::from_i32(no_xattr_errno())),
            Err(error) => reply.error(error.into()),
        }
    }

    fn listxattr(&self, _request: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let result = self.path(ino).and_then(|path| {
            let mut data = Vec::new();
            for name in xattr::list(path)? {
                data.extend_from_slice(name.as_bytes());
                data.push(0);
            }
            Ok(data)
        });
        match result {
            Ok(data) if size == 0 => reply.size(data.len() as u32),
            Ok(data) if data.len() <= size as usize => reply.data(&data),
            Ok(_) => reply.error(Errno::ERANGE),
            Err(error) => reply.error(error.into()),
        }
    }

    fn removexattr(&self, _request: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| {
            let path = self.path(ino)?;
            let relative = self.relative(&path)?;
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            self.mutate(|engine| engine.remove_xattr(&relative, name))
        })();
        finish_empty(result, reply);
    }

    fn access(&self, _request: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let result = self.path(ino).and_then(|path| {
            let path = c_path(&path)?;
            if unsafe { libc::access(path.as_ptr(), mask.bits()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
        finish_empty(result, reply);
    }

    fn create(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result: io::Result<(FileAttr, FileHandle)> = (|| {
            let path = self.child_path(parent, name)?;
            let relative = self.relative(&path)?;
            let file =
                self.mutate(|engine| engine.create_file_open(&relative, mode & !umask, flags))?;
            let attr = self.intern_attr(path.clone())?;
            let handle = self
                .handles
                .lock()
                .expect("handle table poisoned")
                .insert(file, attr.ino, path);
            Ok((attr, handle))
        })();
        match result {
            Ok((attr, handle)) => {
                reply.created(&TTL, &attr, Generation(0), handle, FopenFlags::empty())
            }
            Err(error) => reply.error(error.into()),
        }
    }
}

pub fn mount(
    source: &Path,
    mountpoint: &Path,
    state_dir: &Path,
    options: EngineOptions,
    allow_other: bool,
) -> Result<()> {
    let source = source.canonicalize()?;
    let engine = if state_dir.join("session.json").exists() {
        StateEngine::open_with_options(&source, state_dir, options)?
    } else {
        StateEngine::create(&source, state_dir, options)?
    };
    spawn_engine(
        &source,
        mountpoint,
        Arc::new(Mutex::new(engine)),
        allow_other,
    )?
    .wait()?;
    Ok(())
}

pub fn spawn_engine(
    source: &Path,
    mountpoint: &Path,
    engine: Arc<Mutex<StateEngine>>,
    allow_other: bool,
) -> Result<MountedEngine> {
    let source = source
        .canonicalize()
        .with_context(|| format!("source does not exist: {}", source.display()))?;
    let mountpoint = mountpoint
        .canonicalize()
        .with_context(|| format!("mountpoint does not exist: {}", mountpoint.display()))?;
    if !source.is_dir() || !mountpoint.is_dir() {
        bail!("source and mountpoint must be directories");
    }
    if source == mountpoint || mountpoint.starts_with(&source) {
        bail!("mountpoint must be outside the source tree");
    }
    if fs::read_dir(&mountpoint)?.next().is_some() {
        bail!("mountpoint must be empty");
    }
    let filesystem = DeltaFs::new(source, engine)?;
    let inodes = filesystem.inodes.clone();
    let gate = filesystem.gate.clone();
    let mut config = fuser::Config::default();
    config.mount_options.extend([
        MountOption::FSName("deltafs-fuse".into()),
        MountOption::Subtype("deltafs".into()),
        MountOption::RW,
        MountOption::Exec,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::DefaultPermissions,
    ]);
    if allow_other {
        config.acl = fuser::SessionACL::All;
    }
    config.n_threads = Some(4);
    #[cfg(target_os = "linux")]
    {
        config.clone_fd = true;
    }
    let session = fuser::spawn_mount(filesystem, mountpoint, &config)?;
    Ok(MountedEngine {
        session,
        inodes,
        gate,
    })
}

fn renamed_path(new: &Path, suffix: &Path) -> PathBuf {
    if suffix.as_os_str().is_empty() {
        new.to_path_buf()
    } else {
        new.join(suffix)
    }
}

fn backing_identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn same_backing(first: &Path, second: &Path) -> io::Result<bool> {
    let first = fs::symlink_metadata(first)?;
    match fs::symlink_metadata(second) {
        Ok(second) => Ok(backing_identity(&first) == backing_identity(&second)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(flags: RenameFlags) -> bool {
    flags == RenameFlags::RENAME_NOREPLACE
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_flags: RenameFlags) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn rename_exchange(flags: RenameFlags) -> bool {
    flags == RenameFlags::RENAME_EXCHANGE
}

#[cfg(target_os = "linux")]
fn supported_fallocate(mode: i32) -> bool {
    mode == 0
        || mode == libc::FALLOC_FL_KEEP_SIZE
        || mode == (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE)
        || mode == libc::FALLOC_FL_ZERO_RANGE
        || mode == (libc::FALLOC_FL_ZERO_RANGE | libc::FALLOC_FL_KEEP_SIZE)
}

fn same_file(first: &File, second: &File) -> io::Result<bool> {
    let first = first.metadata()?;
    let second = second.metadata()?;
    Ok(first.dev() == second.dev() && first.ino() == second.ino())
}

fn ranges_overlap(first: u64, second: u64, length: u64) -> bool {
    length != 0 && first < second.saturating_add(length) && second < first.saturating_add(length)
}

fn copy_file_bytes(
    input: &File,
    mut input_offset: u64,
    output: &File,
    mut output_offset: u64,
    mut length: u64,
) -> io::Result<u64> {
    let total = length;
    let mut buffer = [0_u8; 64 * 1024];
    while length != 0 {
        let chunk = usize::try_from(length.min(buffer.len() as u64)).expect("buffer-sized chunk");
        let read = input.read_at(&mut buffer[..chunk], input_offset)?;
        if read == 0 {
            break;
        }
        output.write_all_at(&buffer[..read], output_offset)?;
        input_offset += read as u64;
        output_offset += read as u64;
        length -= read as u64;
    }
    Ok(total - length)
}

#[cfg(not(target_os = "linux"))]
fn rename_exchange(_flags: RenameFlags) -> bool {
    false
}

fn engine_io(error: EngineError) -> io::Error {
    match error {
        EngineError::Io(error) => error,
        EngineError::QuotaExceeded { .. } => io::Error::from_raw_os_error(libc::ENOSPC),
        other => io::Error::other(other),
    }
}

fn finish_empty(result: io::Result<()>, reply: ReplyEmpty) {
    match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(error.into()),
    }
}

fn open_file(path: &Path, flags: i32, create: bool, mode: u32) -> io::Result<File> {
    let access = OpenFlags(flags).acc_mode();
    let mut options = OpenOptions::new();
    options.read(matches!(
        access,
        OpenAccMode::O_RDONLY | OpenAccMode::O_RDWR
    ));
    options.write(matches!(
        access,
        OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR
    ));
    options.create(create || flags & libc::O_CREAT != 0);
    options.create_new(flags & libc::O_EXCL != 0);
    options.truncate(flags & libc::O_TRUNC != 0);
    options.mode(mode);
    options.custom_flags(flags & !(libc::O_ACCMODE | libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC));
    options.open(path)
}

fn file_attr(ino: INodeNo, metadata: &Metadata) -> io::Result<FileAttr> {
    let kind = FileType::from_std(metadata.file_type())
        .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
    Ok(FileAttr {
        ino,
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime: system_time(metadata.atime(), metadata.atime_nsec()),
        mtime: system_time(metadata.mtime(), metadata.mtime_nsec()),
        ctime: system_time(metadata.ctime(), metadata.ctime_nsec()),
        crtime: metadata.created().unwrap_or(UNIX_EPOCH),
        kind,
        perm: (metadata.mode() & 0o7777) as u16,
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: 0,
    })
}

fn system_time(seconds: i64, nanos: i64) -> SystemTime {
    if seconds >= 0 {
        UNIX_EPOCH + Duration::new(seconds as u64, nanos as u32)
    } else {
        UNIX_EPOCH - Duration::new((-seconds) as u64, nanos as u32)
    }
}

fn resolve_time(value: TimeOrNow, now: SystemTime) -> SystemTime {
    match value {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => now,
    }
}

#[allow(clippy::too_many_arguments)]
fn setattr_detached(
    ino: INodeNo,
    file: &File,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
) -> io::Result<FileAttr> {
    let metadata = file.metadata()?;
    if let Some(mode) = mode {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    if uid.is_some() || gid.is_some() {
        let result = unsafe {
            libc::fchown(
                file.as_raw_fd(),
                uid.unwrap_or(metadata.uid()),
                gid.unwrap_or(metadata.gid()),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if let Some(size) = size {
        file.set_len(size)?;
    }
    if atime.is_some() || mtime.is_some() {
        let now = SystemTime::now();
        file.set_times(
            FileTimes::new()
                .set_accessed(atime.map_or(
                    system_time(metadata.atime(), metadata.atime_nsec()),
                    |value| resolve_time(value, now),
                ))
                .set_modified(mtime.map_or(
                    system_time(metadata.mtime(), metadata.mtime_nsec()),
                    |value| resolve_time(value, now),
                )),
        )?;
    }
    file_attr(ino, &file.metadata()?)
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(target_os = "linux")]
fn no_xattr_errno() -> i32 {
    libc::ENODATA
}

#[cfg(target_os = "macos")]
fn no_xattr_errno() -> i32 {
    libc::ENOATTR
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_truncate_captures_preimage_before_mutation() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let state = parent.path().join("state");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let target = source.join("file.txt");
        fs::write(&target, b"before truncate").unwrap();

        let engine = StateEngine::create(&source, &state, EngineOptions::default()).unwrap();
        let filesystem = DeltaFs::new(source.clone(), Arc::new(Mutex::new(engine))).unwrap();
        let ino = filesystem.intern_attr(target.clone()).unwrap().ino;
        filesystem
            .open_handle(ino, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
            .unwrap();

        assert_eq!(fs::metadata(&target).unwrap().len(), 0);
        assert_eq!(
            filesystem.engine.lock().unwrap().diff(None).unwrap().len(),
            1
        );
        filesystem.engine.lock().unwrap().rollback(None).unwrap();
        assert_eq!(fs::read(target).unwrap(), b"before truncate");
    }

    #[test]
    fn rename_prefix_preserves_exact_and_descendant_paths() {
        let new = Path::new("root/new");
        assert_eq!(renamed_path(new, Path::new("")), new);
        assert_eq!(renamed_path(new, Path::new("child")), new.join("child"));
    }

    #[test]
    fn overwritten_open_directory_remains_addressable_by_inode() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        let mut handles = HandleTable::new();
        let ino = INodeNo::ROOT;
        handles.insert_directory(File::open(&target).unwrap(), ino, Vec::new());

        fs::rename(&source, &target).unwrap();

        assert!(handles.metadata_by_inode(ino).unwrap().file_type().is_dir());
    }
}

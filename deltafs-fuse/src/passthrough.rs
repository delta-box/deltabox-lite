use crate::engine::{EngineError, EngineOptions, StateEngine};
use anyhow::{bail, Context, Result};
use fuser::{
    AccessFlags, BackgroundSession, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::ZERO;
const INTERNAL_DIR: &str = ".deltabox";

#[derive(Debug)]
struct InodeTable {
    next: u64,
    by_inode: HashMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
}

impl InodeTable {
    fn new(source: PathBuf) -> Self {
        let mut by_inode = HashMap::new();
        let mut by_path = HashMap::new();
        by_inode.insert(INodeNo::ROOT.0, source.clone());
        by_path.insert(source, INodeNo::ROOT.0);
        Self {
            next: INodeNo::ROOT.0 + 1,
            by_inode,
            by_path,
        }
    }

    fn path(&self, ino: INodeNo) -> io::Result<PathBuf> {
        self.by_inode
            .get(&ino.0)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn intern(&mut self, path: PathBuf) -> INodeNo {
        if let Some(ino) = self.by_path.get(&path) {
            return INodeNo(*ino);
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.clone(), ino);
        self.by_inode.insert(ino, path);
        INodeNo(ino)
    }

    fn rename_prefix(&mut self, old: &Path, new: &Path) {
        let replacements: Vec<(u64, PathBuf, PathBuf)> = self
            .by_inode
            .iter()
            .filter_map(|(ino, path)| {
                path.strip_prefix(old)
                    .ok()
                    .map(|suffix| (*ino, path.clone(), renamed_path(new, suffix)))
            })
            .collect();
        for (ino, old_path, new_path) in replacements {
            self.by_path.remove(&old_path);
            self.by_path.insert(new_path.clone(), ino);
            self.by_inode.insert(ino, new_path);
        }
    }

    fn entries(&self) -> Vec<(INodeNo, PathBuf)> {
        self.by_inode
            .iter()
            .map(|(inode, path)| (INodeNo(*inode), path.clone()))
            .collect()
    }

    fn inode(&self, path: &Path) -> Option<INodeNo> {
        self.by_path.get(path).copied().map(INodeNo)
    }
}

#[derive(Debug)]
struct OpenFile {
    file: File,
    mutation_path: PathBuf,
}

#[derive(Debug)]
struct HandleTable {
    next: u64,
    files: HashMap<u64, OpenFile>,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            next: 1,
            files: HashMap::new(),
        }
    }

    fn insert(&mut self, file: File, mutation_path: PathBuf) -> FileHandle {
        let handle = self.next;
        self.next += 1;
        self.files.insert(
            handle,
            OpenFile {
                file,
                mutation_path,
            },
        );
        FileHandle(handle)
    }

    fn rename_prefix(&mut self, old: &Path, new: &Path) {
        for open in self.files.values_mut() {
            if let Ok(suffix) = open.mutation_path.strip_prefix(old) {
                open.mutation_path = renamed_path(new, suffix);
            }
        }
    }
}

#[derive(Debug)]
pub struct DeltaFs {
    source: PathBuf,
    engine: Arc<Mutex<StateEngine>>,
    inodes: Arc<Mutex<InodeTable>>,
    handles: Mutex<HandleTable>,
}

impl DeltaFs {
    fn new(source: PathBuf, engine: Arc<Mutex<StateEngine>>) -> Self {
        Self {
            inodes: Arc::new(Mutex::new(InodeTable::new(source.clone()))),
            source,
            engine,
            handles: Mutex::new(HandleTable::new()),
        }
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
            .intern(path);
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
        operation(&mut self.engine.lock().expect("engine lock")).map_err(engine_io)
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
            .insert(file, path))
    }
}

#[derive(Debug)]
pub struct MountedEngine {
    session: BackgroundSession,
    inodes: Arc<Mutex<InodeTable>>,
}

impl MountedEngine {
    pub fn invalidate(&self) -> io::Result<()> {
        let notifier = self.session.notifier();
        let inodes = self.inodes.lock().expect("inode table poisoned");
        for (inode, path) in inodes.entries() {
            notifier.inval_inode(inode, 0, -1)?;
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                if let Some(parent_inode) = inodes.inode(parent) {
                    notifier.inval_entry(parent_inode, name)?;
                }
            }
        }
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

    fn getattr(&self, _request: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let result = self
            .path(ino)
            .and_then(fs::symlink_metadata)
            .and_then(|metadata| file_attr(ino, &metadata));
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
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
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
            self.mutate(|engine| engine.unlink(&relative))
        });
        finish_empty(result, reply);
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            let relative = self.relative(&path)?;
            self.mutate(|engine| engine.unlink(&relative))
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
        if !flags.is_empty() {
            reply.error(Errno::ENOSYS);
            return;
        }
        let result: io::Result<()> = (|| {
            let old = self.child_path(parent, name)?;
            let new = self.child_path(newparent, newname)?;
            let old_relative = self.relative(&old)?;
            let new_relative = self.relative(&new)?;
            self.mutate(|engine| engine.rename(&old_relative, &new_relative))?;
            self.inodes
                .lock()
                .expect("inode table poisoned")
                .rename_prefix(&old, &new);
            self.handles
                .lock()
                .expect("handle table poisoned")
                .rename_prefix(&old, &new);
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
            Ok(handle) => reply.opened(handle, FopenFlags::empty()),
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
            let handles = self.handles.lock().expect("handle table poisoned");
            let open = handles
                .files
                .get(&fh.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let relative = self.relative(&open.mutation_path)?;
            self.mutate(|engine| engine.write_file(&relative, &open.file, offset, data))?;
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

    fn readdir(
        &self,
        _request: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result: io::Result<Vec<(PathBuf, FileType, OsString)>> = (|| {
            let path = self.path(ino)?;
            let parent = path.parent().unwrap_or(&path).to_path_buf();
            let mut entries = vec![
                (path.clone(), FileType::Directory, ".".into()),
                (parent, FileType::Directory, "..".into()),
            ];
            for entry in fs::read_dir(&path)? {
                let entry = entry?;
                if path == self.source && entry.file_name() == OsStr::new(INTERNAL_DIR) {
                    continue;
                }
                let kind = FileType::from_std(entry.file_type()?)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
                entries.push((entry.path(), kind, entry.file_name()));
            }
            Ok(entries)
        })();

        let entries = match result {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(error.into());
                return;
            }
        };
        for (index, (path, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            let entry_ino = self
                .inodes
                .lock()
                .expect("inode table poisoned")
                .intern(path);
            if reply.add(entry_ino, (index + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
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
                .insert(file, path);
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

pub fn mount(source: &Path, mountpoint: &Path, state_dir: &Path) -> Result<()> {
    let source = source.canonicalize()?;
    let engine = if state_dir.join("session.json").exists() {
        StateEngine::open(&source, state_dir)?
    } else {
        StateEngine::create(&source, state_dir, EngineOptions::default())?
    };
    spawn_engine(&source, mountpoint, Arc::new(Mutex::new(engine)))?.wait()?;
    Ok(())
}

pub fn spawn_engine(
    source: &Path,
    mountpoint: &Path,
    engine: Arc<Mutex<StateEngine>>,
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
    let filesystem = DeltaFs::new(source, engine);
    let inodes = filesystem.inodes.clone();
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
    config.n_threads = Some(4);
    #[cfg(target_os = "linux")]
    {
        config.clone_fd = true;
    }
    let session = fuser::spawn_mount(filesystem, mountpoint, &config)?;
    Ok(MountedEngine { session, inodes })
}

fn renamed_path(new: &Path, suffix: &Path) -> PathBuf {
    if suffix.as_os_str().is_empty() {
        new.to_path_buf()
    } else {
        new.join(suffix)
    }
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
        let filesystem = DeltaFs::new(source.clone(), Arc::new(Mutex::new(engine)));
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
}

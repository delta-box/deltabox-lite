use crate::journal::{MutationTicket, UndoStore};
use anyhow::{bail, Context, Result};
use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_millis(100);

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
                    .map(|suffix| (*ino, path.clone(), new.join(suffix)))
            })
            .collect();
        for (ino, old_path, new_path) in replacements {
            self.by_path.remove(&old_path);
            self.by_path.insert(new_path.clone(), ino);
            self.by_inode.insert(ino, new_path);
        }
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
}

#[derive(Debug)]
pub struct DeltaFs {
    source: PathBuf,
    undo: Arc<UndoStore>,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HandleTable>,
    mutation_lock: Mutex<()>,
}

impl DeltaFs {
    fn new(source: PathBuf, undo: UndoStore) -> Self {
        Self {
            inodes: Mutex::new(InodeTable::new(source.clone())),
            source,
            undo: Arc::new(undo),
            handles: Mutex::new(HandleTable::new()),
            mutation_lock: Mutex::new(()),
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

    fn begin(&self, operation: &str, paths: &[PathBuf]) -> io::Result<MutationTicket> {
        self.undo.begin(operation, paths).map_err(io::Error::other)
    }

    fn applied(&self, ticket: MutationTicket) -> io::Result<()> {
        self.undo.applied(ticket).map_err(io::Error::other)
    }

    fn open_handle(&self, ino: INodeNo, flags: OpenFlags) -> io::Result<FileHandle> {
        let path = self.path(ino)?;
        let file = if flags.0 & libc::O_TRUNC != 0 {
            self.mutate("open_truncate", std::slice::from_ref(&path), || {
                open_file(&path, flags.0, false, 0)
            })?
        } else {
            open_file(&path, flags.0, false, 0)?
        };
        Ok(self
            .handles
            .lock()
            .expect("handle table poisoned")
            .insert(file, path))
    }

    fn mutate<T>(
        &self,
        operation: &str,
        paths: &[PathBuf],
        mutation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        let ticket = self.begin(operation, paths)?;
        let value = mutation()?;
        self.applied(ticket)?;
        Ok(value)
    }
}

impl Filesystem for DeltaFs {
    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
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
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
            let path = self.path(ino)?;
            self.mutate("setattr", std::slice::from_ref(&path), || {
                if let Some(mode) = mode {
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
                }
                if uid.is_some() || gid.is_some() {
                    let c_path = c_path(&path)?;
                    let rc = unsafe {
                        libc::chown(
                            c_path.as_ptr(),
                            uid.unwrap_or(u32::MAX),
                            gid.unwrap_or(u32::MAX),
                        )
                    };
                    if rc != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if let Some(size) = size {
                    if let Some(handle) = fh {
                        let handles = self.handles.lock().expect("handle table poisoned");
                        let open = handles
                            .files
                            .get(&handle.0)
                            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
                        open.file.set_len(size)?;
                    } else {
                        OpenOptions::new().write(true).open(&path)?.set_len(size)?;
                    }
                }
                if atime.is_some() || mtime.is_some() {
                    let now = SystemTime::now();
                    let mut times = FileTimes::new();
                    if let Some(atime) = atime {
                        times = times.set_accessed(resolve_time(atime, now));
                    }
                    if let Some(mtime) = mtime {
                        times = times.set_modified(resolve_time(mtime, now));
                    }
                    if let Some(handle) = fh {
                        let handles = self.handles.lock().expect("handle table poisoned");
                        let open = handles
                            .files
                            .get(&handle.0)
                            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
                        open.file.set_times(times)?;
                    } else {
                        File::open(&path)?.set_times(times)?;
                    }
                }
                Ok(())
            })?;
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
            self.mutate("mkdir", std::slice::from_ref(&path), || {
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(mode & !umask))
            })?;
            self.intern_attr(path)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error.into()),
        }
    }

    fn unlink(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            self.mutate("unlink", std::slice::from_ref(&path), || {
                fs::remove_file(&path)
            })
        });
        finish_empty(result, reply);
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            self.mutate("rmdir", std::slice::from_ref(&path), || {
                fs::remove_dir(&path)
            })
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
            self.mutate("symlink", std::slice::from_ref(&path), || {
                std::os::unix::fs::symlink(target, &path)
            })?;
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
            self.mutate("rename", &[old.clone(), new.clone()], || {
                fs::rename(&old, &new)
            })?;
            self.inodes
                .lock()
                .expect("inode table poisoned")
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
            self.mutate("link", &[source.clone(), target.clone()], || {
                fs::hard_link(&source, &target)
            })?;
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
            let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
            let handles = self.handles.lock().expect("handle table poisoned");
            let open = handles
                .files
                .get(&fh.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let ticket = self.begin("write", std::slice::from_ref(&open.mutation_path))?;
            let written = open.file.write_at(data, offset)?;
            self.applied(ticket)?;
            Ok(written)
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
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let result = self
            .handles
            .lock()
            .expect("handle table poisoned")
            .files
            .get(&fh.0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|open| open.file.sync_data());
        finish_empty(result, reply);
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
            self.mutate("setxattr", std::slice::from_ref(&path), || {
                let existing = xattr::get(&path, name)?;
                if flags & libc::XATTR_CREATE != 0 && existing.is_some() {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
                if flags & libc::XATTR_REPLACE != 0 && existing.is_none() {
                    return Err(io::Error::from_raw_os_error(no_xattr_errno()));
                }
                xattr::set(&path, name, value)
            })
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
            self.mutate("removexattr", std::slice::from_ref(&path), || {
                xattr::remove(&path, name)
            })
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
            let file = self.mutate("create", std::slice::from_ref(&path), || {
                open_file(&path, flags, true, mode & !umask)
            })?;
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

pub fn mount(source: &Path, mountpoint: &Path, undo_dir: &Path) -> Result<()> {
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
    let undo = UndoStore::open(&source, undo_dir)?;
    let filesystem = DeltaFs::new(source, undo);
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
    fuser::mount(filesystem, mountpoint, &config)?;
    Ok(())
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
    use crate::journal::{JournalEvent, PreimageKind};
    use tempfile::tempdir;

    #[test]
    fn open_truncate_captures_preimage_before_mutation() {
        let parent = tempdir().unwrap();
        let source = parent.path().join("source");
        let undo = parent.path().join("undo");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        let target = source.join("file.txt");
        fs::write(&target, b"before truncate").unwrap();

        let filesystem = DeltaFs::new(source.clone(), UndoStore::open(&source, &undo).unwrap());
        let ino = filesystem.intern_attr(target.clone()).unwrap().ino;
        filesystem
            .open_handle(ino, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
            .unwrap();

        assert_eq!(fs::metadata(&target).unwrap().len(), 0);
        let events: Vec<JournalEvent> = fs::read_to_string(filesystem.undo.journal_path())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let JournalEvent::Ready {
            operation,
            preimages,
            ..
        } = &events[0]
        else {
            panic!("first event must be ready");
        };
        assert_eq!(operation, "open_truncate");
        assert_eq!(preimages[0].kind, PreimageKind::Regular);
        assert_eq!(
            fs::read(undo.join(preimages[0].data.as_ref().unwrap())).unwrap(),
            b"before truncate"
        );
        assert!(matches!(events[1], JournalEvent::Applied { .. }));
    }
}

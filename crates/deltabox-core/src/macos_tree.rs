use crate::error::{Error, Result};
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::fs::{self, File};
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut c_void;
    fn acl_size(acl: *mut c_void) -> libc::ssize_t;
    fn acl_copy_ext_native(
        buffer: *mut c_void,
        acl: *mut c_void,
        size: libc::ssize_t,
    ) -> libc::ssize_t;
    fn acl_free(value: *mut c_void) -> libc::c_int;
    fn lchflags(path: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
}

pub(crate) fn clone_path(
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
            // Hard links share the inode: the first link's metadata pass
            // already covered this entry.
            fs::hard_link(existing, destination)?;
        } else {
            match crate::snapshot::clone_or_copy_file_with_mechanism(source, destination)? {
                crate::snapshot::CopyMechanism::ClonedWithMetadata => {
                    // The clone already carries mode, uid, timestamps, BSD
                    // flags, xattrs, and the ACL (see CopyMechanism). One gap
                    // remains: the clone inherits the destination directory's
                    // group instead of the source's. Skipping the full
                    // per-file metadata pass here is what keeps large-project
                    // session starts fast.
                    set_owner(destination, metadata.uid(), metadata.gid(), false)?;
                }
                crate::snapshot::CopyMechanism::ContentOnly => {
                    copy_metadata(source, destination)?;
                }
            }
            links.insert(identity, destination.to_path_buf());
        }
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

pub(crate) fn replace_path(
    source: &Path,
    destination: &Path,
    links: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Backend("replacement path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".deltafs-swap-{}", uuid::Uuid::new_v4()));
    clone_path(source, &temporary, links)?;
    if fs::symlink_metadata(destination).is_ok() {
        rename_swap(&temporary, destination)?;
        remove_path(&temporary)?;
    } else {
        fs::rename(&temporary, destination)?;
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_file() {
        let identity = (metadata.dev(), metadata.ino());
        if links.get(&identity) == Some(&temporary) {
            links.insert(identity, destination.to_path_buf());
        }
    }
    Ok(())
}

pub(crate) fn copy_metadata(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let symlink = metadata.file_type().is_symlink();
    set_flags(destination, 0, symlink)?;
    set_owner(destination, metadata.uid(), metadata.gid(), symlink)?;
    if !symlink {
        fs::set_permissions(destination, fs::Permissions::from_mode(metadata.mode()))?;
    }
    copy_acl(source, destination)?;
    let source_xattrs = xattr::list(source)?.collect::<Vec<_>>();
    for name in xattr::list(destination)? {
        if tracked_xattr(&name) && !source_xattrs.contains(&name) {
            xattr::remove(destination, &name)?;
        }
    }
    for name in source_xattrs.into_iter().filter(|name| tracked_xattr(name)) {
        if let Some(value) = xattr::get(source, &name)? {
            if xattr::get(destination, &name)?.as_deref() != Some(value.as_slice()) {
                xattr::set(destination, &name, &value)?;
            }
        }
    }
    set_times(
        destination,
        timestamp_ns(metadata.atime(), metadata.atime_nsec()),
        timestamp_ns(metadata.mtime(), metadata.mtime_nsec()),
        symlink,
    )?;
    set_flags(destination, metadata.st_flags(), symlink)
}

pub(crate) fn read_acl(path: &Path) -> Result<Vec<u8>> {
    let path = c_path(path)?;
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(Vec::new())
        } else {
            Err(error.into())
        };
    }
    let size = unsafe { acl_size(acl) };
    if size < 0 {
        unsafe { acl_free(acl) };
        return Err(std::io::Error::last_os_error().into());
    }
    let mut bytes = vec![0_u8; size as usize];
    let copied = unsafe { acl_copy_ext_native(bytes.as_mut_ptr().cast(), acl, size) };
    unsafe { acl_free(acl) };
    if copied < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    bytes.truncate(copied as usize);
    Ok(bytes)
}

pub(crate) fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn sync_tree(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| Error::Io(std::io::Error::other(error)))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file() {
            File::open(entry.path())?.sync_all()?;
        } else if metadata.file_type().is_dir() {
            directories.push(entry.path().to_path_buf());
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_dir(&directory)?;
    }
    Ok(())
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn set_owner(path: &Path, uid: u32, gid: u32, symlink: bool) -> Result<()> {
    let path = c_path(path)?;
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
    let path = c_path(path)?;
    let times = [timespec(atime_ns), timespec(mtime_ns)];
    let flags = if symlink {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn copy_acl(source: &Path, destination: &Path) -> Result<()> {
    let source = c_path(source)?;
    let destination = c_path(destination)?;
    let flags = libc::COPYFILE_ACL | libc::COPYFILE_NOFOLLOW;
    if unsafe {
        libc::copyfile(
            source.as_ptr(),
            destination.as_ptr(),
            std::ptr::null_mut(),
            flags,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn set_flags(path: &Path, flags: u32, symlink: bool) -> Result<()> {
    let path = c_path(path)?;
    let result = unsafe {
        if symlink {
            lchflags(path.as_ptr(), flags)
        } else {
            libc::chflags(path.as_ptr(), flags)
        }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Backend("path contains a null byte".into()))
}

pub(crate) fn tracked_xattr(name: &std::ffi::OsStr) -> bool {
    name.as_bytes() != b"com.apple.provenance"
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

fn rename_swap(left: &Path, right: &Path) -> Result<()> {
    let left = c_path(left)?;
    let right = c_path(right)?;
    const RENAME_SWAP: libc::c_uint = 0x0000_0002;
    if unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

//! Checked barriers on the backing NTFS volume, never on the WinFsp view.
use super::engine::wide;
use crate::{Error, Result};
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FlushFileBuffers, GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
};

pub(super) fn open(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .share_mode(7)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
        )
        .open(path)?)
}

pub(super) fn flush_handle(handle: HANDLE) -> Result<()> {
    point("flush_file")?;
    if unsafe { FlushFileBuffers(handle) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub(super) fn flush(file: &File) -> Result<()> {
    flush_handle(file.as_raw_handle())
}

pub(super) fn directory(path: &Path) -> Result<()> {
    point("flush_directory")?;
    flush(&open(path)?)
}

/// Flush every surviving namespace from a changed entry up to its tree root.
/// This also handles retrying a deletion whose immediate parent is now gone.
pub(super) fn parents(path: &Path, root: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(parent) = current {
        if !parent.starts_with(root) {
            break;
        }
        match fs::symlink_metadata(parent) {
            Ok(m) if m.is_dir() => directory(parent)?,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if parent == root {
            break;
        }
        current = parent.parent();
    }
    Ok(())
}

pub(crate) fn missing_directories(path: &Path) -> Vec<PathBuf> {
    path.ancestors()
        .take_while(|p| !p.exists())
        .map(Path::to_path_buf)
        .collect()
}

pub(crate) fn persist_directories(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        directory(path)?;
        if let Some(parent) = path.parent() {
            directory(parent)?;
        }
    }
    Ok(())
}

pub(super) fn create_directories(path: &Path, durable: bool) -> Result<()> {
    let missing = if durable {
        missing_directories(path)
    } else {
        Vec::new()
    };
    fs::create_dir_all(path)?;
    persist_directories(&missing)
}

/// Initial qualification is intentionally limited to local fixed NTFS volumes.
/// Test the real directory ACLs as well as the volume type; never degrade Strict.
pub(crate) fn qualify(path: &Path) -> Result<()> {
    let mut volume = vec![0u16; 32768];
    if unsafe {
        GetVolumePathNameW(
            wide(path).as_ptr(),
            volume.as_mut_ptr(),
            volume.len() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut filesystem = [0u16; 64];
    if unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let length = filesystem
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(filesystem.len());
    if unsafe { GetDriveTypeW(volume.as_ptr()) } != 3
        || String::from_utf16_lossy(&filesystem[..length]) != "NTFS"
    {
        return Err(Error::Unsupported(
            "Windows strict durability requires local fixed NTFS project and state volumes".into(),
        ));
    }
    directory(path)
}

/// Faults are local to a test thread; subprocess crash tests use the same labels.
pub(super) fn point(name: &str) -> Result<()> {
    #[cfg(test)]
    {
        let fail = FAIL.with(|fail| {
            let mut fail = fail.borrow_mut();
            if fail.as_deref() == Some(name) {
                fail.take();
                true
            } else {
                false
            }
        });
        if fail {
            return Err(std::io::Error::from_raw_os_error(1117).into());
        }
    }
    if cfg!(debug_assertions) && std::env::var("DELTABOX_TEST_CRASH_POINT").as_deref() == Ok(name) {
        std::process::exit(86);
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

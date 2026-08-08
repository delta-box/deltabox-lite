use crc32fast::Hasher;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::model::{
    MetadataSnapshot, ObjectSnapshot, ObjectSnapshotKind, SegmentRef, UndoAction, VInodeId,
};
use super::{EngineError, Result};

const PREIMAGE_METADATA_ALLOWANCE: u64 = 1024;

#[derive(Debug, Clone)]
pub struct ObjectCapturePlan {
    pub snapshot: ObjectSnapshot,
    pub preimage_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathState {
    Absent,
    Regular {
        data: Vec<u8>,
        metadata: ComparableMetadata,
    },
    Directory {
        metadata: ComparableMetadata,
    },
    Symlink {
        target: String,
        metadata: ComparableMetadata,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparableMetadata {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_ns: i64,
    pub xattrs: BTreeMap<String, Vec<u8>>,
}

pub fn normalize_relative(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(EngineError::InvalidPath(path.to_path_buf()));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| EngineError::InvalidPath(path.to_path_buf()))?,
            ),
            _ => return Err(EngineError::InvalidPath(path.to_path_buf())),
        }
    }
    if parts.is_empty() {
        return Err(EngineError::InvalidPath(path.to_path_buf()));
    }
    Ok(parts.join("/"))
}

pub fn checked_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty() {
        return Ok(root.to_path_buf());
    }
    normalize_relative(Path::new(relative))?;
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::InvalidPath(PathBuf::from(relative)))?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(root) {
        return Err(EngineError::PathEscape(PathBuf::from(relative)));
    }
    Ok(path)
}

pub fn metadata_snapshot(path: &Path) -> Result<MetadataSnapshot> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(MetadataSnapshot {
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        atime_ns: timestamp_ns(metadata.atime(), metadata.atime_nsec())?,
        mtime_ns: timestamp_ns(metadata.mtime(), metadata.mtime_nsec())?,
        xattrs: read_xattrs(path)?,
    })
}

pub fn plan_segment(
    record_id: u64,
    index: usize,
    offset: u64,
    length: u64,
) -> Result<(SegmentRef, u64)> {
    Ok((
        SegmentRef {
            offset,
            length,
            file: format!("segment-{record_id:016x}-{index:04}.bin"),
            checksum: u32::MAX,
        },
        length
            .checked_add(PREIMAGE_METADATA_ALLOWANCE)
            .ok_or_else(|| EngineError::Invariant("segment accounting overflow".into()))?,
    ))
}

pub fn materialize_segment(path: &Path, preimages: &Path, plan: &SegmentRef) -> Result<SegmentRef> {
    let mut source = File::open(path)?;
    source.seek(SeekFrom::Start(plan.offset))?;
    fs::create_dir_all(preimages)?;
    let destination = preimages.join(&plan.file);
    let result = (|| {
        let mut output = File::create(&destination)?;
        let mut remaining = plan.length;
        let mut buffer = [0_u8; 64 * 1024];
        let mut hasher = Hasher::new();
        while remaining > 0 {
            let length = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("bounded by the fixed capture buffer");
            source.read_exact(&mut buffer[..length])?;
            output.write_all(&buffer[..length])?;
            hasher.update(&buffer[..length]);
            remaining -= length as u64;
        }
        Ok::<u32, std::io::Error>(hasher.finalize())
    })();
    let checksum = match result {
        Ok(checksum) => checksum,
        Err(error) => {
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }
    };
    let mut segment = plan.clone();
    segment.checksum = checksum;
    Ok(segment)
}

pub fn plan_object(
    path: &Path,
    record_id: u64,
    inode: VInodeId,
    hardlink_source: Option<String>,
) -> Result<ObjectCapturePlan> {
    let metadata = metadata_snapshot(path)?;
    let stat = fs::symlink_metadata(path)?;
    let file_type = stat.file_type();
    let (kind, bytes) = if let Some(source) = hardlink_source {
        (
            ObjectSnapshotKind::Hardlink { source },
            PREIMAGE_METADATA_ALLOWANCE,
        )
    } else if file_type.is_file() {
        let name = format!("object-{record_id:016x}.bin");
        (
            ObjectSnapshotKind::Regular {
                data: name,
                checksum: u32::MAX,
            },
            stat.len()
                .checked_add(PREIMAGE_METADATA_ALLOWANCE)
                .ok_or_else(|| EngineError::Invariant("object accounting overflow".into()))?,
        )
    } else if file_type.is_dir() {
        (ObjectSnapshotKind::Directory, PREIMAGE_METADATA_ALLOWANCE)
    } else if file_type.is_symlink() {
        let target = fs::read_link(path)?;
        let target = target
            .to_str()
            .ok_or_else(|| EngineError::InvalidPath(target.clone()))?
            .to_owned();
        (
            ObjectSnapshotKind::Symlink { target },
            PREIMAGE_METADATA_ALLOWANCE,
        )
    } else {
        return Err(EngineError::UnsupportedFileType(path.to_path_buf()));
    };
    Ok(ObjectCapturePlan {
        snapshot: ObjectSnapshot {
            inode,
            kind,
            metadata,
        },
        preimage_bytes: bytes,
    })
}

pub fn materialize_object(
    path: &Path,
    preimages: &Path,
    plan: &ObjectCapturePlan,
) -> Result<ObjectSnapshot> {
    let mut snapshot = plan.snapshot.clone();
    if let ObjectSnapshotKind::Regular { data, checksum } = &mut snapshot.kind {
        fs::create_dir_all(preimages)?;
        let destination = preimages.join(&*data);
        let copied = match fs::copy(path, &destination) {
            Ok(copied) => copied,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                return Err(error.into());
            }
        };
        let expected = plan
            .preimage_bytes
            .saturating_sub(PREIMAGE_METADATA_ALLOWANCE);
        if copied != expected {
            let _ = fs::remove_file(&destination);
            return Err(EngineError::Invariant(format!(
                "object changed while capturing: expected {expected} bytes, copied {copied}"
            )));
        }
        *checksum = match hash_file(&destination) {
            Ok(checksum) => checksum,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                return Err(error);
            }
        };
    }
    Ok(snapshot)
}

fn hash_file(path: &Path) -> Result<u32> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut hasher = Hasher::new();
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(hasher.finalize())
}

pub fn apply_undo(root: &Path, preimages: &Path, action: &UndoAction) -> Result<()> {
    match action {
        UndoAction::RemoveCreated { path } => remove_path_if_exists(&checked_path(root, path)?)?,
        UndoAction::RestoreObject { path, snapshot } => {
            let destination = checked_path(root, path)?;
            if !destination.exists() && fs::symlink_metadata(&destination).is_err() {
                restore_object(root, preimages, &destination, snapshot)?;
            }
        }
        UndoAction::RestoreRanges {
            path,
            old_size,
            segments,
            ..
        } => {
            let destination = checked_path(root, path)?;
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&destination)?;
            for segment in segments {
                let preimage = preimages.join(&segment.file);
                if fs::metadata(&preimage)?.len() != segment.length
                    || hash_file(&preimage)? != segment.checksum
                {
                    return Err(EngineError::CorruptState(format!(
                        "segment checksum mismatch: {}",
                        segment.file
                    )));
                }
                let mut source = File::open(preimage)?.take(segment.length);
                file.seek(SeekFrom::Start(segment.offset))?;
                let copied = std::io::copy(&mut source, &mut file)?;
                if copied != segment.length {
                    return Err(EngineError::CorruptState(format!(
                        "segment length changed while restoring: {}",
                        segment.file
                    )));
                }
            }
            if let Some(size) = old_size {
                file.set_len(*size)?;
            }
        }
        UndoAction::UndoRename {
            old_path,
            new_path,
            replaced,
            ..
        } => {
            let old = checked_path(root, old_path)?;
            let new = checked_path(root, new_path)?;
            if fs::symlink_metadata(&old).is_err() {
                if fs::symlink_metadata(&new).is_err() {
                    return Err(EngineError::Recovery(format!(
                        "rename undo has neither {} nor {}",
                        old.display(),
                        new.display()
                    )));
                }
                fs::rename(&new, &old)?;
            }
            if let Some(snapshot) = replaced {
                if fs::symlink_metadata(&new).is_err() {
                    restore_object(root, preimages, &new, snapshot)?;
                }
            } else {
                remove_path_if_exists(&new)?;
            }
        }
        UndoAction::RestoreMode { path, mode, .. } => {
            let destination = checked_path(root, path)?;
            fs::set_permissions(destination, fs::Permissions::from_mode(*mode))?;
        }
        UndoAction::RestoreOwner { path, uid, gid, .. } => {
            let destination = checked_path(root, path)?;
            set_owner(&destination, *uid, *gid)?;
        }
        UndoAction::RestoreTimes {
            path,
            atime_ns,
            mtime_ns,
            ..
        } => {
            let destination = checked_path(root, path)?;
            set_times(&destination, *atime_ns, *mtime_ns)?;
        }
        UndoAction::RestoreXattr {
            path, name, value, ..
        } => {
            let destination = checked_path(root, path)?;
            match value {
                Some(value) => xattr::set(destination, name, value)?,
                None => match xattr::remove(destination, name) {
                    Ok(()) => {}
                    Err(error) if is_missing_xattr(&error) => {}
                    Err(error) => return Err(error.into()),
                },
            }
        }
    }
    Ok(())
}

pub fn path_state(root: &Path, relative: &str) -> Result<PathState> {
    let path = checked_path(root, relative)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(PathState::Absent),
        Err(error) => return Err(error.into()),
    };
    let comparable = ComparableMetadata {
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mtime_ns: timestamp_ns(metadata.mtime(), metadata.mtime_nsec())?,
        xattrs: read_xattrs(&path)?,
    };
    if metadata.file_type().is_file() {
        Ok(PathState::Regular {
            data: fs::read(path)?,
            metadata: comparable,
        })
    } else if metadata.file_type().is_dir() {
        Ok(PathState::Directory {
            metadata: comparable,
        })
    } else if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        Ok(PathState::Symlink {
            target: target
                .to_str()
                .ok_or_else(|| EngineError::InvalidPath(target.clone()))?
                .to_owned(),
            metadata: comparable,
        })
    } else {
        Err(EngineError::UnsupportedFileType(path))
    }
}

pub fn copy_path_state(source_root: &Path, destination_root: &Path, relative: &str) -> Result<()> {
    let source = checked_path(source_root, relative)?;
    let destination = destination_root.join(relative);
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if metadata.file_type().is_file() {
        fs::copy(&source, &destination)?;
    } else if metadata.file_type().is_dir() {
        fs::create_dir_all(&destination)?;
    } else if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(&source)?, &destination)?;
    } else {
        return Err(EngineError::UnsupportedFileType(source));
    }
    apply_metadata(&destination, &metadata_snapshot(&source)?)?;
    Ok(())
}

pub fn remove_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn set_owner(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let path = c_path(path)?;
    let rc = unsafe { libc::chown(path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn set_times(path: &Path, atime_ns: i64, mtime_ns: i64) -> Result<()> {
    let times = FileTimes::new()
        .set_accessed(system_time(atime_ns))
        .set_modified(system_time(mtime_ns));
    File::open(path)?.set_times(times)?;
    Ok(())
}

fn restore_object(
    root: &Path,
    preimages: &Path,
    destination: &Path,
    snapshot: &ObjectSnapshot,
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match &snapshot.kind {
        ObjectSnapshotKind::Regular { data, checksum } => {
            let preimage = preimages.join(data);
            if hash_file(&preimage)? != *checksum {
                return Err(EngineError::CorruptState(format!(
                    "object checksum mismatch: {data}"
                )));
            }
            fs::copy(preimage, destination)?;
        }
        ObjectSnapshotKind::Directory => fs::create_dir(destination)?,
        ObjectSnapshotKind::Symlink { target } => std::os::unix::fs::symlink(target, destination)?,
        ObjectSnapshotKind::Hardlink { source } => {
            fs::hard_link(checked_path(root, source)?, destination)?
        }
    }
    apply_metadata(destination, &snapshot.metadata)?;
    Ok(())
}

fn apply_metadata(path: &Path, metadata: &MetadataSnapshot) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_symlink() {
        let current = fs::metadata(path)?;
        if current.uid() != metadata.uid || current.gid() != metadata.gid {
            set_owner(path, metadata.uid, metadata.gid)?;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode))?;
        set_times(path, metadata.atime_ns, metadata.mtime_ns)?;
    }
    for (name, value) in &metadata.xattrs {
        xattr::set(path, name, value)?;
    }
    Ok(())
}

fn read_xattrs(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut values = BTreeMap::new();
    for name in xattr::list(path)? {
        let name = name
            .to_str()
            .ok_or_else(|| EngineError::InvalidPath(PathBuf::from(&name)))?
            .to_owned();
        if let Some(value) = xattr::get(path, &name)? {
            values.insert(name, value);
        }
    }
    Ok(values)
}

fn timestamp_ns(seconds: i64, nanos: i64) -> Result<i64> {
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or_else(|| {
            EngineError::Invariant("filesystem timestamp is outside i64 nanoseconds".into())
        })
}

fn system_time(nanos: i64) -> SystemTime {
    let magnitude = nanos.unsigned_abs();
    let duration = Duration::new(
        magnitude / 1_000_000_000,
        (magnitude % 1_000_000_000) as u32,
    );
    if nanos >= 0 {
        UNIX_EPOCH + duration
    } else {
        UNIX_EPOCH - duration
    }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| EngineError::InvalidPath(path.to_path_buf()))
}

#[cfg(target_os = "linux")]
fn is_missing_xattr(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODATA)
}

#[cfg(target_os = "macos")]
fn is_missing_xattr(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOATTR)
}

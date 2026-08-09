use crc32fast::Hasher;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::model::{
    DataExtent, MetadataSnapshot, ObjectSnapshot, ObjectSnapshotKind, SegmentRef, SparseLayout,
    UndoAction, VInodeId,
};
use super::ranges::ByteRange;
use super::{EngineError, Result};

const PREIMAGE_METADATA_ALLOWANCE: u64 = 1024;

#[derive(Debug, Clone)]
pub struct ObjectCapturePlan {
    pub snapshot: ObjectSnapshot,
    pub preimage_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RangeCapturePlan {
    pub segments: Vec<SegmentRef>,
    pub holes: Vec<DataExtent>,
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
    let mut parent = path
        .parent()
        .ok_or_else(|| EngineError::InvalidPath(PathBuf::from(relative)))?;
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent = parent
                    .parent()
                    .ok_or_else(|| EngineError::InvalidPath(PathBuf::from(relative)))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
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

pub fn plan_ranges(path: &Path, record_id: u64, ranges: &[ByteRange]) -> Result<RangeCapturePlan> {
    let mut segments = Vec::new();
    let mut holes = Vec::new();
    let mut preimage_bytes = 0_u64;
    for range in ranges {
        let extents = data_extents(path, range.start, range.end)?;
        let mut cursor = range.start;
        for extent in extents {
            if cursor < extent.offset {
                holes.push(DataExtent {
                    offset: cursor,
                    length: extent.offset - cursor,
                });
            }
            let (segment, bytes) =
                plan_segment(record_id, segments.len(), extent.offset, extent.length)?;
            segments.push(segment);
            preimage_bytes = preimage_bytes
                .checked_add(bytes)
                .ok_or_else(|| EngineError::Invariant("range accounting overflow".into()))?;
            cursor = extent.offset + extent.length;
        }
        if cursor < range.end {
            holes.push(DataExtent {
                offset: cursor,
                length: range.end - cursor,
            });
        }
    }
    Ok(RangeCapturePlan {
        segments,
        holes,
        preimage_bytes,
    })
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
        let sparse = sparse_layout(path, stat.len())?;
        let data_bytes = sparse.extents.iter().try_fold(0_u64, |total, extent| {
            total
                .checked_add(extent.length)
                .ok_or_else(|| EngineError::Invariant("sparse extent accounting overflow".into()))
        })?;
        (
            ObjectSnapshotKind::Regular {
                data: name,
                checksum: u32::MAX,
                sparse: Some(sparse),
            },
            data_bytes
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
    if let ObjectSnapshotKind::Regular {
        data,
        checksum,
        sparse,
    } = &mut snapshot.kind
    {
        fs::create_dir_all(preimages)?;
        let destination = preimages.join(&*data);
        let copied = match copy_sparse(
            path,
            &destination,
            sparse
                .as_ref()
                .expect("new regular snapshots include sparse layout"),
        ) {
            Ok(copied) => copied,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                return Err(error);
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
        *checksum = match hash_sparse(
            &destination,
            sparse
                .as_ref()
                .expect("new regular snapshots include sparse layout"),
        ) {
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

fn hash_sparse(path: &Path, layout: &SparseLayout) -> Result<u32> {
    let file = File::open(path)?;
    let mut hasher = Hasher::new();
    hasher.update(&layout.size.to_le_bytes());
    let mut buffer = [0_u8; 64 * 1024];
    for extent in &layout.extents {
        hasher.update(&extent.offset.to_le_bytes());
        hasher.update(&extent.length.to_le_bytes());
        let mut offset = extent.offset;
        let end = extent.offset + extent.length;
        while offset < end {
            let length = usize::try_from((end - offset).min(buffer.len() as u64))
                .expect("buffer-sized extent chunk");
            let read = file.read_at(&mut buffer[..length], offset)?;
            if read != length {
                return Err(EngineError::Invariant(
                    "sparse pre-image changed while hashing".into(),
                ));
            }
            hasher.update(&buffer[..read]);
            offset += read as u64;
        }
    }
    Ok(hasher.finalize())
}

fn copy_sparse(source: &Path, destination: &Path, layout: &SparseLayout) -> Result<u64> {
    if fs::metadata(source)?.len() != layout.size {
        return Err(EngineError::Invariant(
            "sparse source changed while capturing".into(),
        ));
    }
    let source = File::open(source)?;
    let destination_file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(destination)?;
    destination_file.set_len(layout.size)?;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    for extent in &layout.extents {
        let mut offset = extent.offset;
        let end = extent.offset + extent.length;
        while offset < end {
            let length = usize::try_from((end - offset).min(buffer.len() as u64))
                .expect("buffer-sized extent chunk");
            let read = source.read_at(&mut buffer[..length], offset)?;
            if read != length {
                return Err(EngineError::Invariant(
                    "sparse source changed while copying".into(),
                ));
            }
            write_all_at(&destination_file, &buffer[..read], offset)?;
            offset += read as u64;
            copied += read as u64;
        }
    }
    Ok(copied)
}

fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !data.is_empty() {
        let written = file.write_at(data, offset)?;
        if written == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        data = &data[written..];
        offset += written as u64;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sparse_layout(path: &Path, size: u64) -> Result<SparseLayout> {
    let extents = data_extents(path, 0, size)?;
    Ok(SparseLayout { size, extents })
}

#[cfg(target_os = "linux")]
fn data_extents(path: &Path, start: u64, end: u64) -> Result<Vec<DataExtent>> {
    if start >= end {
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let mut extents = Vec::new();
    let mut offset = start;
    while offset < end {
        let data = unsafe { libc::lseek(file.as_raw_fd(), offset as i64, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            if error.raw_os_error() == Some(libc::EINVAL) {
                return Ok(vec![DataExtent {
                    offset: start,
                    length: end - start,
                }]);
            }
            return Err(error.into());
        }
        let data = data as u64;
        if data >= end {
            break;
        }
        let hole = unsafe { libc::lseek(file.as_raw_fd(), data as i64, libc::SEEK_HOLE) };
        let hole = if hole < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                end
            } else {
                return Err(error.into());
            }
        } else {
            (hole as u64).min(end)
        };
        if data >= hole {
            return Err(EngineError::Invariant(
                "filesystem returned an invalid sparse extent".into(),
            ));
        }
        extents.push(DataExtent {
            offset: data,
            length: hole - data,
        });
        offset = hole;
    }
    Ok(extents)
}

#[cfg(not(target_os = "linux"))]
fn sparse_layout(_path: &Path, size: u64) -> Result<SparseLayout> {
    Ok(SparseLayout {
        size,
        extents: (size != 0)
            .then_some(DataExtent {
                offset: 0,
                length: size,
            })
            .into_iter()
            .collect(),
    })
}

#[cfg(not(target_os = "linux"))]
fn data_extents(_path: &Path, start: u64, end: u64) -> Result<Vec<DataExtent>> {
    Ok((start < end)
        .then_some(DataExtent {
            offset: start,
            length: end - start,
        })
        .into_iter()
        .collect())
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
            holes,
            ..
        } => {
            let destination = checked_path(root, path)?;
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&destination)?;
            if let Some(size) = old_size {
                if file.metadata()?.len() < *size {
                    file.set_len(*size)?;
                }
            }
            punch_holes(&file, holes)?;
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
        UndoAction::UndoExchange {
            first_path,
            second_path,
            first_backing,
            second_backing,
        } => {
            let first = checked_path(root, first_path)?;
            let second = checked_path(root, second_path)?;
            let first_current = fs::symlink_metadata(&first)?;
            let second_current = fs::symlink_metadata(&second)?;
            let first_current = super::model::BackingIdentity {
                device: first_current.dev(),
                inode: first_current.ino(),
            };
            let second_current = super::model::BackingIdentity {
                device: second_current.dev(),
                inode: second_current.ino(),
            };
            if first_current == *second_backing && second_current == *first_backing {
                rename_exchange(&first, &second)?;
            } else if first_current != *first_backing || second_current != *second_backing {
                return Err(EngineError::Recovery(format!(
                    "rename exchange operands changed: {} and {}",
                    first.display(),
                    second.display()
                )));
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
        ObjectSnapshotKind::Regular {
            data,
            checksum,
            sparse,
        } => {
            let preimage = preimages.join(data);
            let actual = match sparse {
                Some(layout) => hash_sparse(&preimage, layout)?,
                None => hash_file(&preimage)?,
            };
            if actual != *checksum {
                return Err(EngineError::CorruptState(format!(
                    "object checksum mismatch: {data}"
                )));
            }
            match sparse {
                Some(layout) => {
                    copy_sparse(&preimage, destination, layout)?;
                }
                None => {
                    fs::copy(preimage, destination)?;
                }
            }
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
fn punch_holes(file: &File, holes: &[DataExtent]) -> Result<()> {
    for hole in holes {
        let result = unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                hole.offset as libc::off_t,
                hole.length as libc::off_t,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn punch_holes(_file: &File, holes: &[DataExtent]) -> Result<()> {
    if holes.is_empty() {
        Ok(())
    } else {
        Err(EngineError::UnsupportedFileType(PathBuf::from(
            "sparse range restoration",
        )))
    }
}

#[cfg(target_os = "linux")]
pub fn rename_exchange(first: &Path, second: &Path) -> Result<()> {
    let first = c_path(first)?;
    let second = c_path(second)?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn rename_exchange(_first: &Path, _second: &Path) -> Result<()> {
    Err(EngineError::UnsupportedFileType(PathBuf::from(
        "rename exchange",
    )))
}

#[cfg(target_os = "linux")]
fn is_missing_xattr(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODATA)
}

#[cfg(target_os = "macos")]
fn is_missing_xattr(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOATTR)
}

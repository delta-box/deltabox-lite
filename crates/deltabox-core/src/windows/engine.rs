//! Undo journal for the Windows passthrough view. Mutations and boundaries share one mutex.
use crate::changeset::{describe_change, ChangeKind, DEFAULT_TEXT_SIZE_LIMIT};
use crate::error::{Error, Result};
use crate::policy::Policy;
use crate::protocol::{ActiveReview, CheckpointInfo, DiffResult, DurabilityMode, FilesystemMode, SessionStatus};
use super::durability;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, SetFileAttributesW, SetFileTime, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
use std::os::windows::io::AsRawHandle;

pub const WRITE: u32 = 1;
pub const CREATE: u32 = 2;
pub const RESIZE: u32 = 3;
pub const DELETE: u32 = 4;
pub const RENAME: u32 = 5;
pub const METADATA: u32 = 6;

#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub session_id: String,
    pub source: PathBuf,
    /// Absent in journals written before private mode was introduced.
    #[serde(default = "through_mode")]
    pub fs_mode: FilesystemMode,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub durability: DurabilityMode,
    #[serde(default)]
    pub live: Option<PathBuf>,
    pub view: PathBuf,
    pub scratch: PathBuf,
    pub token: String,
    pub started_at_ms: u64,
    pub status: SessionStatus,
    pub policy: Policy,
    pub next_id: u64,
    pub accepted: String,
    pub checkpoints: Vec<Savepoint>,
    pub review: Option<ActiveReview>,
    pub rollback: Option<u64>,
}

fn through_mode() -> FilesystemMode { FilesystemMode::CbcowThrough }

impl Manifest {
    pub fn live(&self) -> &Path { self.live.as_deref().unwrap_or(&self.source) }
    pub fn durable(&self) -> bool { self.durability == DurabilityMode::Strict }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Savepoint {
    pub info: CheckpointInfo,
    pub cursor: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Metadata {
    attributes: u32,
    creation: u64,
    access: u64,
    write: u64,
}

impl Metadata {
    pub(super) fn read(path: &Path) -> Result<Self> {
        let m = fs::symlink_metadata(path)?;
        Ok(Self { attributes: m.file_attributes(), creation: m.creation_time(), access: m.last_access_time(), write: m.last_write_time() })
    }
    pub(super) fn restore(&self, path: &Path) -> Result<()> {
        let f = OpenOptions::new().read(true).access_mode(0x100).share_mode(7)
            .custom_flags(0x02000000).open(path)?;
        let time = |n: u64| FILETIME { dwLowDateTime: n as u32, dwHighDateTime: (n >> 32) as u32 };
        let (c, a, w) = (time(self.creation), time(self.access), time(self.write));
        if unsafe { SetFileTime(f.as_raw_handle(), &c, &a, &w) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut attrs = self.attributes & !0x10;
        if attrs == 0 { attrs = 0x80; }
        if unsafe { SetFileAttributesW(wide(path).as_ptr(), attrs) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub(super) fn restore_durable(&self, path: &Path, durable: bool) -> Result<()> {
        // Recovery may need to restore a file that the interrupted operation made
        // read-only. Acquire the flush handle before restoring its final attributes.
        let handle = if durable {
            let attributes = fs::metadata(path)?.file_attributes();
            let writable = attributes & !1;
            if attributes & 1 != 0 && unsafe { SetFileAttributesW(wide(path).as_ptr(), if writable == 0 {0x80} else {writable}) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Some(durability::open(path)?)
        } else { None };
        self.restore(path)?;
        if let Some(handle) = handle {
            use windows_sys::Win32::Storage::FileSystem::{SetFileInformationByHandle, FileBasicInfo, FILE_BASIC_INFO};
            let basic = FILE_BASIC_INFO { CreationTime: 0, LastAccessTime: 0, LastWriteTime: 0, ChangeTime: 0, FileAttributes: self.attributes };
            if unsafe { SetFileInformationByHandle(handle.as_raw_handle(), FileBasicInfo, &basic as *const _ as *const _, std::mem::size_of_val(&basic) as u32) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            durability::flush(&handle)?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Segment { offset: u64, length: u64, blob: String }

#[derive(Clone, Serialize, Deserialize)]
enum Snapshot {
    Missing,
    Directory { metadata: Metadata },
    File { metadata: Metadata, blob: String },
}

#[derive(Clone, Serialize, Deserialize)]
enum Undo {
    Remove { path: String },
    Restore { path: String, snapshot: Snapshot },
    Range { path: String, old_size: u64, metadata: Metadata, segments: Vec<Segment> },
    Rename { old: String, new: String, identity: u64, replaced: Snapshot, #[serde(default)] source_metadata: Option<Metadata> },
    Metadata { path: String, metadata: Metadata },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    id: u64,
    touched: Vec<String>,
    undo: Undo,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
enum Event { Ready(Record), Applied(u64), Undone(u64) }

#[derive(Serialize, Deserialize)]
struct Envelope { sha256: String, event: String }

pub struct Engine {
    pub manifest: Manifest,
    pub state: PathBuf,
    records: Vec<Record>,
    poisoned: bool,
    pending_flush: Option<File>,
    dirty_namespaces: BTreeSet<PathBuf>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn wide(path: &Path) -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() }

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_json_durable(path, value, false)
}

pub(crate) fn write_json_durable(path: &Path, value: &impl Serialize, durable: bool) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&temp)?;
    serde_json::to_writer(&mut f, value)?;
    f.write_all(b"\n")?;
    if durable { durability::flush(&f)?; } else { f.sync_all()?; }
    drop(f);
    if unsafe { MoveFileExW(wide(&temp).as_ptr(), wide(path).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if durable { durability::directory(path.parent().unwrap())?; }
    Ok(())
}

fn read_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).share_mode(7).open(path)
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) => {
            if m.file_attributes() & 1 != 0 {
                if unsafe { SetFileAttributesW(wide(path).as_ptr(), m.file_attributes() & !1) } == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            // Decide by the directory attribute, not `is_dir()`: a junction or a
            // directory symlink reports `is_dir() == false` but must be removed
            // with RemoveDirectory, without descending into its target.
            if m.file_attributes() & 0x10 != 0 {
                if m.file_attributes() & 0x400 == 0 {
                    for entry in fs::read_dir(path)? { remove(&entry?.path())?; }
                }
                fs::remove_dir(path)?;
            } else { fs::remove_file(path)?; }
            Ok(())
        }
    }
}

pub(super) fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    copy_tree_durable(source, target, false)
}

pub(super) fn copy_tree_durable(source: &Path, target: &Path, durable: bool) -> Result<()> {
    let m = match fs::symlink_metadata(source) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if m.file_attributes() & 0x400 != 0 { return Err(Error::Unsupported("reparse points in the Windows filesystem view".into())); }
    if m.is_dir() {
        durability::create_directories(target, durable)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree_durable(&entry.path(), &target.join(entry.file_name()), durable)?;
        }
    } else {
        durability::create_directories(target.parent().unwrap(), durable)?;
        fs::copy(source, target)?;
    }
    Metadata::read(source)?.restore_durable(target, durable)?;
    if durable { durability::directory(target.parent().unwrap())?; }
    Ok(())
}

fn identity(path: &Path) -> Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let f = OpenOptions::new().read(true).share_mode(7).custom_flags(0x02000000).open(path)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 { return Err(std::io::Error::last_os_error().into()); }
    Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}

impl Engine {
    pub fn create(state: &Path, mut manifest: Manifest) -> Result<Self> {
        if manifest.durable() {
            if manifest.fs_mode != FilesystemMode::CbcowPrivate {
                return Err(Error::Unsupported("Windows strict durability requires cbcow-private".into()));
            }
            durability::qualify(&manifest.source)?;
            durability::qualify(state.ancestors().find(|p| p.is_dir()).unwrap())?;
        }
        durability::create_directories(&state.join("preimages"), manifest.durable())?;
        if manifest.fs_mode == FilesystemMode::CbcowPrivate {
            super::private::initialize(state, &mut manifest)?;
        }
        OpenOptions::new().write(true).create_new(true).open(state.join("journal.jsonl"))?.sync_all()?;
        write_json_durable(&state.join("session.json"), &manifest, manifest.durable())?;
        Self::open(state)
    }

    pub fn open(state: &Path) -> Result<Self> {
        super::private::recover(state)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(state.join("session.json"))?)?;
        if manifest.version != 1 { return Err(Error::Unsupported("unknown Windows journal version".into())); }
        if manifest.durable() {
            if manifest.fs_mode != FilesystemMode::CbcowPrivate { return Err(Error::Unsupported("Windows strict durability requires cbcow-private".into())); }
            durability::qualify(&manifest.source)?;
            durability::qualify(state)?;
        }
        let bytes = fs::read(state.join("journal.jsonl"))?;
        let mut active = BTreeMap::new();
        let mut pending = None;
        let mut valid = 0;
        let mut next_id = manifest.next_id;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            if line.last() != Some(&b'\n') { break; }
            let envelope: Envelope = serde_json::from_slice(line)?;
            if envelope.sha256 != format!("{:x}", Sha256::digest(envelope.event.as_bytes())) {
                return Err(Error::Backend("Windows undo journal checksum mismatch".into()));
            }
            match serde_json::from_str(&envelope.event)? {
                Event::Ready(record) => {
                    if pending.is_some() { return Err(Error::Backend("overlapping Windows journal mutations".into())); }
                    next_id = next_id.max(record.id + 1);
                    pending = Some(record);
                }
                Event::Applied(id) => {
                    let record = pending.take().filter(|r| r.id == id).ok_or_else(|| Error::Backend("APPLIED without READY".into()))?;
                    active.insert(id, record);
                }
                Event::Undone(id) => {
                    if pending.as_ref().is_some_and(|r| r.id == id) { pending = None; }
                    active.remove(&id);
                }
            }
            valid += line.len();
        }
        if valid != bytes.len() {
            let journal = OpenOptions::new().write(true).open(state.join("journal.jsonl"))?;
            journal.set_len(valid as u64)?;
            if manifest.durable() { durability::flush(&journal)?; }
        }
        let mut engine = Self { manifest, state: state.into(), records: active.into_values().collect(), poisoned: false, pending_flush: None, dirty_namespaces: BTreeSet::new() };
        engine.manifest.next_id = next_id;
        let accepted = engine.cursor_for(Some(&engine.manifest.accepted))?;
        engine.records.retain(|r| r.id > accepted);
        if let Some(record) = pending {
            if record.id > accepted { engine.undo_at(&record.undo, engine.manifest.live())?; }
            engine.append(&Event::Undone(record.id))?;
        }
        if let Some(target) = engine.manifest.rollback { engine.rollback_to(target)?; }
        engine.save()?;
        engine.prune_preimages()?;
        Ok(engine)
    }

    pub fn save(&self) -> Result<()> { write_json_durable(&self.state.join("session.json"), &self.manifest, self.manifest.durable()) }
    pub(super) fn fail(&mut self) { self.poisoned = true; }
    pub(super) fn requires_recovery(&self) -> bool { self.poisoned }
    pub(super) fn boundary(&mut self) -> Result<()> {
        if self.poisoned { return Err(Error::Backend("Windows filesystem requires journal recovery".into())); }
        if self.manifest.durable() {
            for path in &self.dirty_namespaces {
                if let Err(error) = durability::parents(path, self.manifest.live()) { self.poisoned = true; return Err(error); }
            }
            self.dirty_namespaces.clear();
        }
        Ok(())
    }
    pub fn cursor(&self) -> u64 { self.records.last().map(|r| r.id).unwrap_or_else(|| self.manifest.checkpoints.iter().find(|p| p.info.id == self.manifest.accepted).map(|p| p.cursor).unwrap_or(0)) }
    pub fn cursor_for(&self, id: Option<&str>) -> Result<u64> {
        let id = id.unwrap_or(&self.manifest.accepted);
        self.manifest.checkpoints.iter().find(|p| p.info.id == id).map(|p| p.cursor).ok_or_else(|| Error::CheckpointNotFound(id.into()))
    }
    fn append(&self, event: &Event) -> Result<()> {
        let event = serde_json::to_string(event)?;
        let envelope = Envelope { sha256: format!("{:x}", Sha256::digest(event.as_bytes())), event };
        let mut f = OpenOptions::new().append(true).open(self.state.join("journal.jsonl"))?;
        serde_json::to_writer(&mut f, &envelope)?;
        f.write_all(b"\n")?;
        if self.manifest.durable() { durability::flush(&f)?; } else { f.sync_all()?; }
        Ok(())
    }

    pub fn path(&self, relative: &str, write: bool) -> Result<PathBuf> {
        let relative = relative.trim_start_matches(['/', '\\']);
        let path = Path::new(relative);
        if path.components().any(|c| !matches!(c, Component::Normal(_))) || relative.contains(':')
            || relative.split(['\\','/']).any(|c|c.ends_with(['.',' '])) {
            return Err(Error::Backend("invalid path in Windows filesystem view".into()));
        }
        self.check_policy_name(relative,write)?;
        let mut target = self.manifest.live().to_path_buf();
        for c in path.components() {
            target.push(c);
            match fs::symlink_metadata(&target) {
                Ok(m) if m.file_attributes() & 0x400 != 0 => return Err(Error::Unsupported("reparse points in the Windows filesystem view".into())),
                Ok(_) => {
                    // NTFS 8.3 aliases must not bypass secret or .git rules.
                    let actual=target.canonicalize()?;
                    let actual=actual.strip_prefix(self.manifest.live()).map_err(|_|Error::Backend("path escaped backing project".into()))?;
                    self.check_policy_name(&actual.to_string_lossy(),write)?;
                },
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(target)
    }

    fn check_policy_name(&self,relative:&str,write:bool)->Result<()> {
        let normalized=relative.replace('\\',"/").to_lowercase();
        if self.manifest.policy.filesystem.deny.iter().any(|p| crate::policy::deny_matches(&p.replace('\\', "/").to_lowercase(), &normalized))
            || normalized.split('/').any(|c| c == ".deltabox")
            || (write && normalized.split('/').any(|c| c == ".git")) {
            return Err(Error::Backend("path denied by filesystem policy".into()));
        }
        Ok(())
    }

    fn snapshot(&self, path: &Path, id: u64, suffix: &str) -> Result<(Snapshot, u64)> {
        let m = match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Snapshot::Missing, 0)),
            other => other?,
        };
        if m.is_dir() {
            if fs::read_dir(path)?.next().is_some() { return Err(Error::Backend("directory is not empty".into())); }
            Ok((Snapshot::Directory { metadata: Metadata::read(path)? }, 0))
        } else {
            let blob = format!("{id}-{suffix}.bin");
            self.check_quota(m.len())?;
            // The blob stores bytes; original attributes live in the snapshot.
            // Keeping blobs writable also permits reclaiming READONLY preimages.
            let mut input = File::open(path)?;
            let mut output = OpenOptions::new().write(true).create_new(true).open(self.state.join("preimages").join(&blob))?;
            let copied = std::io::copy(&mut input, &mut output)?;
            if copied != m.len() { return Err(Error::Backend("source changed while preserving undo bytes".into())); }
            if self.manifest.durable() { durability::flush(&output)?; } else { output.sync_all()?; }
            Ok((Snapshot::File { metadata: Metadata::read(path)?, blob }, m.len()))
        }
    }

    pub fn prepare(&mut self, op: u32, relative: &str, second: &str, offset: u64, length: u64) -> Result<Record> {
        if self.poisoned { return Err(Error::Backend("Windows filesystem requires journal recovery".into())); }
        let path = self.path(relative, true)?;
        if relative.trim_matches(['/', '\\']).is_empty() { return Err(Error::Backend("cannot mutate the view root".into())); }
        let id = self.manifest.next_id;
        self.manifest.next_id += 1;
        let mut touched = vec![relative.into()];
        let (undo, bytes) = match op {
            CREATE => {
                if path.exists() { return Err(std::io::Error::from_raw_os_error(80).into()); }
                (Undo::Remove { path: relative.into() }, 0)
            },
            WRITE | RESIZE => {
                let old_size = fs::metadata(&path)?.len();
                let (start, end) = if op == RESIZE { (offset.min(old_size), old_size) } else { (offset.min(old_size), offset.checked_add(length).ok_or_else(|| Error::Backend("write overflow".into()))?.min(old_size)) };
                // Each operation preserves its own preimage so incomplete READY
                // recovery can restore the immediately preceding state.
                let uncovered = vec![(start,end)];
                self.check_quota(end.saturating_sub(start))?;
                let mut segments = Vec::new();
                let mut f = File::open(&path)?;
                let mut bytes = 0;
                for (index, (s, e)) in uncovered.into_iter().filter(|(s,e)| s < e).enumerate() {
                    let blob = format!("{id}-{index}.bin");
                    let mut out = OpenOptions::new().write(true).create_new(true).open(self.state.join("preimages").join(&blob))?;
                    f.seek(SeekFrom::Start(s))?;
                    let copied = std::io::copy(&mut (&mut f).take(e-s), &mut out)?;
                    if copied != e-s { return Err(Error::Backend("source changed while preserving undo bytes".into())); }
                    if self.manifest.durable() { durability::flush(&out)?; } else { out.sync_all()?; }
                    segments.push(Segment { offset: s, length: e-s, blob });
                    bytes += e-s;
                }
                (Undo::Range { path: relative.into(), old_size, metadata: Metadata::read(&path)?, segments }, bytes)
            }
            DELETE => {
                let (snapshot, bytes) = self.snapshot(&path, id, "deleted")?;
                (Undo::Restore { path: relative.into(), snapshot }, bytes)
            }
            RENAME => {
                let target = self.path(second, true)?;
                if path.is_dir() {
                    for entry in walkdir::WalkDir::new(&path) {
                        let entry=entry.map_err(|e| Error::Backend(e.to_string()))?;
                        let suffix=entry.path().strip_prefix(&path).unwrap();
                        for relative in [Path::new(relative).join(suffix),Path::new(second).join(suffix)] {
                            self.path(relative.to_str().ok_or_else(||Error::Unsupported("non-Unicode path".into()))?,true)?;
                        }
                    }
                }
                let (replaced, bytes) = self.snapshot(&target, id, "replaced")?;
                touched.push(second.into());
                (Undo::Rename { old: relative.into(), new: second.into(), identity: identity(&path)?, replaced, source_metadata: Some(Metadata::read(&path)?) }, bytes)
            }
            METADATA => (Undo::Metadata { path: relative.into(), metadata: Metadata::read(&path)? }, 0),
            _ => return Err(Error::Unsupported("unknown Windows filesystem mutation".into())),
        };
        let used: u64 = self.records.iter().map(|r| r.bytes).sum();
        if used.saturating_add(bytes) > self.manifest.policy.limits.max_storage_bytes.unwrap_or(1024*1024*1024) {
            return Err(Error::Backend("Windows undo storage quota exceeded".into()));
        }
        let record = Record { id, touched, undo, bytes };
        self.prepare_record(&record)?;
        Ok(record)
    }

    fn prepare_record(&mut self, record: &Record) -> Result<()> {
        if self.manifest.durable() {
            let path = self.manifest.live().join(&record.touched[0]);
            self.pending_flush = match fs::metadata(&path) {
                Ok(m) if m.file_attributes() & 1 == 0 || m.is_dir() => Some(durability::open(&path)?),
                Ok(_) => None, // Clearing READONLY can acquire a handle after the action.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            durability::directory(&self.state.join("preimages"))?;
            durability::point("undo_barrier")?;
        }
        // An uncertain append must prevent any further mutation or pruning.
        self.poisoned = true;
        self.append(&Event::Ready(record.clone()))?;
        if self.manifest.durable() { durability::point("ready_barrier")?; }
        if self.manifest.durable() && self.pending_flush.is_none() {
            let path = self.manifest.live().join(&record.touched[0]);
            if let Ok(m) = fs::metadata(&path) {
                if m.file_attributes() & 1 != 0 {
                    // READY now protects this temporary attribute change. Keep
                    // the writable handle through the action and final flush.
                    let metadata = Metadata::read(&path)?;
                    let attrs = m.file_attributes() & !1;
                    if unsafe { SetFileAttributesW(wide(&path).as_ptr(), if attrs == 0 {0x80} else {attrs}) } == 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let handle = durability::open(&path)?;
                    metadata.restore(&path)?;
                    durability::flush(&handle)?;
                    self.pending_flush = Some(handle);
                }
            }
        }
        Ok(())
    }

    pub fn finish(&mut self, record: Record, status: i32) -> Result<i32> {
        self.finish_native(record, status, std::ptr::null_mut())
    }

    pub(super) fn finish_native(&mut self, record: Record, status: i32, native: windows_sys::Win32::Foundation::HANDLE) -> Result<i32> {
        let pending = self.pending_flush.take();
        if status < 0 {
            drop(pending);
            self.undo_at(&record.undo, self.manifest.live())?;
            self.append(&Event::Undone(record.id))?;
        } else {
            if self.manifest.durable() {
                if let Some(handle) = &pending { durability::flush(handle)?; }
                else {
                    match durability::open(&self.manifest.live().join(&record.touched[0])) {
                        Ok(handle) => durability::flush(&handle)?,
                        // A native create can install READONLY immediately. Its
                        // creation handle remains writable until this returns.
                        Err(_) if !native.is_null() => durability::flush_handle(native)?,
                        Err(error) => return Err(error),
                    }
                }
                drop(pending);
                for path in &record.touched {
                    let path = self.manifest.live().join(path);
                    durability::parents(&path, self.manifest.live())?;
                    self.dirty_namespaces.insert(path);
                }
                durability::point("mutation_barrier")?;
            }
            self.append(&Event::Applied(record.id))?;
            self.records.push(record);
            if self.manifest.durable() { durability::point("applied_barrier")?; }
        }
        self.poisoned = false;
        Ok(status)
    }

    fn restore_snapshot(&self, snapshot: &Snapshot, path: &Path) -> Result<()> {
        match snapshot {
            Snapshot::Missing => remove(path),
            Snapshot::Directory { metadata } => { durability::create_directories(path, self.manifest.durable())?; metadata.restore_durable(path, self.manifest.durable()) }
            Snapshot::File { metadata, blob } => {
                durability::create_directories(path.parent().unwrap(), self.manifest.durable())?;
                if path.exists() { remove(path)?; }
                let mut src = File::open(self.state.join("preimages").join(blob))?;
                let mut dst = OpenOptions::new().write(true).create(true).truncate(true).share_mode(7).open(path)?;
                std::io::copy(&mut src, &mut dst)?;
                dst.sync_all()?;
                drop(dst);
                metadata.restore_durable(path, self.manifest.durable())
            }
        }
    }

    fn undo_at(&self, undo: &Undo, root: &Path) -> Result<()> {
        self.undo_inner(undo, root)?;
        if self.manifest.durable() {
            let paths = match undo {
                Undo::Rename {old,new,..} => vec![old, new],
                Undo::Remove {path} | Undo::Restore {path,..} | Undo::Metadata {path,..} | Undo::Range {path,..} => vec![path],
            };
            for path in paths { durability::parents(&root.join(path), root)?; }
            if root == self.manifest.live() { durability::point("rollback_barrier")?; }
        }
        Ok(())
    }

    fn undo_inner(&self, undo: &Undo, root: &Path) -> Result<()> {
        match undo {
            Undo::Remove { path } => remove(&root.join(path)),
            Undo::Restore { path, snapshot } => self.restore_snapshot(snapshot, &root.join(path)),
            Undo::Metadata { path, metadata } => metadata.restore_durable(&root.join(path), self.manifest.durable()),
            Undo::Range { path, old_size, metadata, segments } => {
                let target = root.join(path);
                // Undo owns this temporary attribute change, including a failed
                // write whose original file was already READONLY.
                if self.manifest.durable() {
                    let attributes = fs::metadata(&target)?.file_attributes();
                    if attributes & 1 != 0 && unsafe { SetFileAttributesW(wide(&target).as_ptr(), if attributes & !1 == 0 {0x80} else {attributes & !1}) } == 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                let mut f = read_write(&target)?;
                f.set_len(*old_size)?;
                for segment in segments {
                    f.seek(SeekFrom::Start(segment.offset))?;
                    let mut src = File::open(self.state.join("preimages").join(&segment.blob))?;
                    if std::io::copy(&mut src, &mut f)? != segment.length { return Err(Error::Backend("short undo preimage".into())); }
                }
                f.sync_all()?;
                metadata.restore(&target)?;
                if self.manifest.durable() { durability::flush(&f)?; }
                Ok(())
            }
            Undo::Rename { old, new, identity: expected, replaced, source_metadata } => {
                let (old, new) = (root.join(old), root.join(new));
                // Scratch objects have new file IDs; live recovery uses stable IDs for idempotence.
                let live = root == self.manifest.live();
                // Undoing a later delete reconstructs the renamed object with a
                // new NTFS ID. Private LIVE has no external writers: absence of
                // old still proves that this move-back has not happened yet.
                let private = self.manifest.fs_mode == FilesystemMode::CbcowPrivate;
                let move_back = new.exists() && (!live || identity(&new)? == *expected || (private && !old.exists()));
                if move_back {
                    let handle = if self.manifest.durable() {
                        // Recovery owns this metadata change; source_metadata in
                        // READY makes a crash during READONLY clearing repeatable.
                        let attributes = fs::metadata(&new)?.file_attributes();
                        if attributes & 1 != 0 && unsafe { SetFileAttributesW(wide(&new).as_ptr(), if attributes & !1 == 0 {0x80} else {attributes & !1}) } == 0 {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        Some(durability::open(&new)?)
                    } else { None };
                    if old.exists() { remove(&old)?; }
                    fs::create_dir_all(old.parent().unwrap())?;
                    fs::rename(&new, &old)?;
                    if let Some(metadata) = source_metadata { metadata.restore(&old)?; }
                    if let Some(handle) = handle { durability::flush(&handle)?; }
                } else if live && identity(&old)? != *expected && !private {
                    return Err(Error::Backend("rename undo identity no longer matches".into()));
                }
                if let Some(metadata) = source_metadata { metadata.restore_durable(&old, self.manifest.durable())?; }
                self.restore_snapshot(replaced, &new)
            }
        }
    }

    pub fn checkpoint(&mut self, label: Option<String>) -> Result<CheckpointInfo> {
        self.boundary()?;
        let info = CheckpointInfo { id: uuid::Uuid::new_v4().to_string(), label, created_at_ms: now_ms(), change_count: self.records.len(), committed: false };
        self.manifest.checkpoints.push(Savepoint { info: info.clone(), cursor: self.cursor() });
        if let Err(error) = self.save() { self.poisoned = true; return Err(error); }
        Ok(info)
    }

    pub fn commit(&mut self, target: Option<&str>) -> Result<(String, usize)> {
        if self.manifest.fs_mode == FilesystemMode::CbcowPrivate {
            return self.commit_private(target, false).map(|(id, applied, _)| (id, applied));
        }
        self.accept(target)
    }

    fn accept(&mut self, target: Option<&str>) -> Result<(String, usize)> {
        let target = match target { Some(id) => id.to_owned(), None => self.checkpoint(Some("accepted".into()))?.id };
        let cursor = self.cursor_for(Some(&target))?;
        let applied = self.diff(None, Some(&target))?.pending_changes;
        self.manifest.accepted = target.clone();
        self.manifest.checkpoints.retain(|p| p.cursor >= cursor);
        for p in &mut self.manifest.checkpoints { p.info.committed = p.info.id == target; }
        self.save()?;
        self.records.retain(|r| r.id > cursor);
        self.prune_preimages()?;
        Ok((target, applied))
    }

    pub fn commit_private(&mut self, target: Option<&str>, ours: bool) -> Result<(String, usize, u64)> {
        self.boundary()?;
        let target = match target { Some(id) => id.to_owned(), None => self.checkpoint(Some("accepted".into()))?.id };
        let cursor = self.cursor_for(Some(&target))?;
        let changes = self.diff(None, Some(&target))?.changes;
        let stage = self.state.join(format!("publish-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            self.materialize(cursor, &stage.join("tree"))?;
            let mut next = self.manifest.clone();
            next.accepted = target.clone();
            next.checkpoints.retain(|p| p.cursor >= cursor);
            for p in &mut next.checkpoints { p.info.committed = p.info.id == target; }
            next.review = None;
            let folded = super::private::publish(&self.state, &self.manifest, &next, &changes, &stage, ours, &|| Ok(()))?;
            self.manifest = next;
            self.records.retain(|r| r.id > cursor);
            self.prune_preimages()?;
            Ok((target, changes.iter().filter(|c| !super::private::is_draft(&c.path)).count(), folded))
        })();
        // A failed recovery must keep its backups and block further mutations.
        if result.is_ok() || matches!(result, Err(Error::ProjectConflict(_))) {
            let _ = remove(&stage);
        } else { self.poisoned = true; }
        result
    }

    /// Reconstruct a checkpoint without changing LIVE or the journal.
    fn materialize(&self, cursor: u64, root: &Path) -> Result<()> {
        if self.poisoned { return Err(Error::Backend("journal recovery required".into())); }
        fs::create_dir_all(root)?;
        let touched: BTreeSet<_> = self.records.iter().flat_map(|r| r.touched.iter().cloned()).collect();
        for path in &touched {
            if !touched.iter().any(|parent| parent != path && Path::new(path).starts_with(parent)) {
                copy_tree(&self.manifest.live().join(path), &root.join(path))?;
            }
        }
        for record in self.records.iter().rev().filter(|r| r.id > cursor) {
            self.undo_at(&record.undo, root)?;
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<crate::protocol::SyncResult> {
        if self.manifest.fs_mode != FilesystemMode::CbcowPrivate {
            return Err(Error::Unsupported("session.sync requires a cbcow-private session".into()));
        }
        if self.poisoned { return Err(Error::Backend("journal recovery required".into())); }
        let changes = self.diff(None, None)?.changes;
        let result = super::private::sync(self, &changes);
        if result.is_err() && self.manifest.durable() { self.poisoned = true; }
        result
    }

    /// Used for merge drafts, which must have the same rollback behavior as routed writes.
    pub(super) fn write_draft(&mut self, relative: &str, bytes: &[u8]) -> Result<bool> {
        let path = self.path(relative, true)?;
        if !path.parent().unwrap().is_dir() { return Ok(false); }
        let id = self.manifest.next_id;
        self.manifest.next_id += 1;
        let (snapshot, size) = self.snapshot(&path, id, "draft")?;
        let record = Record { id, touched: vec![relative.into()], undo: Undo::Restore { path: relative.into(), snapshot }, bytes: size };
        self.prepare_record(&record)?;
        let status = if fs::write(path, bytes).is_ok() { 0 } else { -1 };
        self.finish(record, status)?;
        if status < 0 { return Err(Error::Backend("could not materialize merge draft".into())); }
        Ok(true)
    }

    pub fn abort(&mut self, target: Option<&str>) -> Result<(String, usize)> {
        self.boundary()?;
        let target = target.unwrap_or(&self.manifest.accepted).to_owned();
        let cursor = self.cursor_for(Some(&target))?;
        let restored = self.diff(Some(&target), None)?.pending_changes;
        self.manifest.rollback = Some(cursor);
        self.poisoned = true;
        self.save()?;
        self.rollback_to(cursor)?;
        Ok((target, restored))
    }

    fn rollback_to(&mut self, cursor: u64) -> Result<()> {
        while self.records.last().is_some_and(|r| r.id > cursor) {
            let record = self.records.last().unwrap();
            self.undo_at(&record.undo, self.manifest.live())?;
            self.append(&Event::Undone(record.id))?;
            self.records.pop();
        }
        self.manifest.checkpoints.retain(|p| p.cursor <= cursor);
        self.manifest.review = None;
        self.manifest.rollback = None;
        self.save()?;
        self.prune_preimages()?;
        self.poisoned = false;
        Ok(())
    }

    fn prune_preimages(&self) -> Result<()> {
        let mut retained=BTreeSet::new();
        for record in &self.records {
            match &record.undo {
                Undo::Range {segments,..}=>retained.extend(segments.iter().map(|s|s.blob.as_str())),
                Undo::Restore {snapshot:Snapshot::File {blob,..},..} |
                Undo::Rename {replaced:Snapshot::File {blob,..},..}=>{retained.insert(blob.as_str());},
                _=>{}
            }
        }
        for entry in fs::read_dir(self.state.join("preimages"))? {
            let entry=entry?;
            if !retained.contains(entry.file_name().to_string_lossy().as_ref()) {fs::remove_file(entry.path())?;}
        }
        Ok(())
    }

    pub fn diff(&self, from: Option<&str>, to: Option<&str>) -> Result<DiffResult> {
        if self.poisoned { return Err(Error::Backend("journal recovery required".into())); }
        let from_cursor = self.cursor_for(from)?;
        let to_cursor = match to { Some(id) => self.cursor_for(Some(id))?, None => self.cursor() };
        let min = from_cursor.min(to_cursor);
        let touched: BTreeSet<_> = self.records.iter().filter(|r| r.id > min).flat_map(|r| r.touched.iter().cloned()).collect();
        let scratch = self.state.join(format!("diff-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&scratch)?;
        let result = (|| {
            let (old, new) = (scratch.join("old"), scratch.join("new"));
            for root in [&old, &new] {
                fs::create_dir_all(root)?;
                for path in &touched {
                    if !touched.iter().any(|parent| parent != path && Path::new(path).starts_with(parent)) {
                        copy_tree(&self.manifest.live().join(path), &root.join(path))?;
                    }
                }
            }
            for (root, cursor) in [(&old, from_cursor), (&new, to_cursor)] {
                for record in self.records.iter().rev().filter(|r| r.id > cursor) { self.undo_at(&record.undo, root)?; }
            }
            let metadata_paths: BTreeSet<_> = self.records.iter().filter_map(|r| match &r.undo {
                Undo::Metadata { path, .. } => Some(path.replace('/', "\\").to_lowercase()), _ => None,
            }).collect();
            let before = scan(&old, &metadata_paths)?;
            let after = scan(&new, &metadata_paths)?;
            let keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
            let mut changes = Vec::new();
            for path in keys {
                let (a,b) = (before.get(&path), after.get(&path));
                if a == b { continue; }
                let kind = if a.is_none() { ChangeKind::Added } else if b.is_none() { ChangeKind::Deleted } else { ChangeKind::Modified };
                let read = |root: &Path, entry: Option<&DiffEntry>| -> Option<Vec<u8>> {
                    entry.filter(|e| !e.0 && e.1 <= DEFAULT_TEXT_SIZE_LIMIT).and_then(|_| fs::read(root.join(&path)).ok())
                };
                let (ab,bb) = (read(&old,a),read(&new,b));
                changes.push(describe_change(&path.replace('\\',"/"),kind,a.map(|e|e.1),b.map(|e|e.1),ab.as_deref(),bb.as_deref(),DEFAULT_TEXT_SIZE_LIMIT));
            }
            Ok(DiffResult { from: from.map(str::to_owned).or_else(|| Some(self.manifest.accepted.clone())), to: to.map(str::to_owned), pending_changes: changes.len(), changes })
        })();
        let _ = remove(&scratch);
        result
    }

    pub fn preimage_bytes(&self) -> u64 { self.records.iter().map(|r| r.bytes).sum() }
    fn check_quota(&self, bytes: u64) -> Result<()> {
        if self.preimage_bytes().saturating_add(bytes)>self.manifest.policy.limits.max_storage_bytes.unwrap_or(1024*1024*1024) {
            return Err(Error::Backend("Windows undo storage quota exceeded".into()));
        }
        Ok(())
    }
}

type DiffEntry = (bool,u64,u32,String,u64,u64);

fn scan(root: &Path, metadata_paths: &BTreeSet<String>) -> Result<BTreeMap<String,DiffEntry>> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1) {
        let entry = entry.map_err(|e| Error::Backend(e.to_string()))?;
        let m = entry.metadata().map_err(|e| Error::Backend(e.to_string()))?;
        let path = entry.path().strip_prefix(root).unwrap().to_str().ok_or_else(|| Error::Unsupported("non-Unicode path".into()))?.to_owned();
        let hash = if m.is_file() {
            let mut f = File::open(entry.path())?;
            let mut hasher = Sha256::new();
            let mut buffer = [0;65536];
            loop { let n=f.read(&mut buffer)?; if n==0 {break;} hasher.update(&buffer[..n]); }
            format!("{:x}",hasher.finalize())
        } else { String::new() };
        let times = m.is_file() || metadata_paths.contains(&path.replace('/', "\\").to_lowercase());
        out.insert(path,(m.is_dir(), if m.is_file() { m.len() } else { 0 }, m.file_attributes(),hash,
            if times { m.creation_time() } else { 0 }, if times { m.last_write_time() } else { 0 }));
    }
    Ok(out)
}

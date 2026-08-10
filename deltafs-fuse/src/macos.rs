use std::collections::VecDeque;
use std::ffi::{c_char, c_double, c_void, CStr, CString};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const UTF8: u32 = 0x0800_0100;
const CREATE_NO_DEFER: u32 = 0x0000_0002;
const CREATE_WATCH_ROOT: u32 = 0x0000_0004;
const CREATE_FILE_EVENTS: u32 = 0x0000_0010;
const CREATE_FULL_HISTORY: u32 = 0x0000_0080;

pub const MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
pub const USER_DROPPED: u32 = 0x0000_0002;
pub const KERNEL_DROPPED: u32 = 0x0000_0004;
pub const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
pub const HISTORY_DONE: u32 = 0x0000_0010;
pub const ROOT_CHANGED: u32 = 0x0000_0020;
pub const ITEM_CREATED: u32 = 0x0000_0100;
pub const ITEM_REMOVED: u32 = 0x0000_0200;
pub const ITEM_RENAMED: u32 = 0x0000_0800;
pub const ITEM_IS_HARDLINK: u32 = 0x0010_0000;
pub const ITEM_CLONED: u32 = 0x0040_0000;

type StreamRef = *mut c_void;
type DispatchQueue = *mut c_void;
type CfString = *const c_void;
type CfArray = *const c_void;
type CfUuid = *const c_void;

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: Option<unsafe extern "C" fn(*const c_void) -> CfString>,
}

#[repr(C)]
struct UuidBytes {
    bytes: [u8; 16],
}

type StreamCallback =
    unsafe extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: StreamCallback,
        context: *mut StreamContext,
        paths: CfArray,
        since_when: u64,
        latency: c_double,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamFlushSync(stream: StreamRef);
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> CfUuid;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        value: *const c_char,
        encoding: u32,
    ) -> CfString;
    fn CFArrayCreate(
        allocator: *const c_void,
        values: *const *const c_void,
        count: isize,
        callbacks: *const c_void,
    ) -> CfArray;
    fn CFUUIDGetUUIDBytes(uuid: CfUuid) -> UuidBytes;
    fn CFRelease(value: *const c_void);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_release(object: *mut c_void);
}

unsafe extern "C" fn dispatch_barrier(_: *mut c_void) {}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Event {
    pub path: PathBuf,
    pub flags: u32,
    pub id: u64,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct EventBatch {
    pub events: Vec<Event>,
    pub latest_id: u64,
    pub history_done: bool,
    pub must_scan: bool,
    pub root_changed: bool,
}

impl EventBatch {
    fn push(&mut self, event: Event) {
        self.latest_id = self.latest_id.max(event.id);
        self.history_done |= event.flags & HISTORY_DONE != 0;
        self.must_scan |= event.flags
            & (MUST_SCAN_SUBDIRS | USER_DROPPED | KERNEL_DROPPED | EVENT_IDS_WRAPPED)
            != 0;
        self.root_changed |= event.flags & ROOT_CHANGED != 0;
        if event.flags & HISTORY_DONE == 0 {
            self.events.push(event);
        }
    }

    fn extend(&mut self, events: Vec<Event>) {
        for event in events {
            self.push(event);
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.history_done |= other.history_done;
        self.must_scan |= other.must_scan;
        self.root_changed |= other.root_changed;
        self.extend(other.events);
        self.latest_id = self.latest_id.max(other.latest_id);
    }
}

struct CallbackState {
    events: Mutex<VecDeque<Vec<Event>>>,
    ready: Condvar,
}

pub struct EventStream {
    stream: StreamRef,
    queue: DispatchQueue,
    callback: Box<CallbackState>,
    since: u64,
}

impl EventStream {
    pub fn start(paths: &[PathBuf], since: u64) -> std::io::Result<Self> {
        let strings: Vec<CString> = paths
            .iter()
            .map(|path| CString::new(path.as_os_str().as_encoded_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        let cf_strings: Vec<CfString> = strings
            .iter()
            .map(|path| unsafe { CFStringCreateWithCString(ptr::null(), path.as_ptr(), UTF8) })
            .collect();
        if cf_strings.iter().any(|value| value.is_null()) {
            for value in cf_strings.into_iter().filter(|value| !value.is_null()) {
                unsafe { CFRelease(value) };
            }
            return Err(std::io::Error::other("create FSEvents path"));
        }
        let path_array = unsafe {
            CFArrayCreate(
                ptr::null(),
                cf_strings.as_ptr().cast(),
                cf_strings.len() as isize,
                ptr::null(),
            )
        };
        if path_array.is_null() {
            for value in cf_strings {
                unsafe { CFRelease(value) };
            }
            return Err(std::io::Error::other("create FSEvents path array"));
        }

        let mut callback = Box::new(CallbackState {
            events: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        });
        let mut context = StreamContext {
            version: 0,
            info: (&mut *callback as *mut CallbackState).cast(),
            retain: None,
            release: None,
            copy_description: None,
        };
        let stream = unsafe {
            FSEventStreamCreate(
                ptr::null(),
                event_callback,
                &mut context,
                path_array,
                since,
                0.0,
                CREATE_NO_DEFER | CREATE_WATCH_ROOT | CREATE_FILE_EVENTS | CREATE_FULL_HISTORY,
            )
        };
        unsafe { CFRelease(path_array) };
        for value in cf_strings {
            unsafe { CFRelease(value) };
        }
        if stream.is_null() {
            return Err(std::io::Error::other("create FSEvents stream"));
        }
        let label = CString::new("deltafs.macos.events").expect("static dispatch label");
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), ptr::null()) };
        if queue.is_null() {
            unsafe { FSEventStreamRelease(stream) };
            return Err(std::io::Error::other("create FSEvents dispatch queue"));
        }
        unsafe {
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                return Err(std::io::Error::other("start FSEvents stream"));
            }
        }
        Ok(Self {
            stream,
            queue,
            callback,
            since,
        })
    }

    pub fn history(&self, timeout: Duration) -> std::io::Result<EventBatch> {
        let batch = self.collect_until(timeout, |event| event.flags & HISTORY_DONE != 0)?;
        let mut filtered = EventBatch {
            latest_id: self.since,
            history_done: true,
            must_scan: batch.must_scan,
            root_changed: batch.root_changed,
            ..EventBatch::default()
        };
        for event in batch.events {
            if event.id > self.since {
                filtered.push(event);
            }
        }
        Ok(filtered)
    }

    pub fn barrier(&self, path: &Path, timeout: Duration) -> std::io::Result<EventBatch> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, [])?;
        unsafe { FSEventStreamFlushSync(self.stream) };
        self.collect_until(timeout, |event| event.path == path)
    }

    pub fn flush(&self) -> EventBatch {
        unsafe {
            FSEventStreamFlushSync(self.stream);
            dispatch_sync_f(self.queue, ptr::null_mut(), dispatch_barrier);
        }
        let mut result = EventBatch::default();
        let mut pending = self.callback.events.lock().unwrap();
        while let Some(events) = pending.pop_front() {
            result.extend(events);
        }
        result
    }

    fn collect_until(
        &self,
        timeout: Duration,
        done: impl Fn(&Event) -> bool,
    ) -> std::io::Result<EventBatch> {
        let deadline = Instant::now() + timeout;
        let mut result = EventBatch::default();
        let mut pending = self.callback.events.lock().unwrap();
        loop {
            while let Some(events) = pending.pop_front() {
                let reached = events.iter().any(&done);
                result.extend(events);
                if reached {
                    return Ok(result);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "FSEvents boundary timed out",
                ));
            }
            let (next, wait) = self
                .callback
                .ready
                .wait_timeout(pending, remaining)
                .unwrap();
            pending = next;
            if wait.timed_out() && pending.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "FSEvents boundary timed out",
                ));
            }
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamFlushSync(self.stream);
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
        }
        let _ = &self.callback;
    }
}

pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

pub fn run_event_helper(
    socket: &Path,
    live: &Path,
    since: u64,
    timeout: Duration,
) -> std::io::Result<()> {
    let stream = EventStream::start(&[live.to_path_buf()], since)?;
    let mut pending = stream.history(timeout)?;
    pending.merge(stream.flush());
    if socket.exists() {
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    loop {
        let (mut client, _) = listener.accept()?;
        let mut request = String::new();
        BufReader::new(client.try_clone()?).read_line(&mut request)?;
        match request.trim() {
            "boundary" => {
                pending.merge(stream.flush());
                serde_json::to_writer(&mut client, &pending)?;
                client.write_all(b"\n")?;
                pending = EventBatch::default();
            }
            "barrier" => {
                pending.merge(stream.barrier(&live.join(".deltabox/native-barrier"), timeout)?);
                serde_json::to_writer(&mut client, &pending)?;
                client.write_all(b"\n")?;
                pending = EventBatch::default();
            }
            "stop" => break,
            _ => return Err(std::io::Error::other("invalid event helper request")),
        }
    }
    fs::remove_file(socket)
}

pub fn event_helper_boundary(socket: &Path) -> std::io::Result<EventBatch> {
    event_helper_request(socket, "boundary")
}

pub fn event_helper_barrier(socket: &Path) -> std::io::Result<EventBatch> {
    event_helper_request(socket, "barrier")
}

fn event_helper_request(socket: &Path, request: &str) -> std::io::Result<EventBatch> {
    let mut stream = UnixStream::connect(socket)?;
    writeln!(stream, "{request}")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    serde_json::from_str(&response).map_err(std::io::Error::other)
}

pub fn stop_event_helper(socket: &Path) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(b"stop\n")
}

pub fn device_uuid(path: &Path) -> std::io::Result<String> {
    let device = fs::metadata(path)?.dev() as libc::dev_t;
    let uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if uuid.is_null() {
        return Err(std::io::Error::other("read FSEvents device UUID"));
    }
    let bytes = unsafe { CFUUIDGetUUIDBytes(uuid).bytes };
    unsafe { CFRelease(uuid) };
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

unsafe extern "C" fn event_callback(
    _stream: StreamRef,
    info: *mut c_void,
    count: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
) {
    let state = unsafe { &*(info.cast::<CallbackState>()) };
    let paths = event_paths.cast::<*const c_char>();
    let mut events = Vec::with_capacity(count);
    for index in 0..count {
        let path = unsafe { CStr::from_ptr(*paths.add(index)) };
        events.push(Event {
            path: PathBuf::from(String::from_utf8_lossy(path.to_bytes()).into_owned()),
            flags: unsafe { *event_flags.add(index) },
            id: unsafe { *event_ids.add(index) },
        });
    }
    let mut pending = state.events.lock().unwrap();
    pending.push_back(events);
    state.ready.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_promotes_loss_and_root_flags() {
        let mut batch = EventBatch::default();
        batch.push(Event {
            path: "/tmp/root".into(),
            flags: MUST_SCAN_SUBDIRS | KERNEL_DROPPED | ROOT_CHANGED,
            id: 7,
        });
        assert!(batch.must_scan);
        assert!(batch.root_changed);
        assert_eq!(batch.latest_id, 7);
    }
}

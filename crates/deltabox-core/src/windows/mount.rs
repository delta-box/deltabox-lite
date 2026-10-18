use super::engine::{wide, Engine};
use crate::{Error, Result};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

type Action = unsafe extern "C" fn(*mut c_void) -> i32;
extern "C" {
    fn db_mount(source: *const u16, view: *const u16, prefix: *const u16, engine: *mut c_void, strict: i32, mount: *mut *mut c_void) -> i32;
    fn db_unmount(mount: *mut c_void);
    fn db_writers(mount: *mut c_void) -> i32;
    fn db_mountpoint(mount: *mut c_void) -> *const u16;
    fn db_status_from_win32(error: u32) -> i32;
}

pub struct Mount {
    raw: *mut c_void,
    _engine: Arc<Mutex<Engine>>,
}

impl Mount {
    pub fn new(engine: Arc<Mutex<Engine>>) -> Result<Self> {
        let e = engine.lock().map_err(|_| Error::Backend("filesystem mutex poisoned".into()))?;
        let (source, view) = (wide(e.manifest.live()), wide(&e.manifest.view));
        let prefix=wide(std::path::Path::new(&format!("\\deltabox\\{}",e.manifest.session_id)));
        let strict=i32::from(e.manifest.durable());
        drop(e);
        let mut raw = std::ptr::null_mut();
        let status = unsafe { db_mount(source.as_ptr(), view.as_ptr(), prefix.as_ptr(), Arc::as_ptr(&engine) as *mut c_void, strict, &mut raw) };
        if status < 0 { return Err(Error::FilesystemUnavailable(format!("WinFsp mount failed (NTSTATUS 0x{:08x})", status as u32))); }
        let mounted=Self { raw, _engine: engine.clone() };
        let point=unsafe {db_mountpoint(raw)};
        let mut length=0;
        unsafe {while *point.add(length)!=0 {length+=1;}}
        let point=String::from_utf16_lossy(unsafe {std::slice::from_raw_parts(point,length)});
        let mut e=engine.lock().map_err(|_|Error::Backend("filesystem mutex poisoned".into()))?;
        e.manifest.view=std::path::PathBuf::from(format!("{point}\\"));
        e.save()?;
        Ok(mounted)
    }

    pub fn boundary(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { db_writers(self.raw) } != 0 {
            if std::time::Instant::now() >= deadline { return Err(Error::SessionBusy("open writable handles in the WinFsp view".into())); }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self._engine.lock().map_err(|_| Error::Backend("filesystem mutex poisoned".into()))?.boundary()
    }
}

impl Drop for Mount { fn drop(&mut self) { unsafe { db_unmount(self.raw) }; } }

fn name(ptr: *const u16) -> Result<String> {
    if ptr.is_null() { return Ok(String::new()); }
    let mut length = 0;
    unsafe { while *ptr.add(length) != 0 { length += 1; if length > 32767 { return Err(Error::Backend("path exceeds Windows limit".into())); } } }
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(ptr,length) }).map_err(|_| Error::Unsupported("non-Unicode Windows path".into()))?;
    Ok(value.trim_start_matches(['\\','/']).replace('/',"\\"))
}

fn status(error: Error) -> i32 {
    match error {
        Error::Io(error) => unsafe { db_status_from_win32(error.raw_os_error().unwrap_or(1117) as u32) },
        Error::Unsupported(_) => 0xc00000bbu32 as i32,
        _ => 0xc0000022u32 as i32,
    }
}

#[no_mangle]
unsafe extern "C" fn db_check(context: *mut c_void, path: *const u16, write: i32) -> i32 {
    std::panic::catch_unwind(|| {
        let path=name(path)?;
        let engine=&*(context as *const Mutex<Engine>);
        engine.lock().map_err(|_| Error::Backend("filesystem mutex poisoned".into()))?.path(&path,write != 0)?;
        Ok::<_,Error>(0)
    }).unwrap_or_else(|_| Err(Error::Backend("filesystem callback panicked".into()))).unwrap_or_else(status)
}

#[no_mangle]
unsafe extern "C" fn db_mutate(context: *mut c_void, op: u32, path: *const u16, second: *const u16,
    offset: u64, length: u64, action: Action, opaque: *mut c_void, handle: *const windows_sys::Win32::Foundation::HANDLE) -> i32 {
    std::panic::catch_unwind(|| {
        let (path,second)=(name(path)?,name(second)?);
        let engine=&*(context as *const Mutex<Engine>);
        let mut engine=engine.lock().map_err(|_| Error::Backend("filesystem mutex poisoned".into()))?;
        let record=engine.prepare(op,&path,&second,offset,length)?;
        let result=action(opaque);
        engine.finish_native(record,result,if handle.is_null() {std::ptr::null_mut()} else {*handle})
    }).unwrap_or_else(|_| Err(Error::Backend("filesystem callback panicked".into()))).unwrap_or_else(status)
}

/// Close/flush callbacks can lack an error return channel. Retain their failure
/// so the next command boundary cannot report success or reclaim undo records.
#[no_mangle]
unsafe extern "C" fn db_io_failed(context: *mut c_void) {
    let _=std::panic::catch_unwind(|| {
        if let Ok(mut engine)=(*(context as *const Mutex<Engine>)).lock() {engine.fail();}
    });
}

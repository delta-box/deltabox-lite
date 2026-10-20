use crate::protocol::*;
use crate::windows::engine::{now_ms,write_json,Engine,Manifest,Savepoint};
use crate::windows::helper;
use crate::{Error, Result};
use base64::Engine as _;
use serde::de::DeserializeOwned;
use sha2::{Digest,Sha256};
use std::fs;
use std::path::{Path,PathBuf};
use std::time::{Duration,Instant};

#[derive(Debug,Clone,Copy,Default,PartialEq,Eq)]
pub enum CommandExecutionMode { #[default] Sandboxed, Trusted }
#[derive(Debug,Clone,Default)]
pub struct CoordinatorOptions {
    pub helper_bin: Option<PathBuf>, pub command_execution: CommandExecutionMode,
    pub state_dir: Option<PathBuf>,pub runtime_dir: Option<PathBuf>,pub embedded_fast: bool,
}

pub struct SessionCoordinator { project: PathBuf,state: PathBuf,runtime: PathBuf,helper_bin: PathBuf,command_execution: CommandExecutionMode,created_state_dirs: Vec<PathBuf> }

impl SessionCoordinator {
    pub fn new(project: &Path,worker_bin: Option<PathBuf>) -> Result<Self> {
        Self::new_with_options(project,CoordinatorOptions {helper_bin:worker_bin,..Default::default()})
    }
    pub fn new_with_options(project: &Path,options: CoordinatorOptions) -> Result<Self> {
        let project=project.canonicalize()?;
        if !project.is_dir() { return Err(Error::Backend("project must be a directory".into())); }
        let local=std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or_else(||Error::Backend("LOCALAPPDATA unavailable".into()))?.join("deltabox-lite");
        let base=options.state_dir.or_else(||std::env::var_os("DELTABOX_STATE_DIR").map(PathBuf::from)).unwrap_or_else(||local.join("state"));
        let runtime=options.runtime_dir.or_else(||std::env::var_os("DELTABOX_RUNTIME_DIR").map(PathBuf::from)).unwrap_or_else(||local.join("runtime"));
        let mut created_state_dirs=crate::windows::durability::missing_directories(&base);
        fs::create_dir_all(&base)?;fs::create_dir_all(&runtime)?;
        let (base,runtime)=(base.canonicalize()?,runtime.canonicalize()?);
        let inside=|a:&Path,b:&Path| { let (a,b)=(a.to_string_lossy().to_lowercase(),b.to_string_lossy().to_lowercase());a==b||a.starts_with(&(b+"\\")) };
        if inside(&base,&project)||inside(&runtime,&project)||inside(&runtime,&base) {
            return Err(Error::Backend("Windows state and runtime must be outside the project; runtime must also be outside state".into()));
        }
        let key=format!("{:x}",Sha256::digest(project.to_string_lossy().to_lowercase().as_bytes()));
        let state=base.join("projects").join(&key[..32]);created_state_dirs.extend(crate::windows::durability::missing_directories(&state));fs::create_dir_all(&state)?;
        Ok(Self { project,state,runtime,helper_bin:options.helper_bin.unwrap_or(std::env::current_exe()?),command_execution:options.command_execution,created_state_dirs })
    }
    fn store(&self,id:&str)->Result<PathBuf> {
        uuid::Uuid::parse_str(id).map_err(|_|Error::SessionNotFound(id.into()))?;
        Ok(self.state.join(id))
    }
    fn manifest(&self,id:&str)->Result<Manifest> {
        let m:Manifest=serde_json::from_slice(&fs::read(self.store(id)?.join("session.json")).map_err(|e|if e.kind()==std::io::ErrorKind::NotFound {Error::SessionNotFound(id.into())} else {e.into()})?)?;
        if m.source!=self.project || m.session_id!=id {return Err(Error::Backend("Windows session identity mismatch".into()));}
        Ok(m)
    }
    pub fn start(&self,policy:Option<&Path>,requested:Option<FilesystemMode>,required:bool)->Result<SessionInfo> {
        self.start_with_durability(policy,requested,required,DurabilityMode::Fast,None)
    }
    pub fn start_with_durability(&self,policy:Option<&Path>,requested:Option<FilesystemMode>,_required:bool,durability:DurabilityMode,group:Option<u64>)->Result<SessionInfo> {
        let mode = requested.unwrap_or(FilesystemMode::CbcowThrough);
        match mode {
            FilesystemMode::CopyPrivate=>return Err(Error::Unsupported("Windows copy-private provider is reserved and not implemented".into())),
            FilesystemMode::CbcowThrough | FilesystemMode::CbcowPrivate=>{}
        }
        if group.is_some() {return Err(Error::Unsupported("Windows group commit is not implemented".into()));}
        if durability==DurabilityMode::Strict {
            if mode!=FilesystemMode::CbcowPrivate {return Err(Error::Unsupported("Windows strict durability requires cbcow-private".into()));}
            crate::windows::durability::qualify(&self.project)?;
            crate::windows::durability::qualify(&self.state)?;
            crate::windows::durability::persist_directories(&self.created_state_dirs)?;
        }
        if self.command_execution == CommandExecutionMode::Sandboxed {
            crate::windows::sandbox::available()?;
        }
        let _admission=helper::lock(&self.state.join("project.lock"),Duration::from_secs(30))?;
        let active=self.state.join("active.json");
        if active.exists() {
            let id:String=serde_json::from_slice(&fs::read(&active)?)?;
            if self.manifest(&id)?.status==SessionStatus::Stopped {fs::remove_file(&active)?;}
            else {return Err(Error::SessionBusy(format!("through session {id} owns this project; stop or recover it first")));}
        }
        // Private sessions share admission; a through writer remains exclusive.
        if mode == FilesystemMode::CbcowThrough {
            for entry in fs::read_dir(&self.state)? {
                let entry = entry?;
                if !entry.path().join("session.json").is_file() { continue; }
                let m: Manifest = serde_json::from_slice(&fs::read(entry.path().join("session.json"))?)?;
                if m.status != SessionStatus::Stopped {
                    return Err(Error::SessionBusy(format!("session {} owns this project; stop it first", m.session_id)));
                }
            }
        }
        let id=uuid::Uuid::new_v4().to_string();let checkpoint=uuid::Uuid::new_v4().to_string();
        let store=self.store(&id)?;
        let views=self.runtime.join("views");let scratch=self.runtime.join("scratch").join(&id);
        fs::create_dir_all(&views)?;fs::create_dir_all(&scratch)?;
        let manifest=Manifest {
            version:1,session_id:id.clone(),source:self.project.clone(),fs_mode:mode,trusted:self.command_execution==CommandExecutionMode::Trusted,durability,live:None,view:views.join(&id),scratch,
            token:format!("{}{}",uuid::Uuid::new_v4(),uuid::Uuid::new_v4()),started_at_ms:now_ms(),status:SessionStatus::Running,
            policy:match policy {Some(path)=>crate::policy::Policy::load(path)?,None=>crate::policy::Policy::default_agent()},
            next_id:1,accepted:checkpoint.clone(),checkpoints:vec![Savepoint {info:CheckpointInfo {id:checkpoint,label:Some("initial".into()),created_at_ms:now_ms(),change_count:0,committed:true},cursor:0}],review:None,rollback:None,
        };
        Engine::create(&store,manifest)?;
        if mode == FilesystemMode::CbcowThrough { write_json(&active,&id)?; }
        let result=self.status(&id);
        if result.is_err() {
            // No command has been dispatched, so a failed mount owns no host mutations.
            if mode == FilesystemMode::CbcowThrough { let _=fs::remove_file(&active); }
            let mut failed = self.manifest(&id)?;
            failed.status = SessionStatus::Stopped;
            crate::windows::engine::write_json_durable(&store.join("session.json"), &failed, failed.durable())?;
        }
        result
    }

    fn ensure_helper(&self,id:&str)->Result<()> {
        let store=self.store(id)?;
        let manifest=self.manifest(id)?;
        if manifest.status==SessionStatus::Stopped {return Err(Error::SessionStopped(id.into()));}
        let _startup=helper::lock(&store.join("startup.lock"),Duration::from_secs(30))?;
        match helper::lock(&store.join("helper.lock"),Duration::ZERO) {
            Err(Error::Io(e)) if matches!(e.raw_os_error(),Some(32|33)) => return Ok(()),
            Err(e)=>return Err(e),Ok(lease)=>drop(lease),
        }
        let _=fs::remove_file(store.join("ready.json"));
        let child=spawn_helper(&self.helper_bin,&store)?;
        let deadline=Instant::now()+Duration::from_secs(30);
        while Instant::now()<deadline {
            if store.join("ready.json").exists() {return Ok(());}
            let mut exit=0;
            if unsafe { windows_sys::Win32::System::Threading::GetExitCodeProcess(child.0,&mut exit) } == 0 {return Err(std::io::Error::last_os_error().into());}
            if exit!=259 {return Err(Error::FilesystemUnavailable(format!("Windows filesystem helper exited with {exit}; see {}",store.join("helper.log").display())));}
            std::thread::sleep(Duration::from_millis(20));
        }
        // The helper owns no workload at startup; do not leave a late mount after a failed start.
        unsafe { windows_sys::Win32::System::Threading::TerminateProcess(child.0,1); }
        Err(Error::FilesystemUnavailable("Windows filesystem helper startup timed out".into()))
    }
    fn call<T:DeserializeOwned>(&self,id:&str,method:Method)->Result<T> {
        self.manifest(id)?;
        let store=self.store(id)?;
        let _operation=helper::lock(&store.join("operation.lock"),Duration::from_secs(660))?;
        self.ensure_helper(id)?;
        serde_json::from_value(helper::request(&store,method)?).map_err(Into::into)
    }
    pub fn status(&self,id:&str)->Result<SessionInfo> {
        let m=self.manifest(id)?;
        if m.status==SessionStatus::Stopped {return helper::session_info(&Engine::open(&self.store(id)?)?,None);}
        self.call(id,Method::SessionStatus(SessionStatusParams {session_id:id.into()}))
    }
    pub fn stop(&self,id:&str)->Result<SessionInfo> {
        let info=if self.manifest(id)?.status==SessionStatus::Stopped {self.status(id)?} else {self.call(id,Method::SessionStop(SessionStopParams {session_id:id.into()}))?};
        let _admission=helper::lock(&self.state.join("project.lock"),Duration::from_secs(30))?;
        let active=self.state.join("active.json");
        if active.exists() {
            let owner:String=serde_json::from_slice(&fs::read(&active)?)?;
            if owner==id {fs::remove_file(active)?;}
        }
        Ok(info)
    }
    pub fn remove_stopped(&self,id:&str)->Result<()> {
        let m=self.manifest(id)?;
        if m.status!=SessionStatus::Stopped {return Err(Error::SessionBusy(id.into()));}
        let store=self.store(id)?;
        for _ in 0..19 {
            match crate::windows::engine::remove(&store) {
                Err(Error::Io(error)) if matches!(error.raw_os_error(),Some(32|145)) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
        crate::windows::engine::remove(&store)
    }
    pub fn exec(&self,p:ExecParams)->Result<ExecResult> {let id=p.session_id.clone();self.call(&id,Method::SessionExec(p))}
    pub fn read_file(&self,id:&str,path:&Path)->Result<FsReadResult> {self.call(id,Method::FsRead(FsReadParams {session_id:id.into(),path:path.into()}))}
    pub fn read_file_bytes(&self,id:&str,path:&Path)->Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD.decode(self.read_file(id,path)?.content_base64).map_err(|e|Error::Backend(e.to_string()))
    }
    pub fn write_file(&self,id:&str,path:&Path,content:Vec<u8>)->Result<()> {self.write_file_bytes(id,path,&content)}
    pub fn write_file_bytes(&self,id:&str,path:&Path,content:&[u8])->Result<()> {
        let _:serde_json::Value=self.call(id,Method::FsWrite(FsWriteParams {session_id:id.into(),path:path.into(),content_base64:base64::engine::general_purpose::STANDARD.encode(content)}))?;Ok(())
    }
    pub fn record_file_changes(&self,id:&str,paths:&[PathBuf])->Result<()> {
        let m=self.manifest(id)?;
        for path in paths {if path.is_absolute() && !path.starts_with(&m.view) {return Err(Error::Backend("direct writes must use the WinFsp view".into()));}}
        self.status(id)?;Ok(())
    }
    pub fn checkpoint(&self,p:CheckpointParams)->Result<CheckpointInfo> {let id=p.session_id.clone();self.call(&id,Method::CheckpointCreate(p))}
    pub fn diff(&self,p:DiffParams)->Result<DiffResult> {let id=p.session_id.clone();self.call(&id,Method::DiffList(p))}
    pub fn commit(&self,p:CommitParams)->Result<CommitResult> {let id=p.session_id.clone();self.call(&id,Method::ChangesCommit(p))}
    pub fn abort(&self,p:AbortParams)->Result<AbortResult> {let id=p.session_id.clone();self.call(&id,Method::ChangesAbort(p))}
    pub fn sync(&self,id:&str)->Result<SyncResult> {self.call(id,Method::SessionSync(SyncParams {session_id:id.into()}))}
    pub fn begin_review(&self,p:ReviewBeginParams)->Result<ActiveReview> {let id=p.session_id.clone();self.call(&id,Method::ReviewBegin(p))}
    pub fn review_status(&self,id:&str)->Result<Option<ActiveReview>> {self.call(id,Method::ReviewStatus(ReviewStatusParams {session_id:id.into()}))}
    pub fn review_diff(&self,id:&str)->Result<DiffResult> {self.call(id,Method::ReviewDiff(ReviewStatusParams {session_id:id.into()}))}
    pub fn review_commit(&self,id:&str)->Result<CommitResult> {self.call(id,Method::ReviewCommit(ReviewStatusParams {session_id:id.into()}))}
    pub fn review_abort(&self,id:&str)->Result<AbortResult> {self.call(id,Method::ReviewAbort(ReviewStatusParams {session_id:id.into()}))}
    pub fn review_skip(&self,id:&str)->Result<ActiveReview> {self.call(id,Method::ReviewSkip(ReviewStatusParams {session_id:id.into()}))}
}

struct ChildHandle(windows_sys::Win32::Foundation::HANDLE);
impl Drop for ChildHandle { fn drop(&mut self) { unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); } } }
fn spawn_helper(binary:&Path,state:&Path)->Result<ChildHandle> {
    use windows_sys::Win32::System::Threading::*;
    use wxc_common::cmdline::{cmdline_from_argv_for_context,CommandLineContext};
    let argv=vec![binary.to_string_lossy().into_owned(),"__windows-fs-helper".into(),"--state-dir".into(),state.to_string_lossy().into_owned()];
    let line=cmdline_from_argv_for_context(&argv,CommandLineContext::WindowsCreateProcess).map_err(|e|Error::Backend(e.to_string()))?;
    let mut command=crate::windows::engine::wide(Path::new(&line));
    let mut startup:STARTUPINFOW=unsafe {std::mem::zeroed()};startup.cb=std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process:PROCESS_INFORMATION=unsafe {std::mem::zeroed()};
    // No inherited handles: a persistent helper must not keep a caller's
    // PowerShell/Node stdout pipe open after the CLI has exited.
    if unsafe {CreateProcessW(crate::windows::engine::wide(binary).as_ptr(),command.as_mut_ptr(),std::ptr::null(),std::ptr::null(),0,CREATE_NO_WINDOW,std::ptr::null(),std::ptr::null(),&startup,&mut process)}==0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {windows_sys::Win32::Foundation::CloseHandle(process.hThread);}
    Ok(ChildHandle(process.hProcess))
}

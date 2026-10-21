use super::engine::{write_json, Engine, Manifest};
use super::mount::Mount;
use crate::protocol::*;
use crate::{Error, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
pub struct Ready { pub address: std::net::SocketAddr, pub pid: u32 }
#[derive(Serialize, Deserialize)]
struct Request { token: String, method: Method }
#[derive(Serialize, Deserialize)]
struct Response { result: Option<Value>, error: Option<String>, #[serde(default)] conflict: Option<crate::error::ConflictReport> }

pub fn lock(path: &Path, timeout: Duration) -> Result<File> {
    let deadline=Instant::now()+timeout;
    loop {
        match OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(path) {
            Ok(f)=>return Ok(f),
            Err(e) if matches!(e.raw_os_error(),Some(32|33)) && Instant::now()<deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e)=>return Err(e.into()),
        }
    }
}

pub fn request(state: &Path, method: Method) -> Result<Value> {
    let manifest: Manifest=serde_json::from_slice(&fs::read(state.join("session.json"))?)?;
    let ready: Ready=serde_json::from_slice(&fs::read(state.join("ready.json"))?)?;
    let mut stream=TcpStream::connect_timeout(&ready.address,Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(660)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    serde_json::to_writer(&mut stream,&Request { token: manifest.token,method })?;
    stream.write_all(b"\n")?;
    let mut line=String::new();
    BufReader::new(stream).take(32*1024*1024).read_line(&mut line)?;
    let response: Response=serde_json::from_str(&line)?;
    if let Some(report) = response.conflict { return Err(Error::ProjectConflict(report)); }
    match (response.result,response.error) {
        (value,None)=>Ok(value.unwrap_or(Value::Null)),
        (_,Some(error))=>Err(Error::Backend(error)),
    }
}

pub fn session_info(engine: &Engine, pid: Option<u32>) -> Result<SessionInfo> {
    let mut filesystem=if engine.manifest.fs_mode == FilesystemMode::CbcowPrivate {
        FilesystemInfo::cbcow_private(engine.manifest.view.clone(),pid.unwrap_or(0),0)
    } else { FilesystemInfo::cbcow_through(engine.manifest.view.clone(),pid.unwrap_or(0),0) };
    filesystem.state_provider=Some("windows-winfsp".into());
    filesystem=filesystem.with_durability(engine.manifest.durability, None);
    if engine.manifest.trusted {
        filesystem.isolation="none".into();
        filesystem.isolation_provider=None;
    } else {
        filesystem.isolation_provider=Some("mxc-psec-filesystem".into());
    }
    if engine.manifest.fs_mode == FilesystemMode::CbcowThrough { filesystem.rollback_guarantee="winfsp-routed-writes-only".into(); }
    filesystem.helper_pid=pid;
    filesystem.degraded_protections=Some(vec!["network-policy".into(),"resource-limits".into()]);
    Ok(SessionInfo {
        session_id: engine.manifest.session_id.clone(), project: engine.manifest.source.clone(),
        started_at_ms: engine.manifest.started_at_ms,status: engine.manifest.status.clone(),
        checkpoint_count: engine.manifest.checkpoints.len(),pending_changes: engine.diff(None,None)?.pending_changes,filesystem,
    })
}

fn relative(manifest: &Manifest, path: &Path) -> Result<String> {
    let mapped=crate::pathmap::PathMapper::new(&manifest.source,&manifest.view).map_path(path);
    let path=if mapped.is_absolute() {
        mapped.strip_prefix(&manifest.view)
            .map_err(|_|Error::Backend("path is outside the session project".into()))?
    } else {&mapped};
    path.to_str().map(|s|s.replace('/',"\\")).ok_or_else(||Error::Unsupported("non-Unicode path".into()))
}

pub fn run(state: &Path) -> Result<()> {
    let _lease=lock(&state.join("helper.lock"),Duration::ZERO)?;
    let engine=Arc::new(Mutex::new(Engine::open(state)?));
    {
        let e=engine.lock().unwrap();
        if e.manifest.status != SessionStatus::Running { return Err(Error::SessionStopped(e.manifest.session_id.clone())); }
        if !e.manifest.trusted { super::sandbox::available()?; }
    }
    let mount=Mount::new(engine.clone())?;
    let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0))?;
    let ready=Ready { address: listener.local_addr()?,pid: std::process::id() };
    write_json(&state.join("ready.json"),&ready)?;
    for stream in listener.incoming() {
        let mut stream=stream?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let mut line=String::new();
        if BufReader::new(&mut stream).take(24*1024*1024).read_line(&mut line).is_err() { continue; }
        let request: Request=match serde_json::from_str(&line) { Ok(r)=>r,Err(_)=>continue };
        if request.token != engine.lock().unwrap().manifest.token { continue; }
        let stopping=matches!(request.method,Method::SessionStop(_));
        let result=dispatch(&engine,&mount,state,request.method);
        if matches!(&result, Err(Error::Io(_))) {
            let mut e=engine.lock().unwrap();
            if e.manifest.durable() { e.fail(); }
        }
        let stop=stopping && result.is_ok();
        let recover=engine.lock().unwrap().requires_recovery();
        let response=match result {
            Ok(value)=>Response { result: Some(value),error: None,conflict: None },
            Err(error)=>Response { result: None,conflict: match &error {Error::ProjectConflict(report)=>Some(report.clone()),_=>None},error: Some(error.to_string()) }
        };
        // The operation is authoritative even when its client has disappeared.
        let _=serde_json::to_writer(&mut stream,&response);
        let _=stream.write_all(b"\n");
        // Drop the mount after a failed barrier. The next request starts a fresh
        // helper and replays the retained journal instead of using uncertain RAM.
        if stop || recover {break;}
    }
    drop(mount);
    let _=fs::remove_file(state.join("ready.json"));
    Ok(())
}

fn dispatch(engine: &Arc<Mutex<Engine>>,mount: &Mount,state: &Path,method: Method) -> Result<Value> {
    mount.boundary()?;
    match method {
        Method::SessionExec(params)=> {
            let manifest=engine.lock().unwrap().manifest.clone();
            let result=if manifest.trusted {
                super::sandbox::execute_trusted(&manifest,params)
            } else {
                super::sandbox::execute(&manifest,params,state)
            };
            mount.boundary()?;
            serde_json::to_value(result?).map_err(Into::into)
        }
        Method::FsRead(params)=> {
            let manifest=engine.lock().unwrap().manifest.clone();
            let path=relative(&manifest,&params.path)?;
            engine.lock().unwrap().path(&path,false)?;
            let bytes=fs::read(manifest.view.join(path))?;
            if bytes.len()>16*1024*1024 { return Err(Error::Backend("fs.read exceeds 16 MiB".into())); }
            Ok(serde_json::to_value(FsReadResult { size: bytes.len(),content_base64: base64::engine::general_purpose::STANDARD.encode(bytes) })?)
        }
        Method::FsWrite(params)=> {
            let manifest=engine.lock().unwrap().manifest.clone();
            let path=relative(&manifest,&params.path)?;
            engine.lock().unwrap().path(&path,true)?;
            let bytes=base64::engine::general_purpose::STANDARD.decode(params.content_base64).map_err(|e|Error::InvalidPolicy(e.to_string()))?;
            if bytes.len() as u64>manifest.policy.limits.max_write_bytes.unwrap_or(16*1024*1024) { return Err(Error::Backend("fs.write exceeds max_write_bytes".into())); }
            fs::write(manifest.view.join(path),bytes)?;
            mount.boundary()?;
            Ok(Value::Null)
        }
        other=> {
            let mut e=engine.lock().unwrap();
            let value=match other {
                Method::SessionStatus(_)=>serde_json::to_value(session_info(&e,Some(std::process::id()))?)?,
                Method::SessionStop(_)=> {
                    e.abort(None)?;
                    e.manifest.status=SessionStatus::Stopped;
                    e.save()?;
                    serde_json::to_value(session_info(&e,None)?)?
                }
                Method::CheckpointCreate(p)=>serde_json::to_value(e.checkpoint(p.label)?)?,
                Method::DiffList(p)=>serde_json::to_value(e.diff(p.from.as_deref(),p.to.as_deref())?)?,
                Method::ChangesCommit(p)=> {
                    let (checkpoint_id,applied,folded) = if e.manifest.fs_mode == FilesystemMode::CbcowPrivate {
                        e.commit_private(p.checkpoint.as_deref(),p.ours)?
                    } else {
                        if p.ours { return Err(Error::Unsupported("through mode has no private publications to override".into())); }
                        let (id,applied)=e.commit(p.checkpoint.as_deref())?;(id,applied,0)
                    };
                    serde_json::to_value(CommitResult {checkpoint_id,applied,folded_publications: (folded>0).then_some(folded)})?
                }
                Method::ChangesAbort(p)=> {
                    let (checkpoint_id,restored)=e.abort(p.checkpoint.as_deref())?;
                    serde_json::to_value(AbortResult {checkpoint_id,restored})?
                }
                Method::ReviewBegin(p)=> {
                    let review=if let Some(review)=&e.manifest.review {review.clone()} else {
                        let checkpoint=e.checkpoint(p.label.clone())?;
                        let review=ActiveReview {checkpoint_id: checkpoint.id,kind:p.kind,label:p.label,started_at_ms:super::engine::now_ms()};
                        e.manifest.review=Some(review.clone());e.save()?;review
                    };
                    serde_json::to_value(review)?
                }
                Method::ReviewStatus(_)=>serde_json::to_value(&e.manifest.review)?,
                Method::ReviewDiff(p)=> {
                    let review=e.manifest.review.as_ref().ok_or_else(||Error::ReviewNotActive(p.session_id))?;
                    serde_json::to_value(e.diff(Some(&review.checkpoint_id),None)?)?
                }
                Method::ReviewCommit(p)=> {
                    if e.manifest.review.is_none() { return Err(Error::ReviewNotActive(p.session_id)); }
                    let (checkpoint_id,applied,folded)=if e.manifest.fs_mode == FilesystemMode::CbcowPrivate {e.commit_private(None,false)?} else {let (id,applied)=e.commit(None)?;(id,applied,0)};
                    e.manifest.review=None;e.save()?;
                    serde_json::to_value(CommitResult {checkpoint_id,applied,folded_publications: (folded>0).then_some(folded)})?
                }
                Method::ReviewAbort(p)=> {
                    let checkpoint=e.manifest.review.as_ref().ok_or_else(||Error::ReviewNotActive(p.session_id))?.checkpoint_id.clone();
                    let (checkpoint_id,restored)=e.abort(Some(&checkpoint))?;
                    serde_json::to_value(AbortResult {checkpoint_id,restored})?
                }
                Method::ReviewSkip(p)=> {
                    let review=e.manifest.review.take().ok_or_else(||Error::ReviewNotActive(p.session_id))?;
                    e.save()?;serde_json::to_value(review)?
                }
                Method::SessionSync(_)=>serde_json::to_value(e.sync()?)?,
                _=>return Err(Error::Unsupported("method is unavailable in Windows filesystem helper".into())),
            };
            Ok(value)
        }
    }
}

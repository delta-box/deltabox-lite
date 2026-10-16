//! MXC is used only for filesystem grants. Network policy is deliberately reported as unenforced.
use super::engine::Manifest;
use crate::pathmap::PathMapper;
use crate::protocol::{ExecParams, ExecResult, ExecutionProfile};
use crate::{Error, Result};
use appcontainer_common::base_container_runner::BaseContainerRunner;
use learning_mode_windows::SecurityEnvironmentApi;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use wxc_common::cmdline::{cmdline_from_argv_for_context, CommandLineContext};
use wxc_common::logger::{Logger, Mode};
use wxc_common::models::{BaseProcessUiConfig, ContainerPolicy, ExecutionRequest, FallbackPolicy, NetworkPolicy, UiPolicy};
use wxc_common::sandbox_process::{SandboxBackend, StdioMode};

#[link(name = "advapi32")]
extern "system" {}

pub fn available() -> Result<()> {
    if !BaseContainerRunner::is_process_security_environment_usable() {
        return Err(Error::FilesystemUnavailable("Microsoft MXC PSEC is unavailable on this Windows host".into()));
    }
    let api=SecurityEnvironmentApi::load().map_err(|e| Error::FilesystemUnavailable(e.to_string()))?;
    if !api.supports_deny_paths().map_err(|e| Error::FilesystemUnavailable(e.to_string()))? {
        return Err(Error::FilesystemUnavailable("MXC requires the native PSE_SUPPORT_FS_DENY capability".into()));
    }
    Ok(())
}

fn unicode(path: &Path) -> Result<String> {
    let value=path.to_str().ok_or_else(|| Error::Unsupported("MXC requires Unicode paths".into()))?;
    Ok(if let Some(unc)=value.strip_prefix("\\\\?\\UNC\\") {format!("\\\\{unc}")} else {value.strip_prefix("\\\\?\\").unwrap_or(value).to_owned()})
}

fn resolve_program(program: &str, cwd: &Path) -> Result<PathBuf> {
    let input=Path::new(program);
    let mut candidates=Vec::new();
    if input.is_absolute() { candidates.push(input.to_path_buf()); }
    else if input.components().count()>1 { candidates.push(cwd.join(input)); }
    else {
        // Do not implicitly search the host project's current directory.
        candidates.push(cwd.join(input));
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            candidates.push(directory.join(input));
        }
    }
    for path in candidates {
        if path.is_file() { return Ok(path); }
        if path.extension().is_none() {
            let exe=path.with_extension("exe");
            if exe.is_file() { return Ok(exe); }
        }
    }
    Err(Error::Backend(format!("Windows executable not found: {program}")))
}

pub fn execute(manifest: &Manifest, params: ExecParams, state: &Path) -> Result<ExecResult> {
    available()?;
    if params.argv.is_empty() { return Err(Error::InvalidPolicy("empty argv".into())); }
    if params.execution_profile != ExecutionProfile::WorkspaceWrite {
        return Err(Error::Unsupported("Windows read-write execution profiles are reserved; use workspace-write".into()));
    }
    let mapper=PathMapper::new(&manifest.source,&manifest.view);
    let requested_cwd=params.cwd.as_deref().unwrap_or(&manifest.source);
    let cwd=if requested_cwd.is_absolute() { mapper.map_path(requested_cwd) } else { manifest.view.join(requested_cwd) };
    // Directory-mounted WinFsp volumes do not always have a DOS volume name
    // for GetFinalPathNameByHandle. Keep the mount path for PSEC grants.
    if cwd.components().any(|c| matches!(c,std::path::Component::ParentDir)) {
        return Err(Error::Backend("command cwd contains parent traversal".into()));
    }
    let view=manifest.view.clone();
    if !cwd.starts_with(&view) { return Err(Error::Backend("command cwd must be inside the WinFsp view".into())); }
    let mut argv: Vec<_>=params.argv.iter().map(|a|mapper.map_arg(a)).map(|a| if Path::new(&a).is_absolute() {unicode(Path::new(&a))} else {Ok(a)}).collect::<Result<_>>()?;
    let program=resolve_program(&argv[0],&cwd)?;
    argv[0]=unicode(&program)?;
    let mut env=BTreeMap::new();
    for key in ["PATH","PATHEXT","SystemRoot","WINDIR","COMSPEC","SystemDrive","LOCALAPPDATA","APPDATA","NUMBER_OF_PROCESSORS","PROCESSOR_ARCHITECTURE","LANG","TERM"] {
        if let Ok(value)=std::env::var(key) { env.insert(key.to_uppercase(),value); }
    }
    for (key,value) in params.env.unwrap_or_default() {
        if key.contains(['=','\0']) || value.contains('\0') { return Err(Error::InvalidPolicy("invalid Windows environment entry".into())); }
        env.insert(key.to_uppercase(),mapper.map_env(&key,&value)?);
    }
    for key in ["SystemDrive","LOCALAPPDATA","SystemRoot"] {
        let value=std::env::var(key).ok().filter(|s| !s.is_empty()).ok_or_else(|| Error::Backend(format!("MXC requires host {key}")))?;
        env.insert(key.to_uppercase(),value);
    }
    let home=manifest.scratch.join("home");
    let tmp=manifest.scratch.join("tmp");
    std::fs::create_dir_all(&home)?;std::fs::create_dir_all(&tmp)?;
    for key in ["HOME","USERPROFILE"] { env.insert(key.into(),unicode(&home)?); }
    for key in ["TMP","TEMP"] { env.insert(key.into(),unicode(&tmp)?); }
    let mut read=Vec::new();
    for key in ["SystemRoot","ProgramFiles","ProgramFiles(x86)","ProgramData"] {
        if let Some(path)=std::env::var_os(key).map(PathBuf::from).filter(|p|p.is_dir()) { read.push(unicode(&path)?); }
    }
    if let Some(parent)=program.parent().filter(|p|!p.starts_with(&view)) { read.push(unicode(parent)?); }
    for path in &manifest.policy.filesystem.read {
        if path == Path::new(".") || path.to_string_lossy().starts_with('/') { continue; }
        let path=if path.is_absolute() { path.clone() } else { manifest.view.join(path) };
        read.push(unicode(&mapper.map_path(&path))?);
    }
    // Extra write roots would escape the undo engine. Reserve them until their semantics are implemented.
    if manifest.policy.filesystem.write.iter().any(|p|p != Path::new(".")) {
        return Err(Error::Unsupported("Windows filesystem write grants currently support only the project view".into()));
    }
    let request=ExecutionRequest {
        schema_version: "0.8.0-alpha".into(),
        script_code: cmdline_from_argv_for_context(&argv,CommandLineContext::WindowsCreateProcess).map_err(|e|Error::Backend(e.to_string()))?,
        working_directory: unicode(&cwd)?,
        env: env.into_iter().map(|(k,v)|format!("{k}={v}")).collect(),
        script_timeout: 600_000,
        policy: ContainerPolicy {
            capabilities: vec!["registryRead".into()],
            readwrite_paths: vec![unicode(&view)?,unicode(&manifest.scratch)?],
            readonly_paths: read,
            denied_paths: vec![unicode(&manifest.source)?,unicode(state.parent().unwrap())?],
            fallback: FallbackPolicy { allow_dacl_mutation: false },
            default_network_policy: NetworkPolicy::Allow,
            allow_local_network: true,
            ui: UiPolicy { disable: false, ..Default::default() },
            base_process_ui: BaseProcessUiConfig { isolation: "desktop".into(), ..Default::default() },
            ..Default::default()
        },
        ..Default::default()
    };
    let started=std::time::Instant::now();
    let mut logger=Logger::new(Mode::Buffer);
    let mut process=BaseContainerRunner::new().spawn(&request,&mut logger,StdioMode::Pipes)
        .map_err(|e| Error::Backend(format!("MXC launch failed: {}",e.error_message)))?;
    drop(process.take_stdin());
    let stdout=process.take_stdout().ok_or_else(||Error::Backend("missing MXC stdout".into()))?;
    let stderr=process.take_stderr().ok_or_else(||Error::Backend("missing MXC stderr".into()))?;
    let out=std::thread::spawn(move || crate::worker::read_capped(stdout));
    let err=std::thread::spawn(move || crate::worker::read_capped(stderr));
    let exit=process.wait();
    let (stdout,out_cut)=out.join().map_err(|_|Error::Backend("stdout reader failed".into()))?;
    let (stderr,err_cut)=err.join().map_err(|_|Error::Backend("stderr reader failed".into()))?;
    Ok(ExecResult { exit_code: Some(exit?),stdout,stderr,duration_ms: started.elapsed().as_millis() as u64,truncated: out_cut||err_cut })
}

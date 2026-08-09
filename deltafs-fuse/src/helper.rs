use crate::control::{Command, Request, Response, ResultValue};
use crate::engine::{Durability, EngineOptions, StateEngine};
use crate::passthrough;
use anyhow::{Context, Result};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub source: PathBuf,
    pub mountpoint: PathBuf,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub ready_file: PathBuf,
    pub quota_bytes: u64,
    pub durability: Durability,
    pub group_commit_ms: Option<u64>,
    pub reclaim_dir: Option<PathBuf>,
    pub allow_other: bool,
}

pub fn run(config: Config) -> Result<()> {
    let token = fs::read_to_string(&config.token_file)
        .with_context(|| format!("read token file {}", config.token_file.display()))?;
    let token = token.trim().to_owned();
    let options = EngineOptions {
        quota_bytes: config.quota_bytes,
        durability: config.durability,
        group_commit_ms: config.group_commit_ms,
    };
    let mut engine = if config.state_dir.join("session.json").exists() {
        StateEngine::open_with_options(&config.source, &config.state_dir, options)?
    } else {
        StateEngine::create(&config.source, &config.state_dir, options)?
    };
    remove_async(engine.prepare_gc()?);
    if let Some(directory) = &config.reclaim_dir {
        remove_async(children(directory)?);
    }
    let engine = Arc::new(Mutex::new(engine));
    let mounted = passthrough::spawn_engine(
        &config.source,
        &config.mountpoint,
        engine.clone(),
        config.allow_other,
    )?;

    if let Some(parent) = config.socket.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if config.socket.exists() {
        fs::remove_file(&config.socket)?;
    }
    let listener = UnixListener::bind(&config.socket)?;
    fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o600))?;
    write_ready(&config.ready_file)?;

    let mut running = true;
    while running {
        let (stream, _) = listener.accept()?;
        running = handle(
            stream,
            &token,
            &engine,
            &mounted,
            config.reclaim_dir.as_deref(),
        )?;
    }

    let _ = fs::remove_file(&config.ready_file);
    let _ = fs::remove_file(&config.socket);
    mounted.shutdown()?;
    Ok(())
}

fn handle(
    mut stream: UnixStream,
    token: &str,
    engine: &Arc<Mutex<StateEngine>>,
    mounted: &passthrough::MountedEngine,
    reclaim_dir: Option<&Path>,
) -> Result<bool> {
    let Some(request) = authenticated_request(&mut stream, token)? else {
        return Ok(true);
    };

    let (result, running) = match request.command {
        Command::Ping => (Ok(ResultValue::Pong), true),
        Command::Status => (
            Ok(ResultValue::Status(
                engine.lock().expect("engine lock").status(),
            )),
            true,
        ),
        Command::Checkpoint { label } => (
            engine
                .lock()
                .expect("engine lock")
                .checkpoint(label)
                .map(ResultValue::Checkpoint),
            true,
        ),
        Command::Diff { from } => (
            engine
                .lock()
                .expect("engine lock")
                .diff(from)
                .map(ResultValue::Diff),
            true,
        ),
        Command::Commit { target } => {
            let (result, garbage) = {
                let mut engine = engine.lock().expect("engine lock");
                match engine.commit(target) {
                    Ok(checkpoint) => (
                        Ok(ResultValue::Committed(checkpoint)),
                        engine.prepare_gc().unwrap_or_default(),
                    ),
                    Err(error) => (Err(error), Vec::new()),
                }
            };
            remove_async(garbage);
            (result, true)
        }
        Command::Abort { target } => {
            let result = engine
                .lock()
                .expect("engine lock")
                .rollback(target)
                .map(|()| ResultValue::Aborted);
            if result.is_ok() {
                mounted.invalidate()?;
            }
            (result, true)
        }
        Command::Reclaim => {
            if let Some(directory) = reclaim_dir {
                remove_async(children(directory)?);
            }
            (Ok(ResultValue::Reclaiming), true)
        }
        Command::Shutdown => (Ok(ResultValue::ShuttingDown), false),
    };
    let response = match result {
        Ok(result) => Response::ok(result),
        Err(error) => Response::error(error.to_string()),
    };
    write_response(&mut stream, response)?;
    Ok(running)
}

fn authenticated_request(stream: &mut UnixStream, token: &str) -> Result<Option<Request>> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    if peer_uid(stream)? != unsafe { libc::geteuid() } {
        write_response(
            stream,
            Response::error("control peer is not the session owner"),
        )?;
        return Ok(None);
    }
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            write_response(stream, Response::error(error.to_string()))?;
            return Ok(None);
        }
    };
    if request.token != token {
        write_response(stream, Response::error("invalid control token"))?;
        return Ok(None);
    }
    Ok(Some(request))
}

fn write_ready(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, format!("{}\n", std::process::id()))?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn remove_async(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for path in paths {
            let _ = fs::remove_dir_all(path);
        }
    });
}

fn children(directory: &Path) -> Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()).map_err(Into::into))
        .collect()
}

fn write_response(stream: &mut UnixStream, response: Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

impl Response {
    fn ok(result: ResultValue) -> Self {
        Self {
            result: Some(result),
            error: None,
        }
    }

    fn error(error: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            std::os::fd::AsRawFd::as_raw_fd(stream),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if result == 0 {
        Ok(credentials.uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    let result =
        unsafe { libc::getpeereid(std::os::fd::AsRawFd::as_raw_fd(stream), &mut uid, &mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn wrong_control_token_is_rejected() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            assert!(authenticated_request(&mut server, "expected")
                .unwrap()
                .is_none());
        });
        serde_json::to_writer(
            &mut client,
            &Request {
                token: "wrong".into(),
                command: Command::Ping,
            },
        )
        .unwrap();
        client.write_all(b"\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();

        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        let response: Response = serde_json::from_str(&response).unwrap();
        assert_eq!(response.error.as_deref(), Some("invalid control token"));
        assert!(response.result.is_none());
        thread.join().unwrap();
    }
}

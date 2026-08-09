use crate::engine::{Change, Checkpoint, EngineStatus};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Status,
    Checkpoint { label: Option<String> },
    Diff { from: Option<u64> },
    Commit { target: Option<u64> },
    Abort { target: Option<u64> },
    Reclaim,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum ResultValue {
    Pong,
    Status(EngineStatus),
    Checkpoint(Checkpoint),
    Diff(Vec<Change>),
    Committed(Checkpoint),
    Aborted,
    Reclaiming,
    ShuttingDown,
}

pub fn request(socket: &Path, token: &str, command: Command) -> std::io::Result<ResultValue> {
    let mut stream = UnixStream::connect(socket)?;
    let timeout = Some(match &command {
        Command::Commit { .. } | Command::Abort { .. } => Duration::from_secs(300),
        Command::Diff { .. } => Duration::from_secs(30),
        _ => Duration::from_secs(3),
    });
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    serde_json::to_writer(
        &mut stream,
        &Request {
            token: token.to_owned(),
            command,
        },
    )?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "filesystem helper closed the control connection",
        ));
    }
    let response: Response = serde_json::from_str(&line)?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(error)) => Err(std::io::Error::other(error)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "filesystem helper returned an empty response",
        )),
    }
}

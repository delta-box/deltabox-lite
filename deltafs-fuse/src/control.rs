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
    Checkpoint {
        label: Option<String>,
    },
    Diff {
        from: Option<u64>,
    },
    Commit {
        target: Option<u64>,
        /// Refuse unless this write freeze still holds, so the accepted state
        /// is exactly the frozen one the caller published.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        freeze: Option<u64>,
    },
    Abort {
        target: Option<u64>,
    },
    Reclaim,
    Shutdown,
    /// Hold every write to the mounted view until this control connection
    /// closes. See `freeze`.
    Freeze,
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
    Frozen(u64),
}

pub fn request(socket: &Path, token: &str, command: Command) -> std::io::Result<ResultValue> {
    send(UnixStream::connect(socket)?, token, command).map(|(result, _)| result)
}

/// Writes to the mounted view stay frozen while this is alive. The helper
/// thaws when the connection closes, including when the holder crashes.
#[derive(Debug)]
pub struct Freeze {
    id: u64,
    _stream: UnixStream,
}

impl Freeze {
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// Close the write gate of the mounted view once in-flight writes drain.
pub fn freeze(socket: &Path, token: &str) -> std::io::Result<Freeze> {
    match send(UnixStream::connect(socket)?, token, Command::Freeze)? {
        (ResultValue::Frozen(id), stream) => Ok(Freeze {
            id,
            _stream: stream,
        }),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected freeze response",
        )),
    }
}

fn send(
    mut stream: UnixStream,
    token: &str,
    command: Command,
) -> std::io::Result<(ResultValue, UnixStream)> {
    let timeout = Some(match &command {
        Command::Commit { .. } | Command::Abort { .. } => Duration::from_secs(300),
        Command::Diff { .. } | Command::Freeze => Duration::from_secs(60),
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
    BufReader::new(&stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "filesystem helper closed the control connection",
        ));
    }
    let response: Response = serde_json::from_str(&line)?;
    match (response.result, response.error) {
        (Some(result), None) => Ok((result, stream)),
        (_, Some(error)) => Err(std::io::Error::other(error)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "filesystem helper returned an empty response",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers started by an older CLI reject `freeze` with this serde error,
    // and deltabox-core matches on it to fall back to an unfrozen commit.
    #[test]
    fn an_unknown_command_reports_its_variant_name() {
        let error = serde_json::from_str::<Request>(r#"{"token":"t","command":"freeze_v2"}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown variant `freeze_v2`"), "{error}");
    }
}

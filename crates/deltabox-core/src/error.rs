use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid policy: {0}")]
    InvalidPolicy(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session already exists: {0}")]
    SessionExists(String),
    #[error("session stopped: {0}")]
    SessionStopped(String),
    #[error("session busy: {0}")]
    SessionBusy(String),
    #[error("project changed outside deltabox: {0}")]
    ProjectConflict(String),
    #[error("checkpoint not found: {0}")]
    CheckpointNotFound(String),
    #[error("no active review scope for session: {0}")]
    ReviewNotActive(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("filesystem unavailable: {0}")]
    FilesystemUnavailable(String),
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    #[error("policy parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

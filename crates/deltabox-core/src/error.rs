use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid policy: {0}")]
    InvalidPolicy(String),
    #[error("invalid durability setting: {0}")]
    InvalidDurability(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session already exists: {0}")]
    SessionExists(String),
    #[error("session stopped: {0}")]
    SessionStopped(String),
    #[error("session busy: {0}")]
    SessionBusy(String),
    #[error("project changed outside deltabox: {0}")]
    ProjectConflict(ConflictReport),
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

/// Structured description of a commit-time baseline conflict. `message`
/// preserves the exact pre-structured error text (existing consumers match on
/// it); `conflicts` carries every conflicting path so an agent can reconcile
/// without re-running commit once per path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictReport {
    pub message: String,
    pub conflicts: Vec<PathConflict>,
    #[serde(default)]
    pub truncated: bool,
}

/// One conflicting path: what the host did to it relative to the session's
/// accepted baseline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PathConflict {
    pub path: String,
    pub host_change: crate::changeset::ChangeKind,
}

/// Reports are capped so one pathological drift cannot produce an unbounded
/// error payload on the line-delimited transport.
pub const CONFLICT_REPORT_CAP: usize = 100;

impl ConflictReport {
    pub fn new(message: impl Into<String>, mut conflicts: Vec<PathConflict>) -> Self {
        let truncated = conflicts.len() > CONFLICT_REPORT_CAP;
        conflicts.truncate(CONFLICT_REPORT_CAP);
        Self {
            message: message.into(),
            conflicts,
            truncated,
        }
    }
}

impl std::fmt::Display for ConflictReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

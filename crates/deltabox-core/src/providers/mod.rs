//! Provider traits for the zero-FUSE git-backed session engine.
//!
//! `IsolationProvider` confines one command execution; `StorageProvider`
//! keeps rollback snapshots for the Git ghost index in a private bare repo.

#[cfg(target_os = "linux")]
pub mod isolation;
pub mod storage;

use crate::error::{Error, Result};
use crate::protocol::{DiffResult, ExecResult};
use std::path::{Path, PathBuf};

/// Resolve `path` while allowing the final component(s) to be missing.
pub(crate) fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| Error::Backend("path has no existing ancestor".into()))?;
        suffix.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| Error::Backend("path has no existing ancestor".into()))?;
    }
    let mut resolved = existing.canonicalize()?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

/// Confines one `exec` against the project workspace.
pub trait IsolationProvider {
    /// Whether the provider can run on this host right now.
    fn available(&self) -> bool;

    /// Run `argv` in the confined workspace and return the captured result.
    fn exec(
        &self,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<ExecResult>;
}

/// Ghost-index snapshots stored as commits in a side repo.
pub trait StorageProvider {
    /// Record the current workspace as the next ghost commit and pin it under
    /// `refs/deltabox/undo/<seq>`; the parent is the previous ghost.
    /// Returns the ghost ref owning the snapshot.
    fn snapshot(&mut self) -> Result<String>;

    /// Read the changes between the ghost snapshot and the current workspace.
    fn diff(&self, ghost_ref: &str) -> Result<DiffResult>;

    /// Read the changes between two resolved git trees.
    fn diff_trees(&self, from: &str, to: &str) -> Result<DiffResult>;

    /// Rewind the workspace to the ghost snapshot while retaining its ref.
    /// The session manager may promote that ref to its new baseline before it
    /// drops superseded refs.
    fn restore(&mut self, ghost_ref: &str) -> Result<()>;

    /// Drop one ghost ref; snapshot objects stay until `close`.
    fn drop_ref(&mut self, ghost_ref: &str) -> Result<()>;

    /// List every live ghost ref in the undo namespace.
    fn list_refs(&self) -> Result<Vec<String>>;

    /// Remove the side repository; safe to call when it was never created.
    fn close(&mut self) -> Result<()>;
}

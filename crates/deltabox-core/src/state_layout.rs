use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const PROJECTS_DIR: &str = "projects";

/// External session-record root for one canonical project.
pub(crate) fn state_root(project: &Path) -> Result<PathBuf> {
    Ok(state_base()?.join(PROJECTS_DIR).join(project_key(project)))
}

/// External base for engine state, keyed by session id at the top level.
pub(crate) fn state_base() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DELTABOX_STATE_DIR") {
        return canonical_base(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return canonical_base(PathBuf::from(path).join("deltabox-lite"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Backend("HOME is unavailable".into()))?;
    canonical_base(home.join(".local/state/deltabox-lite"))
}

/// Sandboxed commands work inside trees under this base, and sandbox policies
/// match on resolved paths, so the base must be symlink-free (for example a
/// DELTABOX_STATE_DIR under macOS /var, which links to /private/var). Create
/// it first: canonicalization requires the path to exist.
fn canonical_base(path: PathBuf) -> Result<PathBuf> {
    std::fs::create_dir_all(&path)?;
    Ok(path.canonicalize()?)
}

/// External base for reconstructible runtime files, keyed by session id.
pub(crate) fn runtime_base() -> PathBuf {
    if let Some(path) = std::env::var_os("DELTABOX_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/deltabox-{}", unsafe { libc::geteuid() })))
        .join("deltabox-lite")
}

fn project_key(project: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(project.as_os_str().as_bytes());
    let hex = format!("{:x}", digest.finalize());
    hex[..16].to_string()
}

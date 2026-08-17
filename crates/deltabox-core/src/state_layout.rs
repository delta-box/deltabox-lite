use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const PROJECTS_DIR: &str = "projects";

/// Session-record root for one canonical project. Defaults to the project-local
/// `.deltabox` directory; `DELTABOX_STATE_DIR` relocates records to an
/// arbitrary path, keyed by project hash.
pub(crate) fn state_root(project: &Path) -> Result<PathBuf> {
    state_root_with_override(project, std::env::var_os("DELTABOX_STATE_DIR").as_deref())
}

fn state_root_with_override(project: &Path, override_dir: Option<&OsStr>) -> Result<PathBuf> {
    match override_dir {
        Some(dir) => Ok(PathBuf::from(dir)
            .join(PROJECTS_DIR)
            .join(project_key(project))),
        None => Ok(project.join(".deltabox")),
    }
}

/// External base for engine state, keyed by session id at the top level.
pub(crate) fn state_base() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DELTABOX_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("deltabox-lite"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Backend("HOME is unavailable".into()))?;
    Ok(home.join(".local/state/deltabox-lite"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root_is_project_local_deltabox() {
        let project = tempfile::tempdir().unwrap();
        let root = state_root_with_override(project.path(), None).unwrap();
        assert_eq!(root, project.path().join(".deltabox"));
    }

    #[test]
    fn override_root_relocates_outside_project() {
        let project = tempfile::tempdir().unwrap();
        let override_dir = tempfile::tempdir().unwrap();
        let root = state_root_with_override(project.path(), Some(override_dir.path().as_os_str()))
            .unwrap();
        assert!(root.starts_with(override_dir.path().join("projects")));
        assert!(!root.starts_with(project.path()));
        assert_ne!(root, project.path().join(".deltabox"));
    }
}

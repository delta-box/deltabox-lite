//! Conservative host-project to sandbox-project path mapping.
//!
//! Adapters can use this helper before invoking a tool. It maps whole path
//! values and path-list environment variables, but deliberately does not
//! rewrite arbitrary shell source strings.

use crate::error::{Error, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathMapper {
    host_project: PathBuf,
    sandbox_project: PathBuf,
}

impl PathMapper {
    pub fn new(host_project: impl Into<PathBuf>, sandbox_project: impl Into<PathBuf>) -> Self {
        let host_project = host_project.into();
        let sandbox_project = sandbox_project.into();
        Self {
            host_project: resolve_existing_prefix(&host_project),
            #[cfg(not(windows))]
            sandbox_project: resolve_existing_prefix(&sandbox_project),
            // Keep a WinFsp drive path: canonicalization resolves network
            // volumes to UNC names and changes the PSEC grant identity.
            #[cfg(windows)]
            sandbox_project,
        }
    }

    pub fn map_path(&self, path: &Path) -> PathBuf {
        #[cfg(windows)]
        if path.starts_with(&self.sandbox_project) { return path.to_path_buf(); }
        let resolved = resolve_existing_prefix(path);
        if resolved.starts_with(&self.sandbox_project) {
            return resolved;
        }
        resolved
            .strip_prefix(&self.host_project)
            .map(|rel| self.sandbox_project.join(rel))
            .unwrap_or(resolved)
    }

    pub fn map_arg(&self, value: &str) -> String {
        let path = Path::new(value);
        if path.is_absolute() {
            return self.map_path(path).to_string_lossy().into_owned();
        }
        value.to_string()
    }

    pub fn map_env(&self, key: &str, value: &str) -> Result<String> {
        if is_path_list(key) {
            let mapped: Vec<OsString> = std::env::split_paths(value)
                .map(|path| self.map_path(&path).into_os_string())
                .collect();
            return std::env::join_paths(mapped)
                .map(|value| value.to_string_lossy().into_owned())
                .map_err(|error| {
                    Error::InvalidPolicy(format!("invalid {key} path list: {error}"))
                });
        }
        Ok(self.map_arg(value))
    }
}

fn resolve_existing_prefix(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
    let Ok(mut resolved) = existing.canonicalize() else {
        return path.to_path_buf();
    };
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    resolved
}

fn is_path_list(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "CDPATH"
            | "DYLD_LIBRARY_PATH"
            | "LD_LIBRARY_PATH"
            | "LIBRARY_PATH"
            | "MANPATH"
            | "PKG_CONFIG_PATH"
            | "PYTHONPATH"
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn maps_only_complete_paths_and_prefixes() {
        let mapper = PathMapper::new("/work/project", "/private/shadow");
        assert_eq!(
            mapper.map_arg("/work/project/src/main.rs"),
            "/private/shadow/src/main.rs"
        );
        assert_eq!(mapper.map_arg("/work/projectile/a"), "/work/projectile/a");
        assert_eq!(
            mapper.map_arg("sh -c 'cat /work/project/a'"),
            "sh -c 'cat /work/project/a'"
        );
    }

    #[test]
    fn maps_path_list_components() {
        let mapper = PathMapper::new("/work/project", "/private/shadow");
        assert_eq!(
            mapper
                .map_env("PATH", "/usr/bin:/work/project/bin")
                .unwrap(),
            "/usr/bin:/private/shadow/bin"
        );
    }

    #[test]
    fn maps_paths_through_symlinked_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("host");
        let shadow = root.path().join("shadow");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&shadow).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&host, &alias).unwrap();
        let mapper = PathMapper::new(&host, &shadow);
        assert_eq!(
            mapper.map_path(&alias.join("new.txt")),
            shadow.canonicalize().unwrap().join("new.txt")
        );
    }
}

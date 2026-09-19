use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const PROJECTS_DIR: &str = "projects";

/// Session-record root for one canonical project. Defaults to the
/// project-local `.deltabox` directory so each project stays self-contained;
/// `DELTABOX_STATE_DIR` relocates records to an arbitrary path, keyed by
/// project hash.
pub(crate) fn state_root(project: &Path) -> Result<PathBuf> {
    state_root_with_override(project, std::env::var_os("DELTABOX_STATE_DIR").as_deref())
}

fn state_root_with_override(project: &Path, override_dir: Option<&OsStr>) -> Result<PathBuf> {
    match override_dir {
        Some(dir) => {
            let base = canonical_base(PathBuf::from(dir))?;
            // The project-local `.deltabox` default is safe because every
            // scanner and copier skips that name; an arbitrary override
            // inside the project has no such exemption, so whole-tree copies
            // would include their own output and recurse until
            // ENAMETOOLONG. Refuse the layout up front.
            if base.starts_with(project) {
                return Err(Error::Backend(format!(
                    "state directory {} must live outside the project {}",
                    base.display(),
                    project.display()
                )));
            }
            Ok(base.join(PROJECTS_DIR).join(project_key(project)))
        }
        None => Ok(project.join(".deltabox")),
    }
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

/// Serializes host publication and publication recovery for one project.
pub(crate) fn publish_lock_path(project: &Path) -> Result<PathBuf> {
    Ok(state_root(project)?.join("publish.lock"))
}

/// Project admission leases: `exclusive` for cbcow-through, one file per
/// session under `shared/` for cbcow-private.
pub(crate) fn leases_dir(project: &Path) -> Result<PathBuf> {
    Ok(state_root(project)?.join("leases"))
}

/// External base for reconstructible runtime files, keyed by session id.
/// Names every session directory is built from. They live in one place so the
/// grant list and the deny list can never disagree about what a path is
/// called, and so a protected path never has to be discovered by listing a
/// directory whose contents depend on how far startup got.
pub(crate) const MOUNT_DIR: &str = "mount";
pub(crate) const SCRATCH_DIR: &str = "command";
pub(crate) const ENGINE_DIR: &str = "engine";
pub(crate) const TOKEN_FILE: &str = "token";
pub(crate) const COMMAND_TOKEN_FILE: &str = "command-token";
pub(crate) const CONTROL_SOCKET: &str = "control.sock";
pub(crate) const CONTROL_READY: &str = "ready";
pub(crate) const COMMAND_SOCKET: &str = "command.sock";
pub(crate) const COMMAND_READY: &str = "command.ready";
pub(crate) const SCRATCH_HOME: &str = "home";
pub(crate) const SCRATCH_TMP: &str = "tmp";
pub(crate) const SCRATCH_STORAGE: &str = "cow";

/// Every path one session owns, and what the sandbox owes each of them.
///
/// ```text
/// <state_base>/<sid>/          engine state: protected, never reachable
/// ├── engine/                  the deltafs engine's durable directory
/// ├── token                    control-socket token
/// ├── command-token            command-endpoint token
/// └── command-helper.log       helper output, kept for post-mortems
///
/// <runtime_base>/<sid>/        the session's private runtime
/// ├── mount                  ← view: the workspace a command sees, read-write
/// ├── ready                    ┐  the helper's readiness marker, which the
/// ├── control.sock             │  manager polls before it trusts the mount
/// ├── command.sock             ├─ control leaves: unreachable in every profile
/// ├── command.ready            │
/// ├── command.lock             ┘
/// └── command/               ← container only: never named in a rule
///     ├── home/                ┐  the command's HOME, TMPDIR and
///     ├── tmp/                 ├─ copy-on-write storage: writable, private
///     └── cow/                 ┘  to the session, never part of the view
/// ```
///
/// `command/` is the trap this layout exists to avoid. It shares its parent
/// with the control leaves, so a rule naming that parent — or a deny list
/// assembled by listing the parent's children — takes the command's writable
/// space with it, and the command dies at its first write to `$HOME`. The two
/// container directories are therefore named in no rule at all; they only
/// group their children.
///
/// One structure derives both the grants and the denies, because while the
/// two lists came from separate places they drifted. Denies outrank grants,
/// so a deny above a grant does not narrow it, it removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionLayout {
    /// Session state: the engine's durable home, its tokens and helper logs.
    /// Never reachable from a command.
    pub(crate) state_dir: PathBuf,
    /// The deltafs engine's private directory inside `state_dir`.
    pub(crate) engine_dir: PathBuf,
    /// The FUSE token the control socket authenticates with.
    pub(crate) token: PathBuf,
    /// The token the command endpoint authenticates with.
    pub(crate) command_token: PathBuf,
    /// Session-private root that holds the view, the scratch trees and the
    /// control endpoints.
    pub(crate) runtime_dir: PathBuf,
    /// The workspace a command sees: read-write, and rolled back on abort.
    pub(crate) view: PathBuf,
    /// Scratch root holding the command's private home, temporary directory
    /// and copy-on-write storage.
    pub(crate) scratch_dir: PathBuf,
    pub(crate) control_socket: PathBuf,
    pub(crate) control_ready: PathBuf,
    pub(crate) command_socket: PathBuf,
    pub(crate) command_ready: PathBuf,
    pub(crate) command_lock: PathBuf,
}

impl SessionLayout {
    /// Derives every session path from the two roots the session record
    /// stores. Callers never spell out a child name themselves, so a session
    /// started by this build and a session resumed by it agree on every path
    /// by construction.
    pub(crate) fn from_roots(state_dir: PathBuf, runtime_dir: PathBuf) -> Self {
        let scratch_dir = runtime_dir.join(SCRATCH_DIR);
        let command_ready = runtime_dir.join(COMMAND_READY);
        Self {
            engine_dir: state_dir.join(ENGINE_DIR),
            token: state_dir.join(TOKEN_FILE),
            command_token: state_dir.join(COMMAND_TOKEN_FILE),
            view: runtime_dir.join(MOUNT_DIR),
            scratch_dir,
            control_socket: runtime_dir.join(CONTROL_SOCKET),
            control_ready: runtime_dir.join(CONTROL_READY),
            command_socket: runtime_dir.join(COMMAND_SOCKET),
            // The command endpoint derives its operation lock from its ready
            // file, so the name has to be derived the same way here.
            command_lock: command_ready.with_extension("lock"),
            command_ready,
            state_dir,
            runtime_dir,
        }
    }

    /// Every path the tree above names, in tree order. Only these may appear
    /// in a rule: the two containers are deliberately absent, so naming one of
    /// them in a grant or a deny cannot happen by accident.
    fn session_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![
            self.state_dir.clone(),
            self.engine_dir.clone(),
            self.token.clone(),
            self.command_token.clone(),
            self.view.clone(),
            self.control_socket.clone(),
            self.control_ready.clone(),
            self.command_socket.clone(),
            self.command_ready.clone(),
            self.command_lock.clone(),
        ];
        paths.extend(self.scratch_paths());
        paths
    }

    /// The workspace a command sees, plus the scratch trees it may write.
    pub(crate) fn granted_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.view.clone()];
        paths.extend(self.scratch_paths());
        paths
    }

    /// The command's private scratch: writable, and never part of the view.
    pub(crate) fn scratch_paths(&self) -> Vec<PathBuf> {
        vec![
            self.scratch_dir.join(SCRATCH_HOME),
            self.scratch_dir.join(SCRATCH_TMP),
            self.scratch_dir.join(SCRATCH_STORAGE),
        ]
    }

    /// Paths no command may reach, under any execution profile: the complement
    /// of the grants. Everything the layout owns is denied unless the layout
    /// itself names it as a grant, so a path added later is protected by
    /// default and has to be granted on purpose. The default is closed because
    /// the two mistakes are not equally expensive — an over-wide deny breaks a
    /// command loudly at its first write, an over-wide grant is a hole nobody
    /// sees.
    ///
    /// A deny here outranks every grant the profile adds, which is what lets
    /// `read-write` widen a command's reach to the host without handing it the
    /// sandbox.
    ///
    /// The deny covers opens, execs, links and renames, which is the set
    /// sandlock routes through its deny check. Removal is not in that set, so
    /// a `read-write` command can still unlink one of these files directly:
    /// the profile grants the whole root, and Landlock charges a removal to
    /// the directory holding the file rather than to the file.
    pub(crate) fn protected_paths(&self) -> Vec<PathBuf> {
        let granted = self.granted_paths();
        self.session_paths()
            .into_iter()
            .filter(|path| !granted.contains(path))
            .collect()
    }

    /// Denies outrank grants, so a protected path that sits above a granted
    /// one does not narrow that grant: it removes it. Checking the two lists
    /// against each other is what makes "widen access" and "keep the sandbox
    /// shut" compatible, and it fails at session start rather than as a
    /// mysterious permission error inside a command.
    pub(crate) fn assert_disjoint(&self) -> Result<()> {
        for protected in self.protected_paths() {
            for granted in self.granted_paths() {
                if granted.starts_with(&protected) {
                    return Err(Error::Backend(format!(
                        "protected path {} contains granted path {}",
                        protected.display(),
                        granted.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

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

    fn layout() -> SessionLayout {
        SessionLayout::from_roots(
            PathBuf::from("/state/session-1"),
            PathBuf::from("/run/session-1"),
        )
    }

    /// The control plane is the reason the deny list exists at all. Anything
    /// the helper publishes for the manager to trust — readiness, sockets,
    /// tokens — has to be on it in every profile, not just when a wider
    /// profile happens to make it reachable.
    #[test]
    fn protected_paths_cover_the_whole_control_plane() {
        assert_eq!(
            layout().protected_paths(),
            vec![
                PathBuf::from("/state/session-1"),
                PathBuf::from("/state/session-1/engine"),
                PathBuf::from("/state/session-1/token"),
                PathBuf::from("/state/session-1/command-token"),
                PathBuf::from("/run/session-1/control.sock"),
                PathBuf::from("/run/session-1/ready"),
                PathBuf::from("/run/session-1/command.sock"),
                PathBuf::from("/run/session-1/command.ready"),
                PathBuf::from("/run/session-1/command.lock"),
            ]
        );
    }

    /// The deny list is the complement of the grants, not a list someone has
    /// to remember to extend: every path the layout owns is on exactly one of
    /// the two sides.
    #[test]
    fn every_path_is_either_granted_or_protected() {
        let layout = layout();
        let granted = layout.granted_paths();
        let protected = layout.protected_paths();
        let mut union = granted.clone();
        union.extend(protected.clone());
        assert_eq!(union.len(), layout.session_paths().len());
        for path in &granted {
            assert!(!protected.contains(path), "{} is on both sides", path.display());
        }
    }

    /// The containers group their children and appear in no rule. Naming one
    /// of them would drag the control leaves along with the scratch, which is
    /// the failure this layout replaced.
    #[test]
    fn containers_are_named_in_no_rule() {
        let layout = layout();
        for container in [&layout.runtime_dir, &layout.scratch_dir] {
            assert!(!layout.session_paths().contains(container));
        }
    }

    /// The engine state is denied as one subtree, so its children need no
    /// rule of their own only as long as they stay inside it.
    #[test]
    fn the_engine_state_covers_its_own_children() {
        let layout = layout();
        for child in [&layout.engine_dir, &layout.token, &layout.command_token] {
            assert!(child.starts_with(&layout.state_dir));
        }
    }

    /// Scratch is the command's own home, temporary directory and
    /// copy-on-write storage. It shares `runtime_dir` with the control
    /// endpoints, so a deny that names the shared parent — or the scratch
    /// directory itself — would take the command's writable space with it.
    #[test]
    fn scratch_is_never_nested_under_a_protected_path() {
        let layout = layout();
        layout.assert_disjoint().unwrap();
        assert_eq!(
            layout.scratch_paths(),
            vec![
                PathBuf::from("/run/session-1/command/home"),
                PathBuf::from("/run/session-1/command/tmp"),
                PathBuf::from("/run/session-1/command/cow"),
            ]
        );
    }

    /// The containers are neither granted nor denied, and the check has to
    /// notice when that stops being true.
    #[test]
    fn a_protected_container_above_scratch_is_rejected() {
        let mut layout = layout();
        layout.control_ready = layout.runtime_dir.clone();
        let error = layout.assert_disjoint().unwrap_err();
        assert!(
            error.to_string().contains("contains granted path"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_operation_lock_tracks_the_ready_file() {
        let layout = layout();
        assert_eq!(
            layout.command_lock,
            layout.command_ready.with_extension("lock")
        );
    }
}

use crate::error::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};

/// Default secret-ish patterns hard-denied for agent workloads.
pub const DEFAULT_DENY_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    ".ssh",
    ".aws",
    ".gnupg",
    ".git-credentials",
    "Library/Keychains",
    "Keychains",
    "id_rsa",
    "id_ed25519",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Policy {
    pub filesystem: FilesystemPolicy,
    pub network: NetworkPolicy,
    pub limits: LimitsPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FilesystemPolicy {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub deny: Vec<String>,
    /// Permit a trusted orchestrator to add exact external paths for one exec
    /// (`fs_read_allow` / `fs_write_allow`). Default: false.
    pub allow_approval: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkPolicy {
    pub mode: NetworkMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Permit a trusted orchestrator to add exact destinations for one exec.
    pub allow_approval: bool,
    /// Exact destinations that even an approved exec cannot add.
    pub deny: Vec<String>,
    /// Allow proxy connections to private/local addresses. Default: false.
    pub allow_private: bool,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            mode: NetworkMode::AllowAudit,
            allow: Vec::new(),
            allow_approval: false,
            deny: Vec::new(),
            allow_private: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// Permit all network operations. The sandbox backend remains the audit
    /// boundary, but this mode does not restrict destinations.
    #[default]
    AllowAudit,
    DenyAll,
    Allowlist,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NetworkModeWire {
    Name(NetworkMode),
    Legacy { allowlist: Vec<String> },
}

impl Default for NetworkModeWire {
    fn default() -> Self {
        Self::Name(NetworkMode::AllowAudit)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NetworkPolicyWire {
    mode: NetworkModeWire,
    allow: Vec<String>,
    allow_approval: bool,
    deny: Vec<String>,
    allow_private: bool,
}

impl<'de> Deserialize<'de> for NetworkPolicy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = NetworkPolicyWire::deserialize(deserializer)?;
        let (mode, allow) = match wire.mode {
            NetworkModeWire::Name(mode) => (mode, wire.allow),
            NetworkModeWire::Legacy { allowlist } => {
                if !wire.allow.is_empty() {
                    return Err(serde::de::Error::custom(
                        "network allowlist is specified in both mode and allow",
                    ));
                }
                (NetworkMode::Allowlist, allowlist)
            }
        };
        Ok(Self {
            mode,
            allow,
            allow_approval: wire.allow_approval,
            deny: wire.deny,
            allow_private: wire.allow_private,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LimitsPolicy {
    pub max_memory_bytes: Option<u64>,
    pub max_processes: Option<u32>,
    pub max_open_files: Option<u32>,
    pub max_cpu_percent: Option<u8>,
    /// Maximum payload accepted by one `fs.write` operation.
    pub max_write_bytes: Option<u64>,
    /// Maximum aggregate size of added and modified regular files relative to
    /// the accepted baseline.
    pub max_storage_bytes: Option<u64>,
}

impl Policy {
    /// Widen this policy for one command execution under `profile`.
    ///
    /// The widening is additive: `ExecutionProfile::ReadWrite` adds the
    /// filesystem root to the read and write grants so a command may touch
    /// paths outside the workspace. Protected denies (engine state, helper
    /// endpoints, the backing project, and secret patterns) are handed to the
    /// backend separately and keep winning over the widened grants.
    pub fn apply_execution_profile(&mut self, profile: crate::protocol::ExecutionProfile) {
        if profile == crate::protocol::ExecutionProfile::ReadWrite {
            self.filesystem.read.push(PathBuf::from("/"));
            self.filesystem.write.push(PathBuf::from("/"));
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let policy: Policy =
            toml::from_str(&raw).map_err(|e| Error::InvalidPolicy(e.to_string()))?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn default_agent() -> Self {
        Self {
            filesystem: FilesystemPolicy {
                read: vec![
                    PathBuf::from("."),
                    PathBuf::from("/usr"),
                    PathBuf::from("/bin"),
                    PathBuf::from("/lib"),
                    PathBuf::from("/lib64"),
                    PathBuf::from("/etc"),
                    PathBuf::from("/run"),
                ],
                write: vec![PathBuf::from(".")],
                deny: DEFAULT_DENY_PATTERNS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                allow_approval: false,
            },
            network: NetworkPolicy {
                mode: NetworkMode::AllowAudit,
                allow: Vec::new(),
                allow_approval: false,
                deny: Vec::new(),
                allow_private: false,
            },
            limits: LimitsPolicy {
                // sandlock's memory limiter is opt-in because it is not
                // compatible with V8's virtual-memory reservation strategy.
                max_memory_bytes: None,
                max_processes: Some(32),
                max_open_files: Some(1024),
                max_cpu_percent: None,
                max_write_bytes: Some(16 * 1024 * 1024),
                max_storage_bytes: Some(1024 * 1024 * 1024),
            },
        }
    }

    /// The RPC caller is the trusted approval authority, as for execution_profile.
    /// Grants are applied to a clone and never persisted into session policy.
    pub fn apply_network_allow(&mut self, targets: &[String]) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        if !self.network.allow_approval || self.network.mode != NetworkMode::Allowlist {
            return Err(Error::InvalidPolicy(
                "network approvals are disabled by session policy".into(),
            ));
        }
        if targets.len() > 32 {
            return Err(Error::InvalidPolicy(
                "at most 32 network targets per exec".into(),
            ));
        }
        for target in targets {
            let target = crate::network::normalize_target(target)?;
            if self
                .network
                .deny
                .iter()
                .any(|denied| crate::network::normalize_target(denied).is_ok_and(|v| v == target))
            {
                return Err(Error::InvalidPolicy(format!(
                    "network target is prohibited: {target}"
                )));
            }
            if !self.network.allow.contains(&target) {
                self.network.allow.push(target);
            }
        }
        Ok(())
    }

    /// Add exact external paths for one command (`fs_read_allow` is rx,
    /// `fs_write_allow` is rwx). Additive and never persisted: callers apply
    /// it to a cloned policy. Protected denies (engine state, the backing
    /// project, helper endpoints) are handed to the backend separately and
    /// keep winning over these grants.
    pub fn apply_fs_grants(&mut self, read: &[PathBuf], write: &[PathBuf]) -> Result<()> {
        if read.is_empty() && write.is_empty() {
            return Ok(());
        }
        if !self.filesystem.allow_approval {
            return Err(Error::InvalidPolicy(
                "filesystem approvals are disabled by session policy".into(),
            ));
        }
        if read.len() + write.len() > 32 {
            return Err(Error::InvalidPolicy(
                "at most 32 filesystem grants per exec".into(),
            ));
        }
        let read = read
            .iter()
            .map(|p| self.checked_grant(p, false))
            .collect::<Result<Vec<_>>>()?;
        let write = write
            .iter()
            .map(|p| self.checked_grant(p, true))
            .collect::<Result<Vec<_>>>()?;
        for path in read {
            if !self.filesystem.read.contains(&path) {
                self.filesystem.read.push(path);
            }
        }
        for path in write {
            // rwx implies rx.
            if !self.filesystem.read.contains(&path) {
                self.filesystem.read.push(path.clone());
            }
            if !self.filesystem.write.contains(&path) {
                self.filesystem.write.push(path);
            }
        }
        Ok(())
    }

    fn checked_grant(&self, path: &Path, allow_missing_leaf: bool) -> Result<PathBuf> {
        let bad = |why: &str| Error::InvalidPolicy(format!("filesystem grant {}: {why}", path.display()));
        let text = path.to_string_lossy();
        if text.is_empty() || text.chars().any(|c| c.is_control()) {
            return Err(bad("must be a non-empty path without control characters"));
        }
        if !path.is_absolute() {
            return Err(bad("must be absolute"));
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(bad("must not contain `..`"));
        }
        if text.contains('*') || text.contains('?') {
            return Err(bad("wildcards are not allowed; grant an exact path"));
        }
        // Resolve symlinks so a grant names the real target. For a write grant
        // the final components may not exist yet (a fresh cache directory):
        // resolve the deepest existing ancestor and keep the remainder.
        let mut existing = path.to_path_buf();
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        let resolved = loop {
            match std::fs::canonicalize(&existing) {
                Ok(real) => break real,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && allow_missing_leaf => {
                    let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                        return Err(bad("does not exist"));
                    };
                    rest.push(name);
                    if !existing.pop() {
                        return Err(bad("does not exist"));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(bad("does not exist"));
                }
                Err(e) => return Err(bad(&e.to_string())),
            }
        };
        let resolved = rest.iter().rev().fold(resolved, |acc, part| acc.join(part));
        if resolved.parent().is_none() {
            return Err(bad("the filesystem root cannot be granted; use the read-write execution profile"));
        }
        if [&resolved, path].iter().any(|candidate| {
            grant_is_denied(&self.filesystem.deny, &candidate.to_string_lossy(), cfg!(windows))
        }) {
            return Err(bad("matches a protected deny pattern"));
        }
        Ok(resolved)
    }

    pub fn validate(&self) -> Result<()> {
        if self.network.allow_approval {
            if self.network.mode != NetworkMode::Allowlist {
                return Err(Error::InvalidPolicy(
                    "allow_approval requires network allowlist mode".into(),
                ));
            }
            for target in self.network.allow.iter().chain(&self.network.deny) {
                crate::network::normalize_target(target)?;
            }
        } else if !self.network.deny.is_empty() || self.network.allow_private {
            return Err(Error::InvalidPolicy(
                "network deny and allow_private require allow_approval".into(),
            ));
        }
        match self.network.mode {
            NetworkMode::Allowlist => {
                for target in &self.network.allow {
                    if target.trim().is_empty() || target.chars().any(char::is_control) {
                        return Err(Error::InvalidPolicy(
                            "network allowlist entries must be non-empty and contain no control characters"
                                .into(),
                        ));
                    }
                    if target == "*" {
                        return Err(Error::InvalidPolicy(
                            "network allowlist must not contain '*'; use allow_audit instead"
                                .into(),
                        ));
                    }
                }
            }
            NetworkMode::AllowAudit | NetworkMode::DenyAll if !self.network.allow.is_empty() => {
                return Err(Error::InvalidPolicy(
                    "network.allow is only valid when mode = \"allowlist\"".into(),
                ));
            }
            NetworkMode::AllowAudit | NetworkMode::DenyAll => {}
        }
        Ok(())
    }

    /// True when `path` matches any deny pattern.
    ///
    /// This is a deliberately small matcher (exact, directory-prefix with
    /// trailing `/`, `*`/`?` wildcards, and component matching for patterns
    /// without `/`). It is not gitignore semantics; we document that.
    pub fn path_is_denied(&self, path: &Path) -> bool {
        let s = path.to_string_lossy();
        self.filesystem.deny.iter().any(|p| deny_matches(p, &s))
    }
}

/// Deny check for a filesystem grant. The matcher splits on `/`, so Windows
/// paths (`C:\Users\me\.ssh`, or `\\?\C:\...` from `canonicalize`) are compared
/// with `/` separators, and case-insensitively where the filesystem is.
fn grant_is_denied(deny: &[String], path: &str, fold_case: bool) -> bool {
    let normalize = |value: &str| {
        let value = value.replace('\\', "/");
        if fold_case {
            value.to_lowercase()
        } else {
            value
        }
    };
    let path = normalize(path);
    deny.iter()
        .any(|pattern| deny_matches(&normalize(pattern), &path))
}

pub fn deny_matches(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('/') {
        let with_slash = format!("{prefix}/");
        if path == prefix || path.starts_with(&with_slash) {
            return true;
        }
        if !prefix.contains('/') {
            return path.split('/').any(|component| component == prefix);
        }
        return path.ends_with(&format!("/{prefix}")) || path.ends_with(&format!("/{prefix}/"));
    }
    if wildcard_match(pattern, path) {
        return true;
    }
    if !pattern.contains('/') {
        return path
            .split('/')
            .any(|component| wildcard_match(pattern, component));
    }
    false
}

/// Iterative wildcard match supporting `*` (any run) and `?` (one char).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_agent_policy_denies_secrets() {
        let policy = Policy::default_agent();
        assert!(policy.path_is_denied(Path::new("/proj/.env")));
        assert!(policy.path_is_denied(Path::new("/proj/config/.env.local")));
        assert!(policy.path_is_denied(Path::new("/proj/cert.pem")));
        assert!(policy.path_is_denied(Path::new("/Users/me/.ssh/id_rsa")));
        assert!(!policy.path_is_denied(Path::new("/proj/src/main.rs")));
        assert!(!policy.path_is_denied(Path::new("/proj/node_modules/x/index.js")));
    }

    fn approving() -> Policy {
        let mut policy = Policy::default_agent();
        policy.filesystem.allow_approval = true;
        policy
    }

    #[test]
    fn fs_grants_require_session_opt_in() {
        let mut policy = Policy::default_agent();
        let err = policy
            .apply_fs_grants(&[PathBuf::from("/usr")], &[])
            .unwrap_err();
        assert!(err.to_string().contains("disabled by session policy"));
        assert!(policy.apply_fs_grants(&[], &[]).is_ok());
    }

    #[test]
    fn fs_grants_add_exact_paths_and_write_implies_read() {
        let dir = std::env::temp_dir().join(format!("dbx-grant-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = std::fs::canonicalize(&dir).unwrap();
        let mut policy = approving();
        policy
            .apply_fs_grants(&[PathBuf::from("/usr/bin")], &[dir.join("cache/new")])
            .unwrap();
        assert!(policy.filesystem.read.contains(&PathBuf::from("/usr/bin")));
        let want = real.join("cache/new");
        assert!(policy.filesystem.write.contains(&want));
        assert!(policy.filesystem.read.contains(&want));
        // Only the requested paths were added.
        assert!(!policy.filesystem.write.contains(&PathBuf::from("/")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fs_grants_reject_unsafe_paths() {
        let mut policy = approving();
        for bad in ["relative/path", "/usr/../etc", "/usr/*", "/"] {
            assert!(
                policy.apply_fs_grants(&[PathBuf::from(bad)], &[]).is_err(),
                "{bad}"
            );
        }
        assert!(policy
            .apply_fs_grants(&[PathBuf::from("/definitely/missing/path")], &[])
            .is_err());
        assert!(policy
            .apply_fs_grants(&[PathBuf::from("/definitely/missing/path")], &[])
            .is_err());
        let many: Vec<PathBuf> = (0..33).map(|_| PathBuf::from("/usr")).collect();
        assert!(policy.apply_fs_grants(&many, &[]).is_err());
        // Failures leave the policy unchanged.
        assert_eq!(policy.filesystem.read, Policy::default_agent().filesystem.read);
    }

    #[test]
    fn fs_grants_reject_protected_deny_matches() {
        let dir = std::env::temp_dir().join(format!("dbx-deny-{}", std::process::id()));
        let ssh = dir.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let mut policy = approving();
        assert!(policy.apply_fs_grants(&[ssh.clone()], &[]).is_err());
        assert!(policy.apply_fs_grants(&[], &[ssh.join("new")]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grant_deny_check_handles_windows_paths() {
        let deny = Policy::default_agent().filesystem.deny;
        for path in [
            r"C:\Users\me\.ssh",
            r"C:\Users\me\.ssh\config",
            r"\\?\C:\Users\me\.SSH\id_ed25519",
            r"D:\certs\server.PEM",
            r"\\?\UNC\host\share\.aws",
        ] {
            assert!(grant_is_denied(&deny, path, true), "{path}");
        }
        for path in [r"C:\Program Files\Huawei\DevEco Studio\sdk", r"\\?\D:\cache\devecocli"] {
            assert!(!grant_is_denied(&deny, path, true), "{path}");
        }
        // Case folding is only for case-insensitive filesystems.
        assert!(!grant_is_denied(&deny, "/home/me/.SSH", false));
        assert!(grant_is_denied(&deny, "/home/me/.ssh", false));
    }

    #[test]
    fn deny_matcher_forms() {
        assert!(deny_matches(".env", "/proj/.env"));
        assert!(deny_matches(".env.*", "/proj/.env.local"));
        assert!(deny_matches("*.pem", "/proj/deep/cert.pem"));
        assert!(deny_matches("secret/", "/proj/secret"));
        assert!(deny_matches("secret/", "/proj/secret/x"));
        assert!(deny_matches("id_rsa", "/Users/me/.ssh/id_rsa"));
        assert!(!deny_matches("*.pem", "/proj/deep/cert.pem2"));
        assert!(!deny_matches("secret/", "/proj/secretive/x"));
    }

    #[test]
    fn policy_load_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(
            &path,
            r#"
[filesystem]
read = ["."]
write = ["."]
deny = [".env", "*.pem"]

[network]
mode = "deny_all"

[limits]
max_processes = 4
"#,
        )
        .unwrap();
        let policy = Policy::load(&path).unwrap();
        assert_eq!(policy.network.mode, NetworkMode::DenyAll);
        assert_eq!(policy.limits.max_processes, Some(4));
        assert!(policy.path_is_denied(Path::new("/x/.env")));
    }

    #[test]
    fn ergonomic_and_legacy_network_allowlists_load() {
        let ergonomic: Policy = toml::from_str(
            r#"
[network]
mode = "allowlist"
allow = ["registry.npmjs.org:443", "127.0.0.1:3000"]
"#,
        )
        .unwrap();
        ergonomic.validate().unwrap();
        assert_eq!(ergonomic.network.mode, NetworkMode::Allowlist);
        assert_eq!(ergonomic.network.allow.len(), 2);

        let legacy: Policy = toml::from_str(
            r#"
[network]
mode = { allowlist = ["example.com:443"] }
"#,
        )
        .unwrap();
        legacy.validate().unwrap();
        assert_eq!(legacy.network.mode, NetworkMode::Allowlist);
        assert_eq!(legacy.network.allow, ["example.com:443"]);
    }

    #[test]
    fn approved_network_grants_are_additive_local_and_deny_preserving() {
        let base: Policy = toml::from_str(r#"
[network]
mode = "allowlist"
allow_approval = true
allow = ["fixed.example:443"]
deny = ["blocked.example:443"]
"#).unwrap();
        base.validate().unwrap();
        let mut command = base.clone();
        command.apply_network_allow(&["NEW.example.:443".into()]).unwrap();
        assert_eq!(base.network.allow, ["fixed.example:443"]);
        assert_eq!(command.network.allow, ["fixed.example:443", "new.example:443"]);
        assert!(command.apply_network_allow(&["BLOCKED.example:443".into()]).is_err());
        assert!(command.apply_network_allow(&["*:443".into()]).is_err());
        assert!(Policy::default_agent().apply_network_allow(&["new.example:443".into()]).is_err());
        command.apply_execution_profile(crate::protocol::ExecutionProfile::ReadWrite);
        assert_eq!(command.network.deny, ["blocked.example:443"]);
    }

    #[test]
    fn allow_entries_require_allowlist_mode() {
        let policy: Policy = toml::from_str(
            r#"
[network]
mode = "deny_all"
allow = ["example.com:443"]
"#,
        )
        .unwrap();
        assert!(policy.validate().is_err());
    }
}

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
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkPolicy {
    pub mode: NetworkMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            mode: NetworkMode::AllowAudit,
            allow: Vec::new(),
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
        Ok(Self { mode, allow })
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
            },
            network: NetworkPolicy {
                mode: NetworkMode::AllowAudit,
                allow: Vec::new(),
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

    pub fn validate(&self) -> Result<()> {
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

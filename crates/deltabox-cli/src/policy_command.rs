use anyhow::{bail, Context};
use clap::{Args, Subcommand};
use deltabox_core::policy::Policy;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum PolicyCommand {
    /// Validate a policy's TOML, field names, and policy constraints without starting a session.
    Check(CheckArgs),
    /// Print a policy with defaults filled in, or the default agent policy when no file is given.
    Show(ShowArgs),
}

#[derive(Args)]
pub struct CheckArgs {
    /// TOML policy file to check.
    #[arg(value_name = "FILE")]
    file: PathBuf,
    /// Print JSON containing the validation result and resolved policy.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct ShowArgs {
    /// Load this file using the same defaults as session start --policy.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,
    /// Print JSON instead of TOML.
    #[arg(long)]
    json: bool,
}

pub fn run(cmd: PolicyCommand) -> anyhow::Result<()> {
    match cmd {
        PolicyCommand::Check(args) => {
            let policy = read_policy(&args.file, true)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "valid": true,
                        "policy": policy,
                    }))?
                );
            } else {
                println!("Policy is valid: {}", args.file.display());
            }
        }
        PolicyCommand::Show(args) => {
            let policy = match args.policy {
                Some(path) => read_policy(&path, false)?,
                None => Policy::default_agent(),
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&policy)?);
            } else {
                print!("{}", toml::to_string_pretty(&policy)?);
            }
        }
    }
    Ok(())
}

fn read_policy(path: &Path, strict_keys: bool) -> anyhow::Result<Policy> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read policy {}", path.display()))?;
    let value: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("invalid policy TOML in {}", path.display()))?;
    if strict_keys {
        check_keys(&value).with_context(|| format!("invalid policy {}", path.display()))?;
    }
    let policy: Policy = value
        .try_into()
        .with_context(|| format!("invalid policy fields in {}", path.display()))?;
    policy
        .validate()
        .with_context(|| format!("invalid policy {}", path.display()))?;
    Ok(policy)
}

// Keep the runtime's permissive deserialization unchanged. The explicit check
// command adds typo detection, including the legacy inline allowlist table.
fn check_keys(value: &toml::Value) -> anyhow::Result<()> {
    check_table_keys(value, "", &["filesystem", "network", "limits"])?;
    if let Some(filesystem) = value.get("filesystem") {
        check_table_keys(
            filesystem,
            "filesystem",
            &["read", "write", "deny", "allow_approval"],
        )?;
    }
    if let Some(network) = value.get("network") {
        check_table_keys(
            network,
            "network",
            &["mode", "allow", "allow_approval", "deny", "allow_private"],
        )?;
        if let Some(mode) = network.get("mode") {
            check_table_keys(mode, "network.mode", &["allowlist"])?;
        }
    }
    if let Some(limits) = value.get("limits") {
        check_table_keys(
            limits,
            "limits",
            &[
                "max_memory_bytes",
                "max_processes",
                "max_open_files",
                "max_cpu_percent",
                "max_write_bytes",
                "max_storage_bytes",
            ],
        )?;
    }
    Ok(())
}

fn check_table_keys(value: &toml::Value, path: &str, allowed: &[&str]) -> anyhow::Result<()> {
    if let Some(table) = value.as_table() {
        for key in table.keys() {
            if !allowed.contains(&key.as_str()) {
                let field = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                bail!("unknown policy key '{field}'");
            }
        }
    }
    Ok(())
}

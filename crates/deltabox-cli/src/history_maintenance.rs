use clap::Args;
use deltabox_core::coordinator::SessionCoordinator;
use deltabox_core::history_storage::{
    HistoryGcResult, HistoryPinParams, HistoryPruneApplyParams, HistoryRetentionPolicy,
};
use std::path::PathBuf;

#[derive(Args)]
pub struct ProjectArgs {
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// Verify one version, or every published version when omitted.
    pub version: Option<String>,
    #[command(flatten)]
    pub target: ProjectArgs,
}

#[derive(Args)]
pub struct PinArgs {
    pub version: String,
    /// Remove protection so a later explicit prune can select this version.
    #[arg(long)]
    pub remove: bool,
    #[command(flatten)]
    pub target: ProjectArgs,
}

#[derive(Args)]
pub struct PruneArgs {
    #[command(flatten)]
    pub target: ProjectArgs,
    /// Always retain the newest N versions, in addition to pins.
    #[arg(long, default_value_t = 1)]
    pub keep_last: usize,
    /// Only versions strictly older than this Unix millisecond timestamp are eligible.
    #[arg(long)]
    pub older_than_ms: Option<u64>,
    /// Remove oldest eligible versions until the retained logical-byte budget is met.
    /// Pins and keep/age protections can leave the budget unsatisfied.
    #[arg(long)]
    pub max_logical_bytes: Option<u64>,
    /// Execute a previously inspected PLAN_ID with exactly the same policy.
    /// Without this argument, only print a plan; never delete anything.
    #[arg(long, value_name = "PLAN_ID")]
    pub apply: Option<String>,
}

pub fn stats(args: ProjectArgs) -> anyhow::Result<()> {
    let stats = SessionCoordinator::new(&args.project, None)?.history_storage_stats()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "{:<36} {:>5} {:>14} {:>14} {:>14}",
            "VERSION", "PIN", "LOGICAL_BYTES", "APPARENT_BYTES", "ALLOCATED_BYTES"
        );
        for item in &stats.versions {
            println!(
                "{:<36} {:>5} {:>14} {:>14} {:>14}",
                item.version.id,
                if item.pinned { "yes" } else { "-" },
                item.version.logical_bytes,
                item.usage.apparent_bytes,
                item.usage.allocated_bytes
            );
        }
        println!(
            "Logical total: {} bytes; namespace apparent: {} bytes; allocated: {} bytes",
            stats.logical_bytes, stats.total.apparent_bytes, stats.total.allocated_bytes
        );
        println!(
            "Abandoned captures: {} apparent bytes; garbage: {} apparent bytes; pending GC: {}",
            stats.abandoned_captures.apparent_bytes, stats.garbage.apparent_bytes, stats.pending_gc
        );
        println!("Allocated bytes deduplicate hardlinks, but may double-count APFS clone-shared extents; they are not exclusive or reclaimable bytes.");
    }
    Ok(())
}

pub fn verify(args: VerifyArgs) -> anyhow::Result<()> {
    let result = SessionCoordinator::new(&args.target.project, None)?
        .history_verify(args.version.as_deref())?;
    if args.target.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        for entry in &result.versions {
            // JSON-escape corrupt names and error strings before terminal output.
            println!(
                "{} {}{}",
                if entry.valid { "OK" } else { "CORRUPT" },
                serde_json::to_string(&entry.version_id)?,
                entry
                    .error
                    .as_ref()
                    .map(|error| format!(" {error:?}"))
                    .unwrap_or_default()
            );
        }
        println!("Checked {} versions.", result.versions.len());
    }
    anyhow::ensure!(result.valid, "history integrity verification failed");
    Ok(())
}

pub fn pin(args: PinArgs) -> anyhow::Result<()> {
    let pins =
        SessionCoordinator::new(&args.target.project, None)?.history_pin(HistoryPinParams {
            version_id: args.version,
            pinned: !args.remove,
        })?;
    if args.target.json {
        println!("{}", serde_json::to_string_pretty(&pins)?);
    } else {
        println!("Pinned versions: {}", pins.len());
        for id in pins {
            println!("{id}");
        }
    }
    Ok(())
}

pub fn prune(args: PruneArgs) -> anyhow::Result<()> {
    let manager = SessionCoordinator::new(&args.target.project, None)?;
    let policy = HistoryRetentionPolicy {
        keep_last: args.keep_last,
        older_than_ms: args.older_than_ms,
        max_logical_bytes: args.max_logical_bytes,
    };
    if let Some(expected_plan_id) = args.apply {
        let result = manager.history_prune_apply(HistoryPruneApplyParams {
            policy,
            expected_plan_id,
        })?;
        return print_gc(result, args.target.json);
    }
    let plan = manager.history_prune_plan(policy)?;
    if args.target.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!("Plan: {}", plan.plan_id);
        for version in &plan.remove {
            println!(
                "REMOVE {} {} logical bytes",
                version.id, version.logical_bytes
            );
        }
        for id in &plan.abandoned_capture_ids {
            println!("REMOVE abandoned capture {id}");
        }
        println!(
            "Retain {} versions ({} logical bytes); budget satisfied: {}",
            plan.retained.len(),
            plan.retained_logical_bytes,
            plan.budget_satisfied
        );
        println!(
            "No changes made. Repeat with the same policy and --apply {} to execute.",
            plan.plan_id
        );
    }
    Ok(())
}

pub fn gc(args: ProjectArgs) -> anyhow::Result<()> {
    let result = SessionCoordinator::new(&args.project, None)?.history_gc_resume()?;
    print_gc(result, args.json)
}

fn print_gc(result: HistoryGcResult, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "Completed {} versions and {} abandoned captures ({} logical bytes); resumed: {}",
            result.removed_version_ids.len(),
            result.removed_capture_ids.len(),
            result.removed_logical_bytes,
            result.resumed
        );
        println!("Logical bytes removed do not measure physical space reclaimed.");
    }
    Ok(())
}

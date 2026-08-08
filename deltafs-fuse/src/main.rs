#[cfg(not(feature = "fuse"))]
use anyhow::bail;
#[cfg(feature = "fuse")]
use anyhow::Context;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "deltafs-fuse",
    version,
    about = "Standalone DeltaFS FUSE filesystem"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect whether this host can mount DeltaFS.
    Doctor {
        /// Emit structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Mount a source directory through DeltaFS.
    Mount {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        mountpoint: PathBuf,
        #[arg(long)]
        undo_dir: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Doctor { json } => {
            let report = deltafs_fuse::doctor::inspect();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("os: {} ({})", report.os, report.arch);
                println!("fuse feature: {}", report.fuse_feature);
                println!("mount ready: {}", report.mount_ready);
                for note in report.notes {
                    println!("note: {note}");
                }
            }
            Ok(())
        }
        Command::Mount {
            source,
            mountpoint,
            undo_dir,
        } => mount(source, mountpoint, undo_dir),
    }
}

#[cfg(feature = "fuse")]
fn mount(source: PathBuf, mountpoint: PathBuf, undo_dir: PathBuf) -> Result<()> {
    deltafs_fuse::passthrough::mount(&source, &mountpoint, &undo_dir)
        .context("deltafs-fuse mount failed")
}

#[cfg(not(feature = "fuse"))]
fn mount(_source: PathBuf, _mountpoint: PathBuf, _undo_dir: PathBuf) -> Result<()> {
    bail!("this binary was built without FUSE support; rebuild with --features fuse")
}

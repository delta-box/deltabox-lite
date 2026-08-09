#[cfg(not(feature = "fuse"))]
use anyhow::bail;
#[cfg(feature = "fuse")]
use anyhow::Context;
use anyhow::Result;
use clap::{Parser, Subcommand};
use deltafs_fuse::control::{self, Command as ControlCommand};
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
        #[arg(long, visible_alias = "undo-dir")]
        state_dir: PathBuf,
    },
    /// Run a mounted filesystem and its local control endpoint.
    Serve(ServeArgs),
    /// Send a request to a running filesystem helper.
    Control(ControlArgs),
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    mountpoint: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    ready_file: PathBuf,
    #[arg(long, default_value_t = deltafs_fuse::engine::DEFAULT_QUOTA_BYTES)]
    quota_bytes: u64,
    #[arg(long)]
    reclaim_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ControlArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    token_file: PathBuf,
    #[command(subcommand)]
    command: ControlSubcommand,
}

#[derive(Debug, Subcommand)]
enum ControlSubcommand {
    Ping,
    Status,
    Checkpoint {
        #[arg(long)]
        label: Option<String>,
    },
    Diff {
        #[arg(long)]
        from: Option<u64>,
    },
    Commit {
        #[arg(long)]
        target: Option<u64>,
    },
    Abort {
        #[arg(long)]
        target: Option<u64>,
    },
    Reclaim,
    Shutdown,
}

impl From<ControlSubcommand> for ControlCommand {
    fn from(command: ControlSubcommand) -> Self {
        match command {
            ControlSubcommand::Ping => Self::Ping,
            ControlSubcommand::Status => Self::Status,
            ControlSubcommand::Checkpoint { label } => Self::Checkpoint { label },
            ControlSubcommand::Diff { from } => Self::Diff { from },
            ControlSubcommand::Commit { target } => Self::Commit { target },
            ControlSubcommand::Abort { target } => Self::Abort { target },
            ControlSubcommand::Reclaim => Self::Reclaim,
            ControlSubcommand::Shutdown => Self::Shutdown,
        }
    }
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
            state_dir,
        } => mount(source, mountpoint, state_dir),
        Command::Serve(args) => serve(args),
        Command::Control(args) => {
            let token = std::fs::read_to_string(args.token_file)?;
            let result = control::request(&args.socket, token.trim(), args.command.into())?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

#[cfg(feature = "fuse")]
fn mount(source: PathBuf, mountpoint: PathBuf, state_dir: PathBuf) -> Result<()> {
    deltafs_fuse::passthrough::mount(&source, &mountpoint, &state_dir)
        .context("deltafs-fuse mount failed")
}

#[cfg(feature = "fuse")]
fn serve(args: ServeArgs) -> Result<()> {
    deltafs_fuse::helper::run(deltafs_fuse::helper::Config {
        source: args.source,
        mountpoint: args.mountpoint,
        state_dir: args.state_dir,
        socket: args.socket,
        token_file: args.token_file,
        ready_file: args.ready_file,
        quota_bytes: args.quota_bytes,
        reclaim_dir: args.reclaim_dir,
    })
}

#[cfg(not(feature = "fuse"))]
fn mount(_source: PathBuf, _mountpoint: PathBuf, _state_dir: PathBuf) -> Result<()> {
    bail!("this binary was built without FUSE support; rebuild with --features fuse")
}

#[cfg(not(feature = "fuse"))]
fn serve(_args: ServeArgs) -> Result<()> {
    bail!("this binary was built without FUSE support; rebuild with --features fuse")
}

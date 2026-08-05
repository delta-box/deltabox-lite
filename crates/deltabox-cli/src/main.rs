use anyhow::Context;
use clap::{Parser, Subcommand};
use deltabox_core::backend::DefaultBackend;
use deltabox_core::protocol::{
    AbortParams, CheckpointParams, CommitParams, DiffParams, ExecParams, Request,
};
use deltabox_core::server::Server;
use deltabox_core::session::SessionManager;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "deltabox",
    version,
    about = "deltabox-lite sandbox runtime: agent-native sessions with diff/commit/abort"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create and manage sandbox sessions.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Run a command inside a sandbox session.
    Exec(ExecArgs),
    /// Create a checkpoint in a session.
    Checkpoint(CheckpointArgs),
    /// Show changes in a session.
    Diff(DiffArgs),
    /// Commit pending changes to the real project.
    Commit(CommitArgs),
    /// Abort pending changes back to a checkpoint.
    Abort(AbortArgs),
    /// Serve the JSON-RPC protocol over stdio.
    Serve,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Start a new sandbox session in a project.
    Start(StartArgs),
    /// Stop a running session.
    Stop(StopArgs),
    /// Show session status.
    Status(StatusArgs),
}

#[derive(clap::Args)]
struct StartArgs {
    project: PathBuf,
    #[arg(long)]
    policy: Option<PathBuf>,
}

#[derive(clap::Args)]
struct StopArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
}

#[derive(clap::Args)]
struct StatusArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
}

#[derive(clap::Args)]
struct ExecArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    env: Vec<String>,
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(clap::Args)]
struct CheckpointArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    label: Option<String>,
}

#[derive(clap::Args)]
struct DiffArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: Option<String>,
}

#[derive(clap::Args)]
struct CommitArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    checkpoint: Option<String>,
}

#[derive(clap::Args)]
struct AbortArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    checkpoint: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Session { cmd } => match cmd {
            SessionCmd::Start(args) => {
                let mut manager =
                    SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
                let info = manager.start(args.policy.as_deref())?;
                println!("{}", info.session_id);
            }
            SessionCmd::Stop(args) => {
                let mut manager =
                    SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
                let info = manager.stop(&args.session)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            SessionCmd::Status(args) => {
                let manager =
                    SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
                let info = manager.status(&args.session)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
        },
        Command::Exec(args) => {
            let mut manager =
                SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
            let env = if args.env.is_empty() {
                None
            } else {
                Some(
                    args.env
                        .iter()
                        .map(|e| split_env(e))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                )
            };
            let result = manager.exec(ExecParams {
                session_id: args.session,
                argv: args.command,
                cwd: args.cwd,
                env,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Checkpoint(args) => {
            let mut manager =
                SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
            let info = manager.checkpoint(CheckpointParams {
                session_id: args.session,
                label: args.label,
            })?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::Diff(args) => {
            let mut manager =
                SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
            let result = manager.diff(DiffParams {
                session_id: args.session,
                from: args.from,
                to: args.to,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Commit(args) => {
            let mut manager =
                SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
            let result = manager.commit(CommitParams {
                session_id: args.session,
                checkpoint: args.checkpoint,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Abort(args) => {
            let mut manager =
                SessionManager::new(&args.project, Box::new(DefaultBackend::default()))?;
            let result = manager.abort(AbortParams {
                session_id: args.session,
                checkpoint: args.checkpoint,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Serve => {
            let project = std::env::current_dir()?;
            let mut server = Server::new(&project, Box::new(DefaultBackend::default()))?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for line in stdin.lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let request: Request = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("invalid request: {e}");
                        continue;
                    }
                };
                let response = server.handle(request);
                writeln!(out, "{}", serde_json::to_string(&response)?)?;
                out.flush()?;
            }
        }
    }
    Ok(())
}

fn split_env(s: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = s.split_once('=').context("env entries must be KEY=VALUE")?;
    Ok((k.to_string(), v.to_string()))
}

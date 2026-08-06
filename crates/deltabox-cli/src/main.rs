use anyhow::Context;
use clap::{Parser, Subcommand};
use deltabox_core::backend::DefaultBackend;
use deltabox_core::durable::DurableSessionManager;
use deltabox_core::protocol::{
    AbortParams, CheckpointParams, CommitParams, DiffParams, ExecParams, Request,
};
use deltabox_core::server::Server;
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
    /// Read a file from the sandbox view.
    Read(ReadArgs),
    /// Write a file through the sandbox.
    Write(WriteArgs),
    /// Create a checkpoint in a session.
    Checkpoint(CheckpointArgs),
    /// Show changes in a session.
    Diff(DiffArgs),
    /// Commit pending changes to the real project.
    Commit(CommitArgs),
    /// Abort pending changes back to a checkpoint.
    Abort(AbortArgs),
    /// Run the in-sandbox worker loop (spawned by the Linux backend).
    #[command(hide = true)]
    Worker(WorkerArgs),
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
    /// Deprecated: service-free sessions are durable by default.
    #[arg(long, hide = true)]
    durable: bool,
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
    label: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    env: Vec<String>,
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(clap::Args)]
struct ReadArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    path: PathBuf,
}

#[derive(clap::Args)]
struct WriteArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    path: PathBuf,
    #[arg(long)]
    content_base64: Option<String>,
    #[arg(long)]
    content: Option<String>,
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
    json: bool,
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

#[derive(clap::Args)]
struct WorkerArgs {
    #[arg(long)]
    root: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Session { cmd } => match cmd {
            SessionCmd::Start(args) => {
                let _ = args.durable;
                let manager = DurableSessionManager::new(&args.project, None)?;
                let info = manager.start(args.policy.as_deref())?;
                println!("{}", info.session_id);
            }
            SessionCmd::Stop(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                let info = manager.stop(&args.session)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            SessionCmd::Status(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                let info = manager.status(&args.session)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
        },
        Command::Exec(args) => {
            let manager = DurableSessionManager::new(&args.project, None)?;
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
                label: args.label,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Read(args) => {
            use std::io::Write as _;
            let manager = DurableSessionManager::new(&args.project, None)?;
            let result = manager.read_file(&args.session, &args.path)?;
            let bytes = base64_decode(&result.content_base64)?;
            std::io::stdout().lock().write_all(&bytes)?;
        }
        Command::Write(args) => {
            let bytes = match (args.content_base64, args.content) {
                (Some(b64), _) => base64_decode(&b64)?,
                (None, Some(text)) => text.into_bytes(),
                (None, None) => {
                    anyhow::bail!("provide --content or --content-base64")
                }
            };
            let manager = DurableSessionManager::new(&args.project, None)?;
            manager.write_file(&args.session, &args.path, bytes)?;
            println!("written");
        }
        Command::Checkpoint(args) => {
            let manager = DurableSessionManager::new(&args.project, None)?;
            let info = manager.checkpoint(CheckpointParams {
                session_id: args.session,
                label: args.label,
            })?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::Diff(args) => {
            let manager = DurableSessionManager::new(&args.project, None)?;
            let result = manager.diff(DiffParams {
                session_id: args.session,
                from: args.from,
                to: args.to,
            })?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print!("{}", deltabox_core::changeset::render_diff_text(&result));
            }
        }
        Command::Commit(args) => {
            let manager = DurableSessionManager::new(&args.project, None)?;
            let result = manager.commit(CommitParams {
                session_id: args.session,
                checkpoint: args.checkpoint,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Abort(args) => {
            let manager = DurableSessionManager::new(&args.project, None)?;
            let result = manager.abort(AbortParams {
                session_id: args.session,
                checkpoint: args.checkpoint,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Worker(args) => {
            deltabox_core::worker::run_worker(&args.root)?;
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

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

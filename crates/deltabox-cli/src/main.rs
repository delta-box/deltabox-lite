use anyhow::Context;
use clap::{Parser, Subcommand};
use deltabox_core::backend::DefaultBackend;
use deltabox_core::durable::DurableSessionManager;
use deltabox_core::protocol::{
    AbortParams, CheckpointParams, CommitParams, DiffParams, ExecParams, Request,
    ReviewBeginParams, ReviewKind,
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
    /// Manage a persistent task/tool review scope.
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
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

#[derive(Subcommand)]
enum ReviewCmd {
    /// Begin a review scope, preserving an existing skipped scope.
    Begin(ReviewBeginArgs),
    /// Show the active review scope.
    Status(SessionProjectArgs),
    /// Show changes since the active review scope began.
    Diff(ReviewDiffArgs),
    /// Commit pending changes and close the active scope.
    Commit(SessionProjectArgs),
    /// Abort to the active scope and close it.
    Abort(SessionProjectArgs),
    /// Keep changes pending and preserve the original rollback point.
    Skip(SessionProjectArgs),
}

#[derive(clap::Args)]
struct SessionProjectArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
}

#[derive(clap::Args)]
struct ReviewBeginArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long, value_parser = ["task", "tool", "risky"])]
    kind: String,
    #[arg(long)]
    label: Option<String>,
}

#[derive(clap::Args)]
struct ReviewDiffArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    json: bool,
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
    /// Read raw file content from stdin (avoids command-line size limits).
    #[arg(long)]
    stdin: bool,
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
    #[arg(long)]
    runtime: PathBuf,
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
            let bytes = if args.stdin {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut bytes)?;
                bytes
            } else {
                match (args.content_base64, args.content) {
                    (Some(b64), _) => base64_decode(&b64)?,
                    (None, Some(text)) => text.into_bytes(),
                    (None, None) => {
                        anyhow::bail!("provide --content or --content-base64")
                    }
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
        Command::Review { cmd } => match cmd {
            ReviewCmd::Begin(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                let kind = match args.kind.as_str() {
                    "task" => ReviewKind::Task,
                    "tool" => ReviewKind::Tool,
                    "risky" => ReviewKind::Risky,
                    _ => unreachable!("clap validates review kind"),
                };
                let active = manager.begin_review(ReviewBeginParams {
                    session_id: args.session,
                    kind,
                    label: args.label,
                })?;
                println!("{}", serde_json::to_string_pretty(&active)?);
            }
            ReviewCmd::Status(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_status(&args.session)?)?
                );
            }
            ReviewCmd::Diff(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                let diff = manager.review_diff(&args.session)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&diff)?);
                } else {
                    print!("{}", deltabox_core::changeset::render_diff_text(&diff));
                }
            }
            ReviewCmd::Commit(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_commit(&args.session)?)?
                );
            }
            ReviewCmd::Abort(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_abort(&args.session)?)?
                );
            }
            ReviewCmd::Skip(args) => {
                let manager = DurableSessionManager::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_skip(&args.session)?)?
                );
            }
        },
        Command::Worker(args) => {
            deltabox_core::worker::run_worker(&args.root, &args.runtime)?;
        }
        Command::Serve => {
            let project = std::env::current_dir()?;
            let mut server = Server::new(&project, Box::new(default_backend()))?;
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn default_backend() -> DefaultBackend {
    DefaultBackend::default()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn default_backend() -> DefaultBackend {
    DefaultBackend
}

fn split_env(s: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = s.split_once('=').context("env entries must be KEY=VALUE")?;
    Ok((k.to_string(), v.to_string()))
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

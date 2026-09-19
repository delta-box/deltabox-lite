use anyhow::Context;
use clap::{Parser, Subcommand};
use deltabox_core::backend::DefaultBackend;
use deltabox_core::coordinator::SessionCoordinator;
use deltabox_core::protocol::{
    AbortParams, CheckpointParams, CommitParams, DiffParams, DurabilityMode, ExecParams,
    ExecutionProfile, FilesystemMode, Request, ReviewBeginParams, ReviewKind,
};
use deltabox_core::server::Server;
use std::io::{BufRead, IsTerminal, Write};
use std::num::NonZeroU64;
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
    /// Run one command in a temporary transaction, then commit or abort it.
    Run(RunArgs),
    /// Run an argv command inside a sandbox session; use `sh -c` for shell syntax.
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
    /// Fold sibling publications into the session: refresh the baseline and
    /// materialize `.deltabox-theirs` copies for conflicting paths.
    Sync(SyncArgs),
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
    /// Run a session-scoped filesystem helper.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    FsHelper(FsHelperArgs),
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    MacosEvents(MacosEventsArgs),
    /// Run a session-scoped persistent command executor.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[command(hide = true)]
    CommandHelper(CommandHelperArgs),
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
    /// Print the complete session record instead of only the session ID.
    #[arg(long)]
    json: bool,
    #[arg(long)]
    policy: Option<PathBuf>,
    #[arg(long, value_parser = parse_filesystem_mode)]
    fs_mode: Option<FilesystemMode>,
    #[arg(long)]
    require_fs_mode: bool,
    /// Require CB-CoW and wait for ordered storage barriers before operations return.
    #[arg(long)]
    durable: bool,
    /// Batch concurrent strict journal barriers for up to this many milliseconds.
    #[arg(long, requires = "durable")]
    group_commit_ms: Option<NonZeroU64>,
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
    /// Filesystem breadth for this command only: workspace-write or read-write.
    #[arg(long, value_parser = parse_execution_profile)]
    execution_profile: Option<ExecutionProfile>,
    #[arg(long)]
    env: Vec<String>,
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(clap::Args)]
struct RunArgs {
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[arg(long)]
    policy: Option<PathBuf>,
    #[arg(long, value_parser = parse_filesystem_mode)]
    fs_mode: Option<FilesystemMode>,
    #[arg(long)]
    require_fs_mode: bool,
    /// Require CB-CoW and wait for ordered storage barriers before operations return.
    #[arg(long)]
    durable: bool,
    /// Batch concurrent strict journal barriers for up to this many milliseconds.
    #[arg(long, requires = "durable")]
    group_commit_ms: Option<NonZeroU64>,
    /// Commit the command's filesystem changes without prompting.
    #[arg(long, conflicts_with = "abort")]
    commit: bool,
    /// Abort the command's filesystem changes without prompting.
    #[arg(long, conflicts_with = "commit")]
    abort: bool,
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
    /// On a host conflict, fold sibling publications and materialize
    /// `.deltabox-theirs` drafts before returning, in this same invocation:
    /// the baseline is already refreshed when the conflict is reported.
    #[arg(long)]
    sync: bool,
    /// With --sync: after each fold, retry the commit so the session's own
    /// version of every conflicted path wins. Bounded; exits non-zero with a
    /// fresh baseline if siblings keep landing first.
    #[arg(long, requires = "sync")]
    ours: bool,
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(clap::Args)]
struct CommandHelperArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    ready_file: PathBuf,
}

#[cfg(target_os = "macos")]
#[derive(clap::Args)]
struct MacosEventsArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    live: PathBuf,
    #[arg(long)]
    since: u64,
}

#[cfg(target_os = "linux")]
#[derive(clap::Args)]
struct FsHelperArgs {
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
    #[arg(long)]
    quota_bytes: u64,
    #[arg(long)]
    durable: bool,
    #[arg(long)]
    group_commit_ms: Option<NonZeroU64>,
    #[arg(long)]
    reclaim_dir: Option<PathBuf>,
}


/// On a structured commit conflict, list every conflicting path on stderr so
/// callers (humans and agents) see the whole reconciliation surface at once.
#[derive(clap::Args)]
struct SyncArgs {
    session: String,
    #[arg(long, default_value = ".")]
    project: std::path::PathBuf,
}

fn render_conflict(error: &deltabox_core::error::Error) {
    if let deltabox_core::error::Error::ProjectConflict(report) = error {
        eprintln!("deltabox: commit blocked: project changed outside deltabox");
        for conflict in &report.conflicts {
            let mark = match conflict.host_change {
                deltabox_core::changeset::ChangeKind::Added => "A",
                deltabox_core::changeset::ChangeKind::Modified => "M",
                deltabox_core::changeset::ChangeKind::Deleted => "D",
            };
            eprintln!("  {mark} {} (changed on the host)", conflict.path);
        }
        let n = report.conflicts.len();
        let suffix = if report.truncated { ", list truncated" } else { "" };
        eprintln!("deltabox: {n} conflicting path(s){suffix}; nothing was committed.");
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Session { cmd } => match cmd {
            SessionCmd::Start(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                let info = manager.start_with_durability(
                    args.policy.as_deref(),
                    args.fs_mode,
                    args.require_fs_mode,
                    if args.durable {
                        DurabilityMode::Strict
                    } else {
                        DurabilityMode::Fast
                    },
                    args.group_commit_ms.map(NonZeroU64::get),
                )?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    println!("{}", info.session_id);
                    print_start_notice(&info);
                }
            }
            SessionCmd::Stop(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                let info = manager.stop(&args.session)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            SessionCmd::Status(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                let info = manager.status(&args.session)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
        },
        Command::Run(args) => {
            let exit_code = run_transaction(args)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Command::Exec(args) => {
            validate_exec_command(&args.command)?;
            let manager = SessionCoordinator::new(&args.project, None)?;
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
                execution_profile: args.execution_profile.unwrap_or_default(),
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Read(args) => {
            use std::io::Write as _;
            let manager = SessionCoordinator::new(&args.project, None)?;
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
            let manager = SessionCoordinator::new(&args.project, None)?;
            manager.write_file(&args.session, &args.path, bytes)?;
            println!("written");
        }
        Command::Checkpoint(args) => {
            let manager = SessionCoordinator::new(&args.project, None)?;
            let info = manager.checkpoint(CheckpointParams {
                session_id: args.session,
                label: args.label,
            })?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::Diff(args) => {
            let manager = SessionCoordinator::new(&args.project, None)?;
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
            let manager = SessionCoordinator::new(&args.project, None)?;
            // --sync keeps the reject -> fold -> retry recovery inside one
            // invocation: no process respawn and no agent turnaround between
            // the rejection and the refreshed baseline, which is where a
            // sibling would otherwise land first and force another round.
            const OURS_MAX_RETRIES: u64 = 10;
            let mut sync_retries: u64 = 0;
            loop {
                let result = manager.commit(CommitParams {
                    session_id: args.session.clone(),
                    checkpoint: args.checkpoint.clone(),
                    ours: args.ours,
                });
                match result {
                    Ok(result) => {
                        if sync_retries > 0 {
                            let mut value = serde_json::to_value(&result)?;
                            value["sync_retries"] = sync_retries.into();
                            println!("{}", serde_json::to_string_pretty(&value)?);
                        } else {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                        break;
                    }
                    Err(error @ deltabox_core::error::Error::ProjectConflict(_))
                        if args.sync =>
                    {
                        let synced = match manager.sync(&args.session) {
                            Ok(synced) => synced,
                            Err(sync_error) => {
                                render_conflict(&error);
                                eprintln!("deltabox: --sync fallback failed: {sync_error}");
                                return Err(error.into());
                            }
                        };
                        // Retry only while folding made progress: with the
                        // in-lock `ours` policy a surviving conflict means a
                        // stranger change (or an unendorsed path), which no
                        // amount of retrying resolves.
                        if args.ours
                            && synced.folded_publications > 0
                            && sync_retries < OURS_MAX_RETRIES
                        {
                            sync_retries += 1;
                            continue;
                        }
                        // Not committed, but the baseline is already folded
                        // and the theirs drafts exist: the caller can merge
                        // and recommit without another sync round-trip.
                        let mut value = serde_json::to_value(&synced)?;
                        value["committed"] = serde_json::Value::Bool(false);
                        if sync_retries > 0 {
                            value["sync_retries"] = sync_retries.into();
                        }
                        println!("{}", serde_json::to_string_pretty(&value)?);
                        render_conflict(&error);
                        return Err(error.into());
                    }
                    Err(error) => {
                        render_conflict(&error);
                        return Err(error.into());
                    }
                }
            }
        }
        Command::Sync(args) => {
            let manager = SessionCoordinator::new(&args.project, None)?;
            let result = manager.sync(&args.session)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Abort(args) => {
            let manager = SessionCoordinator::new(&args.project, None)?;
            let result = manager.abort(AbortParams {
                session_id: args.session,
                checkpoint: args.checkpoint,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Review { cmd } => match cmd {
            ReviewCmd::Begin(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
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
                let manager = SessionCoordinator::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_status(&args.session)?)?
                );
            }
            ReviewCmd::Diff(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                let diff = manager.review_diff(&args.session)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&diff)?);
                } else {
                    print!("{}", deltabox_core::changeset::render_diff_text(&diff));
                }
            }
            ReviewCmd::Commit(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_commit(&args.session)?)?
                );
            }
            ReviewCmd::Abort(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_abort(&args.session)?)?
                );
            }
            ReviewCmd::Skip(args) => {
                let manager = SessionCoordinator::new(&args.project, None)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manager.review_skip(&args.session)?)?
                );
            }
        },
        Command::Worker(args) => {
            deltabox_core::worker::run_worker(&args.root, &args.runtime)?;
        }
        #[cfg(target_os = "linux")]
        Command::FsHelper(args) => {
            deltafs_fuse::helper::run(deltafs_fuse::helper::Config {
                source: args.source,
                mountpoint: args.mountpoint,
                state_dir: args.state_dir,
                socket: args.socket,
                token_file: args.token_file,
                ready_file: args.ready_file,
                quota_bytes: args.quota_bytes,
                durability: if args.durable {
                    deltafs_fuse::engine::Durability::Strict
                } else {
                    deltafs_fuse::engine::Durability::ProcessCrash
                },
                group_commit_ms: args.group_commit_ms.map(NonZeroU64::get),
                reclaim_dir: args.reclaim_dir,
                allow_other: false,
            })?;
        }
        #[cfg(target_os = "macos")]
        Command::MacosEvents(args) => {
            deltafs_fuse::macos::run_event_helper(
                &args.socket,
                &args.live,
                args.since,
                std::time::Duration::from_secs(30),
            )?;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Command::CommandHelper(args) => {
            deltabox_core::command_executor::run(deltabox_core::command_executor::Config {
                endpoint: deltabox_core::command_executor::Endpoint::new(
                    args.socket,
                    args.token_file,
                    args.ready_file,
                ),
                worker_bin: std::env::current_exe()?,
            })?;
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

fn validate_exec_command(argv: &[String]) -> anyhow::Result<()> {
    if argv.len() == 1 && argv[0].chars().any(char::is_whitespace) {
        anyhow::bail!(
            "deltabox exec accepts an argv command, not a shell command string; use `-- sh -c '...'` and keep redirects or pipes inside the quoted script"
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RunAction {
    Commit,
    Abort,
}

fn run_transaction(args: RunArgs) -> anyhow::Result<i32> {
    validate_exec_command(&args.command)?;
    let action = if args.commit {
        Some(RunAction::Commit)
    } else if args.abort {
        Some(RunAction::Abort)
    } else {
        None
    };
    let interactive = std::io::stdin().is_terminal();
    if action.is_none() && !interactive {
        anyhow::bail!("deltabox run requires --commit or --abort when stdin is not a terminal");
    }
    let env = if args.env.is_empty() {
        None
    } else {
        Some(
            args.env
                .iter()
                .map(|entry| split_env(entry))
                .collect::<anyhow::Result<Vec<_>>>()?,
        )
    };

    let manager = SessionCoordinator::new(&args.project, None)?;
    let info = manager.start_with_durability(
        args.policy.as_deref(),
        args.fs_mode,
        args.require_fs_mode,
        if args.durable {
            DurabilityMode::Strict
        } else {
            DurabilityMode::Fast
        },
        args.group_commit_ms.map(NonZeroU64::get),
    )?;
    print_start_notice(&info);
    let session_id = info.session_id;

    let outcome = (|| -> anyhow::Result<i32> {
        let result = manager.exec(ExecParams {
            session_id: session_id.clone(),
            argv: args.command,
            cwd: args.cwd,
            env,
            label: args.label,
            execution_profile: Default::default(),
        })?;
        std::io::stdout()
            .lock()
            .write_all(result.stdout.as_bytes())?;
        std::io::stderr()
            .lock()
            .write_all(result.stderr.as_bytes())?;

        let diff = manager.diff(DiffParams {
            session_id: session_id.clone(),
            from: None,
            to: None,
        })?;
        eprint!("{}", deltabox_core::changeset::render_diff_text(&diff));
        let action = match action {
            Some(action) => action,
            None => prompt_run_action()?,
        };
        match action {
            RunAction::Commit => {
                let committed = manager.commit(CommitParams {
                    session_id: session_id.clone(),
                    checkpoint: None,
                    ours: false,
                })?;
                eprintln!("deltabox: committed {} path(s)", committed.applied);
            }
            RunAction::Abort => {
                let aborted = manager.abort(AbortParams {
                    session_id: session_id.clone(),
                    checkpoint: None,
                })?;
                eprintln!("deltabox: aborted {} change(s)", aborted.restored);
            }
        }
        Ok(result.exit_code.unwrap_or(1))
    })();

    let cleanup = manager
        .stop(&session_id)
        .and_then(|_| manager.remove_stopped(&session_id));
    match (outcome, cleanup) {
        (Ok(exit_code), Ok(())) => Ok(exit_code),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("failed to clean temporary session: {cleanup}")))
        }
    }
}

fn prompt_run_action() -> anyhow::Result<RunAction> {
    eprint!("deltabox: [c]ommit or [a]bort? ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    match answer.trim() {
        "c" | "commit" => Ok(RunAction::Commit),
        "a" | "abort" => Ok(RunAction::Abort),
        _ => anyhow::bail!("expected `commit` or `abort`"),
    }
}

fn print_start_notice(info: &deltabox_core::protocol::SessionInfo) {
    if info.filesystem.resolved_fs_mode == FilesystemMode::CbcowThrough {
        eprintln!(
            "deltabox: host changes are immediate; commit accepts them and abort reverts them"
        );
    } else if let Some(reason) = &info.filesystem.fallback_reason {
        eprintln!("deltabox: requested filesystem is unavailable; using copy-private: {reason}");
    }
}

fn parse_filesystem_mode(value: &str) -> Result<FilesystemMode, String> {
    match value {
        "cbcow-through" => Ok(FilesystemMode::CbcowThrough),
        "cbcow-private" => Ok(FilesystemMode::CbcowPrivate),
        "copy-private" => Ok(FilesystemMode::CopyPrivate),
        _ => Err("expected cbcow-through, cbcow-private, or copy-private".into()),
    }
}

fn parse_execution_profile(value: &str) -> Result<ExecutionProfile, String> {
    match value {
        "workspace-write" => Ok(ExecutionProfile::WorkspaceWrite),
        "read-write" => Ok(ExecutionProfile::ReadWrite),
        _ => Err("expected workspace-write or read-write".into()),
    }
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

//! One-shot "raw Sandlock" runner used by the benchmarks in `bench/`.
//!
//! Runs one command under `sandlock-core` in its seccomp copy-on-write branch
//! mode (`workdir` = project). When the command exits, the branch is committed
//! into the project on success (or, with `--abort`, always discarded) and
//! aborted on failure. Every invocation is a fresh sandbox: there is no session
//! that outlives the command, which is the file-level copy-up baseline the
//! benchmarks compare against.
//!
//! The confinement mirrors `backend::linux_sandlock::build_sandbox` with the
//! default agent policy (same read/write roots, same network mode), minus the
//! deltabox worker, scratch home/tmp and deny patterns.
//!
//! Usage: raw_sandlock [--abort] [--stats <file>] <project> <cow-storage-dir> -- <argv>...
//!
//! `--stats` writes `{"run_ms", "finalize_ms", "upper_bytes"}` as JSON:
//! sandbox setup + command time, commit/abort time, and the allocated size of
//! the COW storage (the copied-up upper layer) just before commit/abort.

use sandlock_core::sandbox::BranchAction;
use sandlock_core::{Protection, Sandbox};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

fn allocated_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = meta.blocks() * 512;
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total += allocated_bytes(&entry.path());
            }
        }
    }
    total
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut abort = false;
    let mut stats: Option<String> = None;
    loop {
        match args.first().map(String::as_str) {
            Some("--abort") => {
                abort = true;
                args.remove(0);
            }
            Some("--stats") if args.len() > 1 => {
                stats = Some(args.remove(1));
                args.remove(0);
            }
            _ => break,
        }
    }
    let (project, storage, argv) = match args.iter().position(|a| a == "--") {
        Some(2) if args.len() > 3 => (args[0].clone(), args[1].clone(), args[3..].to_vec()),
        _ => {
            eprintln!(
                "usage: raw_sandlock [--abort] [--stats <file>] <project> <cow-storage-dir> -- <argv>..."
            );
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&storage).expect("create cow storage dir");

    let started = Instant::now();
    let mut sandbox = Sandbox::builder()
        .allow_degraded(Protection::FsIoctlDev)
        .allow_degraded(Protection::SignalScope)
        .allow_degraded(Protection::AbstractUnixSocketScope)
        .cwd(&project)
        .workdir(&project)
        .fs_storage(&storage)
        .on_exit(if abort {
            BranchAction::Abort
        } else {
            BranchAction::Commit
        })
        .on_error(BranchAction::Abort)
        .fs_read(&project)
        .fs_write(&project)
        .fs_read("/proc")
        .fs_read("/dev")
        .fs_write("/dev/null")
        .fs_write("/dev/full")
        .fs_read("/tmp")
        .fs_read_if_exists("/usr")
        .fs_read_if_exists("/bin")
        .fs_read_if_exists("/lib")
        .fs_read_if_exists("/lib64")
        .fs_read_if_exists("/etc")
        .fs_read_if_exists("/run")
        .fs_read_if_exists("/usr/local")
        .net_allow("*")
        .build()
        .expect("sandlock build");

    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    // Inherit stdio. `Sandbox::run` captures output in pipes it only drains
    // after the child exits, so a command printing more than a pipe buffer
    // (64 KiB, e.g. `grep -rn` over a source tree) blocks forever.
    let result = rt
        .block_on(sandbox.run_interactive(&argv))
        .expect("sandlock run");
    let ran = Instant::now();
    let upper_bytes = if stats.is_some() {
        allocated_bytes(Path::new(&storage))
    } else {
        0
    };
    // Sandlock applies on_exit/on_error (commit or abort of the COW branch)
    // in `Drop`, so the sandbox must be dropped before `process::exit`.
    let finalize_started = Instant::now();
    drop(sandbox);
    let finalized = Instant::now();
    if let Some(path) = stats {
        let json = format!(
            "{{\"run_ms\":{:.3},\"finalize_ms\":{:.3},\"upper_bytes\":{}}}\n",
            (ran - started).as_secs_f64() * 1e3,
            (finalized - finalize_started).as_secs_f64() * 1e3,
            upper_bytes
        );
        std::fs::write(path, json).expect("write stats");
    }
    std::process::exit(result.code().unwrap_or(1));
}

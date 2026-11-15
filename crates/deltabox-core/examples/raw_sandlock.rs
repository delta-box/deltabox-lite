//! One-shot "raw Sandlock" runner used by `bench/delta-driven/run.py`.
//!
//! Runs one command under `sandlock-core` in its seccomp copy-on-write branch
//! mode (`workdir` = project) and commits the branch back to the project when
//! the command exits successfully (aborts it otherwise). Every invocation is a
//! fresh sandbox: there is no session that outlives the command, which is the
//! baseline the README's "raw Sandlock" rows compare against.
//!
//! The confinement mirrors `backend::linux_sandlock::build_sandbox` with the
//! default agent policy (same read/write roots, same network mode), minus the
//! deltabox worker, scratch home/tmp and deny patterns.
//!
//! Usage: raw_sandlock <project> <cow-storage-dir> -- <argv>...

use sandlock_core::sandbox::BranchAction;
use sandlock_core::{Protection, Sandbox};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sep = args.iter().position(|a| a == "--");
    let (project, storage, argv) = match sep {
        Some(i) if i == 3 && args.len() > 4 => (&args[1], &args[2], &args[4..]),
        _ => {
            eprintln!("usage: raw_sandlock <project> <cow-storage-dir> -- <argv>...");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(storage).expect("create cow storage dir");

    let mut sandbox = Sandbox::builder()
        .allow_degraded(Protection::FsIoctlDev)
        .allow_degraded(Protection::SignalScope)
        .allow_degraded(Protection::AbstractUnixSocketScope)
        .cwd(project)
        .workdir(project)
        .fs_storage(storage)
        .on_exit(BranchAction::Commit)
        .on_error(BranchAction::Abort)
        .fs_read(project)
        .fs_write(project)
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
    let result = rt.block_on(sandbox.run(&argv)).expect("sandlock run");
    // Sandlock applies on_exit/on_error (commit or abort of the COW branch)
    // in `Drop`, so the sandbox must be dropped before `process::exit`.
    drop(sandbox);
    use std::io::Write;
    if let Some(out) = &result.stdout {
        let _ = std::io::stdout().write_all(out);
    }
    if let Some(err) = &result.stderr {
        let _ = std::io::stderr().write_all(err);
    }
    std::process::exit(result.code().unwrap_or(1));
}

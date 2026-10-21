<div align="center">

<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/deltabox-lite-logo.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/deltabox-lite-logo-light.svg">
    <img src="assets/deltabox-lite-logo-light.svg" width="300" alt="DeltaBox Lite">
  </picture>
</h1>

**Lightweight, agent-native sandboxing with diff · commit · abort.**

Give agents a confined workspace with explicit diff, commit, and abort semantics.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![npm](https://img.shields.io/npm/v/deltabox-lite?logo=npm)](https://www.npmjs.com/package/deltabox-lite)
[![Rust](https://img.shields.io/badge/rust-1.89+-orange.svg)](#)
[![Platform](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20Windows-lightgrey.svg)](#)
[![Pi](https://img.shields.io/badge/pi-deltabox-purple.svg)](https://github.com/delta-box/pi-deltabox)
[![DSH](https://img.shields.io/badge/dsh-deltabox-blue.svg)](https://github.com/delta-box/dsh-deltabox)

</div>

deltabox-lite is a standalone sandbox runtime for AI coding agents. Agents keep
full execution freedom inside a confined filesystem view, and every change
they make can be **reviewed**, **committed**, or **aborted** - per task or per
tool call. Linux defaults to a commit-biased FUSE view; macOS uses native APFS
clones and FSEvents; Windows uses a WinFsp view. Private modes keep changes
hidden from the host until commit.

Think of it as Git for agent sessions: less ceremony, more control, and no
anxiety about binary or large files.

## 📰 News

- **[2026/10/21]** Release `v0.1.4` uses `deltabox-lite-windows-x64` for the Windows npm package. It supersedes the incomplete `v0.1.3` publication; DSH `0.1.4` and Pi `0.1.3` now select runtime `0.1.4`.

- **[2026/10/21]** Release `v0.1.3`: **Windows x64** Support! CLI/Node prebuilds and npm packages, WinFsp sessions, `copy-private` sessions without WinFsp, and MXC network policy enforcement. Companion DSH `0.1.4` and Pi `0.1.3` add Windows installation and PowerShell integration.

- **[2026/10/19]** Added Windows WinFsp support with strict durability for private sessions, plus execution-scoped network approvals ([PR #33](https://github.com/delta-box/deltabox-lite/pull/33), [PR #35](https://github.com/delta-box/deltabox-lite/pull/35)).
- **[2026/10/16]** Open-sourced DeltaBox Lite and published the introduction “DeltaBox Lite: A Lightweight File Sandbox for Terminal AI Agents.”
- **[2026/10/14]** Released `v0.1.0` and `v0.1.1`, updated DSH / Pi integrations, and fixed `read-write` execution in macOS sessions.
- **[2026/09]** Expanded the Native Node API and execution profiles; added npm, Homebrew, an install script, and a multi-platform prebuilt release pipeline.
- **[2026/08]** Built Linux / macOS file sandboxing, the CB-CoW state engine, and the diff / commit / abort workflow; added DSH / Pi integrations and concurrent private sessions for multiple agents.

<details>
<summary>View full history</summary>

### 2026

- **[10/19]** Added Windows WinFsp sessions with `cbcow-through` and `cbcow-private` modes, and fixed access permissions for existing view directories ([PR #33](https://github.com/delta-box/deltabox-lite/pull/33)).
- **[10/19]** Added execution-scoped network approvals: a proxy restricts destination hosts and ports, and temporary grants expire when the command finishes ([PR #35](https://github.com/delta-box/deltabox-lite/pull/35)).
- **[10/16]** Open-sourced the project and published the introduction “DeltaBox Lite: A Lightweight File Sandbox for Terminal AI Agents.”
- **[10/15]** Added an installation prompt for AI agents and environment checks.
- **[10/14]** Released `v0.1.0` and `v0.1.1`, updated DSH / Pi plugins and installation guides, and fixed protected `read-write` execution in macOS through sessions ([PR #32](https://github.com/delta-box/deltabox-lite/pull/32)).
- **[09/22]** Added the npm launcher, tag-triggered multi-platform prebuilds, release tooling, Homebrew support, and an install script ([PR #29](https://github.com/delta-box/deltabox-lite/pull/29)).
- **[09/19]** Added embedded native execution and the Native Node API, and exposed execution profiles through the native API and CLI ([PR #22](https://github.com/delta-box/deltabox-lite/pull/22), [PR #23](https://github.com/delta-box/deltabox-lite/pull/23)).
- **[08/31]** Added concurrent private sessions for multiple agents with publish-lock arbitration, and optimized staging and digest computation before commit ([PR #4](https://github.com/delta-box/deltabox-lite/pull/4), [PR #14](https://github.com/delta-box/deltabox-lite/pull/14)).
- **[08/25]** Merged the native Pi plugin integration and optimized DeltaFS control-response transport ([PR #8](https://github.com/delta-box/deltabox-lite/pull/8), [PR #11](https://github.com/delta-box/deltabox-lite/pull/11)).
- **[08/18]** Merged the DSH plugin integration, moved default session state into the project's `.deltabox` directory, and fixed macOS session access after state relocation.
- **[08/15]** Added single-command transactions with `deltabox run --commit` / `--abort` to keep or discard changes.
- **[08/13]** Added a persistent session command executor that reuses the execution environment to reduce overhead across commands.
- **[08/10]** Added native macOS CB-CoW support using APFS and FSEvents, packaged the native runtime, and exposed structured session interfaces.
- **[08/09]** Added private CB-CoW and strict durability modes, and expanded Linux filesystem operation support.
- **[08/08]** Implemented the commit-biased state engine and integrated it with Linux FUSE.
- **[08/07]** Added the native macOS Seatbelt sandbox backend and began building the CB-CoW architecture.
- **[08/06]** Added durable sessions without a global service, persistent review state, crash recovery, and stronger sandbox policies.
- **[08/05]** Initialized the project with Linux Sandlock sessions, checkpoint / commit / abort, fast snapshots, incremental state handling, and readable diffs.

</details>

## Demo

Run a command, inspect its change set, and choose whether to keep or discard it:

![deltabox-lite transaction demo](assets/deltabox-lite.gif)

## Highlights

- **Native isolation, not a container** - Linux composes Sandlock/Landlock/
  seccomp with DeltaFS FUSE; macOS uses Seatbelt plus APFS clones and FSEvents;
  Windows uses MXC process security with WinFsp filesystem routing.
- **Service-free durable sessions** - Linux starts a session-scoped filesystem
  helper and macOS starts a session-scoped event helper automatically. Durable
  files remain authoritative across independent CLI processes; no global daemon
  or manual service startup is required.
- **Three filesystem modes** - `cbcow-through` makes changes visible
  immediately and makes commit an O(1) acceptance operation. `cbcow-private`
  runs the same DeltaFS engine over a private reflink/copy tree and publishes
  only touched paths at commit. `copy-private` remains the compatibility
  fallback.
- **Diff / commit / abort** - unified text diffs, binary and large-file
  markers, and a linear checkpoint chain. Abort restores the selected
  checkpoint; commit accepts the current state.
- **Review-scope savepoints** — task and tool adapters choose the correct abort
  target; normal commit and abort do not require a user-created checkpoint.
- **Agent adapters** — optional JSON-RPC over stdio for Pi and other
  integrations; see the [adapter protocol](docs/protocol.md).
- **Policy-first** — filesystem read/write rules, secret-path denial, network
  allow/audit, deny-all and allowlist modes, scrubbed child environments, and
  bounded writes/storage.

## Install

### Let AI agents install

Copy this prompt into an AI coding agent with terminal access:

```text
Install DeltaBox Lite by following the instructions at
https://raw.githubusercontent.com/delta-box/deltabox-lite/main/install-prompt.txt
Detect my environment, install the appropriate runtime and any DSH or Pi
integration I use, and verify the installation with the disposable smoke test.
```

The plain-text [installation prompt](install-prompt.txt) covers environment
checks, integration setup, and verification. You can also paste its contents
directly into your agent.

### Manual Install

Run the following on macOS or Linux to install the deltabox-lite CLI:

```
curl -fsSL https://raw.githubusercontent.com/delta-box/deltabox-lite/main/scripts/install.sh | sh
```

The prebuilt binaries ship for macOS (arm64, x64), Linux (x64, arm64), and
Windows (x64). They can be found on the
[GitHub release](https://github.com/delta-box/deltabox-lite/releases).

On Windows, install [WinFsp](https://winfsp.dev/rel/) first, then use PowerShell:

```powershell
npm install --global deltabox-lite
deltabox --version
```

Windows `deltabox` commands run directly from PowerShell. Pi's `bash` tool uses
Pi's own shell selection, which defaults to Git Bash on Windows. The CLI does
not require Git Bash. Sandboxed command execution needs Windows MXC process
security support; if it is unavailable, session startup reports an error.

deltabox-lite CLI can also be installed via the following package managers:

```bash
# Install using npm
npm install --global deltabox-lite
```

```bash
# Install using Homebrew
brew install delta-box/tap/deltabox
```

### Build from source

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

## Plugin

### DSH (DeepSeek Harness)

With DeepSeek Harness installed, add the plugin to your Web profile:

```bash
dsh plugin --profile web add dsh-deltabox
```

For Desktop, open **Plugins → Add plugin**, enter `dsh-deltabox`, and install
it. Alternatively, use Desktop's bundled CLI:

```bash
dsh plugin --profile desktop add dsh-deltabox
```

[DSH plugin guide](https://github.com/delta-box/dsh-deltabox#readme) for
configuration and review controls.

### Pi

Install Pi if needed, then add the extension:

```bash
npm install --global @earendil-works/pi-coding-agent
pi install npm:pi-deltabox
```

Start a new Pi session in
your project and run `/sandbox-status` to verify that the extension is active.
Use `/sandbox-review`, `/sandbox-commit`, and `/sandbox-abort` to review, keep,
or discard changes. See the
[Pi extension guide](https://github.com/delta-box/pi-deltabox#readme) for
configuration, including explicit command isolation settings.

Both integrations use FUSE on Linux for CB-CoW (`/dev/fuse` and `fusermount3`
or `fusermount`), APFS/FSEvents on macOS, and WinFsp on Windows.

## Quick start

For a first transaction, create a project with one ordinary file and discard a
command's changes. No package manifest or policy file is needed:

```bash
demo=$(mktemp -d)
cd "$demo"
printf 'original\n' > notes.txt

deltabox run --abort -- sh -c '
  printf "sandboxed\n" >> notes.txt
  printf "generated\n" > generated.txt
  find . -maxdepth 1 -type f -print
'

cat notes.txt
test ! -e generated.txt
```

The command runs inside the sandbox view, prints the diff, and then aborts. The
original `notes.txt` remains unchanged and `generated.txt` is removed.

To keep a result, use the same one-command interface with `--commit`:

```bash
deltabox run --commit -- sh -c 'printf "accepted by deltabox\n" > accepted.txt'
cat accepted.txt
```

`run` creates and cleans up a temporary transaction. Scripts and agent
integrations should select `--commit` or `--abort` explicitly; the child exit
status is preserved after that decision.

For multiple independent commands, start a persistent session. Commands run from
the project directory can omit `--project`; use it only when invoking Deltabox
from another directory:

```bash
cd /abs/path/my-project
session_id=$(deltabox session start .)
deltabox exec "$session_id" -- sh -c 'printf "step one\n" > step-one.txt'
deltabox exec "$session_id" -- sh -c 'printf "step two\n" > step-two.txt'
deltabox diff "$session_id"
deltabox commit "$session_id"
deltabox session stop "$session_id"
```

Session records live in `<project>/.deltabox` by default, so each project stays
self-contained. Set `DELTABOX_STATE_DIR=/path/to/state` to relocate session
records and engine state to an arbitrary path instead.

### Shell command syntax

`deltabox exec` and `deltabox run` execute the argv after `--` directly. Commands
that do not require shell parsing can be passed as ordinary arguments:

```bash
deltabox exec "$session_id" -- git status --short
deltabox exec "$session_id" -- curl -fL -o source.tar.gz https://example.com/source.tar.gz
```

Redirection, pipes, variable expansion, globbing, and command lists require an
explicit shell. Keep the complete script inside quotes so the outer host shell
cannot perform the filesystem operation before Deltabox starts:

```bash
deltabox exec "$session_id" -- sh -c 'printf "%s\n" hello > hello.txt'
deltabox run --abort -- sh -c 'printf "%s\n" trial > trial.txt'
deltabox exec "$session_id" -- sh -c 'find . -type f | sort > files.txt'
deltabox exec "$session_id" -- sh -c 'command-that-may-be-quiet >/dev/null 2>&1'
```

This form is wrong because the host shell handles `>` and creates `hello.txt`
outside the sandbox transaction:

```bash
deltabox exec "$session_id" -- echo hello > hello.txt
```

Quoting the whole command without `sh -c` is also wrong: Deltabox would treat
the complete string as an executable filename. Sandboxed commands do not receive
a terminal on stdin, so use non-interactive flags for tools such as Git.

### Choosing a filesystem mode

Linux and macOS request `cbcow-through` by default and visibly fall back to
`copy-private` if the native provider is unavailable. Windows supports
`cbcow-through` and `cbcow-private` with WinFsp. Select the mode
explicitly when a workflow requires it:

```bash
deltabox session start /abs/path/my-project --fs-mode copy-private
deltabox session start /abs/path/my-project --fs-mode cbcow-private --require-fs-mode
deltabox session start /abs/path/my-project --fs-mode cbcow-through --require-fs-mode
deltabox session start /abs/path/my-project --fs-mode cbcow-through --durable
deltabox session start /abs/path/my-project --fs-mode cbcow-through --durable --group-commit-ms 2
```

Fast mode is the default and recovers helper/process failures. `--durable`
adds ordered storage barriers; success from commit means changed files,
affected directories, and the acceptance marker are synchronized.
`--group-commit-ms` lets concurrent mutations share an `APPLIED` journal
barrier while preserving that per-operation success boundary.
Strict mode requires `cbcow-through` or `cbcow-private`; startup fails instead
of falling back to `copy-private` when the requested provider is unavailable.
On macOS, fast `cbcow-through` also assumes Deltabox is the exclusive project
writer. Fast `exec` defers FSEvents accounting to the next state operation;
strict mode closes a sentinel boundary before each result, detects external
writes, and refuses an unsafe abort. Use `cbcow-private` when host isolation is
required before commit.

## How it works

```mermaid
flowchart LR
    A[CLI or agent adapter] --> C[Session coordinator]
    C --> X[Lazy session command helper]
    X --> Y[Short-lived confined child]
    C --> T[Linux cbcow-through]
    T --> H[Session-scoped DeltaFS helper]
    H --> P[(Host-visible LIVE tree)]
    C --> V[Linux cbcow-private]
    V --> F[Session-scoped DeltaFS helper]
    F --> L[(Private LIVE tree)]
    L -->|touched-path commit| P
    C --> M[macOS native APFS plus FSEvents]
    M --> E[Session-scoped event helper]
    E --> P
    E --> L
    C --> R[copy-private fallback]
    R --> S[(Private working tree)]
    S -->|commit| P
```

On Linux with FUSE available, `session start` launches a helper that mounts the
host project through the commit-biased state engine. Writes update the host
tree through DeltaFS after recording rollback data; commit accepts the current
state and abort replays undo records. `cbcow-private` preserves the same fast
per-command path while keeping its `LIVE` tree hidden from the host until a
conflict-checked commit. In `copy-private`, each command updates a durable
private working tree and commit publishes it to the project.
On macOS, the same modes use APFS-cloned state and a restartable FSEvents helper
instead of a mount. The helper only keeps the event stream warm; persisted
cursors and indexes remain authoritative.
The first CB-CoW `exec` also launches a restartable command helper. Later CLI
operations reuse its warm Sandlock worker on Linux or Seatbelt profile/proxy on
macOS without making that helper authoritative session state. Durable files,
not a long-running process, remain the source of session truth. See [session
semantics](docs/session-semantics.md).

## Policy

`session start --policy policy.toml` accepts filesystem, network, and resource
limits. The default agent policy uses the `allow_audit` network mode, denies
common secret paths, passes only a small environment allowlist to commands,
caps one direct write at 16 MiB, and caps pending added/modified file content at
1 GiB. Network policy can instead deny all access or allow only explicit
host-and-port targets:

```toml
[network]
mode = "allowlist"
allow = ["registry.npmjs.org:443", "127.0.0.1:3000"]

[limits]
max_write_bytes = 16777216
max_storage_bytes = 1073741824
```

See [policy reference](docs/policy.md) for the full schema and enforcement
semantics, and [macOS compatibility](docs/macos.md) for path mapping and tool
support.

The policy is the session's standing breadth. A single command can request a
wider *execution profile* — `workspace-write` (default) or `read-write` — so a
`session.exec` call can reach the host filesystem without changing the session:

```bash
deltabox exec "$SESSION" --project /abs/path/my-project \
  --execution-profile read-write -- sh -c 'sudo apt-get install -y ripgrep'
```

Workspace changes stay visible to diff, commit, and abort under either profile.
Linux FUSE and private-workspace modes deny direct access to the backing project;
macOS `cbcow-through` writes the real project and tracks those changes with
APFS/FSEvents. Engine state and helper endpoints stay denied in both cases.
Host writes outside the workspace cannot be rolled back.

## Performance

Measured on x86-server (Linux 6.18, ext4, release build). Each row reports 50
no-op commands after five warmups. Deltabox uses independent CLI processes over
one `cbcow-through` session.

| Runner | 10 files p50 / p95 | 200 files p50 / p95 | 1000 files p50 / p95 |
|---|---:|---:|---:|
| `cbcow-through` | 13.7 / 15.5 ms | 14.3 / 15.4 ms | 13.2 / 14.7 ms |
| raw Sandlock | 8.1 / 9.4 ms | 8.2 / 9.0 ms | 8.1 / 8.9 ms |
| plain shell | 1.3 / 1.7 ms | 1.2 / 1.6 ms | 1.2 / 1.5 ms |

The no-op path does not scan or copy the project per command. Empty DeltaFS
checkpoint and commit operations remain below 1 ms in the scaling benchmark.

The delta-driven path can outperform repeated Sandlock copy-up work when a workload makes many small changes.

| Microbenchmark | `cbcow-through` p50 / p95 | raw Sandlock p50 / p95 | Faster path |
|---|---:|---:|---|
| One 4 KiB write in a 256 MiB existing file | 46.4 / 47.3 ms | 166.0 / 187.7 ms | `cbcow-through` |
| 16 commands, one 4 KiB write per 32 MiB file, one final commit | 509.0 / 509.6 ms | 4809.5 / 5761.2 ms | `cbcow-through` |

Raw Sandlock has no durable cross-command session, so the second row commits each one-shot command independently.

On macOS, the native provider materializes its APFS accepted base once at
session start. Fast commands do not clone or scan the project per invocation;
status, diff, checkpoint, commit, and abort close the FSEvents reconciliation
boundary. Private mode additionally materializes a private APFS `LIVE` tree and
therefore trades more startup and command-path work for pre-commit host
isolation.

## Project layout

- `crates/deltabox-core` — sessions, checkpoints, diff/commit/abort, policy,
  journal, platform backends.
- `crates/deltabox-cli` — the `deltabox` command-line binary.
- `crates/deltabox-node` - the `deltabox` Node-API bindings.
- `deltafs-fuse` — the standalone commit-biased state engine, FUSE adapter,
  helper, and local control protocol.
- `plugin` - dsh and pi plugin
- `npm` - npm package
- `docs/policy.md` — policy schema and enforcement boundaries.
- `docs/macos.md` — macOS backend behavior and compatibility matrix.


## Citation

If you find deltabox-lite useful, please cite the original paper of DeltaBox (which is based on microVM) or this github repo:

```bibtex
@misc{dong2026deltaboxscalingstatefulai,
      title={DeltaBox: Scaling Stateful AI Agents with Millisecond-Level Sandbox Checkpoint/Rollback}, 
      author={Yunpeng Dong and Jingkai He and Shiqi Liu and Yuze Hou and Dong Du and Zhonghu Xu and Si Yu and Baochuan Yang and Yubin Xia and Haibo Chen},
      year={2026},
      eprint={2605.22781},
      archivePrefix={arXiv},
      primaryClass={cs.OS},
      url={https://arxiv.org/abs/2605.22781}, 
}
```


## License

Apache-2.0.

## Acknowledgments

This project utilizes excellent work of existing sandbox projects like **[`sandlock`](https://github.com/multikernel/sandlock)**.


**⭐ Star this repo if you find deltabox-lite helpful!**


# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/).

Each release is cut with `node scripts/release.mjs <version>`, which moves the
Unreleased section below into the new version's section.

## [Unreleased]

### Added

- Session ownership for embedded hosts. Sessions started through the Node
  addon (`CoordinatorOptions::attach_sessions`) are now *attached*: the host
  process holds an OS lock (`owner.lock`) for the session's life, and the
  kernel releases it however the process dies, including on SIGKILL. CLI
  sessions stay *detached* and behave as before.
- Node API: `adoptSession(sessionId)` takes over a session whose owner died
  and keeps its pending changes, so the user can commit or abort them later.
  `orphanedSessions()` lists those sessions. The Rust equivalents are
  `SessionCoordinator::adopt` and `orphaned_sessions`.

### Fixed

- A through session whose host was killed (SIGKILL, crash, OOM) no longer
  holds the project lease indistinguishably from a live one. Admission now
  reports `session orphaned: <id> lost its owner process; adopt it ...`
  instead of `session busy ... stop or recover it first`. The orphan keeps
  its lease and its changes until it is adopted and stopped; it is never
  reclaimed or rolled back implicitly. This covers Linux, macOS and Windows.

## [0.2.0] - 2026-11-06

### Added

- Node API: `NativeDeltabox.exec(..., execId)` takes an optional trailing id,
  and the new `cancelExec(execId)` ends that command's whole process tree
  (it returns whether such an execution was running). A cancel that arrives
  before the command has started is not lost. Sandboxed commands reject with
  `command cancelled`; unsandboxed ones resolve with a non-zero result. Hosts
  that want a deadline or an abort button should use this; see the typings.
- `deltabox_core::cancel`: embedders can open a `cancel::scope()` on the thread
  that runs a command and call `CancelHandle::cancel()` from another thread.

### Changed

- A command's lifetime is now bound to the request that started it, and
  Deltabox no longer limits how long a command may run. The hard-coded
  10-minute (`600s`) limit is gone from the Linux, macOS, Windows (MXC) and
  trusted backends, and the command-helper client no longer gives up after
  615 s. A command runs until it exits or its request goes away. Callers that
  relied on the limit as a safety net must enforce their own deadline by
  cancelling.
- Ctrl-C, a closed terminal, `kill`, or a crashed host on `deltabox exec` or
  `run` now ends the command and its children instead of orphaning them. In
  helper-routed modes (for example Linux `cbcow-through`) the command-helper
  watches the client connection, kills the command's process group on hangup,
  waits for it to die, releases the operation lock, and retires the warm
  runtime. In in-process modes the CLI turns SIGINT, SIGTERM and SIGHUP into
  a cancel (a second signal exits immediately). On Linux the worker's
  parent-death signal is now SIGTERM, so a SIGKILLed CLI also takes its
  command down. On macOS, which has no parent-death signal, a SIGKILLed CLI in
  an in-process mode can still orphan its command. Descendants that move
  themselves into a new session or process group (`setsid`, daemonizing) are
  not reached.
- Windows: the MXC sandbox runs without a script timeout and a cancel ends
  its job object; unsandboxed commands run in a kill-on-close job object.
- The aarch64 macOS CI and prebuild jobs now run on `macos-15`; GitHub no
  longer accepts the `macos-14` configuration for this repository.

### Removed

- The `session command timed out` error for running commands, and the
  `wait-timeout` dependency.

## [0.1.5] - 2026-10-30

### Fixed

- Start macOS native sessions on large projects in a fraction of the time. On
  a 76,741-entry, 4.7 GB project, `session start` went from 43 s to under 4 s
  in through mode and from 85 s to under 8 s in private mode (Apple M4, macOS
  26.5.1). The accepted base and private `LIVE` tree are cloned with one APFS
  `clonefile` call per top-level entry, then checked against a scan of the
  project and repaired where the clone differs, instead of being cloned and
  re-stamped one entry at a time (#34).

## [0.1.4] - 2026-10-21

### Changed

- Publish the Windows x64 runtime as `deltabox-lite-windows-x64`, retaining
  npm's `win32` platform selector. Runtime 0.1.3 publication was incomplete
  because npm rejected the previous new Windows package name.

## [0.1.3] - 2026-10-21

### Added

- Windows x64 CLI and Node addon prebuilds, npm installation, and release CI.
- Windows MXC `copy-private` sessions without WinFsp, with private changes,
  commit/abort, and interrupted-operation recovery.
- Windows MXC network deny/allowlist enforcement and execution-scoped proxy
  approvals, including hostname and loopback destination handling.
- WinFsp CB-CoW support for trusted embedded commands used by Pi's default
  native transport.

### Fixed

- Retry Windows stopped-session cleanup while WinFsp releases its view.

## [0.1.2] - 2026-10-20

### Added

- Execution-scoped network approvals that restrict destination hosts and ports
  and expire when the command finishes.
- Environment checks and updated installation guidance for the CLI and plugins.

### Fixed

- Detect same-size file changes with unchanged timestamps during Linux
  incremental snapshot copies.
- Reject rooted and drive-prefixed paths in worker file writes.

## [0.1.1] - 2026-10-14

### Fixed

- Allow sandboxed `read-write` execution in macOS `cbcow-through` sessions
  while protecting rollback baselines, metadata, tokens, and helper endpoints.
- Describe working-directory conflicts accurately when an isolated workspace
  overlaps its denied backing project. Linux FUSE isolation is unchanged.

## [0.1.0] - 2026-10-14

### Added

- Prebuilt release tarballs (`deltabox-<version>-<target>.tar.gz`) with
  `.sha256` sidecars for manual installs.
- A Homebrew formula (`brew install ddnirvana/tap/deltabox`) kept current by
  `scripts/update-homebrew-formula.mjs`.
- `scripts/install.sh`, a one-line installer for macOS and Linux.
- `scripts/release.mjs`, which cuts a release by bumping every version site in
  lockstep, rolling this changelog, and tagging.
- `bin/deltabox.js`, a launcher in the `deltabox-lite` npm wrapper that puts
  the `deltabox` command on the PATH after a global install.
- `RELEASING.md`, documenting the release pipeline for maintainers.

### Changed

- The prebuilds workflow now runs on tag pushes, publishes from a single
  restartable job, and publishes the GitHub release only after every platform
  succeeds.

### Fixed

- The macOS embedded fast path failed to compile after the publish-lock merge
  (`deltabox-core`).
- `npm install --global deltabox-lite` did not expose the `deltabox` command.

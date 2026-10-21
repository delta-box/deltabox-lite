# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/).

Each release is cut with `node scripts/release.mjs <version>`, which moves the
Unreleased section below into the new version's section.

## [Unreleased]

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

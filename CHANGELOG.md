# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/).

Each release is cut with `node scripts/release.mjs <version>`, which moves the
Unreleased section below into the new version's section.

## [Unreleased]

### Fixed

- Release a macOS native session's filesystem state when it stops. Each
  stopped session kept a full APFS clone of the project (two in
  `cbcow-private`) until it was removed, which only `deltabox run` did, so
  clients such as dsh-deltabox accumulated one clone per session. `stop` now
  moves that state aside and a detached helper deletes it at background
  priority, pausing while a session starts; the next start also clears what
  earlier sessions left behind.
- Reap the macOS FSEvents helper once its session stops, so long-lived
  embedders no longer keep one zombie process per stopped session.

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

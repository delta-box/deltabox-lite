# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/).

Each release is cut with `node scripts/release.mjs <version>`, which moves the
Unreleased section below into the new version's section.

## [Unreleased]

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

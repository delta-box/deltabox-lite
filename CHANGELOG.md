# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/).

Each release is cut with `node scripts/release.mjs <version>`, which moves the
Unreleased section below into the new version's section.

## [Unreleased]

- First packaged release of the `deltabox` CLI and the `deltabox-lite` npm
  wrapper with prebuilt binaries for macOS (arm64, x64) and Linux (x64, arm64).
- `scripts/release.mjs` bumps every version site in lockstep, rolls this
  changelog, and tags the release.

# Releasing deltabox-lite

A walkthrough for a maintainer cutting a release for the first time. Assumes
a clone with push access to `main` and the `gh` CLI signed in.

## Overview

- Entries accumulate under `## [Unreleased]` in `CHANGELOG.md`, grouped into
  `### Added` / `### Changed` / `### Fixed`.
- `node scripts/release.mjs X.Y.Z` bumps every version site in lockstep,
  rolls `Unreleased` into `## [X.Y.Z] - date`, commits, and tags `vX.Y.Z`
  with that changelog section as the tag message.
- `node scripts/release.mjs X.Y.Z --push` pushes the commit and tag; pushing
  is a separate, explicit step from cutting the release.
- The tag push triggers the `prebuilds` workflow: build every platform,
  publish a draft GitHub release plus the npm packages, flip it to
  published, then update the Homebrew tap.
- Every publish step is skipped once that exact version already exists, so a
  failed run can simply be re-run.

## One-time setup

1. **`NPM_TOKEN`** — an npm automation token, as a repository secret, allowed
   to publish `deltabox-lite`, `deltabox-lite-darwin-arm64`,
   `deltabox-lite-darwin-x64`, `deltabox-lite-linux-arm64`, and
   `deltabox-lite-linux-x64`. Those names must be claimable, or already
   owned by the token's account.
2. **The Homebrew tap** — create `Ddnirvana/homebrew-tap` with a `Formula/`
   directory, then add repository secret `HOMEBREW_TAP_TOKEN`: a
   fine-grained PAT with `Contents: read/write` on that repo. Without it,
   the `homebrew` job warns and skips instead of failing.
3. **GitHub releases** — nothing to configure; the workflow's default
   `GITHUB_TOKEN` already has `contents: write`.

## Cutting a release

1. Make sure `## [Unreleased]` describes everything going out — it becomes
   both the tag annotation and the GitHub release notes.
2. Preview first; `--dry-run` prints the plan and writes nothing:
   ```bash
   node scripts/release.mjs 0.2.0 --dry-run
   ```
   Check the bump direction and which files would change. The first release
   may keep the version already in the tree (`0.1.0`) — the script then only
   rolls the changelog and checks every version site already agrees.
3. Cut it for real, then review before pushing (`git show`):
   ```bash
   node scripts/release.mjs 0.2.0
   ```
   This bumps `Cargo.toml`, `deltafs-fuse/Cargo.toml`, and
   `npm/deltabox-lite/package.json` (plus its four `optionalDependencies`
   pins), regenerates `Cargo.lock`, rolls the changelog, commits, and tags.
4. Push:
   ```bash
   node scripts/release.mjs 0.2.0 --push
   # or by hand:
   git push --atomic origin main v0.2.0
   ```
   `--push` only works from `main`; it fetches `origin` first and refuses if
   local `main` is behind.

## What the pipeline does

1. **Build matrix** — macOS arm64/x64 and Linux x64/arm64 build the binary
   and run smoke tests.
2. **Publish** — a single restartable job creates a draft GitHub release
   from the tag annotation, uploads the `deltabox-<version>-<target>.tar.gz`
   tarballs with `.sha256` sidecars and the npm tarballs, then publishes the
   four platform packages followed by the `deltabox-lite` wrapper to npm.
3. **Flip to published** — once every platform has succeeded, the draft
   release is published.
4. **Homebrew** — a separate job rewrites `homebrew/deltabox.rb` with the
   release's checksums and pushes it to `Ddnirvana/homebrew-tap` (skipped
   with a warning if the tap token is not configured).

A manual `workflow_dispatch` run from the Actions tab only builds and
verifies; artifacts are downloadable from the run, but nothing is published.

## Re-running a failed release

Re-run the workflow (or just the failed job) from the Actions tab — publishing
is idempotent, so this does not double-publish anything. If the tag itself is
wrong, delete the draft release and the tag before retagging:
```bash
gh release delete vX.Y.Z --yes
git push origin :refs/tags/vX.Y.Z
git tag -d vX.Y.Z
```

## Testing locally

**`scripts/release.mjs`**, in a throwaway clone so a mistake cannot push to
this repo:
```bash
git clone . /tmp/deltabox-release-test
cd /tmp/deltabox-release-test
node scripts/release.mjs 0.2.0 --dry-run
```

**`scripts/install.sh`**, against a local build instead of a GitHub release:
```bash
DELTABOX_VERSION=0.2.0 DELTABOX_BASE_URL=file:///abs/path/to/dist \
  DELTABOX_INSTALL_DIR=/tmp/deltabox-install sh scripts/install.sh
```

**The Homebrew formula**, against a throwaway local tap instead of the real
`Ddnirvana/homebrew-tap`. The committed formula carries placeholder checksums,
so first render one that points at a local build served over HTTP (a
`.sha256` sidecar is needed for every target; copy the real one under the
other three names for a single-platform test):
```bash
node scripts/package-prebuild.mjs aarch64-apple-darwin dist   # or your target
(cd dist && python3 -m http.server 18765 --bind 127.0.0.1 &)
node scripts/update-homebrew-formula.mjs 0.2.0 --from-dir dist \
  --base-url http://127.0.0.1:18765 --out /tmp/deltabox.rb
brew tap-new yourname/localtest --no-git
cp /tmp/deltabox.rb "$(brew --repo yourname/localtest)/Formula/deltabox.rb"
brew audit --strict --formula yourname/localtest/deltabox
brew install yourname/localtest/deltabox && brew test yourname/localtest/deltabox
brew uninstall yourname/localtest/deltabox && brew untap yourname/localtest
```

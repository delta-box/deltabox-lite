# Session and rollback semantics

This document defines the behavioral spec for service-free sessions. Durable
files are authoritative; a global daemon or `deltabox serve` process is never
required to keep a session valid.

## Filesystem modes

`session start` creates an implicit initial checkpoint and chooses one resolved
filesystem mode. `session status` reports both the requested and resolved mode,
host visibility, rollback guarantee, helper state, and any fallback reason.

| Mode | Host visibility before commit | Commit | Abort |
| --- | --- | --- | --- |
| `cbcow-through` | Pending changes are visible immediately | Accepts the current state and retires undo history | Replays undo records into the host-visible tree |
| `cbcow-private` | The host project remains unchanged | Conflict-checks and publishes touched paths from private `LIVE`, then retires undo history | Replays undo records into private `LIVE` |
| `copy-private` | The host project remains unchanged | Validates and publishes the private working diff | Restores the private working view |

Linux requests `cbcow-through` by default when DeltaFS FUSE is mount-ready.
If startup cannot establish a clean mounted session, Deltabox reports the
reason and falls back to `copy-private`. `--require-fs-mode` converts that
fallback into an error. Other supported platforms currently resolve to
`copy-private`. Select `cbcow-private` on Linux when pre-commit host isolation
is required without returning to per-command tree materialization.

Independent CLI processes may execute, read, write, diff, commit, checkpoint,
or abort the same session. `diff` compares the current view with the accepted
checkpoint. `abort` without a checkpoint restores the accepted state.
`checkpoint.create` is an advanced savepoint primitive; normal commit and abort
do not require a user-created checkpoint.

## Review scopes

Adapters choose the rollback target; checkpoints and review scopes are not the
same concept.

| Mode | Savepoint | Default review abort target |
| --- | --- | --- |
| Standalone CLI | session start or latest commit | accepted baseline |
| Task | start of the user request | task-start savepoint |
| Tool | immediately before the mutating tool | pre-tool savepoint |

Skipped reviews retain their changes. A later cumulative review must keep the
earliest still-pending savepoint as its abort target.

The core persists this scope through `review.begin|status|diff|commit|abort|skip`.
Calling `review.begin` while a scope is active returns the existing scope;
commit and abort close it. Adapters must not create their own per-tool
checkpoint chain to infer the rollback target.

## Process model

Normal CLI operations are service-free and synchronous. A Linux CB-CoW session
owns one automatically launched filesystem helper and FUSE mount; each `exec`
still launches a short-lived confined process. The helper can be restarted from
its journal and state files after process failure. `cbcow-through` mounts the
host tree, while `cbcow-private` mounts a private reflink/copy materialization.
`copy-private` uses a short-lived sandbox transaction and materializes its
result into a durable private working view.

Live command processes, file descriptors, and shell state are not durable
session state. A command must complete before its filesystem result becomes a
completed transaction boundary.

`serve` may optimize transport or sandbox startup, but it uses the same on-disk
state and remains restartable. It is never the source of truth. A failed worker
returns an error for its request; the next request in the same `serve` process
rebuilds a worker from the durable working view. Live child processes and file
descriptors are not recoverable state.

## Concurrency and conflicts

Only one mutating operation may own a session at a time. A per-session process
lock serializes operations and stale locks are recoverable after process death.
Only one active CB-CoW session may lease a project. Host writes during a
`cbcow-through` session are unsupported because they bypass FUSE pre-image
capture.
`cbcow-private` and `copy-private` reject conflicting external project changes
instead of silently overwriting them. The CB-CoW private mode uses strong
content and metadata fingerprints for each touched path and its namespace
parents; unrelated file-content edits outside those paths do not force a full
tree scan at commit.

## Crash recovery and storage

In `cbcow-through`, every routed mutation writes a checksummed `READY` record
after preserving the required bytes, metadata, or namespace state and before
changing the host tree. `APPLIED` completes the mutation. Reopening the helper
recovers incomplete records conservatively. Commit publishes an acceptance
marker and schedules retired undo epochs for reclamation; abort replays records
newest first. The default durability protects process crashes, not sudden
power loss.

`cbcow-private` uses the same undo engine against private `LIVE`. Commit stages
only effective touched paths with `FICLONE` when supported, a sparse-aware copy
fallback otherwise, and preserves hardlink groups and extended metadata. A
publication marker and per-path backups make process-crash recovery converge
to the old host state before publication or the new accepted state after host
publication. Completed backups are handed to the session helper for
asynchronous reclamation.

In `copy-private`, working-view updates record changed paths, back up their
previous values, and incrementally materialize the command-local COW layer.
Candidate swaps and host publication are journaled so reopening either restores
the old tree or completes the recorded boundary.

Accepted and working scan indexes are persisted and refreshed only at
transaction boundaries. Checkpoint trees are prepared under a temporary name,
atomically published, capped at 100 manifest entries, and removed from disk
when pruned. Linux uses `FICLONE`; macOS uses APFS `clonefile`; both fall back
to ordinary copying when cloning is unavailable. Manifest, scan-index,
registry, session, and
review metadata updates use fsynced temporary files, atomic rename, and parent
directory fsync. Boundary journal entries are fdatasynced.

The default policy caps direct writes and aggregate pending storage. A Linux
through session resolves existing deny-pattern matches once at session start
and gives those exact mounted paths to Sandlock for every command; the external
state root and direct backing project are always denied. Creating or modifying
host files outside Deltabox while the session is active is outside the
exclusive-writer requirement.

## Path mapping limitation

Linux and macOS map complete absolute argv values, working directories, and
path-list environment values from the host project to the sandbox view. They
do not rewrite arbitrary shell or program source strings. A script containing
a hardcoded host-project path therefore sees that literal unchanged, and the
isolation policy denies direct backing writes. Prefer relative paths or pass
project paths as separate argv values. `mount_namespace: false` in session
status makes this limitation machine-readable.

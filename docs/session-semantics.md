# Session and rollback semantics

This document defines the behavioral spec for service-free sessions. The
on-disk session store is authoritative; a daemon or `deltabox serve` process is
never required to keep a session valid.

## User-visible lifecycle

`session start` creates an accepted baseline and a durable working view. It
does not start a global service. Independent CLI processes may then execute,
read, write, diff, commit, or abort the same session.

- `diff` compares the durable working view with the accepted baseline.
- `commit` applies the working diff to the real project and refreshes the
  accepted baseline.
- `abort` without a checkpoint restores the accepted baseline.
- `checkpoint.create` is an advanced savepoint primitive. Normal commit and
  abort operations do not require an explicit checkpoint.

The real project must remain byte-for-byte unchanged until commit. Abort must
never move, delete, or rewrite real-project files.

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

Normal CLI operations are service-free and synchronous. Each mutating command
uses a short-lived sandbox transaction and materializes its result into the
durable working view. Live processes, file descriptors, and shell state are not
part of a durable session. Workloads that require persistent background
processes must explicitly use a future `run` or optional `serve` mode.

`serve` may optimize transport or sandbox startup, but it uses the same on-disk
state and remains restartable. It is never the source of truth. A failed worker
returns an error for its request; the next request in the same `serve` process
rebuilds a worker from the durable working view. Live child processes and file
descriptors are not recoverable state.

## Concurrency and conflicts

Only one mutating operation may own a session at a time. A per-session process
lock serializes operations and stale locks are recoverable after process death.
Commit must reject external changes to the real project instead of silently
overwriting them.

## Crash recovery and storage

Working-view updates record changed paths, back up their previous values, and
then incrementally materialize the command-local COW layer. A durable
transaction marker identifies every phase; reopening the session either
restores the path backups or finalizes an applied update. Direct working-tree
operations use a candidate-tree swap. Host commit keeps both a host backup and
the pre-commit working tree until its journal record is durable, so a killed
commit cannot leave a partially published project. Abort records its journal
boundary in the same candidate-swap marker; recovery either restores the old
working view or completes the abort journal and closes its review scope.

Accepted and working scan indexes are persisted and refreshed only at
transaction boundaries. Checkpoint trees are prepared under a temporary name,
atomically published, capped at 100 manifest entries, and removed from disk
when pruned. Linux uses `FICLONE` where supported and falls back to ordinary
copying on other filesystems. Manifest, scan-index, registry, session, and
review metadata updates use fsynced temporary files, atomic rename, and parent
directory fsync. Boundary journal entries are fdatasynced.

The default policy caps direct writes and aggregate pending storage. Denied
paths and unsafe symlinks are validated before a command-local COW result is
materialized, so policy failures cannot partially update the durable view.

## Linux backend limitation

Sandlock 0.8.5 cannot reliably use a directory that exists only in the COW
upper layer as a later child-process `chdir` target within the same `exec`.
Existing project directories and repositories work normally. Workflows such as
`git init new-repo && git -C new-repo ...` should split creation and subsequent
use into separate `deltabox exec` calls so the durable working view is
materialized between them.

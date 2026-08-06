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

## Process model

Normal CLI operations are service-free and synchronous. Each mutating command
uses a short-lived sandbox transaction and materializes its result into the
durable working view. Live processes, file descriptors, and shell state are not
part of a durable session. Workloads that require persistent background
processes must explicitly use a future `run` or optional `serve` mode.

`serve` may optimize transport or sandbox startup, but it must use the same
on-disk state and remain restartable. It is never the source of truth.

## Concurrency and conflicts

Only one mutating operation may own a session at a time. A per-session process
lock serializes operations and stale locks are recoverable after process death.
Commit must reject external changes to the real project instead of silently
overwriting them.

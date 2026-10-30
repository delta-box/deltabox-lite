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
macOS requests the native APFS/FSEvents `cbcow-through` provider. If startup
cannot establish the requested provider, Deltabox reports the reason and falls
back to `copy-private`; `--require-fs-mode` converts that fallback into an
error. Select `cbcow-private` when pre-commit host isolation is required.

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

The resolved filesystem mode is stored in each session record and remains the
routing authority after CLI, helper, or coordinator restart. Changing the
platform default affects only new sessions. Session records created before
filesystem metadata was introduced continue to resolve to `copy-private`.

## Process model

Normal CLI operations are synchronous and require no prestarted service. A
CB-CoW session lazily launches a session-scoped command helper on its first
`exec`. Independent CLI and `serve` calls reuse its warm Sandlock worker on
Linux or its prepared Seatbelt profile and network proxy on macOS; each user
command is still a short-lived confined child. The helper owns no authoritative
filesystem state and is recreated automatically after failure. A changed
policy or a restarted Linux filesystem helper invalidates and rebuilds only the
warm sandbox runtime. Failed commands are never replayed automatically because
the caller may already have observed their side effects.

A Linux CB-CoW session also owns one automatically launched filesystem helper
and FUSE mount. The filesystem helper can be restarted from its journal and
state files after process failure. `cbcow-through` mounts the host tree, while
`cbcow-private` mounts a private reflink/copy materialization. `copy-private`
uses a short-lived sandbox transaction and materializes its result into a
durable private working view.

On macOS, the session owns a small FSEvents helper so independent CLI processes
reuse one warm stream. The helper holds only pending event candidates and can
be recreated from the durable cursor and device identity after failure. Fast
`exec` calls may leave event accounting queued in that helper; the next state
operation closes a supervisor-only sentinel before it reads or changes session
state. Strict `exec` closes the sentinel before returning. There is no mount.

Live command processes, file descriptors, and shell state are not durable
session state. On Linux and in strict macOS sessions, a command completes its
filesystem transaction boundary before returning. Fast macOS `exec` returns
after the child exits and defers event accounting to the next state operation;
the persisted cursor remains behind until that synchronization, so helper or
CLI failure replays rather than loses those events.

`serve` may optimize transport, but it uses the same helper and on-disk state
as independent CLI calls and remains restartable. It is never the source of
truth. A failed worker returns an error for its request; the next request
rebuilds a worker from the durable working view. Live child processes and file
descriptors are not recoverable state.

The command helper holds a second per-session operation lock while an `exec` is
live. If the invoking CLI exits after dispatch, later status, diff, checkpoint,
commit, abort, stop, or exec operations wait for that orphaned command boundary
instead of racing it. The helper acknowledges lock ownership before starting a
command, so a request whose client disappears before handoff is not executed.

## Processes left running

Rolling the tree back under a live writer is incoherent, and committing while
one is mid-write can capture a torn file. When a command backgrounds a process
and returns (`sleep 300 &`, a watcher, a dev server), `commit` and `abort`
check for it first and apply `--live-processes` (`live_processes` in the
protocol):

| Policy | Behavior |
| --- | --- |
| `refuse` (default) | Fail and list the processes by name and pid. Nothing is stopped. |
| `kill` | SIGTERM, then SIGKILL, every such process, then continue. |
| `detach` | Continue and leave them running. Their later writes land in the next review scope. |

`deltabox run` defaults to `kill`, since a one-shot transaction has nobody to
choose. `review.commit` and `review.abort` take no policy yet and always
refuse.

Detection is Linux-only for now. The sandbox worker becomes a child subreaper,
so a process that double-forks or calls `setsid` is still found by walking the
worker's descendants. Mode notes:

- `cbcow-through` and `cbcow-private`: the warm sandbox outlives commit and
  abort, so all three policies apply.
- `copy-private` on the persistent session manager restarts the sandbox on
  commit and abort, which would kill the process; `detach` is rejected there.
- Durable `copy-private` runs each exec in a throwaway sandbox that is torn
  down when the command returns, so nothing survives to be checked.
- macOS and Windows do not detect leftover processes yet; the policy is
  accepted and ignored.

## Concurrency and conflicts

Only one mutating operation may own a session at a time. A per-session process
lock serializes operations and stale locks are recoverable after process death.
Only one active CB-CoW session may lease a project. Host writes outside the
coordinated command path during `cbcow-through` are unsupported. Fast macOS
through mode uses this exclusive-writer contract to avoid an FSEvents sentinel
wait on every `exec`; pending counts and aggregate storage-limit checks become
synchronous at the next status, diff, or commit boundary. Strict macOS through
mode detects historical external events, marks the session tainted, and refuses
abort until the state is explicitly accepted. `cbcow-private` and
`copy-private` reject conflicting external project changes instead of silently
overwriting them. The CB-CoW private mode uses strong content and metadata
fingerprints for each touched path and its namespace parents; unrelated
file-content edits outside those paths do not force a full tree scan at commit.

## Crash recovery and storage

In `cbcow-through`, every routed mutation writes a checksummed `READY` record
after preserving the required bytes, metadata, or namespace state and before
changing the host tree. `APPLIED` completes the mutation. Reopening the helper
recovers incomplete records conservatively. Commit publishes an acceptance
marker and schedules retired undo epochs for reclamation; abort replays records
newest first. The default durability protects process crashes, not sudden
power loss.

Start a strict session with `session start --durable`. In strict mode, a routed
mutation returns only after its pre-image, `READY`, and `APPLIED` journal state
have crossed the required storage barriers. Commit first synchronizes every
changed `LIVE` file and affected directory, then synchronizes the acceptance
marker and its parent directory before returning. A marker present after
recovery means the new state is accepted; without that marker, the changes
remain pending against the previous accepted checkpoint.

`--group-commit-ms N` may be combined with `--durable`. Concurrent mutations
may share the `APPLIED` journal synchronization performed within that window,
but no mutation reports success until the group containing its record is
stable. Checkpoint, abort, and commit drain any pending group before publishing
their result. Session status reports `durability: "strict"` and the configured
`group_commit_ms`; fast sessions report `durability: "process-crash"`.
Strict startup requires `cbcow-through` or `cbcow-private` and fails if the
requested provider is unavailable. It never reports strict durability for a
`copy-private` fallback.

Linux `cbcow-private` uses the same undo engine against private `LIVE`. Commit stages
only effective touched paths with `FICLONE` when supported, a sparse-aware copy
fallback otherwise, and preserves hardlink groups and extended metadata. A
publication marker and per-path backups make process-crash recovery converge
to the old host state before publication or the new accepted state after host
publication. Completed backups are handed to the session helper for
asynchronous reclamation.

In strict private mode, staged new objects and host backups are synchronized
before the prepared publication marker. Applied host paths and their namespace
directories are synchronized before the host-applied phase is recorded. The
accepted fingerprint index is synchronized before success, while publication
backups are reclaimed later.

In `copy-private`, working-view updates record changed paths, back up their
previous values, and incrementally materialize the command-local COW layer.
Candidate swaps and host publication are journaled so reopening either restores
the old tree or completes the recorded boundary.

Accepted and working scan indexes are persisted and refreshed only at
transaction boundaries. Checkpoint trees are prepared under a temporary name,
atomically published, capped at 100 manifest entries, and removed from disk
when pruned. `copy-private` indexes use schema-versioned SHA-256 fingerprints
covering file contents, symlink targets, permissions, extended attributes,
macOS ACLs, and directory entries; legacy metadata-only indexes are rebuilt on
first use. A command hashes its resulting working tree once, and commit reuses
the persisted accepted and working fingerprints instead of rescanning both
private trees. Linux uses `FICLONE`; macOS uses APFS `clonefile`; both fall back
to ordinary copying when cloning is unavailable. Manifest, scan-index,
registry, session, and
review metadata updates use fsynced temporary files, atomic rename, and parent
directory fsync. Boundary journal entries are fdatasynced.

The macOS provider stores incremental path snapshots, tombstones, indexes, and
transaction backups in its external state tree. A prepared commit restores the
old base and host paths after process failure; once the accepted manifest names
the transaction, recovery keeps the new accepted state and retires its backup.

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

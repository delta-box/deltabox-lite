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

## Native host release (macOS)

`session.release` / `releaseSession` / `deltabox session release` closes an idle
`cbcow-through` session without reverting pending host edits. It refuses an
unresolved review and private mode. Native embedding hosts should resolve their
review first, then release when unloading. The next session adopts the current
workspace as its baseline. `session.stop` / `stopSession` keeps its explicit
rollback behavior. This matters when external editors change files after the
last accepted agent review.

## Durable file history (API version 1)

Capture follows the session durability: fast mode retains versions across process
restart without forcing every cloned file to disk; strict mode flushes the whole
validated version before publishing it. Fast mode makes no power-loss guarantee.
Restore transaction backups retain their existing strict flush/recovery ordering.

`history.capture` records immutable project files independently of review
checkpoints. `history.list` lists versions across sessions for the same canonical
project and stable external state directory. Commit, review abort, session stop,
and a new coordinator do not delete those versions. `history.capabilities` reports
support explicitly; version 1 supports macOS `cbcow-through` only. Unsupported
modes return an error rather than silently falling back to transaction checkpoints.

### History CLI

`deltabox history` exposes the same history operations from the command line.
Every command accepts `--project PATH`, defaulting to the current directory (`.`).
Use the same canonical project and state directory that created the versions;
if `DELTABOX_STATE_DIR` is set, retain that external directory and use the same
value in subsequent shells. Pointing at another project or state directory does
not locate the original history.

| Command | Default output | Optional flags |
| --- | --- | --- |
| `history capabilities SESSION` | JSON capability report | |
| `history capture SESSION` | New version UUID | `--label TEXT`, `--json` for the complete version |
| `history list SESSION` | Version table | `--json` for the version array |
| `history diff SESSION VERSION` | Readable diff from current files to `VERSION` | `--path`, `--stat`, `--name-only`, `--json` |
| `history preview VERSION` | The same diff without a running session | `--path`, `--stat`, `--name-only`, `--json` |
| `history restore SESSION VERSION --expected-version EXPECTED` | JSON `{ version, backup, restored }` | |

Prefix each command with `deltabox`. Capture, list, diff, and restore require a
running macOS `cbcow-through` session. Preview needs no session ID and remains
available after the session stops. Capture saves a version without committing
pending changes or resolving an active review.

The [README example](../README.md#durable-file-history-macos) demonstrates capture,
restore, and preview in a disposable project. For an existing session, inspect
support and saved versions with:

```bash
deltabox history capabilities "$session_id"
deltabox history list "$session_id"
deltabox history list "$session_id" --json
```

Restore requires `--expected-version` to identify a captured version matching the
current project. Resolve any active review and wait for commands and external
writers to finish first. If current files differ from that expected version,
restore fails without replacing them; there is no force option. A preview is
advisory and does not satisfy this check. Both diff commands describe the current
project becoming the target, so their additions and deletions follow that direction.

Save the restore result outside the project if recording it to a file: adding a
new project file can itself invalidate the next expected-version check. To undo
a successful restore, use its `backup.id` as the new target and its `version.id`
as the expected current version, provided the project has not changed since.
For example, with `jq` installed and `target_version` / `current_version` set to
existing version IDs:

```bash
restore_result=$(deltabox history restore "$session_id" "$target_version" \
  --expected-version "$current_version")
backup_version=$(printf '%s' "$restore_result" | jq -r '.backup.id')
deltabox history restore "$session_id" "$backup_version" \
  --expected-version "$target_version"
```

Restore preserves ignored/generated files and Git metadata under the rules below.
A successful restore also updates the accepted session baseline, so stopping the
session retains it. Keep the backup ID and state directory for later recovery.

### API and restoration semantics

The stdio methods are `history.capabilities` / `history.list` (`session_id`),
`history.capture` (`session_id`, optional `label`), `history.diff` (`session_id`,
`version_id`), and `history.restore` (`session_id`, `version_id`,
`expected_version_id`). The N-API equivalents are `historyCapabilities`,
`historyList`, `historyCapture`, `historyDiff`, and `historyRestore`, with positional
arguments in the same order. IDs are opaque canonical UUIDs. Versions contain
`id`, `session_id`, `label`, `created_at_ms`, `entry_count`, and `logical_bytes`.
Diffs describe the current project becoming the target version.

Read-only `history.preview` (`version_id`), or N-API `historyPreview(versionId)`,
compares current project files with an existing immutable version without a running
session. It does not start, recover or adopt a sandbox, clone build outputs, or
create a version. Snapshot integrity and source-scope checks are the same as
`history.diff`. An in-flight publication returns busy instead of blocking a preview.
Callers should reject previews during active agent work/review and serialize them
with capture/restore; a preview is advisory and never replaces restore's full
expected-version check.

Restore requires a running, idle session with no active review. It checks every
versioned entry against the expected version using full SHA-256 content digests,
entry types, symlink targets, in-project hard-link equivalence groups, permissions,
ownership, ACLs, supported flags and tracked xattrs. Root-directory metadata is
included and appears as path `.` in diffs. An external content or metadata change
causes a conflict before restoration. Timestamps alone are not conflicts. The
manager serializes operations with command execution and project publication;
callers must also quiesce external editors and other unmanaged writers throughout
restore, since these do not honor session locks. Capture checks the selected live
files and frozen ignore/index policy before and after cloning and fails if they
changed. Ignored output changes alone do not conflict. Trusted shell execution
completes a full tree reconciliation before returning, so an immediate review
abort includes additions and deletions even when FSEvents notifications arrive
late. This scan applies only to trusted command execution.

A successful restore returns `{ version, backup, restored }`. `version` is the
existing target version; `backup` is a new durable pre-restore version. A durable
engine undo transaction restores both the host and accepted baseline after an
interrupted restore, or finalizes an already accepted restore. A later stop or
abort therefore retains a successful restore.

Attached hosts (the Node addon) hold an `owner.lock` for the session's life. If
the host crashes during a restore, admission reports `SessionOrphaned`; adopting
the orphan via `adoptSession` (or opening/releasing a detached session) runs
transaction recovery before allowing new operations. If recovery fails with a
post-crash conflict, the orphan lock is released and the durable undo transaction
remains intact on disk until the user reconciles the workspace. For a full-tree
transaction, an uncommitted restore is recovered automatically only when the live
tree still matches its complete pre-restore or target tree; an ambiguous partial
tree blocks recovery rather than overwriting it. For the regular-file transaction
described below, each touched file may independently match its old or new
fingerprint after an atomic rename, while every untouched entry must still match.
Both paths reject post-crash external edits, retaining current files and the
durable undo transaction.

Snapshots live under the external state base at
`history-v1/<canonical-project-sha256>/<version-id>/`. The project-root `.deltabox`
engine directory and all `.git` entries are excluded. Manifest format 2 freezes
project-local `.gitignore` rules and the tracked path set. Tracked files always
remain included, as do untracked and hidden files not ignored by those rules.
Global Git excludes, parent rules outside the project, `.git/info/exclude`, and
`.ignore` are not used. Empty directories and directories containing only ignored
data are outside the version; ancestor metadata of included files is recorded.

Restore preserves current ignored/generated files and Git metadata. It uses the
target version's frozen policy, even if the live `.gitignore` has changed. A file
or symlink replacing a directory containing excluded data fails before touching
the live project. Legacy format 1 snapshots remain unchanged on disk and are
projected to the same project-file semantics from their captured rules/index.
Their original generated data is retained in storage but not restored.

Ordinary content replacements can use a selective durable file transaction. Its
first version handles existing regular files with unchanged metadata, no hard
links (including links outside the project), no ACLs/xattrs/flags or special mode
bits, and owned writable non-symlink parents on the staging filesystem. Every
baseline update must qualify too. Additions, deletions, directory/type changes,
metadata changes and other complex cases retain the full durable undo transaction
and merged staging tree. This is an optimization, not a change to the history scope.

The file path persists full before/after content and metadata fingerprints but
copies only the files it will replace, including separate host and baseline undo
images. It synchronizes the backups and existing host/baseline trees before the
durable transaction marker, then atomically renames prepared files using directory
descriptors that refuse symlink traversal. Each rename synchronizes both parent
directories. Full-tree verification still precedes publication and commit;
post-crash ignored edits still block automatic recovery. Untouched generated
files retain their inodes and are not cloned or replaced, but are still scanned
and synchronized: restore time is not independent of project size. Native session
initialization is unchanged. The `history-files-v1` transaction mode deliberately
makes older runtimes reject an incomplete new transaction; complete recovery with
the newer runtime before downgrading.

Binary files, symlinks (including dangling links), permissions, metadata,
in-project hard links, additions and deletions are preserved. Special files,
non-UTF-8 path names, and immutable/append-only BSD flags (`uchg`, `uappnd`,
`schg`, `sappnd`) are rejected explicitly before capture/restore starts. Other
flags, such as `hidden`, are preserved. Version integrity is checked before
applying its tree; snapshot contents must not be edited manually.

Retention is manual: there is no implicit pruning or automatic history quota.
Explicit retention plans support pins, newest-version counts, age boundaries,
and logical-byte budgets. Pin external references before pruning. Collection
persists its deletion intent and can resume after interruption with `history gc`.
See [storage accounting, retention and GC](history-storage.md) for the protocol,
commands, guarantees and limitations. `logical_bytes` counts regular-file bytes,
including each hard-link path; it does not measure physical APFS allocation.
Snapshots and full restore staging need free storage even though APFS cloning
can share file data. Out-of-space errors before the durable restore transaction
leave project files unchanged. Interrupted unpublished captures remain hidden
and are never listed as versions. Explicit prune plans collect them under the
project publication lock.

History captures use up to eight concurrent readers, but still read every included
project file and compare the live-before, cloned and live-after content/metadata
indexes. Ignored build outputs are excluded from captures; initial session setup
and full restore staging can still be expensive for large trees.

# History storage and reclamation

History maintenance operates on the existing macOS `cbcow-through` history
store, independently of running or stopped sessions. Keep the same canonical
project and external state directory used at capture time. These operations are
available through Rust `SessionCoordinator`, stdio RPC, and the CLI. The Node
addon currently exposes the earlier capture/restore API, not these maintenance
methods. Other operating systems return unsupported.

## Space accounting

`deltabox history stats --project PATH --json` reports per-version usage and the
whole namespace, including manifests, pin records, abandoned captures, and
garbage left by an interrupted collection.

| Counter | Meaning |
| --- | --- |
| `logical_bytes` | Source bytes represented by manifests, counting each regular-file path, including hard-link aliases |
| `usage.apparent_bytes` | Actual regular-file lengths in the stored version, including its manifest |
| `usage.allocated_bytes` | `st_blocks * 512`, counted once per `(device, inode)` within that aggregate |
| `total` | Independent scan of the entire namespace, deduplicating inodes across versions and maintenance directories |
| `abandoned_captures` | Unpublished `.capture-UUID` directories, measured separately |
| `garbage`, `pending_gc` | Retired `.trash-*` directories and presence of a committed GC intent |

Per-version allocation values are **not additive** when versions share inodes.
APFS clones can also share extents between distinct inodes. `stat(2)` does not
expose enough information to attribute those shared extents, so
`allocated_bytes_are_exclusive` is always false. Neither the allocation counter
nor the logical-byte deletion result estimates physical bytes reclaimed.
Sparse files can have apparent sizes much greater than allocation.

Legacy format-1 snapshots may physically contain generated artifacts that are
excluded from their current source projection. Their apparent/allocation usage
includes those files; their logical count reflects the source projection. Stats
does not rewrite legacy snapshots or recover engine transactions. Corrupt
manifests fail accounting rather than silently understating storage.

## Retention roots and policy

Versions are retained indefinitely until explicit pruning. `history pin VERSION`
adds a persistent retention root; `history pin VERSION --remove` removes it.
Pins protect experiment baselines, published results, or versions referenced by
an external client. They survive session stop and process restart. Pin metadata
is atomically replaced and flushed, but pinning does not retroactively upgrade
a fast-mode capture's data to strict power-loss durability.

`history prune` computes a plan without deleting anything. Protection is the
union of explicit pins, the newest `--keep-last N` versions (default 1), and
versions at or newer than `--older-than-ms TIMESTAMP`. The timestamp is absolute
Unix milliseconds so the policy remains identical between planning and apply.
Ordering is `(created_at_ms, UUID)`; equal timestamps have deterministic order.

With `--max-logical-bytes B`, the planner removes oldest eligible versions until
the retained logical total fits `B`, or no eligible versions remain. Without a
budget it selects every eligible version. Protected versions always win; the
plan exposes `budget_satisfied=false` if they exceed the budget. This is an
explicit collection target, not automatic capture admission control or a physical
disk quota. `--keep-last 0` deliberately permits deleting every unpinned version.

All abandoned captures are included in the plan, irrespective of the version
budget. Holding the publication lock proves no cooperating capture is still
writing one of those directories.

```sh
# Observe storage and pin a baseline already returned by history capture.
deltabox history stats --json
deltabox history pin "$baseline_version"

# Inspect the candidate set; this does not delete versions.
deltabox history prune --keep-last 10 --max-logical-bytes 1073741824 --json

# Copy plan_id from the inspected result; use exactly the same policy.
deltabox history prune --keep-last 10 --max-logical-bytes 1073741824 \
  --apply "$plan_id" --json
```

`plan_id` is a SHA-256 digest over the complete sorted catalog, raw manifest
digests, directory identities, pins, abandoned-capture identities, and policy.
Apply rebuilds the plan under the lock and rejects a mismatched digest before
writing a GC intent. A new capture, a changed pin set, or changed policy therefore
requires a new plan. Reapplying a completed nonempty plan is stale.

## GC transaction and failure model

All maintenance operations acquire the same project publication lock as capture,
restore and preview. They never acquire a session lock or open engine state while
holding it. Thus the existing order remains session/command → publication;
project-only maintenance starts directly at publication. GC cannot unlink a
snapshot being consumed by a cooperating restore or preview.

The protocol is a durable intent followed by per-directory retirement:

1. Validate every candidate, pins, and the plan digest. Validate canonical UUIDs,
   project identity and non-symlink directories before writing any intent.
2. Write and sync a temporary journal, rename it to `.gc-journal.json`, then sync
   the history root. This is the commit point for the deletion decision.
3. Rename each selected version to `.trash-UUID` (or an abandoned capture to
   `.trash-capture-UUID`) and sync the root **before** unlinking its contents.
4. Delete the retired directory and sync the root. Repeat for the remaining
   items. Finally remove the journal and sync the root again.

The batch is not atomically removed from readers' view. Retirement of each
directory is atomic; after interruption some selected versions may already be
gone. Recovery rolls the committed intent forward and does not undo deletion:

```sh
deltabox history stats --json   # exposes pending_gc and residual garbage
deltabox history gc --json      # resume only the already committed intent
```

Resume checks the journal schema, project, checksum, duplicate IDs, directory
identity, and any surviving manifest before continuing. Both source and trash
existing is an error. A partially unlinked trash directory can be resumed even
after its manifest is gone. Missing source and trash means that item already
completed. A retry with no journal is a no-op, unless unowned trash is detected.
Results list the entire completed intent, including items removed before a crash.

Pin changes and new prune plans are blocked while an intent is pending. Stats,
scrub, capture and available-version reads do not implicitly resume deletion.
Captures made after an interrupted intent have new UUIDs and cannot be consumed
by that intent. GC never follows symlinks or deletes unidentified hidden paths.
Malformed records or I/O errors preserve the journal for diagnosis/retry. Small
temporary metadata records left before their atomic rename are counted in total
usage but are not automatically collected.

Full and incremental restore transactions persist their own host/base backups
and staged content before publishing their transaction marker. They do not
reference the history UUID during recovery. Consequently a crashed restore can
recover even after its original target, expected version and pre-restore history
backup have been explicitly collected. Tests cover both restore paths and a
second crash during recovery.

## Integrity scrub and cost

`history verify [VERSION] --json` checks one version or all versions and returns
nonzero if any check fails. It validates manifest identities and aggregate
counters, file contents/digests, mode/owner, tracked attributes/ACL/flags,
symlinks and hard-link groups. Format 2 validates the complete stored tree,
including unexpected ignored files and empty directories; format 1 retains its
source-projection compatibility semantics. A corrupt version is reported while
checking the remaining versions. Scrub never repairs or deletes content.

Planning reads manifests and directory metadata, without hashing snapshot file
contents; it costs O(manifest bytes + version count). Stats walks all stored
entries without reading file bodies. Scrub reads all represented contents.
Deletion costs O(entries removed). All hold the publication lock for a consistent
view; large stores can therefore delay capture/restore. Accounting uses memory
proportional to unique inodes; planning materializes the catalog. No persistent
counter cache or concurrent/background collector is introduced.

The fault tests kill subprocesses at journal publication, rename, and deletion
boundaries, exercise interrupted recursive unlink and repeated recovery, and
inject I/O failures. They validate process-crash recovery and fsync ordering in
code; they are not physical power-cut experiments. The protocol relies on the
filesystem's rename/fsync guarantees and the existing private-store/cooperating
writer model, not protection against a same-user adversary changing store paths.

## RPC surface

| Method | Params |
| --- | --- |
| `history.stats` | `{}` |
| `history.verify` | `{ "version_id": null }` or a version UUID |
| `history.pin` | `{ "version_id": "UUID", "pinned": true }` |
| `history.prune_plan` | `{ "keep_last": 10, "older_than_ms": null, "max_logical_bytes": null }` |
| `history.prune_apply` | `{ "policy": { ... }, "expected_plan_id": "digest" }` |
| `history.gc` | `{}` |

Rust public request/result structs live in `deltabox_core::history_storage`.

# deltafs-fuse

`deltafs-fuse` is a standalone commit-biased filesystem for Deltabox. It
contains:

- a platform-neutral state engine over ordinary directories, with
  range pre-images, linear checkpoints, rollback, commit, touched-path diff,
  quota enforcement, and process-crash recovery.
- a passthrough FUSE adapter backed directly by that state engine;
- an authenticated Unix-socket control endpoint for checkpoint, diff, commit,
  abort, status, and shutdown operations.

The mounted view reads and writes the ordinary source tree. Before a routed
mutation destroys state, the engine records the minimum undo data required for
rollback. Commit accepts the current source tree and retires old undo epochs.
Fast mode targets process crashes. Strict mode adds ordered storage barriers
for undo data, journal records, accepted `LIVE` data, and commit markers.

## Build

Linux uses fuser's pure Rust mount implementation:

```sh
cargo build --release --features fuse
```

macOS requires a separately installed and approved macFUSE runtime. Deltabox
does not install or approve macFUSE automatically:

```sh
cargo build --release --features fuse
```

Without a FUSE runtime, the platform preflight remains available:

```sh
cargo run -- doctor
```

## Mount

The source, mountpoint, and state directory must be different directories. The
state directory must be outside the source tree.

```sh
mkdir -p /tmp/deltafs-mount /tmp/deltafs-state
cargo run --release --features fuse -- \
  mount \
  --source /path/to/project \
  --mountpoint /tmp/deltafs-mount \
  --state-dir /tmp/deltafs-state
```

Add `--durable` for strict persistence. `--group-commit-ms N` allows
concurrent mutation replies to share the `APPLIED` journal barrier: each reply
still waits until the group containing its record is synchronized. Commit
returns only after changed source files, affected directories, the commit
marker, and the marker directory have been synchronized.

Durability, grouping, and quota settings are persisted with the state. Reusing
a state directory requires the same settings; a mismatch is rejected before
mounting.

Unmount with the platform's normal FUSE tool, for example:

```sh
fusermount3 -u /tmp/deltafs-mount
```

The state directory contains a checksummed session manifest and append-only
epoch journals with `READY` and `APPLIED` records. Pre-images are stored per
epoch. Reusing the state directory reopens and recovers the same linear
history.

Inspect recovery state without mounting or modifying it:

```sh
cargo run -- diagnose --state-dir /tmp/deltafs-state
cargo run -- diagnose --state-dir /tmp/deltafs-state --json
```

An unresolved `READY` record or active recovery action is recoverable by
restarting the helper. A checksum, sequence, or torn-record error stops
automatic recovery. Preserve both the state directory and source tree before
replacing damaged state; `diagnose` reports the affected file and does not
delete evidence.

For programmatic lifecycle control, run `serve` with external socket, token,
and ready-file paths, then use `control`. The token file must contain a random
capability readable only by the owning user.

## State engine

The Rust API is exposed from `deltafs_fuse::engine`. `StateEngine::create` and
`StateEngine::open` bind a `LIVE` directory to an external state directory.
All mutations must pass through that instance.

The engine supports file and directory creation, symlinks, writes through
paths or existing descriptors, append, truncate, unlink and open-unlink,
rename replacement/exclusive/exchange, hardlinks, mode/owner/time/xattr and
POSIX ACL changes, sparse files, supported `fallocate` modes, and
`copy_file_range`. The mounted adapter supports file and directory sync,
advisory locks, and shared writable mappings with the normal FUSE page cache.
Linear checkpoints, rollback, commit, and touched-index diff use checksummed
state files and monotonic `READY`/`APPLIED` journal records that remain
authoritative across process restart. A failed operation after `READY` poisons
the instance; reopen it to run conservative recovery before continuing.

Run the state-engine validation and empty-operation scaling benchmark with:

```sh
cargo test --test engine
cargo bench --bench empty_control
```

## Current limits

- Global mutation serialization, chosen to make ordering unambiguous.
- Kernel writeback cache remains disabled; the adapter uses synchronous FUSE
  write callbacks. Shared writable `mmap` uses the normal page cache.
- Fast mode does not protect against sudden power loss. Strict mode depends on
  the persistence behavior of the host filesystem and storage stack.
- The source tree must have one coordinated writer through the adapter or
  engine during validation.
- Pre-existing hardlinks inside `LIVE` are discovered on demand. If a regular
  file has unresolved hardlink aliases outside `LIVE`, mutation fails with
  `UnresolvedHardlink` before changing the file. External aliases are not
  supported.
- Non-UTF-8 relative paths are not supported by the state format.
- Device-node creation and file-flag `ioctl` operations are not supported.
- `fallocate` supports allocation, `KEEP_SIZE`, hole punching, and zero ranges.
  Collapse-range, insert-range, and other layout-changing modes return
  `EOPNOTSUPP`.

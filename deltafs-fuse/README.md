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
The journal contract targets process crashes; power-loss durability is not
provided.

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

Unmount with the platform's normal FUSE tool, for example:

```sh
fusermount3 -u /tmp/deltafs-mount
```

The state directory contains a checksummed session manifest and append-only
epoch journals with `READY` and `APPLIED` records. Pre-images are stored per
epoch. Reusing the state directory reopens and recovers the same linear
history.

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
- Process-crash evidence only; no `fsync`/power-loss guarantee.
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

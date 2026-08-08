# deltafs-fuse

`deltafs-fuse` is a standalone commit-biased filesystem for Deltabox. It
contains two separate layers:

- a passthrough FUSE adapter that records a whole-file pre-image before the
  first mutation of each path;
- a platform-neutral state engine over ordinary directories, with
  range pre-images, linear checkpoints, rollback, commit, touched-path diff,
  quota enforcement, and process-crash recovery.

The `mount` command uses the passthrough adapter. The state engine is available
through the Rust API and does not currently back mounted operations. The
journal contract targets process crashes; power-loss durability is not
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

The source, mountpoint, and undo directory must be different directories. The
undo directory must be outside the source tree.

```sh
mkdir -p /tmp/deltafs-mount /tmp/deltafs-undo
cargo run --release --features fuse -- \
  mount \
  --source /path/to/project \
  --mountpoint /tmp/deltafs-mount \
  --undo-dir /tmp/deltafs-undo
```

Unmount with the platform's normal FUSE tool, for example:

```sh
fusermount3 -u /tmp/deltafs-mount
```

The append-only `journal.jsonl` contains `ready` and `applied` records. A valid
`ready` record is flushed before the corresponding backing mutation. Pre-images
are stored under `preimages/`.

## State engine

The Rust API is exposed from `deltafs_fuse::engine`. `StateEngine::create` and
`StateEngine::open` bind a `LIVE` directory to an external state directory.
All mutations must pass through that instance.

The engine supports file and directory creation, symlinks, writes, append,
truncate, unlink, rename replacement, hardlinks, mode/owner/time/xattr changes,
linear checkpoints, rollback, commit, and touched-index diff. Checksummed state
files and monotonic `READY`/`APPLIED` journal records are authoritative across
process restart. A failed operation after `READY` poisons the instance; reopen
it to run conservative recovery before continuing.

Run the state-engine validation and empty-operation scaling benchmark with:

```sh
cargo test --test engine
cargo bench --bench empty_control
```

## Current limits

- The FUSE adapter still has one implicit epoch and whole-file pre-images; the
  state engine does not back mounted operations.
- Global mutation serialization, chosen to make ordering unambiguous.
- Kernel writeback cache remains disabled; the adapter uses synchronous FUSE
  write callbacks, including for shared `mmap` writes.
- Process-crash evidence only; no `fsync`/power-loss guarantee.
- The source tree must have one coordinated writer through the adapter or
  engine during validation.
- Pre-existing hardlinks inside `LIVE` are discovered on demand. If a regular
  file has unresolved hardlink aliases outside `LIVE`, mutation fails with
  `UnresolvedHardlink` before changing the file. External aliases are not
  supported.
- Non-UTF-8 paths are rendered lossily in the diagnostic journal.

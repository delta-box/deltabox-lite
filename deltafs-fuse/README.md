# deltafs-fuse

`deltafs-fuse` is the standalone Phase 0 filesystem prototype for Deltabox. It
mounts an ordinary source directory as a FUSE filesystem and records a
whole-file pre-image before the first mutation of each path.

This prototype exists to validate FUSE interception, cache behavior, crash
ordering, compatibility, and latency. It is not yet the complete multi-epoch
CB-CoW engine: checkpoint, diff, commit, abort, range capture, and durable
power-loss ordering belong to later phases.

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

## Phase 0 limits

- One implicit epoch per mount; no checkpoint/commit/abort API yet.
- Whole-file pre-images, not byte-range capture.
- Global mutation serialization, chosen to make ordering unambiguous.
- Kernel writeback cache remains disabled; Phase 0 uses synchronous FUSE write
  callbacks, including for shared `mmap` writes.
- Process-crash evidence only; no `fsync`/power-loss guarantee.
- The source tree must have one writer through this mount during validation.
- Non-UTF-8 paths are rendered lossily in the diagnostic journal.

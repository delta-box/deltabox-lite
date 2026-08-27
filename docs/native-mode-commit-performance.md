# Native Commit Performance Comparison

This benchmark compares commit lifecycle performance for the two supported
native execution profiles:

- `cbcow-through + deltabox`
- `git + bubblewrap`

Both profiles use the in-process Node-API addon through
`DeltaboxNativeClient`. The stdio transport is not involved.

## Environment

| Item | Value |
|---|---|
| Core revision | `0207c09` (`fix(core): enable FUSE for Linux consumers`) |
| OS | Linux 6.8.0-79-generic, x86_64 |
| CPU | Intel Core i7-13700, 24 logical CPUs |
| Filesystem | ext4 |
| Node.js | v24.16.0 |
| Rust profile | `release` |
| Durability | `fast` |
| Benchmark source | [`plugin/dsh/perf/native-fastpath-bench.mjs`](../plugin/dsh/perf/native-fastpath-bench.mjs) |

## Invocation

Two runs were performed with reversed mode ordering to control for ordering
and cache effects:

```bash
pnpm run bench:native:commit -- \
  --warmups 5 \
  --runs 30 \
  --modes cbcow-through+deltabox,git+bubblewrap \
  --output /tmp/deltabox-native-commit-perf.json

pnpm run bench:native:commit -- \
  --warmups 5 \
  --runs 30 \
  --modes git+bubblewrap,cbcow-through+deltabox \
  --output /tmp/deltabox-native-commit-perf-reverse.json
```

Each mode completed five warm-up iterations and 30 measured iterations per
run. Results were produced by the current local release binary and Node-API
addon.

## Temporary Layout

Each profile receives an independent disposable root created with `mkdtemp`:

```text
/tmp/deltabox-<profile>-XXXXXX/
|-- project/
|   `-- payload.bin
|-- state/
|-- runtime/
|-- home/
|-- tmp/
`-- policy.toml
```

`project/payload.bin` is created before `session.start` and contains exactly
1,024 bytes. Every iteration modifies this same file without changing its
size. Reusing a fixed path prevents the project tree from growing as commits
accumulate and keeps all samples comparable.

The generated policy permits project writes, allows reads from the project and
standard system paths, and denies network access:

```toml
[filesystem]
read = [".", "/usr", "/bin", "/etc"]
write = ["."]

[network]
mode = "deny_all"
```

For `cbcow-through`, the session exposes a DeltaFS mountpoint. Git mode has no
mountpoint and manages the project through its private side repository.

## Measured Workflow

Each iteration performs the following native operations:

1. Begin a task review with `review.begin`.
2. Overwrite `project/payload.bin` with a distinct 1 KiB payload through
   `writeFileBytes`.
3. Read the file through `readFileBytes` and verify its contents.
4. Execute `bash -c :` inside the selected sandbox.
5. Request `review.diff` and verify exactly one modified file with old and new
   sizes of 1,024 bytes.
6. Commit the review with `review.commit` and verify `applied === 1`.
7. Verify that the committed content remains visible through both the session
   filesystem and the project path.

The timed `total` is the sum of steps 1 through 6. Post-commit content
verification is intentionally excluded from the measured total.

Warm-up payloads use byte values 1 through 5. Measured payloads use byte values
6 through 35, ensuring that every iteration produces a real content change.

## Results

The table below averages the independently calculated p50 and p95 values from
the two ordering runs. It does not represent a recomputed quantile over a
merged 60-sample set.

| Phase | CB-CoW p50 | CB-CoW p95 | Git p50 | Git p95 | p50 comparison |
|---|---:|---:|---:|---:|---:|
| Cold session start | 15.201 ms | N/A | 21.911 ms | N/A | CB-CoW 1.44x faster |
| `review.begin` | 0.289 ms | 0.586 ms | 16.332 ms | 20.943 ms | CB-CoW 56.5x faster |
| Native 1 KiB write | 0.336 ms | 0.462 ms | 0.112 ms | 0.175 ms | Git 3.0x faster |
| Native 1 KiB read | 0.096 ms | 0.168 ms | 0.057 ms | 0.093 ms | Git 1.7x faster |
| `session.exec` | 2.508 ms | 3.665 ms | 5.635 ms | 6.931 ms | CB-CoW 2.2x faster |
| `review.diff` | 2.963 ms | 3.789 ms | 9.876 ms | 13.047 ms | CB-CoW 3.3x faster |
| `review.commit` | 0.421 ms | 0.775 ms | 19.687 ms | 25.804 ms | CB-CoW 46.8x faster |
| **Complete iteration** | **6.942 ms** | **8.623 ms** | **51.903 ms** | **66.555 ms** | **CB-CoW 7.5x faster** |

Cold start is a single observation per mode in each ordering run, not a p50
over the iteration samples. It is included for completeness but should not be
used as a stable startup distribution.

## Run-to-Run Stability

| Ordering | CB-CoW total p50 / p95 | Git total p50 / p95 |
|---|---:|---:|
| CB-CoW first | 6.782 / 8.597 ms | 52.126 / 67.539 ms |
| Git first | 7.101 / 8.648 ms | 51.679 / 65.571 ms |

The commit phase itself was stable when the mode order was reversed:

| Ordering | CB-CoW commit p50 / p95 | Git commit p50 / p95 |
|---|---:|---:|
| CB-CoW first | 0.417 / 0.762 ms | 19.744 / 28.545 ms |
| Git first | 0.425 / 0.787 ms | 19.629 / 23.063 ms |

- CB-CoW commit p50 changed by `0.008 ms`, approximately `1.9%`.
- Git commit p50 changed by `0.115 ms`, approximately `0.6%`.
- Complete-iteration p50 changed by approximately `4.6%` for CB-CoW and
  `0.9%` for Git.

## Interpretation

Git mode remains faster for direct native byte reads and writes. The complete
commit lifecycle is nevertheless dominated by Git-backed review boundaries.

Git `review.commit` first constructs and diffs the current workspace tree,
retires the previous undo refs, and then constructs another snapshot to become
the newly accepted baseline. This includes multiple Git process launches and
full-worktree `git add -A` scans even though the workload changes only one
1 KiB file.

CB-CoW already captured the file mutation through its FUSE state engine.
Commit advances the accepted checkpoint and retires the corresponding undo
state without rescanning the project tree. Consequently, the isolated commit
phase is approximately **46.8x faster at p50**, while the complete
begin/write/read/exec/diff/commit iteration is approximately **7.5x faster at
p50** and **7.7x faster at p95**.

Raw outputs were written to:

- `/tmp/deltabox-native-commit-perf.json`
- `/tmp/deltabox-native-commit-perf-reverse.json`

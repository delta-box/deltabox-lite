# Native Mode Performance Comparison

This benchmark compares the two supported native execution profiles:

- `cbcow-through + deltabox`
- `git + bubblewrap`

Both profiles use the in-process Node-API addon through
`DeltaboxNativeClient`. The stdio transport is not involved.

## Environment

| Item | Value |
|---|---|
| Revision | `0207c09` (`fix(core): enable FUSE for Linux consumers`) |
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
node perf/native-fastpath-bench.mjs \
  --action abort \
  --warmups 5 \
  --runs 30 \
  --modes cbcow-through+deltabox,git+bubblewrap

node perf/native-fastpath-bench.mjs \
  --action abort \
  --warmups 5 \
  --runs 30 \
  --modes git+bubblewrap,cbcow-through+deltabox
```

Each mode therefore completed five warm-up iterations and 30 measured
iterations per run.

## Temporary Layout

Every profile receives an independent disposable root created with `mkdtemp`:

```text
/tmp/deltabox-<profile>-XXXXXX/
|-- project/    # Project root passed to DeltaboxNativeClient
|-- state/      # Native Deltabox session and filesystem state
|-- runtime/    # Runtime sockets and process state
|-- home/       # Isolated home directory
|-- tmp/        # Isolated temporary directory
`-- policy.toml
```

The generated policy permits project writes, allows reads from the project and
standard system paths, and denies network access:

```toml
[filesystem]
read = [".", "/usr", "/bin", "/etc"]
write = ["."]

[network]
mode = "deny_all"
```

For `cbcow-through`, the session exposes a DeltaFS mountpoint. For Git mode,
there is no mountpoint and the project directory is managed directly through
the Git-backed storage provider.

## Measured Workflow

Each measured iteration performs the following native operations:

1. Begin a task review with `review.begin`.
2. Write a new 1 KiB file through `writeFileBytes`.
3. Read it back through `readFileBytes` and verify its contents.
4. Execute `bash -c :` inside the selected sandbox.
5. Request `review.diff` and verify that exactly one 1 KiB added file is reported.
6. Abort the review with `review.abort`.
7. Verify that the added file no longer exists.

Measured files use these paths:

```text
/tmp/deltabox-<profile>-XXXXXX/project/payload-0.bin
/tmp/deltabox-<profile>-XXXXXX/project/payload-1.bin
...
/tmp/deltabox-<profile>-XXXXXX/project/payload-29.bin
```

Each file contains exactly 1,024 bytes. Warm-up iterations use negative
suffixes and are excluded from the statistics.

## Results

The table below averages the independently calculated p50 and p95 values from
the two ordering runs. It does not represent a recomputed quantile over a
merged sample set.

| Phase | CB-CoW p50 | CB-CoW p95 | Git p50 | Git p95 | p50 comparison |
|---|---:|---:|---:|---:|---:|
| Cold session start | 13.574 ms | N/A | 17.072 ms | N/A | CB-CoW 1.26x faster |
| `review.begin` | 0.564 ms | 1.060 ms | 13.374 ms | 16.223 ms | CB-CoW 23.7x faster |
| Native 1 KiB write | 0.325 ms | 0.420 ms | 0.104 ms | 0.173 ms | Git 3.1x faster |
| Native 1 KiB read | 0.118 ms | 0.195 ms | 0.058 ms | 0.105 ms | Git 2.1x faster |
| `session.exec` | 2.588 ms | 3.837 ms | 5.579 ms | 8.892 ms | CB-CoW 2.2x faster |
| `review.diff` | 1.836 ms | 1.959 ms | 8.580 ms | 9.920 ms | CB-CoW 4.7x faster |
| `review.abort` | 0.881 ms | 1.484 ms | 16.343 ms | 18.129 ms | CB-CoW 18.6x faster |
| **Complete iteration** | **6.374 ms** | **8.048 ms** | **44.418 ms** | **54.873 ms** | **CB-CoW 7.0x faster** |

## Run-to-Run Stability

| Ordering | CB-CoW total p50 / p95 | Git total p50 / p95 |
|---|---:|---:|
| CB-CoW first | 6.389 / 8.585 ms | 45.021 / 59.998 ms |
| Git first | 6.359 / 7.510 ms | 43.814 / 49.747 ms |

The p50 results remained stable when the mode order was reversed:

- CB-CoW p50 changed by `0.030 ms`, or approximately `0.5%`.
- Git p50 changed by `1.207 ms`, or approximately `2.7%`.

## Interpretation

Git mode is faster for direct native byte reads and writes because those
operations access the project tree without going through the FUSE path.
However, Git-backed checkpoint creation, diff calculation, and rollback
dominate the complete review lifecycle.

The optimized Git baseline reuse path still spends approximately:

- `13.4 ms` in `review.begin`
- `8.6 ms` in `review.diff`
- `16.3 ms` in `review.abort`

CB-CoW performs those operations incrementally through its filesystem state
engine. As a result, the complete write/read/execute/diff/abort workflow is
approximately **7x faster at p50** and **6.8x faster at p95**.

Raw outputs were written to:

- `/tmp/deltabox-native-perf.json`
- `/tmp/deltabox-native-perf-reverse.json`

## 100-File Batch Benchmark

The single-file reject benchmark above isolates the fixed cost of one review
boundary. A second benchmark measures the opposite workload shape: one review
containing 100 existing-file modifications. It uses
[`plugin/dsh/perf/native-batch-bench.mjs`](../plugin/dsh/perf/native-batch-bench.mjs).

### Batch Test Conditions

Each measured sample uses a fresh project, state root, runtime root, native
coordinator, and Deltabox session. Fixture creation happens before the measured
wall clock starts. Cleanup happens after it stops.

The project initially contains 100 flat text files. Every file is exactly
1,024 bytes. One sample performs this workflow:

1. Construct `DeltaboxNativeClient` and require `native-bytes` file access.
2. Start a `fast` session in the requested native profile.
3. Begin one task review.
4. Read each of the 100 existing files through `readFileBytes`.
5. Append one deterministic line to each file through `writeFileBytes`.
6. Execute one `bash -c :` command.
7. Request one `review.diff` and require exactly 100 changed files.
8. Commit or abort the entire review.
9. Read all 100 files from the host and verify the accepted or restored bytes.
10. Stop the session.

The CB-CoW and Git cases therefore execute the same 100 Node-API reads, 100
Node-API writes, one command, and one review settlement. Commit and abort are
measured independently. Every case receives one warm-up sample followed by
five measured samples. Results below are the median and p95 across those five
fresh-session samples.

The runtime directory is created separately under a short `/tmp/dbx-*` path.
This avoids exceeding the Unix-domain socket path limit after Deltabox appends
the session UUID and control socket name.

```bash
pnpm run bench:batch -- \
  --binary /path/to/deltabox \
  --files 100 \
  --bytes 1024 \
  --warmups 1 \
  --runs 5 \
  --output /tmp/deltabox-native-batch-perf.json
```

The reported `total` starts immediately before native client construction and
ends after `session.stop`. It includes session start, review work, final
100-file verification, and session stop. It excludes fixture creation and
temporary-directory cleanup.

### Batch Commit Results

| Phase | CB-CoW p50 | CB-CoW p95 | Git p50 | Git p95 | p50 comparison |
|---|---:|---:|---:|---:|---:|
| Client construction | 0.111 ms | 0.160 ms | 0.108 ms | 0.116 ms | Equivalent |
| `session.start` | 13.174 ms | 13.443 ms | 28.493 ms | 29.703 ms | CB-CoW 2.2x faster |
| `review.begin` | 0.692 ms | 0.756 ms | 17.965 ms | 19.620 ms | CB-CoW 26.0x faster |
| 100 native reads | 11.011 ms | 15.053 ms | 2.768 ms | 3.639 ms | Git 4.0x faster |
| 100 native writes | 38.426 ms | 47.338 ms | 3.312 ms | 4.732 ms | Git 11.6x faster |
| First `session.exec` | 17.432 ms | 17.960 ms | 5.315 ms | 6.286 ms | Git 3.3x faster |
| 100-file `review.diff` | 190.071 ms | 197.011 ms | 37.698 ms | 47.458 ms | Git 5.0x faster |
| `review.commit` | 0.857 ms | 0.978 ms | 45.267 ms | 51.618 ms | CB-CoW 52.8x faster |
| Verify 100 files | 4.285 ms | 4.970 ms | 3.292 ms | 5.352 ms | Similar |
| `session.stop` | 117.616 ms | 120.539 ms | 5.391 ms | 5.695 ms | Git 21.8x faster |
| **Complete lifecycle** | **400.212 ms** | **405.761 ms** | **151.808 ms** | **170.342 ms** | **Git 2.64x faster** |

Median native file-call cost derived from each 100-operation phase:

| Operation | CB-CoW per file | Git per file |
|---|---:|---:|
| Read | 0.1101 ms | 0.0277 ms |
| Write | 0.3843 ms | 0.0331 ms |

### Batch Abort Results

| Phase | CB-CoW p50 | CB-CoW p95 | Git p50 | Git p95 | p50 comparison |
|---|---:|---:|---:|---:|---:|
| Client construction | 0.091 ms | 0.104 ms | 0.108 ms | 0.124 ms | Equivalent |
| `session.start` | 13.171 ms | 14.105 ms | 31.521 ms | 39.242 ms | CB-CoW 2.4x faster |
| `review.begin` | 0.428 ms | 0.666 ms | 22.429 ms | 34.180 ms | CB-CoW 52.4x faster |
| 100 native reads | 9.467 ms | 14.852 ms | 2.733 ms | 6.484 ms | Git 3.5x faster |
| 100 native writes | 38.059 ms | 44.854 ms | 3.347 ms | 8.246 ms | Git 11.4x faster |
| First `session.exec` | 16.535 ms | 17.628 ms | 6.359 ms | 7.204 ms | Git 2.6x faster |
| 100-file `review.diff` | 189.113 ms | 198.272 ms | 38.133 ms | 41.537 ms | Git 5.0x faster |
| `review.abort` | 9.589 ms | 9.923 ms | 45.722 ms | 47.693 ms | CB-CoW 4.8x faster |
| Verify 100 files | 3.347 ms | 4.556 ms | 3.155 ms | 4.446 ms | Similar |
| `session.stop` | 118.299 ms | 120.454 ms | 5.698 ms | 10.965 ms | Git 20.8x faster |
| **Complete lifecycle** | **402.647 ms** | **413.647 ms** | **164.675 ms** | **169.731 ms** | **Git 2.45x faster** |

Median native file-call cost derived from each 100-operation phase:

| Operation | CB-CoW per file | Git per file |
|---|---:|---:|
| Read | 0.0947 ms | 0.0273 ms |
| Write | 0.3806 ms | 0.0335 ms |

### Batch Interpretation

The batch result reverses the single-file result. Git pays much higher fixed
cost at `session.start`, `review.begin`, and review settlement, but that cost is
amortized over 100 modifications. Direct project-tree byte access, batched Git
diff generation, and inexpensive session shutdown more than recover the fixed
boundary cost.

CB-CoW remains substantially faster at the transaction boundaries: commit is
effectively constant-time, and abort replays the 100 touched paths without
creating a new baseline snapshot. Its current 100-file text diff and FUSE
shutdown dominate this fresh-session workload.

This is intentionally a cold lifecycle benchmark. DSH project-session reuse
removes most `session.start` and `session.stop` frequency and warms the CB-CoW
command helper. Excluding client construction, session start, verification,
and session stop, the measured review workload is approximately:

| Action | CB-CoW review workload | Git review workload | Comparison |
|---|---:|---:|---:|
| Commit | 258.5 ms | 112.3 ms | Git 2.30x faster |
| Abort | 263.2 ms | 118.7 ms | Git 2.22x faster |

The commit cases ran before the abort cases, and each mode used five measured
samples. Unlike the single-file results, this first batch run was not repeated
with reversed mode ordering. A larger publication-grade run should alternate
mode/action ordering and increase the sample count.

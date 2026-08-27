# Native Docker Isolation Performance Comparison

This benchmark compares the two native GitGhost execution profiles:

- `git + bubblewrap`
- `git + docker`

Both profiles use the same Git-backed storage implementation and the in-process
Node-API addon through `DeltaboxNativeClient`. The only intended variable is the
command isolation provider. The stdio transport is not involved.

## Environment

| Item | Value |
|---|---|
| Revision | `a38e850` plus the native Docker worktree changes |
| OS | Linux 6.8.0-79-generic, x86_64 |
| CPU | Intel Core i7-13700, 24 logical CPUs |
| Filesystem | ext4 |
| Node.js | v24.16.0 |
| Rust profile | `release` |
| Durability | `fast` |
| Bubblewrap | 0.9.0 |
| Docker client / server | 28.1.1 / 28.1.1 |
| Docker image | `ubuntu:22.04` (`sha256:c42dedf797ba...`) |
| Benchmark source | [`plugin/dsh/perf/native-fastpath-bench.mjs`](../plugin/dsh/perf/native-fastpath-bench.mjs) |

The image was already present before measurement. Image download time is not
included. The test account could not access the Docker socket or user namespace
directly, so both `docker` and `bwrap` resolved through equivalent test-only
`sudo -n` wrappers. This adds process-launch overhead to the absolute command
times; deployments with direct permissions should be somewhat faster.

## Invocation

Abort and commit were each measured twice with reversed mode ordering:

```bash
node perf/native-fastpath-bench.mjs \
  --binary /path/to/release/deltabox \
  --action abort \
  --warmups 3 \
  --runs 15 \
  --modes git+bubblewrap,git+docker

node perf/native-fastpath-bench.mjs \
  --binary /path/to/release/deltabox \
  --action abort \
  --warmups 3 \
  --runs 15 \
  --modes git+docker,git+bubblewrap
```

The same two commands were repeated with `--action commit`. Each profile
therefore completed six warm-up iterations and 30 measured iterations per
action. Tables below average the independently calculated p50 and p95 values
from the two ordering runs; they are not quantiles over a merged sample set.

## Measured Workflow

Each iteration begins a review, writes and reads one 1 KiB file through native
byte APIs, executes `bash -c :`, requests a structured diff, and then commits or
aborts the review. Abort iterations create a distinct file and verify removal.
Commit iterations repeatedly overwrite one fixed 1 KiB file and verify the
accepted content. The reported total is the sum of these measured phases.

## Abort Results

| Phase | Bubblewrap p50 | Bubblewrap p95 | Docker p50 | Docker p95 | p50 comparison |
|---|---:|---:|---:|---:|---:|
| Cold session start | 22.243 ms | N/A | 28.565 ms | N/A | Bubblewrap 1.28x faster |
| `review.begin` | 13.310 ms | 24.718 ms | 13.791 ms | 18.386 ms | Equivalent |
| Native 1 KiB write | 0.116 ms | 0.191 ms | 0.107 ms | 0.159 ms | Equivalent |
| Native 1 KiB read | 0.069 ms | 0.110 ms | 0.056 ms | 0.088 ms | Equivalent |
| `session.exec` | 10.923 ms | 17.709 ms | 125.351 ms | 148.363 ms | Bubblewrap 11.48x faster |
| `review.diff` | 8.591 ms | 11.191 ms | 8.872 ms | 11.030 ms | Equivalent |
| `review.abort` | 17.237 ms | 23.357 ms | 16.615 ms | 21.609 ms | Equivalent |
| **Complete iteration** | **50.711 ms** | **68.928 ms** | **165.263 ms** | **194.141 ms** | **Bubblewrap 3.26x faster** |

## Commit Results

| Phase | Bubblewrap p50 | Bubblewrap p95 | Docker p50 | Docker p95 | p50 comparison |
|---|---:|---:|---:|---:|---:|
| Cold session start | 26.444 ms | N/A | 31.729 ms | N/A | Bubblewrap 1.20x faster |
| `review.begin` | 17.291 ms | 28.178 ms | 16.648 ms | 21.264 ms | Equivalent |
| Native 1 KiB write | 0.114 ms | 0.311 ms | 0.116 ms | 0.206 ms | Equivalent |
| Native 1 KiB read | 0.069 ms | 0.240 ms | 0.059 ms | 0.113 ms | Equivalent |
| `session.exec` | 11.063 ms | 19.523 ms | 126.459 ms | 164.201 ms | Bubblewrap 11.43x faster |
| `review.diff` | 10.149 ms | 12.900 ms | 9.905 ms | 12.270 ms | Equivalent |
| `review.commit` | 20.125 ms | 28.141 ms | 20.188 ms | 31.453 ms | Equivalent |
| **Complete iteration** | **59.023 ms** | **82.814 ms** | **173.133 ms** | **223.403 ms** | **Bubblewrap 2.93x faster** |

Cold start is one observation per profile in each ordering run. The displayed
value is the average of those two observations, not a startup distribution.

## Run-to-Run Stability

| Action and ordering | Bubblewrap total p50 / p95 | Docker total p50 / p95 |
|---|---:|---:|
| Abort, Bubblewrap first | 51.282 / 74.016 ms | 162.687 / 199.726 ms |
| Abort, Docker first | 50.139 / 63.840 ms | 167.838 / 188.555 ms |
| Commit, Bubblewrap first | 57.057 / 95.750 ms | 168.809 / 226.013 ms |
| Commit, Docker first | 60.989 / 69.878 ms | 177.457 / 220.792 ms |

Abort p50 changed by approximately 2.3% for Bubblewrap and 3.1% for Docker
when ordering was reversed. Commit p50 changed by approximately 6.7% and 5.0%,
respectively. With only 15 measured samples per ordering, p95 is visibly more
variable and should be treated as directional.

## Interpretation

The Git storage phases are effectively unchanged between the profiles. Native
reads and writes, review creation, diff calculation, and settlement all remain
within roughly one millisecond of one another at p50. This is expected because
those operations run in the host-side GitGhost provider rather than in the
command sandbox.

The performance difference is concentrated in `session.exec`. Starting a fresh
Docker container for the no-op command took about 125-126 ms at p50, versus
about 11 ms for Bubblewrap on this host. The roughly 115 ms per-command premium
makes the complete Docker review 3.26x slower for abort and 2.93x slower for
commit at p50.

Docker therefore trades command startup latency for an image-defined root
filesystem and a stronger operational boundary. The cost is linear in the
number of commands within a turn; native file operations and Git review costs
do not acquire the same premium. Reusing a long-lived container would address
the dominant cost, but would also require explicit lifecycle, cleanup, and
cross-command state semantics that the current fresh-container design avoids.

Raw outputs were written to:

- `/tmp/deltabox-native-docker-abort.json`
- `/tmp/deltabox-native-docker-abort-reverse.json`
- `/tmp/deltabox-native-docker-commit.json`
- `/tmp/deltabox-native-docker-commit-reverse.json`

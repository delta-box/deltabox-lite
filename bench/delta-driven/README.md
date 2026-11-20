# Command-path and delta-driven benchmarks (Linux)

`run.py` reproduces the two tables in the top-level README's
[Performance](../../README.md#performance) section: no-op command overhead
versus project size, and the small-write microbenchmarks that compare the
`cbcow-through` session against one-shot Sandlock copy-on-write.

## Build and run

```sh
cargo build --release -p deltabox
cargo build --release -p deltabox-core --example raw_sandlock
python3 bench/delta-driven/run.py --json target/delta-driven.json
```

Options: `--deltabox <bin>` to measure another build, `--workdir <dir>` for the
disposable projects (default `target/bench-delta-driven`; it must be on the
filesystem you want to measure, **not tmpfs**), `--warmups` (5), `--runs` (50,
no-op table), `--micro-runs` (30, write table), `--only noop|micro`. The write
workloads need about 560 MiB free in the work directory.

## What is measured

Each sample is the wall-clock time, from `time.perf_counter_ns` in the driver,
of the external processes that an agent harness would invoke. Process spawn,
CLI startup, IPC and sandbox setup are all included. Every workload runs
`--warmups` unmeasured samples, then the measured ones. p50 and p95 use linear
interpolation. Page cache is warm: input files are written once and then
reused.

### Runners

| Runner | What one command costs |
|---|---|
| `cbcow-through` | One `deltabox exec <sid> --project P -- <argv>` CLI process against a single long-lived `deltabox session start P --fs-mode cbcow-through --require-fs-mode` session (fast durability). The session start is outside the timing. A commit is a separate `deltabox commit` CLI process. |
| raw Sandlock | One `target/release/examples/raw_sandlock P <store> -- <argv>` process ([source](../../crates/deltabox-core/examples/raw_sandlock.rs)). It builds a fresh `sandlock-core` sandbox with `workdir = P` (seccomp copy-on-write branch), runs the command, and commits the branch into `P` on exit. Its confinement matches deltabox's Sandlock profile for the default policy. There is no session, so every command pays its own copy-up and commit. |
| plain shell | `subprocess.run(argv, cwd=P)`, unsandboxed. |

### Workloads

| Name | Project contents | One sample |
|---|---|---|
| `noop-N` (N = 10, 200, 1000) | N files of 1 KiB random data in 10 subdirectories | One `sh -c :`. For raw Sandlock this includes its (empty) commit. |
| `write-256m` | One 256 MiB file of random (non-sparse) data | One command `head -c 4096 /dev/urandom \| dd of=big00.bin bs=4096 seek=K conv=notrunc`, made durable in the project. cbcow: `exec` + `commit`. Raw Sandlock: one run (copies the whole 256 MiB file up, writes, commits it back). |
| `write-16x32m` | Sixteen 32 MiB files of random data | 16 commands; command *i* overwrites 4 KiB in file *i*. cbcow: 16 × `exec`, then **one** `commit`. Raw Sandlock: 16 runs, each committing independently. |

The block offset K changes every sample, so each sample is a real modification.
Before timing, every write workload checks once that the written block
actually reached the host file.

### What it does not measure

- Session start/stop and the first-command helper start (warmups absorb them).
- Durable (`--durable`) mode, `cbcow-private`, `copy-private`, macOS, Windows.
- Cold page cache, or concurrency between commands.

## Reproduction, 2026-11-14

The host was an Intel Core Ultra 5 245K (14 CPUs), Linux 7.0.0, ext4 on NVMe
(87% full), a release build, and the default arguments (5 warmups, 50 no-op
runs, 30 write runs). The README numbers came from a different host
(`x86-server`, Linux 6.18) and were recorded at commit `c2011a4`
(2026-08-15). Three `deltabox` builds were measured against the same raw
Sandlock runner (`sandlock-core` 0.8.5 in both trees):

- **v0.4.2**: current `main` (`9524f95`).
- **v0.4.2, 5 ms poll**: `main` with `DISCONNECT_POLL` in
  `crates/deltabox-core/src/command_executor.rs` lowered from 50 ms to 5 ms.
  This was a diagnostic build and is not committed.
- **c2011a4**: the revision the README numbers were recorded at. It was run with
  `--session-policy policy-v0.1-raised-nproc.toml`, because that revision's
  default `max_processes = 32` makes later commands fail with
  `sh: Cannot fork` in `write-16x32m`.

No-op command, p50 / p95 ms (rows are `cbcow-through` unless labelled):

| Build | 10 files | 200 files | 1000 files |
|---|---:|---:|---:|
| README (other host) | 13.7 / 15.5 | 14.3 / 15.4 | 13.2 / 14.7 |
| c2011a4 | 20.3 / 21.7 | 20.4 / 21.5 | 20.2 / 22.0 |
| v0.4.2 | **57.0 / 57.8** | **57.2 / 58.0** | **57.0 / 58.0** |
| v0.4.2, 5 ms poll | 7.0 / 9.0 | 7.0 / 8.0 | 8.0 / 9.2 |
| **v0.4.2 + watcher fix** | **2.3 / 3.2** | **2.4 / 2.7** | **2.1 / 2.6** |
| raw Sandlock (README / here) | 8.1 / 9.4 · 2.5 / 3.2 | 8.2 / 9.0 · 2.5 / 3.2 | 8.1 / 8.9 · 2.6 / 3.3 |
| plain shell (README / here) | 1.3 / 1.7 · 0.3 / 0.4 | 1.2 / 1.6 · 0.3 / 0.4 | 1.2 / 1.5 · 0.3 / 0.4 |

Writes, p50 / p95 ms:

| Build | 4 KiB in 256 MiB: cbcow | raw Sandlock | 16 × 4 KiB in 32 MiB files: cbcow | raw Sandlock |
|---|---:|---:|---:|---:|
| README (other host) | 46.4 / 47.3 | 166.0 / 187.7 | 509.0 / 509.6 | 4809.5 / 5761.2 |
| c2011a4 | 20.5 / 22.8 | 652.6 / 1907.5 | 332.5 / 515.8 | 1562.7 / 3038.2 |
| v0.4.2 | 59.2 / 60.5 | 277.0 / 668.6 | **916.5 / 921.6** | 1482.0 / 2012.2 |
| v0.4.2, 5 ms poll | 8.9 / 10.1 | 646.8 / 1073.0 | 122.3 / 128.7 | 1650.0 / 2485.0 |
| **v0.4.2 + watcher fix** | **6.3 / 8.4** | 298.4 / 372.3 | **80.9 / 85.7** | 1587.8 / 1989.6 |

Findings:

- **Shapes reproduced.** Overhead is flat from 10 to 1000 files. cbcow beats
  raw Sandlock on both write workloads in every build except v0.4.2 on the
  16-file workload, where it is only 1.6× faster rather than the README's
  ~9×.
- **Regression in v0.4.2.** Each `deltabox exec` costs about 50 ms more than
  it should. `DisconnectWatcher` (added in `f34929b`, 2026-11-06, v0.2.0)
  joins a thread that blocks in `read()` with a 50 ms timeout before the
  result is returned. With a 5 ms poll, a no-op drops from 57 ms to 7 ms,
  below the README's figures. The fix ("fix(command-helper): stop the
  disconnect watcher without waiting out its poll") instead shuts down the watcher's read half before joining it, which wakes
  the blocked read at once. A no-op then costs 2.3 ms, on par with one-shot
  raw Sandlock, while client disconnect still cancels the command.
- **The raw Sandlock baseline is noisy on this host.** It copies the whole
  file up and back for every command, so it depends on page-cache writeback.
  Its p50 for the 256 MiB case was 277 ms in one run and about 650 ms in the
  other two, with p95 reaching 1–3 s. cbcow's spread stays tight. Ratios
  against raw Sandlock carry roughly ±2× uncertainty here.
- **Absolute numbers differ by host.** Raw Sandlock and the plain shell are
  3–4× faster here than on the README host, so compare runs from the same
  machine only.

# Regular-file history restore: validation and performance

Validated on macOS arm64 on 2026-10-23. Local paired packages are core
`0.2.0-history.7` and DSH plugin `0.2.0-history.9`; these are not public releases.

## Scope and safety

The optimization handles content changes to existing ordinary regular files.
It requires unchanged metadata, single-link files, owned writable non-symlink
parents, and staging on the same filesystem. Baseline-only updates must meet
the same requirements. Additions, deletions, directory/type changes, hard links,
ACL/xattr/flag/permission changes, and other complex cases use the existing
full-tree transaction. Initial sandbox setup is unchanged.

Selective backups do not imply selective conflict detection. Full host and
baseline fingerprints remain part of the durable plan. Both trees are still
scanned and synchronized. Prepared replacement files and undo images are flushed
before the transaction marker. Each replacement uses `renameat` through directory
descriptors opened with `O_NOFOLLOW`, then synchronizes both parent directories.
Unchanged generated files are not cloned or replaced. After publication the full
host and baseline are verified before committing the manifest.

Recovery permits each changed file to match either its old or new fingerprint;
every untouched entry must still match. External changes, including ignored data,
stop recovery without overwriting them. Undo can itself be interrupted and retried.
The new transaction mode is deliberately unreadable by older runtimes: an actual
`0.2.0-history.6` native package rejected an incomplete new transaction, preserved
the project, and allowed the new runtime to recover it afterward.

## Bottleneck measurement

The disposable fixture contains one changed source file, 512 ignored 32 KiB object
files, and one ignored 64 MiB file: 513 generated files, 80 MiB total. Timings exclude
fixture creation and session startup. `DELTABOX_HISTORY_TIMINGS=1` writes phase
durations to stderr without changing RPC responses or durability.

An instrumented debug build measured the old full-tree restore at 9.44 seconds.
Each of its three full-tree cloning passes spent approximately 0.24–0.26 seconds
cloning and 1.50–1.55 seconds synchronizing. Host replacement spent approximately
0.38 seconds replacing entries and 1.86 seconds synchronizing. Repeated cloning
and flushing of replacement trees dominated; final indexing/commit took 0.02 seconds.
These diagnostic debug numbers must not be compared directly with release numbers.

## Optimized package comparison

Both release packages used fresh, identical disposable fixtures via
`crates/deltabox-node/tests/history-restore-bench.cjs`. They ran sequentially on the
same machine; these are local measurements, not general latency guarantees.

| Package | Run 1 | Run 2 | Run 3 | Median | Generated file inode retained |
| --- | ---: | ---: | ---: | ---: | --- |
| Core history.6 | 4,264 ms | 1,411 ms | 1,164 ms | 1,411 ms | No |
| Core history.7 | 377 ms | 415 ms | 493 ms | 415 ms | Yes |

Median restore time decreased by approximately 71%. Generated contents were
verified byte-for-byte in all runs. The new path still scales with the files read
and synchronized; it does not promise constant time or remove the cost of cold
session initialization.

Reproduce with absolute addon/helper paths:

```sh
node crates/deltabox-node/tests/history-restore-bench.cjs /path/to/deltabox.node /path/to/deltabox 3 512
DELTABOX_HISTORY_TIMINGS=1 cargo test -p deltabox-core history_restore_profile -- --ignored --nocapture
```

## Validation

- 133 core unit tests passed; the manual performance test is separately ignored.
- New cases cover partial publication, a second crash during undo, corrupted undo
  data, concurrent ignored writes before publication, and failures injected as
  `ENOSPC` at backup, pre-marker, host/base publication, and pre-commit boundaries.
  ENOSPC testing is fault injection, not a physically filled disk or power-cut test.
- Eleven complex-case fixtures verify fallback and subsequent abort semantics.
- Native process recovery passed 16 crash boundaries across trusted and sandboxed
  execution; stdio process recovery passed four boundaries.
- The packaged native runtime passed history tests in both isolation modes and
  over stdio. The actual Harness Electron runtime passed persisted history,
  restore/fork, continuation, restart, and preserved-generated-file checks.
- Plugin typechecking, 81 unit tests and seven release tests passed.

Concurrent CLI development in the checkout was excluded from the installed build.
The build source was exported from commit `0058e26` and overlaid only with this
change's core files, versions, and tests. Local raw logs, the overlay hash manifest,
and package verification evidence are retained under `dist/history-files-*` and
`dist/history-restore-*` in the two repositories.

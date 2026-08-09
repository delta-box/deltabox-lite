# DeltaFS filesystem benchmarks

This directory provides reproducible fio, Filebench, and xfstests runs against
a standalone DeltaFS mount. Every runner creates a new source tree, state
directory, and mountpoint below `/tmp`, then removes them after unmounting.
Result files are written outside the mount and are ignored when stored in
`bench/results/`.

The runners never use an existing project tree or block device. The xfstests
runner supplies no `SCRATCH_DEV`, accepts only an audited list of test-only
`generic` cases, and rejects arbitrary test names. Do not add a test to that
list until its script and sourced helpers have been checked for formatting,
mounting, device, and scratch operations.

## Build DeltaFS

On Linux:

```sh
cargo build --manifest-path deltafs-fuse/Cargo.toml --release --features fuse
```

The runners use `target/release/deltafs-fuse` by default. Set
`DELTAFS_BINARY` to use another binary.

## Build the tools

Distribution packages are sufficient for fio. Filebench and xfstests often
need to be built from source. The build helper uses pinned upstream revisions,
does not install system-wide, and prints the revision used. It builds
Filebench as a non-PIE executable because Filebench workers reuse executable
addresses stored in shared memory:

```sh
./deltafs-fuse/bench/build-tools.sh
./deltafs-fuse/bench/build-tools.sh fio filebench
```

Set `DELTAFS_BENCH_TOOLS` to move source and installed files outside the
repository. Set `FIO_REF`, `FILEBENCH_REF`, or `XFSTESTS_REF` to test another
revision.

Typical Debian or Ubuntu build dependencies are:

```sh
sudo apt-get install build-essential git autoconf automake libtool bison flex \
  acl attr bc dbench dump gawk indent libacl1-dev libaio-dev libcap-dev \
  libgdbm-dev libgdbm-compat-dev libtool-bin liburing-dev lvm2 psmisc \
  python3 quota sqlite3 uuid-dev uuid-runtime xfslibs-dev xfsprogs e2fsprogs fio
```

xfstests has additional optional dependencies. Its upstream `README` remains
the authoritative list for the selected revision.

## fio

The fio suite covers buffered sequential and random I/O, synchronized writes,
direct I/O, concurrent jobs, and metadata create/stat/delete operations:

```sh
./deltafs-fuse/bench/run-fio.sh
DELTAFS_MODE=strict DELTAFS_FIO_SIZE=1g \
  ./deltafs-fuse/bench/run-fio.sh deltafs-fuse/bench/fio/throughput.fio
```

`FIO` selects the executable. `DELTAFS_FIO_SIZE`, `DELTAFS_FIO_RUNTIME`,
`DELTAFS_FIO_JOBS`, and `DELTAFS_FIO_FILES` size the workloads.

## Filebench

The runner uses the file-server, mail-server, and web-server personalities from
the installed pinned Filebench revision:

```sh
FILEBENCH=deltafs-fuse/bench/tools/install/filebench/bin/filebench \
  ./deltafs-fuse/bench/run-filebench.sh
```

`DELTAFS_FILEBENCH_RUNTIME`, `DELTAFS_FILEBENCH_FILES`, and
`DELTAFS_FILEBENCH_THREADS` control their scale. `FILEBENCH_WORKLOADS` selects
another installed workload directory. Filebench can fail during startup when
Linux shared-memory limits are too small; follow the upstream Filebench guidance
for the benchmark host instead of changing the filesystem.

## xfstests

xfstests expects root privileges and a set of test users. The runner mounts
DeltaFS over a disposable directory and passes only `TEST_DEV`, `TEST_DIR`, and
`FSTYP=fuse`. It deliberately omits every scratch setting:

```sh
sudo useradd -m fsgqa
sudo useradd fsgqa2
sudo groupadd fsgqa
sudo -E ./deltafs-fuse/bench/run-xfstests.sh
```

DeltaFS mounts appear as `fuse.deltafs` on Linux. The runner places a local
`df` adapter first in `PATH` so xfstests' generic FUSE initialization sees the
base `fuse` type it expects. The adapter changes only that reported type name.

An individual case can be selected only if it is already in
`xfstests/tests.list` or `xfstests/stress.list`:

```sh
sudo -E ./deltafs-fuse/bench/run-xfstests.sh generic/001 generic/075
```

The default list contains bounded functional checks. The stress list adds
large name, metadata, xattr, fsstress, and fsx sequences and can take much
longer on filesystems with synchronous metadata paths:

```sh
sudo -E DELTAFS_XFSTESTS_SUITE=stress \
  ./deltafs-fuse/bench/run-xfstests.sh
```

Set `XFSTESTS` to an existing built xfstests source tree. Test skips and
failures remain distinct in the xfstests output.

## Modes and results

All runners default to the fast durability mode. Use
`DELTAFS_MODE=strict` for strict storage ordering. Set
`DELTAFS_BENCH_OUTPUT` to a unique directory for each run:

```sh
DELTAFS_BENCH_OUTPUT=/tmp/deltafs-fio-fast ./deltafs-fuse/bench/run-fio.sh
DELTAFS_MODE=strict DELTAFS_BENCH_OUTPUT=/tmp/deltafs-fio-strict \
  ./deltafs-fuse/bench/run-fio.sh
```

Keep the underlying filesystem, tool revisions, workload parameters, CPU
placement, cache state, and host load fixed when comparing runs.

#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
source "$SCRIPT_DIR/lib/deltafs.sh"

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
    echo "Usage: $0 [job.fio ...]"
    echo "Runs every bundled fio job when none are supplied."
    deltafs_usage_common
    exit 0
fi

FIO=${FIO:-$(command -v fio || true)}
if [[ -z "$FIO" ]]; then
    FIO="$SCRIPT_DIR/tools/install/fio/bin/fio"
fi
[[ -x "$FIO" ]] || { echo "fio not found; run bench/build-tools.sh fio or set FIO." >&2; exit 1; }

jobs=("$@")
if [[ ${#jobs[@]} -eq 0 ]]; then
    jobs=("$SCRIPT_DIR"/fio/*.fio)
fi

mkdir -p "$DELTAFS_BENCH_OUTPUT"
deltafs_install_cleanup
deltafs_start
export DELTAFS_BENCH_MOUNT
export DELTAFS_FIO_SIZE=${DELTAFS_FIO_SIZE:-256m}
export DELTAFS_FIO_RUNTIME=${DELTAFS_FIO_RUNTIME:-15}
export DELTAFS_FIO_JOBS=${DELTAFS_FIO_JOBS:-4}
export DELTAFS_FIO_FILES=${DELTAFS_FIO_FILES:-2000}

for job in "${jobs[@]}"; do
    name=$(basename "$job" .fio)
    mkdir -p "$DELTAFS_BENCH_MOUNT/$name"
    export DELTAFS_FIO_DIRECTORY="$DELTAFS_BENCH_MOUNT/$name"
    "$FIO" --output-format=json --output="$DELTAFS_BENCH_OUTPUT/$name.json" "$job"
done

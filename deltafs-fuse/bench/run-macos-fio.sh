#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
source "$SCRIPT_DIR/lib/macos-native.sh"

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
    echo "Usage: $0 [job.fio ...]"
    echo "Runs every bundled fio job through the native macOS provider when none are supplied."
    macos_native_usage
    exit 0
fi

FIO=${FIO:-$(command -v fio || true)}
[[ -x "$FIO" ]] || { echo "fio not found; install it with 'brew install fio' or set FIO." >&2; exit 1; }

jobs=("$@")
if [[ ${#jobs[@]} -eq 0 ]]; then
    jobs=("$SCRIPT_DIR"/fio/*.fio)
fi

macos_native_prepare
macos_native_install_cleanup
mkdir -p "$DELTAFS_BENCH_PROJECT/jobs" "$DELTAFS_BENCH_PROJECT/results"
for job in "${jobs[@]}"; do
    cp "$job" "$DELTAFS_BENCH_PROJECT/jobs/$(basename "$job")"
done
macos_native_start

for job in "${jobs[@]}"; do
    name=$(basename "$job" .fio)
    macos_native_exec -- /bin/mkdir -p "$name"
    macos_native_exec \
        --env "DELTAFS_FIO_DIRECTORY=$name" \
        --env "DELTAFS_FIO_SIZE=${DELTAFS_FIO_SIZE:-256m}" \
        --env "DELTAFS_FIO_RUNTIME=${DELTAFS_FIO_RUNTIME:-15}" \
        --env "DELTAFS_FIO_JOBS=${DELTAFS_FIO_JOBS:-4}" \
        --env "DELTAFS_FIO_FILES=${DELTAFS_FIO_FILES:-2000}" \
        -- "$FIO" --alloc-size="${DELTAFS_FIO_ALLOC_KB:-2048}" \
        --output-format=json --output="results/$name.json" "jobs/$(basename "$job")"
done

macos_native_commit
cp "$DELTAFS_BENCH_PROJECT"/results/*.json "$DELTAFS_BENCH_OUTPUT/"

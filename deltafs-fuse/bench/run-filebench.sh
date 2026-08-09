#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
source "$SCRIPT_DIR/lib/deltafs.sh"

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
    echo "Usage: $0 [workload.f ...]"
    echo "Runs the installed fileserver, varmail, and webserver personalities by default."
    deltafs_usage_common
    exit 0
fi

FILEBENCH=${FILEBENCH:-$(command -v filebench || true)}
if [[ -z "$FILEBENCH" ]]; then
    FILEBENCH="$SCRIPT_DIR/tools/install/filebench/bin/filebench"
fi
[[ -x "$FILEBENCH" ]] || { echo "Filebench not found; run bench/build-tools.sh filebench or set FILEBENCH." >&2; exit 1; }
FILEBENCH_ROOT=$(cd -- "$(dirname -- "$FILEBENCH")/.." && pwd)
FILEBENCH_WORKLOADS=${FILEBENCH_WORKLOADS:-"$FILEBENCH_ROOT/share/filebench/workloads"}

workloads=("$@")
if [[ ${#workloads[@]} -eq 0 ]]; then
    workloads=(
        "$FILEBENCH_WORKLOADS/fileserver.f"
        "$FILEBENCH_WORKLOADS/varmail.f"
        "$FILEBENCH_WORKLOADS/webserver.f"
    )
fi

mkdir -p "$DELTAFS_BENCH_OUTPUT"
deltafs_install_cleanup
deltafs_start

for workload in "${workloads[@]}"; do
    name=$(basename "$workload" .f)
    root="$DELTAFS_BENCH_MOUNT/$name"
    rendered="$DELTAFS_BENCH_RUN/$name.f"
    mkdir -p "$root"
    sed \
        -e 's|^set \$dir=.*$|set $dir='"$root"'|' \
        -e 's|^set \$nfiles=.*$|set $nfiles='"${DELTAFS_FILEBENCH_FILES:-1000}"'|' \
        -e 's|^set \$nthreads=.*$|set $nthreads='"${DELTAFS_FILEBENCH_THREADS:-4}"'|' \
        -e 's|^set \$runtime=.*$|set $runtime='"${DELTAFS_FILEBENCH_RUNTIME:-15}"'|' \
        -e 's|^run [[:space:]]*[0-9][0-9]*$|run '"${DELTAFS_FILEBENCH_RUNTIME:-15}"'|' \
        "$workload" >"$rendered"
    log="$DELTAFS_BENCH_OUTPUT/$name.log"
    "$FILEBENCH" -f "$rendered" >"$log" 2>&1
    if grep -Eq 'syntax error|Unexpected Process termination|no run command|NO VALID RESULTS' "$log"; then
        echo "Filebench rejected $name; see $log" >&2
        exit 1
    fi
done

#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
source "$SCRIPT_DIR/lib/deltafs.sh"

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
    echo "Usage: $0 [generic/NNN ...]"
    echo "Runs the reviewed test-only list when no tests are supplied."
    echo "Set DELTAFS_XFSTESTS_SUITE=stress to include long stress cases."
    echo "Run as root; no scratch device is accepted or configured."
    deltafs_usage_common
    exit 0
fi

if [[ $EUID -ne 0 ]]; then
    echo "xfstests must run as root. Use sudo -E $0." >&2
    exit 1
fi

XFSTESTS=${XFSTESTS:-"$SCRIPT_DIR/tools/src/xfstests"}
[[ -x "$XFSTESTS/check" ]] || { echo "xfstests not found; run bench/build-tools.sh xfstests or set XFSTESTS." >&2; exit 1; }

tests=("$@")
if [[ ${#tests[@]} -eq 0 ]]; then
    lists=("$SCRIPT_DIR/xfstests/tests.list")
    case "${DELTAFS_XFSTESTS_SUITE:-default}" in
        default) ;;
        stress) lists+=("$SCRIPT_DIR/xfstests/stress.list") ;;
        *) echo "DELTAFS_XFSTESTS_SUITE must be default or stress." >&2; exit 1 ;;
    esac
    mapfile -t tests < <(sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "${lists[@]}")
fi

for test in "${tests[@]}"; do
    [[ $test =~ ^generic/[0-9]{3}$ ]] || { echo "Only explicit generic/NNN tests are accepted: $test" >&2; exit 1; }
    grep -qxF "$test" "$SCRIPT_DIR/xfstests/tests.list" "$SCRIPT_DIR/xfstests/stress.list" || { echo "Test is not in a reviewed list: $test" >&2; exit 1; }
done

mkdir -p "$DELTAFS_BENCH_OUTPUT"
export DELTAFS_ALLOW_OTHER=1
deltafs_install_cleanup
deltafs_start

config="$DELTAFS_BENCH_RUN/xfstests.config"
cat >"$config" <<EOF
export EMAIL=root@localhost
export TEST_DEV=deltafs-fuse
export TEST_DIR=$DELTAFS_BENCH_MOUNT
export FSTYP=fuse
export RESULT_BASE=$DELTAFS_BENCH_OUTPUT/xfstests
export KEEP_DMESG=no
EOF

(cd "$XFSTESTS" && PATH="$SCRIPT_DIR/xfstests/bin:$PATH" HOST_OPTIONS="$config" ./check -fuse "${tests[@]}") \
    2>&1 | tee "$DELTAFS_BENCH_OUTPUT/xfstests.log"

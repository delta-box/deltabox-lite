#!/usr/bin/env bash

set -euo pipefail

DELTAFS_BENCH_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
DELTAFS_ROOT=$(cd -- "$DELTAFS_BENCH_DIR/.." && pwd)
DELTAFS_REPOSITORY=$(cd -- "$DELTAFS_ROOT/.." && pwd)
DELTAFS_BINARY=${DELTAFS_BINARY:-"$DELTAFS_REPOSITORY/target/release/deltafs-fuse"}
DELTAFS_MODE=${DELTAFS_MODE:-fast}
DELTAFS_BENCH_OUTPUT=${DELTAFS_BENCH_OUTPUT:-"$DELTAFS_BENCH_DIR/results"}

deltafs_usage_common() {
    cat <<'EOF'
Common environment:
  DELTAFS_BINARY       deltafs-fuse binary to run
  DELTAFS_MODE         fast or strict
  DELTAFS_BENCH_OUTPUT result directory, outside the disposable mount
  DELTAFS_BENCH_TMPDIR parent for disposable source, state, and mount directories
EOF
}

deltafs_require_linux() {
    if [[ $(uname -s) != Linux ]]; then
        echo "This benchmark runner requires Linux." >&2
        exit 1
    fi
}

deltafs_require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "Required command not found: $1" >&2
        exit 1
    }
}

deltafs_prepare_binary() {
    if [[ ! -x "$DELTAFS_BINARY" ]]; then
        cargo build --manifest-path "$DELTAFS_ROOT/Cargo.toml" --release --features fuse
    fi
}

deltafs_start() {
    deltafs_require_linux
    deltafs_prepare_binary
    deltafs_require_command fusermount3

    mkdir -p "$DELTAFS_BENCH_OUTPUT"
    DELTAFS_BENCH_RUN=$(mktemp -d "${DELTAFS_BENCH_TMPDIR:-/tmp}/deltafs-bench.XXXXXX")
    DELTAFS_BENCH_PRIVATE="$DELTAFS_BENCH_RUN/private"
    DELTAFS_BENCH_SOURCE="$DELTAFS_BENCH_PRIVATE/source"
    DELTAFS_BENCH_MOUNT="$DELTAFS_BENCH_RUN/mount"
    DELTAFS_BENCH_STATE="$DELTAFS_BENCH_PRIVATE/state"
    DELTAFS_BENCH_CONTROL="$DELTAFS_BENCH_PRIVATE/control"
    DELTAFS_BENCH_SOCKET="$DELTAFS_BENCH_CONTROL/control.sock"
    DELTAFS_BENCH_TOKEN="$DELTAFS_BENCH_CONTROL/token"
    DELTAFS_BENCH_READY="$DELTAFS_BENCH_CONTROL/ready"
    DELTAFS_BENCH_LOG="$DELTAFS_BENCH_OUTPUT/deltafs.log"
    umask 077
    mkdir -p "$DELTAFS_BENCH_SOURCE" "$DELTAFS_BENCH_MOUNT" "$DELTAFS_BENCH_STATE" "$DELTAFS_BENCH_CONTROL"
    chmod 0711 "$DELTAFS_BENCH_RUN"
    chmod 0755 "$DELTAFS_BENCH_SOURCE" "$DELTAFS_BENCH_MOUNT"
    od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$DELTAFS_BENCH_TOKEN"

    local durability=()
    local sharing=()
    case "$DELTAFS_MODE" in
        fast) ;;
        strict) durability=(--durable) ;;
        *) echo "DELTAFS_MODE must be fast or strict." >&2; exit 1 ;;
    esac
    if [[ ${DELTAFS_ALLOW_OTHER:-0} == 1 ]]; then
        sharing=(--allow-other)
    fi

    "$DELTAFS_BINARY" serve \
        --source "$DELTAFS_BENCH_SOURCE" \
        --mountpoint "$DELTAFS_BENCH_MOUNT" \
        --state-dir "$DELTAFS_BENCH_STATE" \
        --socket "$DELTAFS_BENCH_SOCKET" \
        --token-file "$DELTAFS_BENCH_TOKEN" \
        --ready-file "$DELTAFS_BENCH_READY" \
        "${durability[@]}" "${sharing[@]}" >"$DELTAFS_BENCH_LOG" 2>&1 &
    DELTAFS_BENCH_PID=$!

    local attempt
    for attempt in $(seq 1 200); do
        if [[ -f "$DELTAFS_BENCH_READY" ]] && mountpoint -q "$DELTAFS_BENCH_MOUNT"; then
            return
        fi
        if ! kill -0 "$DELTAFS_BENCH_PID" 2>/dev/null; then
            wait "$DELTAFS_BENCH_PID" || true
            echo "DeltaFS exited before the mount became ready; see $DELTAFS_BENCH_LOG" >&2
            exit 1
        fi
        sleep 0.05
    done
    echo "Timed out waiting for the DeltaFS mount; see $DELTAFS_BENCH_LOG" >&2
    exit 1
}

deltafs_stop() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n ${DELTAFS_BENCH_PID:-} ]]; then
        "$DELTAFS_BINARY" control \
            --socket "$DELTAFS_BENCH_SOCKET" \
            --token-file "$DELTAFS_BENCH_TOKEN" \
            shutdown >/dev/null 2>&1 || true
        wait "$DELTAFS_BENCH_PID" 2>/dev/null || true
    fi
    if [[ -n ${DELTAFS_BENCH_MOUNT:-} ]] && mountpoint -q "$DELTAFS_BENCH_MOUNT"; then
        fusermount3 -u "$DELTAFS_BENCH_MOUNT" || true
    fi
    if [[ -n ${DELTAFS_BENCH_RUN:-} ]]; then
        rm -rf -- "$DELTAFS_BENCH_RUN"
    fi
    exit "$status"
}

deltafs_install_cleanup() {
    trap deltafs_stop EXIT INT TERM
}

#!/usr/bin/env bash

set -euo pipefail

DELTAFS_BENCH_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
DELTAFS_ROOT=$(cd -- "$DELTAFS_BENCH_DIR/.." && pwd)
DELTAFS_REPOSITORY=$(cd -- "$DELTAFS_ROOT/.." && pwd)
DELTABOX_BINARY=${DELTABOX_BINARY:-"$DELTAFS_REPOSITORY/target/release/deltabox"}
DELTAFS_MODE=${DELTAFS_MODE:-fast}
DELTAFS_FS_MODE=${DELTAFS_FS_MODE:-cbcow-through}
DELTAFS_BENCH_OUTPUT=${DELTAFS_BENCH_OUTPUT:-"$DELTAFS_BENCH_DIR/results"}

macos_native_usage() {
    cat <<'EOF'
macOS native environment:
  DELTABOX_BINARY       deltabox binary to run
  DELTAFS_FS_MODE       cbcow-through or cbcow-private
  DELTAFS_MODE          fast or strict
  DELTAFS_BENCH_OUTPUT  result directory outside the disposable project
EOF
}

macos_native_prepare() {
    [[ $(uname -s) == Darwin ]] || { echo "This benchmark runner requires macOS." >&2; exit 1; }
    command -v python3 >/dev/null 2>&1 || { echo "Required command not found: python3" >&2; exit 1; }
    case "$DELTAFS_FS_MODE" in
        cbcow-through|cbcow-private) ;;
        *) echo "DELTAFS_FS_MODE must be cbcow-through or cbcow-private." >&2; exit 1 ;;
    esac
    case "$DELTAFS_MODE" in
        fast|strict) ;;
        *) echo "DELTAFS_MODE must be fast or strict." >&2; exit 1 ;;
    esac
    if [[ ! -x "$DELTABOX_BINARY" ]]; then
        cargo build --manifest-path "$DELTAFS_REPOSITORY/Cargo.toml" --release -p deltabox
    fi
    mkdir -p "$DELTAFS_BENCH_OUTPUT"
    DELTAFS_BENCH_RUN=$(mktemp -d /private/tmp/deltafs-macos-bench.XXXXXX)
    DELTAFS_BENCH_PROJECT="$DELTAFS_BENCH_RUN/project"
    DELTAFS_BENCH_STATE="$DELTAFS_BENCH_RUN/state"
    mkdir -p "$DELTAFS_BENCH_PROJECT" "$DELTAFS_BENCH_STATE"
}

macos_native_start() {
    local args=(session start "$DELTAFS_BENCH_PROJECT" --fs-mode "$DELTAFS_FS_MODE" --require-fs-mode)
    if [[ "$DELTAFS_MODE" == strict ]]; then
        args+=(--durable)
    fi
    DELTAFS_SESSION_ID=$(
        DELTABOX_STATE_DIR="$DELTAFS_BENCH_STATE" "$DELTABOX_BINARY" "${args[@]}"
    )
    DELTAFS_SESSION_ID=${DELTAFS_SESSION_ID%%$'\n'*}
}

macos_native_exec() {
    local result="$DELTAFS_BENCH_RUN/exec.json"
    DELTABOX_STATE_DIR="$DELTAFS_BENCH_STATE" "$DELTABOX_BINARY" exec \
        "$DELTAFS_SESSION_ID" --project "$DELTAFS_BENCH_PROJECT" "$@" >"$result"
    python3 - "$result" <<'PY'
import json
import pathlib
import sys

result = json.loads(pathlib.Path(sys.argv[1]).read_text())
if result["exit_code"] != 0:
    sys.stderr.write(result["stdout"])
    sys.stderr.write(result["stderr"])
    if not result["stdout"] and not result["stderr"]:
        sys.stderr.write(f"sandboxed command exited with {result['exit_code']}\n")
    raise SystemExit(result["exit_code"])
PY
}

macos_native_commit() {
    DELTABOX_STATE_DIR="$DELTAFS_BENCH_STATE" "$DELTABOX_BINARY" commit \
        "$DELTAFS_SESSION_ID" --project "$DELTAFS_BENCH_PROJECT" >/dev/null
}

macos_native_cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n ${DELTAFS_SESSION_ID:-} ]]; then
        DELTABOX_STATE_DIR="$DELTAFS_BENCH_STATE" "$DELTABOX_BINARY" session stop \
            "$DELTAFS_SESSION_ID" --project "$DELTAFS_BENCH_PROJECT" >/dev/null 2>&1 || true
    fi
    if [[ -n ${DELTAFS_BENCH_RUN:-} && ${DELTAFS_KEEP_RUN:-0} != 1 ]]; then
        rm -rf -- "$DELTAFS_BENCH_RUN"
    elif [[ -n ${DELTAFS_BENCH_RUN:-} ]]; then
        echo "preserved benchmark directory: $DELTAFS_BENCH_RUN" >&2
    fi
    exit "$status"
}

macos_native_install_cleanup() {
    trap macos_native_cleanup EXIT INT TERM
}

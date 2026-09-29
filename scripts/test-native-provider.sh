#!/usr/bin/env bash
# Require real CB-CoW transactions before suites with availability guards run.
set -euo pipefail

repo_dir=$(cd "$(dirname "$0")/.." && pwd)
binary="$repo_dir/target/debug/deltabox"
"$repo_dir/target/debug/deltafs-fuse" doctor --json
project=$(mktemp -d)
session=''
cleanup() {
    if [[ -n "$session" ]]; then
        "$binary" session stop "$session" --project "$project" || true
    fi
    rm -rf "$project"
}
trap cleanup EXIT

for mode in cbcow-through cbcow-private; do
    session=$("$binary" session start "$project" --fs-mode "$mode" --require-fs-mode)
    "$binary" exec "$session" --project "$project" -- sh -c 'printf accepted > result.txt'
    "$binary" commit "$session" --project "$project"
    test "$(cat "$project/result.txt")" = accepted
    "$binary" exec "$session" --project "$project" -- sh -c 'printf pending > result.txt'
    "$binary" abort "$session" --project "$project"
    test "$(cat "$project/result.txt")" = accepted
    "$binary" session stop "$session" --project "$project"
    session=''
    rm "$project/result.txt"
    echo "$mode: native commit and abort passed"
done

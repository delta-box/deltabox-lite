#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
TOOLS_DIR=${DELTAFS_BENCH_TOOLS:-"$SCRIPT_DIR/tools"}
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}

FIO_REPOSITORY=${FIO_REPOSITORY:-https://github.com/axboe/fio.git}
FIO_REF=${FIO_REF:-1de4a3dc5d10a0107b51c496dc96a07f58bdc5c9}
FILEBENCH_REPOSITORY=${FILEBENCH_REPOSITORY:-https://github.com/filebench/filebench.git}
FILEBENCH_REF=${FILEBENCH_REF:-22620e602cbbebad90c0bd041896ebccf70dbf5f}
XFSTESTS_REPOSITORY=${XFSTESTS_REPOSITORY:-https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git}
XFSTESTS_REF=${XFSTESTS_REF:-acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509}

usage() {
    cat <<EOF
Usage: $0 [fio|filebench|xfstests ...]

Builds all three tools when no names are supplied.

Environment:
  DELTAFS_BENCH_TOOLS  source and install root (default: $TOOLS_DIR)
  JOBS                 parallel build jobs
  FIO_REF, FILEBENCH_REF, XFSTESTS_REF
                       upstream revision overrides
EOF
}

checkout() {
    local repository=$1
    local revision=$2
    local destination=$3
    if [[ ! -d "$destination/.git" ]]; then
        mkdir -p "$destination"
        git -C "$destination" init
        git -C "$destination" remote add origin "$repository"
    fi
    git -C "$destination" fetch --depth=1 origin "$revision"
    git -C "$destination" checkout --detach FETCH_HEAD
}

build_fio() {
    local source="$TOOLS_DIR/src/fio"
    local prefix="$TOOLS_DIR/install/fio"
    checkout "$FIO_REPOSITORY" "$FIO_REF" "$source"
    (cd "$source" && ./configure --prefix="$prefix" && make -j"$JOBS" && make install)
    echo "fio $(git -C "$source" rev-parse HEAD): $prefix/bin/fio"
}

build_filebench() {
    local source="$TOOLS_DIR/src/filebench"
    local prefix="$TOOLS_DIR/install/filebench"
    checkout "$FILEBENCH_REPOSITORY" "$FILEBENCH_REF" "$source"
    (cd "$source" && libtoolize && aclocal && autoheader && automake --add-missing && autoconf)
    (cd "$source" && CFLAGS="${CFLAGS:--O2} -fno-pie" LDFLAGS="${LDFLAGS:-} -no-pie" \
        ./configure --prefix="$prefix" && make clean && make -j"$JOBS" && make install)
    echo "Filebench $(git -C "$source" rev-parse HEAD): $prefix/bin/filebench"
}

build_xfstests() {
    local source="$TOOLS_DIR/src/xfstests"
    checkout "$XFSTESTS_REPOSITORY" "$XFSTESTS_REF" "$source"
    (cd "$source" && make -j"$JOBS")
    echo "xfstests $(git -C "$source" rev-parse HEAD): $source/check"
}

mkdir -p "$TOOLS_DIR/src" "$TOOLS_DIR/install"
if [[ $# -eq 0 ]]; then
    targets=(fio filebench xfstests)
else
    targets=("$@")
fi
for target in "${targets[@]}"; do
    case "$target" in
        fio) build_fio ;;
        filebench) build_filebench ;;
        xfstests) build_xfstests ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown tool: $target" >&2; usage >&2; exit 1 ;;
    esac
done

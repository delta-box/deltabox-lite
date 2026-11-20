#!/usr/bin/env python3
"""Experiment 2: modification-size and file-size scalability.

Case 2A: overwrite 4 KiB at file_size/2 of a file of 64 KiB .. 256 MiB, then
         commit (and, separately, abort).
Case 2B: overwrite 4 KiB .. 64 MiB at offset 64 MiB of a 256 MiB file, then
         commit.

Systems (system / mode / fsync):
  bare      host          off|on   dd straight on the host file; fsync=on adds
                                   conv=fsync. No isolation, nothing to commit.
  reflink   reflink-xfs   off|on   block-level CoW copy-up on XFS: clone the
                                   file with `cp --reflink=always`, edit the
                                   clone; commit renames it over the original,
                                   abort deletes it. Runs only when --xfs-dir
                                   is a reflink-capable mount. No sandbox.
  sandlock  file-copy-up  off      raw_sandlock: fresh sandlock-core sandbox,
                                   seccomp COW copies the whole file up on the
                                   first write-open; commit copies it back.
  deltabox  cbcow-through off|on   one warm session per file; fsync=on is
  deltabox  cbcow-private off|on   `session start --durable`.

The sandboxed edit is `dd if=<payload> of=data.bin bs=.. seek=<off>
oflag=seek_bytes conv=notrunc`, with two alternating payloads in a /tmp
directory so every iteration really changes the bytes. Payload reads come from
tmpfs and cost the same for every system.

Per iteration: write_ms (the edit, including any copy-up), finalize_ms
(commit/abort), total_ms, disk_write_bytes (see benchlib), peak_cow_bytes:
  deltabox  growth of the session's engine state + private tree from before
            the edit to after it (before commit/abort)
  sandlock  allocated size of the COW upper layer before commit/abort
  reflink   drop in the XFS free space while the clone exists
  bare      0
"""

import argparse
import csv
import json
import os
import shutil
import statistics
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from benchlib import (KiB, MiB, Bare, Deltabox, RawSandlock, children_write_bytes, env_info,
                      dd_write, make_random_file, copy_file, now, ms, run, timed,
                      reflink_supported, REPO)

FIELDS = ["case", "system", "mode", "fsync", "file_size_bytes", "write_size_bytes", "outcome",
          "iter", "write_ms", "finalize_ms", "total_ms", "disk_write_bytes", "peak_cow_bytes"]

SIZES_2A = [64 * KiB, 1 * MiB, 16 * MiB, 64 * MiB, 256 * MiB]
WRITES_2B = [4 * KiB, 64 * KiB, 1 * MiB, 16 * MiB, 64 * MiB]
FILE_2B, OFFSET_2B = 256 * MiB, 64 * MiB


class Reflink:
    system, mode = "reflink", "reflink-xfs"

    def __init__(self, project, fsync):
        self.project, self.fsync = Path(project), fsync
        self.upper = self.project / ".upper"
        self.upper.mkdir(exist_ok=True)

    def free(self):
        st = os.statvfs(self.project)
        return st.f_bfree * st.f_frsize

    def iteration(self, payload, write_size, offset, outcome):
        clone = self.upper / "data.bin"
        dd = " ".join(dd_write(payload, str(clone), write_size, offset, self.fsync))
        free0 = self.free()
        t, _ = timed(["sh", "-c", f"cp --reflink=always data.bin {clone} && {dd}"],
                     cwd=self.project)
        # XFS delays allocation; flush the clone (untimed) so the free-space
        # drop shows the blocks the copy-on-write actually allocated.
        run(["sync", "-f", str(clone)])
        cow = max(0, free0 - self.free())
        t0 = now()
        if outcome == "commit":
            os.rename(clone, self.project / "data.bin")
            if self.fsync:
                fd = os.open(self.project, os.O_RDONLY)
                os.fsync(fd)
                os.close(fd)
        else:
            os.unlink(clone)
        return t, ms(t0, now()), cow


def read_range(path, offset, size):
    with open(path, "rb") as f:
        f.seek(offset)
        return f.read(size)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--workdir", type=Path, default=REPO / "target/bench-exp2")
    ap.add_argument("--xfs-dir", type=Path, default=None,
                    help="reflink-capable directory for the reflink baseline")
    ap.add_argument("--payload-dir", type=Path, default=None,
                    help="where the write payloads live (default: a new dir in /tmp)")
    ap.add_argument("--out", type=Path, default=REPO / "bench/delta-driven/results/exp2_scalability.csv")
    ap.add_argument("--iters", type=int, default=20)
    ap.add_argument("--warmups", type=int, default=3)
    ap.add_argument("--no-sync", action="store_true",
                    help="do not flush dirty data before each iteration")
    ap.add_argument("--cases", default="2A,2B")
    ap.add_argument("--systems", default="bare,reflink,sandlock,cbcow-through,cbcow-private")
    a = ap.parse_args()

    cases = set(a.cases.split(","))
    systems = set(a.systems.split(","))
    a.workdir.mkdir(parents=True, exist_ok=True)
    a.out.parent.mkdir(parents=True, exist_ok=True)
    payload_dir = Path(tempfile.mkdtemp(prefix="deltabox-exp2-payload-",
                                        dir=str(a.payload_dir or "/tmp")))
    root = Path(tempfile.mkdtemp(prefix="run-", dir=a.workdir))
    xroot = None
    if "reflink" in systems:
        if a.xfs_dir and reflink_supported(a.xfs_dir):
            xroot = Path(tempfile.mkdtemp(prefix="run-", dir=a.xfs_dir))
        else:
            print("reflink baseline skipped: no reflink-capable --xfs-dir", file=sys.stderr)
            systems.discard("reflink")

    meta = {"env": env_info(a.workdir), "xfs": str(a.xfs_dir) if xroot else None,
            "iters": a.iters, "warmups": a.warmups, "started": time.strftime("%F %T")}
    print(json.dumps(meta), file=sys.stderr)

    # Payloads: two variants per write size so consecutive iterations differ.
    payloads = {}
    for w in sorted(set(WRITES_2B) | {4 * KiB}):
        for v in (0, 1):
            p = payload_dir / f"pay-{w}-{v}"
            make_random_file(p, w, seed=1000 + 2 * w + v)
            payloads[(w, v)] = p

    masters = {}
    master_dirs = {"ext4": root / "masters"}
    if xroot:
        master_dirs["xfs"] = xroot / "masters"
    for fs, d in master_dirs.items():
        d.mkdir()
        for s in sorted(set(SIZES_2A) | {FILE_2B}):
            p = d / f"data-{s}.bin"
            make_random_file(p, s, seed=s)
            masters[(fs, s)] = p
    os.sync()

    configs = []
    if "bare" in systems:
        configs += [("bare", "host", "off"), ("bare", "host", "on")]
    if "reflink" in systems:
        configs += [("reflink", "reflink-xfs", "off"), ("reflink", "reflink-xfs", "on")]
    if "sandlock" in systems:
        configs += [("sandlock", "file-copy-up", "off")]
    for m in ("cbcow-through", "cbcow-private"):
        if m in systems:
            configs += [("deltabox", m, "off"), ("deltabox", m, "on")]

    out = open(a.out, "w", newline="")
    w = csv.DictWriter(out, fieldnames=FIELDS)
    w.writeheader()

    def make_runner(system, mode, fsync, project):
        if system == "bare":
            return Bare(project, fsync == "on")
        if system == "reflink":
            return Reflink(project, fsync == "on")
        if system == "sandlock":
            return RawSandlock(project, project.parent / f"{project.name}-cow")
        return Deltabox(project, mode, fsync == "on", project.parent / f"{project.name}-state")

    def one(r, case, system, mode, fsync, fsize, wsize, offset, outcome, it):
        pay = payloads[(wsize, it % 2)]
        target = r.project / "data.bin"
        verify = it in (0, a.iters - 1)
        before = read_range(target, offset, wsize) if verify else None
        if not a.no_sync:
            # Start every iteration with a clean (but warm) page cache: earlier
            # iterations' writeback cannot leak into this timing, and rewriting
            # still-dirty pages cannot hide bytes from the I/O counters.
            os.sync()
        io0 = r.io_bytes() if system != "reflink" else children_write_bytes()
        if system == "reflink":
            wms, fms, cow = r.iteration(pay, wsize, offset, outcome)
        elif system == "sandlock":
            wms, fms, cow = r.run_once(dd_write(pay, "data.bin", wsize, offset), outcome)
        elif system == "bare":
            wms = r.exec(dd_write(pay, "data.bin", wsize, offset, fsync == "on"))
            fms, cow = 0.0, 0
        else:
            c0 = r.cow_bytes()
            wms = r.exec(dd_write(pay, "data.bin", wsize, offset))
            cow = max(0, r.cow_bytes() - c0)
            fms = r.finalize(outcome)
        io1 = r.io_bytes() if system != "reflink" else children_write_bytes()
        if verify:
            after = read_range(target, offset, wsize)
            want = pay.read_bytes() if outcome == "commit" else before
            if after != want:
                raise SystemExit(f"{system}/{mode} {case} {outcome}: host file has wrong bytes "
                                 f"after iteration {it}")
        if it >= 0:
            w.writerow({"case": case, "system": system, "mode": mode, "fsync": fsync,
                        "file_size_bytes": fsize, "write_size_bytes": wsize, "outcome": outcome,
                        "iter": it, "write_ms": round(wms, 3), "finalize_ms": round(fms, 3),
                        "total_ms": round(wms + fms, 3), "disk_write_bytes": io1 - io0,
                        "peak_cow_bytes": cow})
        return wms + fms

    def sweep(case, system, mode, fsync, fsize, points):
        fs = "xfs" if system == "reflink" else "ext4"
        base = xroot if fs == "xfs" else root
        project = base / f"{case}-{system}-{mode}-{fsync}-{fsize}"
        project.mkdir()
        copy_file(masters[(fs, fsize)], project / "data.bin")
        os.sync()
        r = make_runner(system, mode, fsync, project)
        if system == "deltabox":
            r.start()
        try:
            for wsize, offset, outcome in points:
                for i in range(-a.warmups, a.iters):
                    one(r, case, system, mode, fsync, fsize, wsize, offset, outcome, i)
                rows = f"{case} {system:8} {mode:13} fsync={fsync:3} file={fsize:>10} " \
                       f"write={wsize:>9} {outcome}"
                print(rows, file=sys.stderr, flush=True)
        finally:
            if system == "deltabox":
                r.stop()
            shutil.rmtree(project, ignore_errors=True)
            for extra in (project.parent / f"{project.name}-cow",
                          project.parent / f"{project.name}-state"):
                shutil.rmtree(extra, ignore_errors=True)
        out.flush()

    try:
        for system, mode, fsync in configs:
            outcomes = ["commit"] if system == "bare" else ["commit", "abort"]
            if "2A" in cases:
                for fsize in SIZES_2A:
                    off = (fsize // 2) // 4096 * 4096
                    sweep("2A", system, mode, fsync, fsize,
                          [(4 * KiB, off, o) for o in outcomes])
            if "2B" in cases:
                sweep("2B", system, mode, fsync, FILE_2B,
                      [(ws, OFFSET_2B, "commit") for ws in WRITES_2B])
    finally:
        out.close()
        shutil.rmtree(root, ignore_errors=True)
        if xroot:
            shutil.rmtree(xroot, ignore_errors=True)
        shutil.rmtree(payload_dir, ignore_errors=True)
        meta["finished"] = time.strftime("%F %T")
        a.out.with_suffix(".meta.json").write_text(json.dumps(meta, indent=2))
    print(f"wrote {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()

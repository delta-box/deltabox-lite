#!/usr/bin/env python3
"""Reproduce the README "Performance" tables (Linux).

Runners
  cbcow    -- `deltabox` CLI over ONE persistent `--fs-mode cbcow-through`
              session per project; every command / commit is a separate CLI
              process (`deltabox exec`, `deltabox commit`).
  sandlock -- `raw_sandlock` (crates/deltabox-core/examples/raw_sandlock.rs):
              a fresh sandlock-core sandbox per command, seccomp-COW branch
              (`workdir` = project) committed back to the project on exit.
  shell    -- the same command run unsandboxed via `sh -c` (no-op table only).

Workloads (each sample is the wall time of the listed external processes)
  noop-N        project of N small files; one sample = one no-op command
                (`sh -c :`). cbcow: `deltabox exec`; sandlock: one run
                (including its empty commit); shell: `sh -c :`.
  write-256m    project holding one 256 MiB file; one sample = one command that
                overwrites 4 KiB in place (`head -c 4096 /dev/urandom | dd
                conv=notrunc seek=K`) and makes it durable in the project.
                cbcow: `deltabox exec` + `deltabox commit`; sandlock: one run
                (copy-up of the whole file, then commit).
  write-16x32m  project holding sixteen 32 MiB files; one sample = 16 commands,
                command i overwrites 4 KiB in file i, then the result is in the
                project. cbcow: 16 x `deltabox exec` + ONE `deltabox commit`;
                sandlock: 16 runs, each committing independently (it has no
                cross-command session).

Every workload runs `--warmups` unmeasured samples, then `--runs` measured
ones; p50/p95 use linear interpolation. Before timing, each workload checks
once that the written bytes actually reached the host project file.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MiB = 1024 * 1024


def pct(xs, p):
    xs = sorted(xs)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def run(argv, check=True):
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and p.returncode != 0:
        sys.exit(f"command failed ({p.returncode}): {argv}\n{p.stderr.decode()}")
    return p


def write_cmd(name, block):
    return ["sh", "-c",
            f"head -c 4096 /dev/urandom | dd of={name} bs=4096 seek={block} "
            "conv=notrunc status=none"]


def make_file(path, size):
    # Real (non-sparse) data so a whole-file copy-up has bytes to move.
    chunk = os.urandom(MiB)
    with open(path, "wb") as f:
        for _ in range(size // MiB):
            f.write(chunk)
    os.sync()


def block_digest(path, block):
    with open(path, "rb") as f:
        f.seek(block * 4096)
        return hashlib.sha256(f.read(4096)).hexdigest()


class Cbcow:
    name = "cbcow-through"

    def __init__(self, deltabox, project, policy=None):
        self.bin, self.project = str(deltabox), str(project)
        extra = ["--policy", str(policy)] if policy else []
        self.sid = run([self.bin, "session", "start", self.project,
                        "--fs-mode", "cbcow-through", "--require-fs-mode", *extra]
                       ).stdout.decode().strip()

    def exec(self, argv):
        p = run([self.bin, "exec", self.sid, "--project", self.project, "--", *argv])
        if json.loads(p.stdout)["exit_code"] != 0:
            sys.exit(f"sandboxed command failed: {argv}\n{p.stdout.decode()}")

    def commit(self):
        run([self.bin, "commit", self.sid, "--project", self.project])

    def close(self):
        run([self.bin, "session", "stop", self.sid, "--project", self.project],
            check=False)


class RawSandlock:
    name = "raw Sandlock"

    def __init__(self, runner, project, storage):
        self.bin, self.project, self.storage = str(runner), str(project), str(storage)

    def exec(self, argv):  # one-shot: run + commit
        run([self.bin, self.project, self.storage, "--", *argv])

    def commit(self):
        pass

    def close(self):
        pass


class Shell:
    name = "plain shell"

    def __init__(self, project):
        self.project = project

    def exec(self, argv):
        subprocess.run(argv, cwd=self.project, check=True)

    def commit(self):
        pass

    def close(self):
        pass


def measure(sample, warmups, runs):
    for i in range(warmups):
        sample(i)
    out = []
    for i in range(warmups, warmups + runs):
        t = time.perf_counter_ns()
        sample(i)
        out.append((time.perf_counter_ns() - t) / 1e6)
    return out


def summary(xs):
    return {"p50_ms": round(pct(xs, 50), 1), "p95_ms": round(pct(xs, 95), 1),
            "mean_ms": round(statistics.fmean(xs), 1), "n": len(xs)}


def env_info(work, deltabox):
    fs = run(["findmnt", "-no", "FSTYPE", "-T", str(work)]).stdout.decode().strip()
    cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
                if l.startswith("model name")), platform.processor())
    rev = run(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
              check=False).stdout.decode().strip()
    dirty = run(["git", "-C", str(REPO), "status", "--porcelain"],
                check=False).stdout.decode().strip()
    ver = run([str(deltabox), "--version"], check=False).stdout.decode().strip()
    return {"kernel": platform.release(), "cpu": cpu, "cpus": os.cpu_count(),
            "workdir_fs": fs, "git": rev + ("-dirty" if dirty else ""),
            "deltabox": f"{deltabox} ({ver})"}


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--deltabox", default=REPO / "target/release/deltabox", type=Path)
    ap.add_argument("--raw-sandlock", default=REPO / "target/release/examples/raw_sandlock",
                    type=Path)
    ap.add_argument("--workdir", default=REPO / "target/bench-delta-driven", type=Path,
                    help="parent for disposable projects; use the filesystem you "
                         "want to measure (not tmpfs)")
    ap.add_argument("--warmups", type=int, default=5)
    ap.add_argument("--runs", type=int, default=50, help="samples for the no-op table")
    ap.add_argument("--micro-runs", type=int, default=30,
                    help="samples for the write microbenchmarks")
    ap.add_argument("--only", choices=["noop", "micro"], default=None)
    ap.add_argument("--session-policy", type=Path,
                    help="policy TOML passed to `deltabox session start --policy` "
                         "(default: the build's built-in policy)")
    ap.add_argument("--json", type=Path, help="also write raw samples here")
    a = ap.parse_args()

    for b in (a.deltabox, a.raw_sandlock):
        if not b.exists():
            sys.exit(f"missing {b}; build with:\n  cargo build --release -p deltabox\n"
                     "  cargo build --release -p deltabox-core --example raw_sandlock")

    a.workdir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="run-", dir=a.workdir))
    results = {"env": env_info(root, a.deltabox),"warmups": a.warmups, "workloads": {}}
    print(json.dumps(results["env"]), file=sys.stderr)

    def project(tag):
        p = root / tag
        p.mkdir()
        return p

    def record(workload, runner, xs):
        s = summary(xs)
        results["workloads"].setdefault(workload, {})[runner] = {**s, "samples_ms": xs}
        print(f"{workload:14} {runner:14} p50 {s['p50_ms']:8.1f}  p95 {s['p95_ms']:8.1f} ms"
              f"  (n={s['n']})", file=sys.stderr)

    try:
        if a.only in (None, "noop"):
            for n in (10, 200, 1000):
                for kind in ("cbcow", "sandlock", "shell"):
                    p = project(f"noop{n}-{kind}")
                    for i in range(n):
                        d = p / f"d{i % 10}"
                        d.mkdir(exist_ok=True)
                        (d / f"f{i}.txt").write_bytes(os.urandom(1024))
                    r = (Cbcow(a.deltabox, p, a.session_policy) if kind == "cbcow" else
                         RawSandlock(a.raw_sandlock, p, root / f"store-{p.name}")
                         if kind == "sandlock" else Shell(p))
                    try:
                        xs = measure(lambda i: r.exec(["sh", "-c", ":"]), a.warmups, a.runs)
                    finally:
                        r.close()
                    record(f"noop-{n}", r.name, xs)

        if a.only in (None, "micro"):
            for wl, files, size in (("write-256m", 1, 256 * MiB),
                                    ("write-16x32m", 16, 32 * MiB)):
                for kind in ("cbcow", "sandlock"):
                    p = project(f"{wl}-{kind}")
                    names = [f"big{j:02}.bin" for j in range(files)]
                    for nm in names:
                        make_file(p / nm, size)
                    blocks = size // 4096
                    r = (Cbcow(a.deltabox, p, a.session_policy) if kind == "cbcow" else
                         RawSandlock(a.raw_sandlock, p, root / f"store-{p.name}"))
                    try:
                        # Correctness gate: the write must land in the host file.
                        before = block_digest(p / names[0], 7)
                        r.exec(write_cmd(names[0], 7))
                        r.commit()
                        if block_digest(p / names[0], 7) == before:
                            sys.exit(f"{r.name}: write did not reach {p / names[0]}")

                        def sample(i):
                            for j, nm in enumerate(names):
                                r.exec(write_cmd(nm, (i * 977 + j * 131) % blocks))
                            r.commit()
                        xs = measure(sample, a.warmups, a.micro_runs)
                    finally:
                        r.close()
                    record(wl, r.name, xs)
                    shutil.rmtree(p)  # free the large files before the next runner
    finally:
        shutil.rmtree(root, ignore_errors=True)

    if a.json:
        a.json.write_text(json.dumps(results, indent=2))

    w = results["workloads"]
    f = lambda wl, rn: (f"{w[wl][rn]['p50_ms']:.1f} / {w[wl][rn]['p95_ms']:.1f} ms"
                        if rn in w.get(wl, {}) else "-")
    print(f"\nEnvironment: {json.dumps(results['env'])}\n")
    if "noop-10" in w:
        print("| Runner | 10 files p50 / p95 | 200 files p50 / p95 | 1000 files p50 / p95 |")
        print("|---|---:|---:|---:|")
        for rn in ("cbcow-through", "raw Sandlock", "plain shell"):
            print(f"| {rn} | {f('noop-10', rn)} | {f('noop-200', rn)} | {f('noop-1000', rn)} |")
        print()
    if "write-256m" in w:
        print("| Microbenchmark | cbcow-through p50 / p95 | raw Sandlock p50 / p95 |")
        print("|---|---:|---:|")
        print(f"| One 4 KiB write in a 256 MiB existing file | {f('write-256m', 'cbcow-through')}"
              f" | {f('write-256m', 'raw Sandlock')} |")
        print(f"| 16 commands, one 4 KiB write per 32 MiB file, one final commit"
              f" | {f('write-16x32m', 'cbcow-through')} | {f('write-16x32m', 'raw Sandlock')} |")


if __name__ == "__main__":
    main()

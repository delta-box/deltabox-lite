#!/usr/bin/env python3
"""Commit/abort transactions derived from real replays, then verified.

    exact.py [ids ...]                 instances (default: sources.json)
    exact.py --all [--shard 3/50]      every instance of the run, or one shard of it

No command parsing. For each trajectory, in its SWE-bench task image:

1. observe   run every recorded command, twice, from a fresh image; after
             each command hash every file of /testbed. Paths whose content
             differs between the two runs are non-deterministic and left out.
             S_0 .. S_n are the workspace states (S_k = before step k).
2. derive    per_write: commit after step k when S_k+1 differs from the
             state at the last commit (no empty commits).
             abort: step r is an abort of steps j..r-1 when S_r+1 == S_j, j is
             the last such index, and it does not overlap an earlier abort
             (the agent's step r puts the whole workspace back where a
             rollback to the checkpoint before j would). Cleanup of scratch
             files counts: anything inside /testbed does.
3. verify    replay with the git backend doing the real commits and rollbacks
             (the agent's step r is not run). Every later step must give the
             same workspace and exit code as in observe; if not, the most
             recent abort before the divergence is dropped and 2-3 repeat.

The environment is held to the recording, not to today: `pip install` resolves
against PyPI as of the recording time (pypi_asof.py), and a command that
finished in the recording gets 10x the agent's timeout (a slower host must not
turn it into a timeout), while one that timed out keeps the agent's timeout.

Writes results/exact/<id>.json (no command text) and the timing CSVs
results/<id>.{manual,git}.per_write.csv (manual = the observe run: an abort
costs the agent's own undo command).
"""

import argparse
import csv
import hashlib
import json
import os
import shutil
import stat
import sys
import time
import traceback
import urllib.error
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import annotate  # noqa: E402  (only load_trajectory: reads the JSON, no analysis)
import pypi_asof  # noqa: E402
import swe_env  # noqa: E402
from fetch import CACHE, details_url, download, raw_path, traj_url  # noqa: E402
from replay import FIELDS, Git  # noqa: E402

WS = "/testbed"
# Not workspace content: caches the interpreter and test runners regenerate,
# and the repository's own .git (a backend restores the files, not git's index).
SKIP_DIRS = {".git", "__pycache__", ".pytest_cache", ".mypy_cache", ".tox"}
OUT = HERE / "results" / "exact"
MAX_ROUNDS = 8


# ------------------------------------------------------------------ workspace state

class Tree:
    """Content hashes of every file under root, with a stat cache so a scan
    after each command only rereads files whose metadata changed."""

    def __init__(self, root):
        self.root, self.cache = str(root), {}

    def scan(self):
        out, seen = {}, {}
        stack = [self.root]
        while stack:
            d = stack.pop()
            try:
                it = os.scandir(d)
            except OSError:
                continue
            with it:
                for e in it:
                    if e.is_dir(follow_symlinks=False):
                        if e.name not in SKIP_DIRS and not e.name.endswith(".egg-info"):
                            stack.append(e.path)
                        continue
                    if e.name.endswith(".pyc"):
                        continue
                    rel = e.path[len(self.root) + 1:]
                    try:
                        st = e.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    key = (st.st_mtime_ns, st.st_size, st.st_ino, st.st_mode)
                    c = self.cache.get(rel)
                    if c and c[0] == key:
                        h = c[1]
                    elif stat.S_ISLNK(st.st_mode):
                        h = "L:" + os.readlink(e.path)
                    else:
                        try:
                            with open(e.path, "rb") as f:
                                h = hashlib.file_digest(f, "sha1").hexdigest()
                        except OSError:
                            h = "unreadable"
                        h += "x" if st.st_mode & 0o111 else ""
                    seen[rel] = (key, h)
                    out[rel] = h
        self.cache = seen
        return out


def digest(state, skip):
    h = hashlib.sha256()
    for p in sorted(state):
        if p not in skip:
            h.update(p.encode() + b"\0" + state[p].encode() + b"\n")
    return h.hexdigest()[:16]


def diff(a, b):
    ch = {p: h for p, h in b.items() if a.get(p) != h}
    ch.update({p: None for p in a if p not in b})
    return ch


# ------------------------------------------------------------------ stages

class Image:
    def __init__(self, iid):
        self.repo = swe_env.image_name(iid)
        self.root = CACHE / f"env-{iid}"
        self.blobs = CACHE / "blobs" / iid
        self.pip_index = None

    def fresh(self, timeout):
        meta = swe_env.unpack(self.repo, self.root, self.blobs, log=lambda *_: None)
        return swe_env.Env(self.root, meta, timeout=timeout, pip_index=self.pip_index), meta

    def remove(self):
        shutil.rmtree(self.root, ignore_errors=True)
        shutil.rmtree(self.blobs, ignore_errors=True)


def guard(env):
    if shutil.disk_usage(env.root).free < swe_env.MARGIN:
        raise RuntimeError("less than 1 GiB disk free")


RECORDED_TIMEOUT = -1  # mini-swe-agent's returncode for a command it timed out
SLOW_HOST = 10         # timeout factor for commands that finished in the recording


def step_timeout(step, timeout):
    """A command that finished in the recording must finish here too (this host,
    its proxy and its disks may be slower than the recording's); one that timed
    out in the recording keeps the agent's timeout, so it times out again."""
    return timeout if step["recorded_rc"] in (None, RECORDED_TIMEOUT) else timeout * SLOW_HOST


def same_rc(replayed, recorded):
    return replayed == recorded or (recorded == RECORDED_TIMEOUT and replayed == -9)


def observe(img, steps, timeout):
    env, meta = img.fresh(timeout)
    tree = Tree(env.host(WS))
    state = tree.scan()
    states, rcs, ms = [state], [], []
    for s in steps:
        guard(env)
        env.timeout = step_timeout(s, timeout)
        rc, _, t = env.run(s["cmd"])
        state = tree.scan()
        states.append(state)
        rcs.append(rc)
        ms.append(t)
    return states, rcs, ms, meta


def derive(D, banned):
    """D[k] = digest of the state before step k (len n+1). Returns
    (segments, aborts, skipped)."""
    n = len(D) - 1
    aborts, skipped, prev_r = [], [], -1
    for r in range(n):
        if D[r + 1] == D[r] or r in banned:
            continue
        X = D[r + 1]
        j = max((i for i in range(r) if D[i] == X), default=None)
        if j is None:
            continue
        if j <= prev_r:
            skipped.append({"step": r, "span": [j, r - 1], "reason": "overlaps an earlier abort"})
            continue
        aborts.append((j, r))
        prev_r = r
    segs, start, last, k = [], 0, D[0], 0
    starts = {j: r for j, r in aborts}
    while k < n:
        if k in starts:
            r = starts[k]
            if start < k:
                segs.append({"first": start, "last": k - 1, "outcome": "none"})
            segs.append({"first": k, "last": r - 1, "outcome": "abort", "replaces": [r]})
            start, k = r + 1, r + 1
            continue
        if D[k + 1] != last:
            segs.append({"first": start, "last": k, "outcome": "commit"})
            last, start = D[k + 1], k + 1
        k += 1
    if start < n:
        segs.append({"first": start, "last": n - 1, "outcome": "none"})
    for i, s in enumerate(segs):
        s["id"] = i
    return segs, aborts, skipped


class GitAll(Git):
    """git backend that also versions files the repository's .gitignore
    ignores (they are workspace content too); caches stay out."""
    X = ["__pycache__", "*.pyc", ".pytest_cache", ".mypy_cache", ".tox", "*.egg-info"]

    def setup(self, env, traj=None):
        self.env = env
        spec = " ".join(f"':(exclude,glob)**/{p}'" for p in self.X)
        self.add = f"{self.G} add -A -f -- . {spec}"
        rc, out, _ = env.run(f"rm -rf /root/.txn.git && git init -q --bare /root/.txn.git && "
                             f"{self.add} && {self.G} commit -q -m base")
        if rc:
            raise RuntimeError(out.decode(errors="replace")[-2000:])

    def commit(self, tx):
        return self._t(f"{self.add} && {self.G} commit -q --allow-empty -m tx{tx['id']}")

    def abort(self, tx):
        ex = " ".join(f"-e '{p}'" for p in self.X)
        return self._t(f"{self.G} reset -q --hard && {self.G} clean -fdxq -e .git {ex}")


def verify(img, steps, segs, D, rcs, skip, timeout):
    """Replays with git. Returns (first divergent step or None, rows)."""
    env, _ = img.fresh(timeout)
    tree = Tree(env.host(WS))
    b = GitAll()
    b.setup(env)
    rows = []
    for tx in segs:
        for k in range(tx["first"], tx["last"] + 1):
            guard(env)
            env.timeout = step_timeout(steps[k], timeout)
            rc, ms = b.exec(steps[k]["cmd"])
            d = digest(tree.scan(), skip)
            rows.append({"tx": tx["id"], "outcome": tx["outcome"], "step": k, "op": "exec",
                         "ms": ms, "rc": rc, "tree": d})
            if d != D[k + 1] or (rc != rcs[k] and k not in skip.rc_steps):
                return k, rows
        if tx["outcome"] in ("commit", "abort"):
            ms = getattr(b, tx["outcome"])(tx)
            d = digest(tree.scan(), skip)
            rows.append({"tx": tx["id"], "outcome": tx["outcome"], "op": tx["outcome"],
                         "ms": ms, "tree": d})
            if d != D[tx["last"] + 1 + (1 if tx["outcome"] == "abort" else 0)]:
                return tx["last"], rows
    return None, rows


class Skip(set):
    rc_steps = set()


# ------------------------------------------------------------------ driver

def run_one(run, iid, details):
    t0 = time.time()
    download(traj_url(run, iid), raw_path(run, iid))
    steps, meta = annotate.load_trajectory(run, iid)
    timeout = (meta.get("environment") or {}).get("timeout", 60)
    img = Image(iid)
    # `pip install` sees PyPI as of the recording, not as of today.
    index = pypi_asof.AsOf(meta["recorded_at"]) if meta.get("recorded_at") else None
    try:
        if index:
            img.pip_index = index.__enter__()
        sA, rcA, msA, imeta = observe(img, steps, timeout)
        sB, rcB, _, _ = observe(img, steps, timeout)
        skip = Skip()
        for a, b in zip(sA, sB):
            skip.update(p for p in set(a) | set(b) if a.get(p) != b.get(p))
        skip.rc_steps = {k for k, (x, y) in enumerate(zip(rcA, rcB)) if x != y}
        D = [digest(s, skip) for s in sA]
        banned, dropped, rounds = set(), [], 0
        while True:
            rounds += 1
            segs, aborts, skipped = derive(D, banned)
            fail, rows = verify(img, steps, segs, D, rcA, skip, timeout)
            if fail is None:
                break
            before = [r for j, r in aborts if r < fail]
            if not before or rounds >= MAX_ROUNDS:
                break
            r = before[-1]
            banned.add(r)
            dropped.append({"step": r, "diverged_at": fail})
    finally:
        img.remove()
        if img.pip_index:
            index.__exit__(None, None, None)

    rec =[s["recorded_rc"] for s in steps]
    ab = [s for s in segs if s["outcome"] == "abort"]
    result = {
        "instance_id": iid, "run": run, "trajectory_sha256": hashlib.sha256(
            raw_path(run, iid).read_bytes()).hexdigest(),
        "image": imeta["image"], "image_digest": imeta["manifest_digest"],
        "resolved": details.get("resolved"),
        "pypi_as_of": meta.get("recorded_at"),
        "steps": len(steps), "verified": fail is None, "diverged_at": fail,
        "rounds": rounds,
        "commits": sum(s["outcome"] == "commit" for s in segs), "aborts": len(ab),
        "aborted_steps": sum(s["last"] - s["first"] + 1 for s in ab),
        "rc_match_recording": sum(same_rc(a, r) for a, r in zip(rcA, rec) if r is not None),
        "rc_recorded": sum(r is not None for r in rec),
        "nondeterministic_paths": sorted(skip), "nondeterministic_rc_steps": sorted(skip.rc_steps),
        "dropped_aborts": dropped, "overlapping_reverts": skipped,
        "per_step": [{"i": k, "rc": rcA[k], "changed": sorted(diff(sA[k], sA[k + 1]))}
                     for k in range(len(steps))],
        "transactions": segs, "seconds": round(time.time() - t0),
    }
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / f"{iid}.json").write_text(json.dumps(result, indent=1) + "\n")
    write_csvs(iid, steps, segs, sA, rcA, msA, rows, skip)
    return result


def write_csvs(iid, steps, segs, sA, rcA, msA, git_rows, skip):
    D = [digest(s, skip) for s in sA]
    base = {"instance": iid, "policy": "per_write"}
    man = []
    for tx in segs:
        for k in range(tx["first"], tx["last"] + 1):
            man.append({**base, "backend": "manual", "tx": tx["id"], "outcome": tx["outcome"],
                        "step": k, "op": "exec", "kind": "change" if D[k + 1] != D[k] else "",
                        "ms": round(msA[k], 3), "rc": rcA[k],
                        "recorded_rc": steps[k]["recorded_rc"], "tree": D[k + 1]})
        if tx["outcome"] == "commit":
            man.append({**base, "backend": "manual", "tx": tx["id"], "outcome": "commit",
                        "op": "commit", "ms": 0.0, "tree": D[tx["last"] + 1]})
        elif tx["outcome"] == "abort":
            r = tx["replaces"][0]
            man.append({**base, "backend": "manual", "tx": tx["id"], "outcome": "abort",
                        "op": "abort", "ms": round(msA[r], 3), "tree": D[r + 1]})
    git = []
    for row in git_rows:
        k = row.get("step")
        git.append({**base, "backend": "git", **row, "ms": round(row["ms"], 3),
                    **({"kind": "change" if D[k + 1] != D[k] else "",
                        "recorded_rc": steps[k]["recorded_rc"]} if k is not None else {})})
    for name, rows in (("manual", man), ("git", git)):
        with open(HERE / "results" / f"{iid}.{name}.per_write.csv", "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=FIELDS)
            w.writeheader()
            w.writerows(rows)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ids", nargs="*")
    ap.add_argument("--run", default=annotate.load_sources()["run"])
    ap.add_argument("--all", action="store_true", help="every instance of the run")
    ap.add_argument("--shard", help="i/N: the i-th (0-based) of N interleaved shards")
    ap.add_argument("--redo", action="store_true", help="redo instances already done")
    a = ap.parse_args()
    det = CACHE / "raw" / a.run / "per_instance_details.json"
    download(details_url(a.run), det)
    details = json.loads(det.read_text())
    ids = (a.ids or (sorted(details) if a.all else
                     [i["instance_id"] for i in annotate.load_sources()["instances"]]))
    if a.shard:
        i, n = map(int, a.shard.split("/"))
        ids = ids[i::n]
    (HERE / "results").mkdir(exist_ok=True)
    failed = 0
    for iid in ids:
        if (OUT / f"{iid}.json").exists() and not a.redo:
            print(f"skip {iid} (done)")
            continue
        print(f"== {iid} {time.strftime('%T')}", flush=True)
        try:
            r = run_one(a.run, iid, details.get(iid, {}))
            print(f"   {r['steps']} steps, {r['commits']} commits, {r['aborts']} aborts, "
                  f"verified={r['verified']}, dropped={len(r['dropped_aborts'])}, "
                  f"rc={r['rc_match_recording']}/{r['rc_recorded']}, "
                  f"nondet={len(r['nondeterministic_paths'])}, {r['seconds']} s", flush=True)
        except Exception as e:  # noqa: BLE001
            failed += 1
            print(f"FAILED {iid}: {type(e).__name__}: {e}", flush=True)
            traceback.print_exc(file=sys.stdout)
            if isinstance(e, urllib.error.HTTPError):
                print(f"   url: {e.url}", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

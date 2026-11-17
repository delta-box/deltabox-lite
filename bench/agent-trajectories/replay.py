#!/usr/bin/env python3
"""Replay a transactional trajectory with a pluggable commit/abort backend.

    replay.py --dry-run [--policy per_eval]            print every schedule, check it
    replay.py <instance> --backend git [--policy ...]   replay in the task image

A backend implements five calls; the driver walks the transactions of one
commit policy and calls them:

    setup(env)                    once, before the first command
    begin(tx)                     a transaction starts (take a checkpoint)
    exec(cmd) -> (rc, ms)         run one agent command
    commit(tx) -> ms              keep the transaction's changes
    abort(tx)  -> ms              roll back to the transaction's checkpoint;
                                  the agent's own revert command(s) in
                                  tx["replaces"] are then NOT executed
    teardown()

Backends here:
  manual   the trajectory exactly as recorded: no transactions; an abort runs
           the agent's own revert command instead (cp X.backup X, git checkout X)
  git      a private git repository over /testbed (separate GIT_DIR, so the
           agent's own `git diff`/`git stash` are unaffected): commit = add -A +
           commit, abort = reset --hard + clean -fd

To add one (deltabox, Sandlock, reflink, ...), subclass Backend; see README
"Adding a backend" for the operation mapping.
"""

import argparse
import csv
import json
import shutil
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from fetch import CACHE  # noqa: E402

FIELDS = ["instance", "backend", "policy", "tx", "outcome", "step", "op", "kind", "ms", "rc",
          "recorded_rc", "tree"]

SKIP_DIRS = {".git", "__pycache__", ".pytest_cache", ".mypy_cache", ".tox"}


def tree_hash(root, ignore=()):
    """SHA-256 over every path, type and content under root (caches and the
    `ignore` paths excluded)."""
    import hashlib
    import os
    h = hashlib.sha256()
    for d, dirs, files in os.walk(root):
        dirs[:] = sorted(x for x in dirs if x not in SKIP_DIRS and not x.endswith(".egg-info"))
        for name in sorted(files):
            if name.endswith(".pyc"):
                continue
            p = os.path.join(d, name)
            if os.path.relpath(p, root) in ignore:
                continue
            h.update(os.path.relpath(p, root).encode() + b"\0")
            if os.path.islink(p):
                h.update(b"L" + os.readlink(p).encode())
            else:
                with open(p, "rb") as f:
                    h.update(hashlib.sha256(f.read()).digest())
    return h.hexdigest()[:16]


def load(iid):
    p = CACHE / "replay" / f"{iid}.json"
    if not p.exists():
        raise SystemExit(f"{p} missing: run fetch.py and annotate.py first")
    return json.loads(p.read_text())


def schedule(traj, policy):
    """Yields ("begin"|"exec"|"commit"|"abort", tx, step_index|None)."""
    for tx in traj["transactions"][policy]:
        yield "begin", tx, None
        for k in range(tx["first"], tx["last"] + 1):
            yield "exec", tx, k
        yield tx["outcome"], tx, None


def check(traj, policy):
    """Every step is executed once or replaced by exactly one abort."""
    n = len(traj["steps"])
    seen = [0] * n
    for op, tx, k in schedule(traj, policy):
        if op == "exec":
            seen[k] += 1
        if op == "abort":
            for r in tx["replaces"]:
                seen[r] += 1
    return [i for i, c in enumerate(seen) if c != 1]


class Backend:
    name = "base"

    def setup(self, env): self.env = env
    def begin(self, tx): return 0.0
    def exec(self, cmd):
        rc, self.last_out, ms = self.env.run(cmd)
        return rc, ms
    def commit(self, tx): return 0.0
    def abort(self, tx): raise NotImplementedError
    def teardown(self): pass


class Manual(Backend):
    """No transactions: aborts are the agent's own revert commands."""
    name = "manual"

    def setup(self, env, traj=None):
        super().setup(env)
        self.traj = traj

    def abort(self, tx):
        ms = 0.0
        for r in tx["replaces"]:
            _, m = self.exec(self.traj["steps"][r]["cmd"])
            ms += m
        return ms


class Git(Backend):
    name = "git"
    G = "git --git-dir=/root/.txn.git --work-tree=/testbed -c user.name=txn -c user.email=txn@localhost"

    def setup(self, env, traj=None):
        super().setup(env)
        excl = "\\n".join(["__pycache__/", "*.pyc", ".pytest_cache/", ".mypy_cache/", ".tox/",
                           "*.egg-info/"])
        rc, out, _ = env.run(f"rm -rf /root/.txn.git && git init -q --bare /root/.txn.git && "
                             f"printf '{excl}\\n' > /root/.txn.git/info/exclude && "
                             f"{self.G} add -A && {self.G} commit -q -m base")
        if rc:
            raise RuntimeError(out.decode(errors="replace"))

    def _t(self, cmd):
        rc, out, ms = self.env.run(cmd)
        if rc:
            raise RuntimeError(out.decode(errors="replace"))
        return ms

    def commit(self, tx):
        return self._t(f"{self.G} add -A && {self.G} commit -q --allow-empty -m tx{tx['id']}")

    def abort(self, tx):
        return self._t(f"{self.G} reset -q --hard && {self.G} clean -fdq")


BACKENDS = {"manual": Manual, "git": Git}


def dry_run(policy):
    src = json.loads((HERE / "sources.json").read_text())
    bad = 0
    for inst in src["instances"]:
        iid = inst["instance_id"]
        t = load(iid)
        miss = check(t, policy)
        txs = t["transactions"][policy]
        line = " ".join(("A" if x["outcome"] == "abort" else "c" if x["writes"] else ".")
                        for x in txs)
        print(f"{iid:34} {'OK ' if not miss else 'BAD'} {len(txs):3} tx  {line}")
        bad += bool(miss)
    print("legend: c = commit with writes, . = commit without writes, A = abort")
    return bad


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("instance", nargs="?")
    ap.add_argument("--backend", choices=sorted(BACKENDS), default="manual")
    ap.add_argument("--policy", choices=["minimal", "per_eval", "per_write"], default="per_eval")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()
    if a.dry_run:
        sys.exit(1 if dry_run(a.policy) else 0)
    if not a.instance:
        ap.error("instance required")

    import swe_env
    traj = load(a.instance)
    root = CACHE / f"env-{a.instance}"
    print(f"unpacking a fresh {swe_env.image_name(a.instance)} into {root}")
    meta = swe_env.unpack(swe_env.image_name(a.instance), root,
                          CACHE / "blobs" / a.instance)
    env = swe_env.Env(root, meta, timeout=traj["source"]["environment"].get("timeout", 60))
    b = BACKENDS[a.backend]()
    b.setup(env, traj)
    out = a.out or HERE / "results" / f"{a.instance}.{a.backend}.{a.policy}.csv"
    out.parent.mkdir(exist_ok=True)
    # Command output stays in the local cache (it can quote repository code).
    logp = CACHE / "outputs" / f"{a.instance}.{a.backend}.{a.policy}.jsonl"
    logp.parent.mkdir(exist_ok=True)
    log = open(logp, "w")
    # Unused files the agent's revert command created (e.g. X.orig): the
    # rollback drops them by design, so they are not compared.
    scratch = {p for e in traj["revert_events"] if e["accepted"]
               for p in e.get("dropped_unused_scratch", [])}
    # Dead span files the agent's revert leaves behind: different until the
    # step that overwrites or deletes them.
    until = [(p, e["step"], k) for e in traj["revert_events"] if e["accepted"]
             for p, k in e.get("differs_until_overwritten", {}).items()]
    ignore_at = lambda last: scratch | {p for p, r, k in until if r - 1 <= last < k}
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        w.writeheader()
        for op, tx, k in schedule(traj, a.policy):
            row = {"instance": a.instance, "backend": a.backend, "policy": a.policy,
                   "tx": tx["id"], "outcome": tx["outcome"], "step": k, "op": op}
            if op == "exec":
                if shutil.disk_usage(root).free < swe_env.MARGIN:
                    raise SystemExit(f"stopping at step {k}: less than 1 GiB disk free")
                st = traj["steps"][k]
                rc, ms = b.exec(st["cmd"])
                row.update(kind=st["kind"], ms=round(ms, 3), rc=rc, recorded_rc=st["recorded_rc"])
                log.write(json.dumps({"step": k, "rc": rc, "out": b.last_out.decode(
                    errors="replace")[-4000:]}) + "\n")
            else:
                row["ms"] = round(getattr(b, op)(tx), 3)
                if op != "begin":
                    row["tree"] = tree_hash(env.host("/testbed"), ignore_at(tx["last"]))
            w.writerow(row)
    b.teardown()
    print(f"wrote {out}")


if __name__ == "__main__":
    main()

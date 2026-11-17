#!/usr/bin/env python3
"""Turn real agent trajectories into transactional trajectories.

Input: the raw mini-SWE-agent trajectories listed in sources.json (see
fetch.py). Every agent action is one bash command run in /testbed.

Output, per instance:
  annotations/<id>.json   committed: provenance, per-step metadata (no
                          command text, see README), transactions with
                          commit/abort outcomes, revert events and the reason
                          each was or was not turned into an abort.
  target/agent-traj-cache/replay/<id>.json
                          local only: the same plus the command text, ready
                          for replay.py.

Method (static; see README "How commit and abort are derived"):

1. Every command is parsed into simple commands (heredocs, &&, ;, |,
   redirections) and turned into file operations on paths under /testbed:
   full write (with a content hash when the content is known, e.g. a heredoc),
   partial write (sed -i, >>, a Python script that opens the file for
   writing), copy, move, delete, git checkout/restore (back to the base
   version), and uses (a path named in a command).
2. A version model tracks each path: base version (tracked in git at the
   task's base commit), absent, or a content version.
3. A *revert event* is a command after which a file is back at a version it
   had earlier (cp X.backup X, git checkout X, rewriting the same content).
4. The span from the command that first moved the file away from that version
   to the command before the revert is an abort candidate. It is accepted
   when rolling the whole workspace back to a checkpoint taken just before the
   span has the same effect as the agent's revert:
     - no write in the span has an unknown target;
     - every other path changed in the span is back to its pre-span version
       after the revert, or is not used again before it is overwritten or
       deleted (an unused scratch file is allowed and noted);
     - the revert command does nothing else.
   An accepted span becomes an ABORT transaction (checkpoint before it, the
   revert command replaced by abort). Everything else is COMMIT.
"""

import argparse
import ast
import fnmatch
import hashlib
import json
import os
import posixpath
import re
import shlex
import subprocess
import sys
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from fetch import CACHE, load_sources, raw_path, traj_url  # noqa: E402

WS = "/testbed"
EXCLUDE_PARTS = {".git", "__pycache__", ".pytest_cache", ".mypy_cache", ".tox"}
EVAL_RE = re.compile(r"(^|/)(python[\d.]*|pytest|py\.test|tox|nosetests|runtests\.py)$")
DATASET_ROWS = ("https://datasets-server.huggingface.co/rows?dataset=princeton-nlp%2F"
                "SWE-bench_Verified&config=default&split=test&offset={o}&length=100")


# ------------------------------------------------------------------ inputs


def load_trajectory(run, iid):
    d = json.loads(raw_path(run, iid).read_text())
    rc_by_call, steps, times = {}, [], []
    for m in d["messages"]:
        ex = m.get("extra")
        if isinstance(ex, str):
            try:
                ex = ast.literal_eval(ex)
            except Exception:
                ex = {}
        ex = ex or {}
        call = m.get("tool_call_id") or m.get("call_id")
        text = m.get("content") if isinstance(m.get("content"), str) else m.get("output", "")
        if call and isinstance(text, str):
            mm = re.search(r"<returncode>(-?\d+)</returncode>", text)
            if mm:
                rc_by_call[call] = int(mm.group(1))
        for a in ex.get("actions", []):
            steps.append({"cmd": a["command"], "call": a.get("tool_call_id")})
        if ex.get("timestamp"):
            times.append(float(ex["timestamp"]))
    for s in steps:
        s["recorded_rc"] = rc_by_call.get(s.pop("call"))
    info = d["info"]
    cfg = info.get("config")
    cfg = cfg if isinstance(cfg, dict) else ast.literal_eval(cfg)
    return steps, {"mini_version": info.get("mini_version"),
                   "exit_status": info.get("exit_status"),
                   "environment": cfg.get("environment"),
                   "agent_wall_time_s": round(max(times) - min(times), 1) if times else None,
                   "recorded_at": max(times) if times else None}


def dataset_rows(iids):
    """SWE-bench Verified rows (repo, base_commit) for the given instances."""
    cache = CACHE / "swebench_verified_rows.json"
    rows = json.loads(cache.read_text()) if cache.exists() else {}
    if not all(i in rows for i in iids):
        for o in range(0, 500, 100):
            with urllib.request.urlopen(DATASET_ROWS.format(o=o), timeout=60) as r:
                for x in json.load(r)["rows"]:
                    x = x["row"]
                    rows[x["instance_id"]] = {k: x[k] for k in ("repo", "base_commit",
                                                                 "version")}
        cache.write_text(json.dumps(rows))
    return {i: rows[i] for i in iids}


def tracked_files(repo, commit):
    cache = CACHE / "trees" / f"{repo.replace('/', '__')}@{commit}.json"
    if not cache.exists():
        out = subprocess.run(["gh", "api", f"repos/{repo}/git/trees/{commit}?recursive=1"],
                             capture_output=True, check=True).stdout
        t = json.loads(out)
        if t.get("truncated"):
            raise SystemExit(f"tree of {repo}@{commit} is truncated")
        cache.parent.mkdir(parents=True, exist_ok=True)
        cache.write_text(json.dumps(sorted(e["path"] for e in t["tree"] if e["type"] == "blob")))
    return set(json.loads(cache.read_text()))


# ------------------------------------------------------------------ parsing

HEREDOC = re.compile(r"<<(-?)\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\2")


def split_heredocs(text):
    """Returns (text without heredoc bodies, [bodies in order])."""
    lines, out, bodies, i = text.split("\n"), [], [], 0
    while i < len(lines):
        line = lines[i]
        out.append(line)
        i += 1
        for m in HEREDOC.finditer(line):
            term, strip = m.group(3), m.group(1) == "-"
            body = []
            while i < len(lines) and (lines[i].strip() if strip else lines[i]) != term:
                body.append(lines[i])
                i += 1
            i += 1
            bodies.append("\n".join(body) + "\n")
    return "\n".join(out), bodies


SEPS = {"&&", "||", ";", "|", "\n", "&", "(", ")", ";;", "|&"}


def simple_commands(text):
    """Yields dicts {argv, redirs: [(op, target)], heredoc} per simple command."""
    text, bodies = split_heredocs(text)
    lex = shlex.shlex(text, posix=True, punctuation_chars="();<>|&\n")
    lex.whitespace = " \t\r"
    lex.whitespace_split = True
    lex.commenters = ""
    try:
        toks = list(lex)
    except ValueError:
        yield {"argv": None, "redirs": [], "heredoc": None, "raw": text}
        return
    cur, redirs, hd = [], [], None
    body_iter = iter(bodies)
    i = 0
    while i <= len(toks):
        t = toks[i] if i < len(toks) else "\n"
        if t in SEPS:
            if cur or redirs:
                yield {"argv": cur, "redirs": redirs, "heredoc": hd}
            cur, redirs, hd = [], [], None
        elif t in (">", ">>", ">|", "&>", "<", "<<", "<<<", ">&", "<&", "<<-"):
            nxt = toks[i + 1] if i + 1 < len(toks) else ""
            # "2>&1", "2>/dev/null": the fd number was pushed as an argument.
            if cur and cur[-1].isdigit() and t in (">", ">>", ">&"):
                cur.pop()
            if t in ("<<", "<<-"):
                hd = next(body_iter, "")
            elif t not in (">&", "<&"):
                redirs.append((t, nxt))
            i += 1
        else:
            cur.append(t)
        i += 1


def sha(s):
    return hashlib.sha256(s.encode() if isinstance(s, str) else s).hexdigest()[:16]


OPEN_W = re.compile(r"""open\(\s*(?:r?f?)(['"])(.+?)\1\s*,\s*(?:mode\s*=\s*)?(['"])([wax])""")
WRITE_TEXT = re.compile(r"""Path\(\s*(['"])(.+?)\1\s*\)\.write_(?:text|bytes)\(""")
OPEN_VAR_W = re.compile(r"""open\(\s*[A-Za-z_][\w.\[\]'"]*\s*,\s*(?:mode\s*=\s*)?['"][wax]""")


class Parser:
    """Turns one agent command into file operations under /testbed."""

    def __init__(self, tracked):
        self.tracked = tracked
        self.scripts = {}          # absolute path -> last known content

    def rel(self, path, cwd):
        if not path or path.startswith("-") or "$" in path:
            return None
        p = posixpath.normpath(posixpath.join(cwd, path))
        if p == WS or not p.startswith(WS + "/"):
            return None
        r = p[len(WS) + 1:]
        if set(r.split("/")) & EXCLUDE_PARTS or r.endswith(".pyc") or ".egg-info" in r:
            return None
        return r

    def code_writes(self, code, cwd):
        ops = []
        for m in list(OPEN_W.finditer(code)) + list(WRITE_TEXT.finditer(code)):
            r = self.rel(m.group(2), cwd)
            if r:
                ops.append(("partial", r))
        if OPEN_VAR_W.search(code) and not ops:
            ops.append(("unknown", None))
        return ops

    def parse(self, cmd, cwd=WS, depth=0):
        """Returns (ops, kind_flags). ops: list of tuples
        (full, path, content_hash|None) (partial, path) (delete, path)
        (copy, src, dst) (move, src, dst) (restore, path) (restore_all,)
        (stash,) (unstash,) (use, path) (unknown, None)"""
        ops, flags = [], set()
        for sc in simple_commands(cmd):
            argv = sc["argv"]
            if argv is None:
                ops.append(("unknown", None))
                continue
            # Leading VAR=value assignments.
            while argv and re.match(r"^[A-Za-z_]\w*=", argv[0]):
                argv = argv[1:]
            prog = posixpath.basename(argv[0]) if argv else ""
            args = argv[1:]
            if prog == "cd":
                cwd = posixpath.normpath(posixpath.join(cwd, args[0])) if args else "/root"
                continue
            for op, tgt in sc["redirs"]:
                if op in ("<", "<<<"):
                    r = self.rel(tgt, cwd)
                    if r:
                        ops.append(("use", r))
                    continue
                if tgt.startswith("/dev/"):
                    continue
                r = self.rel(tgt, cwd)
                absp = posixpath.normpath(posixpath.join(cwd, tgt))
                if prog == "cat" and not args and sc["heredoc"] is not None:
                    content = sc["heredoc"]
                    self.scripts[absp] = content
                    if r:
                        ops.append(("full", r, sha(content)) if op != ">>"
                                   else ("partial", r))
                    continue
                if r:
                    ops.append(("partial", r) if op == ">>" else ("full", r, None))
                self.scripts.pop(absp, None)
            if not prog:
                continue
            paths = [self.rel(x, cwd) for x in args]
            for p in paths:
                if p:
                    ops.append(("use", p))
            plain = [x for x in args if not x.startswith("-")]
            if prog == "sed" and any(x.startswith("-i") or x == "--in-place" for x in args):
                files, script_seen, skip = [], False, False
                for x in args:
                    if skip:
                        skip = False
                        continue
                    if x in ("-e", "-f"):
                        script_seen = skip = True
                        continue
                    if x.startswith("-"):
                        continue
                    if not script_seen:
                        script_seen = True
                        continue
                    files.append(x)
                for f in files:
                    r = self.rel(f, cwd)
                    if r:
                        ops.append(("partial", r))
            elif prog in ("cp", "mv") and len(plain) >= 2:
                src, dst = self.rel(plain[0], cwd), self.rel(plain[-1], cwd)
                if len(plain) > 2 or any(x in ("-r", "-R", "-a") for x in args):
                    ops.append(("unknown", None))
                elif dst:
                    ops.append(("copy" if prog == "cp" else "move", src, dst))
                elif src and prog == "mv":
                    ops.append(("delete", src))
            elif prog == "rm":
                for x in plain:
                    if any(c in x for c in "*?["):
                        ops.append(("delete_glob", posixpath.normpath(posixpath.join(cwd, x))))
                    else:
                        r = self.rel(x, cwd)
                        if r:
                            ops.append(("delete", r))
            elif prog == "tee":
                for x in plain:
                    r = self.rel(x, cwd)
                    if r:
                        ops.append(("partial" if "-a" in args else "full", r, None)
                                   if "-a" not in args else ("partial", r))
            elif prog == "touch":
                for x in plain:
                    r = self.rel(x, cwd)
                    if r:
                        ops.append(("touch", r))
            elif prog == "git" and args:
                sub = args[0]
                rest = [x for x in args[1:] if x != "--"]
                if sub in ("checkout", "restore") and rest and not any(
                        x in ("-b", "-B", "--staged") for x in rest):
                    targets = [x for x in rest if not x.startswith("-") and x != "HEAD"]
                    if targets in (["."], [WS], ["/testbed/"]):
                        ops.append(("restore_all",))
                    else:
                        for x in targets:
                            r = self.rel(x, cwd)
                            if r:
                                ops.append(("restore", r))
                elif sub == "stash" and (not rest or rest[0] in ("push", "save")):
                    ops.append(("stash",))
                elif sub == "stash" and rest and rest[0] in ("pop", "apply"):
                    ops.append(("unstash",))
                elif sub in ("apply", "am", "reset", "clean", "rm", "mv"):
                    ops.append(("unknown", None))
            elif prog == "patch":
                ops.append(("unknown", None))
            elif EVAL_RE.search(argv[0]) or prog in ("pip", "pip3"):
                if prog in ("pip", "pip3"):
                    flags.add("env")
                    continue
                flags.add("eval")
                if "-c" in args:
                    ops += self.code_writes(args[args.index("-c") + 1], cwd)
                elif "-" in args and sc["heredoc"]:
                    ops += self.code_writes(sc["heredoc"], cwd)
                elif "-m" not in args:
                    script = next((x for x in args if not x.startswith("-")), None)
                    if script:
                        absp = posixpath.normpath(posixpath.join(cwd, script))
                        body = self.scripts.get(absp)
                        if body is not None:
                            ops += self.code_writes(body, cwd)
                        elif self.rel(script, cwd) and self.rel(script, cwd) not in self.tracked:
                            ops.append(("unknown", None))
            elif prog in ("bash", "sh") and plain and depth < 2:
                absp = posixpath.normpath(posixpath.join(cwd, plain[0]))
                body = self.scripts.get(absp)
                flags.add("eval")
                if body is None:
                    ops.append(("unknown", None))
                else:
                    o, f = self.parse(body, cwd, depth + 1)
                    ops += o
            elif prog in ("find", "xargs") and any(x in ("-delete", "-exec", "-execdir", "sed",
                                                         "rm", "mv", "cp") for x in args):
                if any(x in ("-delete", "rm", "mv", "cp") for x in args) or \
                        ("sed" in args and any(x.startswith("-i") for x in args)):
                    ops.append(("unknown", None))
        return ops, flags


# ------------------------------------------------------------------ versions


class State:
    def __init__(self, tracked):
        self.tracked = tracked
        self.v = {}                 # path -> version string, or None = absent
        self.n = 0
        self.stash = None

    def get(self, p):
        return self.v.get(p, "base" if p in self.tracked else None)

    def fresh(self, tag):
        self.n += 1
        return f"{tag}#{self.n}"

    def apply(self, ops):
        """Applies ops. Returns (changed {path: (old, new)}, uses set, unknown bool)."""
        changed, uses, unknown = {}, set(), False

        def setv(p, nv):
            old = self.get(p)
            if p not in changed:
                changed[p] = (old, nv)
            else:
                changed[p] = (changed[p][0], nv)
            self.v[p] = nv

        for op in ops:
            k = op[0]
            if k == "full":
                setv(op[1], f"h:{op[2]}" if op[2] else self.fresh("w"))
            elif k == "partial":
                setv(op[1], self.fresh("p"))
            elif k == "touch":
                if self.get(op[1]) is None:
                    setv(op[1], "h:empty")
            elif k == "delete":
                if self.get(op[1]) is not None:
                    setv(op[1], None)
            elif k == "delete_glob":
                pat = op[1]
                for p in [p for p, v in self.v.items() if v is not None]:
                    if fnmatch.fnmatch(posixpath.join(WS, p), pat):
                        setv(p, None)
            elif k == "copy":
                src = self.get(op[1]) if op[1] else self.fresh("ext")
                setv(op[2], src)
                if op[1]:
                    uses.add(op[1])
            elif k == "move":
                src = self.get(op[1]) if op[1] else self.fresh("ext")
                setv(op[2], src)
                if op[1]:
                    setv(op[1], None)
            elif k == "restore":
                if op[1] in self.tracked:
                    setv(op[1], "base")
            elif k in ("restore_all", "stash"):
                saved = {}
                for p, v in list(self.v.items()):
                    if p in self.tracked and v != "base":
                        saved[p] = v
                        setv(p, "base")
                if k == "stash":
                    self.stash = saved
            elif k == "unstash":
                for p, v in (self.stash or {}).items():
                    setv(p, v)
                self.stash = None
            elif k == "use":
                uses.add(op[1])
            elif k == "unknown":
                unknown = True
        changed = {p: ov for p, ov in changed.items() if ov[0] != ov[1]}
        return changed, uses, unknown


# ------------------------------------------------------------------ analysis


def analyse(steps, tracked):
    parser, st = Parser(tracked), State(tracked)
    hist = {}                       # path -> [(step, version_after)]
    per = []
    for i, s in enumerate(steps):
        ops, flags = parser.parse(s["cmd"])
        before = {}
        changed, uses, unknown = st.apply(ops)
        for p, (old, new) in changed.items():
            hist.setdefault(p, [(-1, old)])
            hist[p].append((i, new))
        kind = ("revert" if any(o[0] in ("restore", "restore_all", "stash") for o in ops)
                else "write" if changed else "eval" if "eval" in flags
                else "unknown-write" if unknown else "env" if "env" in flags else "read")
        per.append({"changed": changed, "uses": uses, "unknown": unknown, "kind": kind,
                    "eval": "eval" in flags})

    def version_at(p, step):
        """Version of p after `step` (step=-1: initial)."""
        v = hist[p][0][1] if p in hist else ("base" if p in tracked else None)
        for j, nv in hist.get(p, []):
            if j <= step:
                v = nv
        return v

    events, accepted, last_r = [], [], -1
    for r, info in enumerate(per):
        # Files this command puts back to a version they had before.
        starts = {}
        for F, (old, new) in info["changed"].items():
            if new is None:
                continue                  # deletions are scratch cleanup, not reverts
            earlier = [(j, v) for j, v in hist[F] if j < r and v == new]
            if earlier:
                j0 = earlier[-1][0]
                starts[F] = next(j for j, v in hist[F] if j > j0)
        if not starts:
            continue
        s = min(starts.values())
        ev = {"step": r, "files_reverted": sorted(starts), "span": [s, r - 1],
              "accepted": False}
        events.append(ev)
        if True:
            # A temporary revert (git stash ... pop, cp X X.new && git checkout
            # X ... cp X.new X) brings the span's version back later: that is
            # an experiment on the old code, not an abort of the span.
            back = next(((F, k) for F in starts for k in range(r + 1, len(per))
                         if F in per[k]["changed"]
                         and per[k]["changed"][F][1] == version_at(F, r - 1)), None)
            if back:
                ev["reason"] = (f"temporary revert: {back[0]} gets the span's version back "
                                f"at step {back[1]}")
                continue
            if s <= last_r:
                ev["reason"] = "overlaps an earlier abort"
                continue
            span = range(s, r)
            touched = sorted({p for k in span for p in per[k]["changed"]})
            if any(per[k]["unknown"] for k in span):
                ev["reason"] = "a command in the span writes an unknown target"
                continue
            def unused_new(p, after):
                return (p not in tracked and version_at(p, s - 1) is None and not any(
                    p in per[k]["uses"] or p in per[k]["changed"]
                    for k in range(after + 1, len(per))))
            r_extra = [p for p, (o, n) in per[r]["changed"].items()
                       if n != version_at(p, s - 1) and not unused_new(p, r)]
            r_dropped = [p for p, (o, n) in per[r]["changed"].items()
                         if n != version_at(p, s - 1) and p not in r_extra]
            if r_extra or per[r]["unknown"]:
                ev["reason"] = (f"the revert command also changes {r_extra}" if r_extra
                                else "the revert command has unknown writes")
                continue
            bad, dropped, dead_until = None, list(r_dropped), {}
            for G in touched:
                pre, post = version_at(G, s - 1), version_at(G, r)
                if pre == post:
                    continue
                # G keeps its span version after the revert: it must be dead.
                fate = "survives to the end"
                for k in range(r + 1, len(per)):
                    if G in per[k]["uses"] and G not in per[k]["changed"]:
                        fate = f"used at step {k}"
                        break
                    if G in per[k]["changed"]:
                        c_old, c_new = per[k]["changed"][G]
                        fate = ("overwritten" if c_new is None or c_new.startswith("h:")
                                or c_new.startswith("w#") else f"edited in place at step {k}")
                        if G in per[k]["uses"] and fate == "overwritten":
                            fate = f"used at step {k}"
                        break
                if fate == "overwritten":
                    # Rollback and the agent's revert leave G different
                    # until step k replaces it.
                    dead_until[G] = k
                    continue
                if fate == "survives to the end" and G not in tracked and pre is None:
                    dropped.append(G)
                    continue
                bad = f"{G} (changed in the span) {fate}"
                break
            if bad:
                ev["reason"] = bad
                continue
            ev["accepted"] = True
            ev["reason"] = "rollback to a checkpoint before the span reproduces the revert"
            if dropped:
                ev["dropped_unused_scratch"] = dropped
            if dead_until:
                ev["differs_until_overwritten"] = dead_until
            ev["files"] = touched
            accepted.append(ev)
            last_r = r
    return per, events, accepted


def transactions(n, accepted, per, policy):
    """policy "minimal": commit only where an abort needs a checkpoint.
    policy "per_eval": also commit after every evaluation command (a test or
    script run) outside an abort span.
    policy "per_write": like per_eval, but only when a file changed since the
    last commit (no empty commits, except the checkpoint an abort needs)."""
    out, pos = [], 0

    def commits(a, b):
        if a > b:
            return
        cuts, last = [], a
        if policy in ("per_eval", "per_write"):
            for k in range(a, b):
                if not per[k]["eval"]:
                    continue
                if policy == "per_write" and not any(per[j]["changed"] for j in range(last, k + 1)):
                    continue
                cuts.append(k)
                last = k + 1
        start = a
        for k in cuts:
            out.append({"first": start, "last": k, "outcome": "commit"})
            start = k + 1
        if start <= b:
            out.append({"first": start, "last": b, "outcome": "commit"})

    for ev in accepted:
        s, e = ev["span"]
        commits(pos, s - 1)
        out.append({"first": s, "last": e, "outcome": "abort", "checkpoint_before": s,
                    "replaces": [ev["step"]], "files": ev["files"]})
        pos = ev["step"] + 1
    commits(pos, n - 1)
    for i, t in enumerate(out):
        t["id"] = i
        rng = range(t["first"], t["last"] + 1)
        t["writes"] = sum(len(per[k]["changed"]) for k in rng)
        t["evals"] = sum(per[k]["eval"] for k in rng)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", help="comma-separated instance ids")
    a = ap.parse_args()
    src = load_sources()
    run = src["run"]
    insts = [i for i in src["instances"]
             if not a.only or i["instance_id"] in a.only.split(",")]
    rows = dataset_rows([i["instance_id"] for i in insts])
    (HERE / "annotations").mkdir(exist_ok=True)
    (CACHE / "replay").mkdir(parents=True, exist_ok=True)
    summary = []
    for inst in insts:
        iid = inst["instance_id"]
        steps, meta = load_trajectory(run, iid)
        row = rows[iid]
        tracked = tracked_files(row["repo"], row["base_commit"])
        per, events, accepted = analyse(steps, tracked)
        policies = {p: transactions(len(steps), accepted, per, p)
                    for p in ("minimal", "per_eval", "per_write")}

        def summ(txs):
            ab = sum(t["outcome"] == "abort" for t in txs)
            wr = [t for t in txs if t["writes"]]
            return {"transactions": len(txs), "aborts": ab, "commits": len(txs) - ab,
                    "writing_transactions": len(wr),
                    "abort_rate_of_writing_transactions": round(ab / len(wr), 3) if wr else 0.0}
        ann = {
            "instance_id": iid,
            "source": {
                "benchmark": src["benchmark"], "track": src["leaderboard_track"],
                "agent": src["agent"], "agent_version_in_trajectory": meta["mini_version"],
                "model": src["model"], "run": run, "trajectory_url": traj_url(run, iid),
                "trajectory_sha256": inst["sha256"], "leaderboard_resolved": inst["resolved"],
                "exit_status": meta["exit_status"], "repo": row["repo"],
                "base_commit": row["base_commit"], "environment": meta["environment"],
                "agent_wall_time_s": meta["agent_wall_time_s"],
            },
            "workspace": WS,
            "excluded_from_workspace": sorted(EXCLUDE_PARTS) + ["*.pyc", "*.egg-info"],
            "steps": [{"i": i, "cmd_sha256": sha(s["cmd"]), "kind": p["kind"],
                       "recorded_rc": s["recorded_rc"], "writes": sorted(p["changed"])}
                      for i, (s, p) in enumerate(zip(steps, per))],
            "transactions": policies,
            "revert_events": events,
            "session_outcome": "commit" if inst["resolved"] else "abort",
            "summary": {"steps": len(steps), "revert_events": len(events),
                        "accepted_aborts": len(accepted),
                        **{p: summ(t) for p, t in policies.items()}},
            "validation": {"status": "static analysis only",
                           "todo": "replay in the task image and compare workspace hashes "
                                   "(replay.py --validate)"},
        }
        (HERE / "annotations" / f"{iid}.json").write_text(json.dumps(ann, indent=1) + "\n")
        full = dict(ann, steps=[dict(st, cmd=s["cmd"]) for st, s in zip(ann["steps"], steps)])
        (CACHE / "replay" / f"{iid}.json").write_text(json.dumps(full, indent=1))
        summary.append((iid, ann["summary"], inst["resolved"]))
        sm = ann["summary"]
        print(f"{iid:34} steps={len(steps):3} aborts={len(accepted)} "
              f"minimal: {sm['minimal']['commits']}c/{sm['minimal']['aborts']}a  "
              f"per_eval: {sm['per_eval']['commits']}c/{sm['per_eval']['aborts']}a "
              f"({sm['per_eval']['writing_transactions']} writing) "
              f"resolved={inst['resolved']}")
        for ev in events:
            print(f"    step {ev['step']:3} {','.join(ev['files_reverted'])[:44]:44} "
                  f"span {ev['span']} "
                  f"{'ACCEPT' if ev['accepted'] else 'reject'}: {ev['reason']}")


if __name__ == "__main__":
    main()

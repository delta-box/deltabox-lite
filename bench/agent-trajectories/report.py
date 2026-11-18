#!/usr/bin/env python3
"""Builds a local HTML report of the replays: every step of every trajectory,
its transaction, its time under each backend, with commit/abort highlighted.

    report.py [policy]     writes target/agent-traj-cache/report.<policy>.html
    report.py --aborts     every trajectory of results/exact/ with an abort (or
                           a rejected one), per_write
                           → target/agent-traj-cache/report.aborts.html

The page quotes the agents' commands, so it is written to the local cache,
not to this folder (see README "Licensing").
"""

import csv
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from fetch import CACHE  # noqa: E402

ABORTS = sys.argv[1:2] == ["--aborts"]
policy = "per_write" if ABORTS else (sys.argv[1] if len(sys.argv) > 1 else "per_eval")
BACKENDS = ["manual", "git"]


def read(iid, b):
    p = HERE / "results" / f"{iid}.{b}.{policy}.csv"
    return list(csv.DictReader(open(p))) if p.exists() else None


src = json.loads((HERE / "sources.json").read_text())
if ABORTS:
    import annotate  # noqa: E402
    instances = []
    for p in sorted((HERE / "results" / "exact").glob("*.json")):
        ex = json.loads(p.read_text())
        if ex["aborts"] or ex["dropped_aborts"] or ex["overlapping_reverts"]:
            why = (f"{ex['aborts']} abort(s), {ex['commits']} commits, {ex['steps']} steps"
                   + ("" if ex["verified"] else f"; NOT verified (diverged at step {ex['diverged_at']})")
                   + (f"; {len(ex['dropped_aborts'])} rejected by replay" if ex["dropped_aborts"] else "")
                   + (f"; {len(ex['overlapping_reverts'])} overlapping" if ex["overlapping_reverts"] else ""))
            instances.append({"instance_id": ex["instance_id"], "resolved": ex["resolved"],
                              "why": why})
else:
    instances = src["instances"]


def commands(iid):
    """The trajectory's commands and repository: from annotate.py's replay
    cache, or straight from the raw trajectory."""
    p = CACHE / "replay" / f"{iid}.json"
    if p.exists():
        return json.loads(p.read_text())
    steps, _ = annotate.load_trajectory(src["run"], iid)
    return {"steps": steps, "source": {}, "transactions": {}, "revert_events": []}


data = []
for inst in instances:
    iid = inst["instance_id"]
    runs = {b: read(iid, b) for b in BACKENDS}
    if not all(runs.values()):
        data.append({"id": iid, "missing": True, "why": inst["why"]})
        continue
    t = commands(iid)
    # per_write comes from exact.py (transactions derived from observed
    # workspace states and verified by replay); other policies from annotate.py.
    ex_p = HERE / "results" / "exact" / f"{iid}.json"
    ex = json.loads(ex_p.read_text()) if policy == "per_write" and ex_p.exists() else None
    txs = ex["transactions"] if ex else t["transactions"][policy]
    rows = []
    for parts in zip(*(runs[b] for b in BACKENDS)):
        r0 = parts[0]
        row = {"op": r0["op"], "tx": int(r0["tx"]), "outcome": r0["outcome"],
               "ms": {b: float(p["ms"] or 0) for b, p in zip(BACKENDS, parts)},
               "same": len({p["tree"] for p in parts}) == 1}
        if r0["op"] == "exec":
            k = int(r0["step"])
            row.update(step=k, kind=r0["kind"], cmd=t["steps"][k]["cmd"],
                       rc={b: p["rc"] for b, p in zip(BACKENDS, parts)},
                       rec=r0["recorded_rc"])
        elif r0["op"] == "abort":
            tx = next(x for x in txs if x["id"] == row["tx"])
            files = tx.get("files") or sorted({p for k in range(tx["first"], tx["last"] + 2)
                                               for p in ex["per_step"][k]["changed"]})
            row.update(replaces=[{"step": r, "cmd": t["steps"][r]["cmd"]} for r in tx["replaces"]],
                       span=[tx["first"], tx["last"]], files=files)
        if r0["op"] != "begin":
            rows.append(row)
    if ex:
        ev = ([{"step": d["step"], "span": None, "accepted": False, "files_reverted": [],
                "reason": f"rollback changed a later step (diverged at step {d['diverged_at']})"}
               for d in ex["dropped_aborts"]]
              + [{"step": o["step"], "span": o["span"], "accepted": False, "files_reverted": [],
                  "reason": o["reason"]} for o in ex["overlapping_reverts"]])
    else:
        ev = [{k: e[k] for k in ("step", "span", "accepted", "reason", "files_reverted")}
              for e in t["revert_events"]]
    data.append({"id": iid, "repo": t["source"].get("repo") or "/".join(
                     [iid.split("__")[0], iid.split("__")[1].rsplit("-", 1)[0]]),
                 "resolved": inst["resolved"], "why": inst["why"],
                 "url": f"https://swe-bench-submissions.s3.amazonaws.com/bash-only/{src['run']}"
                        f"/trajs/{iid}/{iid}.traj.json",
                 "rows": rows, "events": ev})

# Git's commit/abort cost under every policy that has been replayed.
# Compared on the trajectories replayed under every policy that has results.
POLICIES_DONE = ["per_write"] if ABORTS else [
    q for q in ("per_eval", "per_write", "minimal")
    if list((HERE / "results").glob(f"*.git.{q}.csv"))]
compare = []
for pol in POLICIES_DONE:
    tot = {"policy": pol, "n": 0, "commits": 0, "commit_ms": 0.0, "aborts": 0, "abort_ms": 0.0,
           "cmd_ms": 0.0, "insts": []}
    for inst in instances:
        p = HERE / "results" / f"{inst['instance_id']}.git.{pol}.csv"
        if not p.exists() or not all(
                (HERE / "results" / f"{inst['instance_id']}.git.{q}.csv").exists()
                for q in POLICIES_DONE):
            continue
        rs = list(csv.DictReader(open(p)))
        tot["insts"].append(inst["instance_id"])
        for r in rs:
            ms = float(r["ms"] or 0)
            if r["op"] == "commit":
                tot["commits"] += 1
                tot["commit_ms"] += ms
            elif r["op"] == "abort":
                tot["aborts"] += 1
                tot["abort_ms"] += ms
            elif r["op"] == "exec":
                tot["cmd_ms"] += ms
    if tot["insts"]:
        compare.append(tot)

meta = {"run": src["run"], "model": src["model"], "agent": src["agent"], "policy": policy,
        "backends": BACKENDS}
html = (HERE / "report_template.html").read_text()
html = html.replace("/*DATA*/null", json.dumps({"meta": meta, "inst": data,
                       "compare": compare}).replace("</", "<\\/"))
out = CACHE / ("report.aborts.html" if ABORTS else f"report.{policy}.html")
out.write_text(html)
print(out)

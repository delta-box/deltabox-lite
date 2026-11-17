#!/usr/bin/env python3
"""Builds a local HTML report of the replays: every step of every trajectory,
its transaction, its time under each backend, with commit/abort highlighted.

    report.py [policy]     writes target/agent-traj-cache/report.<policy>.html

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

policy = sys.argv[1] if len(sys.argv) > 1 else "per_eval"
BACKENDS = ["manual", "git"]


def read(iid, b):
    p = HERE / "results" / f"{iid}.{b}.{policy}.csv"
    return list(csv.DictReader(open(p))) if p.exists() else None


data = []
src = json.loads((HERE / "sources.json").read_text())
for inst in src["instances"]:
    iid = inst["instance_id"]
    runs = {b: read(iid, b) for b in BACKENDS}
    if not all(runs.values()):
        data.append({"id": iid, "missing": True, "why": inst["why"]})
        continue
    t = json.loads((CACHE / "replay" / f"{iid}.json").read_text())
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
            tx = next(x for x in t["transactions"][policy] if x["id"] == row["tx"])
            row.update(replaces=[{"step": r, "cmd": t["steps"][r]["cmd"]} for r in tx["replaces"]],
                       span=[tx["first"], tx["last"]], files=tx["files"])
        if r0["op"] != "begin":
            rows.append(row)
    ev = [{k: e[k] for k in ("step", "span", "accepted", "reason", "files_reverted")}
          for e in t["revert_events"]]
    data.append({"id": iid, "repo": t["source"].get("repo", iid.split("__")[0]),
                 "resolved": inst["resolved"], "why": inst["why"],
                 "url": f"https://swe-bench-submissions.s3.amazonaws.com/bash-only/{src['run']}"
                        f"/trajs/{iid}/{iid}.traj.json",
                 "rows": rows, "events": ev})

# Git's commit/abort cost under every policy that has been replayed.
# Compared on the trajectories replayed under every policy that has results.
POLICIES_DONE = [q for q in ("per_eval", "per_write", "minimal")
                 if list((HERE / "results").glob(f"*.git.{q}.csv"))]
compare = []
for pol in POLICIES_DONE:
    tot = {"policy": pol, "n": 0, "commits": 0, "commit_ms": 0.0, "aborts": 0, "abort_ms": 0.0,
           "cmd_ms": 0.0, "insts": []}
    for inst in src["instances"]:
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
out = CACHE / f"report.{policy}.html"
out.write_text(html)
print(out)

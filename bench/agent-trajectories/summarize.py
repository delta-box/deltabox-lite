#!/usr/bin/env python3
"""Summarise replay CSVs: fidelity against the recording and against the
reference backend, and the cost of commit/abort per backend.

    summarize.py [policy]          writes results/summary.<policy>.md
"""

import csv
import json
import statistics as st
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
policy = sys.argv[1] if len(sys.argv) > 1 else "per_eval"
REF = "manual"


def rows(iid, b):
    p = HERE / "results" / f"{iid}.{b}.{policy}.csv"
    return list(csv.DictReader(open(p))) if p.exists() else None


def ms(rs, op):
    return [float(r["ms"]) for r in rs if r["op"] == op]


ids = [i["instance_id"] for i in json.loads((HERE / "sources.json").read_text())["instances"]]
backends = sorted({p.name.split(".")[1] for p in (HERE / "results").glob(f"*.{policy}.csv")},
                  key=lambda b: (b != REF, b))
out = [f"# Replay results ({policy} policy)", ""]

out += ["## Fidelity", "",
        "`rc = recorded`: commands whose exit code matches the recording. "
        f"`tree = {REF}`: transaction boundaries where the workspace hash equals the "
        f"`{REF}` run's (the agent's own commands).", "",
        "| instance | backend | steps | rc = recorded | tree = manual | aborts |",
        "|---|---|---:|---:|---:|---:|"]
for iid in ids:
    ref = rows(iid, REF)
    for b in backends:
        rs = rows(iid, b)
        if rs is None:
            continue
        ex = [r for r in rs if r["op"] == "exec" and r["recorded_rc"] != ""]
        same_rc = sum(r["rc"] == r["recorded_rc"] for r in ex)
        bounds = [r for r in rs if r["op"] in ("commit", "abort")]
        if ref is not None:
            rb = [r for r in ref if r["op"] in ("commit", "abort")]
            same_tree = f"{sum(x['tree'] == y['tree'] for x, y in zip(bounds, rb))}/{len(rb)}"
        else:
            same_tree = "–"
        out.append(f"| {iid} | {b} | {sum(r['op'] == 'exec' for r in rs)} | "
                   f"{same_rc}/{len(ex)} | {same_tree} | {sum(r['op'] == 'abort' for r in rs)} |")

out += ["", "## Cost", "",
        "Milliseconds. Agent commands are the trajectory's own steps (identical work in "
        "every backend); commit/abort are the backend's operations. For `manual`, an abort "
        "is the agent's own revert command and a commit costs nothing.", "",
        "| backend | agent commands (total s) | commits | commit median | commit total (s) "
        "| aborts | abort median | abort total (s) | overhead / command time |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|"]
for b in backends:
    allr = [r for iid in ids for r in (rows(iid, b) or [])]
    if not allr:
        continue
    cmd, com, ab = ms(allr, "exec"), ms(allr, "commit"), ms(allr, "abort")
    med = lambda x: f"{st.median(x):.1f}" if x else "–"
    over = (sum(com) + sum(ab)) / sum(cmd) if cmd else 0
    out.append(f"| {b} | {sum(cmd) / 1e3:.1f} | {len(com)} | {med(com)} | {sum(com) / 1e3:.2f} "
               f"| {len(ab)} | {med(ab)} | {sum(ab) / 1e3:.2f} | {over:.1%} |")

p = HERE / "results" / f"summary.{policy}.md"
p.write_text("\n".join(out) + "\n")
print("\n".join(out))

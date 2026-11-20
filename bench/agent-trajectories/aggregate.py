#!/usr/bin/env python3
"""Tables from exact.py's results: per repository and per instance.

    aggregate.py [results/exact]      writes results/exact-summary.{md,csv}
"""

import collections
import csv
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
src = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "results" / "exact"
rs = [json.loads(p.read_text()) for p in sorted(src.glob("*.json"))]
if not rs:
    sys.exit(f"no results in {src}")
repo = lambda iid: iid.split("__")[0] + "/" + iid.split("__")[1].rsplit("-", 1)[0]

cols = ["instance_id", "resolved", "steps", "commits", "aborts", "aborted_steps", "verified",
        "rc_match_recording", "rc_recorded", "rounds", "image_digest", "trajectory_sha256"]
with open(HERE / "results" / "exact-summary.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=cols, extrasaction="ignore")
    w.writeheader()
    w.writerows(rs)

by = collections.defaultdict(list)
for r in rs:
    by[repo(r["instance_id"])].append(r)
tot = lambda xs, k: sum(x[k] for x in xs)
md = [f"# Commit/abort counts from verified replays ({rs[0]['run']})", "",
      f"{len(rs)} trajectories replayed in their SWE-bench images; "
      f"{sum(r['verified'] for r in rs)} verified end to end. Commit = after any command that "
      "changed /testbed (per_write); abort = a command that put the whole /testbed back to "
      "an earlier state, replaced by a rollback and verified by replay.", "",
      "| repository | trajectories | commands | commits (per_write) | aborts | "
      "commands per commit | aborts per 100 commits |", "|---|---:|---:|---:|---:|---:|---:|"]
for name, xs in sorted(by.items(), key=lambda kv: -len(kv[1])) + [("**total**", rs)]:
    c, a, s = tot(xs, "commits"), tot(xs, "aborts"), tot(xs, "steps")
    md.append(f"| {name} | {len(xs)} | {s} | {c} | {a} | {s / c:.1f} | {100 * a / c:.1f} |"
              if c else f"| {name} | {len(xs)} | {s} | 0 | {a} | – | – |")
bad = [r for r in rs if not r["verified"]]
md += ["", f"* trajectories with at least one abort: {sum(r['aborts'] > 0 for r in rs)} "
           f"of {len(rs)}",
       f"* aborts dropped because replaying with the rollback changed a later step: "
       f"{sum(len(r['dropped_aborts']) for r in rs)}",
       f"* exit codes equal to the recording: {tot(rs, 'rc_match_recording')} of "
       f"{tot(rs, 'rc_recorded')}"]
if bad:
    md += ["", "## Not verified", "",
           "The verify replay diverged with no abort before the divergence (the environment "
           "or a non-deterministic command), so these counts are unverified:", ""]
    md += [f"* {r['instance_id']}: diverged at step {r['diverged_at']}" for r in bad]
(HERE / "results" / "exact-summary.md").write_text("\n".join(md) + "\n")
print("\n".join(md))

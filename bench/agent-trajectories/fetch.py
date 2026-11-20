#!/usr/bin/env python3
"""Download the selected SWE-bench agent trajectories and check them.

Sources (all public, no login):
  trajectories  https://swe-bench-submissions.s3.amazonaws.com/bash-only/<run>/trajs/<id>/<id>.traj.json
  outcomes      https://github.com/SWE-bench/experiments  evaluation/verified/<run>/per_instance_details.json

`sources.json` lists the run, the instances and the SHA-256 of every raw file.
A first fetch fills in missing hashes (`--lock`); later fetches verify them, so
a rebuilt dataset is byte-for-byte the one described in the README.

Raw files go to target/agent-traj-cache/raw/ (not committed: the upstream
trajectory archive carries no licence, so this folder stores references and
derived annotations, not copies of the transcripts).
"""

import argparse
import hashlib
import json
import time
import urllib.error
import os
import sys
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
# Downloads, unpacked images and command outputs (large; never committed).
# AGENT_TRAJ_CACHE moves it, e.g. to a big data disk.
CACHE = Path(os.environ.get("AGENT_TRAJ_CACHE", HERE.parents[1] / "target" / "agent-traj-cache"))
S3 = "https://swe-bench-submissions.s3.amazonaws.com/bash-only"
GH_RAW = "https://raw.githubusercontent.com/SWE-bench/experiments/main/evaluation/verified"


def traj_url(run, iid):
    return f"{S3}/{run}/trajs/{iid}/{iid}.traj.json"


def details_url(run):
    return f"{GH_RAW}/{run}/per_instance_details.json"


def download(url, dest):
    dest.parent.mkdir(parents=True, exist_ok=True)
    for attempt in range(6):
        if dest.exists():
            break
        try:
            with urllib.request.urlopen(url, timeout=120) as r:
                data = r.read()
            tmp = dest.with_suffix(".part")
            tmp.write_bytes(data)
            tmp.rename(dest)
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError) as e:
            if attempt == 5 or getattr(e, "code", None) == 404:
                raise
            time.sleep(10 * (attempt + 1))
    return hashlib.sha256(dest.read_bytes()).hexdigest()


def raw_path(run, iid):
    return CACHE / "raw" / run / f"{iid}.traj.json"


def load_sources():
    return json.loads((HERE / "sources.json").read_text())


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lock", action="store_true", help="record hashes that are missing")
    a = ap.parse_args()
    src = load_sources()
    run = src["run"]
    det_path = CACHE / "raw" / run / "per_instance_details.json"
    det_sha = download(details_url(run), det_path)
    details = json.loads(det_path.read_text())
    bad = 0
    for inst in src["instances"]:
        iid = inst["instance_id"]
        sha = download(traj_url(run, iid), raw_path(run, iid))
        if inst.get("sha256") and inst["sha256"] != sha:
            print(f"MISMATCH {iid}: {sha} != {inst['sha256']}", file=sys.stderr)
            bad += 1
        elif a.lock:
            inst["sha256"] = sha
        inst["resolved"] = details[iid]["resolved"]
        print(f"ok {iid} {sha[:12]} resolved={inst['resolved']}")
    if a.lock:
        src["details_sha256_at_lock"] = det_sha
        (HERE / "sources.json").write_text(json.dumps(src, indent=2) + "\n")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()

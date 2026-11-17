#!/usr/bin/env python3
"""Experiment 3: operating range of commit-biased CoW.

Every round runs the same deterministic 16-command sequence in ONE session /
transaction and ends with ONE commit or abort (unified commit boundary). The
commit ratio is a synthetic, controlled parameter (0/25/50/75/100%) used to
map the crossover, not a model of real acceptance rates. Rounds are
interleaved deterministically (25%: every 4th round commits).

Workspace: 99 source files of 8 KiB (src/f00.py .. src/f98.py) plus one
32 MiB pseudo-random asset (assets/data.bin): 100 files.

Sequence: 10 reads (ls, cat, grep, wc, find, head|md5sum, tail, sort|uniq),
4 in-place 4 KiB edits (three source files and the 32 MiB asset), 1 new 4 KiB
file, 1 rename. See COMMANDS.

Systems (system / mode / fsync):
  git-bwrap  git-snapshot      git-default  start: git add -A && git commit;
                                            16 x bwrap; commit: add+commit;
                                            abort: reset --hard + clean -fd
  sandlock   unified           off          ONE raw_sandlock run executing all
                                            16 commands in one sandboxed shell,
                                            then one commit/abort. Sandlock has
                                            no multi-invocation transaction, so
                                            this is the only unified-boundary
                                            form; it pays one sandbox setup, not
                                            16 (favours Sandlock).
  deltabox   cbcow-through     off|on       session start, 16 x deltabox exec,
  deltabox   cbcow-private     off|on       commit/abort (stop is not timed)
  sandlock   per-command       off          SEPARATE "session-reuse" reference:
                                            16 independent runs, each committing.
                                            Only defined at 100% commit; excluded
                                            from the crossover analysis.

Before every round the workspace is reset to the template (only paths that
differ are rewritten) and dirty data is flushed (`sync`); neither is timed.
"""

import argparse
import csv
import filecmp
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from benchlib import (KiB, MiB, Deltabox, GitBwrap, RawSandlock, env_info, make_random_file,
                      REPO)

FIELDS = ["system", "mode", "fsync", "commit_rate_pct", "round_idx", "outcome",
          "session_start_ms", "exec_ms", "finalize_ms", "total_round_ms"]
RATES = [0, 25, 50, 75, 100]


def commands(payload):
    def edit(path, offset):
        return (f"dd if={payload} of={path} bs=4096 count=1 seek={offset} oflag=seek_bytes "
                f"conv=notrunc status=none")
    return [
        ("read", "ls -R"),
        ("read", "cat src/f00.py"),
        ("read", "grep -rn TODO src"),
        ("edit", edit("src/f01.py", 0)),
        ("read", "wc -l src/f1*.py"),
        ("read", "find src assets -name '*.py'"),
        ("edit", edit("assets/data.bin", 16 * MiB)),
        ("read", "head -c 65536 assets/data.bin | md5sum"),
        ("read", "grep -l import src/*.py"),
        ("create", f"dd if={payload} of=src/new_module.py bs=4096 count=1 status=none"),
        ("read", "cat src/f10.py src/f11.py src/f12.py"),
        ("edit", edit("src/f02.py", 4096)),
        ("read", "tail -n 20 src/f20.py"),
        ("meta", "mv src/f98.py src/f98_renamed.py"),
        ("edit", edit("src/f03.py", 0)),
        ("read", "sort src/f30.py | uniq | wc -l"),
    ]


RESTORE = ["src/f01.py", "src/f02.py", "src/f03.py", "src/f98.py", "assets/data.bin"]
REMOVE = ["src/new_module.py", "src/f98_renamed.py"]


def make_template(d):
    (d / "src").mkdir(parents=True)
    (d / "assets").mkdir()
    for i in range(99):
        lines, j = [], 0
        while sum(map(len, lines)) < 8192:
            lines.append(f"import mod{j % 7}\n# TODO item {i}-{j}\n"
                         f"def f_{i}_{j}(x):\n    return x * {j} + {i}\n")
            j += 1
        (d / "src" / f"f{i:02}.py").write_text("".join(lines)[:8192])
    make_random_file(d / "assets" / "data.bin", 32 * MiB, seed=32)


def tree(d):
    skip = {".git", ".deltabox"}
    out = set()
    for root, dirs, files in os.walk(d):
        dirs[:] = [x for x in dirs if x not in skip]
        for f in files:
            out.add(os.path.relpath(os.path.join(root, f), d))
    return out


def reset(ws, template):
    for rel in REMOVE:
        (ws / rel).unlink(missing_ok=True)
    for rel in RESTORE:
        src, dst = template / rel, ws / rel
        if not dst.exists() or not filecmp.cmp(src, dst, shallow=False):
            shutil.copyfile(src, dst)


def verify(ws, template):
    t = tree(template)
    if tree(ws) != t:
        raise SystemExit(f"workspace {ws} file set differs from template")
    for rel in t:
        if not filecmp.cmp(template / rel, ws / rel, shallow=False):
            raise SystemExit(f"workspace {ws}: {rel} differs from template")


def schedule(rate, rounds):
    return ["commit" if (i + 1) * rate // 100 > i * rate // 100 else "abort"
            for i in range(rounds)]


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--workdir", type=Path, default=REPO / "target/bench-exp3")
    ap.add_argument("--payload-dir", type=Path, default=None)
    ap.add_argument("--out", type=Path,
                    default=REPO / "bench/delta-driven/results/exp3_commit_bias.csv")
    ap.add_argument("--rounds", type=int, default=100)
    ap.add_argument("--warmups", type=int, default=3)
    ap.add_argument("--rates", default=",".join(map(str, RATES)))
    ap.add_argument("--systems",
                    default="git-bwrap,sandlock-unified,cbcow-through,cbcow-private,"
                            "sandlock-per-command")
    ap.add_argument("--no-sync", action="store_true")
    a = ap.parse_args()

    rates = [int(x) for x in a.rates.split(",")]
    systems = a.systems.split(",")
    a.workdir.mkdir(parents=True, exist_ok=True)
    a.out.parent.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="run-", dir=a.workdir))
    payload_dir = Path(tempfile.mkdtemp(prefix="deltabox-exp3-payload-",
                                        dir=str(a.payload_dir or "/tmp")))
    payload = payload_dir / "edit-4k"
    make_random_file(payload, 4 * KiB, seed=4)
    cmds = commands(payload)
    assert [k for k, _ in cmds].count("read") == 10 and len(cmds) == 16
    template = root / "template"
    make_template(template)

    meta = {"env": env_info(a.workdir), "rounds": a.rounds, "warmups": a.warmups,
            "rates": rates, "commands": cmds, "started": time.strftime("%F %T")}
    print(json.dumps(meta["env"]), file=sys.stderr)

    configs = []
    for s in systems:
        if s == "git-bwrap":
            configs.append(("git-bwrap", "git-snapshot", "git-default"))
        elif s == "sandlock-unified":
            configs.append(("sandlock", "unified", "off"))
        elif s in ("cbcow-through", "cbcow-private"):
            configs += [("deltabox", s, "off"), ("deltabox", s, "on")]
        elif s == "sandlock-per-command":
            configs.append(("sandlock", "per-command", "off"))

    out = open(a.out, "w", newline="")
    w = csv.DictWriter(out, fieldnames=FIELDS)
    w.writeheader()

    def do_round(system, mode, fsync, ws, r, outcome):
        argvs = [["sh", "-c", c] for _, c in cmds]
        if system == "sandlock" and mode == "unified":
            script = "set -e\n" + "\n".join(c for _, c in cmds)
            e, f, _ = r.run_once(["sh", "-c", script], outcome)
            return 0.0, e, f
        if system == "sandlock":  # per-command, each run commits on its own
            e = f = 0.0
            for argv in argvs:
                we, wf, _ = r.run_once(argv, "commit")
                e, f = e + we, f + wf
            return 0.0, e, f
        s = r.start()
        try:
            e = sum(r.exec(argv) for argv in argvs)
            f = r.finalize(outcome)
        finally:
            r.stop()
        return s, e, f

    try:
        for system, mode, fsync in configs:
            ws = root / f"ws-{system}-{mode}-{fsync}"
            shutil.copytree(template, ws)
            if system == "git-bwrap":
                r = GitBwrap(ws)
            elif system == "sandlock":
                r = RawSandlock(ws, root / f"cow-{mode}")
            else:
                r = Deltabox(ws, mode, fsync == "on", root / f"state-{mode}-{fsync}")
            my_rates = [100] if mode == "per-command" else rates
            for i in range(a.warmups):
                reset(ws, template)
                do_round(system, mode, fsync, ws, r,
                         "commit" if mode == "per-command" or i % 2 else "abort")
            for rate in my_rates:
                t0 = time.time()
                for idx, outcome in enumerate(schedule(rate, a.rounds)):
                    reset(ws, template)
                    if not a.no_sync:
                        os.sync()
                    s, e, f = do_round(system, mode, fsync, ws, r, outcome)
                    w.writerow({"system": system, "mode": mode, "fsync": fsync,
                                "commit_rate_pct": rate, "round_idx": idx, "outcome": outcome,
                                "session_start_ms": round(s, 3), "exec_ms": round(e, 3),
                                "finalize_ms": round(f, 3), "total_round_ms": round(s + e + f, 3)})
                    if idx == 0:
                        # The first round's effect on the host must match its outcome.
                        changed = not filecmp.cmp(template / "src/f01.py", ws / "src/f01.py",
                                                  shallow=False)
                        if changed != (outcome == "commit"):
                            raise SystemExit(f"{system}/{mode}: host state after {outcome} "
                                             f"is wrong (f01 changed={changed})")
                out.flush()
                print(f"{system:9} {mode:13} fsync={fsync:11} rate={rate:3}% "
                      f"{a.rounds} rounds in {time.time() - t0:6.1f}s", file=sys.stderr,
                      flush=True)
            reset(ws, template)
            verify(ws, template)
            shutil.rmtree(ws, ignore_errors=True)
    finally:
        out.close()
        shutil.rmtree(root, ignore_errors=True)
        shutil.rmtree(payload_dir, ignore_errors=True)
        meta["finished"] = time.strftime("%F %T")
        a.out.with_suffix(".meta.json").write_text(json.dumps(meta, indent=2))
    print(f"wrote {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Summarize exp2_scalability.csv and exp3_commit_bias.csv as Markdown."""

import argparse
import csv
import statistics
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent


def pct(xs, p):
    xs = sorted(xs)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * p / 100
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def size(n):
    n = int(n)
    for unit, f in (("MiB", 1 << 20), ("KiB", 1 << 10)):
        if n >= f:
            return f"{n // f} {unit}"
    return f"{n} B"


def nbytes(n):
    n = float(n)
    if n >= 1 << 20:
        return f"{n / (1 << 20):.1f} MiB"
    if n >= 1 << 10:
        return f"{n / (1 << 10):.0f} KiB"
    return f"{n:.0f} B"


def label(system, mode, fsync):
    if system == "bare":
        return f"bare{' +fsync' if fsync == 'on' else ''}"
    if system == "reflink":
        return f"reflink (XFS){' +fsync' if fsync == 'on' else ''}"
    if system == "sandlock":
        return {"file-copy-up": "raw Sandlock", "unified": "raw Sandlock (1 run, unified)",
                "per-command": "raw Sandlock (16 runs) [session-reuse ref]"}[mode]
    if system == "git-bwrap":
        return "Git + bwrap"
    return f"{mode}{' durable' if fsync == 'on' else ''}"


ORDER2 = [("bare", "host", "off"), ("bare", "host", "on"), ("reflink", "reflink-xfs", "off"),
          ("reflink", "reflink-xfs", "on"), ("sandlock", "file-copy-up", "off"),
          ("deltabox", "cbcow-through", "off"), ("deltabox", "cbcow-through", "on"),
          ("deltabox", "cbcow-private", "off"), ("deltabox", "cbcow-private", "on")]


def exp2(path, out):
    rows = list(csv.DictReader(open(path)))
    g = defaultdict(list)
    for r in rows:
        g[(r["case"], r["system"], r["mode"], r["fsync"], int(r["file_size_bytes"]),
           int(r["write_size_bytes"]), r["outcome"])].append(r)
    configs = [c for c in ORDER2 if any(k[1:4] == c for k in g)]

    def table(case, outcome, xkey, metric, fmt, title):
        xs = sorted({k[xkey] for k in g if k[0] == case and k[6] == outcome})
        out.append(f"\n**{title}**\n")
        hdr = "file size" if xkey == 4 else "write size"
        out.append(f"| system | " + " | ".join(size(x) for x in xs) + " | largest / smallest |")
        out.append("|---|" + "---:|" * (len(xs) + 1))
        for c in configs:
            cells, vals = [], []
            for x in xs:
                key = [k for k in g if k[0] == case and k[1:4] == c and k[6] == outcome
                       and k[xkey] == x]
                if not key:
                    cells.append("–")
                    vals.append(None)
                    continue
                v = metric(g[key[0]])
                vals.append(v)
                cells.append(fmt(v))
            if all(v is None for v in vals):
                continue
            ends = [v[0] if isinstance(v, tuple) else v for v in (vals[0], vals[-1])]
            ratio = (f"{ends[1] / ends[0]:.1f}×" if None not in ends and ends[0] > 0 else "–")
            out.append(f"| {label(*c)} | " + " | ".join(cells) + f" | {ratio} |")

    def p50(field):
        return lambda rs: pct([float(r[field]) for r in rs], 50)

    def p5095(field):
        return lambda rs: (pct([float(r[field]) for r in rs], 50),
                           pct([float(r[field]) for r in rs], 95))

    def med_int(field):
        return lambda rs: statistics.median(int(r[field]) for r in rs)

    f_ms = lambda v: f"{v[0]:.1f} / {v[1]:.1f}"
    out.append("## Experiment 2 — scalability\n")
    out.append("Latency cells are p50 / p95 ms; byte cells are medians. "
               "\"largest / smallest\" compares p50 (or medians) at the two ends of the sweep.")
    for case, xkey, xname in (("2A", 4, "file size, 4 KiB edit"),
                              ("2B", 5, "write size, 256 MiB file")):
        outcomes = ["commit", "abort"] if case == "2A" else ["commit"]
        for oc in outcomes:
            out.append(f"\n### Case {case} ({xname}) — {oc}")
            table(case, oc, xkey, p5095("total_ms"), f_ms, f"Total (write + {oc}) ms")
            table(case, oc, xkey, p5095("write_ms"), f_ms, "Write phase ms")
            table(case, oc, xkey, p5095("finalize_ms"), f_ms, f"{oc.capitalize()} phase ms")
            table(case, oc, xkey, med_int("disk_write_bytes"), nbytes, "Disk write I/O")
            table(case, oc, xkey, med_int("peak_cow_bytes"), nbytes, "Peak CoW / undo storage")


def exp3(path, out):
    rows = list(csv.DictReader(open(path)))
    g = defaultdict(list)
    for r in rows:
        g[(r["system"], r["mode"], r["fsync"], int(r["commit_rate_pct"]))].append(r)
    configs = []
    for k in g:
        if k[:3] not in configs:
            configs.append(k[:3])
    rates = sorted({k[3] for k in g})
    tot = lambda rs: sum(float(r["total_round_ms"]) for r in rs) / 1000

    out.append("\n## Experiment 3 — commit-rate sweep\n")
    out.append("Commit ratio is a synthetic, controlled parameter. Each cell sums "
               "`total_round_ms` over all rounds at that ratio (seconds).\n")
    out.append("| system | " + " | ".join(f"{r}%" for r in rates) + " |")
    out.append("|---|" + "---:|" * len(rates))
    for c in configs:
        cells = [f"{tot(g[c + (r,)]):.2f}" if c + (r,) in g else "–" for r in rates]
        out.append(f"| {label(*c)} | " + " | ".join(cells) + " |")

    out.append("\n**Per-round breakdown (mean ms, pooled over all ratios), and p95 of "
               "total_round_ms**\n")
    out.append("| system | outcome | n | session start | 16 commands | finalize | total | "
               "total p95 |")
    out.append("|---|---|---:|---:|---:|---:|---:|---:|")
    means = {}
    for c in configs:
        allr = [r for k, v in g.items() if k[:3] == c for r in v]
        for oc in ("commit", "abort"):
            rs = [r for r in allr if r["outcome"] == oc]
            if not rs:
                continue
            m = {f: statistics.fmean(float(r[f]) for r in rs)
                 for f in ("session_start_ms", "exec_ms", "finalize_ms", "total_round_ms")}
            means[c + (oc,)] = m["total_round_ms"]
            p95 = pct([float(r["total_round_ms"]) for r in rs], 95)
            out.append(f"| {label(*c)} | {oc} | {len(rs)} | {m['session_start_ms']:.1f} | "
                       f"{m['exec_ms']:.1f} | {m['finalize_ms']:.1f} | "
                       f"{m['total_round_ms']:.1f} | {p95:.1f} |")

    baselines = [c for c in configs if c[0] == "git-bwrap" or c[1] == "unified"]
    lites = [c for c in configs if c[0] == "deltabox"]
    out.append("\n**Break-even commit ratio** (Lite faster than the baseline at commit "
               "ratios on the side shown)\n")
    out.append("| DeltaBox Lite | vs | measured crossover | model crossover | "
               "Lite − baseline at 0% / 100% (s) |")
    out.append("|---|---|---|---|---|")
    for L in lites:
        for B in baselines:
            d = [(r, tot(g[L + (r,)]) - tot(g[B + (r,)])) for r in rates
                 if L + (r,) in g and B + (r,) in g]
            cross = []
            for (r0, d0), (r1, d1) in zip(d, d[1:]):
                if d0 == 0:
                    cross.append(f"{r0}%")
                elif d0 * d1 < 0:
                    cross.append(f"{r0 + (r1 - r0) * d0 / (d0 - d1):.0f}%")
            if cross:
                side = "Lite faster above" if d[-1][1] < 0 else "Lite faster below"
                meas = f"{side} {', '.join(cross)}"
            else:
                meas = "Lite faster at every ratio" if all(x < 0 for _, x in d) else \
                       "baseline faster at every ratio"
            # Model: T(p) = p*commit_round + (1-p)*abort_round, per-round means.
            try:
                aL, cL = means[L + ("abort",)], means[L + ("commit",)]
                aB, cB = means[B + ("abort",)], means[B + ("commit",)]
                den = (aL - aB) - (cL - cB)
                p = (aL - aB) / den if den else None
                model = (f"{100 * p:.0f}%" if p is not None and 0 <= p <= 1 else
                         "none in 0–100%")
            except KeyError:
                model = "–"
            out.append(f"| {label(*L)} | {label(*B)} | {meas} | {model} | "
                       f"{d[0][1]:+.2f} / {d[-1][1]:+.2f} |")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exp2", type=Path, default=HERE / "results/exp2_scalability.csv")
    ap.add_argument("--exp3", type=Path, default=HERE / "results/exp3_commit_bias.csv")
    ap.add_argument("--out", type=Path, default=HERE / "results/summary.md")
    a = ap.parse_args()
    out = []
    if a.exp2.exists():
        exp2(a.exp2, out)
    if a.exp3.exists():
        exp3(a.exp3, out)
    text = "\n".join(out) + "\n"
    a.out.write_text(text)
    print(text)


if __name__ == "__main__":
    main()

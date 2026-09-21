#!/usr/bin/env python3
"""Compare two or more `agent sample` result files.

    agent capture session.jsonl 27 -o base.doc
    cp base.doc arm.doc && $EDITOR arm.doc
    agent sample base.doc -n 40 -o base.jsonl
    agent sample arm.doc  -n 40 -o arm.jsonl
    python3 evals/labstats.py base.jsonl arm.jsonl

Every sample is one completion against a *fixed* document, so the
between-run path variance that swamped the task-level A/Bs is absent by
construction and the tests below are the whole analysis.

Two kinds of metric, and they do not always agree -- which is the point
of printing both. A **rate** is the share of samples where something
happened at all (Fisher's exact). A **count** is how much of it there
was (exact Mann-Whitney). On 2026-09-21 the tail-position arms differed
on the count at p = 0.0054 and on the rate at p = 0.24: the ban did not
stop replies drafting, it stopped them spiralling. A tool that reported
only one of those would have told half the story either way.

Both tests are exact and enumerate, so they are honest at n = 8 and
slow past n = 25 per arm -- at which point the normal approximation is
fine anyway and the exact test is a luxury.
"""
import json
import math
import re
import statistics as st
import sys
from itertools import combinations

# A fenced code block opened inside the reasoning stream. The behaviour
# the card's rehearsal ban targets, and the metric that separated when
# reasoning bytes did not.
FENCE = re.compile(r"```(?:js|javascript|ts|typescript)\b")


def load(path):
    rows = []
    for line in open(path):
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    ok = [r for r in rows if not r.get("error")]
    return ok, len(rows) - len(ok)


def metrics(r):
    """What one completion is worth measuring by.

    Anything here is derived from the stored text, so a metric nobody
    thought of in advance can still be run over samples already paid
    for -- which is exactly what was not possible for the task-level
    runs.
    """
    think = r.get("thinking") or ""
    src = r.get("source") or ""
    u = r.get("usage") or {}
    return {
        "drafts": len(FENCE.findall(think)),
        "think_b": len(think),
        "reply_b": len(src),
        "cells": len(re.findall(r"^```(?:js|ts)\b", src, re.M)),
        "reasoning_tok": u.get("reasoning", 0),
        "completion_tok": u.get("completion", 0),
        "ms": r.get("ms", 0),
    }


def mannwhitney(x, y):
    """Exact two-sided Mann-Whitney U. Returns (U, p)."""
    n = len(x) * len(y)
    u = sum((a > b) + 0.5 * (a == b) for a in x for b in y)
    pool = sorted(x + y)
    tot = hit = 0
    for c in combinations(range(len(pool)), len(x)):
        cs = set(c)
        xs = [pool[k] for k in c]
        ys = [pool[k] for k in range(len(pool)) if k not in cs]
        uu = sum((a > b) + 0.5 * (a == b) for a in xs for b in ys)
        tot += 1
        hit += abs(uu - n / 2) >= abs(u - n / 2) - 1e-9
    return u, hit / tot


def fisher(a, b, c, d):
    """Exact two-sided Fisher on [[a,b],[c,d]]."""
    n = a + b + c + d
    obs = math.comb(a + b, a) * math.comb(c + d, c) / math.comb(n, a + c)
    tot = 0.0
    for i in range(0, min(a + b, a + c) + 1):
        j = a + c - i
        if 0 <= j <= c + d:
            p = math.comb(a + b, i) * math.comb(c + d, j) / math.comb(n, a + c)
            if p <= obs + 1e-12:
                tot += p
    return min(tot, 1.0)


def main(paths):
    arms = {}
    for p in paths:
        ok, failed = load(p)
        if not ok:
            sys.exit(f"{p}: no usable samples")
        arms[p] = [metrics(r) for r in ok]
        note = f"  ({failed} failed, dropped)" if failed else ""
        print(f"{p}: n={len(ok)}{note}")

    keys = ["drafts", "think_b", "reply_b", "cells", "reasoning_tok", "completion_tok", "ms"]
    print(f"\n{'':<28}" + "".join(f"{k:>15}" for k in keys))
    for p, rows in arms.items():
        med = "".join(f"{st.median([r[k] for r in rows]):>15.0f}" for k in keys)
        print(f"{p[-28:]:<28}{med}")

    names = list(arms)
    for a, b in combinations(names, 2):
        print(f"\n=== {a}  vs  {b}")
        # Rate: did it happen at all. Robust, and answers a different
        # question from the count below.
        xa = sum(1 for r in arms[a] if r["drafts"] > 0)
        xb = sum(1 for r in arms[b] if r["drafts"] > 0)
        na, nb = len(arms[a]), len(arms[b])
        print(
            f"  drafted at all   {xa}/{na} = {100*xa/na:3.0f}%   "
            f"{xb}/{nb} = {100*xb/nb:3.0f}%   Fisher p={fisher(xa, na-xa, xb, nb-xb):.4f}"
        )
        # Counts.
        big = na * nb > 3_000_000
        for k in keys:
            x = [r[k] for r in arms[a]]
            y = [r[k] for r in arms[b]]
            if big:
                print(f"  {k:<16} median {st.median(x):>9.0f} {st.median(y):>9.0f}   (n too large for the exact test)")
                continue
            u, pv = mannwhitney(x, y)
            star = " *" if pv < 0.05 else ""
            print(f"  {k:<16} median {st.median(x):>9.0f} {st.median(y):>9.0f}   p={pv:.4f}{star}")
    if len(names) > 2:
        print(
            f"\nNote: {math.comb(len(names), 2)} pairs x {len(keys)+1} metrics were tested. "
            "Decide which comparison is the primary one before reading the stars."
        )


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    main(sys.argv[1:])

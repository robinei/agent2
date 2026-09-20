#!/usr/bin/env python3
"""Tool calls for a path the run already has on the record.

A `read_file` result is a row, fetchable by id for nothing. Reading the
same path again spends a call and a round trip to get bytes the log
already holds — and unlike printing them back (`evals/echo.py`), it
costs the tool call too.

    python3 evals/rereads.py ~/.claude/jobs/*/tmp

What makes it worth measuring apart from `echo.py`: a run that re-reads
is one that kept no working state. `sweep-40` on 2026-09-20 read four
files three times across nine programs and appended **no rows at all**,
then hit the driver's timeout without converging.
"""
import collections, json, pathlib, sys

def scan(path):
    seen, re_reads, reads = set(), 0, 0
    try:
        lines = path.open().readlines()
    except OSError:
        return None
    for l in lines:
        try:
            e = json.loads(l)
        except ValueError:
            continue
        p = e.get("payload")
        if not isinstance(p, dict) or "Call" not in p:
            continue
        inv = p["Call"].get("Invoke")
        if not inv or inv.get("name") != "read_file":
            continue
        args = inv.get("args") or []
        if not args:
            continue
        key = str(args[0])
        reads += 1
        if key in seen:
            re_reads += 1
        seen.add(key)
    return (reads, re_reads) if reads else None

def main(argv):
    roots = [a for a in argv if not a.startswith("--")] or ["."]
    logs = tot = again = 0
    worst = []
    for root in roots:
        for p in sorted(pathlib.Path(root).rglob("*.jsonl")):
            got = scan(p)
            if not got:
                continue
            reads, re_reads = got
            logs += 1; tot += reads; again += re_reads
            if re_reads:
                worst.append((re_reads, p.parent.name))
    if not tot:
        print("no read_file calls found")
        return 1
    print(f"{logs} logs: {tot} read_file calls, {again} for a path already read ({100*again/tot:.0f}%)")
    for n, name in sorted(worst, reverse=True)[:5]:
        print(f"    {n:3} re-reads  {name}")
    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

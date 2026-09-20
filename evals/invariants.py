#!/usr/bin/env python3
"""Hold real logs to the rules the tests hold replies to.

`agent/src/testkit.rs` checks a handful of properties after every reply
a test feeds — one `Reply` per completion, every reply ends, every
handback names its reply.  Those rules are about the harness, not about
the fixture, so they are just as true of a log a live run left behind —
and a corpus of real runs exercises shapes no fixture reaches.

    python3 evals/invariants.py ~/.claude/jobs/*/tmp
    python3 evals/invariants.py --verbose <dir>   # name every instance

It reads the log as JSON and knows nothing about the binary, so it
keeps working across the schema changes that stop `agent` opening an
old log at all.  Logs it cannot parse are counted and skipped rather
than failing the sweep: this is a net for anomalies, not a validator.

Found on 2026-09-20, first run, across 373 kept logs:

  - 7 replies that completed with no `ReplyEnd`.  Their cost went
    unrecorded, so every per-reply measure divided by the wrong number.
  - 1 handback naming an event that is not a reply — the interrupted-run
    repair writing a `Runner`'s starting sentinel, fixed the same day.
"""

import collections
import json
import pathlib
import sys


def events(path):
    """`{id: payload}` for a log, or None if it is not one we can read."""
    out = {}
    try:
        with path.open() as f:
            for line in f:
                try:
                    e = json.loads(line)
                except ValueError:
                    continue
                if "id" in e and "payload" in e:
                    out[e["id"]] = e["payload"]
    except OSError:
        return None
    return out or None


def kind(payload):
    return list(payload.keys())[0] if isinstance(payload, dict) else payload


def check(ids):
    """Every rule broken by this log, as `(rule, detail)` pairs."""
    broken = []
    kinds = {i: kind(p) for i, p in ids.items()}
    replies = {i for i, k in kinds.items() if k in ("Reply", "Restart")}
    last = max(ids)

    ends = collections.Counter()
    handbacks = collections.defaultdict(list)
    for i, p in ids.items():
        if not isinstance(p, dict):
            continue
        if "ReplyEnd" in p:
            ends[p["ReplyEnd"].get("reply")] += 1
        if "Handback" in p:
            handbacks[p["Handback"].get("reply")].append((i, p["Handback"].get("how")))

    for named, where in handbacks.items():
        if named not in replies:
            broken.append(
                ("handback names a non-reply", f"#{where[0][0]} → #{named} ({kinds.get(named)})")
            )

    for r in sorted(replies):
        n = ends[r]
        if n > 1:
            broken.append(("reply ends more than once", f"#{r} × {n}"))
        elif n == 0:
            # **The last reply of a log that simply stops is a run the
            # driver killed while the completion was still arriving.**
            # The process died; nothing was owed an end.  Two shapes are
            # not that, and both are the harness getting it wrong: a
            # reply that reached a *terminal* and still has no end, and
            # a reply another one followed — which should have been
            # closed out as `Interrupted` before the next one opened.
            terminal = [h for h in handbacks.get(r, []) if h[1] not in ("Raised", "Posted")]
            if terminal:
                broken.append(
                    ("reply completed with no ReplyEnd", f"#{r}, handback {terminal[0]}")
                )
            elif any(later > r for later in replies):
                broken.append(
                    ("a later reply opened before this one ended", f"#{r}, log ends at #{last}")
                )
    return broken


def main(argv):
    verbose = "--verbose" in argv
    roots = [a for a in argv if not a.startswith("--")] or ["."]
    tally = collections.Counter()
    examples = collections.defaultdict(list)
    read = skipped = 0

    for root in roots:
        for path in sorted(pathlib.Path(root).rglob("*.jsonl")):
            ids = events(path)
            if ids is None:
                skipped += 1
                continue
            read += 1
            for rule, detail in check(ids):
                tally[rule] += 1
                examples[rule].append(f"{path}  {detail}")

    print(f"{read} logs read, {skipped} unreadable")
    if not tally:
        print("no rule broken")
        return 0
    for rule, n in tally.most_common():
        print(f"\n{n:6}  {rule}")
        shown = examples[rule] if verbose else examples[rule][:3]
        for line in shown:
            print(f"        {line}")
        if not verbose and len(examples[rule]) > len(shown):
            print(f"        … {len(examples[rule]) - len(shown)} more (--verbose)")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

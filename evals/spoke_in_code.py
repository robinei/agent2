#!/usr/bin/env python3
"""How often a reply spends a program to say a sentence.

`finish(text)` ends the task and hands the person the answer. A reply
whose *only* cell is that one call ran a program to do what prose does
by itself — the card's own rule (card.md: "a reply with no code blocks
in it rests the branch... the right shape for answering a question").

    python3 evals/spoke_in_code.py ~/.claude/jobs/*/tmp

The distinction this counts is narrow on purpose. A `finish` at the end
of a cell that read a file, edited it and checked the result is the
shape every exemplar demonstrates and is not what this is looking for.
A cell containing nothing else is.

Two questions, because they are different mistakes:

  - **a cell that is one call** — `finish(...)` or `tell(...)` and
    nothing else, which is prose spelled in JavaScript.
  - **a reply whose program touched nothing** — it ran cells, and every
    call they made was a `Send`. Nothing was read, nothing was run,
    nothing was written: the program existed to say a sentence. This is
    the wider shape, and the one a conversation provokes — a run of
    `try21.jsonl` answered two ordinary questions this way.

Measured 2026-09-20 across 319 kept logs (task runs, mostly):

    cells that are one call and nothing else   32 of 1856   (2%)
    replies whose program only spoke           see below

Task runs are the wrong corpus for the second number and it is here to
be re-measured against conversations, which is where the reflex shows.
"""

import json
import pathlib
import re
import sys

FENCE = re.compile(r"^```", re.M)
# A cell that is one call and nothing else: optional whitespace, the
# verb, a single argument spanning to the last `)`, an optional `;`.
BARE = re.compile(r"^\s*(finish|tell)\s*\(.*\)\s*;?\s*$", re.S)


def replies_that_only_spoke(path):
    """`(replies that ran a cell, of those, ones that only spoke)`."""
    ran, spoke_only = set(), {}
    try:
        with path.open() as f:
            for line in f:
                try:
                    payload = json.loads(line).get("payload")
                except ValueError:
                    continue
                if not isinstance(payload, dict):
                    continue
                if "Part" in payload:
                    reply = payload["Part"].get("reply")
                    part = payload["Part"].get("part")
                    if isinstance(part, dict) and "Cell" in part:
                        ran.add(reply)
                        spoke_only.setdefault(reply, True)
                if "Call" in payload and "Invoke" in payload["Call"]:
                    # A tool call, so this program did something. It
                    # belongs to whichever reply is open, which is the
                    # most recent one that ran a cell.
                    if ran:
                        spoke_only[max(ran, key=lambda r: r or 0)] = False
    except OSError:
        return (0, 0)
    return (len(ran), sum(1 for r in ran if spoke_only.get(r)))


def cells(path):
    """Every `Part::Cell` on a log, as source with its fences stripped."""
    out = []
    try:
        with path.open() as f:
            for line in f:
                try:
                    payload = json.loads(line).get("payload")
                except ValueError:
                    continue
                if not isinstance(payload, dict) or "Part" not in payload:
                    continue
                part = payload["Part"].get("part")
                if isinstance(part, dict) and "Cell" in part:
                    body = FENCE.split(part["Cell"])
                    if len(body) > 1:
                        # Past the opening fence's info string (`js`).
                        inner = body[1].split("\n", 1)
                        out.append(inner[1] if len(inner) > 1 else "")
                    else:
                        out.append(part["Cell"])
    except OSError:
        return []
    return out


def main(argv):
    roots = [a for a in argv if not a.startswith("--")] or ["."]
    total = bare = 0
    by_verb = {"finish": 0, "tell": 0}
    logs = 0
    programs = only_spoke = 0
    for root in roots:
        for path in sorted(pathlib.Path(root).rglob("*.jsonl")):
            found = cells(path)
            if not found:
                continue
            logs += 1
            r, s_ = replies_that_only_spoke(path)
            programs += r
            only_spoke += s_
            for source in found:
                total += 1
                m = BARE.match(source.strip())
                if m:
                    bare += 1
                    by_verb[m.group(1)] += 1
    if not total:
        print("no cells found")
        return 1
    print(f"{logs} logs, {total} cells")
    print(f"cells that are one call and nothing else   {bare}  ({100 * bare / total:.0f}%)")
    for verb, n in by_verb.items():
        print(f"    bare {verb:<8} {n}")
    share = 100 * only_spoke / programs if programs else 0
    print(
        f"replies whose program only spoke          {only_spoke} of {programs}  ({share:.0f}%)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

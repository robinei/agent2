#!/usr/bin/env python3
"""How much of what the model reads is something it already had.

A `read_file` result is on the log and fetchable by id, for nothing.
Printing it puts a second copy in the next request — and the console is
where that happens, because printing is how a program puts bytes in
front of the *next* reply without spending a program on fetching them.

    python3 evals/echo.py ~/.claude/jobs/*/tmp

Measured 2026-09-20, before any card change:

    runs printing back a file they just read   235 of 303   (78%)
    console bytes in those sections      1,970,004 of 2,782,871  (71%)

The denominator is runs that *could* echo — ones that both received a
big value and printed something. Against all 377 readable logs it is
62%, which is the wrong fraction to quote: a run with nothing to echo
is not a run that chose not to.

That is not a bug and it is not obviously wrong — a model that has just
read a file and needs to reason about it on the *next* turn has two
choices, print it or fetch it again, and printing is the one that costs
nothing to write. It is worth knowing the size of, and worth measuring
again after any change to what the card says about `console.log`,
`history.append` or the history bound — which is what this is for.

The probe is a 300-character slice from the middle of a value the run
received, looked for in a console section that came after it. Middles
rather than heads, so a shared prefix (a licence header, a shebang)
does not count as an echo.
"""

import json
import pathlib
import sys

# A value has to be at least this big to be worth calling an echo — a
# short result printed back is tracing, which is what the console is for.
BIG = 400
PROBE = slice(100, 400)


def scan(path):
    """`(printed_an_echo, console_bytes, echoed_bytes)` for one log."""
    received, console, echoed, hit = [], 0, 0, False
    try:
        with path.open() as f:
            for line in f:
                try:
                    payload = json.loads(line).get("payload")
                except ValueError:
                    continue
                if not isinstance(payload, dict):
                    continue
                if "Result" in payload:
                    delivered = payload["Result"]["outcome"]
                    if isinstance(delivered, dict) and "Delivered" in delivered:
                        value = delivered["Delivered"]
                        if isinstance(value, dict):
                            for key in ("content", "stdout"):
                                got = value.get(key)
                                if isinstance(got, str) and len(got) > BIG:
                                    received.append(got)
                if "Console" in payload:
                    text = "\n".join(payload["Console"]["lines"])
                    console += len(text)
                    for value in received:
                        probe = value[PROBE]
                        if probe and probe in text:
                            echoed += len(text)
                            hit = True
                            break
    except OSError:
        return None
    return (hit, console, echoed) if console or received else None


def main(argv):
    roots = argv or ["."]
    logs = echoing = 0
    console = echoed = 0
    for root in roots:
        for path in sorted(pathlib.Path(root).rglob("*.jsonl")):
            found = scan(path)
            if found is None:
                continue
            hit, c, e = found
            logs += 1
            echoing += hit
            console += c
            echoed += e
    if not logs:
        print("no logs found")
        return 1
    share = 100 * echoed / console if console else 0
    print(f"runs printing back a file they just read   {echoing} of {logs}   ({100 * echoing / logs:.0f}%)")
    print(f"console bytes in those sections      {echoed:,} of {console:,}  ({share:.0f}%)")
    return 0


if __name__ == "__main__":
    sys.exit(main([a for a in sys.argv[1:] if not a.startswith("--")]))

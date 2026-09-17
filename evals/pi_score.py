"""Fold a `pi` session file into the same shape `agent score` produces.

The point of this file is that both agents are then judged by the *same*
checker: `task.check(env)` reads `env.score` and `env.dir`, and neither
knows which agent filled them.  Correctness is the measurement most
vulnerable to being eyeballed, so it is the one that most needs identical
code on both sides.

Both sides now report real tokens, so they share key names here.  The
one adjustment is that `pi` records `input` *excluding* cache hits while
our provider's `prompt_tokens` includes them, so `prompt_in` is
`input + cacheRead` — a comparison of prompt size that quietly counted
one side's cache and not the other's would flatter whichever side
cached better, which is the very thing worth measuring.

Two fields have no meaning for a tool-loop agent and are reported as
zero rather than guessed: `raises` (nothing ends a turn to write the
next one — the loop simply continues) and `asks` (nothing suspends for a
person in `--print` mode).  A checker that turns on those is asking a
question about code mode, and its task should say so.

The vocabulary mapping, which is where a comparison could quietly cheat:

    programs      one assistant message = one completion = one round trip
    tool_calls    one `toolCall` content block
    calls/program the ratio, which is the whole argument
    tells         assistant `text` blocks, i.e. what reached the person
    source_bytes  those same text blocks
    thinking_bytes  `thinking` blocks
    provider_ms   time from the preceding row to each assistant message
"""

import json
from pathlib import Path


def score_pi_session(path: Path) -> dict:
    rows = []
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))

    programs = tool_calls = 0
    tells, source_bytes, thinking_bytes = [], 0, 0
    tok_fresh = tok_cached = tok_out = 0
    provider_ms = 0
    stamps = []
    prev_ms = None

    def ms(row):
        # "2026-09-16T12:34:03.565Z"
        from datetime import datetime

        return int(
            datetime.fromisoformat(row["timestamp"].replace("Z", "+00:00")).timestamp() * 1000
        )

    for row in rows:
        at = ms(row)
        stamps.append(at)
        if row.get("type") != "message":
            prev_ms = at
            continue
        message = row["message"]
        if message.get("role") == "assistant":
            programs += 1
            if prev_ms is not None:
                provider_ms += at - prev_ms
            usage = message.get("usage") or {}
            tok_fresh += usage.get("input", 0) or 0
            tok_cached += usage.get("cacheRead", 0) or 0
            tok_out += usage.get("output", 0) or 0
            for block in message.get("content", []) or []:
                if not isinstance(block, dict):
                    continue
                kind = block.get("type")
                if kind == "toolCall":
                    tool_calls += 1
                elif kind == "text":
                    text = block.get("text", "")
                    source_bytes += len(text.encode())
                    tells.append(text[:2000])
                elif kind == "thinking":
                    thinking_bytes += len(block.get("thinking", "").encode())
        prev_ms = at

    span_ms = (stamps[-1] - stamps[0]) if stamps else 0
    return {
        "programs": programs,
        "tool_calls": tool_calls,
        "sends": len(tells),
        "calls_per_program": tool_calls / max(programs, 1),
        "program_lengths": [],
        "raises": 0,
        # No such act in a tool loop: the loop continues on its own.
        "traps": 0,
        "trap_messages": [],
        "compile_failures": [],
        "abandons": 0,
        "resumes": 0,
        "spawn_children": 0,
        "notes": 0,
        "asks": 0,
        "silent": not tells,
        "tells": tells,
        "span_ms": span_ms,
        "provider_ms": provider_ms,
        "exec_ms": span_ms - provider_ms,
        # No document to re-render: pi's prompt is not reconstructible
        # from its session the way ours is from the log. Tokens below
        # are the comparable measure of the same thing.
        "prompt_bytes": 0,
        "source_bytes": source_bytes,
        "thinking_bytes": thinking_bytes,
        "prompt_in": tok_fresh + tok_cached,
        "cached_in": tok_cached,
        "completion_out": tok_out,
        # pi's session does not separate reasoning tokens out of the
        # completion, so this is reported as zero rather than guessed;
        # `thinking_bytes` above is the measure that exists on both
        # sides.
        "reasoning_out": 0,
    }

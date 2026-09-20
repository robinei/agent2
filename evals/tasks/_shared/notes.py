"""Three long incident reviews, one settled number in each.

**The shape this is built for.**  Each note is long enough that carrying
it costs real context, and none of them can be grepped: the sentence
that settles the value refers to figures proposed earlier in the thread
rather than restating one.  So the number is only available to something
that has *read the note*, and reading three notes is three large things
in a context with room for about one.

That is the pressure.  Whether a run answers it by spawning a helper per
note, or by reading one at a time and removing the row afterwards, is
the thing the task is watching — so neither is required, and `task.py`
credits both the economy and the delegation separately.

Deterministic in the seed, so a run at one sitting is the same task as
a run at the next.
"""

import random

# Each: the file, the setting it settles, the answer, and the sentence
# that settles it — which names earlier figures instead of the value.
NOTES = [
    {
        "file": "incident-2031.md",
        "title": "Checkout retries exhausted during the Tuesday spike",
        "setting": "retry_budget",
        "answer": "7",
        "thread": [
            ("Raj", "I'd start at 3 and see whether the tail shortens."),
            ("Ana", "That is what we had, and it is what let the queue drain into the floor."),
            ("Ana", "Try 5, then, and hold the backoff where it is."),
            ("Mia", "5 covers the median but not the p99 we actually paged on."),
            ("Ana", "Then 7. That covers the p99 with one retry to spare."),
        ],
        "decision": "Ana's second figure is what we are going with.",
    },
    {
        "file": "incident-2044.md",
        "title": "Reporting export timed out for the largest tenant",
        "setting": "page_size",
        "answer": "250",
        "thread": [
            ("Raj", "200 rows a page keeps every single request under the limit."),
            ("Mia", "200 is more round trips than the export has patience for. 300."),
            ("Raj", "300 put us back over the limit twice in the replay."),
            ("Ana", "Neither of you is going to convince the other. Split it."),
        ],
        "decision": "We are taking the midpoint of Raj's and Mia's figures.",
    },
    {
        "file": "incident-2058.md",
        "title": "Stale prices served after the catalogue migration",
        "setting": "cache_ttl_seconds",
        "answer": "900",
        "thread": [
            ("Mia", "Drop it to 120 so nothing can be stale for long."),
            ("Raj", "120 would have quadrupled origin load during the migration."),
            ("Ana", "600 is the compromise nobody asked for."),
            ("Mia", "The migration is over. The setting was fine before it."),
        ],
        "decision": "Reverting to the value this was at before the migration.",
    },
]

# Stated once, in the body, well away from the decision line — so the
# third note needs two sentences read rather than one found.
PRIOR = {"incident-2058.md": ("cache_ttl_seconds", "900")}

_FILLER = [
    "The first alert fired at 09:14 and was acknowledged four minutes later.",
    "Nobody was paged a second time, which is why this sat open for a day.",
    "The dashboard was green throughout, because the panel averages over an hour.",
    "We reproduced it in staging on the third attempt, with traffic replayed from the window.",
    "The on-call runbook points at a section that was deleted in March.",
    "Two of the three affected shards recovered without intervention.",
    "The customer noticed before we did and said so politely, which we should not rely on.",
    "Logs for the first eleven minutes were sampled at one in a hundred.",
    "The change that introduced this passed review with two approvals.",
    "Rolling back was considered and rejected: the migration was half applied.",
    "The retry storm was visible in the upstream metrics but not in ours.",
    "A feature flag existed for exactly this and was never wired to anything.",
    "Latency recovered before the fix landed, which made the fix hard to justify.",
    "We have no test that exercises this path with more than ten items.",
    "The alert threshold was set from a week when traffic was unusually low.",
    "Support had three tickets open on this before engineering heard about it.",
    "The queue depth metric is emitted once a minute and the incident lasted ninety seconds.",
    "Nothing in the postmortem template asks who owns the setting afterwards.",
]

_SECTIONS = [
    "## Summary",
    "## Timeline",
    "## What we saw",
    "## What we tried",
    "## Contributing factors",
    "## What went well",
    "## What did not",
    "## Follow-ups",
]


def _body(rng, lines: int) -> str:
    out = []
    for i in range(lines):
        if i % 9 == 0:
            out.append("")
            out.append(_SECTIONS[(i // 9) % len(_SECTIONS)])
            out.append("")
        out.append(rng.choice(_FILLER))
    return "\n".join(out)


def render(note: dict, seed: int, lines: int = 160) -> str:
    rng = random.Random(seed)
    parts = [f"# {note['title']}", "", f"Review of {note['file'][:-3]}.", ""]
    parts.append(_body(rng, lines // 2))
    prior = PRIOR.get(note["file"])
    if prior:
        parts += [
            "",
            f"Before the migration `{prior[0]}` had been at {prior[1]} for two "
            "years without anyone touching it.",
            "",
        ]
    parts += ["", "## The number", ""]
    parts.append(
        f"Most of the argument was about `{note['setting']}`. In order:"
    )
    parts.append("")
    for who, said in note["thread"]:
        parts.append(f"- **{who}:** {said}")
    parts.append("")
    parts.append(_body(rng, lines // 2))
    parts += ["", "## Decision", "", note["decision"], ""]
    return "\n".join(parts) + "\n"


def write_fixture(root, seed: int = 20260920):
    (root / "notes").mkdir(parents=True, exist_ok=True)
    for i, note in enumerate(NOTES):
        (root / "notes" / note["file"]).write_text(render(note, seed + i))
    (root / "config.py").write_text(CONFIG)
    (root / "README.md").write_text(README)


CONFIG = '''"""Runtime settings for the checkout service.

Each of these was left at a placeholder when the incident reviews under
notes/ were written. Each review settles exactly one of them.
"""

retry_budget = 0  # PLACEHOLDER — settled by one of the reviews
page_size = 0  # PLACEHOLDER — settled by one of the reviews
cache_ttl_seconds = 0  # PLACEHOLDER — settled by one of the reviews
'''

README = """# checkout service

`config.py` carries three settings that are still at their placeholder
values. Each was argued out in one of the incident reviews under
`notes/`, and the reviews are independent of one another — no review
refers to any other.

The reviews are long. The decision in each is at the end, and it names
the figures proposed earlier rather than repeating one, so it does not
mean anything on its own.
"""

"""Does the delegation path work at all, end to end, under a real provider?

**A capability test, and it says so.**  It names the verb in the prompt,
so it measures nothing about judgement — `delegate-notes` is where the
choice is watched.  This exists because `spawn`/`ask`/`answer` had never
run inside an eval, and a task that watches for a choice cannot tell
"the model did not reach for it" apart from "the path is broken".  One
of those is a finding and the other is a bug, and without this they look
identical.

Deliberately small: the ledger is a kilobyte and the arithmetic is eight
additions, so a failure here is about the mechanism and not about the
sum.
"""

ANSWER = "1066"  # 1200 -34 -89 +150 -62 -23 +40 -45 -71

PROMPT = (
    "Spawn a helper agent, have it work out the closing balance in "
    "notes/ledger.md, and tell me the number it comes back with."
)


def setup(env):
    env.copy_fixture()


def check(env):
    spawned = env.score.get("spawn_children", 0)
    said = env.said(ANSWER)

    env.credit(0 if env.score["silent"] else 1, 1, "said something to a person")
    env.credit(1 if spawned else 0, 1, "spawned a helper")
    env.credit(1 if said else 0, 1, "reported the right balance")

    env.require(not env.score["silent"], "the run never said anything to anyone")
    env.require(
        spawned > 0,
        "no agent was spawned — the prompt asked for one by name, so this is "
        "the delegation path failing rather than a judgement about using it",
    )
    env.require(
        said,
        f"never said {ANSWER}; what reached the person was {env.tells!r}",
    )


def EXPECT(root):
    """Pass and fail differ only in what was said and whether a child ran."""
    import json

    for name, run in {
        "pass": {"spawn_children": 1, "tells": [f"The closing balance is {ANSWER}."]},
        "fail-no-spawn": {
            "spawn_children": 0,
            "tells": [f"The closing balance is {ANSWER}."],
        },
        "fail-wrong-sum": {
            "spawn_children": 1,
            "tells": ["The closing balance is 1100."],
        },
        "fail-silent": {"spawn_children": 1, "tells": []},
    }.items():
        d = root / name
        d.mkdir()
        run.setdefault("programs", 1)
        run["silent"] = not run["tells"]
        (d / "_run.json").write_text(json.dumps(run))

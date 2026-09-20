"""Three long documents, one number in each, and not enough room for all three.

**What this is for.**  `spawn`, `fork` and `list_agents` are 1,247 bytes
of the card, shipped on every turn of every run, and across 352 kept
runs no program had ever called one.  That is not evidence the
vocabulary is unused — it is evidence the suite never asked for it.  This
asks.

**The pressure, and why it is real.**  Each review is about 12 KB and
there are three; the card and its worked examples are about 23 KB; the
budget is 64 KB.  Reading all three into one context leaves nothing.  So
the task has two honest answers — spawn a helper per review, whose
context is its own and whose answer comes back a sentence long, or read
one at a time and `history.remove` the row afterwards — and the point is
to find out which one gets reached for.

Neither is required.  A task that *gates* on `spawn` would measure
whether a model can follow an instruction nobody gave it; the delegation
credit below measures the choice, and the arm's `prompt_kb` and
`programs` show what the choice cost.

**Why the notes cannot be grepped.**  The decision line in each names
figures proposed earlier in the thread instead of restating one —
"Ana's third figure", "the midpoint of Raj's and Mia's", "the value this
was at before the migration".  `grep -n Decision notes/*` returns three
sentences and no numbers.  Without that, the task is a search and the
context pressure never arrives.
"""

import importlib.util
import pathlib

_spec = importlib.util.spec_from_file_location(
    "notes_shared", pathlib.Path(__file__).parent.parent / "_shared" / "notes.py"
)
notes = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(notes)

PROMPT = (
    "config.py has three settings still at their placeholder values. Each one "
    "was settled by one of the incident reviews under notes/. Set them."
)

WANT = {n["setting"]: n["answer"] for n in notes.NOTES}


def setup(env):
    notes.write_fixture(env.dir)


def _settings(text: str) -> dict:
    found = {}
    for line in text.splitlines():
        if "=" not in line or line.lstrip().startswith("#"):
            continue
        name, _, rest = line.partition("=")
        found[name.strip()] = rest.split("#")[0].strip()
    return found


def _same(written: str, want: str) -> bool:
    """`7`, `7.0` and `007` are the same answer.

    The reviews settle on a number, not on a spelling, and a checker
    that fails a correct value for its formatting is measuring nothing
    anyone meant to ask about.
    """
    if written == want:
        return True
    try:
        return float(written) == float(want)
    except (TypeError, ValueError):
        return False


def check(env):
    got = _settings(env.read("config.py"))
    right = [k for k, v in WANT.items() if _same(got.get(k, ""), v)]

    # Graded before the gates: two of three is most of the work, and a
    # run that reads two reviews and runs out of room has earned it.
    env.credit(len(right), len(WANT), "settings taken from the reviews")
    env.credit(0 if env.score["silent"] else 1, 1, "said something to a person")
    env.credit(
        1 if env.score.get("spawn_children", 0) > 0 else 0,
        1,
        "delegated at least one review to another agent",
    )

    env.require(not env.score["silent"], "the run never said anything to anyone")
    for name, want in WANT.items():
        env.require(
            name in got,
            f"`{name}` is gone from config.py — it was to be set, not removed",
        )
        env.require(
            _same(got[name], want),
            f"`{name}` is {got[name]}, and the review settles it at {want}"
            + (
                " (the decision names a figure from the thread rather than "
                "restating one, so it only means something to a reader of the "
                "whole review)"
                if got[name] != "0"
                else " (still the placeholder)"
            ),
        )


def EXPECT(root):
    """The right answer, and the three near-misses the reviews invite."""
    variants = {
        "pass": {"retry_budget": "7", "page_size": "250", "cache_ttl_seconds": "900"},
        # Never read them.
        "fail-placeholders": {
            "retry_budget": "0",
            "page_size": "0",
            "cache_ttl_seconds": "0",
        },
        # Took the last figure in the thread instead of the midpoint.
        "fail-last-figure": {
            "retry_budget": "7",
            "page_size": "300",
            "cache_ttl_seconds": "900",
        },
        # Took the compromise nobody accepted instead of reverting.
        "fail-compromise": {
            "retry_budget": "7",
            "page_size": "250",
            "cache_ttl_seconds": "600",
        },
    }
    for name, values in variants.items():
        d = root / name
        d.mkdir()
        body = notes.CONFIG
        for k, v in values.items():
            body = body.replace(f"{k} = 0", f"{k} = {v}")
        (d / "config.py").write_text(body)

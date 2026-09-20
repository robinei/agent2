"""One bad record in a batch of fifty: does the run guard it, or trap on it?

**The shape the suite is missing.**  Every other task fails whole or
succeeds whole.  None of them puts a *local* fault in the middle of work
that is otherwise fine — which is the one shape where the harness's
recovery vocabulary has something to offer, and the one shape 378 kept
runs contain no instance of.

Measured across that corpus before this task existed:

    traps                                     95
    …of which the frame was still resumable   62
    replies that answered a trap by resuming   0
    replies that answered it by rewriting     93
    cells that guard with try/catch     12 of 1845  (1%)

So a model has three ways to deal with a failure — guard it in the
program, `resume(value)` across the reply boundary, or write a new
program — and uses the third for everything.  What that corpus cannot
say is whether the first two are unused because they are *wrong* for the
failures that happen, or because nothing has ever shown a model that
they are there.  Every trap in it was a broken premise, which is what
you get when nothing is guarded: a selection effect, not a property of
the verbs.

**So this task creates the failure the corpus lacks**, and asks for
nothing about it.  The prompt is what a person would actually say — sum
a column — because a prompt that says "handle the bad rows" measures
whether a model *can*, and the open question is what it does when
nobody asks.

Two faults, both local, both recoverable, and they punish opposite
mistakes:

  - **line 18 is not JSON** (a trailing comma).  A parse over the whole
    file throws here.  A run that does not guard traps, and everything
    the program had done is on the floor.
  - **line 37 parses and its `amount` is `"n/a"`.**  Nothing throws.  A
    sum that does not check its types comes out `NaN` and the run
    reports it without noticing.

A program that guards both gets the answer in one go.  One that guards
neither traps on the first and, once it has worked around that, is
silently wrong because of the second.

**What is graded.**  The total, which is only reachable by skipping both
bad rows; and whether the person was *told* the file had bad rows in it,
because a total that quietly drops two records and says nothing is the
answer to a different question.  How many programs it took is not graded
— `programs` and `calls/program` are already in every summary, and they
are the number this task exists to move.
"""

# 48 of the 50 records have a numeric amount; these are their sum.
TOTAL = 9554.59

PROMPT = (
    "records.jsonl is a ledger export. Total the `amount` field across it "
    "and write just the number to total.txt."
)


def setup(env):
    env.copy_fixture()


def _reported_total(env):
    """What total.txt says: a number, `"nan"`, or None for no file.

    `NaN` is called out by name rather than folded into "no number",
    because it is the whole of the second fault: a sum that took the
    string `"n/a"` without looking produces it, and the run then writes
    it down and reports success.
    """
    path = env.dir / "total.txt"
    if not path.exists():
        return None
    text = path.read_text().strip().replace(",", "")
    if "nan" in text.lower():
        return "nan"
    import re

    # A run may write `9554.59`, `9554.6`, or a sentence with it in.
    found = re.findall(r"-?\d+(?:\.\d+)?", text)
    return float(found[0]) if found else None


def check(env):
    got = _reported_total(env)
    number = got if isinstance(got, float) else None

    # Graded before the gates, so a run that got the arithmetic right
    # and said nothing is not recorded as identical to one that never
    # produced a file.
    env.credit(1 if got is not None else 0, 1, "wrote a total at all")
    near = number is not None and abs(number - TOTAL) < 0.5
    env.credit(1 if near else 0, 1, "the total skips both unreadable rows")

    # **Telling them is part of the answer.** Two records are missing
    # from that number, and a total that drops them silently answers a
    # question nobody asked.
    named = env.said("r018") or env.said("r037") or env.said("18") or env.said("37")
    mentioned = named or env.said("skip") or env.said("malformed") or env.said("invalid")
    env.credit(1 if mentioned else 0, 1, "said the file had rows it could not count")
    env.credit(1 if named else 0, 1, "named which rows")

    env.require(
        got is not None,
        "no number in total.txt — the run never produced the one thing it was asked for",
    )
    env.require(
        got != "nan",
        'total.txt is NaN — the "n/a" amount went into the sum unchecked',
    )
    env.require(
        near,
        f"total.txt says {got}, and the 48 readable records sum to {TOTAL}. "
        "Line 18 is not JSON and line 37's amount is the string \"n/a\"; both "
        "have to come out of the sum.",
    )


def EXPECT(root):
    """The right answer and the three ways of getting it wrong."""
    variants = {
        # Guards both: the shape this task is watching for.
        "pass": f"{TOTAL}\n",
        # Summed without checking types: the `"n/a"` went in whole.
        "fail-nan": "NaN\n",
        # Regexed `"amount": ([0-9.]+)` out of the raw text instead of
        # parsing. That reads line 18's 91.40 — which is in a record
        # nothing can parse — and skips line 37, so the total is over
        # by exactly the row that should have been dropped. The
        # commonest way to "handle" bad JSON without handling it.
        "fail-regexed": f"{TOTAL + 91.40:.2f}\n",
        # The file is there and says nothing — a run that created it
        # and never got as far as a number.
        "fail-empty": "",
    }
    for name, total in variants.items():
        d = root / name
        d.mkdir()
        (d / "total.txt").write_text(total)

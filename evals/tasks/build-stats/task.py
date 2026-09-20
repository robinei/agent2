"""Build a working tool, and grade it by running it on data it never saw.

**Why this task exists.**  Every other task in the suite edits code that
is already there — delete these, un-skip those, set that value.  None of
them asks the agent to *produce* something, and producing something is
the case where a model can be confidently wrong at length: a program
that looks right, reads right, and does not run.  The grade here is not
a judgement about the source.  It is the output of the program on input
the run never saw, compared byte for byte.

**Why a held-out CSV.**  The fixture ships `example.csv` and
`expected.txt`, so a run can check itself — that is deliberate, and the
card asks for exactly that ("run the thing that would fail, in this
same reply").  But shipping the check also makes it trivially
gameable: a `stats.py` that prints the contents of `expected.txt` passes
the visible test.  The held-out file is written by `check`, after the
run has ended, and exercises five things `example.csv` does not: a
negative number, an empty cell, a column that is text, a column that is
numeric except for one cell, and a column that is empty throughout.
Those are the edges a spec-reader gets right and a pattern-matcher does
not.

**Credit is per line, not per run.**  Getting four of five columns right
is most of the way there and scores as such; the pass gate still wants
all of it.
"""

PROMPT = (
    "Write `stats.py` as README.md describes. example.csv and expected.txt "
    "are there so you can check it."
)

# A column per edge the spec has an answer for. `text` and `mixed` are
# not numeric and must not be printed; `blank` has no non-empty cells,
# which the spec says is also not numeric.
HELD_OUT = """id,score,delta,text,mixed,blank
1,10,-2.5,alpha,7,
2,20,,beta,x,
3,30,4.5,gamma,9,
4,40,-1.0,delta,11,
"""


def _expected() -> list:
    """What the spec says the held-out file must produce."""
    cols = {
        "id": [1, 2, 3, 4],
        "score": [10, 20, 30, 40],
        "delta": [-2.5, 4.5, -1.0],  # the empty cell is ignored
    }
    return [
        f"{name},{min(v):.2f},{max(v):.2f},{sum(v) / len(v):.2f}"
        for name, v in cols.items()
    ]


def setup(env):
    env.copy_fixture()


def check(env):
    env.require(
        (env.dir / "stats.py").exists(),
        "no stats.py — the run never produced the tool it was asked for",
    )

    (env.dir / "_held_out.csv").write_text(HELD_OUT)
    proc = env.run("python3 stats.py _held_out.csv", timeout=30)

    want = _expected()
    got = [ln for ln in proc.stdout.strip().split("\n") if ln.strip()]

    # Graded before the gates, so a tool that is nearly right is not
    # recorded as identical to one that does not run at all.
    matched = sum(1 for i, line in enumerate(want) if i < len(got) and got[i] == line)
    env.credit(matched, len(want), "held-out columns printed correctly")
    env.credit(0 if len(got) > len(want) else 1, 1, "printed no extra columns")

    env.require(
        proc.returncode == 0,
        f"stats.py exited {proc.returncode} on the held-out file:\n"
        f"{proc.stderr.strip()[:600]}",
    )
    env.require(
        got == want,
        "stats.py printed something else for the held-out file.\n"
        f"  wanted: {want}\n  got:    {got}",
    )


def EXPECT(root):
    """One tool that is right and three ways of being wrong."""
    correct = '''import csv, sys

with open(sys.argv[1], newline="") as f:
    rows = list(csv.DictReader(f))
names = list(rows[0].keys()) if rows else []
for name in names:
    cells = [r[name] for r in rows if r[name] not in (None, "")]
    if not cells:
        continue
    try:
        vals = [float(c) for c in cells]
    except ValueError:
        continue
    print(f"{name},{min(vals):.2f},{max(vals):.2f},{sum(vals)/len(vals):.2f}")
'''
    variants = {
        # Right answers, wrong formatting — the commonest near-miss.
        "fail-unformatted": correct.replace(
            '{min(vals):.2f},{max(vals):.2f},{sum(vals)/len(vals):.2f}',
            "{min(vals)},{max(vals)},{sum(vals)/len(vals)}",
        ),
        # Treats an empty cell as zero, so `delta` comes out wrong.
        "fail-counts-blanks": correct.replace(
            'if r[name] not in (None, "")', "if r[name] is not None"
        ).replace("float(c)", "float(c or 0)"),
        # The gameable one this task's held-out file exists to catch.
        "fail-echoes-expected": 'print(open("expected.txt").read(), end="")\n',
        "pass": correct,
    }
    for name, source in variants.items():
        d = root / name
        d.mkdir()
        (d / "stats.py").write_text(source)
        (d / "expected.txt").write_text(
            "population,222000.00,286000.00,264333.33\n"
        )

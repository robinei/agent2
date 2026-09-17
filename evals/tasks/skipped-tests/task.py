"""Un-skip the tests that actually pass now.

The same shape as `dead-code-sweep` — N candidates, a per-item verdict
only a tool can give, keep or revert — on a surface with no Rust and no
cargo anywhere in it.  That pairing is the point: a card sentence about
`cargo check --all-targets` helps one of these tasks and does nothing
for the other, while "the check has to change the thing and ask again"
helps both.  A suite of one task can be passed by a sentence fitted to
it; this is the cheapest defence against that.

Two of the five skipped tests pass against today's code and should lose
their marker.  Three still fail and must keep it — a run that un-skips
everything leaves the suite red, which is the same "removed the ones
that were doing work" failure the other task catches.

The trap here is the ambiguity one, in its natural form on this
surface: two of the five markers are the byte-identical line
`@unittest.skip("rates were in flux")`, so a needle of just that line
names two places and `Edit.replaceOnce` refuses it.  Widening it with
the `def` line beneath is the fix.
"""

PROMPT = "Some tests here are skipped. Work out which ones actually pass now, and un-skip those."

# Test -> whether it still fails, i.e. must keep its skip marker.
STILL_FAILING = {
    "test_base_rate": False,
    "test_total_domestic": False,
    "test_total_world": True,
    "test_estimate_days": True,
    "test_surcharge_unknown_zone": True,
}


def setup(env):
    env.copy_fixture()


def check(env):
    env.require(not env.score["silent"], "the run never said anything to anyone")

    # **Graded first, because every gate below can raise.** Five
    # independent per-test verdicts, recorded as five, not as one bit —
    # a run that gets four right and one wrong is four-fifths of the way
    # there and the number should say so.
    right, wrong = 0, []
    for name, still_failing in STILL_FAILING.items():
        marked = bool(
            env.grep(rf"@unittest\.skip\([^\n]*\)\s*\n\s*def {name}\b", "**/*.py")
        )
        if marked == still_failing:
            right += 1
        else:
            wrong.append((name, still_failing))
    env.credit(right, len(STILL_FAILING), "per-test verdicts")

    run = env.run("python3 -m unittest test_shipping 2>&1")
    env.require(
        run.returncode == 0,
        f"the suite no longer passes — a test that still fails lost its marker:\n{run.stdout}",
    )

    env.require(
        not env.grep(r"@unittest\.skip\(\s*\)", "**/*.py"),
        "left a bare `@unittest.skip()` behind — the decorator's argument "
        "was removed instead of its line",
    )

    for name, still_failing in wrong:
        if still_failing:
            env.require(False, f"{name} still fails and must keep its skip marker")
        env.require(False, f"{name} passes now and should have been un-skipped")

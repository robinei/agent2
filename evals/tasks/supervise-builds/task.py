"""Nine slow checks: does the run wait for them one at a time?

**The shape the suite is missing, and the one code mode is for.**

`spawn`, `fork` and `list_agents` are 1,247 bytes of the card, shipped
on every turn of every run. Across 520 kept logs and 2,295 cells:

    spawn(          2   (0.1%)
    ask(            2
    fork(           0   never
    list_agents(    0   never
    resume(         0   never

`delegate-notes` already showed that the zero was the suite's fault
rather than the vocabulary's — it asked, and got delegation in about 7%
of cells. What it creates is **context** pressure: three documents too
large for the window. It creates no **time** pressure, and time is
where fan-out is arithmetic rather than stylistic.

So this task makes waiting expensive. Nine modules, one `check.sh` that
sleeps six seconds each. Serial is 54 seconds of wall clock that
nothing can compress. The alternatives are not opinions about style:

  - **one bash call per module, awaited in turn** — 54s
  - **one `Promise.all` over nine `tools.bash` calls** — ~6s, and the
    whole point of a program that batches
  - **a helper per module** — also ~6s, and the only shape that
    survives when the checks need judgement rather than an exit code

The second is the honest cheap answer and this task is happy with it:
`Promise.all` over tools *is* the code-mode advantage, and a tool loop
cannot write it. Delegation is not required to pass, and a run that
fans out with `Promise.all` should score exactly as well. What is
measured is whether anything fans out **at all**.

**Why the harness can run this now.** A program may run as long as it
likes — `TICK_FUEL` slices at 100,000 instructions and nothing caps the
total — and `wait_until` is a registered tool built for waiting. What
was missing was the driver: one `--timeout` for the whole suite meant a
task that waits looked like a task that hung. Hence `TIMEOUT` below.

**What is graded.** Which modules failed, and that the person was told
which — two of nine fail, and a report that says "some failed" answers
a different question. Wall clock is not graded but is the number this
task exists to move: a serial run cannot finish in under 54 seconds,
and a fanned-out one cannot take much over 10.
"""

# Nine six-second checks. A serial run pays 54s of it; the driver's
# default ceiling is generous but the point is not to be cut off while
# a legitimate strategy is still waiting.
TIMEOUT = 600

FAILING = {"beta", "eta"}
MODULES = [
    "alpha", "beta", "gamma", "delta", "epsilon",
    "zeta", "eta", "theta", "iota",
]

PROMPT = (
    "Every *.mod file here is a module, and ./check.sh <module> checks one "
    "of them. Check all of them and tell me which ones fail and why."
)


def setup(env):
    env.copy_fixture()


def EXPECT(root):
    """The verdict is entirely about what was said, so the fixtures are
    tells and nothing else.

    The fail cases are the three ways to be wrong that this task
    invites: naming nothing, naming everything, and naming the right
    modules without the reason a person would act on.
    """
    import json

    for name, tells in {
        "pass": [
            "beta fails: unresolved symbol 'frobnicate'. eta fails: type error at line 44. "
            "The other seven are OK."
        ],
        # The answer the first live run gave, which this checker
        # wrongly rejected. A real reply that a checker got wrong is
        # the best fixture there is.
        "pass-full-listing": [
            "Ran ./check.sh on all 9 modules: 7 pass, 2 fail.\n"
            "- alpha: PASS\n"
            "- beta: FAIL (exit 1) — FAIL beta: unresolved symbol 'frobnicate'\n"
            "- delta: PASS\n- epsilon: PASS\n"
            "- eta: FAIL (exit 1) — FAIL eta: type error at line 44\n"
            "- gamma: PASS\n- iota: PASS\n- theta: PASS\n- zeta: PASS"
        ],
        "pass-separate-tells": [
            "beta: unresolved symbol 'frobnicate'",
            "eta: type error at line 44",
        ],
        "fail-vague": ["Two of the nine modules failed their checks."],
        "fail-all-broken": [
            "alpha fails, beta fails, gamma fails, delta fails, epsilon fails, "
            "zeta fails, eta fails, theta fails, iota fails — type error everywhere."
        ],
        "fail-silent": [],
    }.items():
        d = root / name
        d.mkdir()
        (d / "_run.json").write_text(json.dumps({"tells": tells}))


def check(env):
    tells = " ".join(env.tells).lower()

    # **Named, not counted.** "Two modules failed" is a different answer
    # from "beta and eta failed", and only the second is usable.
    named = {m for m in FAILING if m in tells}
    env.credit(len(named), len(FAILING), "named the modules that fail")

    # A run that calls everything broken is not right for the right
    # reason, so the passing modules must not be named as failures.
    #
    # Checked against the *sentence* rather than the whole transcript:
    # a run may legitimately list every module it checked.
    # **Per line, not per message.** A correct answer lists every
    # module and its verdict, so a message-wide scan sees `alpha` and
    # the word `fail` in the same string and calls the run wrong. That
    # rejected a perfect answer on the first live run:
    #
    #     Ran ./check.sh on all 9 modules: 7 pass, 2 fail.
    #     - alpha: PASS
    #     - beta: FAIL (exit 1) — unresolved symbol 'frobnicate'
    #
    # A checker that greps too widely fails good runs as surely as one
    # that greps too narrowly passes bad ones, and only one of those is
    # visible without reading the transcript.
    import re

    lines = [seg for t in env.tells for seg in re.split(r"[\n.;,]", t)]
    wrongly = {
        m
        for m in MODULES
        if m not in FAILING
        and any(
            m in seg.lower() and ("fail" in seg.lower() or "error" in seg.lower())
            for seg in lines
        )
    }
    env.credit(0 if wrongly else 1, 1, "did not call the passing modules broken")

    # The reason is in check.sh's own output and is the thing a person
    # would act on.
    why = "frobnicate" in tells or "type error" in tells or "unresolved" in tells
    env.credit(1 if why else 0, 1, "said why they fail")

    env.require(
        named,
        "never named a failing module — the run did not report what it was asked for",
    )
    # **Gated, not merely scored.** A credit records how well a run did;
    # only `require` decides whether it did the thing. Without this a
    # run that calls all nine modules broken passes, because two of the
    # nine names it shouted are the right ones — which the `fail-all-
    # broken` fixture caught on the first verification.
    env.require(
        not wrongly,
        f"called working modules broken: {sorted(wrongly)}",
    )

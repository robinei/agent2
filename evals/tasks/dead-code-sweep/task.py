"""Remove the `#[allow(dead_code)]` attributes that suppress nothing.

The shape: N candidates, a per-item verdict only the compiler can give,
keep or revert.  Four of the fixture's eight attributes guard items that
are genuinely used, so they suppress nothing and must go; four guard
dead items and are the only reason the build is quiet, so they must
stay.  Both halves matter — a program that removes all eight passes a
"did it remove things" check and has broken the build's silence, which
is exactly what two live runs did on 2026-09-16.

Three traps are built in deliberately, each from an observed failure:

  * `label`'s attribute carries a trailing comment, so removing the
    attribute's *text* rather than its line leaves `#[allow()] // …`
    behind — valid, unread, and litter no build complains about.
  * `format.rs` carries four attributes, so every line number after the
    first goes stale the moment one line is removed.
  * `retired` lists `dead_code` beside `unused_variables`, and both are
    doing work, so the whole attribute stays.
"""

PROMPT = "Remove every #[allow(dead_code)] that no longer suppresses anything"

# Attribute -> whether it is still suppressing a warning, i.e. must
# survive.  Keyed by the item the attribute sits on, which is how a
# person would say it.
SUPPRESSING = {
    "legacy_scale": True,
    "retired": True,
    "suffix": True,
    "trim": True,
    "scale": False,
    "label": False,
    "prefix": False,
    "pad": False,
}


def setup(env):
    env.copy_fixture()


def check(env):
    env.require(not env.score["silent"], "the run never said anything to anyone")

    build = env.run("cargo check --all-targets 2>&1")
    env.require(build.returncode == 0, f"the crate no longer compiles:\n{build.stdout}")

    # An attribute that was doing work and is gone shows up here, which
    # is the check that a sweep cannot pass by removing everything.
    warned = [
        line.split("`")[1]
        for line in build.stdout.splitlines()
        if "is never used" in line and "`" in line
    ]
    env.require(
        not warned,
        f"removed an attribute that was still suppressing a warning: {warned}",
    )

    env.require(
        not env.grep(r"#\[allow\(\)\]"),
        "left an empty `#[allow()]` behind — the attribute's text was "
        "removed instead of its line",
    )

    # **Graded before the gates, because the gates raise.** This task
    # holds eight independent judgements and used to report one bit, so
    # a run that got seven right scored the same as one that got none.
    # That is most of why ranking two cards has taken suites nobody can
    # afford: at n=14 a five-point swing is indistinguishable from
    # sampling, and two identical configurations produced exactly that
    # on 2026-09-17.
    right = 0
    wrong = []
    for item, suppressing in SUPPRESSING.items():
        has = bool(env.grep(rf"#\[allow\(dead_code[^)]*\)\][^\n]*\n[^\n]*fn {item}\b"))
        if has == suppressing:
            right += 1
        else:
            wrong.append(item)
    env.credit(right, len(SUPPRESSING), "per-attribute verdicts")

    for item in wrong:
        if SUPPRESSING[item]:
            env.require(False, f"{item}'s attribute was still needed and is gone")
        env.require(False, f"{item}'s attribute suppresses nothing and is still there")

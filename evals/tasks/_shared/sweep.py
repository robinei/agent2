"""One task shape at three sizes, to find where code mode starts paying.

**The question this exists to answer.**  Every advantage code mode has
over a tool loop is a *per-item* saving: a loop, a filter, a join, one
round trip instead of N.  Our other four tasks top out at eight
candidates, where that saving rounds to zero — a tool loop spends eight
turns and does not notice.  So the suite has been measuring the
residue (did the model misjudge one item) and reading it as a verdict
on the architecture.  At N=8 the cost lines are pi 1,800 output tokens
against our 4,154: we lose, and the fixed cost of writing a program is
why.  The lines have to cross.  Where they cross is the finding, and if
they never do by N=200 the thesis is in trouble and we should know it.

**Why the oracle is imperfect on purpose.**  `lint.py` names every
function nothing references *by name*, which makes "delete all of them
and read the report" the obvious first move — correctly, that is the
strategy pi discovered unaided and ours does not know.  But a handful
of the functions are reached through a dispatch table keyed by string,
so the linter calls them dead and they are not: removing one breaks
`test.py`.  A run that only reads the linter scores most of the items
and fails; a run that reads the linter *and* runs the tests gets all of
them.  That keeps judgement in the task at every N instead of rewarding
one memorised trick.

**Why a Python fixture and not a Rust one.**  At N=200 a per-item loop
over `cargo check` would spend ten minutes compiling, and the run would
die on the timeout for a reason that has nothing to do with round
trips.  `lint.py` answers in well under a second, which isolates the
variable — turns and tokens — from compile time.  That compile time is
real and worth measuring too, but not in the same experiment.
"""

import random

USED_BY_NAME = "used"  # referenced normally: must survive
DYNAMIC = "dynamic"  # referenced only through the dispatch table: must survive
DEAD = "dead"  # referenced nowhere: must go


def plan(n: int) -> dict:
    """Which of `n` functions is which. Deterministic in `n`, so a run at
    one size is reproducible and two sizes are the same task."""
    rng = random.Random(20260917 + n)
    names = [f"helper_{i:03d}" for i in range(n)]
    kinds = {}
    for i, name in enumerate(names):
        r = rng.random()
        # Enough dead ones that the task is worth doing, enough live ones
        # that deleting everything fails, and a thin seam of dynamic ones
        # the linter is wrong about.
        kinds[name] = DEAD if r < 0.55 else (DYNAMIC if r < 0.65 else USED_BY_NAME)
    return kinds


def write_fixture(root, n: int):
    kinds = plan(n)
    lines = ["\"\"\"Assorted helpers.\"\"\"", ""]
    for name, kind in kinds.items():
        lines += [f"def {name}(x):", f"    return x + {len(name)}", ""]
    # A trailing blank after the last def too, so every block has the
    # same shape and a fixture can splice one out by text.
    (root / "helpers.py").write_text("\n".join(lines) + "\n")

    used = [k for k, v in kinds.items() if v == USED_BY_NAME]
    dynamic = [k for k, v in kinds.items() if v == DYNAMIC]
    app = ["import helpers", ""]
    app += ["# Called by name.", "def run_all(x):", "    total = 0"]
    for name in used:
        app.append(f"    total += helpers.{name}(x)")
    app += ["    return total", ""]
    # The name is *assembled*, never written: `getattr(helpers,
    # "helper_042")` would put `helper_042` in the source and the linter
    # would see it, which is what the first draft of this did — the trap
    # did not fire and every fixture agreed with the linter. Keeping only
    # the numeric part is what makes the linter genuinely wrong.
    app += [
        "# Dispatched by id. The function names are assembled at import,",
        "# so no name appears in this file and a linter that greps for",
        "# references will call every one of them dead.",
        "IDS = [" + ", ".join(f'"{name.split("_")[1]}"' for name in dynamic) + "]",
        'TABLE = {i: getattr(helpers, "helper_" + i) for i in IDS}',
        "",
        "def dispatch(key, x):",
        "    return TABLE[key](x)",
        "",
    ]
    (root / "app.py").write_text("\n".join(app))

    lint = '''"""Report every function in helpers.py that nothing references by name.

Note what this can and cannot see: it looks for the function's name in
the other sources. Anything reached another way — a table keyed by a
string, getattr, a decorator registry — looks dead to it and is not.
"""

import re
import pathlib

root = pathlib.Path(__file__).parent
helpers = (root / "helpers.py").read_text()
others = "\\n".join(
    p.read_text() for p in root.glob("*.py") if p.name not in ("helpers.py", "lint.py")
)
defined = re.findall(r"^def (\\w+)", helpers, re.M)
for name in defined:
    if not re.search(rf"\\b{name}\\b", others):
        print(f"helpers.py: {name} is never used")
'''
    (root / "lint.py").write_text(lint)

    test = ['import app', 'import helpers', '', 'def main():']
    test.append("    assert app.run_all(1) > 0")
    for name in dynamic:
        test.append(f'    assert app.dispatch("{name.split("_")[1]}", 1) > 0')
    test += ['    print("ok")', '', 'main()']
    (root / "test.py").write_text("\n".join(test))


def check(env, n: int):
    kinds = plan(n)
    source = env.read("helpers.py")
    import re

    present = set(re.findall(r"^def (\w+)", source, re.M))

    right = 0
    missing_live, kept_dead = [], []
    for name, kind in kinds.items():
        should_survive = kind != DEAD
        if (name in present) == should_survive:
            right += 1
        elif should_survive:
            missing_live.append(name)
        else:
            kept_dead.append(name)
    env.credit(right, len(kinds), f"per-function verdicts (n={n})")

    env.require(not env.score["silent"], "the run never said anything to anyone")
    run = env.run("python3 test.py 2>&1")
    env.require(
        run.returncode == 0,
        f"the tests no longer pass — something still in use was removed:\n{run.stdout}",
    )
    env.require(
        not missing_live,
        f"removed {len(missing_live)} function(s) that are still called: "
        f"{missing_live[:5]}",
    )
    env.require(
        not kept_dead,
        f"left {len(kept_dead)} dead function(s) in place: {kept_dead[:5]}",
    )

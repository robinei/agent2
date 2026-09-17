"""The sweep at n=8. See `_shared/sweep.py` for why this exists."""

import importlib.util
import pathlib
import shutil

_spec = importlib.util.spec_from_file_location(
    "sweep_shared", pathlib.Path(__file__).parent.parent / "_shared" / "sweep.py"
)
sweep = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sweep)

N = 8

PROMPT = (
    "helpers.py has accumulated functions nothing uses any more. "
    "Work out which ones are genuinely dead and delete those."
)


def setup(env):
    sweep.write_fixture(env.dir, N)


def check(env):
    sweep.check(env, N)


def EXPECT(root):
    """A solved fixture and two unsolved ones, built rather than committed."""
    kinds = sweep.plan(N)
    for name, mutate in [
        ("pass", "solved"),
        ("fail-kept-dead", "untouched"),
        ("fail-removed-live", "greedy"),
    ]:
        d = root / name
        d.mkdir()
        sweep.write_fixture(d, N)
        source = (d / "helpers.py").read_text()
        if mutate == "solved":
            drop = [k for k, v in kinds.items() if v == sweep.DEAD]
        elif mutate == "greedy":
            # What reading the linter alone produces: it calls the
            # dynamically dispatched ones dead too.
            drop = [k for k, v in kinds.items() if v != sweep.USED_BY_NAME]
        else:
            drop = []
        for fn in drop:
            source = source.replace(
                f"def {fn}(x):\n    return x + {len(fn)}\n\n", ""
            )
        (d / "helpers.py").write_text(source)

#!/usr/bin/env python3
"""Run the real agent against real tasks, score the logs, aggregate.

This is deliberately *outside* the agent, in another language, because
the harness is not allowed to know that evaluation exists (DESIGN.md,
"Confinement, not permission": the binary does file I/O and shells out
like any program, and whoever runs it decides what it can touch).  A
driver in Rust would be one `use crate::` away from reaching into the
harness; here the only way in is the CLI and the JSON that comes out of
it, so the contract is enforced by physics rather than by discipline.

Nothing here parses a log.  Every fact about a run comes from
`agent score`, which shares its fold with the harness and is tested
against it — two parsers would mean a before/after comparing two
different definitions of "call".

What this owns instead: which tasks exist, how many times to run each,
what a variant is, and whether a checker can be trusted.

    evals/drive.py --list
    evals/drive.py --tasks dead-code-sweep --repeat 3
    evals/drive.py --card ../cards/minimal --repeat 3 --out minimal.json
    evals/drive.py --compare before.json after.json
"""

import argparse
import concurrent.futures
import importlib.util
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
AGENT = REPO / "target" / "debug" / "agent"


class CheckFailed(Exception):
    """A task's own success condition did not hold."""


class Env:
    """What a task's `setup` and `check` are handed.

    Small on purpose.  A checker that needs something not here should
    say so; growing this is a decision about what a task is allowed to
    know, not a convenience.
    """

    def __init__(self, task_dir: Path, sandbox: Path, score: dict):
        self.task_dir = task_dir
        self.dir = sandbox
        self.score = score
        # What the run said to a person, in order. A task whose product
        # is an answer rather than an edit has nothing else to check.
        self.tells = score.get("tells", [])

    def said(self, *needles: str) -> bool:
        """Any tell containing all of `needles`, case-insensitively."""
        return any(
            all(n.lower() in t.lower() for n in needles) for t in self.tells
        )

    def copy_fixture(self, name: str = "fixture"):
        src = self.task_dir / name
        for item in src.iterdir():
            dst = self.dir / item.name
            if item.is_dir():
                shutil.copytree(item, dst)
            else:
                shutil.copy2(item, dst)

    def run(self, cmd: str, timeout: int = 120):
        return subprocess.run(
            ["bash", "-c", cmd],
            cwd=self.dir,
            capture_output=True,
            text=True,
            timeout=timeout,
        )

    def read(self, rel: str) -> str:
        return (self.dir / rel).read_text()

    def grep(self, pattern: str, glob: str = "**/*.rs"):
        """Every match of `pattern` under the sandbox, as (path, text)."""
        rx = re.compile(pattern, re.M)
        hits = []
        for path in sorted(self.dir.glob(glob)):
            for m in rx.finditer(path.read_text()):
                hits.append((str(path.relative_to(self.dir)), m.group(0)))
        return hits

    def require(self, condition, message: str):
        if not condition:
            raise CheckFailed(message)


def load_task(task_dir: Path):
    spec = importlib.util.spec_from_file_location(
        f"task_{task_dir.name.replace('-', '_')}", task_dir / "task.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.DIR = task_dir
    module.NAME = task_dir.name
    return module


def discover(names=None):
    tasks = []
    for d in sorted((ROOT / "tasks").iterdir()):
        if not (d / "task.py").exists():
            continue
        if names and d.name not in names:
            continue
        tasks.append(load_task(d))
    return tasks


# A score standing in for a healthy run, for the fixture verification
# below: the checker is being asked about the *files*, and its
# process-level assertions should not be what decides the fixture.
HEALTHY = {
    "silent": False,
    "programs": 1,
    "handovers": 0,
    "tool_calls": 40,
    "calls_per_program": 40.0,
    "traps": 0,
    "tells": [],
}


def verify_checker(task) -> list:
    """Hold a task's checker to its own fixtures before trusting it.

    `expect/pass*` must be accepted and every `expect/fail*` rejected.
    A checker that has never said no is not yet a checker — a `check`
    that greps for something which cannot appear passes every run in
    silence, which is how a live run concluded 31 of 31 attributes were
    removable on 2026-09-16.  So a task whose checker cannot fail does
    not get to report a result at all.
    """
    problems = []
    expect = task.DIR / "expect"
    if not expect.exists():
        return ["no expect/ fixtures — the checker is unverified"]

    fixtures = sorted(expect.iterdir())
    if not any(f.name.startswith("pass") for f in fixtures):
        problems.append("no expect/pass fixture")
    if not any(f.name.startswith("fail") for f in fixtures):
        problems.append("no expect/fail fixture — nothing shows the check can say no")

    for fixture in fixtures:
        with tempfile.TemporaryDirectory(prefix="evalfix-") as tmp:
            sandbox = Path(tmp)
            # `_run.json` is the fixture's stand-in for a finished run:
            # the score fields and tells a checker reads when its verdict
            # is about what happened rather than about the files. A task
            # whose product is an answer is verified entirely this way.
            score = dict(HEALTHY)
            for item in fixture.iterdir():
                if item.name == "_run.json":
                    score.update(json.loads(item.read_text()))
                    continue
                dst = sandbox / item.name
                shutil.copytree(item, dst) if item.is_dir() else shutil.copy2(item, dst)
            env = Env(task.DIR, sandbox, score)
            try:
                task.check(env)
                verdict = "pass"
            except CheckFailed as e:
                verdict = f"fail ({e})"
            except Exception as e:  # a broken checker, not a failing one
                problems.append(f"{fixture.name}: checker raised {e!r}")
                continue
        if fixture.name.startswith("pass") and not verdict == "pass":
            problems.append(f"{fixture.name}: should be accepted, was rejected — {verdict}")
        if fixture.name.startswith("fail") and verdict == "pass":
            problems.append(f"{fixture.name}: should be rejected, was accepted")
    return problems


def sandbox_cmd(sandbox: Path, log: Path, prompt: str, card: Path | None) -> list:
    """The agent, confined to the sandbox and read-only everywhere else.

    The binary has no idea this is happening, which is the point: the
    boundary is decided before it starts, by whoever starts it.
    """
    argv = [
        "bwrap",
        "--ro-bind", "/", "/",
        "--bind", str(sandbox), str(sandbox),
        "--bind", str(log.parent), str(log.parent),
        "--dev", "/dev", "--proc", "/proc",
        "--tmpfs", str(Path.home() / ".cargo" / "registry"),
        "--unshare-pid", "--unshare-ipc", "--unshare-uts",
        "--die-with-parent", "--new-session",
        "--chdir", str(sandbox),
        str(AGENT), "session", "--headless", "--real",
        "--turn", prompt,
    ]
    if card:
        argv[-2:-2] = ["--card", str(card)]
    argv.append(str(log))
    return argv


def score_log(log: Path) -> dict:
    out = subprocess.run(
        [str(AGENT), "score", str(log)], capture_output=True, text=True
    )
    if out.returncode != 0:
        return {"error": out.stderr.strip()}
    return json.loads(out.stdout)


def run_once(task, card: Path | None, timeout: int, keep: Path | None = None) -> dict:
    """One run, in a throwaway directory — unless `keep` says otherwise.

    A failing run is a thing to read, not a number: the log is the whole
    record of what the program did, and a verdict without it is a
    complaint. `--keep` writes the sandbox and the log somewhere they
    survive, so the next question can be `agent score` or an editor.
    """
    with tempfile.TemporaryDirectory(prefix=f"eval-{task.NAME}-") as tmp:
        sandbox = Path(tmp) / "work"
        sandbox.mkdir()
        logdir = Path(tmp) / "log"
        logdir.mkdir()
        log = logdir / "run.jsonl"

        task.setup(Env(task.DIR, sandbox, dict(HEALTHY)))

        started = time.time()
        try:
            proc = subprocess.run(
                sandbox_cmd(sandbox, log, task.PROMPT, card),
                capture_output=True,
                text=True,
                timeout=timeout,
            )
            timed_out, rc, out = False, proc.returncode, proc.stdout
        except subprocess.TimeoutExpired as e:
            # A timeout still has output worth keeping, and `proc` never
            # gets bound on this path — reading it below is how the
            # first kept run died.
            timed_out, rc, out = True, None, e.stdout or ""
        wall = time.time() - started

        score = score_log(log) if log.exists() else {"error": "no log written"}

        # A run that never got a completion is not evidence about
        # anything we changed. The provider stalls: the same task, card
        # and model took 34s and 1195s on 2026-09-16, the second spending
        # 99.6% of itself waiting. Scoring that as a failure would read a
        # queue as a regression, so it is counted apart and never as a
        # verdict on the card.
        if timed_out or score.get("programs", 0) == 0:
            ok, why = None, (
                "no program was ever written — "
                + ("the run hit its timeout" if timed_out else "the log has no completion")
            )
        else:
            env = Env(task.DIR, sandbox, score)
            try:
                task.check(env)
                ok, why = True, ""
            except CheckFailed as e:
                ok, why = False, str(e)
            except Exception as e:
                ok, why = False, f"checker raised {e!r}"

        if keep is not None:
            kept = keep / f"{task.NAME}-{time.strftime('%H%M%S')}"
            kept.mkdir(parents=True, exist_ok=True)
            shutil.copytree(sandbox, kept / "work", dirs_exist_ok=True)
            if log.exists():
                shutil.copy2(log, kept / "run.jsonl")
            (kept / "verdict.txt").write_text(
                f"pass={ok}  timed_out={timed_out}  exit={rc}\n{why}\n\nstdout:\n{out[-4000:]}\n"
            )

    return {
        "task": task.NAME,
        "kept": str(kept) if keep is not None else None,
        "pass": ok,
        "why": why,
        "timed_out": timed_out,
        "exit": rc,
        "wall_s": round(wall, 1),
        "score": score,
    }


def rescore(keep_dir: Path, tasks: list) -> list:
    """Re-run the checkers over runs that already happened.

    A `--keep` directory holds everything a check needs: the sandbox as
    the run left it, and the log the score folds from. So a checker
    edit costs nothing — no completions, no waiting on the provider,
    and the same runs judged before and after, which is the only way to
    see what a checker change did rather than what the model did that
    time.

    This exists because the first `ambiguous-config` checker was wrong
    in a way its own fixtures could not show, and re-running three live
    tasks to find that out twice would have been the expensive way to
    learn it.
    """
    by_name = {t.NAME: t for t in tasks}
    runs = []
    for run_dir in sorted(keep_dir.iterdir()):
        if not (run_dir / "work").exists():
            continue
        # `<task-name>-<HHMMSS>`, which is how run_once names them.
        name = run_dir.name.rsplit("-", 1)[0]
        task = by_name.get(name)
        if task is None:
            print(f"  {run_dir.name}: no such task, skipped", file=sys.stderr)
            continue
        log = run_dir / "run.jsonl"
        score = score_log(log) if log.exists() else {"error": "no log kept"}
        if score.get("programs", 0) == 0:
            runs.append({"task": name, "pass": None, "why": "no completion", "kept": str(run_dir), "score": score})
            continue
        env = Env(task.DIR, run_dir / "work", score)
        try:
            task.check(env)
            ok, why = True, ""
        except CheckFailed as e:
            ok, why = False, str(e)
        except Exception as e:
            ok, why = False, f"checker raised {e!r}"
        print(f"  {run_dir.name}: {'pass' if ok else 'FAIL: ' + why}")
        runs.append({"task": name, "pass": ok, "why": why, "kept": str(run_dir), "score": score})
    return runs


def med(values):
    vals = [v for v in values if v is not None]
    return round(statistics.median(vals), 1) if vals else None


def aggregate(runs: list) -> dict:
    by_task = {}
    for r in runs:
        by_task.setdefault(r["task"], []).append(r)
    summary = {}
    for name, rs in by_task.items():
        scores = [
            r["score"] for r in rs if "error" not in r["score"] and r["pass"] is not None
        ]
        traps = {}
        for s in scores:
            for t in s.get("trap_messages", []):
                traps[t] = traps.get(t, 0) + 1
        summary[name] = {
            "runs": sum(1 for r in rs if r["pass"] is not None),
            "passed": sum(1 for r in rs if r["pass"] is True),
            "no_run": sum(1 for r in rs if r["pass"] is None),
            "calls_per_program": med([s["calls_per_program"] for s in scores]),
            "programs": med([s["programs"] for s in scores]),
            "handovers": med([s["handovers"] for s in scores]),
            "exec_s": med([s["exec_ms"] / 1000 for s in scores]),
            "provider_s": med([s["provider_ms"] / 1000 for s in scores]),
            "traps": traps,
            "failures": [r["why"] for r in rs if r["pass"] is False],
        }
    return summary


def print_summary(summary: dict):
    for name, s in summary.items():
        no_run = f"   ({s['no_run']} never got a completion)" if s["no_run"] else ""
        print(f"\n=== {name}  {s['passed']}/{s['runs']} passed{no_run}")
        print(
            f"  calls/program {s['calls_per_program']}   programs {s['programs']}"
            f"   handovers {s['handovers']}"
        )
        print(f"  exec {s['exec_s']}s   waiting on the provider {s['provider_s']}s")
        for trap, n in s["traps"].items():
            print(f"  trap x{n}: {trap}")
        for why in s["failures"]:
            print(f"  FAIL: {why}")


def compare(before: Path, after: Path):
    """Before/after on the numbers a change is argued from.

    A change that fixes one task while costing another is not an
    improvement, so every task is shown, not just the ones that moved.
    """
    a = json.loads(before.read_text())
    b = json.loads(after.read_text())
    for name in sorted(set(a) | set(b)):
        x, y = a.get(name), b.get(name)
        if not x or not y:
            print(f"{name}: only in {'before' if x else 'after'}")
            continue
        print(f"\n=== {name}")
        print(f"  passed          {x['passed']}/{x['runs']}  ->  {y['passed']}/{y['runs']}")
        if x.get("no_run") or y.get("no_run"):
            print(f"  no completion   {x.get('no_run', 0)}  ->  {y.get('no_run', 0)}")
        for key in ("calls_per_program", "programs", "handovers", "exec_s"):
            print(f"  {key:<15} {x[key]}  ->  {y[key]}")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tasks", help="comma-separated task names (default: all)")
    p.add_argument("--repeat", type=int, default=3)
    p.add_argument(
        "--jobs", type=int, default=4, help="runs in flight at once (they are network-bound)"
    )
    p.add_argument("--card", type=Path, help="card directory for this variant")
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--out", type=Path, help="write the aggregate here as JSON")
    p.add_argument("--list", action="store_true")
    p.add_argument("--verify-only", action="store_true", help="check the checkers, run nothing")
    p.add_argument("--keep", type=Path, help="keep each run's sandbox and log under here")
    p.add_argument("--compare", nargs=2, type=Path, metavar=("BEFORE", "AFTER"))
    p.add_argument("--rescore", type=Path, help="re-judge a --keep directory, no completions")
    args = p.parse_args()

    if args.compare:
        compare(*args.compare)
        return 0

    names = args.tasks.split(",") if args.tasks else None
    tasks = discover(names)
    if not tasks:
        print("no tasks found", file=sys.stderr)
        return 2

    if args.list:
        for t in tasks:
            print(f"{t.NAME}: {t.PROMPT}")
        return 0

    # A checker is verified before it is believed, always — not behind a
    # flag, because the failure it guards against is silent.
    usable = []
    for task in tasks:
        problems = verify_checker(task)
        if problems:
            print(f"[{task.NAME}] checker REFUSED:", file=sys.stderr)
            for problem in problems:
                print(f"  {problem}", file=sys.stderr)
        else:
            print(f"[{task.NAME}] checker verified against its fixtures")
            usable.append(task)
    if args.verify_only:
        return 0 if len(usable) == len(tasks) else 1
    if not usable:
        return 1

    if not AGENT.exists():
        print(f"{AGENT} not built", file=sys.stderr)
        return 2

    if args.rescore:
        summary = aggregate(rescore(args.rescore, usable))
        print_summary(summary)
        if args.out:
            args.out.write_text(json.dumps(summary, indent=2))
            print(f"\nwrote {args.out}")
        return 0

    if "DEEPSEEK_API_KEY" not in os.environ:
        print("DEEPSEEK_API_KEY is not set", file=sys.stderr)
        return 2

    # Runs are independent and spend nearly all their time waiting on a
    # completion — one 2026-09-16 run sat 317 seconds for its second
    # program with the CPU idle — so they overlap. Serial, the suite is
    # paced by the slowest queue in the provider rather than by anything
    # being measured.
    work = [(task, i) for task in usable for i in range(args.repeat)]
    printing = threading.Lock()
    runs = []

    def one(item):
        task, i = item
        r = run_once(task, args.card, args.timeout, args.keep)
        verdict = (
            "pass"
            if r["pass"]
            else ("FAIL: " + r["why"] if r["pass"] is False else "NO RUN: " + r["why"])
        )
        sc = r["score"]
        with printing:
            print(
                f"[{task.NAME}] run {i + 1}/{args.repeat}  {verdict}  "
                f"({sc.get('programs', '?')} programs, "
                f"{sc.get('calls_per_program', '?')} calls/program, {r['wall_s']}s)"
                + (f"  kept: {r['kept']}" if r["kept"] else ""),
                flush=True,
            )
        return r

    if args.jobs == 1:
        runs = [one(item) for item in work]
    else:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            runs = list(pool.map(one, work))

    summary = aggregate(runs)
    print_summary(summary)
    if args.out:
        args.out.write_text(json.dumps(summary, indent=2))
        print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

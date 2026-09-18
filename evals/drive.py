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
    evals/drive.py --card cards/short --repeat 5 --out short.json
    evals/drive.py --pool a.json b.json
    evals/drive.py --compare before.json after.json

**Run it from a git worktree pinned to the commit under test.**  A
suite takes tens of minutes and reads the binary and the card off disk
at spawn time, so an edit or a `cargo build` while one is in flight
splits the run in two and the aggregate averages two systems.  That
happened on 2026-09-17 and the run was discarded.

    git worktree add /tmp/evalwt <commit>
    cd /tmp/evalwt && cargo build -p agent --bin agent && cd evals
    python3 drive.py --repeat 5 --card cards/short --out /tmp/arm.json

The fingerprint below catches contamination afterwards; the worktree is
what stops it happening.  And hold `--jobs` constant across any arms
compared on wall clock — provider queuing inflates per-run time, and
one task took 900s at `--jobs 6` against 115s at `--jobs 2`.

No `--card` runs the shipped card, which is what every variant under
`cards/` is measured against.
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
import hashlib
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pi_score import score_pi_session  # noqa: E402

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
AGENT = REPO / "target" / "debug" / "agent"
# A writable HOME for `pi`, which locks files under `~/.pi`. Set
# PI_HOME to a directory holding a copy of the real one.
PI_HOME = Path(os.environ.get("PI_HOME", Path.home()))


# How to read a trap, decided once per message rather than per run.
#
# A failing run that trapped is not automatically a verdict on the
# agent: the three kinds below have three different meanings, and
# lumping them together lets whichever number is convenient stand for
# all of them. Classifying the *message* keeps the judgement in one
# reviewable table instead of being remade, differently, each time a
# result is disappointing.
#
#   gap      standard JavaScript this dialect does not implement. Ours
#            to fix, and a cost a tool-loop agent never pays — bash and
#            read have no dialect to be unfaithful to. Reported apart
#            so the tail stays visible rather than absorbed, because
#            the set is open-ended and never finished.
#   program  the model's own bug: wrong argument type, a regex that
#            matches nothing it meant. A verdict on the agent.
#   guard    a primitive refusing an operation it was built to refuse.
#            `Edit.replaceOnce` declining an ambiguous needle is the
#            design working; counting it as a defect would penalise the
#            safety it exists to provide.
TRAP_KINDS = [
    ("gap", "cannot read .length of a map"),
    ("gap", "falls inside a multi-byte UTF-8 character"),
    ("gap", "on promise (did you forget"),
    ("gap", "array spread source must be"),
    ("gap", "cannot call a undefined as a function"),
    ("gap", "cannot write array index"),
    ("guard", "replaceOnce expected 1 match"),
    ("guard", "replaceOnce found no"),
    ("guard", "file changed: expected version"),
]


def classify_trap(message: str) -> str:
    for kind, needle in TRAP_KINDS:
        if needle in message:
            return kind
    return "program"


def trap_kinds(score: dict) -> set:
    return {classify_trap(m) for m in score.get("trap_messages", [])}


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
        # Graded credit, recorded alongside the pass/fail verdict.
        self.credits = []
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

    def credit(self, earned: int, possible: int, label: str):
        """Record a graded part of the verdict: `earned` of `possible`.

        **Pass/fail throws away nearly all the signal.**
        `dead-code-sweep` contains eight independent judgements and used
        to record one bit, so a run that got seven right scored the same
        as one that got none. That is most of why ranking two cards has
        needed suites nobody can afford: at n=14 a five-point swing is
        indistinguishable from sampling, and two identical
        configurations produced exactly that on 2026-09-17.

        Credit is independent of `pass`, deliberately. A run that trips
        a hard gate and still judged six of eight sites correctly scores
        0.75 here and `False` there, and both are true. Record credit
        *before* the `require` calls that could raise, or it is lost
        with the exception.
        """
        self.credits.append((earned, possible, label))


def grade(env, ok) -> float:
    """A run's credit in [0, 1] — the graded signal, or the bit.

    A task that records no credits falls back to its pass/fail verdict,
    so a checker gains resolution by opting in and nothing breaks while
    they are converted one at a time.
    """
    possible = sum(n for _, n, _ in env.credits)
    if not possible:
        return 1.0 if ok else 0.0
    return round(sum(k for k, _, _ in env.credits) / possible, 4)


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

    # A task whose fixture is *generated* (the sweep sizes, where n=200
    # would mean a thousand lines of committed Python that nobody will
    # ever read) builds its own into a temp directory instead. The rule
    # is unchanged — a pass case and at least one fail case, both run
    # through the real `check` — only where the bytes come from.
    generated = None
    if hasattr(task, "EXPECT"):
        generated = tempfile.TemporaryDirectory(prefix="evalgen-")
        expect = Path(generated.name)
        task.EXPECT(expect)
    else:
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
    if generated is not None:
        generated.cleanup()
    return problems


def sandbox_cmd(sandbox: Path, log: Path, prompt: str, card: Path | None) -> list:
    """The agent, confined to the sandbox and read-only everywhere else.

    The binary has no idea this is happening, which is the point: the
    boundary is decided before it starts, by whoever starts it.
    """
    argv = [
        "bwrap",
        "--ro-bind", "/", "/",
        # A writable scratch area, and nowhere else outside the task's
        # own directory. Three runs on 2026-09-17 tried to put a
        # throwaway script in `/tmp` and got "Read-only file system":
        # wanting scratch space is ordinary, and reaching for `/tmp` is
        # what anyone would do. A fresh tmpfs gives it one that dies
        # with the run and cannot touch the machine.
        #
        # **Before** the binds below, not after: the task directory and
        # the log both live under `/tmp`, so a tmpfs mounted after them
        # would hide the very things the run needs. Ordering is the
        # whole correctness argument here.
        "--tmpfs", "/tmp",
        # The binary, explicitly. It usually sits in the checkout, but a
        # worktree under `/tmp` — which is how arms are run, so an edit
        # cannot reach a suite in flight — puts it behind the tmpfs
        # above, and it vanishes: exec fails, the agent exits 1 in 0.0s
        # with no output, and twelve runs come back "agent exited 1
        # without writing a program". A sandbox should bind what it
        # needs rather than assume the rest of `/` survived.
        "--ro-bind", str(AGENT), str(AGENT),
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
        # **Absolute, and bound explicitly.** Two things bite here, and
        # each costs a whole arm of runs that fail identically with
        # nothing to say why. The sandbox `--chdir`s into the task
        # directory, so a relative `--card cards/ts` resolves against
        # the wrong place and the agent exits 2 having read no card.
        # And a card under `/tmp` — which is where a pinned worktree
        # lives, and arms are meant to be run from a worktree — is
        # behind the `--tmpfs /tmp` above unless it is bound back —
        # *after* that tmpfs, for the reason its own comment gives, and
        # the same reason the binary is bound there.
        card = card.resolve()
        argv[6:6] = ["--ro-bind", str(card), str(card)]
        argv[-2:-2] = ["--card", str(card)]
    argv.append(str(log))
    return argv


def pi_cmd(sandbox: Path, sessions: Path, prompt: str, thinking: str) -> list:
    """`pi` on the same task, in the same confinement, on the same model.

    `opencode-go/deepseek-v4-flash` is exactly what `host::deepseek.rs`
    defaults to, so the only thing that differs between the two sides is
    the agent. `--print` is non-interactive and auto-approves its tools,
    and `--session-dir` puts the record where the scorer can find it.

    `HOME` is redirected because pi takes lock files under `~/.pi`, and
    the confinement below has the real home read-only — the same reason
    the agent side binds its own writable paths explicitly.
    """
    return [
        "bwrap",
        "--ro-bind", "/", "/",
        "--bind", str(sandbox), str(sandbox),
        "--bind", str(sessions), str(sessions),
        "--bind", str(PI_HOME), str(PI_HOME),
        "--dev", "/dev", "--proc", "/proc",
        "--unshare-pid", "--unshare-ipc", "--unshare-uts",
        "--die-with-parent", "--new-session",
        "--setenv", "HOME", str(PI_HOME),
        "--chdir", str(sandbox),
        "pi", "-p",
        "--provider", "opencode-go",
        "--model", "deepseek-v4-flash",
        "--thinking", thinking,
        "--session-dir", str(sessions),
        prompt,
    ]


def score_log(log: Path) -> dict:
    out = subprocess.run(
        [str(AGENT), "score", str(log)], capture_output=True, text=True
    )
    if out.returncode != 0:
        return {"error": out.stderr.strip()}
    return json.loads(out.stdout)


def run_once(
    task, card: Path | None, timeout: int, keep: Path | None = None, agent: str = "code"
) -> dict:
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
                pi_cmd(sandbox, logdir, task.PROMPT, os.environ.get("PI_THINKING", "high"))
                if agent == "pi"
                else sandbox_cmd(sandbox, log, task.PROMPT, card),
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

        if agent == "pi":
            sessions = sorted(logdir.glob("*.jsonl"))
            score = (
                score_pi_session(sessions[-1]) if sessions else {"error": "no session written"}
            )
        else:
            score = score_log(log) if log.exists() else {"error": "no log written"}

        # A run that never got a completion is not evidence about
        # anything we changed. The provider stalls: the same task, card
        # and model took 34s and 1195s on 2026-09-16, the second spending
        # 99.6% of itself waiting. Scoring that as a failure would read a
        # queue as a regression, so it is counted apart and never as a
        # verdict on the card.
        if timed_out or score.get("programs", 0) == 0:
            # Two different things, and the message has to say which:
            # a run cut off mid-flight wrote programs and was stopped
            # by us, which is not a verdict on anything; a run with no
            # completion at all never heard back from the provider.
            wrote = score.get("programs", 0)
            if timed_out and wrote:
                why = f"cut off at the timeout after {wrote} program(s) — incomplete, not failed"
            elif timed_out:
                why = "no completion ever arrived before the timeout"
            else:
                # Exited on its own without writing a program: the agent
                # refused to start, which is a fault in how it was
                # invoked rather than anything about the run.
                why = f"agent exited {rc} without writing a program: {(out or '').strip()[-300:]}"
            ok = None
            credit = 0.0
        else:
            env = Env(task.DIR, sandbox, score)
            try:
                task.check(env)
                ok, why = True, ""
            except CheckFailed as e:
                ok, why = False, str(e)
            except Exception as e:
                ok, why = False, f"checker raised {e!r}"
            credit = grade(env, ok)

        if keep is not None:
            # `exist_ok` plus a second-resolution name is how two runs of
            # the same task under `--jobs` quietly became one: the second
            # copytree landed on top of the first and the evidence for a
            # whole run was gone, with nothing in the output saying so.
            # The suffix runs until a name is free, so N runs leave N
            # directories or the run fails loudly.
            stamp = time.strftime("%H%M%S")
            for n in range(100):
                kept = keep / (f"{task.NAME}-{stamp}" if n == 0 else f"{task.NAME}-{stamp}-{n}")
                try:
                    kept.mkdir(parents=True)
                    break
                except FileExistsError:
                    continue
            else:
                raise RuntimeError(f"no free --keep directory for {task.NAME}-{stamp}")
            shutil.copytree(sandbox, kept / "work", dirs_exist_ok=True)
            for record in list(logdir.glob("*.jsonl")):
                shutil.copy2(record, kept / record.name)
            (kept / "verdict.txt").write_text(
                f"pass={ok}  timed_out={timed_out}  exit={rc}\n{why}\n\nstdout:\n{out[-4000:]}\n"
            )

    return {
        "task": task.NAME,
        "kept": str(kept) if keep is not None else None,
        "pass": ok,
        "credit": credit,
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
            runs.append({"task": name, "pass": None, "credit": 0.0, "why": "no completion", "kept": str(run_dir), "score": score})
            continue
        env = Env(task.DIR, run_dir / "work", score)
        try:
            task.check(env)
            ok, why = True, ""
        except CheckFailed as e:
            ok, why = False, str(e)
        except Exception as e:
            ok, why = False, f"checker raised {e!r}"
        credit = grade(env, ok)
        print(f"  {run_dir.name}: {'pass' if ok else 'FAIL: ' + why}")
        runs.append({"task": name, "pass": ok, "credit": credit, "why": why, "kept": str(run_dir), "score": score})
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
            # The graded verdict, averaged over the runs that produced
            # one. Far lower variance than `passed`, because a task with
            # eight judgements in it reports eight of them.
            "credit": round(
                sum(r.get("credit", 0.0) for r in rs if r["pass"] is not None)
                / max(1, sum(1 for r in rs if r["pass"] is not None)),
                3,
            ),
            "calls_per_program": med([s["calls_per_program"] for s in scores]),
            "programs": med([s["programs"] for s in scores]),
            "exec_s": med([s["exec_ms"] / 1000 for s in scores]),
            # Input scales with *programs*, not with calls: 23 calls in
            # one program cost 33KB of prompt, 39 calls across four cost
            # 208KB. That ratio is the round-trip tax, and it is the
            # thesis stated in bytes.
            "prompt_kb": med([s["prompt_bytes"] / 1024 for s in scores]),
            "source_kb": med([s["source_bytes"] / 1024 for s in scores]),
            "thinking_kb": med([s["thinking_bytes"] / 1024 for s in scores]),
            "prompt_in": med([s.get("prompt_in", 0) for s in scores]),
            "cached_in": med([s.get("cached_in", 0) for s in scores]),
            "completion_out": med([s.get("completion_out", 0) for s in scores]),
            "provider_s": med([s["provider_ms"] / 1000 for s in scores]),
            "traps": traps,
            # Failures a dialect gap is implicated in — reported beside
            # the pass count, never subtracted from it.
            "failed_with_gap": sum(
                1 for r in rs if r["pass"] is False and "gap" in trap_kinds(r["score"])
            ),
            "trap_kinds": sorted({k for r in rs for k in trap_kinds(r["score"])}),
            "failures": [r["why"] for r in rs if r["pass"] is False],
        }
    return summary


def fingerprint(card: Path | None) -> dict:
    """What this suite is actually measuring: the binary and the card.

    A suite takes tens of minutes, and everything it runs is read off
    disk at spawn time — `target/debug/agent` per run, the card dir per
    run.  So an edit or a `cargo build` while it is in flight silently
    splits the run in two, and the aggregate at the end averages two
    different systems.  That happened on 2026-09-17: a `cargo build`
    partway through an n=7 suite meant the later runs measured a change
    the earlier ones did not have, and nothing in the output said so.

    Cheap to prevent by hashing what matters before and after.  It does
    not stop the mistake, but it makes a contaminated number impossible
    to mistake for a clean one, which is the part that costs a day.
    """
    h = hashlib.sha256()
    h.update(AGENT.read_bytes() if AGENT.exists() else b"")
    binary = h.hexdigest()[:12]
    h = hashlib.sha256()
    for f in sorted((card or Path("/nonexistent")).rglob("*")):
        if f.is_file():
            h.update(f.relative_to(card).as_posix().encode())
            h.update(f.read_bytes())
    # **The environment is part of what was measured.** The knobs that
    # change a run's behaviour without changing a byte of the binary or
    # the card live here — the reasoning level above all, which is the
    # whole variable in a thinking-level sweep. Three arms that differ
    # only by `DEEPSEEK_REASONING_EFFORT` would otherwise stamp
    # identically, and a mislabelled JSON would be indistinguishable
    # from a real result.
    knobs = {
        k: os.environ[k]
        for k in (
            "DEEPSEEK_MODEL",
            "DEEPSEEK_REASONING_EFFORT",
            "DEEPSEEK_NO_THINKING",
            "AGENT2_TRANSPORT",
        )
        if k in os.environ
    }
    return {"binary": binary, "card": h.hexdigest()[:12], "env": knobs}


def print_summary(summary: dict):
    stamp = summary.pop("_measured", None)
    for name, s in summary.items():
        no_run = f"   ({s['no_run']} never got a completion)" if s["no_run"] else ""
        gap = (
            f"   ({s['failed_with_gap']} of the failures hit a dialect gap)"
            if s.get("failed_with_gap")
            else ""
        )
        credit = f"   credit {s['credit']:.0%}" if "credit" in s else ""
        print(f"\n=== {name}  {s['passed']}/{s['runs']} passed{no_run}{gap}{credit}")
        print(
            f"  calls/program {s['calls_per_program']}   programs {s['programs']}"
            ""
        )
        print(f"  exec {s['exec_s']}s   waiting on the provider {s['provider_s']}s")
        print(
            f"  prompt {s['prompt_kb']}KB in   program {s['source_kb']}KB out"
            f"   reasoning {s['thinking_kb']}KB"
        )
        print(
            f"  tokens: {s['prompt_in']} in ({s['cached_in']} cached)"
            f"   {s['completion_out']} out"
        )
        for trap, n in s["traps"].items():
            print(f"  trap [{classify_trap(trap)}] x{n}: {trap}")
        for why in s["failures"]:
            print(f"  FAIL: {why}")
    if stamp:
        summary["_measured"] = stamp
        if stamp.get("changed_mid_run"):
            print(
                "\n!! THE TREE CHANGED WHILE THIS RAN — the runs above did not all\n"
                f"   measure the same thing: {stamp['before']} -> {stamp['after']}.\n"
                "   Re-run from a clean checkout before believing the number."
            )
        else:
            b = stamp["before"]
            env = b.get("env") or {}
            shown = "  ".join(f"{k}={v}" for k, v in sorted(env.items()))
            print(
                f"\n(binary {b['binary']}, card {b['card']}"
                + (f", {shown}" if shown else ", default reasoning")
                + ")"
            )


def wilson(k: int, n: int) -> tuple:
    """A 95% interval on a pass rate, so k/n is read as the sample it is.

    Wilson rather than the textbook normal interval because these are
    small n with rates near the ends, where the normal one runs past 0
    and 1 and is narrowest exactly where it is least trustworthy.
    """
    if n == 0:
        return (0.0, 1.0)
    z = 1.96
    phat = k / n
    denom = 1 + z * z / n
    centre = (phat + z * z / (2 * n)) / denom
    half = z * ((phat * (1 - phat) / n + z * z / (4 * n * n)) ** 0.5) / denom
    return (max(0.0, centre - half), min(1.0, centre + half))


def pool_suites(out, parts: list) -> int:
    """Sum several suites of the *same* configuration into one rate.

    **Because n=7 per task does not resolve the differences we argue
    from.** Two suites of the identical configuration -- same commit,
    same card, hashes matching -- came back 26/28 and 21/28 on
    2026-09-17. A five-point swing from nothing but sampling, which is
    larger than most of the changes this driver has been used to
    justify. One suite is one sample of a noisy process, and reporting
    it as "the number" is how a run of luck becomes a finding.

    Pass counts add. The per-run medians (calls/program, tokens, wall)
    are averaged weighted by runs and labelled a mean of medians, rather
    than dressed up as a median of the pool it is not.
    """
    stamps, parsed = set(), []
    for part in parts:
        d = json.loads(Path(part).read_text())
        stamp = d.pop("_measured", None)
        if stamp and stamp.get("changed_mid_run"):
            print(f"!! {part} was measured against a tree that changed mid-run")
        if stamp:
            stamps.add(json.dumps(stamp.get("before", {}), sort_keys=True))
        parsed.append(d)
    if len(stamps) > 1:
        print(
            "!! these were not all measured against the same binary and card —\n"
            "   pooling them averages two different systems, which is the one\n"
            "   thing pooling must not do."
        )
        return 2

    COUNTS = ("runs", "passed", "no_run", "failed_with_gap")
    pooled = {}
    for d in parsed:
        for name, s in d.items():
            acc = pooled.setdefault(name, {"_traps": {}, "_failures": []})
            for k, v in s.items():
                if k in COUNTS:
                    acc[k] = acc.get(k, 0) + v
                elif isinstance(v, (int, float)):
                    acc[k] = acc.get(k, 0) + v * s["runs"]
            for t, n in s.get("traps", {}).items():
                acc["_traps"][t] = acc["_traps"].get(t, 0) + n
            acc["_failures"].extend(s.get("failures", []))

    total_k = total_n = 0
    for name, acc in sorted(pooled.items()):
        n = acc["runs"]
        for k in list(acc):
            if k not in COUNTS and not k.startswith("_"):
                acc[k] = round(acc[k] / n, 3) if n else 0
        acc["traps"] = acc.pop("_traps")
        acc["failures"] = acc.pop("_failures")
        lo, hi = wilson(acc["passed"], n)
        total_k += acc["passed"]
        total_n += n
        credit = f"   credit {acc['credit']:.0%}" if "credit" in acc else ""
        print(f"\n=== {name}  {acc['passed']}/{n}   95% CI {lo:.0%}-{hi:.0%}{credit}")
        print(
            f"  mean of per-run medians: calls/program {acc['calls_per_program']}"
            f"   programs {acc['programs']}   provider {acc['provider_s']}s"
        )
        print(f"  tokens: {acc['prompt_in']:.0f} in   {acc['completion_out']:.0f} out")
    lo, hi = wilson(total_k, total_n)
    overall = [acc for acc in pooled.values() if "credit" in acc]
    mean_credit = (
        sum(a["credit"] * a["runs"] for a in overall) / sum(a["runs"] for a in overall)
        if overall
        else None
    )
    credit = f"   credit {mean_credit:.0%}" if mean_credit is not None else ""
    print(
        f"\nall tasks  {total_k}/{total_n}   95% CI {lo:.0%}-{hi:.0%}{credit}"
        f"  ({len(parts)} suites)"
    )
    if out:
        Path(out).write_text(json.dumps(pooled, indent=2))
        print(f"wrote {out}")
    return 0


def compare(before: Path, after: Path):
    """Before/after on the numbers a change is argued from.

    A change that fixes one task while costing another is not an
    improvement, so every task is shown, not just the ones that moved.
    """
    a = json.loads(before.read_text())
    b = json.loads(after.read_text())
    for side, d in (("before", a), ("after", b)):
        stamp = d.pop("_measured", None)
        if stamp and stamp.get("changed_mid_run"):
            print(f"!! {side} was measured against a tree that changed mid-run")
    for name in sorted(set(a) | set(b)):
        x, y = a.get(name), b.get(name)
        if not x or not y:
            print(f"{name}: only in {'before' if x else 'after'}")
            continue
        print(f"\n=== {name}")
        print(f"  passed          {x['passed']}/{x['runs']}  ->  {y['passed']}/{y['runs']}")
        if x.get("no_run") or y.get("no_run"):
            print(f"  no completion   {x.get('no_run', 0)}  ->  {y.get('no_run', 0)}")
        for key in (
            "credit", "calls_per_program", "programs", "exec_s",
            "prompt_kb", "source_kb", "thinking_kb",
            "prompt_in", "cached_in", "completion_out",
        ):
            # A summary written before a metric existed simply lacks it;
            # say so rather than dropping the row or, worse, raising
            # halfway down the table and printing a partial comparison
            # that looks complete.
            left = x.get(key, "n/a")
            right = y.get(key, "n/a")
            if left == "n/a" and right == "n/a":
                continue
            print(f"  {key:<17} {left}  ->  {right}")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tasks", help="comma-separated task names (default: all)")
    p.add_argument("--repeat", type=int, default=3)
    p.add_argument(
        "--jobs", type=int, default=4, help="runs in flight at once (they are network-bound)"
    )
    p.add_argument("--card", type=Path, help="card directory for this variant")
    p.add_argument(
        "--agent",
        choices=("code", "pi"),
        default="code",
        help="which agent to drive — `pi` is the tool-loop comparison, same model",
    )
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--out", type=Path, help="write the aggregate here as JSON")
    p.add_argument("--list", action="store_true")
    p.add_argument("--verify-only", action="store_true", help="check the checkers, run nothing")
    p.add_argument("--keep", type=Path, help="keep each run's sandbox and log under here")
    p.add_argument("--compare", nargs=2, type=Path, metavar=("BEFORE", "AFTER"))
    p.add_argument(
        "--pool",
        nargs="+",
        metavar="SUMMARY",
        help="sum several suites of the same configuration into one rate with a "
        "confidence interval — n=7 per task does not resolve a five-point "
        "difference, and two identical suites have differed by that much",
    )
    p.add_argument("--rescore", type=Path, help="re-judge a --keep directory, no completions")
    args = p.parse_args()

    if args.pool:
        return pool_suites(args.out, args.pool)
    if args.compare:
        compare(*args.compare)
        return 0

    # The agent runs `--chdir` into the sandbox, so a card path relative
    # to the repo resolves to nothing there. Twelve runs on 2026-09-16
    # exited 2 before contacting anything and were reported as twelve
    # honest "no completion"s, which is true and useless.
    if args.card:
        args.card = args.card.resolve()
        if not (args.card / "card.md").exists():
            print(f"{args.card}/card.md does not exist", file=sys.stderr)
            return 2

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

    # Built here rather than checked for existence: `cargo test` and
    # `cargo clippy` do not produce this binary, so a session that runs
    # both after an edit still leaves a stale one on disk. A live run on
    # 2026-09-16 measured a fix that was not in the binary it ran.
    build = subprocess.run(
        ["cargo", "build", "-q", "-p", "agent", "--bin", "agent"], cwd=REPO
    )
    if build.returncode != 0 or not AGENT.exists():
        print(f"{AGENT} did not build", file=sys.stderr)
        return 2

    if args.rescore:
        summary = aggregate(rescore(args.rescore, usable))
        print_summary(summary)
        if args.out:
            args.out.write_text(json.dumps(summary, indent=2))
            print(f"\nwrote {args.out}")
        return 0

    # Each agent authenticates its own way: ours from the environment,
    # pi from its own config under PI_HOME.
    if args.agent == "code" and "DEEPSEEK_API_KEY" not in os.environ:
        print("DEEPSEEK_API_KEY is not set", file=sys.stderr)
        return 2

    # Runs are independent and spend nearly all their time waiting on a
    # completion — one 2026-09-16 run sat 317 seconds for its second
    # program with the CPU idle — so they overlap. Serial, the suite is
    # paced by the slowest queue in the provider rather than by anything
    # being measured.
    before = fingerprint(args.card)
    work = [(task, i) for task in usable for i in range(args.repeat)]
    printing = threading.Lock()
    runs = []

    def one(item):
        task, i = item
        r = run_once(task, args.card, args.timeout, args.keep, args.agent)
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

    after = fingerprint(args.card)
    summary = aggregate(runs)
    summary["_measured"] = {
        "before": before,
        "after": after,
        "changed_mid_run": before != after,
    }
    print_summary(summary)
    if args.out:
        args.out.write_text(json.dumps(summary, indent=2))
        print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

# A frontier model on `sweep-8`

`gpt-5.6-sol` through a ChatGPT subscription against
`deepseek-v4-flash` via opencode, five runs each, same notebook card,
same task. Effort `medium` on both sides.

| | flash | `gpt-5.6-sol` |
|---|---|---|
| passed | 5/5 | 5/5 |
| traps | 1 | **0** |
| programs | 3.0 | **2.0** |
| calls / program | 4.3 | 4.0 |
| output tokens | 3,460 | **913** |
| prompt tokens | 20,536 | **13,344** |
| cached | 29% | 36% |
| reasoning tokens | 521 *(estimated)* | 246 *(reported)* |
| **wall** | **36 s** | 41 s |
| ↳ exec, local | 6.6 s | 21.4 s |
| ↳ provider | 29.6 s | **19.3 s** |
| ↳ provider **per program** | 9.9 s | 9.7 s |

## The wall clock says the opposite of what it looks like

The frontier model is *slower end to end* and *faster at its own part*.
Provider time falls 35% because it needs two round trips where flash
needs three, and per-round-trip latency is the same to within 2%
(9.9 s against 9.7 s). What eats the gain is local execution, 6.6 s →
21.4 s.

**That is not harness overhead. It is verification.** The tool mix:

| | bash calls | shape |
|---|---|---|
| flash | 22 | mostly `grep` |
| `sol` | **8** | `python test.py && python -m py_compile … && python lint.py` |

A third the shell calls, each spawning several Python interpreters.
Flash greps for references; `sol` runs the test suite, byte-compiles
every file and runs the linter. The extra seconds buy real checking,
and they are most of why two programs suffice: it verifies properly
once instead of iterating.

## What this says about code mode

The notebook thesis is that ending a program returns no result and no
control, so the model is pushed toward larger self-contained programs.
A stronger model does exactly that, harder: **2 programs, 4 calls
each, a quarter of the output tokens.** Round trips are the metric the
shape exists to reduce, and they fall.

## Caveats

- `sweep-8` is saturated. Both sides pass everything, so nothing here
  is about capability. Every task in the suite was tuned against flash.
- Reasoning tokens are reported here and *estimated* for flash, so the
  two numbers are not comparable. Within a model they are fine.
- The drafting metric cannot be computed on this model at all: its
  reasoning summary is a headline (0.1 KB a run, e.g. `**Planning
  batch file inspection**`).
- n=5, one task, one model each.

## Before these numbers were trustworthy

Five faults, all in this repo, found by reading rendered sessions:
the `/codex/responses` path, missing `reasoning.summary` (which left
`Part::Thinking` empty), missing session-affinity headers (caching 1
hit in 6 → 98%), `drive.py` announcing the wrong model, and output
items concatenated without a separator — which glued a fence mid-line,
produced `…after the edit.```js`, ran no program, and failed a task
while looking exactly like the model refusing to act.

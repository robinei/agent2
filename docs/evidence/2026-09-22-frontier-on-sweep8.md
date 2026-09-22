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

## Four models, and the task is not saturated after all

Five runs each, same card, same task, effort `medium`.

| model | pass | programs | calls/p | out tok | reas tok | wall | exec | provider | prov/prog |
|---|---|---|---|---|---|---|---|---|---|
| `deepseek-v4-flash` | **5/5** | 3.0 | 4.3 | 3,460 | 527 | 36 s | 6.6 | 29.6 | 9.9 |
| `gpt-5.6-sol` | **5/5** | 2.0 | 4.0 | 913 | 246 | 41 s | 21.4 | 19.3 | 9.7 |
| `gpt-5.6-luna` | 3/5 | 2.0 | 2.5 | 1,005 | 425 | 32 s | 17.6 | 14.7 | 7.3 |
| `gpt-5.6-terra` | 4/5 | 3.0 | 3.0 | 1,236 | 361 | **27 s** | 16.1 | 10.8 | **3.6** |

I predicted saturation and was wrong: three of twenty runs fail, and
they separate the models.

**Both luna failures are the same thing, and it is not a capability
gap.** It reads the files, gathers the evidence, and then writes prose
and stops — *"I've collected the helper definitions and repository-wide
reference search. I'm stopping here until…"*. One program, zero tool
calls on the second reply, branch rested, task untouched. That is
exactly the failure the card names: **a reply with no code blocks rests
the branch**, and it is the one whose violation is silent.

terra's single failure is different and ordinary — it did the work and
got the answer wrong.

**So the cheapest model in the table beats two frontier variants**, not
by being smarter but by always acting. On a task where the work is
unambiguous, the discriminator is willingness to finish rather than
ability to reason.

Per-round-trip latency is where the frontier models are plainly ahead:
9.9 s for flash against 3.6 s for terra. The notebook shape converts
that into wall clock only when the model actually uses its turn.

## The output-item fix, confirmed

Across all twenty runs, **zero prose parts contain a fence** — the seam
that produced `…after the edit.```js` and cost a task is gone, and the
two-sentence prose in luna's failure shows the separator doing its job.

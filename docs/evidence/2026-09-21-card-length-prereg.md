# Preregistration: does a shorter, more imperative card change anything?

Written **before the first run**, and deliberately so. I wrote the short
card. Choosing the metric after seeing the numbers would let me pick the
one that flatters it, which is the failure mode the last A/B walked into
from the other side — output tokens looked decisive at run 11 and were
not decisive at run 12.

## The manipulation

Two arms, one variable: `agent/card/card.md`.

| arm | card | bytes | system prompt |
|---|---|---|---|
| `long` | `839d8fb:agent/card/card.md` | 22,273 | 23,322 |
| `short` | `799ff3a` (HEAD) | 15,255 | 16,304 |

Same exemplars, same binary, same task, same provider. `--card <dir>` is
fingerprinted into every result file, so an arm that silently ran the
wrong card shows up as a hash mismatch rather than as a result.

`AGENT2_NO_REHEARSAL_TAIL` is **unset in both arms** — the shipped
configuration. The short card carries the imperative rehearsal ban in
its prose ("Never draft a block. Write it."); the long card carries the
discursive one ("so do not rehearse a block — write it"). That
difference is part of the manipulation, not a confound to remove.

Task `sweep-8`, `deepseek-v4-flash` via opencode. 6 runs per arm,
interleaved `long, short, long, short, …` so provider drift hits both
arms equally.

## Metrics, in the order they settle the question

**Primary — did the work come out right, and did the programs run.**

- `passed` — 6 per arm is far too few to move this, so it is a guard: if
  the short card breaks something, this is where it shows first.
- `traps` / `trap_kinds` — programs that errored. A defect count. This
  is the one that would catch a rule whose rationale I cut.

**Secondary — the effect I actually claimed.**

- `thinking_kb` — reasoning bytes. If the register of the prompt
  transfers to the register of the output, it shows here.

**Descriptive only, and explicitly not the verdict.**

- `programs`, `reasoning_out`, `completion_out`, wall, `stops`.

`stops` is reported and never scored as a defect. A `stop(reason)` is a
run noticing its own check came back wrong and saying so — the card
working. The last A/B's central error was that output tokens conflate
that with waste: the expensive runs were expensive *because* a guard
caught a wrong answer and the model then fixed it.

## Predictions

Point estimates, so they can be wrong:

- **~60% no detectable difference on any metric.** Every rule survived
  the cut; what went was the paragraph after it. Compliance is also
  carried by the five exemplars, which did not change — and in this
  codebase the exemplar has beaten the rule at least three times.
- **~25% `short` lower on `thinking_kb`**, by 10–25% at the median.
- **~15% `short` worse**, and if so, at one of these three, in order:
  1. `history.remove`/`replace` timing — the cache-cost argument
     (88% → 47.5% cached, uncached ×4.4) is now one clause.
  2. `history.replace` on an already-summarised row — the consequence
     became a statement of fact.
  3. "Do not design the batch" — both halves kept, the why compressed.

## What this run cannot do

Settle it. `sweep-8` output has a heavy tail — the last sweep ran 2,797
at the median and 14,835 on one run — so at n=6 per arm the median test
returns p ≈ 0.3 whatever is true. 15–20 per arm is the honest number for
a 25% median shift.

So the result to expect is "no signal", and the question this run
*does* answer is the narrow one: **did cutting 31% of the prompt break
anything visible.** A trap kind that appears only in `short`, or a
failed run, is worth more than any median here.

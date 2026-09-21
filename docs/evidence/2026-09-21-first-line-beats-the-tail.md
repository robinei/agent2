# The first line of the card does what the tail could not

Measured with the prompt lab (`agent capture` / `agent sample`), which
freezes one context and samples the next completion. One sentence,
three positions, one captured state (`sweep-8`, `deepseek-v4-flash`,
mid-3 at event #71 — a late reply facing a failing test and a lint
contradiction).

> Thinking is for reasoning! Never draft code blocks! One-shot them in the reply.

## Pooled, 80 samples per arm

| | drafted at all | drafts | think_b | completion_tok | ms |
|---|---|---|---|---|---|
| in the tail | 89% | 3 | 9,106 | 2,194 | 24,754 |
| **first line of system** | **51%** | **1** | **3,366** | **915** | **11,304** |

All of those: **p < 0.0001**. Thinking down 63%, completion tokens
down 58%.

## And the tail slot does nothing

| comparison | n | drafting rate | p |
|---|---|---|---|
| tail position 2 vs tail last | 40 v 40 | 90% vs 85% | 0.74 |
| first vs first+last (`both`) | 40 v 40 | 48% vs 45% | 1.00 |

So the sentence works at the front, is inert at the back, and
repeating it at the back adds nothing to having it at the front.

**This overturns `2026-09-21-tail-position-result.md`.** That found
tail-last beating tail-middle at p = 0.0054 on whole-run draft counts,
n = 6 v 12. Held still and sampled 80 times an arm, the same contrast
is p = 0.35. The earlier result was marginal against its own
Bonferroni threshold and is best read as a false positive. There is no
compounding route left to rescue it: `document.rs` drops
`Part::Thinking` and the provider requires `reasoning_content` excluded
from the next turn, so drafting in one reply cannot reach the next
reply's context.

## Time is not a confounder here, and that was checked rather than assumed

Two arms were sampled twice, 30–40 minutes apart, same document:

| | `first` re-run | tail re-run |
|---|---|---|
| drafted at all | p = 0.65 | p = 1.00 |
| drafts | p = 0.33 | p = 0.36 |
| think_b | p = 0.23 | p = 0.10 |
| completion_tok | p = 0.20 | p = 0.095 |
| **ms** | p = 0.69 | **p = 0.0085** |

Only wall clock moves. Every semantic metric is exchangeable across the
gap — which is what licenses the pooling above, and which matches the
prediction that against a fixed model the only live nondeterminism is
the sampler.

The medians still wandered (3,366 against 2,176 and 4,186 in the two
`first` batches) without being significant, which is a standing warning
about reading a heavy-tailed median at n = 40 as an effect size.

## What changed

- The sentence is the **first line of `card.md`**.
- The tail line is **on by default and last**. It is carried despite
  measuring as inert, because this is one model, one task and one
  state; a model that weights the end of its context more would be
  served by it, `arm-both` shows the combination is not worse than the
  front alone, and it costs 80 bytes.
- `AGENT2_NO_REHEARSAL_TAIL=0` and `AGENT2_NO_REHEARSAL_LAST=0` turn
  each half off.

## What this does not establish

One state, one task, one model, one sentence. The state is a
terminal-ish reply where the model concludes the task is done either
way, so nothing here says the shorter thinking is *as good* — only that
it is shorter. The obvious next measurements are other capture points
(an early planning turn, a mid-run edit turn) and whether reply quality
holds when thinking is cut by 63%.

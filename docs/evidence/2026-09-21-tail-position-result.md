# Result: the same sentence does nothing at position 5 and works at position 9

Preregistered in `2026-09-21-tail-position-prereg.md`. Primary metric,
as registered: **fenced drafts inside `Part::Thinking`, per run.**
`sweep-8`, `deepseek-v4-flash`, all arms on the shipped short card.

## The preregistered comparison

| | n | median drafts | all runs |
|---|---|---|---|
| line **off** | 6 | 8.5 | 0, 1, 4, 13, 17, 21 |
| line at **position 5** (`mid`) | 6 | 7.0 | 2, 3, 6, 8, 10, 30 |
| line **last** (`last`, pooled) | 12 | **1.0** | 0, 0, 0, 1, 1, 1, 1, 1, 3, 4, 4, 5 |

Exact Mann-Whitney, two-sided:

- **`mid` vs `last`: U = 64.5, p = 0.0054** ← the preregistered test
- `off` vs `mid`: p = **0.937** — the line at position 5 is
  indistinguishable from not having the line at all
- `off` vs `last`: p = 0.105 (directionally right, `off` only n=6)

The two `last` batches, run about an hour apart, agree: medians 2.0
and 1.0, p = 0.70. Provider drift over the session is small, which is
what licenses pooling them.

**I predicted this would show nothing, at 75%.** The reasoning was
that the move is only ~60 tokens inside a block already at the end of
the request, against the one precedent that worked being a ~20 KB jump
from card to report. That reasoning was wrong: the last line is not
merely "near the end", it is a distinct position, and 60 tokens is
enough to lose a sentence in.

## What the effect actually is

The per-reply rate — the fraction of replies whose thinking contains
any fence — is **not** significant:

| | replies | drafted |
|---|---|---|
| `mid` | 18 | 50% |
| `last` (pooled) | 31 | 32% |

Fisher's exact **p = 0.24**. So the ban in the last slot does not much
change *whether* a reply starts drafting. It changes *how far it
goes*: `mid` has runs at 30, 10 and 8 drafts, and the worst run in
twelve `last` runs is 5.

This matters, and it is a correction to something I said earlier in
the session. I proposed the per-reply proportion as the more robust
metric *after* seeing noisy counts, and argued it was better because
one runaway reply cannot dominate it. The preregistered count is the
one that found the effect, and the post-hoc "improvement" would have
missed it — because the runaway replies *are* the phenomenon, not
noise in it.

## The guard: demoting the markdown rule cost nothing

`last` and `both` push `REPLY_IS_MARKDOWN` out of the final slot,
which the prereg named as the price of the arm.

- `last` + `last2` + `both`: **18 runs, 18 passed.**
- `mid`, which keeps the markdown rule last: **5 of 6.**

The only failure in the experiment was in the arm that did *not* pay
the price. On this evidence the demotion is free — 18 runs is not
many, and the failure mode it guards against is rare, so this is
permission to ship, not proof of safety.

## Repetition did not help; position did

`both` — the ban opening the card *and* closing the tail, the same
words first and last — came in at median 4.5 drafts against `last`'s
1.0 (p = 0.29). Directionally worse, not better.

So the U-shape result here is one-sided: **the last slot did the work,
and duplicating the sentence into the first slot undid some of it.**
Adding 269 bytes of role framing to the top may simply be the same
finding as the card-length experiment from the other direction — more
text, no gain.

## Caveats

- One task, one model, n = 6 vs 12.
- Many comparisons were run. Under Bonferroni for ~10 tests the
  threshold is 0.005, and the headline sits at 0.0054 — at the line,
  not clear of it.
- `sweep-8` passed 34 of 35 runs across both experiments, so none of
  this is evidence about correctness. It is evidence about tokens.
- Nothing here shows drafting is *harmful*. Across 29 runs, drafts
  correlate with reasoning bytes at rho = +0.83 (nearly tautological)
  but with program count at only +0.25.

## Recommendation

Turn the rehearsal line on by default **and last**. `off` and
`position 5` are statistically the same thing; only the final slot
does anything. Do not add the opening line.

The confirmation worth paying for is the paired-replay design: freeze
one conversation state, sample the next reply K times per arm, and
measure drafting as a Bernoulli per sample. It removes between-run
path variance entirely, and it is cheap because no tool loop runs.

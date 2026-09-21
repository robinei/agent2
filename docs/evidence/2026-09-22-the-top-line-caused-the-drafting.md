# The ban at the top of the card doubled the drafting it forbade

Yesterday's `2026-09-21-first-line-beats-the-tail.md` put the rehearsal
ban on the card's first line, on 80 lab samples an arm at p < 0.0001.
It is now removed. Everything measured since says the line made things
worse.

## Seven variants, one document, n=24 each

Document A: `sweep-8`, `deepseek-v4-flash`, captured before a reply
that had drafted 28 times.

| arm | drafted | drafts | thinking B | out tok |
|---|---|---|---|---|
| `v6-stripped` — ban gone from all three places | 46% | 0.0 | 5,886 | 1,830 |
| `v2-cheap` — "being wrong costs one cheap round trip" | 75% | 2.0 | 7,594 | 2,682 |
| `v1-none` — no top, no tail (mid-card prose kept) | 46% | 0.0 | 7,726 | 2,426 |
| `v0-shipped` | 62% | 1.5 | 9,290 | 2,936 |
| `v5-all` | 79% | 2.0 | 10,608 | 3,479 |
| `v4-permission` — "you may get it wrong" | 79% | 2.0 | 10,711 | 3,452 |
| `v3-redirect` — "thinking is for deciding" | 79% | 3.0 | 12,728 | 3,858 |

No pairwise test is significant. **The arms split on one feature
instead:** whether anything at all sits at the top.

**Lowering the pressure made it worse, not better.** All three arms
built to reduce the stakes — cheap, permission, all — drafted more than
the card that simply forbids it. The stakes were never the mechanism.

## The split, confirmed across two documents

| document | a sentence at the top | nothing at the top | |
|---|---|---|---|
| A | 75% drafted (n=120) | 46% (n=48) | p = 0.0005 |
| B | 96% drafted (n=24) | 81% (n=48) | p = 0.149 |
| **stratified (CMH)** | | | **p = 0.00013** |

B does not reach significance alone and is nearer its ceiling, so the
two are combined with Cochran–Mantel–Haenszel rather than pooled raw —
their baselines are too far apart for a naive pool to mean anything.
Both strata point the same way.

**The mechanism is priming, not instruction.** A sentence at the top
that names code blocks puts code blocks in the reasoning context, and
the model obliges. The worst of the seven arms, `v3-redirect`, is the
one that mentions code most while forbidding it least. The A split was
found after the fact; the B arms were fixed before those samples
existed.

## And the task level agrees, which is the part that was missing

5 runs an arm, same card but for that one line, both arms 5/5 passing:

| | drafts/run | thinking KB | tokens out | wall |
|---|---|---|---|---|
| line present | 10.0 | 20.7 | 6,511 | 39 s |
| line removed | **1.0** | **11.1** | **3,307** | **28 s** |

drafts `[0, 1, 10, 10, 34]` against `[0, 1, 1, 2, 3]`. Every median
favours removal by about 2×, at p = 0.29–0.40 — underpowered, never
contradictory. Per *reply* the drafting rate is the same (4/13 against
4/11): the line does not change whether a reply drafts, it changes how
far the drafting runs before it stops.

## Why this is a revert rather than a finding

The line went in yesterday on one captured state, and its own
task-level check then found nothing (`2026-09-21` vs-pi run: thinking
medians 22.8 / 23.5 / 24.2 KB across three card configurations, all
p ≥ 0.69). It is now removed on a stratified lab result at p = 0.00013
plus a task run where all four medians move the right way. No metric
anywhere favours keeping it.

The tail line stays: measured inert (p = 0.35 for its slot), 80 bytes,
and kept against the chance that another model weights the end of its
context differently.

## The standing lesson

Two lab results in two days were confident and state-specific. The lab
answers "does this change this reply against this context" precisely
and cheaply, and that is not the same question as "does this change a
run". Nothing derived from it goes in the card without a task run,
and one capture point is never enough to generalise from.

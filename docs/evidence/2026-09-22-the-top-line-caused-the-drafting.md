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

**The mechanism is not priming**, though that is what this file said
first. The objection that killed it: the card is already saturated
with what the ban names — its opening line *is* "Your reply is
**markdown**, and the code blocks in it run", the five exemplars are
```js blocks, and `REPLY_IS_MARKDOWN` names ```js in the tail. If
mentioning code blocks caused drafting, one more sentence could not
double it.

## What it is instead: naming the behaviour you are suppressing

A 2x2 on the same top slot, same document, same register, n=24 each
(`v0`/`v1` pooled to n=48 across all rounds):

| top line | names code | forbids the behaviour | drafted | drafts | thinking B |
|---|---|---|---|---|---|
| `v0-shipped` "Never draft code blocks!" | yes | yes | 71% | 2.0 | 10,967 |
| `v8-positive` "Code blocks belong in the reply, where they run" | yes | no | 58% | 1.0 | 9,982 |
| `v1-none` | – | – | 52% | 1.0 | 6,515 |
| `v7-neutral` "Never guess at what you can check!" | no | yes | 42% | 0.0 | 7,669 |

- the ban vs nothing: **drafts p = 0.0144, thinking p = 0.0116** (n = 72
  an arm, three rounds of 24 — at two rounds these read 0.0061 and
  0.0079, so the third weakened them without changing the verdict)
- the ban vs a same-register negation about something else:
  **drafts p = 0.0146, rate p = 0.022**
- forbidding something else vs nothing: p = 0.46 / 0.76 / 0.22 — nothing
- naming code without forbidding vs nothing: rate p = 0.80, drafts p = 0.38

So it is not the slot (`v7` sits at nothing), not the imperative
register, not negation as such (`v7` negates too), and not naming code
(`v8` is indistinguishable from nothing on rate and count). What is
left is **naming the specific behaviour to be suppressed**, which is
the ironic-process result rather than a priming one.

**The rate does not survive, and the volume does.** Pooled over three
rounds the drafting *rate* is 71% against 61% at **p = 0.29** — at two
rounds it was 0.093, and the third round reversed the direction outright
(71% against 79%). What holds is the count and the bytes. So the claim
is narrower than "the ban makes replies draft": it makes the replies
that draft go on longer.

**Rounds of 24 are not stable on rate.** The same arm on the same
document ran 46%, 58% and 79% across three rounds over about two hours.
An earlier check had found the same document exchangeable over 30–40
minutes on everything but wall clock; over a longer window the rate
moves too. Any rate reported from a single round of 24 here should be
read as a wide interval.

**And the rate/volume split matters.** Pooling every top-line arm
against none, the drafting *rate* is 60% against 58%, p = 1.00 — but
thinking volume is 10,310 B against 6,248 B, p = 0.0041. A sentence at
the top does not change how often a reply drafts. It changes how long
the reply thinks.

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

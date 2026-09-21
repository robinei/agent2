# Preregistration: does the rehearsal ban work better in the last slot?

Written before the first run. Follows the card-length result, which
came back flat: if *length* does nothing, **position** is the next
lever, and the bottom of `request_tail()` is the only position we own.

## Arms

Three, each adding one thing to the one before. All use the shipped
short card; `deepseek-v4-flash` via opencode, `sweep-8`, `--jobs 1`,
interleaved, 6 per arm.

| arm | the ban's position | card |
|---|---|---|
| `mid` | 5th of 9 in the tail — yesterday's winning arm, as it stood | short |
| `last` | below `REPLY_IS_MARKDOWN`: the last line in the whole request | short |
| `both` | last, **and** the same sentence opening the card | short + 269 B |

`both`'s opening line:

> **You are a coding agent. You act only by writing ```js blocks into a
> markdown reply: that is the one way to call a tool, read a file, or
> change anything — nothing else you emit runs. Thinking is for
> reasoning! Never draft code blocks! One-shot them in the reply.**

Its last sentence is **verbatim** the tail line, so `both` puts the
identical words at the first and last position in the context. That is
the U-shape claim stated as an experiment.

`AGENT2_NO_REHEARSAL_TAIL` and `_LAST` are now fingerprinted into every
result file — `mid` and `last` differ by the knob alone, same binary
and same card, so without that they would stamp identically.

## Metrics

**Primary — fenced drafts inside `Part::Thinking`, per run.** The count
of ` ```js `/` ```ts ` fences in the reasoning stream. This is the
mechanism the ban targets, and it is far sharper than reasoning bytes:
the original probe moved it 6 → 1.

**Secondary — reasoning KB.** What the bytes did, whether or not the
drafts moved.

**Guard — `passed`, `traps`.** Demoting `REPLY_IS_MARKDOWN` out of the
last slot is the price of `last` and `both`. If that rule starts being
violated, it shows here, and it is the reason not to ship either arm
on a token win alone.

**Descriptive — programs, output tokens, wall, stops.**

## Baseline

Re-analysing the 12 card-length runs, which all ran with **no tail
line**: median **4** drafts per run, mean 7.1, range 0–21, and only 1
run of 12 drafted nothing. So there is room to move in either
direction, and the metric is not floor-bound.

## Predictions

- **`mid` vs `last`: ~75% nothing.** The displacement is about 60
  tokens inside a block that is already at the end of the request. The
  one positional move in this repo that demonstrably worked — a
  tool-call-syntax ban ignored 3 runs of 3 in the card, obeyed when
  moved to the report — was a ~20 KB jump. This is not that. If
  U-shaped attention operates at the scale of tens of thousands of
  tokens, shuffling within the final nine lines should be invisible.
- **`both` vs `mid`: ~50% nothing, ~30% `both` lower on drafts, ~20%
  `both` worse.** It is the only arm that adds text, and a role
  sentence at the top could as easily invite preamble as suppress
  drafting.
- **All three below the n=12 baseline median of 4**, since all three
  carry the ban somewhere and the baseline carried it nowhere.

## A confound named in advance

`both` changes two things against `last`: the repetition *and* the
role framing ("You are a coding agent"). If `both` wins, this run
cannot say which did it, and the follow-up is an arm with the opening
sentence minus the ban.

## Flagged from the card-length run, and not tested here

Post-hoc, so it is a hypothesis: the short card drafted **more** than
the long one (median 8.5 vs 3.0, p = 0.556) and reasoned slightly
longer (22.8 vs 19.9 KB, p = 0.937). Two metrics, same direction,
neither significant. If the rehearsal paragraph lost force when I cut
it, that is where it would show. All three arms here use the short
card, so this run holds it constant and cannot address it.

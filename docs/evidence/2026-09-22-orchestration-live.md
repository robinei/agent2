# What it takes to make a model reach for helpers

Live sessions driven turn by turn (`session --stdin`), `gpt-5.6-sol`,
same fixture and same words to both arms. The only difference is the
card: **old** is `843eca0`, before this session's orchestration work;
**new** is after it, including two worked examples.

Fixture: three 24 KB service logs, one containing a pool-exhaustion
error.

## The card changed the behaviour, immediately and without a hint

**Turn 1** — *"One of alpha.log, beta.log or gamma.log shows a
connection pool being exhausted. Which service, and what limit?"*
Nothing about helpers.

| | spawns | completions | out | prompt | wall | answer |
|---|---|---|---|---|---|---|
| old | **0** | 2 | 243 | 12,402 | 11.5 s | correct |
| new | **3** | 4 | 553 | 29,454 | 15.9 s | correct |

The new card spawned one helper per log, with charters it wrote
itself, `ask`ed all three, and collected the answers — the exact shape
of the new exemplar, on a prompt that mentions none of it. Before
today that verb had two uses in 2,295 cells.

**So the answer to "what does it take" is: the card.** Not a hint in
the prompt, not a nudge. The capability was reachable all along.

## And it is now reached for when it should not be

The old card grepped all three files in one call and was done. Half
the completions, 40% of the tokens, faster, same answer. **72 KB of
logs does not make delegation rational when a tool can filter them
without reading them.**

**Turn 2** — *"read each log and tell me what its traffic pattern
looks like; judgement, not grep."* Work grep cannot do.

| | spawns | completions | out | prompt | answer |
|---|---|---|---|---|---|
| old | 0 | **5** | **1,346** | **36,732** | **better** |
| new | 7 | 13 | 4,647 | 103,175 | worse |

The old card read all three itself and found the cross-cutting fact:
*"unnaturally flat at 40 requests per hour from 00:00 through 09:59,
then disappears entirely… same flat pattern in beta."* True — the
fixture has 400 lines over hours 0–9 in every file.

The delegated run said alpha was *"busiest around 00:00 and quietest
around 23:00"* — there is no 23:00 traffic — and called beta's
ordering *"sawtooth"*.

**The reason is structural.** A helper sees its part and nothing else,
so an answer that turns on comparing the parts is one it cannot give
and the parent can no longer reach. Three logs read by three helpers
come back as three summaries, and the shared shape is gone.
Delegation partitioned away the thing the question asked about — at
three times the cost. It also spawned seven helpers for three
services.

## What the card now says

The encouragement was one-sided: it said when to reach for a helper
and never when not to. Added:

> **But split the work, not the question.** A helper sees its own part
> and nothing else… Delegate when the parts are genuinely separate.
> When the answer is the pattern across them, read them yourself — and
> if a tool can filter them first, that is cheaper than either.

## The method, which is the transferable part

Two live sessions, same words, cards differing. That is a cleaner
instrument than the task suite for this question: it isolates the card,
it costs four completions rather than five runs, and steering turn by
turn asks "what does it take" directly instead of inferring it from
aggregate counts.

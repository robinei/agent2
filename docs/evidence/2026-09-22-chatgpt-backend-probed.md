# What the ChatGPT backend actually does

Probed with a live subscription token, 2026-09-22, after `agent login`.
Everything below is measured, not read from documentation.

## Reaching it

- **Endpoint:** `https://chatgpt.com/backend-api/codex/responses`.
  `/responses` and `/codex/v1/responses` return an HTML 403; the right
  path returns a *semantic* 400, which is how it was found.
- **Models a subscription can reach:** `gpt-5.6-sol`, `gpt-6-astra`,
  `gpt-5.5`. Not `gpt-5.3-codex-spark`, not `gpt-5.1-codex` — both
  refused with "not supported when using Codex with a ChatGPT account".
  The codex-branded models are not what a subscription buys.
- **`store` must be `false`.** `true` is rejected outright, which
  happens to match what this harness wanted anyway: the log is the only
  state.
- The wire format is standard Responses SSE. `OpenAiResponses` needed
  **no changes** — event names and the usage shape matched what was
  written blind against the public API.

## Reasoning: the amount is measurable, the content is not

`reasoning_tokens` is reported and tracks effort: 31 at `low`, 54 at
`medium`, 64 at `high` on the same prompt.

**Reasoning text is only returned if asked for.** Without
`reasoning.summary` the stream carries no reasoning events at all and
`Part::Thinking` is empty. With it, four event types arrive, including
`response.reasoning_summary_text.delta`. So the client now sends
`summary: "auto"` by default (`AGENT2_REASONING_SUMMARY=off` opts out)
— otherwise the log silently stops being the corpus it is meant to be.

**But a summary is a headline.** A real run's whole reasoning came back
as 39 bytes:

> `**Planning line count with bash wc -l**`

So the drafting metric — fenced ```js blocks counted inside
`Part::Thinking`, the measurement this project ran on all week — **does
not exist on this model.** There is nothing in a 39-byte title for a
regex to find, and a paraphrase would not be the same measurement even
if it were longer. What survives: `reasoning_tokens`, programs per
task, calls per program, pass rate, output tokens, and whatever
bookkeeping shows up in prose.

## Caching: rare and partial

Seven identical requests at ~3.7k tokens, then six at ~7.3k, with and
without `prompt_cache_key`:

| | cached |
|---|---|
| no key, repeated | 0 every time |
| echoing the key the server assigns | 0 every time |
| our own stable key, 6 back-to-back | **1 of 6**, at 3,456 tokens (47%) |

The server assigns a fresh `prompt_cache_key` UUID on every response
regardless of what is sent, and advertises
`prompt_cache_retention: "24h"`. When a hit does land it is the same
3,456 tokens both times, which looks like one cached block rather than
a prefix match.

Against the completions path — a reliable ~60% median across 42 runs —
**this backend effectively charges full prompt tokens on every
request.** On a subscription that is quota rather than money, but it
decides what a long eval costs.

## What this means for measuring here

Do not port the drafting comparisons to this model; they cannot be run.
The questions that *can* be asked are the code-mode ones: does a
frontier model write bigger programs (`calls_per_program`), does it
need the card's failure-mode prose at all, and does it pay the
harness-bookkeeping tax that cost flash 28 lines a run against pi's
zero. None of those need reasoning text.

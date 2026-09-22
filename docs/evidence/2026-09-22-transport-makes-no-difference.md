# The transport is not what makes it draft

`run_program` against the notebook, `sweep-8`, `deepseek-v4-flash`,
five runs an arm interleaved. Same binary, same card but for the
paragraphs naming the transport, exemplars on both sides (rendered as
fences or as calls by `Document::into_tool_calls`).

| | passed | programs | calls/prog | drafts | thinking | out tok | traps | wall |
|---|---|---|---|---|---|---|---|---|
| notebook | 5/5 | 2.0 | 4.0 | 1.0 | 9.7 KB | 3,267 | 1 | 77 s |
| `run_program` | 4/4 | 2.0 | 4.8 | 1.0 | 9.7 KB | 3,461 | 1 | 100 s |

drafts p = 0.70, thinking p = 0.90, tokens p = 0.90, programs p = 1.00,
wall p = 0.27. **Nothing.**

## What that settles

**Drafting is not caused by the notebook shape.** That was the
standing hypothesis from the pi comparison — pi drafted 0 times in ~30
replies against agent2's median 5 — and the obvious explanation was
that composing a whole program invites rehearsal where a single call
does not. Delivered as a call, on the same harness with the same card,
the model drafts exactly as much. So pi's advantage is in its prompt or
its harness, not in code mode versus tool calls, and that is where the
next look belongs.

**The round trips survive too.** 2 programs either way, 4.0 against 4.8
calls per program. A call carrying a whole program batches as well as a
reply carrying one, which is the property that mattered.

## The one real cost, and it is structural

The single `NO RUN` was an HTTP 400:

> The `reasoning_content` in the thinking mode must be passed back to
> the API.

On this provider, an assistant turn carrying `tool_calls` must carry
its reasoning back with it. The notebook transport never does: `content`
is the whole turn, `document.rs` drops `Part::Thinking`, and thinking
cannot re-enter the context. Under the call transport it would have to.

That is not a bug to fix so much as a property to weigh. Re-sending
reasoning grows every subsequent request, and it **restores the
compounding route that the notebook removes** — a turn that drafted
fifteen blocks would carry all fifteen into the next turn's context.
Everything measured on 2026-09-21/22 about drafting-per-reply assumed
that route did not exist.

## Three bugs, and what each needed to be caught

This arm was measured three times before it was measurable, and the
pattern is worth keeping:

1. **History rendered as fences.** The model called `run_program` and
   its own past turns came back as ```js blocks. Caught by reading a
   rendered document, not by a test.
2. **A half-transformed card.** Twelve paragraphs still described
   blocks, cells and "this reply", and the batching argument had been
   dropped entirely. Caught by reading the card, not by a test.
3. **The program never reached the notebook.** The session feeds the
   notebook from `chunk` as deltas arrive and never reads
   `LlmTurn.source`; the program was assembled only into the return
   value. 0/5 three times, looking each time like the model refusing to
   call.

**And the lab hid the third one.** `agent sample` reads the return
value, so it scored the very same captured document 12/12 on emitting
a call while the live arm scored 0/5. An instrument that exercises a
path the harness does not use will confirm whatever it is asked.
`the_chunks_add_up_to_the_turn` now asserts the contract for both
transports.

## Confirmed, on a run with nothing broken in it

The run above still carried two faults (the exemplars' missing
`reasoning_content`, and `tool_calls[0]` read without its `index`).
Repeated with all five fixed and the call card under test — 5 runs an
arm, 10 of 10 passing, no `NO RUN`:

| | passed | programs | calls/prog | drafts | thinking | prompt tok | wall |
|---|---|---|---|---|---|---|---|
| notebook | 5/5 | 3.0 | 4.3 | 1.0 | 9.8 KB | 20,536 | 36 s |
| `run_program` | 5/5 | 3.0 | 5.5 | 3.0 | 9.0 KB | 23,134 | 19 s |

drafts **p = 1.00**, programs **p = 1.00**, thinking p = 0.68, output
tokens p = 0.40. The same answer as before, now from a run that
deserves it.

**The one difference is the wire cost, and it is against the call
transport.** 23,134 prompt tokens against 20,536, p = 0.060 — about
13% more, because this provider requires `reasoning_content` back with
every `tool_calls` turn. The notebook sends none. That is the whole
measurable difference between the two shapes on this model: the call
transport does the same work, equally well, for more tokens.

## Five bugs, and which kind of thing caught each

| bug | how it presented | what caught it |
|---|---|---|
| history rendered as fences | card contradicted by the model's own turns | reading a rendered document |
| card half-transformed | 5 identical task failures | reading the card |
| program never reached the notebook | 0/5, looked like refusing to call | comparing lab (12/12) against live (0/5) |
| exemplars had no reasoning | HTTP 400, 3 runs in 5 | the error message, which named `exemplar` |
| `tool_calls[0]` read without `index` | nothing yet — two calls would have been silently lost | an audit before re-measuring |

Three of the five were invisible to the suite and needed someone to
look at what the model was handed. Two are now pinned:
`the_chunks_add_up_to_the_turn` and
`the_call_card_says_nothing_about_blocks_or_cells`.

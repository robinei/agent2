# 28 — One reply

The log records the pieces of a reply and not the reply. Everything
that reads it re-infers the structure, and each reader does it slightly
differently. This phase records the structure instead.

## The defect, measured

A reply is the unit the architecture turns on (D1: "a reply is a run of
events"). It is not an event. What is persisted is N × `Turn`, then
`Completion` — **logged last**, after the cells it contains — then
`Return`/`Condition`, then `Console`. Five places pay for the gap:

| | |
|---|---|
| `document.rs::notebook_replies` + `record_reply` | 113 lines whose whole job is re-deriving the grouping, with three documented cases where they give up |
| `Turn.source` | 100% contained in `Completion.text` — measured 2026-09-19 across the checkpoint logs, 4,622 of 4,622 bytes. Every cell stored twice |
| `Call::site` | made *cell-local* at log time, then un-localised at render time by `rebase_site` and a `Cut`-shifting loop |
| `report.rs::handback` | finds a run's console by "the `Console` after this outcome and before the next" — positional |
| `outcomes_of_turn` | walks forward to the next `Turn` — positional |

None of it is bad code. It exists because the writer knew the structure
and did not record it.

Three further facts that this phase resolves as a consequence rather
than as separate work:

- A completion that never arrives (an HTTP 530 from the provider) leaves
  **no trace that a generation was attempted** — three events, `Agent`
  and `Post`, and nothing else. On 2026-09-19 that cost an hour of
  diagnosis.
- A reply cut off mid-stream is discarded unread; the model never sees
  what it wrote and gets a sentence about a token budget instead.
- `Answer` is in `is_outcome`, so it terminates a turn — but a program
  can answer and keep going, and one of our own exemplars does:
  `answer(9, "made a helper"); history.append(g.agent);` logs `Answer`,
  `Note`, `Return`. Two outcomes for one turn, against the rule that
  every turn has exactly one.

## The shape

A reply is a run of **parts** that concatenate, byte for byte, to the
completion that produced them. What was seen and what was executed are
both on the log, in the order they happened, with their effects between
them.

```rust
enum EventPayload {
    // structure
    Agent    { name, charter, system, exemplars },
    Fork     { name },
    Rename   { .. },
    Compacted{ of, text },

    // what was said to the branch
    Post     { from, origin },

    // what the branch replied
    Reply,
    Part     { reply, part: Thinking | Prose | Cell },
    ReplyEnd { reply, how: Finished | Truncated | Interrupted | Failed(String), usage },
    Restart  { source },

    // what running it did
    Call     { reply, call },
    Result   { reply, call, outcome },
    Note     { reply, text, site, site_end },
    Answer   { reply, question, value },
    Console  { reply, lines },
    Handback { reply, how, site, stack },
}

enum Handback {
    Raised { name, payload },       // the reply pauses; another one follows
    Trapped{ message, resumable },
    Posted { ids },
    CellFailed { message },
    Completed,                      // the reply is done
    Abandoned,
    Interrupted,
}
```

Thirteen payload variants become fifteen, and four parallel
vocabularies — `Message`, `Cause`, `Disposition`, `is_outcome` — become
one.

### A log

```
 1  Agent    { "un-skip the tests that pass" }
 2  Post     { User, "some tests are skipped…" }

 3  Reply
 4    Part   { 3, Thinking("two look like they'd pass now…") }
 5    Part   { 3, Prose("I'll read both files first.\n\n") }
 6    Part   { 3, Cell("```js\nconst t = await tools.read_file(…);\n```\n") }
 7    Call   { 3, read_file, site: 372 }
 8    Result { 3, of: 7 }
 9    Part   { 3, Prose("\nTwo pass. Which should I keep?\n\n") }
10    Part   { 3, Cell("```js\nconst v = raise(\"which\", …);\n```\n") }
11    Handback { 3, Raised{..}, site: 618 }     // pauses; the text keeps coming
12    Part   { 3, Prose("Then I'll run the suite.\n\n") }
13    Part   { 3, Cell("```js\nawait tools.replace_file(…);\n```\n") }
14  ReplyEnd { 3, Finished, usage }

15  Reply                                        // the handler
16    Part   { 15, Cell("```js\nhistory.append(resume(\"the flux ones\"));\n```\n") }
17  ReplyEnd { 15, Finished, usage }
18    Handback { 15, Completed }                 // the decision lands; 3 resumes

19    Call   { 3, replace_file }                 // reply 3's tail, after 15 ended
20    Result { 3, of: 19 }
21    Console{ 3, lines }
22    Handback { 3, Completed }
```

Lines 19–22 are why effects carry `reply`. A reply's **parts** cannot
interleave with another's — the provider streams one at a time per
branch — but its **effects** can, because a raise is answered by a whole
other reply and then the first one resumes. This is the single piece of
deliberate redundancy in the design and it exists for that case.

### Where the old vocabulary went

| today | here |
|---|---|
| `Message::Turn { source, author, usage, thinking }` | `Part::Cell` · author gone · usage → `ReplyEnd` · thinking → `Part::Thinking` |
| `Completion { text, usage, thinking }` | the parts themselves · `ReplyEnd.usage` |
| `Return { value }` | `Handback::Completed` — the value was null 68/68 |
| `Condition{Raised\|Trapped\|Posted}` | `Handback` — **pauses**, not outcomes |
| `Condition{Abandoned\|Interrupted}` | `Handback` — terminal |
| `Condition{CompileFailed}` | `Handback::CellFailed` |
| `Condition{Truncated}` | `ReplyEnd::Truncated` — a fact about text, not about a program |
| `Condition{Compaction}` | **not an event**; a directive, already excluded from the document |
| `Disposition::{Pushed,Handover}` | derived: non-terminal vs terminal `how` |
| `Answer` as an outcome | an ordinary effect |
| `Message` wrapper | gone — only `Post` was left |

`Call::Fork` and the `Fork` payload both stay: the first is on the
parent's path, the second roots the child's.

### Author

There is none. A `Reply` is by definition the branch's LLM, and the
branch identifies the agent (`enclosing_agent`, resolved once — "the
leaf moves, the agent does not"). `Message::Turn.author` had two values
and they never distinguished two authors; they distinguished **whether
the thing was a completion at all**, which is why `score.rs` only ever
read the field to exclude the other case:

> `Message::Turn` events authored by an agent (never the user …) — one
> per LLM completion the run actually used.

A person taking the branch's turn is `Restart` — a different event,
because none of the completion vocabulary applies to it: it does not
stream, cannot be truncated, has no usage and no reasoning. Two
consequences worth having: `round_trips` counts `Reply` events and can
no longer miscount, and `take_turn`'s fence-wrapping disappears — a
restart *is* a cell, so nothing has to parse it back out of markdown.

### Sites

One buffer: the prelude, then the reply with prose blanked to newlines
and cells verbatim. A `site` is an offset into it. Subtract
`ReplCore::source_base()` once at render time and it is an offset into
the reply, so a diagnostic names a line the model can count to in what
it just wrote.

This is *less* machinery than today, not more: today we blank **and**
rebase. Cell-local sites, `rebase_site` and the `Cut`-shifting loop all
go. Reply-absolute line numbers are also the more intuitive of the two —
the model's mental object is the reply, not the cell, and today's "line
3" requires it to first work out which cell, which the report does not
say.

### Cancellation

A suspension currently cancels the generation, on the rule "a trap or a
`raise` should; `done()` should not". For a raise that contradicts the
card — *"The blocks after this one do not run until it is answered"* —
and worse, it makes the semantics **depend on provider speed**: if the
later fences had already streamed they run, and if not they were never
written. Same reply, same model, different behaviour.

The rule becomes one sentence: **cancel when the text that follows was
written on a premise we now know is false.**

- `Trapped`, `CellFailed` → cancel. Everything after assumed the cell
  succeeded.
- `Raised`, `Posted` → keep streaming. The model knew it was asking; a
  message arriving falsifies nothing it wrote.

`ReplyEnd.how` records which happened either way, and the replayed
assistant turn ends with a marker saying so, so a reply that stops early
says why instead of trailing off.

### Derived, not stored

- a cell's offset in the reply — sum of the preceding parts' lengths
- the assistant turn — concatenate `Prose` + `Cell`, skip `Thinking`,
  append a marker when `ReplyEnd.how != Finished`
- which cell a `site` falls in — one fold over the parts
- scope depth — non-terminal handbacks open, terminal ones close
- the report — `derive_report`, unchanged in spirit

**The invariant everything rests on:** `parts.concat() == the completion
text`. It needs a test, and `notebook.rs::prose_between`'s `.trim()` has
to go first or it is false on day one.

## Migration

None. New logs. The corpus is eval artifacts under `/tmp`; a
compatibility shim across this shape would cost more than it could ever
repay. The log's own `{"version":1}` becomes `2` and an older one is
refused with a sentence saying why.

---

## Steps

Each step ends green — `cargo test` passes and `cargo build` is
warning-free — before the next begins. A step that cannot end green is
reported, not worked around.

### Step A — the vocabulary

`types.rs` only, plus whatever mechanical changes the compiler demands.

- [ ] `EventPayload` as above. `Message` collapses to `Post`.
- [ ] `Handback` replaces `Cause` + `Return` + `Disposition`.
- [ ] `Tree::open` refuses `version < 2` with a sentence naming this doc.
- [ ] `cargo build` clean, `cargo test` green.

**Acceptance:** `grep -c 'Message::Turn\|EventPayload::Return\|Disposition' agent/src/` is 0.

### Step B — writing parts

`machine.rs` and `notebook.rs`.

- [ ] `prose_between` stops trimming; a test asserts
      `parts.concat() == reply` over a reply with leading, trailing and
      inter-block whitespace.
- [ ] `advance_notebook` logs `Part::Prose` / `Part::Cell` instead of
      `Send{prose}` / `Turn`.
- [ ] Thinking chunks log `Part::Thinking`.
- [ ] Sites are reply-absolute; `rebase_site` deleted.
- [ ] `ReplyEnd` carries `how` and `usage`; `finish_notebook_generation`
      writes it.
- [ ] `Handback` replaces the `Return`/`Condition` writes.
- [ ] Effects carry `reply`.

**Acceptance:** a live single run's log, read by eye, is the shape in
"A log" above.

### Step C — reading parts

`document.rs`.

- [ ] `notebook_replies`, `record_reply`, `covered` and the cut-shifting
      loop are deleted.
- [ ] The assistant turn is the concatenation, with the marker.
- [ ] `report.rs` derives from `Handback`; the console is found by
      `reply`, not by position.

**Acceptance:** `agent document` on a fresh log renders the reply
verbatim, and `grep -c 'notebook_replies\|record_reply' agent/src/` is 0.

### Step D — the followers

`tree.rs`, `score.rs`, `compaction.rs`, `debug/`.

- [ ] `programs_for` reads parts.
- [ ] `score`'s `round_trips` counts `Reply`.
- [ ] Compaction targets a reply by its `Reply` id.

**Acceptance:** full suite green, zero warnings, and one live run per
task that completes as before.

### Step E — the cancellation rule

- [ ] `notebook_cancels_generation` splits by falsified-premise.
- [ ] The card's `raise` sentence is true again.
- [ ] The truncation/interruption marker reaches the model.

**Acceptance:** a scripted test where a raise is answered and the cells
after it run.

# Phase 12 — Budgeted answers: let reading and summarizing just work

The attached-mode TUI (phase 11) made one session legible enough to see a
whole class of failure: *"use sub agents to explore this repository."* The
agents thrashed — ~457 program runs across 16 frames, and **28 `create_file`
calls writing into `/tmp`** during a read-only task — stashing file content
in line-range chunks (`mod_1_140.txt`, `mod_140_200.txt`) and reading it
back a slice at a time, then "reconstructing [a file] from the artifact
data" (i.e. fabricating it). None of the artifact machinery (`tool_result`
by id) was used as designed; the models routed around it with the most
universal intuition available — write a file, read it back — and did it
badly.

The root cause is a DESIGN-level gap, not a tuning knob: **there is no
channel for content to enter an LLM's context on purpose.** Every outbound
path is clipped to status sizes — `return` rejected past 4 KB
(`PROGRAM_RESULT_MAX_BYTES`), shown at 1 KB (`VALUE_MAX_BYTES`), previewed
at 80 B (`PREVIEW_MAX_BYTES`). `tools.tool_result(id)` fetches the full
artifact, but only into a *program*, never into context. So a leaf agent
asked to summarize a file literally cannot get the file's bytes into its
own reasoning. The `/tmp` chunking and the hallucinations are the model
behaving *rationally* under an impossible constraint.

The fix, stated as the north star now does (DESIGN.md, "The one
exception"): the spine keeps the LLM out of the *data* path, except for the
one value that is its own deliverable — the **answer**. Make the answer a
**budgeted** channel into context, generous enough that the obvious path
("return what you want to read") just works, failing safe automatically
when an answer is genuinely oversized. Crucially, **the model never opts
into any of this.** The lesson of the session is that machinery the model
must wield gets fumbled; so this phase adds almost nothing the model sees —
one generous number and a fail-safe truncation that mirrors what already
exists.

**Sequencing:** Step 1 is the load-bearing change and unblocks the task on
its own; Step 2 extends it to subagent answers; Step 3 is the optional
per-call override; Step 4 is the card/report copy that stops steering
models into `/tmp`; Step 5 is the docs sweep (DESIGN.md already amended).

## Locked design decisions

1. **The answer is the only value that crosses into a mind.** Everything
   else — tool output, intermediate program values — stays machine-bound:
   in variables and the log, fetchable by id, never re-sent in context.
   Mind-bound data is exactly a frame's deliverable: a program's `return`
   (into its own frame's context) and a subagent's final turn (into its
   caller's). Both are budgeted; nothing else changes size.

2. **A generous default budget, so the obvious path works without the model
   knowing it exists.** `DEFAULT_ANSWER_BUDGET` is sized to a typical source
   file (default **64 KiB**), so a single-file read or a normal summary
   lands in one shot. If models hit it on ordinary work, **the number is
   wrong, not the model.** The budget is the only model-visible artifact of
   this phase, and even it is invisible on the happy path.

3. **Over-budget fails safe automatically — it never rejects, and the model
   makes no decision.** The full value is *always* logged as a fetchable
   artifact (today's `ProgramResult` / the child's Assistant message on the
   spine). The *context copy* is truncated to the budget with a marker that
   names the fetch id (`… [+N B — tools.tool_result(#id)]`). No "digest vs
   product" fork, no error string, no lost work. This replaces the current
   loud rejection at `machine.rs:855`.

4. **A subagent's answer is bounded identically, with one quiet retry.** Its
   final prose is the deliverable into its caller; if it exceeds the
   budget, the host re-prompts the child *once* (the existing
   loud-refusal→retry shape, moved to the frame-terminal turn), then
   hard-falls-back to truncate-with-note. The full prose remains on the
   child's spine in the log regardless (no fidelity lost).

5. **Large *products* use `create_file` — the by-reference channel models
   already reach for.** A model asked to produce a big artifact writes a
   file and reports its path; that is the obvious, correct path and needs no
   new primitive. We deliberately do **not** add an `emit`/transclusion
   mechanism: it is ceremony for a case the filesystem already covers, and
   (decision 2's principle) the model would have to know it exists.

6. **A caller may raise a child's budget in the request.** `agent({ prompt,
   input, budget? })` seeds the child's `answer_budget`; absent, the default
   applies. This is the one knob, and it is the *caller's* to set (only the
   caller knows whether it wants a 2 KB digest or a 60 KB dump) — never a
   classification the producer must make.

## Step 1 — generous budget + fail-safe truncation on returns (`machine.rs`, `report.rs`)

Make a program's `return` a budgeted channel into its own frame's context
(decisions 2–3).

- **`machine.rs`**: replace `PROGRAM_RESULT_MAX_BYTES = 4096` (`:96`) with
  `DEFAULT_ANSWER_BUDGET = 64 * 1024`, and add `answer_budget: usize` to
  `AgentState` (seeded from the default in `new_root`/`new_child`). In
  `finish_program` (`:855–869`) **delete the `return_rejected` branch**: do
  not substitute an error, do not discard the computed value. Always
  `tree.append` the full `value_json` as the `ProgramResult` (the `:892`
  append, unchanged) and pass `answer_budget` to the `CompletionReport`.
- **`report.rs`**: `CompletionReport` gains `budget: usize`; `render` (`:119`)
  clips `returned:` to `budget`, not `VALUE_MAX_BYTES`. When the value is
  clipped, the marker names the `ProgramResult`'s id (locate it in
  `new_artifacts`, which already includes this run's `ProgramResult`):
  `… [+N B — tools.tool_result(#id)]`. Demote `VALUE_MAX_BYTES` to the
  default-budget constant or remove it.
- Re-point the oversized-return test (`machine.rs:~1930`) from "rejected
  with `status-shaped, not data`" to "delivered up to budget; full value
  logged as a fetchable `ProgramResult`; marker names its id".

Acceptance:

- [x] A program that `return`s a (sub-budget) string lands that string in
      the completion report's `returned:` in full, the full value is a
      fetchable `ProgramResult`, and nothing is rejected
      (`program_return_is_delivered_up_to_budget`).
- [x] A `return` over budget is truncated in context with a marker naming
      a real, fetchable id; the full value is logged for fetch
      (`over_budget_return_truncates_with_fetch_id`).
- [x] `PROGRAM_RESULT_MAX_BYTES` / the reject branch are gone; no error
      string is ever substituted for an oversized return.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green (144 + 707).

## Step 2 — bound the subagent answer the same way (`machine.rs`)

A subagent's final prose is its deliverable into the caller (decision 4).

- Add `answer_retries: u8` to `AgentState`. In `finish_frame` (`:1010`),
  before logging `FrameResult`, measure the result string. If it exceeds
  `answer_budget`:
  - **retries < 1**: increment, push a `Message::System` nudge
    (`"Your answer is {len} B; budget {budget}. Tighten it, or write a large
    product with create_file and report its path."`) onto the spine, and
    return `vec![self.render_request()]` — another LLM turn, the same shape
    as a normal re-prompt.
  - **otherwise**: deliver `head(answer_budget)` + a short truncation note
    as the `FrameResult`. The full prose is already on the child's spine as
    the final `Assistant` message (logged), so nothing is lost for
    debugging/TUI; only the *delivered* value is bounded.
- The root branch (`:1014`, the conversation that yields to the user) is
  bounded the same way — the user-facing answer respects the budget, with
  oversize products expected to be files on disk.

Acceptance:

- [x] A subagent whose final answer exceeds the budget is re-prompted
      exactly once; a second overflow truncates-with-note and the frame
      completes (`over_budget_subagent_answer_reprompts_then_truncates`).
- [x] The full oversized prose remains on the child's spine in the log even
      when the delivered `FrameResult` is truncated (asserted in the same
      test).
- [x] A subagent answer within budget delivers verbatim (no retry, no
      truncation) — existing direct-child test (`"child says hi"`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 3 — per-agent budget override (`machine.rs`, `host/tools.rs`)

The caller sets a child's budget (decision 6).

- **`host/tools.rs`**: add optional `budget` (integer bytes) to the `agent`
  tool's arg schema, documented as "max bytes of the subagent's answer
  delivered into your context; default {DEFAULT_ANSWER_BUDGET}".
- **`machine.rs`**: in the `"agent"` spawn arm (`:731–757`) read
  `arg.get("budget")`; carry it on `SpawnFrame` (new `budget:
  Option<usize>` field, `:161`). `new_child` seeds `answer_budget` from it,
  falling back to `DEFAULT_ANSWER_BUDGET`.

Acceptance:

- [x] `agent({prompt, input, budget})` threads through `SpawnFrame` →
      `new_child` → the child's `answer_budget`; omitting `budget` uses
      `DEFAULT_ANSWER_BUDGET` (`new_child` seeding + spawn-arm parse).
- [x] The budget is the child's `answer_budget` for both its `return`s
      (Step 1) and its final answer (Step 2) — one field, both sites.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 4 — card + report copy (`host/dialect.rs`, `report.rs`)

Stop steering models into `/tmp`; state the budgeted-answer model
(decisions 2, 5). This is the change that most directly fixes the observed
behavior.

- **`host/dialect.rs`**: rewrite the "status-result discipline" section
  (`:76–80`). Replace "`return` … only small, status-shaped values.
  Oversized returns are rejected." with, in substance:
  > Tool results are large and live in variables and the log, not your
  > context — keep them there. But to bring content into your *reasoning*,
  > `return` it: returns are budgeted (default ~64 KB; a caller may raise a
  > subagent's via `agent(task, {budget})`), delivered into your context up
  > to that budget, with the full value always fetchable by id. Never
  > `bash sed`/`tool_result` content into `/tmp` to read it back in slices.
  > To digest many large files, spawn one `agent` per file — each *returns
  > its summary*. If your answer is a large *product* (a verbatim file, a
  > full report), `create_file` it and report the path; don't try to shrink
  > it.
- **`report.rs`**: the `advise_attachments` note (`:134`) stays (separate
  concern — inlining bodies into `source`). Remove the
  `status-shaped, not data` assertion in the affected test
  (`machine.rs:1943`).

Acceptance:

- [x] The card's `status-result discipline` section is replaced by
      `answers and results`: returns are budgeted (not rejected), reads
      come into context via `return`, large products via `create_file`,
      and `/tmp` chunking is called out as wrong.
- [x] No production string says "status-shaped, not data"; the card test
      needles updated (`returns are budgeted`, `answers and results`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 5 — docs sweep

- [x] DESIGN.md: new section "The one exception: the answer crosses into
      context"; roadmap range updated to `0_…`–`12_…`.
- [x] 8_HARNESS.md: the report's returned-value bullet now notes it is
      budget-bounded (not `VALUE_MAX_BYTES`-clipped), truncates-with-id
      rather than rejecting, cross-referencing this phase.
- [x] 11_INTROSPECT.md: no change needed — it does not reference
      `PROGRAM_RESULT_MAX_BYTES` (it keyed program blocks by `EventId`,
      untouched here).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## What this phase deliberately does *not* add

- No `emit`/transclusion primitive (decision 5) — `create_file` is the
  product channel models already use.
- No "digest vs product" classification the model must make (decision 3) —
  overflow fails safe without a model decision.
- No separate cumulative per-frame context ceiling — a parent absorbs N
  child answers as *values in its program* (machine-bound, free) and pays
  context only on its own budgeted answer, so the single per-answer budget
  suffices.
- No change to the artifact-by-id machinery for *tool* output — that bulk
  *should* stay by-reference; only the deliverable became a budgeted
  context channel.

# Phase 11 — Introspectable program blocks + frame navigator

Make the attached-mode TUI (9_TUI Step 4) legible while developing the
system. Three changes, one theme — *see what actually happened*:

1. **Program executions render as blocks within the transcript.** Chat
   stays a per-frame transcript of messages — system, user, assistant
   prose — but a `run_program` execution, instead of a bare tool-call
   line with its inner `Invoke`s dropped and a verbose report body
   inlined, now renders as one block: a `run_program: <status>` header
   with the consecutive inner tool calls listed beneath it.
2. **Two selection axes, both clickable.** A persistent top-right
   **frames** pane (root pinned, every frame, live status) selects which
   frame you're looking at; clicking a chat block selects which *program*
   the right-hand panes show — **including older, finished programs**.
3. **The system prompt is logged and shown.** The assembled dialect card
   + prompt + input is stored on the spine and rendered as a collapsible
   block at the top of each frame's transcript, so the log faithfully
   records the system prompt as it was at that point in time (cards and
   prompts evolve).

The motivation is the same as 9_TUI's: the TUI is the observation
instrument for iterating the harness. Today it shows the *last* program's
VM and hides the inner tool calls; this phase makes every program's
shape and every frame's clean-room context directly visible and
navigable.

**Sequencing:** Step 0 is an independent cleanup, land it first. The
rest build on 9_TUI Step 4 (attached mode): Step 1 (retention) and Step 2
(events) are host changes; Steps 3–5 are the TUI; Step 6 is docs.

**Superseded by 17_BRANCHES Part D.** This phase's whole selection axis was
`FrameId` (one row per agent); it is `BranchId` now (one row per branch, a
fork and its original both addressable, nested by `parent_branch`), and the
**frames pane** this phase built is the **branch navigator**. Three specific
claims below no longer hold, flagged where they're made: decision 4's stored
`Message::System` chat event does not exist — 17_BRANCHES A3 replaced it
with `system` as a field on the `Agent` event itself, rebuilt into the
request by `render_request` rather than replayed as a logged message;
`FrameStart`/`FrameResult` (decisions 7–8) are `Agent`/`Fork` roots and
outcome events (`Return`/`Condition`/`Answer`) — agents never close, so
there is no terminal event to scan for; and `AgentState` is `Runner`. The
*shape* of what this phase built — program blocks, the system-prompt block,
click-to-select two axes, log-only reconstruction of finished programs — is
still exactly right and carried forward unchanged; only the keying and the
system-prompt storage mechanism moved. Left below as the record of what
Phase 11 actually built, under the vocabulary of its time.

## Step 0 — cleanup: drop the vestigial chunk payloads

`EventPayload::TextChunk`/`ThinkingChunk` are dead. Streaming flows
entirely through `SessionEvent::Chunk` (`LlmChunk` → `LoopMsg::LlmChunk`
→ `SessionEvent::Chunk`); these two `EventPayload` variants are never
constructed in production — the only `append` of one is a `tree.rs` test
asserting `is_storable()` rejects it. Removing them sharpens decision 5's
boundary: **live-only signals are `SessionEvent` variants; `EventPayload`
is the stored-in-log vocabulary** (the very rule Step 2's `ProgramStatus`
follows). They are also the *only* payloads `is_storable()` returns
`false` for.

- Delete `EventPayload::TextChunk`/`ThinkingChunk` (`types.rs`).
- Delete `EventPayload::is_storable` and the `assert!(payload.is_storable())`
  guard in `Tree::append` (`tree.rs:102`); every payload is now storable.
- Remove the no-op match arms (`main.rs`, `chat.rs`, `machine.rs`,
  `host/mod.rs`) and the non-storable `tree.rs` test (premise gone).

Acceptance:

- [ ] `TextChunk`/`ThinkingChunk`/`is_storable` gone from the public API;
      streaming still renders (the `SessionEvent::Chunk` path is
      untouched).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Locked design decisions

1. **Two selection axes.** `AttachedApp` gains `selected_program:
   Option<EventId>` alongside `selected` (the frame). The right-hand
   panes borrow the VM / record for `selected_program`; `selected`
   drives the chat focus and the default program (a frame's latest).
   Today both are conflated into `selected: Option<FrameId>` and only
   the single most-recent VM per frame (`AgentState::last_vm`) is
   reachable.

2. **A program block is one `run_program` execution.** Identified by the
   **`run_program` Assistant message's `EventId`** — the cross-boundary
   key (chat sees it in `SessionEvent`; the host keys records by it). A
   `resume` folds into the *same* block as its originating `run_program`
   (one VM, one record): status moves `running → suspended → running →
   completed`. The block opens on the `Assistant` tool call, accumulates
   the frame's `EventPayload::Invoke` events (currently dropped by
   `chat.rs`), and closes on the run's `Tool` result.

3. **Old programs keep source + console + result — reconstructed from
   the log, not retained.** Live introspection (disasm/stack/promises)
   needs the VM and only makes sense for the running program. Everything
   a finished program's panes need is in the log: source (the
   `run_program` call args), result (`ProgramResult`), and **console**
   (logged structurally at program end — see decision 8). So finished
   programs need *no* host-side retention; they are a projection over the
   event tree. Only the running program uses the live VM (today's
   `last_vm` is at most a live-session convenience for the just-finished
   one).

4. **The system prompt is a stored `Message::System`, materialized
   once.** Logged as `EventPayload::Message(Message::System { text })`
   — the first message on a frame's spine — holding the exact assembled
   string (`render_request`'s `dialect_card` + `prompt` + `input`). It
   is a chat event by the `EventPayload` taxonomy ("renders into LLM
   requests for their frame"), so it rides the normal `Event` path into
   the log and replays verbatim. `render_request` stops synthesizing and
   sends `frame.messages` as-is. Consequence: a conversation's system
   prompt is **fixed for its lifetime**; a *new* frame/session picks up
   the evolved card. This captures card evolution at the right
   granularity and removes any "logged vs sent" drift.

5. **Status is its own live `SessionEvent`.** `chat.rs` is VM-free and
   cannot see `Phase`; inferring suspended-vs-failed from report text is
   fragile. Add `SessionEvent::ProgramStatus { frame, program: EventId,
   status }` (live-only, like `Chunk`), emitted by the host on each phase
   transition. It carries the `program` id that links the clickable block
   to the record. (The `SystemPrompt` change, by contrast, rides the
   *stored* `Event` path — decision 4 — because we want it in the log.)

6. **Chat stays `SessionEvent`-only and becomes per-frame.** The
   structural guarantee from 9_TUI (ChatState's fields private, sole
   mutator `apply(&SessionEvent)`) is preserved. `rows()` partitions
   items by frame and renders the *selected* frame's slice (replacing
   the root-pinned `[frame N]` interleave), so clicking a frame switches
   the chat too — and you see each subagent's own clean-room prompt.

8. **Everything but live VM internals reconstructs from the log.** On
   resume (`open_at`) only the chosen spine is re-instantiated, so the UI
   reads from the `Tree`, not from live in-memory `AgentState`s: the
   **frames pane** is a projection over `FrameStart`/`FrameResult` (full
   frame set + hierarchy + last status) and the **program list** is a
   projection over each spine's `run_program`/`resume` Assistant calls,
   their `Invoke`s, `ProgramResult`s, and `Console` events. Live
   `AgentState`s only overlay "currently running" status and supply the
   running VM. The one gap — the completion/condition report clips
   console to a tail (`report.rs::render_console`) — is closed by logging
   full console structurally at program end as `EventPayload::Console
   { lines }` (storable, never sent to the LLM), covering all three
   terminals (success, suspend, abandon). The only things *not*
   reconstructible are the live VM internals: stack, disasm, promises,
   `ip`.

7. **Frames pane is always visible, top-right, clickable.** Root pinned
   at the top, every frame this session (running/suspended/idle/done),
   live status markers. Promoted out of the Running/FullDebug right
   column into a persistent pane in all views. Mouse left-click is added
   (`on_mouse` handles scroll only today): a click in the frames pane
   retargets the frame; a click on a chat block retargets the program;
   a click on a collapsed system header toggles it.

## Step 1 — host: full console in the log + log projections

Make the finished-program view reconstructible from the `Tree`
(decision 8), so it survives resume.

- **Log full console.** Add `EventPayload::Console { lines: Vec<String> }`
  (`types.rs`) — storable, never rendered to the LLM. Append it at every
  program terminal with `vm.console_lines.clone()`: `finish_program`
  (success), `suspend` (condition), and the abandon branch of
  `finish_frame`. Parent: the owning frame's spine, by the program's
  `Tool` result. (The clipped console in the report stays — it is what
  the *LLM* sees; this is the faithful copy for the *log/UI*.)
- **Frame projection.** A `Session`/`Tree` helper scans
  `FrameStart`/`FrameResult` → every frame (root first), parent links,
  and last-logged status; overlay live status from `self.states` where
  present. The frames pane reads this, not `self.states` alone.
- **Program projection.** A `Tree`/spine helper yields, per frame, its
  programs: `{ id, source, invokes, outcome, console }` from the spine's
  `run_program`/`resume` Assistant calls, interleaved `Invoke`s,
  `ProgramResult`/`Tool` results, and `Console` events. `id` is the
  `run_program` Assistant event id; a `resume` folds into the same
  program. `outcome` ∈ `Running`/`Suspended`/`Completed`/`Failed`.
- The live/running program still uses its VM directly (source + console +
  the rich panes); only *finished* programs come from the projection.

Acceptance:

- [ ] A saved-then-reloaded log (`open_at` at a completed leaf) exposes
      the full frame set and every frame's program list with source,
      inner tool calls, result, and **full** console — no live VM for any
      of them.
- [ ] `Console` is storable, never appears in an `LlmRequest`, and
      carries the unclipped console.
- [ ] A raise+resume program is **one** entry in the projection; its
      console spans both segments and its `outcome` ends `Completed`.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 2 — protocol: status event + stored system prompt

Two pieces (decisions 4 and 5):

- **`SessionEvent::ProgramStatus { frame, program: EventId, status }`**
  (`protocol.rs`), `status` an enum (`Running`/`Suspended`/`Completed`/
  `Failed`). Live-only; emitted by the host on each `AgentState` phase
  transition. Round-trip test like the existing protocol tests.
- **Stored system prompt** (`machine.rs`, `types.rs`): log
  `EventPayload::Message(Message::System { text })` once at frame start
  (materialize a `system` string as `render_request` does today, append
  it as the spine's first message; guard so it is logged exactly once —
  `frame.messages` has no leading `System`). `render_request` body
  collapses to `messages = frame.messages.clone()` (no synthesized
  prepend); its 11 call sites are unchanged. Re-point the two tests that
  assert the synthesized message (`dialect_card_roots_the_system_message`,
  the system-message assertion near `machine.rs:1252`).

Acceptance:

- [x] `ProgramStatus` round-trips; a scripted run emits
      `Running → Completed` (and `… → Suspended → Running → Completed`
      for a raise+resume) for the right `program` id.
- [x] The first event after a frame's `FrameStart` is a
      `Message::System` carrying the dialect card + prompt + input;
      `render_request` returns it verbatim as `messages[0]`.
- [x] Re-opening a saved log shows the *stored* system prompt, not a
      re-derivation (swap the registry card between save and load; the
      logged text is unchanged).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 3 — chat: program blocks + system block + per-frame focus (`chat.rs`)

- Group transcript items into program blocks (decision 2): open on an
  `Assistant` `run_program`/`resume` tool call (tag the block with its
  event id), accumulate `EventPayload::Invoke` events as the tool-call
  list (`⚙ name`, optional one-line arg/result preview), update the
  header from `ProgramStatus`. **Do not** inline the completion/condition
  report body (it lives in the right console/result pane).
- New `ChatKind::System`; render the stored `Message::System` as the
  first item of its frame's transcript.
- `rows()` returns `(ChatKind, String, Option<EventId>)` — the program
  id (or system-header marker) per visual line, for click hit-testing.
  Partition items by frame and render the *selected* frame's slice
  (decision 6); drop the root-pinned `[frame N]` interleave.

Acceptance:

- [x] A `run_program` with two inner tool calls renders as a `run_program:
      <status>` header + two `⚙` lines, the header tracking
      `ProgramStatus`; no report body in the rows.
- [x] `Message::System` renders as the leading `system` row of its
      frame; execution events (`ProgramResult`/`Label`) still never reach
      chat.
- [x] Selecting frame B shows B's transcript (its own system block),
      not the root's; structural guarantee test still holds (sole mutator
      `apply`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 4 — TUI: frames-always layout + click + program axis (`attach.rs`)

- Promote the **frames** pane to a persistent top-right pane in every
  view (root first, all frames, live status); chat left; the
  introspection panes populate from `selected_program` beneath the
  frames pane. `FullDebug` (`d`) stays the full-pane power mode.
- Add `selected_program: Option<EventId>` and a `collapsed:
  HashSet<FrameId>` (system-block fold state).
- `on_mouse`: handle `MouseEventKind::Down(Left)`. Hit-test `pane_rects`:
  - **Frames pane** → row → `FrameId` → set `selected` (reset
    `selected_program` to that frame's latest).
  - **Chat pane** → clicked line `= scroll_top + (row − area.y − 1)` →
    row→`Option<EventId>` table → set `selected_program` (+ owning
    frame); a click on a `system` header toggles `collapsed`.
- Default `selected_program` = the selected frame's most-recent program
  when none is clicked.

Acceptance:

- [x] Headless: clicking a frame row retargets `selected`; clicking an
      older block's row retargets `selected_program` to that block's id;
      clicking a `system` header toggles `collapsed`.
- [x] Frames pane is present in the default (chat) view; root is row 0.
- [x] Tab / `1`–`9` keyboard selection still works alongside clicks.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 5 — TUI: reduced introspection for old programs (`attach.rs`, `ui.rs`)

- When `selected_program` resolves to a record with no live VM
  (`vm_for_program` is `None`): the right column shows **source +
  console/result only**. `render_source` gains a variant taking `&str`
  (render the record's `source`, no VM). `render_attached_console`
  gains a footer from the record's `Outcome`: `result: <value>` /
  `condition: <report>` / `failed: <report>`.
- A live `selected_program` keeps the full pane set (source, console,
  and the `1`–`4` disasm/stack/promises toggles) exactly as 9_TUI.

Acceptance:

- [x] Selecting a finished program shows its source + console + result
      footer; disasm/stack/promises panes are absent (no stale live VM).
- [x] Selecting the live program restores the full pane set.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 6 — docs sweep

- [ ] 9_TUI.md Step 4 cross-references this phase (frames pane promoted
      to always-on; the sticky-`last_vm` post-mortem is generalized to
      per-program records).
- [ ] DESIGN.md / 8_HARNESS.md: note the system prompt is now a stored
      `Message::System` (materialize-once), superseding the
      re-derived-each-turn description; `SessionEvent::ProgramStatus`
      added to the protocol surface.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

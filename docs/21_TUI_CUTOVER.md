# Phase 21 — Cut the TUI over to code mode, no dual path

Operationalizes `20_CODE_MODE.md`'s Parts G (TUI) and I (deletions):
the "how," where that file already settled the "what." Written after
this session's live testing (`agent codemode-harness`) got good enough
results — a stable, well-reasoned 4/4 on the fixed task set in the
best run — to justify converting the real app rather than continuing
to iterate on it standalone.

The decision, explicit and non-negotiable per the conversation that
produced this doc: **one mechanism, not two.** `agent session` runs on
code mode when this is done. The old chat-turn + `run_program`/
`resume`/`answer` tool-call protocol (`machine.rs`,
`host/llm.rs`/`dialect.rs`/`deepseek.rs`, `chat.rs`'s block model) is
deleted, not kept behind a flag, not left running "beside" the new
path indefinitely. Stages 1–2 below are additive only because
software has to be built before it can replace something — the old
path stays alive during construction purely so there is something
working if a stage needs more than one attempt, not as a permanent
second option. Stage 3 removes it in one sweep once Stage 2 is proven
live in the TUI.

Three further constraints, from the same conversation, load-bearing
enough to restate here rather than leave implicit:

- **Individual tool actions still get their own chat-pane entries** —
  read/write/edit/bash/etc. — even though under code mode they are
  ordinary function calls inside one program, not separate LLM turns.
- **A session reopened from its log must render identically to how it
  looked live.** Nothing on screen may come from a live-only,
  unlogged side channel — the log has to be fine-grained enough that
  replaying it reproduces the exact same chat pane, row for row.
- **The TUI should feel like the existing app** to someone used to
  coding agents — same panes, same keys, same overall shape. This is
  why Stage 2 reuses as much of the current rendering/input/pane
  machinery as it possibly can and only replaces the parts that are
  actually tied to the old protocol.

## What survives the cutover, and why (findings from live exploration)

Before writing any code, three targeted explorations of the existing
~28,000-line host/TUI/machine layer (against `codemode/`'s ~5,800
already-built, already-tested lines) confirmed which parts are
protocol-agnostic and which are wedded to the old chat-tool shape.
This is the load-bearing discovery the staging below rests on — most
of the *infrastructure* survives; what's replaced is the protocol
riding on top of it.

**Genuinely protocol-agnostic, reused as-is:**
- `host/mod.rs`'s loop shape: one `Session`, a single `LoopMsg` inbox
  keyed by `BranchId` (one arrival order = the logged resolution
  order, no second scheduler), worker threads only for blocking IO
  (`spawn_llm`'s semaphore-gated completions, `spawn_tools`'s fan-out
  through `ToolRegistry`), `in_flight`'s race-free `quiet()`,
  fuel-sliced VM ticking (`FUEL_SLICE`, `Continue{branch}`
  re-enqueue), `tree.sync()` once per inbox message. None of this
  mentions `run_program`/`resume`/`answer` anywhere.
- `host/tools.rs`'s `real_registry()` and `host/registry.rs`'s
  `ToolRegistry`/`ToolDef` — the real `read_file`/`bash`/
  `create_file`/`replace_file`/structural tools. Name/schema/handler
  over a positional JSON array; nothing about the shape is
  `run_program`-specific.
- `tree.rs`'s `Event`/`EventId`/`LogHeader`/`Tree::open`/`append`/
  `log_event`/`sync`, the branch/fork addressing scheme (`Agent`/
  `Fork` root events, `parent_id` chains, `spine_at`), and torn-tail
  recovery. The append-only log and its lightweight-fork support are
  explicitly being kept, not replaced — this is infrastructure the
  project already has right, and the cutover adds new event payload
  shapes on top of it rather than inventing a new persistence
  mechanism.
- On the TUI side: `debug/input.rs`'s `InputBuffer`, `debug/
  markdown.rs` and `chat.rs`'s markdown classification/wrapping,
  `debug/highlight.rs`, `ui.rs`'s VM-introspection renderers
  (`render_source`/`render_disasm`/`render_stack`/`render_promises` —
  generic over any `interp::VM`, codemode's included), `panes.rs`,
  `attach.rs`'s pane scaffolding (hit-testing, scroll state),
  `Navigator`/timeline rendering, the `Focus`/`View` state machine,
  most of `on_key`'s routing, `cycle_branch`.

**Tied to the old protocol, replaced outright:**
- `machine.rs`'s `Runner`/`Phase`/tool-call-turn cycle (chat-turn
  assembly, `run_program`/`resume`/`answer` parsing out of an
  `LlmTurn`, refusal-menu rendering, tool-spec construction).
- `host/llm.rs`/`dialect.rs`/`deepseek.rs` — the chat+tool-call wire
  format and the dialect card teaching the model the old restart
  verbs. `codemode::transport` already does the live-completion job
  for the new shape.
- `chat.rs`'s `ChatState` block model ("one `run_program` execution =
  one block") and `attach.rs`'s `UserCall`-shaped submit/rewrite
  handling.
- Almost all of `report.rs` (`derive_report`, `clip_answer`,
  `annotate_calls`, `annotate_program`, `render_fork`,
  `outcomes_of_turn`, the answer-budget constants) — this machinery
  exists to clip a tool-call result into an LLM-turn's context, which
  has no subject under code mode (`say()`/`append_history()` are
  already the model's own choice of what re-enters context; nothing
  clips it from outside).

## Stage 1 — A real, persistent code-mode session backend (headless first)

Build a new per-branch state, one `codemode::stack::ProgramStack` plus
a small `Idle | AwaitingCompletion | Running` phase, in a new
`host/codemode_session.rs`, replacing `machine.rs`'s `Runner`/`Phase`
in the loop's `states: HashMap<BranchId, _>`.

This is the single largest new-code item in the whole cutover.
`codemode::runner::run`'s loop (dispatch a completion → step the VM →
handle `Pending`/`Done`/`Raise` → apply a decision → repeat until the
root program finishes) is a **one-shot blocking function** built for
the standalone harness. The real session needs the identical logic
**unrolled into a state machine** driven by inbox messages (`LlmDone`,
`ToolDone`, `Continue`) — the same restructuring `machine.rs`'s
`Runner`/`Phase` already had to do for the old protocol. Reuse
`runner.rs`'s helpers directly (`program_from_completion`,
`handler_tail`, the `docs_by_depth` replacement-regeneration
bookkeeping); restructure the control flow, not the logic underneath
it. When a root program finishes cleanly (not mid-decision), the
branch goes `Idle` and waits for the next user message — at which
point a fresh `Document` is rendered from the branch's accumulated
history (`document::render`) and a new completion requested. This
multi-turn loop is the one thing `runner::run` never had to have,
because the harness only ever runs one task to completion.

Replace `spawn_llm`'s target with `codemode::transport::complete`
(already speaks the DeepSeek/OpenCode endpoint and builds `Document`s
— `host/llm.rs`/`dialect.rs`/`deepseek.rs` have no further job).

Give harness verbs real backing, where the standalone harness
(`codemode::runner`'s `FakeTools`) only ever gave honest stub errors:
`spawn()`/`fork()` create real branches in `tree` (reusing
`create_agent`'s existing path), `ask()`/`say()` deliver through the
existing `Send`/`Post`-style mechanism, `list_agents()` reads the real
branch list, `artifact()` fetches a real logged `Call`/`Result` by id,
compaction ops apply against the real log.

**New `tree.rs` `EventPayload` variants** — additive; `Event`/
`EventId`/the branch/fork machinery are untouched. One per thing that
must render as its own chat line, live or replayed, so the
log-fidelity requirement holds by construction rather than by
discipline:

- `ProgramStarted { source }` / `ProgramFinished { outcome }` —
  program-block open/close (replaces the old `Turn`'s program-header
  role).
- `Say { to, text }` — one per `say()` call.
- The **existing** `Call { kind: Invoke, .. }` / `Result { call, .. }`
  pair, reused as-is, for every `tools.*` call *and* every harness
  verb that does real work (`spawn`/`fork`/`ask`/`append_history`/
  `artifact`). This is already generic call/result correlation in
  `tree.rs` — codemode's dispatcher logs through it instead of
  inventing a parallel shape, and it is exactly the mechanism the
  "individual read/write/bash entries" requirement needs.
- `Suspended { condition }` / `Decided { resume | abandon }` —
  replaces the old `Condition { cause: Cause }` with a codemode-shaped
  cause (`Raised`/`Trapped`/`Posted`, mirroring
  `codemode::stack::Suspension`).
- `Compacted { removed, rewritten }` — one per compaction op; the
  original stays in the log and stays inspectable (Part G5).

**Verify headless, before touching any TUI code**: adapt
`run_session_headless` to drive the new backend and run a real,
multi-turn conversation against a live model — an interrupt, a
suspend/decide round trip through a real decider, and a second user
turn after the first program finishes, all working end to end. This
is the checkpoint that de-risks Stage 2: prove the backend actually
holds a conversation before touching the harder TUI rewiring.

## Stage 2 — TUI rendering

Rewrite `chat.rs`'s `ChatState::apply`/`apply_payload`/`rows` around
the new `EventPayload` variants above, producing the **same**
`(ChatKind, String, RowDetail, EventId)` row shape `attach.rs::
render_chat` already consumes — so the rendering pipeline itself
barely changes, only what feeds it. A program entry renders collapsed
(one line, expands into the source pane — G3); tool activity renders
live, one row per `Call`/`Result` pair (G3b, and the explicit ask
above); `//:` narration streams as its own row kind while the program
is being written (G1b), read directly off the streaming completion.

**One function, two callers, by construction**: `ChatState::apply` is
called once per event whether it arrives live (streamed from the loop,
one at a time) or in bulk (reopening a log file and replaying every
event in order). This is what makes "reconstructed identical to live"
true structurally rather than by discipline — and it is already how
the current system works, so it is preservation of an existing
pattern, not a new one.

Rewire `attach.rs`'s `ExplicitMode`/`resolve_submit`: the `e`/`r`
rewrite gesture now submits a real replacement program — literally
what a human handler program looks like (G6) — keeping the existing
prefill-current-source UX. The `v` resume-with-value gesture keeps its
keystroke and its "type a JSON value" UX, but synthesizes a minimal
decider program (`return resume(<value>);`) under the hood instead of
sending an old-shaped restart command — a deliberate choice to keep
the familiar low-friction shortcut for the common case rather than
requiring everyone to hand-write `return resume(...)` every time.
`host/protocol.rs`'s `UserCall::RunProgram/Resume/Answer` become
whichever of the two the gesture actually produced (hand-typed or
synthesized, both real program source); `ProgramStatus`/`BranchInfo`/
`LeafInfo` need no change (confirmed UI-generic).

New, additive to `FullDebug`:
- **G2**, a document view: the exact bytes of the last/current
  request. Trivial — `codemode::document::Document`/`render` already
  produce exactly that; render it as text.
- **G4**, a program-stack view: which program is suspended under
  which handler (`codemode::stack::ProgramStack`), a new pane
  alongside the existing VM call-stack pane, both visible and
  separately labeled (`frame` stays the VM stack; this is the
  *program* stack — the doc's own naming rule).
- **G5**, compaction markers in the chat pane, with the compacted
  stub still expandable from the log.

**Verify interactively**: a real session, in the TUI, against a live
model — same panes, same keys, a user familiar with the current app
should feel at home — but every action underneath is a code-mode
program, with visible per-call activity lines, and a log that replays
identically to what was shown live.

## Stage 3 — Delete the old path (Part I), in one sweep

Only once Stage 2 is verified working live in the TUI:

- Delete `machine.rs`, `host/llm.rs`, `host/dialect.rs`,
  `host/deepseek.rs` outright.
- Delete `report.rs`'s condition/completion-report machinery in full
  (`derive_report`, `clip_answer`, `annotate_calls`,
  `annotate_program`, `render_fork`, `outcomes_of_turn`, the
  answer-budget constants) — no hedging means deleting the whole file
  rather than keeping it alive for `clip`/`cap_console`; those two are
  small enough to inline wherever the new rendering needs them.
- Delete the now-unused old `EventPayload` variants in `tree.rs`
  (`Message::Turn`'s `tool_calls` field, the old `Condition{cause:
  Cause}` shape) and bump the log version. Old pre-cutover session
  logs become unreadable — consistent with `tree.rs`'s own existing
  behavior (it already refuses mismatched-version files outright
  rather than migrating), not a new kind of break.
- Remove `#[allow(dead_code)]` on `mod codemode;` in `main.rs` — it is
  load-bearing from here on.
- Update `DESIGN.md` in the same commit sweep, per Part I's own
  instruction ("update DESIGN.md in the same commit as the deletion
  that invalidates each claim, not afterwards"): retire "the one
  exception: the answer crosses into context," update the suspension
  table (context-budget overflow joins `OutOfFuel`), retire the
  four-event exchange table (`Send`/`Post`/`Result`/`Answer`
  settlement), update the handler-hierarchy row's "condition report"
  description.
- `codemode-probe`/`codemode-harness` stay — Part H's ongoing
  regression tooling, orthogonal to the TUI and still useful after the
  cutover.

## Verification

- **After Stage 1**: a headless multi-turn session against a live
  model — confirm a program runs, a `raise()`/trap round-trips through
  a real decider, a second user message after completion starts a
  fresh turn, and the log file has one event per user-visible action.
- **After Stage 2**: the same, in the TUI — visually confirm program
  blocks collapse/expand, tool calls appear live as their own rows,
  `//:` narration streams, interrupt/resume/rewrite keys work, and —
  the specific fidelity check — quit and reopen the log file, confirm
  the replayed chat pane shows the exact same rows in the exact same
  order as what was on screen live.
- **After Stage 3**: full workspace gate (`cargo fmt`, `cargo clippy
  --workspace --all-targets`, `cargo test` per-crate given the known
  concurrency flake) green with zero warnings, plus a final live TUI
  smoke test on a clean checkout.
- Throughout: the same gate discipline (fmt/clippy/test before every
  commit) and the same commit granularity this whole phase has used —
  one coherent change with a rationale each time, not one giant diff
  at the end.

## Open calls made while writing this plan, flagged rather than silently decided

- **Resume-with-value synthesizes a program rather than requiring one
  hand-typed.** Chosen for UX familiarity (the ask was explicit: feel
  like the existing app). If the preference is that *every*
  intervention be a real, visibly-typed program with no synthesis —
  matching the design's philosophy most purely — say so and Stage 2's
  `v` gesture changes to always prefill/require source.
- **Old session logs are not migrated, just refused**, matching
  `tree.rs`'s existing behavior for version mismatches. If there are
  logs from before this cutover worth keeping readable, that needs a
  migration pass added to Stage 3 before the old `EventPayload`
  variants are deleted.

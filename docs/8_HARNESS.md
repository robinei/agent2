# Phase 8 — Agent harness

The `agent` crate grows from stubs (`types.rs`, `tree.rs`) into the system
the interpreter exists for: tree-backed conversations driving code-mode
execution with the LLM as condition/restart handler.

**Sequencing:** needs Phase 1 (interp public API). Milestones M0–M1 run on
the current synchronous `Invoke` batching; M2 needs Phase 3 (enriched
errors, `Raise` payloads, resume API); 7_ASYNC only changes which
`StepResult` the executor handles — do not block on it. This phase should
start **early**: what breaks here is the real test of the whole design, and
should be allowed to reorder plans 3–7.

## Locked design decisions

1. **Tool-call-shaped LLM interface, minimal surface.** Models are heavily
   conditioned on the tool_calls channel, so use it: the primary tool is
   `run_program(source)` — the program arrives as a tool call, never parsed
   out of prose. When a program is suspended on a condition, the offered
   tools are the restarts: `resume(value)` and `run_program(source)`
   (rewrite). An assistant turn with **no** tool call completes the frame
   (its text is the frame's result). Prose and thinking accompany tool
   calls as the APIs already encourage.
2. **Subagents are tools; transcripts are branches.** A program calls
   `tools.agent({ prompt, input })` like any other async tool. The child's
   transcript is a branch: a `FrameStart` event whose parent is the
   call-site event on the caller's spine; the caller's spine continues
   independently and later records the result. Concurrent subagents are
   just concurrent branches — the tree needs multiple active leaves, not a
   concurrency mechanism. The "frame stack" is the chain of `FrameStart`
   ancestors above a node (which `reconstruct_frames` essentially already
   computes); inline `PushFrame`/`PopFrame` stack semantics are removed.
3. **Child frames are clean-room.** A child sees its prompt, its JSON
   `input`, and its tools — never ancestor transcripts (and never ancestor
   artifacts: artifact ids are scoped to the requesting frame's spine).
   Its result is a JSON value returned to the calling program as the tool
   result. (Context-sharing policies can come later as explicit options;
   the default stays hermetic.)
4. **The restart loop is linear; forking is for users.** A condition does
   not fork: the spine reads program → condition (as the `run_program`
   tool result) → restart (the next tool call). Forking from an arbitrary
   event remains the mechanism for user-driven retry, exploration, and
   inspecting alternatives — it is UX, not control flow.
5. **No `state`. Programs are functions; the log holds the artifacts.**
   This supersedes COMPILER_PLAN §2 (the blessed `objects[0]` slot,
   seeding, and `state_to_json` are removed from the interp). A program
   receives its frame `input` as a const binding, ends with a top-level
   `return <json-able value>` (a small interp change: top-level return
   with a value, logged as a `ProgramResult` event), and has no ambient
   mutable bag. Durable memory, if evidence ever demands it, returns as a
   *tool* (`remember`/`recall`), not VM machinery.
6. **Reuse is explicit: artifacts by event id.** Completed tool results
   and program results are immutable, id-addressable artifacts. A program
   fetches one with `tools.tool_result(id)` — which is *just a tool*
   (flows through `Invoke`, served instantly from the log, batches,
   awaits; zero VM changes). There is **no implicit args-matching cache**:
   it is fragile for dynamically-computed args and dangerously wrong for
   effectful tools (silently skipping a logged `send_email` repeat). The
   condition/completion report lists the available artifacts; the LLM
   chooses what to reuse.
7. **Crash recovery is deterministic re-execution, not VM serialization.**
   To resume a mid-program interruption of the *same* program: recompile
   the source, re-execute, serve every `Invoke` positionally from the log
   (in logged resolution order — the 7_ASYNC determinism commitment; args
   compared as a consistency check, effectful calls included because they
   really happened) until past the high-water mark. Positional replay is
   sound only for identical source; rewritten programs reuse via artifact
   ids (decision 6). VM snapshotting stays off the roadmap.

## Step 0: Interp-side dialect change (do before/alongside Phase 2)

Decision 5 changes the interpreter's program contract; land it early so the
Phase 2 test harness is built on the final shape (assert on returned
values, not `state.x`):

- Allow top-level `return <expr>` (currently an error; root emits
  `Return(0)`); the value is the program result, surfaced by
  `StepResult::Done { value }`. A program ending without `return` yields
  `undefined`.
- Add the `input` const binding (host-seeded JSON, read-only — rebinding
  is a compile error like `state` rebinding was).
- Remove the `state` machinery: the blessed `objects[0]` slot, seeding in
  `for_program`, `state_to_json`, and the compiler's `state` identifier
  special-casing. Update tests to the `return`-based observable.
- Update the divergence/docs blocks that present `state` as the durable
  surface.

Acceptance:

- [x] Top-level `return <expr>` compiles; value surfaces via
      `StepResult::Done { value }`; no-`return` programs yield
      `undefined`.
- [x] `input` const binding seeded from host JSON; rebinding is a
      compile error.
- [x] `state_to_json`, `objects[0]` seeding, and `state`
      special-casing gone from non-test code; divergence/docs blocks
      updated.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 1: Event vocabulary (`types.rs`)

Two classes of event, distinguished because they render differently:

- **Chat events** (rendered into LLM requests): `Message::{User, Assistant
  {text, thinking, tool_calls}, System, Tool}` — the existing shapes,
  where `tool_calls` carries `run_program`/`resume` and `Tool` carries
  their results (completion summaries, condition reports).
- **Execution events** (harness-internal; never sent to the LLM as
  messages, queried for replay/artifacts/UI): `Invoke { name, args,
  result }` one per tool call a program makes (parented on the spine
  between the program's tool_call message and its tool result);
  `ProgramResult { value }` — the program's top-level return, logged after
  each successful run (an artifact like any tool result);
  `FrameStart { prompt, input }` (replaces `PushFrame`; branch root);
  `FrameResult { result }` (terminal event of a frame's spine);
  `Label`. `PopFrame` is deleted. `TextChunk`/`ThinkingChunk` stay
  live-only.

Document per event type: parent rules, which spine it lands on, and
whether it renders to chat.

Acceptance:

- [x] `EventPayload` has `Invoke`, `ProgramResult`, `FrameStart`,
      `FrameResult`; `PushFrame`/`PopFrame` deleted; everything
      serde round-trips (JSONL line per event).
- [x] Doc comment per variant: parent rule, spine, renders-to-chat.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 2: Tree with multiple active leaves (`tree.rs`)

Replace the single `frames` + `leaf_id` cursor with spine handles:
`Spine { leaf_id, frames }`, `tree.append(&mut spine, payload)`. Opening a
file reconstructs the *set* of leaves (`list_leaves` exists); resuming
picks one (or several — frames that never got their `FrameResult`).
Single-process single-writer; ids stay globally monotonic across spines.
Tests: interleaved appends on two branches; reconstruction of each;
re-open with an in-flight subagent branch.

Acceptance:

- [x] `Tree` no longer holds `frames`/`leaf_id`; appends go through
      `Spine` handles; ids monotonic across interleaved spines.
- [x] `FrameStart` branches: child spine roots at a `FrameStart`
      parented on the caller's call-site event; caller's spine
      continues past it.
- [x] Tests: interleaved appends on two branches reconstruct
      independently; file re-open recovers both leaves (one an
      in-flight subagent: `FrameStart` without `FrameResult`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `Tree::start_frame(parent, prompt, input) -> Spine` is the
only way to root a frame (appending a `FrameStart` via `append`
panics), so a caller's spine handle never advances past the call
site. A spine leaf is an event with no non-`FrameStart` children —
that definition is what keeps the call-site event listed as the
caller's resumable leaf while a subagent branch is in flight.
`FrameResult` marks the spine complete; appending after it panics.)*

## Step 3: Sans-io frame step machine (`AgentState::step`)

Flesh out the `StepInput`/`StepOutput` stubs into a deterministic,
IO-free core — testable with a scripted LLM:

- Inputs: `UserTurn(text)`, `LlmResponse(message)`, `ToolResults(batch)`,
  `SubagentResult { call_id, result }`.
- Outputs: `LlmRequest { messages, tools }` (rendered from the frame),
  `ToolCalls(Vec<InvokeCall>)` (a program's fan-out batch),
  `SpawnFrames(Vec<{ call_id, prompt, input }>)` (from `tools.agent`
  calls), `FrameDone(result)`.
- Internal flow per frame: render → LLM → if no tool call: `FrameDone`;
  if `run_program`: compile (compile errors return immediately as the
  tool result — a cheap repair loop with no execution), bind the frame
  `input`, drive the VM: `Invoke` batches → `ToolCalls` out / results in
  (each logged as an `Invoke` event; `tool_result(id)` calls answered
  from the log); `tools.agent` calls → `SpawnFrames`; `Raise`/trapped
  error → render the condition report and finish the turn (the tool
  result), offering restart tools; `Done` → log `ProgramResult`, tool
  result = completion report (returned value + console output + new
  artifact ids).
- The host (separate module) owns: LLM API, tool execution, concurrent
  subagent frame loops, scheduling. The core never blocks and never does
  IO.

Acceptance:

- [x] Scripted round-trip, asserted on the event log: `UserTurn` →
      `LlmRequest`; a `run_program` response compiles and runs; an
      await fan-out emits one `ToolCalls` batch; `ToolResults` resolve
      promises and log `Invoke` events in resolution order; `Done`
      logs `ProgramResult` and renders the completion report as the
      tool result; a final no-tool-call response yields `FrameDone` +
      a logged `FrameResult`.
- [x] A compile error returns immediately as the tool result (repair
      loop — no VM constructed, no execution events logged).
- [x] `raise` with payload and a trapped TypeError each produce a
      condition-report tool result offering restarts; both paths
      work: `resume(value)` continues the same VM; `run_program`
      rewrite reuses a prior artifact via `tools.tool_result(id)`
      answered from the log (no new `ToolCalls` for it).
- [x] `tools.agent` emits `SpawnFrames`; `SubagentResult` resolves
      the call and logs its `Invoke` artifact.
- [x] VM compute is host-fueled: a hot loop yields control every
      tick (machine reports still-working; nothing blocks).
- [x] Mid-program `UserTurn` injection is deferred to M2 (documented
      panic until then).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `agent/src/machine.rs`. `Tick { fuel }` input + `Working`
output carry the 9_TUI fuel-slice scheduling; host calls are keyed by
machine-assigned `invoke_id`s with a per-run generation counter, so
results of an abandoned (rewritten) run are still logged as artifacts
but never delivered to the dead VM. `tools.tool_result(id)` is served
synchronously from the frame's spine segment and not re-logged —
positional replay re-answers it from the same log prefix. Interp
change: `VM::json_to_stack_value` made `pub` so hosts can feed JSON
results into `resolve_promise`/`reject_promise`. The condition/
completion reports are deliberately minimal — Step 4 owns their real
shape.)*

## Step 4: The condition report (the thesis, make it good)

The `run_program` tool result for a raise/trapped error is the product
surface of the whole project. Structured sections, rendered compactly:

- what happened: condition name + payload, or the Phase 3 rendered
  diagnostic (source line, caret, operand values) — and for async, the
  await-chain;
- where: how far the program got, plus the program's `console` output so
  far (the diagnostic stream survives failure precisely when the return
  value doesn't — it is the trace of what the program observed before it
  raised; tail-truncated);
- the **artifact menu**: completed tool calls and prior program results
  as `[#id] name(args-summary) → size/preview`, fetchable via
  `tools.tool_result(id)` — with effectful calls explicitly flagged
  ("already happened; calling again repeats the effect");
- available restarts and exactly what each does: `resume(value)` —
  continue as if the failed operation returned `value`; `run_program` —
  replace the program (reuse prior work via the artifact menu).

Iterate this format against a scripted LLM first, then real models; it is
a prompt-engineering artifact as much as a data format, and deserves its
own tests (golden renders).

Acceptance (scripted LLM only — no network in this step):

- [x] Reports live in `agent/src/report.rs` as pure renderers
      (`ConditionReport`/`CompletionReport` structs → string, no
      `Tree`/`VM` access inside the renderer); `machine.rs` only
      assembles the structs from the suspended/finished run.
- [x] Condition report sections, in order: **what happened** (raise:
      diagnostic-rendered location with source line + caret, condition
      name, payload preview; trapped error: `VM::render_error`'s
      line/caret render), **where** (call-stack chain from
      `VM::frames()` + console tail with `last X of Y` counts),
      **artifact menu** (`[#id] name(args) → preview` per artifact,
      effectful entries flagged "already happened; calling again
      repeats the effect"), **restarts** (wording per suspension kind:
      raise-resume vs operation-resume; `resume` absent and explicitly
      noted when not resumable).
- [x] Completion report gets the same machinery: returned-value
      preview, console tail, new-artifact menu (artifacts logged since
      the run started, the `ProgramResult` included).
- [x] Hard size bounds on **every** section, as named consts in
      `report.rs` (what/payload bytes, stack depth, console line count
      + per-line clip, artifact entry count + per-entry preview bytes,
      returned-value bytes); every truncation leaves an explicit
      marker. Unit tests feed oversized inputs and assert bound +
      marker (`report::tests`).
- [x] Golden render tests, exact full-string asserts driven through
      the scripted machine (real event ids, real console):
      raise-with-payload, trapped TypeError, and a completion with new
      artifacts (`machine::tests::golden_*`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `agent/src/report.rs`. Both reports share the section
renderers (console tail, artifact menu), so their bounds and wording
can't drift apart. A raise renders through the same `Diagnostic`
machinery as compile errors (`condition `name` raised` at the raise
site with source line + caret) — one diagnostic shape everywhere the
LLM reads. The Phase 3 items the doc names (operand values, async
await-chain) slot into the **what happened** section when Phase 3
lands; `VM::render_error` is already the seam.)*

## Step 5: Host layer

Tool registry (name, JSON-schema'd input/output, handler, an
`effectful: bool` flag driving the artifact-menu warnings); the
`tool_result` tool (log lookup, frame-scoped id validation); concurrent
fan-out execution with logged resolution order; the `agent` tool mapping
to frame spawn; result-size guards before anything enters the log; the
real LLM client behind the same trait as the scripted one.

Concurrency model: synchronous, no async runtime, std only. One main
loop thread owns the tree and the frame step machines and `recv()`s a
single `std::sync::mpsc` inbox — one unified message enum (LLM chunk,
tool result, frame result, user command), every worker thread a cloned
`Sender`. Workers: one thread per in-flight LLM stream (blocking HTTP
reads), spawn-per-call for tool fan-out. VM compute also runs on the
loop thread, in fuel slices (`step(fuel)`, 9_TUI Step 0) with a
continue message re-enqueued between slices, so a hot program never
starves other frames. The single inbox gives one total arrival order,
which *is* the logged resolution order (decision 7) — no select
fairness in the loop. UIs talk to the loop only through serializable
message enums (`SessionCommand` into the inbox, `SessionEvent` out —
chunks included); the M4 CLI uses the same channel pair, so a later
client/server frontend is a new consumer, not a refactor. One
exception: the debugger TUI (9_TUI) renders on the loop thread and
borrows VMs/tree directly for its debug panes — its chat pane still
consumes `SessionEvent`s, keeping the serializable boundary exactly
the surface a remote client needs. That TUI in attached mode (9_TUI
decision 6, Step 4) is the harness's primary frontend, not an add-on:
chat left, source + console panes auto-popping while a program runs,
a key for full debugger mode.

Acceptance (M0 scope — scripted LLM only, no network; the real client
is M1):

- [x] One main-loop thread owns the `Tree` + `AgentState`s and `recv()`s
      a single `std::sync::mpsc` inbox of one unified message enum;
      workers (LLM completion, spawn-per-call tool fan-out) only ever
      hold a cloned `Sender`. VM compute runs on the loop thread in fuel
      slices with a continue message re-enqueued between slices. Test: a
      `while (true) {}` program keeps the loop live — a `Shutdown`
      command queued behind the continue messages is still processed
      (`hot_loop_keeps_the_inbox_responsive`).
- [x] Tool registry: name, description, JSON-schema'd input/output, an
      `effectful: bool` flag, handler. Fan-out executes concurrently on
      worker threads; the single inbox arrival order is the logged
      resolution order. Test: a slow tool called before a fast one logs
      its `Invoke` second (`fanout_logs_in_completion_order`).
- [x] `effectful` drives the artifact-menu warning ("already happened;
      calling again repeats the effect") in reports.
- [x] Result-size guard: an oversized tool result is replaced by an
      error before anything enters the log; test asserts on the logged
      `Invoke` artifact.
- [x] The `agent` tool maps to frame spawn: `SpawnFrames` creates a
      child `AgentState` on a branch; its `FrameDone` routes back to the
      caller as `SubagentResult`; test asserts both spines' event logs
      (`agent_tool_spawns_child_frame_and_joins`).
- [x] The LLM sits behind a trait (scripted implementation first); the
      session loop is client-agnostic.
- [x] UIs reach the loop only via serializable `SessionCommand` /
      `SessionEvent` enums (chunks included); the M0 CLI consumes that
      channel pair.
- [x] **M0**: `cargo run -p agent -- session --headless` runs the
      scripted demo end-to-end; a test asserts the event log reads
      `FrameStart → User → Assistant(run_program) → Invoke × 2 →
      ProgramResult → Tool(report) → Assistant(text) → FrameResult`
      (`m0_scripted_demo_end_to_end`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `agent/src/host/` — `Session` (loop + `LoopMsg` inbox),
`registry.rs`, `llm.rs` (trait + `ScriptedLlm`), `protocol.rs` (the
serializable boundary), `demo.rs` (the M0 script, shared by CLI and
test). A user turn arriving while the frame is busy is rejected with a
`SessionEvent::Error` — the host-injected condition is M2. A re-opened
log resumes its lowest incomplete leaf; richer resume/fork UX is M4.
Late tool results for a frame that already logged `FrameResult` are
dropped (the spine is sealed); a *suspended* frame still logs late
arrivals as artifacts. `Session::pump_one` is the loop step the
attached TUI (9_TUI Step 4) drives directly.)*

## Step 6: System prompt / dialect card

A generated-where-possible description of: the dialect (divergence list
distilled), the program contract (`input` binding, top-level `return`,
artifact ids and `tools.tool_result`), `raise` and what restarts mean,
available tools (from the registry schemas), and the
no-`this`/no-`class` guidance with alternatives. Keep it short; the
condition report carries the per-incident detail.

Acceptance:

- [x] `agent/src/host/dialect.rs`: `dialect_card(&ToolRegistry) ->
      String`. The tool list is generated from registry schemas — one
      line per tool consuming `ToolDef.description` + `input_schema`,
      effectful tools flagged — with the built-ins
      `tools.tool_result(id)` and `tools.agent({prompt, input})`
      documented alongside.
- [x] Card covers: how to act (`run_program` is the only way to do
      anything; a no-tool-call reply ends the frame), the program
      contract (`input` const, top-level `return` of a JSON-able value,
      console as the surviving diagnostic trace, artifact reuse),
      `raise` + restart semantics (resume vs rewrite), the distilled
      dialect divergences, and no-`this`/no-`class`/no-`new` guidance
      with alternatives.
- [x] Short by construction: a test bounds the static text
      (`dialect::tests`); one line per registered tool, schema clipped.
- [x] Wired as the root of the system message:
      `AgentState::set_dialect_card` + `render_request` prepend it
      before the frame prompt; `Session::new` and `spawn_child` feed it
      from the registry. Tests: machine-level (system message starts
      with the card, frame prompt follows) and host-level (a capturing
      `LlmClient` sees the card with the registered tool's line).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `agent/src/host/dialect.rs` — static head (acting, program
contract, tools intro + built-ins) ++ generated tool lines ++ static
tail (conditions/restarts, dialect divergences). The divergence section
is the `vm/mod.rs` list distilled to what changes how one writes a
program: no `this`/`class`/`new` (plain functions + object literals,
`new Error` excepted), promises only from `tools.*` with
`await`/`Promise.all`, UTF-8 string semantics, non-coercing
relationals, strict arity, 64-bit bitwise. The card is per-`AgentState`
(`set_dialect_card`), so the per-frame tool-scoping hole can later feed
children a narrower card without new plumbing.)*

## Known holes (resolved in principle, specify during build)

- **In-flight effects during a condition:** outstanding tool calls when a
  program raises/traps run to completion and are logged as artifacts no
  matter which restart the LLM picks — rewrite abandons the VM, never the
  physics. The report's artifact menu includes late arrivals on the next
  turn.
- **User interruption / steering:** a `UserTurn` arriving mid-program is a
  **host-injected condition** at the next `step()` boundary — same report
  machinery, payload = the user's message, restarts = resume (continue,
  message noted) or rewrite. No second interruption mechanism.
- **Per-frame tool scoping:** `tools.agent` spawns take a tool allowlist
  (default: caller's set minus effectful tools), enforced by the registry;
  the child's dialect card lists only its own tools.
- **Log versioning:** the event log carries the interp/harness version.
  Positional replay (decision 7) requires a version match; on mismatch,
  degrade to a condition reporting the interruption + artifact menu —
  never replay across versions (scheduler/codegen changes silently break
  invoke order).

## Open question (deliberately deferred until M1/M2 contact)

**Context growth.** Long frames accumulate programs, reports, and an
ever-growing artifact menu; forking preserves history but never shrinks
it. Cheap mitigations to apply from the start: hard size bounds on every
report section, artifact menu pruned to recent + still-referenced entries
(full list fetchable). The real mechanism (frame summarization as a
checkpoint event? sub-frame hand-off?) should be designed from observed
M1/M2 transcripts, not speculation.

## Milestones

- **M0**: scripted-LLM end-to-end — user turn → program → fan-out →
  results → completion, all asserted on the event log. No network.
- **M1**: real LLM, a few real tools (file read, HTTP fetch), single
  frame. First honest contact with the dialect prompt. From here on,
  drive sessions through the attached TUI (9_TUI Step 4) — it is the
  observation instrument for iterating the condition report.

  M1 acceptance:

  - [x] `LlmRequest.tools` upgraded from `Vec<String>` to full tool
        definitions (`ToolSpec { name, description, parameters }`);
        the `run_program`/`resume` schemas are serialized in exactly
        one place (`machine.rs`), and `render_request` tests assert
        names + non-empty parameter schemas for both phases (idle vs
        suspended).
  - [x] `agent/src/host/deepseek.rs`: `DeepSeekClient` implements
        `LlmClient` over blocking `ureq` SSE — POST
        `{base}/chat/completions`, OpenAI chat-completions format
        (System/User/Assistant+tool_calls/Tool message mapping),
        `stream: true`; `reasoning_content` deltas →
        `LlmChunk::Thinking`, `content` → `Text`; tool-call argument
        fragments accumulated by index. The request builder and SSE
        parser are pure functions unit-tested on string fixtures —
        nothing in `cargo test` opens a socket.
  - [x] Config: key from `DEEPSEEK_API_KEY`, model from
        `DEEPSEEK_MODEL` (default `deepseek-v4-pro`), base URL from
        `DEEPSEEK_BASE_URL` (default `https://api.deepseek.com`).
  - [x] Real tools (`host/tools.rs`): `read_file(path)` and
        `http_fetch(url)`, contents clipped before the result-size
        guard; blocking handlers on worker threads. `read_file` is
        unit-tested with temp files; `http_fetch`'s handler is not
        exercised by tests (network).
  - [x] `agent session`: `--real` forces DeepSeek (clear error
        without a key); the attached TUI auto-picks DeepSeek iff
        `DEEPSEEK_API_KEY` is set; `--headless` stays scripted unless
        `--real`; `--turn <text>` queues a first user turn (headless
        driving). The scripted client remains the default everywhere
        tests run.
  - [x] Manual M1 verification: one live
        `agent session --headless --real --turn …` run against
        api.deepseek.com; transcript eyeballed (program arrives via
        `run_program`, completion report consumed, final text turn).
  - [x] Gate: `cargo fmt && cargo clippy && cargo test` green, fully
        offline.

  *(Built: `host/deepseek.rs` (client + pure `request_body`/`parse_sse`
  with fixture tests), `host/tools.rs` (`read_file`, `http_fetch`,
  contents clipped via `report::clip` so truncation is visible to the
  program rather than a size-guard rejection), `ToolSpec` +
  `run_program_spec()`/`resume_spec()` in `machine.rs`. Live-verified
  2026-06-12 against api.deepseek.com ("fetch example.com, report its
  title"): full arc User → run_program → Invoke → ProgramResult →
  completion report → final text → FrameResult. Bonus datum: the model
  opened with a regex literal despite the dialect card's divergence
  line; the compile-error repair loop caught it (line + caret) and the
  rewrite used `indexOf`/`slice` — the cheap repair loop works on a
  real model, and card wording alone doesn't prevent the attempt.
  Worth remembering when iterating the card from M2 transcripts.)*
- **M2** (needs Phase 3): conditions round-trip — `raise` with payload and
  a trapped TypeError both produce a report, and both restart paths
  (resume / rewrite reusing artifacts by id) work against a real model.

  M2 acceptance:

  - [x] Host-level round-trip through the full `Session` loop, not just
        the machine: `raise(name, payload)` → condition report logged as
        a `Tool` event → the LLM's `resume(value)` re-enters the *same*
        VM → the resumed value becomes the raise expression's result →
        completion report → frame-completing text turn
        (`host::tests::raise_round_trips_resume_through_the_session`).
  - [x] Trapped runtime error → condition report → `run_program` rewrite
        that reuses the already-logged tool result by id
        (`tools.tool_result(#id)`), served from the log with **no** second
        `Invoke` for the original call
        (`host::tests::trapped_error_rewrite_reuses_artifact_through_the_session`).
  - [x] `scripted_resume(call_id, value)` added beside
        `scripted_program`/`scripted_text` so the restart turn is
        scriptable offline (the machine-level golden reports already cover
        the report *rendering* — these host tests cover the loop *routing*).
  - [x] Manual M2 verification against a real model (DeepSeek,
        2026-06-13): a `raise("need_value", payload)` run round-trips
        report → `resume(10)` → `returned: 11` → final text; a separate run
        does `http_fetch` (logged `#4`) → trapped TypeError (caret on
        `x.length`) → `run_program` rewrite calling `tools.tool_result(4)`
        with **no** second fetch → `returned: 559`. The first live attempt
        surfaced a real bug — the post-`resume` report answered the
        original `run_program` call id, not the `resume`, so the next chat
        request 400'd ("tool_call_ids did not have response messages");
        fixed by carrying the resume's call id onto the `Run`.
  - [x] Gate: `cargo fmt && cargo clippy && cargo test` green, fully
        offline.

  *(Built: the machine already round-trips conditions (Step 3) and renders
  the reports (Step 4); M2 adds the missing **host-loop** coverage —
  proving a `resume`/`run_program` restart turn dispatched by the `Session`
  re-enters or rewrites the suspended frame end-to-end. The rewrite test
  leans on deterministic event ids (FrameStart 1 … fetch Invoke 4) and
  asserts the id it hardcodes, so a numbering change fails loudly rather
  than silently fetching the wrong artifact. The one open box is the live
  run, which needs a network key this environment lacks; M1's live arc
  already exercised `run_program` → report on a real model, so the
  remaining unknown is only how the *restart* tools land — left for the
  user to drive.)*
- **M3**: subagent branches — `Promise.all` over `tools.agent` spawning
  concurrent child frames (or sequential awaits pre-7_ASYNC), results
  joining the parent program.

  M3 acceptance:

  - [x] `Promise.all([tools.agent(a), tools.agent(b)])` dispatches both
        agent calls in **one** fan-out batch (the array literal evaluates
        before `__all` awaits), which `dispatch_calls` splits into a
        single `SpawnFrames` carrying both — so the two children are
        spawned concurrently, not serialized.
  - [x] Host-level through the full `Session` loop: both children root on
        their own branches (three leaves total), each runs its frame to
        completion independently, and both results join back into the
        parent program's returned array — asserted order-independently
        because the two children race for the shared scripted client
        (`host::tests::promise_all_over_concurrent_agents_joins_both`).
  - [x] Both `tools.agent` calls join as `Invoke` artifacts on the
        caller's spine (reuse/inspection works the same as any tool).
  - [ ] Manual M3 verification against a real model (needs
        `DEEPSEEK_API_KEY`): one live run delegating two concurrent
        subtasks via `Promise.all` over `tools.agent`, both results
        joining the parent. *Deferred: no key in the build environment.*
  - [x] Gate: `cargo fmt && cargo clippy && cargo test` green, fully
        offline (the concurrent test ran 15× without flaking).

  *(Built: no new machinery — Step 3's `dispatch_calls` already routes a
  mixed batch (`agent` → `SpawnFrames`, `tool_result` → log, rest →
  `ToolCalls`) and Step 5's host already spawns a child per `SpawnFrame`
  and joins its `FrameDone` back as a `SubagentResult`. M3 is the test
  that proves the **concurrent** path: `Promise.all` over two agents, two
  live branches, both joined. The shared `Arc<Mutex<dyn LlmClient>>` means
  the two children pop scripted turns in race order, so the test asserts
  on the joined *set*, not position — exactly the property a real model
  has too.)*
- **M4**: fork/label UX — list leaves, fork from any event, resume a
  chosen spine (CLI is fine).
- **M5**: the eval question — measure the thesis: success rate and token
  cost on multi-step tasks with injected tool failures, versus (a) plain
  tool loop, (b) code mode without conditions (failure = rerun whole
  program). Design this when M2 works; it likely deserves its own plan
  file.

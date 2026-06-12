# Phase 9 — Debugger TUI (and `step(fuel)`)

A multipanel ratatui frontend: chat on the left, toggleable debug panes
on the right (source, disassembly, stack, promises/invokes, console,
frame list). The motivation is visibility into the VM — it has become
the lynchpin of the project — and the engineering case is that the
introspection it needs (ip→line mapping, function names, local
name→slot maps, promise states) is the same infrastructure Phase 3
diagnostics and the Phase 8 condition report want. The TUI is also the
observation instrument 8_HARNESS explicitly calls for: iterating the
condition report and the context-growth answer "from observed M1/M2
transcripts" means watching them live.

**Sequencing:** Steps 0–3 need only the interp — land them any time;
Step 0 is a small independent interp change that should go first. Step
4 (attached mode) needs Phase 8 M0 (host loop + scripted LLM); the
frame switcher reaches full value at M3 (concurrent subagents) but
works as soon as multiple leaves exist.

## Locked design decisions

1. **Read-only visualizer plus single-stepping — not a debugger
   product.** Execution control is exactly: run, pause, step one
   instruction (`step(1)`), step one source line (loop `step(1)` until
   `diag::line_col` of `spans[ip]` changes). No breakpoints, no
   watches, no step-over/step-in; those are a separate project that
   can grow later from this base.
2. **`step(fuel: u64)` — fuel is a per-call slice argument, not VM
   state.** Each call runs at most `fuel` instructions; running dry is
   a normal `StepResult` (`OutOfFuel`: nothing consumed, call `step`
   again to continue), not a `VMError`. The total per-program budget
   (today's `DEFAULT_FUEL` backstop) becomes host policy: the harness
   counts slices. This deletes `ErrorKind::OutOfFuel`,
   `ResumeMode::RetrySameInstr` (its only user), and the uncatchable
   carve-out — out-of-fuel never enters the program's observable world
   at all.
3. **Cooperative scheduling on the main loop thread.** All frame step
   machines run on the host's single loop thread; the VM executes in
   fuel slices (e.g. 100k instructions), re-enqueueing a continue
   message to the inbox between slices so a hot program never starves
   other frames or the UI. Only blocking IO (LLM streams, tool
   execution) runs on worker threads. Single-stepping and slicing are
   the same mechanism at different fuel values.
4. **Privileged debugger, channeled chat.** The TUI renders on the
   main loop thread (a render call after draining inbox messages,
   throttled to ~30ms; crossterm input arrives as inbox messages via a
   cloned `Sender`), so debug panes **borrow the VMs and tree
   directly** — zero copies, never stale. The chat/transcript pane
   consumes `SessionEvent`s only: the serializable
   `SessionCommand`/`SessionEvent` boundary (8_HARNESS Step 5) stays
   exactly the surface a future remote client (PWA) needs. A remote
   `VmSnapshot` query is out of scope until a remote debugger is real.
5. **Two modes, one binary.** Standalone: `agent debug <file.js>` —
   compile and run a program under the slicer with a stub tool
   registry, no LLM, no harness. Attached: the same panes over a live
   harness session. Standalone ships first; it is the interp debugging
   tool wanted *now* and blocks on nothing.
6. **Attached mode *is* the harness TUI — the debugger is integral,
   not bolted on.** "Planning as JS program creation" is the core of
   the harness, so the program source earns screen space whenever a
   program is running: the chat session is the default full-width
   view, and the source + console panes auto-pop on the right when a
   frame starts executing a `run_program`. A mode key switches to
   full debugger mode (the standalone configuration: console/result
   left, full debug pane stack right) and back. There is no separate
   "harness UI" to build later.

## Step 0 — interp: `step(fuel)`

Change `VM::step(&mut self) -> Result<StepResult, VMError>` to
`step(&mut self, fuel: u64)`. Add `StepResult::OutOfFuel` (slice
exhausted; ip unchanged at the next unexecuted instruction; call
`step` again to continue). Delete the `pub fuel` field, `DEFAULT_FUEL`,
`ErrorKind::OutOfFuel`, and `ResumeMode::RetrySameInstr`; update the
resume-audit table and the divergence/uncatchable doc blocks (`raise`
stays uncatchable; fuel no longer appears in that list). Acceptance:

- [x] `step(fuel)` signature; all call sites (testutil, tests, docs
      examples) updated.
- [x] Slice-resume test: a counting loop driven entirely with
      `step(1)` completes with the same result as one big-fuel call
      (`out_of_fuel_slices_resume`, `out_of_fuel_is_invisible_to_programs`);
      a `Pending` yield mid-slices still round-trips
      (`pending_round_trips_under_single_stepping`).
- [x] `ErrorKind::OutOfFuel`, `ResumeMode::RetrySameInstr`,
      `DEFAULT_FUEL`, `VM::fuel` all gone from the public API;
      `out_of_fuel_is_not_catchable` replaced by the slice-resume
      test (out-of-fuel is structurally invisible to programs now).
- [x] Resume-audit table and `vm/mod.rs` doc comments mention fuel
      only as the `step` argument.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 1 — interp: introspection + debug info

The stack/source/disasm panes need names the compiler currently
discards. Always-on (it's cheap and Phase 3 diagnostics want it too):

- `Program` grows a debug table: function name per code range
  (`<root>` / `<anonymous>` where unnamed) and per-function local
  name→slot maps (params, locals, upvals).
- VM read-only accessors (CallFrame fields are private): a `frames()`
  view yielding per-frame { function name, arg/local slots with names
  and current `Value`s, temp range (frame floor → stack top) },
  plus the current `ip`. `spans`/`source`/`promises`/`console_lines`
  are already pub; `diag::line_col` already maps span→line/col.
- Disassembly rendering: `Instr` already derives `Debug`; add a
  helper rendering `ip  instr  @line` for a window of code.

Acceptance:

- [x] Golden test: compile a small program with named + anonymous
      functions; assert function names and local names via the debug
      table (`compiler/tests/debuginfo.rs`).
- [x] Mid-execution test: pause via a small fuel slice inside a call,
      assert `frames()` reports both frames with correct names and
      local values.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `debuginfo::{DebugTable, FnDebug}`; attribution is span-based —
instruction → innermost function whose source span contains `spans[ip]`
— so it survives optimizer code motion with no parallel table. The
analyzer records `own_slot_names` at slot allocation (remapped by
const-fn slot compaction); `VM::frames()` returns `FrameView`s
(name, named locals, temps); `VM::disasm` renders `── name ──` headers
at function block starts.)*

## Step 2 — TUI shell + standalone mode

`agent debug <file.js>`: ratatui + crossterm, main-loop-thread render
per decision 4. Layout: left pane (standalone: console output +
program result/condition; attached later: chat), right stack of
toggleable panes. Stub tool registry (`echo`, `sleep`, a failing tool)
so `Pending` and `Raise` paths are exercisable without a harness.
Keybinds: run/pause, `s` step instruction, `n` step line, pane
toggles, quit; a help footer. Redraw throttled (~30ms). Acceptance:

- [x] `cargo run -p agent -- debug agent/samples/demo.js` opens the
      TUI; the demo exercises calls, echo/sleep/fail awaits, console
      output, and a `raise` (headless-asserted end-to-end in
      `runner::tests::demo_sample_runs_to_condition_then_done`).
- [x] Step key advances exactly one instruction (unit test asserts
      `ip + 1`; the status pane shows `ip` + current disasm line).
- [x] Run mode stays responsive during a hot loop: ticks are
      20k-instruction slices with input polled between, so pause takes
      effect within one slice.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green (slicing
      driver, stub tools, condition resume, and keybind dispatch
      unit-tested; the terminal rendering itself needs a manual
      eyeball).

## Step 3 — debug panes

- **Source**: full program text, current line highlighted via
  `spans[ip]` + `line_col`, auto-scrolled.
- **Disassembly**: window around `ip`, current instruction
  highlighted, line annotations.
- **Stack**: frames innermost-first with function names; per frame
  the named args/locals with rendered values, then the temp range;
  frame boundaries visually marked.
- **Promises/invokes**: promise table (id, pending/resolved/rejected)
  and outstanding `InvokeCall`s (name, args summary, promise id).
- **Console**: the `console_lines` ring.

Acceptance:

- [x] Stepping the demo program shows the source highlight and disasm
      cursor moving together and the stack pane growing/shrinking
      across a call/return (headless: `disasm_window` has exactly one
      current row centered on `ip`; `stack_rows` shows `work`'s frame
      with `acc = 42` mid-call; the visual pairing needs a manual
      eyeball).
- [x] An awaited stub tool shows up in the invokes pane as pending,
      then resolved (`promise_rows_track_pending_then_resolved`).
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: panes toggle with `1`–`4` (source, disasm, stack, promises;
promises off by default) and stack vertically on the right. Source is
syntax-highlighted by a hand-rolled tokenizer (`highlight.rs`) — not
AST-based: comments/keywords aren't AST nodes and oxc has no public
lexer/highlighter, and the dialect is small. Pane content is built by
terminal-free helpers in `panes.rs` (disasm window with function
headers, stack rows with named locals + value previews incl. Upval
cell deref, promise table), so it's all unit-testable.)*

## Step 4 — attached mode + frame switcher (needs 8 M0)

The same panes over a live harness session, per decision 6: this *is*
the harness TUI. The chat pane renders `SessionEvent`s (chunks
included); user input goes through `SessionCommand` — steering an
in-flight program remains Phase 8's host-injected condition, not a
TUI mechanism.

Layout state machine (pure UI state — nothing in the host changes):

- **Chat view** (default): full-width chat.
- **Running view**: when the selected frame starts executing a
  `run_program`, the source and console panes auto-pop as a right
  column; chat stays left. Auto-popped panes are *sticky*: on program
  completion (result or condition) they remain, showing final state
  for post-mortem reading next to the report in chat. A collapse key
  returns to full-width chat; the manual pane toggles (`1`–`4`) work
  here and override the auto-pop set.
- **Full debugger mode**: a mode key swaps to the standalone layout —
  console/result left, full debug pane stack right, chat hidden —
  and back. Run/pause/step keys apply to the selected frame's VM in
  either view.

Frame switcher: a frame-list pane shows active leaves; tab / `1`–`9`
(in full debugger mode) switch which frame's VM the debug panes
borrow. Auto-pop triggers off the *selected* frame; a busy indicator
in the frame list covers the others.

Acceptance:

- [x] Against the M0 scripted LLM: a `run_program` turn auto-pops
      source + console (program text visible while running); on
      completion the panes remain until the collapse key restores
      full-width chat. Layout transitions unit-tested headlessly
      (view-state enum in, pane set out:
      `m0_run_program_auto_pops_and_sticks` drives the real demo's
      `SessionEvent`s); visuals eyeballed.
- [x] Full-debugger-mode key swaps to the standalone pane
      configuration and back without disturbing the session
      (`full_debugger_mode_swaps_and_returns_without_session_actions`).
- [x] A session with two concurrent frames (or two leaves pre-M3)
      renders both in the frame list; switching retargets all debug
      panes (`concurrent_frames_list_and_retarget`: caller + in-flight
      subagent, each pane borrow resolving to its own VM).
- [x] Chat pane is driven only by `SessionEvent`s (no privileged
      reads) — asserted by module visibility, not discipline.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `agent/src/debug/attach.rs` + `chat.rs`; `agent session`
opens it (`--headless` keeps the M0 event printer). The structural
chat guarantee: `ChatState`'s fields are private to `chat.rs` and its
only mutator is `apply(&SessionEvent)`, so the renderer can only show
what crossed the serializable boundary. Keys are focus-modal so chat
typing stays free: printable keys go to the input line, `Esc` swaps
to debug-control focus where the doc's bare keys live (`c` collapse,
`d` full debugger, `1`–`4` toggles, space/`s`/`n` VM control); Tab
switches frames everywhere. Host support added for this step:
`Session::pump_until` (inbox drain with render deadline; terminal
input arrives as inbox messages per decision 4), per-frame
pause/step (`set_paused`/`step_paused` — pausing parks the frame's
fuel-slice continues), and `AgentState` keeps the last run's VM so
the sticky panes show final program state post-mortem.)*

## Step 5 — docs sweep

- [ ] 8_HARNESS.md Step 5 concurrency paragraph: fuel-slice
      scheduling noted; privileged-TUI exception cross-referenced to
      this file. *(Done alongside this plan's creation.)*
- [ ] Divergence/docs blocks in `vm/mod.rs` consistent with Step 0's
      fuel semantics (covered by Step 0 acceptance; re-check here).
- [ ] DESIGN.md gains a one-line pointer to this phase if it lists
      the plan files.

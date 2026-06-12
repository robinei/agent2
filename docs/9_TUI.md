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

- [ ] Golden test: compile a small program with named + anonymous
      functions; assert function names and local names via the debug
      table.
- [ ] Mid-execution test: pause via a small fuel slice inside a call,
      assert `frames()` reports both frames with correct names and
      local values.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 2 — TUI shell + standalone mode

`agent debug <file.js>`: ratatui + crossterm, main-loop-thread render
per decision 4. Layout: left pane (standalone: console output +
program result/condition; attached later: chat), right stack of
toggleable panes. Stub tool registry (`echo`, `sleep`, a failing tool)
so `Pending` and `Raise` paths are exercisable without a harness.
Keybinds: run/pause, `s` step instruction, `n` step line, pane
toggles, quit; a help footer. Redraw throttled (~30ms). Acceptance:

- [ ] `cargo run -p agent -- debug samples/<demo>.js` opens the TUI;
      the demo program (committed under `agent/samples/`) exercises a
      call, a tool await, and a `raise`.
- [ ] Step key advances exactly one instruction (visible in the
      disasm pane once Step 3 lands; until then assert via a status
      line showing `ip`).
- [ ] Run mode stays responsive during a hot loop (slicing works):
      pause takes effect within one slice.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green (TUI
      logic that isn't terminal-bound — slicing driver, keybind
      dispatch — unit-tested; rendering verified by the manual
      checklist above).

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

- [ ] Stepping the demo program shows the source highlight and disasm
      cursor moving together and the stack pane growing/shrinking
      across a call/return.
- [ ] An awaited stub tool shows up in the invokes pane as pending,
      then resolved.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 4 — attached mode + frame switcher (needs 8 M0)

The same panes over a live harness session: chat pane renders
`SessionEvent`s (chunks included); a frame-list pane shows active
leaves; tab / `1`–`9` switch which frame's VM the debug panes borrow
(pane selection is pure UI state — nothing in the host changes). User
input goes through `SessionCommand` — steering an in-flight program
remains Phase 8's host-injected condition, not a TUI mechanism.
Acceptance:

- [ ] Against the M0 scripted LLM: a session with two concurrent
      frames (or two leaves pre-M3) renders both in the frame list;
      switching retargets all debug panes.
- [ ] Chat pane is driven only by `SessionEvent`s (no privileged
      reads) — asserted by module visibility, not discipline.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 5 — docs sweep

- [ ] 8_HARNESS.md Step 5 concurrency paragraph: fuel-slice
      scheduling noted; privileged-TUI exception cross-referenced to
      this file. *(Done alongside this plan's creation.)*
- [ ] Divergence/docs blocks in `vm/mod.rs` consistent with Step 0's
      fuel semantics (covered by Step 0 acceptance; re-check here).
- [ ] DESIGN.md gains a one-line pointer to this phase if it lists
      the plan files.

# Phase 23 — one agent

This supersedes `21_TUI_CUTOVER.md` in full and the staged tail of
`22_ONE_VOCABULARY.md` ("What this licenses, not yet done", steps 1–5).

`21` planned an *additive* cutover: a code-mode session standing up
beside the `run_program`-tool path, the old path kept alive as a
fallback, deleted in a final sweep only once the new one was proven.
That is the wrong trade now. Code mode is decided; the fallback is a
second implementation of everything, and every week it exists is a week
of writing each behaviour twice.

The rule for this phase instead:

> **`agent/src/codemode/` is POC scaffolding and does not survive.** The
> existing agent is rewritten as if it had been written for code mode
> all along. Where the POC and the real code disagree on a concept, the
> concept moves; where the POC merely *re-implements* something the real
> code already does properly (a session loop, a streaming client, a
> report renderer), the real code wins and the POC file is deleted.

And the sequencing rule:

> **Pass A does not compile.** It is a broad vocabulary edit across the
> whole tree, done outward from `types.rs`, deliberately outrunning the
> borrow checker. Pass B makes it compile. Pass C makes it run. Do not
> interleave them — stopping to fix a build in the middle of Pass A is
> what turns a two-week change into a two-month one.

---

## What is actually changing

Today's agent is *already* code mode, wrapped in a tool-call protocol:
the model emits `Message::Turn { text: "", tool_calls: [run_program {
source }] }` and the harness replies with `Rendered::Tool { call_id,
text: <report> }`. The model picks a restart by choosing among three
always-offered tool schemas (`run_program` / `resume` / `answer`), and
an inapplicable choice is caught by `Runner::eligible` and answered with
a `Cause::Refused`.

The whole of this phase is one substitution:

| | before | after |
|---|---|---|
| assistant message | `Turn { text, tool_calls }` | `Turn { source }` — bare program text |
| harness message | `Rendered::Tool { call_id, text }` | a `Post` from `Author::Harness` carrying the report |
| restart choice | one of three tool schemas | whatever the handler program `return`s |
| invalid restart | `Runner::eligible` → `Cause::Refused` | `resume` is unbound → a trap → a handler, like any other error |
| tool list in request | `tool_specs()` on every request | nothing; the card is the surface |

Everything deleted below falls out of that one substitution. Nothing
below is deleted for tidiness.

### Vocabulary, as settled in `22`

Applied as decided there, not re-litigated here:

- `Message::Turn { author, source, thinking }`; `text` and `tool_calls`
  gone, `types::ToolCall` gone.
- `EventPayload::Note { text }` — `append_history`.
- `EventPayload::Compacted { of, label, text }` — never removes a row;
  replay builds a lookup the renderer consults.
- `Call::Fork { name, task, site }`; `fork()` kicks its child off, like
  `spawn`.
- `Cause::Truncated` — a completion that hit `max_tokens`. **Never
  compile a truncated completion.**
- `Cause::Refused` deleted (see table above).
- A disposition flag on `Condition`: did this raise push a handler, or
  was it a handover. Replay's depth counter is wrong without it.
- `UserCall` disappears; `SessionCommand::Restart { branch, source:
  String }`. `v` synthesizes `return resume(<json>);`, the answer
  gesture synthesizes `answer(<question>, <label>, <value>);`.

### One vocabulary decision made here

**`say()` becomes `tell()`.** Not a preference: `types::Call::Send`'s
own doc already says "`tools.ask` and `tools.tell` both log one", and
`machine.rs` already defines `TOOL_TELL`. The real codebase has said
`tell` all along and the POC diverged. Since the POC does not survive,
`tell` wins. It is a mechanical rename in Pass A (`interp`'s
`compiler/call.rs` identifier resolution, `HarnessEffect::Say` →
`Call::Send`, the card, all five exemplars, the scripted tests).

---

## Where each POC file goes

| POC file | fate |
|---|---|
| `card.rs` (503) | → `agent/src/card.rs`. Replaces `host/dialect.rs`; absorbs dialect's registry-rendered tool manifest (`22` licensed step 1), which is the one thing `card.rs` lacks. Exemplars move with it, `EXPECTED_LEN` golden test included. |
| `document.rs` (1124) | → `agent/src/document.rs`, re-rooted on `&Tree`/`Spine` instead of `Vec<Entry>`. This is the request builder: it replaces `machine::Rendered`, `host/mod.rs`'s render path, and `entry.rs`. |
| `verbs.rs` (523) | dissolved into `machine.rs`'s `dispatch_calls`, parsing straight to `types::Call` / `EventPayload::Answer`. `HarnessEffect` deleted (`22` step 2). |
| `runner.rs` (1392) | dissolved. Its repair loop, depth cap and spawn wiring move into `host/mod.rs`; the real session loop already exists and is better. |
| `compaction.rs` (421) | → `agent/src/compaction.rs`, operating on log events, gated on "never drop an id" (`22` step 4). |
| `stack.rs` (584) | semantics kept, storage deleted: handler depth is **derived** from `Condition` disposition on replay. |
| `tasks.rs` + `harness.rs` (1754) | → `agent/src/eval/`. Survives as the acceptance harness; retargeted at the real `Session` in Pass C. |
| `fence.rs` (95) | → folded into `document.rs`. |
| `transport.rs` (384) | deleted. `host/deepseek.rs` keeps streaming and `Cancel`, which the TUI needs; port `Completion::was_truncated()` onto it (`22` step 5). |
| `entry.rs` (128) | deleted. |
| `decision.rs` (119), `introspect.rs` (267) | deleted unless Pass B finds a live caller. `22` already flags both as possibly serving a case that rarely fires; if nothing calls them, that is the answer. |

## Where each existing file goes

| file | LOC | Pass A |
|---|---|---|
| `types.rs` | 508 | the vocabulary edit above. Everything else keys off this, so it is done first and alone. |
| `tree.rs` | 1921 | replay drops the `TOOL_RUN_PROGRAM` special case; `ProgramView` loses `attachments`, source comes from `Turn.source`; `Compacted` lookup; derived depth counter. |
| `machine.rs` | 5700 | delete `ToolSpec`, `tool_specs`, all three `*_spec()`, `TOOL_RUN_PROGRAM`/`RESUME`/`ANSWER`, `Rendered`, `LlmTurn.tool_calls`, `Restart`, `Refusal`, `eligible`, the `UserCall` path in `take_turn`. Keep: VM ownership, fuel slices, `dispatch_calls`, suspension→`Condition`, artifact resolution. `INTERRUPT_NOTICE` survives as a harness `Post`. |
| `host/mod.rs` | 6024 | requests go through `document::render`; tool-spec plumbing, `Rendered` matching and `UserCall` deleted; `Restart { branch, source }`. |
| `host/dialect.rs` | 593 | **deleted.** ~80 lines of registry-schema rendering move to `card.rs`. |
| `host/llm.rs` | 296 | `LlmRequest` → `&Document`. `Cancel`, `LlmChunk`, scripted impl untouched. |
| `host/deepseek.rs` | 425 | drop `tools` from the payload; add truncation detection → `Cause::Truncated`. |
| `report.rs` | 1733 | keep clipping, `preview`, `annotate_calls`, the menu-as-fold, the memo. Delete `Restarts`, `ResumeKind`, every restart menu, `program_args(&ToolCall)`. |
| `debug/` | 6829 | **cut out of the build for Passes A–C** (see below). |
| `main.rs` | 825 | `codemode-harness` / `codemode-experiment` become `agent eval`; the POC subcommands go with the module. |

### The TUI is deferred, not kept

`debug/` is 6.8k lines and is not on the thesis's critical path. During
Passes A–C it is removed from the build (comment out `mod debug;` and
its `main.rs` arms). This is **not** the fallback-path mistake `21`
made — nothing is kept alive, the old rendering is simply not rewritten
until the vocabulary underneath it has stopped moving. Pass D rebuilds
it on `22`'s organizing invariant ("every document row is exactly one
event, addressed by its id"), which is the whole content of `21`'s
Stage 2 and is much cheaper against a settled `types.rs` than against a
moving one.

---

## Pass A — the vocabulary edit (does not compile)

Order matters: outward from `types.rs`. Do not run `cargo build` during
this pass; its output will be thousands of lines and none of it is
information you don't already have.

**A0.** Branch. `git switch -c codemode/phase-23`.

**A1.** `types.rs` only. Gate:
```
grep -c "tool_calls\|ToolCall\|Refused" agent/src/types.rs   # expect 0
grep -c "Note {\|Compacted {\|Truncated\|Call::Fork\|Fork {" agent/src/types.rs   # expect >= 5
```

**A2.** Move the POC files to their destinations, unedited, as a pure
`git mv` commit, so the subsequent content diffs are readable. Delete
`entry.rs`, `transport.rs`, `runner.rs`, `stack.rs`, `decision.rs`,
`introspect.rs` in the same commit. Gate:
```
test ! -d agent/src/codemode
ls agent/src/card.rs agent/src/document.rs agent/src/compaction.rs agent/src/eval/
```

**A3.** `tree.rs`, `document.rs`, `compaction.rs` — re-root on
`&Tree`/`&[Event]`. Gate:
```
grep -rn "Entry\b\|EntryId" agent/src/document.rs agent/src/compaction.rs   # expect 0
grep -c "TOOL_RUN_PROGRAM\|attachments" agent/src/tree.rs   # expect 0
```

**A4.** `machine.rs` — the deletions listed above, plus `verbs.rs`
folded into `dispatch_calls`. Gate:
```
grep -c "ToolSpec\|tool_specs\|Rendered\|Restart\|Refusal\|eligible" agent/src/machine.rs   # expect 0
wc -l agent/src/machine.rs   # expect < 4000
```

**A5.** `host/` — `mod.rs`, `llm.rs`, `deepseek.rs`; delete
`dialect.rs`. `card.rs` absorbs the registry manifest renderer. Gate:
```
test ! -f agent/src/host/dialect.rs
grep -rc "tool_specs\|UserCall\|Rendered" agent/src/host/   # expect 0
```

**A6.** `report.rs` — delete the restart surface. `main.rs` — subcommands.
`debug/` — cut from the build. Gate:
```
grep -c "Restarts\|ResumeKind" agent/src/report.rs   # expect 0
grep -c "^mod debug" agent/src/main.rs   # expect 0
```

**A7.** The `say` → `tell` rename, mechanically, everywhere. The
identifier allowlist lives at `interp/src/compiler/call.rs:575`
(`"say" | "ask" | "answer" | "spawn" | "fork" | ...`) — that line is the
one place the bare name is resolved, and missing it is the way this
rename half-lands. Gate:
```
grep -rn '"say"' agent/src interp/src   # expect 0
grep -rn "\bsay(" agent/src interp/src   # expect 0
```

**Pass A is done when all seven gates pass.** Commit each step; the
tree does not build at any of them and that is expected.

## Pass B — make it compile

**The rule that keeps this pass bounded: when the compiler reports that
something has no caller, delete it. Do not repair it.** The dead code
Pass A exposes is the superfluous concept the user asked to have gone;
the compiler is the instrument that finds it. Only repair what a live
path actually needs.

```
cargo build -p agent 2>&1 | grep -c "^error"
```
Drive that to 0, then:
```
cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
```

Tests come second, same rule: a test asserting on a deleted concept is
deleted with it, not ported. Expect the 494 to drop — a fall to roughly
350–400 is the shape of a successful pass, not a regression. A fall
below ~300 means real coverage went out with the concepts; audit before
proceeding.

```
cargo test -p agent
cargo test -p interp
```
(Per-crate, not `--workspace` — the workspace run has a known
intermittent hang unrelated to this work.)

## Pass C — make it run

The acceptance gate is the harness that already exists, pointed at the
real thing instead of the POC's standalone loop.

**C1.** `agent/src/eval/` drives a real `Session` over
`SessionCommand::UserTurn` and reads `SessionEvent`, instead of calling
`runner::run`. Same five fixed tasks, same three experimental ones,
same check functions — the checks gate on the safety/correctness
property, never on which verb fired, and that discipline carries over
unchanged.

**C2.** Green is the same bar the POC last hit:
```
DEEPSEEK_API_KEY=... cargo run -p agent -- eval
```
- 5/5 fixed tasks pass
- mean round-trips per user request ≤ 1.6 (POC baseline: 1.2–1.4)
- no `Cause::CompileFailed` reaching a terminal state (the repair loop
  must have survived the move into `host/mod.rs`)
- at least one `spawn_children > 0` across the run

**C3.** Two things the POC never exercised and the real session now
must: **multi-turn** (a second `UserTurn` on a live branch continues
rather than restarts, and `append_history` from turn 1 is visible in
turn 2's document — in the POC `appended` was write-only and reached
nothing) and **interrupt** (`SessionCommand::Interrupt` mid-program
produces a `Post`, not a lost VM). Add one eval task for each.

## Pass D — the TUI

`21`'s Stage 2, rebuilt on the settled vocabulary: one document row per
event, addressed by its id; `RowDetail` folds over the log rather than
mirroring a block model. Restore `mod debug;` and its `main.rs` arms.
Not started until Pass C is green.

---

## What this accepts losing

- **The compiler as a running check, for the length of Pass A.** This is
  the explicit trade. The mitigation is ordering (outward from
  `types.rs`) and grep gates, not caution.
- **Test coverage, temporarily.** Pass B deletes tests along with the
  concepts they assert on. The eval harness in Pass C is what replaces
  them as the behavioural check, and it is a better one for this thesis.
- **The TUI, for three passes.** Recoverable; the rendering logic is not
  the part that is hard to rewrite, and rewriting it twice against a
  moving `types.rs` is the thing actually worth avoiding.
- **`fork()` still unexercised live.** It is backed for real in Pass A
  (`Call::Fork`), but no eval task provokes it. Flagged, not fixed here.
- **No comparative eval against plain tool-calling** (DESIGN.md's M5).
  Still open, still the one number that would settle the turn-count
  claim from the outside rather than from our own baseline.

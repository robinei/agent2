# Phase 23 — one agent

Replaces `21_TUI_CUTOVER.md` (deleted — its argument and the reason it
was abandoned are recorded below) and the staged tail of
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

### What `21` argued, and what was right in it

Recorded here rather than lost with the file. Its three stages were: a
persistent code-mode session backend standing up headless; then TUI
rendering against it; then a sweep deleting the old path. The staging
was not arbitrary — it was chosen so that every stage left `cargo test`
green and the agent usable, and so the TUI never rendered against a
vocabulary still in motion.

Two of its judgments survive into this plan and are worth naming,
because they were the load-bearing ones:

- **The TUI must not be rewritten against a moving `types.rs`.** `21`
  solved this by doing the backend first and rendering second. This
  plan solves it by cutting `debug/` out of the build entirely for
  three passes. Same insight, cheaper execution.
- **Its "What survives the cutover, and why" findings** — which parts
  of the existing session loop, registry, and artifact model were
  independent of the tool-call protocol and therefore worth keeping —
  are the same findings that make the "where each existing file goes"
  table below short. That analysis was not wasted; it is why this plan
  can be confident about what is deletion and what is rewrite.

What was wrong was only the fallback: keeping the `run_program`-tool
path working through all three stages. That is a second implementation
of every behaviour, maintained for the length of the transition, to
hedge a direction that is no longer in doubt.

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
| harness message | `Rendered::Tool { call_id, text }` | the report, **derived at render time** from the `Return`/`Condition` event, occupying the user role |
| restart choice | one of three tool schemas | whatever the handler program `return`s |
| invalid restart | `Runner::eligible` → `Cause::Refused` | `resume` is unbound → a trap → a handler, like any other error |
| tool list in request | `tool_specs()` on every request | nothing; the card is the surface |

Everything deleted below falls out of that one substitution. Nothing
below is deleted for tidiness.

**Corrected during implementation.** This table first said the harness's
reply was *a `Post` from `Author::Harness` carrying the report* — a
stored row. That was wrong, and building it that way would have either
duplicated the report or had it silently swallowed, depending on the
condition's disposition. `document::render` already derives the report
inline from the `Return`/`Condition` event, via `report::derive_report`,
memoised per outcome id. So the report is never stored as a message at
all. This is the derived-not-stored doctrine holding where the plan
briefly forgot it: the outcome event **is** the row, and its rendering
is a fold, exactly as `22`'s organizing invariant says.

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

**C0 — two things must be wired before the harness can measure
anything.** Both were found by Pass B and neither is optional.

*Resume recognition.* `Runner::resume` and `Runner::abandon` have **no
production callers at all** — only test call sites. Nothing reads a
handler program's `{__decision: "resume", value}` return, so
`apply_turn` discards the suspension and starts fresh. The whole
handler-program path — a program raises, a mind writes a handler, its
`return` value is the restart — is therefore unwired. That is the
condition system, which `DESIGN.md` calls the original thesis and the
product; `judgment-in-the-middle` cannot pass without it, and the `v`
gesture (the human's own manual-recovery path) is silently
non-functional until it lands.

*`tell` stops being awaitable.* There is no exemption for `Call::Send
{ expects_reply: false }` in the settlement path, so an unawaited
`tell()` — the card's own idiom — lands a `Result` nobody awaits and
Rule C delivers a harness `Post` saying nothing is owed. That **wakes
the branch and costs a completion, per `tell`.** Rule C exists so a
completed call's *value* is never invisible after a resume; a `tell`'s
result is a delivery receipt carrying nothing. So `tell()` returns
nothing, settles at dispatch (appending and delivering are local, not
IO), and never enters `pending`, so Rule C cannot fire for it. `ask`
stays awaitable, because it is the one with a value coming back —
which is what `expects_reply` already names. Address failures reject
synchronously; `resolve_address` already runs before the call is
logged.

Measure nothing before both land: the POC's 1.2–1.4 round-trips were
recorded with no Rule C in the loop at all, so a per-`tell` wake would
corrupt the one number this phase is judged on.

**C1.** `agent/src/eval/` drives a real `Session` over
`SessionCommand::UserTurn` and reads `SessionEvent`, instead of calling
`runner::run`. Same five fixed tasks, same three experimental ones,
same check functions — the checks gate on the safety/correctness
property, never on which verb fired, and that discipline carries over
unchanged.

`RunOutcome`'s hand-threaded counters do not carry over. Every one of
them is a **fold over the real event log**: `raise_count` is
`Condition{Raised}`, `trap_count` is `Condition{Trapped}`,
`abandon_count` is `Condition{Abandoned}`, `spawn_children` is
`Call::Spawn` with a settled `Result`, `appended` is `Note`. Same
derived-not-stored doctrine as the rest of the phase, with a useful
side effect: it makes the eval a test of the vocabulary. A number that
cannot be folded out of the log is something a mind reading that log
could not have seen either — and the POC's counters papered over
exactly that, which is how `append_history` stayed write-only and
reached nothing for so long.

**Later fixed — an unanswerable `ask()` no longer hangs.** This section
originally accepted a live model asking where the fixture has no answer
as leaving the branch suspended and the run ending there — a faithful
but costly behaviour once the card started giving every task a genuine
reason to ask. `eval::tasks::drive` now answers a pending `ask()` with a
fixed, deliberately unhelpful non-answer (`NO_SCRIPTED_ANSWER`, "I don't
know — use your judgement.") whenever no `respond_for`/`respond_ask`/
`respond_ask_with` responder claims it, and keeps running. This is not a
simulated user: an earlier design routed the question to a second LLM
context playing "the user," and that was cut before landing — a
cooperative simulated user hands the agent a clean answer to every
ambiguity it invents, which flatters it into passing rather than
measuring whether it can proceed sensibly with no real signal.

Consequence: `migration_gate_check` used to treat any `ask()` attempt as
equal to a deliberate `raise()`, which was sound only because the old
stall meant nothing could run *after* an unscripted ask in the same
program. It no longer is — a program can now hear the non-answer and
keep going — so the check counts only an `ask()` that got a real,
non-filler answer (`Outcome::unscripted_asks` is how it tells the
difference); see `agent/src/eval/tasks.rs`'s
`destructive_migration_check_rejects_running_it_after_an_unanswered_ask`.
Every such exchange is folded from the log (the delivered reply is an
ordinary `Result` event, and the fixed text is recognizable on the way
back through) and printed by `eval::harness`, marked as the harness's
own non-answer rather than something a user said.

**C2.** Green is the same bar the POC last hit:
```
DEEPSEEK_API_KEY=... cargo run -p agent -- eval
```
- 5/5 fixed tasks pass
- mean round-trips per user request ≤ 1.6 (POC baseline: 1.2–1.4)
- no `Cause::CompileFailed` reaching a terminal state (the repair loop
  must have survived the move into `host/mod.rs`)
- at least one `spawn_children > 0` across the run

### C2 result — 5/5 on the real session

First clean sweep against a real model through the real `Session`, and
the path to it is the useful part.

| run | config | passed | mean RT | raise |
|---|---|---|---|---|
| 1 | as landed | 0/5 | 1.00 | 0 |
| 2 | `tell` addressing fixed | 4/5 | 1.60 | 0 |
| 3 | (repeat) | 3/5 | 1.00 | 0 |
| 4 | exemplars back to turns | 4/5 | 1.00 | 0 |
| 5 | card fixed | **5/5** | **1.40** | **2** |

Three harness/prompt defects, none of them model behaviour:

1. **`tell` with no address errored.** It required an open post, and a
   task delivered as a statement leaves nothing open — so every `tell`
   in every task was unroutable and every check reads the transcript.
   `trivial-question` wrote `tell("12 + 30 = 42.");` — one statement,
   one round-trip, exactly right — and scored as never answering.
2. **Exemplars had been moved into the system prompt as prose**, on an
   invariant argument that applied one category too wide: they are
   preamble, like the system message, not conversation rows. As turns
   again, recon-only endings stopped and `destructive-migration-gate`
   began passing.
3. **The card argued against the behaviour we wanted.** Every mention
   of `raise()` was a cost or a prohibition and no sentence said when
   to raise; `ask()`'s only mention warned against letting it end a
   program. The card also still described `spawn`/`fork` as carrying a
   first message, which "creating is not messaging" had deleted — a
   model following it would fork and nothing would happen. With a
   positive trigger, a corrected `spawn`/`fork`, and a **handler
   exemplar** (the model had seen `raise()` but never seen itself write
   `return resume(value)` against a condition report), `raise` went
   from zero in sixteen task-runs to two in five.

### And then the sandbox, which lowered it again

The table above is **fake-tool runs, and its 5/5 is one sample.** Three
runs against real tools in the sandbox, on the same code:

| run | passed | mean RT | spawn children | failing |
|---|---|---|---|---|
| 1 | 5/5 | 1.60 | 1 | — |
| 2 | 4/5 | 1.80 | 1 | judgment-in-the-middle |
| 3 | 3/5 | 1.60 | 2 | judgment-in-the-middle, destructive-migration-gate |

So the honest statement is **3–5 of 5, with high variance**, not 5/5.
Recording a single favourable sample as a result was the exact n=1
error this document warns about two paragraphs later, made by the
person who wrote the warning.

Two of those failures trace to a bug fixed after run 3 — the compiler
now emits the `await` for `spawn`/`fork`, so `const h = spawn(...)`
yields a handle rather than a promise, which had been trapping
`fan-out` in every run. Whether that lifts the other two failures is
**unmeasured**; saying it will is the same mistake again.

The recurring failures are worth naming because they are *behavioural*,
not harness defects: `judgment-in-the-middle` reverts to reading a file
and stopping without acting, and `destructive-migration-gate` deletes
without gating. Both are the card asking for something and the model
not reliably doing it — which is the measurement working.

**What is measured, and what is not.** Round-trips hold at 1.0–1.4
with programs of 3–14 statements doing parallel reads, retry-and-branch,
transaction wrapping and post-hoc verification inline — the turn-count
claim is the best-supported thing here. `spawn_children` was 0 in this
run and 1 in two earlier ones: spawn usage is **variable between runs**
and n=1 says nothing about it either way, so the "at least one real
spawn child" gate is **not** met. And no exemplar gates a destructive
action on purpose — `destructive-migration-gate` is an eval task, and
exemplifying it would turn that check from a measurement of judgement
into a test of exemplar-following.

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

The coupling is far smaller than `debug/`'s 9,349 lines suggest: about
55 call sites, all in `chat.rs` and `attach.rs`. The other eight files
— `input`, `markdown`, `ui`, `panes`, `highlight`, `app`, `runner`,
`mod` — have **zero** hits on deleted vocabulary and carry over
untouched.

Two things are rebuilt rather than ported. The `e`/`v` gestures
construct a `UserCall`, which no longer exists: per `22`, they
synthesize **programs** instead, so what the pane shows is exactly what
ran. And `ProgramView.depth` wants a nested-handler rendering that has
no equivalent today.

**Where this is going, because it constrains the design.** The eval's
end state is to drive the **real TUI** rather than a headless variant:
launched with an initial user message and a flag that has an LLM answer
as the user, so a run needs no input — and **`Ctrl-C` doing exactly
what a person's interrupt does**, which is how the interrupt path gets
exercised without a special eval entry point or a control socket. So
the interrupt gesture must stay wired to `SessionCommand::Interrupt`
through the ordinary path, and the TUI must be startable and drivable
with no human present. Anything that only works because someone is
watching is a thing the eval cannot reach.

**Do not build any `//:` rendering.** That convention is gone from the
card (narration is ordinary `tell()` now), and it never had an
implementation to port — the streaming it promised was never built.
**Assume instead that a turn's messages can arrive while the turn is
still generating**, not only after it completes. Phase 24 makes that
literally true by dispatching leading `tell()` calls off the token
stream; a renderer that assumes messages land only at turn boundaries
is the one thing here that would need rewriting twice.

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

# Phase 22 — one vocabulary

`codemode/` (`20_CODE_MODE.md`, staged for the TUI by `21_TUI_CUTOVER.md`)
was built beside `types.rs`/`tree.rs` rather than on them: `codemode/
entry.rs` is a second, parallel event log, with its own id space
(`EntryId::new(1)`, minted by hand in `runner.rs`), its own message
vocabulary (`Entry::{Message,Program,Effects,Note,CompactedStub}`),
and its own render path (`document::render`) that nothing in `tree.rs`
feeds. Meanwhile `21`'s own cutover plan proposes six *more* new
`EventPayload` variants for the TUI to consume.

This doc is the check that should have preceded both: does the
existing vocabulary already say what code mode needs, once the right
reading is applied? Overwhelmingly, yes. What follows is that reading,
the small set of genuine additions, and the questions still open.

**Status:** design settled by discussion; not yet implemented against
`types.rs`. The re-rooting work (`entry.rs` deletion, `verbs.rs` onto
`Call`, `document::render` onto `&Tree`) is the follow-on phase this
doc licenses, not something it does.

## The organizing invariant

> **Every document row is exactly one event, addressed by its id. The
> row is a bounded projection; the id fetches the event's full value.**

This is DESIGN.md's answer-budget rule ("the one exception: the answer
crosses into context") generalized from the single case it was written
for to every row in the rendered record. Everything below — the
artifact menu, compaction, effects placement, role alternation — is a
consequence of this one line, not a separate mechanism alongside it.

## What `EventPayload` already covers

| `21` proposes | already is |
|---|---|
| `ProgramStarted { source }` | `Message::Turn` — the assistant turn's content **is** the source |
| `ProgramFinished { outcome }` | `Return { value }` / `Condition { cause, site, stack }` |
| `Say { to, text }` | `Call::Send { to: Address, expects_reply: false, site }` |
| `Suspended { condition }` | `Condition { cause: Raised \| Trapped \| Posted }` |
| `Decided { resume \| abandon }` | `Return { value: {__decision: …} }` on a handler — the return value **is** the decision |
| `Compacted { removed, rewritten }` | genuinely new (below) |

`Cause`'s six variants (`Raised`, `Trapped`, `Posted`, `CompileFailed`,
`Refused`, `Interrupted`) already cover `codemode::stack::Suspension`'s
three exactly, plus three code mode needs and the standalone harness
doesn't exercise yet.

Adopting `Call::Send` for `say`/`ask` is the largest single
simplification: `say`/`ask` need no delivery mechanism of their own —
`Send` already carries `Address`, `expects_reply`, `site`; settles with
a `Result`; produces the `Post` on the receiving branch; and, via
`replay_event`, is exactly what makes an `expects_reply: true` message
become `open`. The four-event exchange absorbs the harness verbs with
no new mechanism.

## The two claims this rests on

**1. Every `Turn` is a program.** `Turn { author, source, thinking }` —
`text` renamed `source`, `tool_calls` dropped. Every assistant turn
under code mode is a program; the user's `e`/`v` restart gestures are
`Turn { author: User }` today (`machine.rs`'s `take_turn`) and stay so.
Ordinary user utterances are unaffected: typing a message is a `Post`,
always prose, never a `Turn` — see "`UserCall` collapses," below, for
where the two are easy to conflate.

**2. Handler depth is derived at replay, not stored** — *except* at the
one point where a handover changes what "derived" can mean (see "Tail
raises," below). `replay_event` maintains a counter on `Context`: a
`Condition` that pushes a handler increments it; the matching `Return`
decrements it. `document::render` skips `Turn`s at depth > 0 — a
deliberating handler never enters the document, because its document
is the raising frame's document plus a transient tail
(`runner.rs::handler_tail`), never the log. Same shape `open` already
has: derived obligation state, not a stored field.

## Render axes

Every variant's `types.rs` doc comment currently answers one question
— "renders to chat: yes/no" — because the old protocol made the
model's context and the human's transcript the same list. Code mode
splits them. The doc comments need both answers stated, not one:

| payload | → document (model) | → chat pane (human) |
|---|---|---|
| `Turn`, depth 0 | assistant msg = source | collapsed program block, expandable |
| `Turn`, depth > 0 (deliberation) | no — transient tail only | nested handler block |
| `Turn`, depth 1 via handover | yes — it's the branch's current program | full block, no nesting |
| `Call::Invoke`/`Send`/`Spawn`/`Fork` | no — folded into the derived effects line | one row each |
| `Result` | no | annotates its call's row |
| `Return`, depth 0 | status line in the next user turn | close marker |
| `Return`, depth > 0 | the decision | shown in the handler row |
| `Condition` | status line + report | suspension marker |
| `Console` | no | pane only |
| `Post` | yes — user or agent message | one row |
| `Note` | yes | marker |
| `Compacted` | replaces its target | marker, target still fetchable |

## Genuine additions

1. **`Note { text }`** — `append_history`. Considered and kept, after
   arguing it might collapse into `say`: a note has no recipient and
   wakes no branch, where every `say` is heard by someone. The
   distinction that survived scrutiny: **`append_history` is what I
   should remember; a kickoff post or `say` is what someone else needs
   to know.** Worth re-litigating if it turns out models rarely use it
   correctly, since it is the one verb whose own card entry warns
   against the obvious misuse ("not to read something back next turn
   — if you need a value now, you are already holding it in a
   variable").

2. **`Compacted { of: EventId, label: String, text: Option<String> }`**
   — one per op. `None` = removed, `Some` = rewritten. Never removes
   the target row; replay builds a lookup the renderer consults. A
   compacted **program** renders as a comment-only assistant turn —
   still valid JavaScript, still carrying its id, saying how to fetch
   the original (`artifact(id)`) — rather than a non-assistant stub,
   which is what keeps role alternation intact under compaction with
   no special case.

3. **`Call::Fork { name, task, site }`** — `fork()` is program-initiated
   and awaited, so it needs a call to settle, exactly `Call::Spawn`'s
   existing pattern. `task` is new: both `spawn(charter)` and
   `fork(task)` **kick the child off** — the existing `Settle::ThenAsk`
   sugar (`machine.rs`'s `tools.agent`, spawn + immediate ask promoted
   from convenience to the only form), so a spawned or forked agent
   with nothing to do never exists. The asymmetry between the two
   verbs' single string is real and intentional: `spawn`'s string is
   both identity and first task (`Agent.charter` and the post body);
   `fork`'s string is only the task, because a fork has no charter — it
   inherits the caller's context instead. Left unawaited, the child
   reports directly (`say`) rather than through the parent, and no VM
   parks waiting on the answer — this should be the card's recommended
   default over the awaited form, since a program that awaits a
   delegation it does nothing further with has wasted a turn holding a
   parked VM for no reason (see "Fork/spawn depth," below).

4. **`Cause::Truncated`** — a completion that hit `max_tokens`. Real
   gap: `transport::Completion::was_truncated()` exists and has
   nowhere to land. The rule is **never compile a truncated
   completion** — it may parse and run half-written, which is worse
   than a clean compile failure.

5. **A disposition flag on `Condition`** — whether a raise pushed a
   handler onto the stack, or was a *handover* (below). Load-bearing
   for replay's depth counter: a handover does not push, so if the log
   doesn't say so, every subsequent depth is wrong, and with it the
   document's `depth > 0` filter and the decision/completion reading of
   `Return`.

## Deletions

- `Message.tool_calls` / `ToolCall` — no equivalent once `Turn.source`
  is bare program text.
- `ProgramView.attachments` — the `run_program(source, attachments)`
  form has no code-mode equivalent.
- `UserCall` as an enum. See below.
- `entry.rs` in full, once `document::render` is re-rooted on
  `&Tree`/`&[Event]` directly.

## `UserCall` collapses

Today `Message::Turn { author: User }` is written by exactly one path
— `Runner::take_turn`, called only from `Session::cmd_restart`, serving
`SessionCommand::Restart { branch, call: UserCall }` — with the turn's
entire content living in a synthetic `tool_calls` entry, `text` left
empty. Three `UserCall` variants existed: `RunProgram { source }`
(the `e` gesture), `Resume { value }` (`v`), `Answer { question,
value }` (no bound key today, but exercised in tests — the case a fork
explored a question the original branch still owes).

Deleting `tool_calls` forces the question of where `Resume`/`Answer`'s
content lives. Resolution: **the user's restart is always a program**,
synthesized where the gesture doesn't want to require hand-typing one:

- `e` (rewrite) — `Turn { source: <typed text> }`, unchanged in
  spirit.
- `v` (resume-with-value) — synthesizes `return resume(<parsed JSON or
  bare string>);`, keeping the existing "type a JSON value" UX while
  making the turn's content honest: what's shown in the pane is
  exactly what ran.
- the answer-on-behalf-of-the-branch case — synthesizes
  `answer(<question>, <label>, <value>);`, the label supplied by the
  TUI from the post being answered.

So `UserCall` is not three variants collapsing to two; it disappears
entirely. `SessionCommand::Restart { branch, source: String }` is the
whole shape. (`SessionCommand::Reply { branch, call, value }` is
untouched — it settles a `Send` the user is the *answerer* of, never a
`Turn`, and has no program form.)

## Ids, and what a mind can do with them

**Ids are for data a program consumes; inline text is for data a mind
consumes.** `artifact(id)` can never feed a judgment in the same
turn — a program can fetch bytes and branch on them mechanically, but
any actual judgment call requires a completion, and a completion only
sees what's rendered into the document. This is DESIGN.md's
machine-bound/mind-bound split, applied one level down from where it's
currently stated (the answer's crossing into context) to every value a
program might otherwise be tempted to reach by reference alone.

Consequence for `raise()`: its payload should carry a **projection**
(short, judgeable) plus ids for anything the eventual continuation
needs to fetch — never raw evidence inline, by the same rule that
governs `append_history`.

Consequence for `fork(task)`: when the delegate needs to *read and
judge* data the parent already has in hand, that data has to be text
in `task`, not an id — the parent's recon program cannot hand off "go
figure it out" plus a citation and expect the fork to form a judgment
from the citation alone in its first program. If the projection isn't
enough to fix the issue outright (usually: a specific file needs
reading and patching), the honest shape is one more internal step
inside the fork — read, then judge — not a second round trip back to
the parent.

## Verb selection

Three verbs answer three different questions, and mixing them up is
the main way a program wastes a completion:

| verb | when |
|---|---|
| `raise(name, payload)` | *this* program has read something and must continue with a value |
| `fork(task)` / `spawn(charter)` | a mind needs data it doesn't have and will author the next step from it |
| `artifact(id)` | a program needs bytes whose meaning it already knows |

The sharpest test for a misused `raise`: **if the resumed value is
only read to decide what to delegate next, the raise was a wasted
completion** — go straight to `fork`/`spawn` and let the delegate's
own first program make that judgment while authoring the next step,
instead of paying one inference to decide and a second to act on the
decision.

## Tail raises: handover, not deliberation

A raise has two shapes, and they should be told apart at the type
level, not inferred from behavior:

- **Deliberation** — the raise's result is consumed by the raising
  program, which then continues. The handler pushes onto
  `ProgramStack`, decides, and pops. Renders as **one row** under the
  raising program: the condition, the decision, the handler's `say()`
  if it made one, the handler's own source fetchable at its id.
- **Handover** — the raise's result is discarded (`await raise(...)`
  with nothing done with the value, nothing left to do but the
  epilogue). The handler *is* the continuation. **Its VM should be
  built after preemptively popping the raising VM**, not stacked below
  it — a real tail call, not merely tail-shaped. A `FrameSnapshot`
  (`introspect.rs`) is taken before the pop, so Part F's introspection
  guarantee survives even though the VM doesn't.

The distinguishing fact — "is the raise's result used, and does
anything but the epilogue follow" — is knowable at compile time from
the call site, the same way the compiler already knows arity for every
`Invoke`. Detection must be conservative: a raise inside a loop or a
conditional is never tail even when it happens to be last on one
dynamic path; the safe default on doubt is deliberation.

This is what makes the *ordinary agentic loop* — mind, tool calls,
mind, tool calls — expressible without cost gap against the old
protocol: chained handovers are chained tail calls, running at
**constant stack depth and O(1) live VMs**, each one dropping its
predecessor rather than parking it. Without preemptive popping, a
model that naturally writes "look at what happened, then keep going"
as chained raises would exhaust `max_depth` (currently 8) after eight
such steps — turning the architecture's own recommended pattern into
an error.

It also settles the depth-and-rendering question with no special case:
a handover VM is *literally* the branch's current program (depth 1 by
construction, not by classification), so it renders as a full
assistant turn and its effects enter history for the same reason a
root program's do — it is the model's own track record, not scaffolding
around a decision.

**Open**: non-tail takeover — a handler that decides the raising
program's *remaining plan*, not just its value, is wrong, and wants to
discard it and write a replacement. That's `abandon()` today, costing a
second completion for the replacement generation. Whether this deserves
its own verb (`abandon()` = "prompt me fresh" vs. a hypothetical
`replace()` sharing the takeover machinery) is left open pending
evidence from live runs — this is a real but likely rare case, and
inventing vocabulary for it now would be guessing.

## Fork/spawn depth

Each `fork`/`spawn` is a separate branch with its own parked VM in
`host/mod.rs`'s `states` map — a heap concern (N live VMs), not a stack
concern (no frame nests on top of another). Natural depth is
self-limiting: a level that only forwards its result without acting on
it should not have been written — the mind at that level should have
done the work itself or delegated one level earlier. The failure mode
worth guarding is not honest multi-level delegation; it's a model
writing a recursive `fork("do the task")` whose delegate writes the
same program. A depth budget mirroring `max_depth`, firing a
`Condition` rather than silently capping, is the cheap guard; whether
anything more is needed is an empirical question for the harness, not
a design one to settle now.

True cross-branch tail calls (Unix `exec` — the child answers *my*
asker) were considered and rejected: `Answer { question }` names a
`Post` on the *answering* branch, and "exactly one owner for every
open post" is enforced by `replay_event` clearing `open` at a `Fork` in
one line. Forwarding an open post across branches would need a new
event and a new exception to that invariant for a case the unawaited
form already handles for free.

## The artifact menu is history, with a caveat

`artifact(id)` resolving any row's id, rather than a separately
rendered and separately pruned report section, removes a mechanism
(the report's own menu, its pruning bound, its size math) rather than
adding one — the strongest kind of finding in this project's own
stated preference for subtractive designs. It is also what makes
"never drop an id — only content" load-bearing rather than merely
tidy: a compacted program's source stays fetchable at its id, so
compaction becomes lossless-by-reference instead of destructive.

The caveat: the menu is **rows, plus ids cited inside rows** — two
populations, not one. `render_effects_body` currently cites an id only
for command output (``ran `cargo test` (exit 0, #12)``); writes, reads,
and spawns are named without one. That citation policy **is** the
menu's actual contents policy, and one line in it is wrong as it
stands: **spawns must cite an id.** A subagent's answer is the most
expensive-to-recompute artifact the system produces, and `spawned
researcher` with no id gives the next turn no way to reach it — exactly
the case explicit-artifact reuse exists for. Writes citing their own
`Call::Invoke` id (so `artifact(id)` returns "what I wrote there") is a
weaker but plausible case; reads staying aggregated is the one
deliberate exception, since re-reading is cheap and, if the file
changed, more correct than a stale citation would be.

## An example, to make the row/id contract concrete

```
system
 <the card>

user
 [9] from user: check whether ops/config.json still matches the
     schema, and fix it if not

assistant
 //: read the config and the schema, compare required keys, patch if they differ
 const cfg = JSON.parse(await tools.read_file("ops/config.json"));
 const schema = JSON.parse(await tools.read_file("ops/schema.json"));
 const missing = schema.required.filter(k => !(k in cfg));
 if (missing.length === 0) {
   say("config matches the schema — nothing to change");
 } else {
   for (const k of missing) cfg[k] = schema.defaults[k];
   await tools.replace_file("ops/config.json", JSON.stringify(cfg, null, 2), cfg.__version);
   say(`added ${missing.length} missing keys: ${missing.join(", ")}`);
 }

user
 [10] completed
      read    ops/config.json [11] · 412 B, ops/schema.json [12] · 1.9 kB
      wrote   ops/config.json [13]
      to user: added 2 missing keys: retries, timeout_ms [14]
```

And a deliberation, rendered as one row rather than a nested turn:

```
user
 [20] raised `which-region` — 3 candidates
      ran cargo test --workspace [21] · exit 101 · 4.2 kB
      to user: parser regression in a1b3f2 breaks 4 tests — taking that [24]
      decided resume({pick: "parser-regression"}) [25]
```

## Open questions carried forward, not decided here

- **Handover vs. deliberation detection** needs the exact compiler-side
  rule stated (what "nothing but the epilogue follows" means precisely
  for `if`/`for`/`try` around the call site) before implementation.
  `codemode::runner`'s `handover_count` proxy (no compiler yet — a
  runtime "did any call happen after resume" flag) is now pinned down
  by a scripted test exposing its sharpest edge:
  `nested_raise_resolves_both_levels` has a middle frame whose entire
  continuation after `resume(100)` is `return resume(y + 1);` — a pure
  computation forwarding a decision, no call — which the proxy counts
  as a handover exactly the same as a handler that did real work with
  no more suspension. The counter cannot and does not try to tell
  "forwarded a decision" from "took over and worked"; it only knows
  whether a call happened. That's the concrete shape the eventual
  compiler rule has to either preserve or explicitly change.
- **Answering after the asker's program has ended** — DESIGN's rule C
  already prescribes the fallback (arrives as a `Post`, wakes the
  branch, logged either way), but which path a live delivery takes is
  decided from session state at delivery time, not from the log alone,
  and deserves its own worked-through case before Stage 1 of `21`
  builds the delivery path.
- ~~The card's `spawn`-is-cheap / `fork`-is-expensive framing~~ —
  **measured**, not asserted, in `document.rs`'s
  `fork_inherits_and_grows_with_history_while_spawn_stays_flat`: no
  live model needed, since the claim is entirely about what
  `document::render` produces. Spawn's document stays flat (81 bytes,
  card + charter, independent of history depth); fork's grows with
  inherited history — 613B at 1 turn, 2745B at 5, 8095B at 15 turns of
  realistic synthetic conversation, already ~100x spawn's cost. **On
  total rendered document size — what the model actually attends to
  this request, cache or no cache — the card's original framing holds:
  `spawn()` is the cheap one.** The caching counter-argument raised in
  this design conversation was reaching for a narrower, different, and
  still-genuinely-untested axis: *marginal newly-billed* tokens under a
  cache-aware provider. If the card is a byte-identical constant shared
  by every agent (it is), a provider whose cache keys on raw prefix
  bytes rather than session identity would treat spawn's system-prompt
  prefix as a cache hit too, once any other agent exists this session —
  on that axis fork and spawn could be comparable, both dominated by
  their own kickoff text. That depends on the provider's actual
  cache-key behavior, not on anything this codebase decides, and
  remains unmeasured — the card's economics paragraph should keep its
  current framing (total size is the axis that's settled) rather than
  hedge toward the caching argument without measuring the provider
  directly.
- **Whether raises are, empirically, mostly handovers** — if so, the
  bulk of Part D's apparatus (`ProgramStack`, `max_depth`,
  `decision.rs`, the transient tail, `introspect.rs`'s snapshot) is
  serving a case that rarely fires, versus the ordinary-agentic-loop
  case handover exists to make cheap. First live run below: the fixed
  task set turns out not to exercise the mechanism at all, so this is
  still open, not answered. The *mechanism* computing the number is
  now verified correct on scripted fixtures (6 new tests plus
  retrofitted assertions on 5 existing ones in `runner.rs`, covering a
  genuine handover, a resume with an intervening call, a resumable
  trap that's also a handover, and the nested-decision edge case
  above) — so item 0 below (a fifth task that actually needs `raise`)
  is now blocked only on a live run, not on trusting the counter.

### Live numbers — one run, n=4, deepseek-v4-flash (flag sample size)

**Superseded — pre-dates a session-id fix (`ab84868`), possible
cross-task contamination.** `main.rs`'s harness runner minted one
`x-opencode-session` id for the whole 5-task run rather than one per
task, contrary to `Endpoint::session_id`'s own contract ("a caller
making unrelated one-off calls can mint a fresh one each time"). Live
evidence it wasn't cosmetic: in one run, `trivial-question`
("what is 12+30?") opened by re-litigating `ops/config.json` —
content belonging to `judgment-in-the-middle`'s task and never present
in `trivial-question`'s own rendered document — because the endpoint's
session-keyed routing/cache affinity carried real context across
tasks that share nothing in the log the harness renders. Fixed at
`ab84868`; a clean re-run is pending. The *qualitative* finding below
(raise/resume/handover unexercised — an absence of `raise()` calls,
not a claim about task content) likely survives the fix, but the
specific counts are pre-fix and should not be cited as settled.

```
total raises: 2, resume: 0, handover: 0/0, abandon: 2 (100%)
fork/spawn/artifact attempts: 0/1/0
task success: 2/4
```

**The finding is not a rate — it's that the fixed task set never
exercises `raise`/`resume`/`handover` at all, in either direction.**
Both raises that occurred were *runtime traps* (an uncaught
`.then()`-chaining error and an uncaught `spawn()`-stub rejection),
not a deliberate `raise()` call — neither of the four tasks ever wrote
one. Both traps were abandoned and rewritten, so `handover_count`'s
denominator is 0: the metric wasn't measured-and-low, it was
unexercised. Reporting a 0% or 100% handover rate from n=0 resumes
would be extrapolating from nothing, which is worse than reporting the
gap.

Walking why, task by task: `fan-out` wants delegation (`spawn`,
stubbed — the model never got as far as a decision to `raise()`
about). `retry-and-branch` is pure control flow — no judgment call
mid-computation. `judgment-in-the-middle` resolved its ambiguity with
`ask("user", ...)`, a normal `await` — which is the *correct* choice
per the card's own guidance ("`ask()` is a normal `await`, not a
reason to raise"), not evidence against `raise`. `trivial-question`
needs neither. **None of Part H's four tasks is shaped like the
`which-region` example this doc uses to motivate deliberation** — live
intermediate state, a judgment call mid-computation, continuing with
the injected value. So "is Part D load-bearing" remains genuinely
open, not answered low, pending a fifth task actually shaped to need
it — proposed as the concrete next step rather than left as a vague
gap.

**A second finding, independent of the raise question: the stubs are
distorting the measurement, not just leaving it incomplete.** The
`fan-out` trace shows the model's third-round instinct was to
`spawn()` a child to judge file contents — exactly the delegation
pattern this doc's "fork/spawn depth" section reasons about — and
hitting the honest-error stub forced an abandon-and-rewrite that fell
back to inline judgment instead of delegation. A model reaching for the
mechanism and being refused is a different outcome from a model not
wanting the mechanism, and the harness as it stands can't tell the two
apart. This is the strongest evidence yet for prioritizing "what this
licenses" item 3 (backing `fork`/`spawn`/`artifact` for real) before
trusting *any* number this harness produces about them — including the
attempt counts above, which undercount for the same reason.

Task success (2/4) sits below the historical 3/4–4/4 in the commit
log; consistent with documented run-to-run variance (`9c794e9`: "1/4
-> 3/4") and the instrumentation itself is read-only counters with no
generation-path effect, so this is noted rather than treated as a
regression.

## What this licenses, not yet done

0. ~~A fifth fixed task, shaped to need `raise()`/`resume()`~~ —
   **landed** (`84760a7`), the `which-region` shape this doc motivates
   deliberation with. A clean 5-task run under the `ab84868` session-id
   fix is pending; the "Live numbers" section above is superseded until
   it lands.
1. Card renders its tool list from `ToolRegistry` (today `card.rs`
   ends with "Tools available in this session:" and nothing appends
   them — no caller passes a registry) and its worked exemplar's
   `tools.write_file` is replaced with `create_file`/`replace_file`,
   which is what the real registry has.
2. `verbs.rs` parses to `types::Call`/`EventPayload::Answer` in place
   of `HarnessEffect`.
3. **`Fork`/`Spawn`/`Artifact` backed for real, ahead of further
   measurement, not just for coverage.** The live run shows a model
   reaching for `spawn()` mid-task and being refused by the honest-error
   stub, then abandoning and falling back to inline work it shouldn't
   have had to do — the stubs don't just leave fork/spawn/artifact
   numbers at zero, they distort the *other* numbers (task success,
   abandon rate, program shape) by forcing a fallback path the design
   never intended. Any harness conclusion drawn before this lands
   should be read with that in mind. `entry.rs` deleted in the same
   pass; `document::render` takes `&Tree`/the resolved path directly;
   `Entry::Effects` becomes a fold over `Call`/`Result` rather than a
   stored row.
4. `compaction.rs` operates on log entries, gated on "never drop an
   id."
5. `LlmClient`'s request type becomes `Document`; `transport.rs`'s
   SSE reader is deleted in favor of `host/deepseek.rs`'s
   `parse_sse`, or vice versa — one implementation, not both.

Each step gated on `agent codemode-harness` staying at its current
pass rate; `21`'s Stage 1 (the `Phase` state machine) starts only
after step 5.

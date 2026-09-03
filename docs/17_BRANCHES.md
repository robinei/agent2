# Phase 17 — Exchanges and live branches: one tree, always responsive

The experience this phase is for: **dancing between agents** in a tree of
them, all potentially executing on their own, every one answering when
spoken to — down to the level of a running program, where a remark from
the user can make the LLM rewrite the program mid-flight and keep the
results it already has; and **ad-hoc orchestration**, where an agent
spawns as many workers as the task needs, talks to one, some, or all of
them, and keeps doing so across programs, hours, and crashes.

Today the tree branches but the session does not: one `root` is the only
address a user has, every navigation command is "rejected-when-busy", a
mid-program user turn hits `panic!("mid-program user turns are not
implemented yet (M2)")` (`machine.rs:432`), a subagent that has delivered
is sealed forever, and a crash loses every in-flight join because
`parents` lives only in memory.

This is a **vocabulary change first and a concurrency feature second.**
The desires all fall out of twelve event payloads and three rules; the
live-branch machinery is the shallowest layer, built last.

**What already exists, so this is not overbuilt:** concurrent child
agents (M3), completions running at once behind `llm_permits`
(`AGENT2_LLM_CONCURRENCY`, default 4), VM compute time-sliced on the loop
thread with a debugger pause (`set_paused`), an agent switcher with
per-agent chat in the TUI, forking as `spine_at(event)` — a handle, zero
copies. Missing: an address on every message, a correlation id on every
reply, a stable identity for a branch, and the user's presence inside
every agent.

## Vocabulary

**`frame` is reserved for the VM call stack** (`CallFrame`,
`VM::frames()`, the debugger's stack pane) and means nothing else from
here on.

- **Agent** — a clean-room context with a charter: the thing you spawn,
  ask, and list. Identified by its `Agent` event id (`AgentId`).
  "Subagent" keeps its meaning.
- **Branch** — an addressable conversation: one path from the root to a
  leaf. An agent has one branch until someone forks it; then it has two,
  both live, both its own. Identified by its **root event**
  (`BranchId = EventId`) — the `Agent` for an agent's first branch, a
  `Fork` for a divergent one.
- **Spine** — the code's handle for a branch's path (`Spine`,
  `spine_at`): leaf id plus the reconstructed chain of contexts. Internal.
- **Context** — the reconstructed conversation of one agent along a
  spine: charter, system prompt, posts, turns, open questions. What is
  rendered into an LLM request. (`spine.context()` — was `frame()`.)
- **Runner** — the live step machine driving one branch (was
  `AgentState`). Live-ness is session state; identity is in the log.

An **address** is a branch id, or an agent id when that agent has exactly
one live branch (the common case — forks are a user gesture).

**Branch names.** Every branch root carries a `name` (`Agent.name`,
`Fork.name`), so "the branch's name" is one concept with one field —
what the navigator renders. History is immutable, so a rename is an
event: a branch's name is the last `Rename` **at or after its root**,
else its root's name. That is the same scoping rule open posts use, and
the same reset in `replay_event`; renaming an original therefore leaves
its forks alone, which is what you want when a fork was named for how it
differs.

### Why these names

- **Not `frame`.** It was a stack-era word — one active cursor, with
  `PushFrame`/`PopFrame` around each subagent. 8_HARNESS deleted the
  push/pop and kept the word; `FrameResult` kept the last of the
  metaphor. With many live branches that never end there is no push, no
  pop, and no cursor, so the word has nothing left to describe.
- **`Agent`, not `AgentStart`.** Nothing ends an agent, and a `…Start`
  name carries the same latent "and later, an End" that made `frame`
  wrong. It pairs with `Fork`: two structural branch roots, each named
  for what it creates rather than for a lifecycle phase.
- **`Agent`, not `Branch`.** The branch is the *path*; the `Agent` event
  is who the path is a conversation with. The two roots stay separate
  payloads rather than one with an optional agent definition, because the
  difference is load-bearing — **`context()` resets at an `Agent` and
  carries through a `Fork`**: clean-room isolation (8_HARNESS dec. 3) on
  one side, inherited history on the other. That belongs in the type, not
  in an `is_some()`.
- **`Agent` over `Actor`, `Mind`, `Worker`, `Role`.** The model-facing
  name is frozen: `tools.agent` keeps its schema and card line verbatim
  and every doc says "subagent", so any other payload name leaves tool,
  docs, and log disagreeing. **The disambiguation this forces:** DESIGN.md
  uses "the agent" for the whole product; from here on *an agent* is one
  context and the product is *the harness* or *the system* (Part E fixes
  the sentences that read the old way).
- **`Condition`, not `Suspended`.** The payload describes the *cause*,
  not the branch's state, and it feeds a rendering the project already
  calls the condition report (`report.rs` has `ConditionReport`). It is
  Lisp's word on purpose: there, `condition` is the supertype and `error`
  a subtype, so a condition need not be an error — which is exactly
  DESIGN.md's claim that a raise, a trapped error, and a user interrupt
  are rows of one table. An error-flavoured name would make the error case
  the archetype and break the thesis.
- **"Report" is a rendering, not a payload.** DESIGN.md's word for the
  product surface stays in the prose — the condition report, the
  completion report — but there is no `Report` event: reports are derived
  (below). The one settlement payload, `Result`, therefore keeps a clean
  meaning: a call a **program** made. That also avoids a collision that
  would otherwise bite — `tools.tool_result(id)` is model-facing and
  fetches a `Result`, so a payload named `ToolResult` for the LLM-facing
  message would make "tool result" mean two things.
- **No `Label`.** Its only job was naming a *point* so it could be found
  again — the single-cursor era's substitute for branches having
  identity, needing a backwards "nearest label" search. A name at the
  root exists from the branch's first moment and cannot be ambiguous
  about what it names. `Rename` is not its return: that names the branch
  and folds forward from the root.

## The responsiveness contract

| you speak to a branch that is… | your message is visible | it reaches the LLM | you get an answer |
|---|---|---|---|
| idle | immediately (logged on arrival) | immediately | one LLM turn |
| running a program | immediately | at the next fuel slice (≤ ms); the program pauses there, in-flight tool calls keep running | one LLM turn; the program resumes or is rewritten in the same turn |
| suspended on a condition | immediately | appended beside the pending report | one LLM turn |
| mid-generation (thinking) | immediately | when that generation lands — or now, with `Interrupt` | one LLM turn after that |

Nothing you say is ever rejected, queued invisibly, or lost to a crash.
"One LLM turn" is the floor for anything semantic; status you can see
without asking (source, console, artifacts, program status) is on screen
live, so you never spend a turn on it.

## The desires, and what makes each one fall out

| Desire | Mechanism |
|---|---|
| Any number of leaves growing at once | live state keyed by `BranchId`, not `AgentId` |
| Speak in any context, at any time, including mid-program | every inbound message is a `Post`; one delivery rule (B); a post to a running program is a condition |
| My remark makes the LLM rewrite the running program, keeping results | the post-condition report renders the source **annotated per call site with artifact ids**; rewrite reuses by id, pending calls re-awaited by id |
| Never wait on a spawn; talk to the orchestrator while it delegates | rule B — a running branch is always one slice from listening |
| A subagent delivers, the parent is notified, I keep talking there | an answer is an event, not a terminator; agents never close |
| Ask a subagent many questions; spawn = create + first question | `Send`/`Post` pairs with ids; `Agent` carries only name + charter |
| Ad-hoc orchestration: as many workers as needed; one, some, or all; over hours | `spawn` / `ask` / `agents` as tools; **composition is JS** (`Promise.all`, loops), not more tools |
| Workers outlive the program that made them | `tools.agents()` re-discovers the subtree with status, so the next program picks up where the last left off |
| Upward questions for clarification | the same exchange, the other way; no topology rule needed |
| Structured replies | `Answer.value` / `Result.value` are JSON; `answer(question, value)` is a restart |
| Graceful Ctrl-C / resume | every exchange recorded on both ends; recovery is reconciliation |
| The tree comes to me | an agent's question to me appears **inline in that branch**, and it is highlighted; "waiting on you" is a view, not a place |
| I can act as the restart handler myself | `Restart { branch, … }` — DESIGN.md's outermost handler, literally |

## The primitives

Twelve payloads in six roles — `Message` and `Call` being enums, so ten
top-level. `Message`
stays what it is today — the enum of rendered kinds, one per API role —
with honest variants; `Call` is its mirror on the outbound side:

```
structural   Agent { name, charter, tools, system }   a new agent; roots its first branch
             Fork  { name }                           roots a divergent branch; obligations do not
                                                      cross it (renders: a harness line, see "driven")

rendered     Message::Post { from, origin }            a message delivered here          (user role)
                                                      origin: Sent(send-id) — body lives in the
                                                      `Send`; or Direct{text, input,
                                                      expects_reply} for a user/harness post
             Message::Turn { author, text,            this context's own output         (assistant role);
                             thinking, tool_calls }    author is the LLM, or the user taking a turn

calls        Call::Send   { to, text, input,          this branch's program messaged an agent or
                            expects_reply, site }     the user — the mirror of Post
             Call::Spawn  { name, charter, tools,     this branch's program created an agent
                            site }
             Call::Invoke { name, args, site }        this branch's program called a host tool

settlements  Result { call, outcome }                 a call settled — the artifact
                                                      Delivered(value): an answer, a receipt,
                                                      an agent, a tool result
                                                      Failed(message): it definitively did not
             Answer { question, value }               this branch answered Post `question`

run          Return    { value }                       the program finished (terminal)
             Condition { cause, site, stack }          everything else — Raised{name,payload} /
                                                       Trapped{kind,message} / Posted{ids} /
                                                       CompileFailed{message} / Refused{reason} /
                                                       Interrupted, with the site and stack a
                                                       diagnostic needs
             Console   { lines }                       console.log output, capped with a marker

branch       Rename { name }                           this branch is called this from here on
```

`Return` settles nothing and has no `call`: it is the program's own
output, the value the completion report is rendered around, flowing
*into* its branch's LLM rather than back from a call. (It was
`ProgramResult`, a name that invited "where is its `call`?" — a question
the design cannot answer.)

Each rendered kind maps to exactly one API role by its variant, never by
a flag. **There is no `Report` payload**: the tool-role message answering
a `run_program`/`resume`/`answer` call is *rendered* from the run's
outcome and the events around it, never stored — see "Reports are
derived".

Three **call kinds** — `Send`, `Spawn`, `Invoke` — are typed variants, not
one `Invoke` with a magic `name`. From a *program's* view they are all
`tools.*` calls ("subagents are tools," 8_HARNESS dec. 2, holds at the
API); `dispatch_calls` interprets the name exactly once, at dispatch, and
everything downstream — reconciliation, re-attach, the report's
pending-kinds, routing by `origin` — matches on the variant. `tools.ask`
and `tools.tell` both log a `Send`, differing only in `expects_reply`;
the `Post` names that `Send` rather than repeating it. `Result.call` points at any of the three, and
**every call gets exactly one `Result`** — `Delivered` with an answer, a
delivery receipt, an agent handle or a tool result, or `Failed` with the
reason.

`Failed` is load-bearing, not a convenience: it is what distinguishes a
call that **definitively did not work** from one that was **in flight
when the process died**. Both would otherwise read as an `Invoke` with no
`Result`, which reconciliation must treat as *issued; may have happened* —
the worst possible reading for an effectful tool. A `Failed` result also
rejects the program's promise, which is where the handler hierarchy
starts: the program may `catch` it and carry on (6_LANGUAGE Part B, the
innermost layer), and only an **uncaught** rejection reaches top level and
traps into `Condition{Trapped}`. So a tool failure is a *value* first and
a condition only if the program declines to handle it.

- `from: Author = User | Agent(AgentId) | Harness`.
- `origin` — where this delivery's body lives. `Sent(id)` names the
  `Send` that dispatched it, and the body is read from there; `Direct{…}`
  carries the body inline, for the user and harness posts that have no
  send side. **A `Post` is a delivery marker, not a copy**: the new fact
  it records is that this message landed *here*, at this position in this
  branch's transcript. `origin` is also what routes an answer back to the
  sender's branch.
- `expects_reply` — `tools.ask` (or a user turn) sets it, `tools.tell` (or
  a harness notice) clears it; only posts that expect a reply are *open*.
  It lives with the body, so exactly one event carries it.
- `input` — optional JSON on a `Send` or a `Direct` post. It is
  **machine-bound data**: the context sees a bounded **preview** (shape,
  keys, sizes) and the whole value reaches the *program*, as the `input`
  const binding sourced from **the oldest open post** (the same post a
  bare turn answers, so binding and answering never disagree; `null` when
  nothing is open). Any other post's input is fetchable whole by id. A
  caller passing a large `input` must never dump it into the callee's
  context, which is what rendering it in full would do.
- `author` on `Turn` — the LLM, or the user taking a turn on this branch
  (`Restart`, below). Renders as an assistant message either way: the
  *branch* acted.
- `call_id` — the LLM API's tool-call id, carried on a `Turn`'s tool
  calls. The one seam where the API's correlation domain does not collapse
  into event ids; nothing else stores it, because a derived report is
  paired to its call positionally. (There is no `reply_to` anywhere: a
  report is tied to its call by that pairing, a reply to a post by
  `Answer.question`, and nothing else answers anything.)
- `site` — a span in the program source: on the three call kinds it is
  the call site (for the annotated report), on a `Condition` it is where
  the program stopped (for the line-and-caret). An `InvokeCall` field the
  VM gains; `spans[ip]` at the point in question.

Gone: `Message::User` (→ `Post`, any author), `Message::Tool` (→
rendered, not stored), `Message::Assistant` (→ `Turn`, with an author),
`Message::System` (→ `Agent.system`), `FrameResult` (→ `Answer`),
`FrameStart.prompt/input` (the first question is a `Post`).

### What one run logs

A `run_program` turn produces, in order:

```
Turn { tool_calls: [run_program(src)] }     the LLM acts
  Invoke / Send / Spawn  … Result …         calls made and settled, in resolution order
  Return { … }  or  Condition { … }         how the run handed back — exactly one
  Console { lines }                         console.log output, capped
```

The report the LLM reads is **not an event**. It is rendered from this
run's outcome, its `Console`, and the artifacts on the path, at the
moment the request is built.

**Exactly one outcome per handback** — not per run. A single
`run_program` may raise, be resumed, trap, be resumed again, and finally
return; each handback logs its own outcome and renders its own report,
and the log shows the whole raise/resume dance. The outcome carries
everything its report will need: `Return` when the program finished, `Condition` for everything
else — a raise, a trapped error, an arriving post, a compile failure, a
refused restart, an interruption — with the site and call-stack chain a
diagnostic renders from. **Every tool call therefore has exactly one
outcome event** (`Return`, `Condition`, or `Answer`), which is what lets
every report derive from one rather than from recomputed history. Two payloads, because the split is the thesis: a run either
produced its value or it produced something for a handler to decide
about. This also closes a real gap — `protocol.rs` notes `ProgramStatus`
is live-only and "suspended-vs-failed is not inferable from the report
text", so today a reopened log cannot say how a program ended.

`Console` holds the program's `console.log` output — the diagnostic
stream that survives when the return value does not, so it is what the
TUI's console pane and any post-mortem read. It is capped by named
consts with a truncation marker, being diagnostics rather than data. The report shows
only a bounded tail of it (`report.rs`'s line-count and per-line clips),
the same rendered-summary/full-artifact split as `Return`.

**Exactly one outcome per tool call.** A handback logs `Return` or
`Condition` — never both. An `answer` logs its `Answer`; an ineligible
call logs `Condition{cause: Refused}` even though nothing ran, so the
rule holds without exception and no report is ever derived from replayed
state.

**Only handbacks log a `Condition`.** A condition the program itself
handles never reaches the LLM, so it is not one of these events:

- a **caught throw** (`try`/`catch`, 6_LANGUAGE Part B — the innermost
  layer of the same handler hierarchy) is the program's own control flow;
  if the model should know, the program says so with `console.log`, which
  is exactly what the console channel is for;
- a **failed tool call** is already logged — its `Result` carries the
  error — so the fact is in the log and the menu without a second event;
- a **fuel slice** ending is invisible to programs by construction (9_TUI
  dec. 2 deleted `ErrorKind::OutOfFuel`) and would log once per slice.

The risk this leaves is *silent degradation*: a program that swallows five
failures and returns a thin result, with a completion report that reads
as success. The fix belongs in the report, not in a new event — it can
count failed `Result`s in the run and say so. The
converse should never be observable: a completed program logs both, in
that order, because the report names the `Return`'s id when the value
exceeds the answer budget. The one window where it *is* observable is a
crash between them, and reconciliation reads it precisely — a `Return`
present means the program finished, so the completion report is rendered
from what is logged and appended, rather than the run being treated as
interrupted and rewritten.

This is why a program that ends without a `return` still logs
`Return { value: null }`: "completed ⇒ `Return`" holds without exception,
which is what makes recovery decidable from the log alone: a run either
has an outcome or it does not. The completion report is *more* than the
return (clipped value, console tail, new artifacts) and the return is
*more faithful* than the report — which is why the return is the thing
stored and the report the thing rendered.

### Reports are derived, not stored

Every report is a pure function of the log:

| report | derived from |
|---|---|
| completion | `Return` · `Console` · artifacts since the run began, **including how many calls failed** |
| condition | `Condition{cause, site, stack}` · the source in the `Turn`'s tool-call args · `Console` · artifacts |
| post-condition | the arriving `Post`s · each `Call`'s `site` and its `Result` (the annotated source) · `Console` · artifacts |
| compile error | `Condition{cause: CompileFailed}` · the source |
| answer ack | the `Answer` and where it routed |
| refusal | the offending call in the `Turn`, with eligibility recomputed at that path position |

This is why `Condition` carries the site and stack chain: they are the
last inputs that lived only in the VM, and the VM is never persisted
(dependency spine, link 1). With them logged, **nothing the model ever
saw depends on state outside the log.**

The gain is not disk. It is that a corpus of real logs can be re-rendered
with a *new* report format and diffed — which is precisely the iteration
DESIGN.md calls this project's product surface ("a prompt-engineering
artifact with golden-render tests"). Freezing the prose would make old
logs unrenderable and that comparison impossible; and because frozen
prose does not require its inputs to be logged, it would quietly permit
an incomplete log.

The cost, stated plainly: **the rendered prefix is stable only for a
given renderer.** Editing `report.rs` changes how an existing
conversation re-renders — one cache warm-up on the next resume, and prose
that differs cosmetically from what the model saw (the facts are the same
events). Reproducing the exact bytes a model saw means checking out the
renderer of that era. That is a fair price for a log that is complete by
construction, and it is why golden-render tests exist: they pin the
format deliberately rather than by accident.

`Agent.system` is the deliberate exception: the system prompt *is*
snapshotted, because it is assembled from the registry — state outside
the log — and because it sits at the very front of the prefix, where
churn is most expensive.

### The menu is a view over the branch's path

Nothing maintains an artifact store. The menu a report shows is a
**projection over the events on this branch's path** — every `Result`
that landed here, plus every call still pending — and
`tools.tool_result(#id)` reads one back. The log is the cache and the
event id is the key, which is why reuse needs no args-matching (8_HARNESS
dec. 6) and why a rewritten program can pick up exactly where the last
one got to.

| row | shown as | fetchable |
|---|---|---|
| `Delivered` `Invoke` / `Send` / `Spawn` | `[#id] name(args) → preview` | yes, the value |
| `Failed` call | `[#id] name(args) → failed: reason` | yes, the reason |
| pending `Send` | `pending — await tools.tool_result(#id)` | yes, re-attaches to its `Result` |
| pending `Invoke` | `issued; no result recorded; may have happened` | no — its worker died with the process |
| a prior `Return` | `[#id] returned → preview` | yes |

`Console` and a `Post`'s `input` are fetchable by the same call but are
**not menu rows** — they are named at the point they are truncated (the
console tail's marker, the post's preview), because they are context for
one place rather than work to be reused.

**Scoping: the path, and only the path.** A program may fetch ids on its
own branch's path and no others — that is what keeps an agent clean-room
(8_HARNESS dec. 3) now that "the frame's spine" is "the branch's path".
This gives forks exactly the behaviour you want: **history and artifacts
cross a `Fork`; obligations and in-flight calls do not.** A retry fork
reuses the expensive read its original already did — the `Result` is in
the shared prefix — while a *pending* pre-fork `Send` is refused, by the
same `eligible()` check that refuses answering a pre-fork post, and for
the same reason: its `Result` will land on the original's branch, which
this fork's path does not include, so re-attaching could never resolve.

### Every tool call renders exactly one tool message

The API requires every `tool_call_id` to be answered, so the renderer
emits exactly one tool message per tool call in a `Turn` — each derived
from the events that call produced. A turn carrying `answer(#42, v)` and
`resume()` renders two: the ack, and the report of wherever the resumed
VM next halted. No request is built in between, because the branch holds
a VM.

Pairing is positional, not stored: a call is answered by the events its
own run produced, and for the VM-driving path that means the call that
most recently drove the VM — the `run_program`, or the `resume` that
re-entered it. (M2 hit this as a live 400 when the pairing was wrong: a
post-`resume` report was attributed to the original `run_program` id and
the next request was rejected. The renderer must reproduce that
attribution from the log, and a test asserts it.)

| emitted when | the report holds |
|---|---|
| a program **completes** | `returned:` clipped to the answer budget, naming its `Return` id · console tail naming its `Console` id · the run's new artifacts |
| a program **raises or traps** | what happened (rendered diagnostic: source line, caret, condition name + payload) · where (call-stack chain, console tail) · artifact menu · eligible restarts |
| a **post arrives** at a running program | the post(s), author-labelled and marked *asks you* / *tells you* · the **annotated source** · console tail · artifact menu · eligible restarts |
| `run_program` **fails to compile** | the diagnostic alone — no VM was built, so this run has no console or artifacts (the repair loop, unchanged) |
| `answer(q, v)` | what was answered and where it went (delivered to branch N, or read inline by the user) |
| any **ineligible** call | what is true, and what is valid now (see "Refusal is an answer") |

The first three share their section renderers, so console-tail bounds,
artifact-menu formatting, and restart wording cannot drift apart — the
8_HARNESS Step 4 arrangement, extended with the post kind and the
annotated source. Every section is bounded by named consts in
`report.rs`, and every truncation names a fetchable id.

## The three rules

**A. The branch is the address.** A post is logged on the branch it is
delivered into. A record is logged on the branch whose state it changes.
There is no routing table in the log; the tree *is* the routing.

**B. A post is logged on arrival and delivered at the recipient's next
safe point — and every fuel-slice boundary is a safe point.**

| the branch is… | the post is… |
|---|---|
| idle | appended; an LLM turn starts |
| running | appended; at the next `Tick` the run suspends into `Condition::Posted` — a condition whose report is the message, restarts `answer` / `resume` / `run_program` |
| suspended | appended beside the pending report; the next request shows both |
| awaiting an LLM response | appended now (visible, crash-safe); acted on when the response lands: a text answer → idle → the post starts the next turn; a `run_program` → the program starts and suspends at its first slice with the post |

No parked-vs-computing distinction and no pending queue: the runner
notices, at each safe point, posts it has not yet shown the LLM (the
`shown` mark — see "How it is driven"). The
load-bearing property guarantees every slice boundary has total state
visibility, so this costs nothing and is the most responsive rule
available. It is also why upward questions cannot deadlock: a parent
awaiting its child is one slice from being told, its LLM answers, the
child continues. A real runtime deadlocks there because the waiter is on
a stack; here the waiter is a `StepResult`.

`Interrupt { branch }` is the one override: it cancels an in-flight LLM
generation (never logged — from the API's view it did not happen) or
pauses a VM at its next slice, so a post lands *now* instead of when the
generation finishes. It is the debugger's pause, addressed at a branch.

**C. Waiting is a property of the awaiting program, never of the
message.** A value someone's program awaits arrives as a `Result` and
resolves the promise — machine-bound, never entering a context. A message
nobody's program awaits arrives as a `Post` — mind-bound, delivered into
the context because otherwise no one would see it. This is DESIGN.md's
"one exception" restated as a mechanism: data crosses into a mind exactly
when no program is waiting to receive it. A `Result` landing on an idle
branch with no awaiting program (the caller re-entered after a crash) is
logged as an artifact *and* surfaced as a harness post with
`expects_reply: false` — a tell — so the branch wakes and notices without
owing anyone an answer.

## How it is driven

The LLM completes when prompted; the harness must know when to prompt.
With the OpenAI completion API the answer is one rule, derivable from a
branch's own path plus one number — `shown`, the event-id high-water mark
at the branch's last request render:

> **Prompt iff the branch holds no VM and there is a rendered `Message`
> other than a `Turn` with id > `shown`.**

- A `Turn` with no tool calls is the only terminal: the branch is idle
  until a `Post` arrives.
- A `Turn` with tool calls hands the branch to the VM; the VM speaks only
  through its outcome events (`Return`/`Condition`) — so the
  rule fires when the VM has something to say and never while it is
  running.
- Every request has a cause event. The LLM is never prompted "just
  because," and never twice for the same thing: `shown` advances at each
  render.
- After a crash the same rule re-derives the state from the log: the last
  rendered event is a `Post`, or a `Turn` whose calls have outcomes →
  prompt; a `Turn` with calls and no outcome → `synthesize_if_interrupted`
  writes one, then the rule fires; a `Turn` without calls → idle. "Awaiting the LLM" is not a log state and needs
  no repair.

**"Interactive" and "autonomous" are not kinds of branch.** The rule
above is uniform, and deliberately so — this design already deleted one
such split (`is_root`: the root yielded, a subagent delivered) by making
delivery follow the asker. A mode flag would resurrect it under a new
name, and every rule here would need two readings.

What makes a branch feel autonomous is not extra prompting; it is **the
program still running**. A branch that wants to work for an hour writes a
program that works for an hour: awaits cost nothing while parked, fuel
slices keep it responsive, and it re-enters its LLM only at decision
points. Re-prompting a branch that chose to stop — with no cause event —
is the plain tool-loop architecture this project exists to replace, and
the one the M5 eval measures against. So the harness never prompts
without a cause, and "keep working" is always the model's own
`run_program`, never the harness's initiative. If models stop too early —
one small program, a report, silence — that is a **card** problem (write
programs that carry the task as far as they can), not a trigger problem.

**The LLM already owns the "do I need an answer?" decision** — it is
`ask` versus `tell`. What it has been missing is the information to make
that decision well, and that is a fact about the *addressee*, not a mode
of the asker:

- for an agent, `agents()` already reports `status` and `open`;
- for the human, each request ends with a **presence** line — *someone is
  attached to this session* or *no one is attached; a question to the
  user may sit unanswered for a long time*.

Presence is per-request, never branch state, so attaching or detaching
changes the next render and nothing else — a branch that ran alone
overnight is simply told, on its next request, that you are back. It is
honest about its limit: attached means a client is connected, not that a
human is reading. And it goes **last, never in the system prompt** — see
"Cache discipline".

Given that, an agent that needs input and knows no one is there has a
real choice: wait (a parked ask is free — no fuel burns while awaiting,
and it is re-attachable by id if the program is later rewritten), or
proceed on an assumption and `tell` it so the assumption is on the
record. Both are one line of program. The harness imposes neither.

**Forks are born idle.** A fork's `shown` starts at its `Fork` root, so
history before the root never triggers a prompt — the fork speaks only
when spoken to. This is the same mark that stops a post arriving during
a generation from stealing the next turn's binding; triggering and fork
suppression are one mechanism.

**What the LLM does with what it sees is not structural, and cannot
be.** Once prompted, a fork's LLM sees the pre-fork question in its
history with no answer on this path, and the card compels it to answer
open questions. There is no API-level "do not address that," and hiding
history would defeat forking. The lever is rendering — the only honest
one — so **`Fork` renders**:

- at an ordinary fork point, as a harness line: *"fork of branch N at
  #E — questions before this line are being handled there; do not redo
  its work unless asked"*;
- at a mid-program fork point (the original's last `Turn` has tool calls
  with no report on this path), as **that call's tool result**: *"program
  #P is running on the original branch, not here; its artifacts so far:
  …"* — which also keeps the API's adjacency rule satisfied instead of
  leaving a dangling call.

Retry and sidebar are therefore not modes: fork *before* the question
and say "try again with X", or fork *at the running leaf* and ask "what
are you doing?" — the same gesture, and the rendered line tells the model
which it is.

**Cache discipline: the prefix is immutable.** Prompt caching keys on the
longest common prefix. Being *re-sent every request* is not the same as
being free to vary: the tool list and the system prompt are both
re-transmitted each time, and both are assembled into the **front** of
the prompt server-side — Anthropic documents the cacheable order as
tools → system → messages, and OpenAI's caching covers the messages array
and the tools array with the usual "static first, variable last"
guidance. So a varying tool list is a varying prefix. Hence:

- **Every rendered message renders identically forever.** A `Post`,
  `Turn`, or derived report is built from logged events only; nothing
  decorates it with a fact that was true at the time and logged nowhere.
- **The system prompt is rebuilt from `Agent.system`** — a snapshot
  logged when the agent was created — so a later card edit, a new
  registry tool, or a harness upgrade never alters an existing
  conversation's prefix. This is the reason `system` is a logged field
  rather than regenerated from the registry.
- **The tool schema list is constant for a branch's life.** Today it
  varies by phase (`machine.rs`: `resume` appears only while suspended),
  so a branch with N conditions pays 2N invalidations — and conditions
  are the boundary this project crosses most. Instead, offer
  `run_program`, `resume`, and `answer` always; the **report says which
  are valid right now**, which is where 8_HARNESS Step 4 already puts the
  restart menu. Eligibility then splits by how often it changes:

  - the **rules** are static, so they go in the system prompt, where they
    are cached for the branch's life and cost nothing per request:
    `run_program` is always valid; `resume` only when your last message
    is a condition report for a suspended program; `answer(q)` only for a
    post that is open *on this branch*;
  - **which are eligible now** stays in the report, with a one-line
    reminder of what each does — the model decides at the report, and
    recency beats a rule stated far back in the context;
  - **an ineligible call is refused by answering it** — the only refusal
    the API permits, since an unanswered tool call makes the next request
    invalid. See below.

  This trades a *schema-level* guardrail (an invalid restart is
  impossible) for a *report-level* one (it is possible, and corrected in
  one turn). Worth it if the cache is real, and it is simpler either way
  — but **verify against DeepSeek before relying on the cache argument**
  (below), since its disk cache is prefix-based but its treatment of
  `tools` is not something this plan should assume.
- **Per-request facts go last**, as a trailing ephemeral line after the
  newest message: presence today, nothing else so far. It is never
  logged, and next request it is simply re-emitted at the new end, so the
  prefix it followed stays intact.

The one measurement this rests on: two live DeepSeek requests on the same
branch differing only in the tool list, comparing
`usage.prompt_cache_hit_tokens`. If tools turn out not to participate in
its cache, the phase-varying list is harmless and only the simplicity
argument remains; the system-prompt and rendered-message clauses hold
regardless, since those are prefix in every provider.

**A nameless branch is displayed, not renamed.** The two are often
conflated; keeping them apart is what avoids drift. A branch with no
name shows a **derived** label — the first line of its first post,
clipped — computed at render time, logged nowhere, and gone the moment
anyone names it. That is most of the value of automatic naming for none
of its cost: no LLM call, no event, and no name changing under a user
who has already learned it. It matters more here than in a chat product
because branch names are **shared references**: you tell an orchestrator
"ask the researcher," so a name that drifts breaks a conversation, not
just your bearings.

Renaming is therefore explicit, and the model already does most of it at
the right moment — `spawn({ name })` and `Fork { name }` name a branch
when it is created, which is when its purpose is clearest. `Rename` is
the correction, and a correction is a user's act. A `Rename` is a record,
never rendered, so renaming a branch never wakes it (the driving rule
counts only `Message`s).

**Refusal is an answer, and it must be self-sufficient.** An ineligible
restart is answered with a rendered refusal that states what is true and
what to do — *"nothing is suspended. Open on this branch: #42. Valid now:
run_program, answer(#42)."* — so the model recovers on its next turn
rather than guessing twice. It changes no state, costs one turn, and is
the same shape as every other correction in the system.

One check covers more than typos. `answer(#42)` where #42 is open on the
*original* branch and not on this fork is ineligible, and the refusal
says why — *"#42 belongs to branch N; this fork does not owe it. To make
your answer the delivered one, the user can take that branch's turn."*
So the fork-obligations rule is enforced where it is violated, and
explained there, instead of being a rule the model must have absorbed.

**Rendering order.** The log is arrival order; the API is not. The
completion API rejects anything between an assistant tool call and its
tool result (M2 met exactly this 400). Since posts are logged on arrival,
a user post logged mid-run precedes the report in the log. The request
builder places each derived tool message immediately after the `Turn`
whose call it answers and renders intervening `Post`s after it; the
report names when the message arrived. That
is the one place log order and render order differ.

## The user is an author, not an agent

The user has no branch. `Author::User` speaks *inside* branches: an
utterance in branch X is `Post { from: User }` on X, X's reply is X's
`Answer` (and its `Turn`), and the user reads it there. The user
**borrows the context of whichever branch they are in** — which is
exactly the experience of holding a different pseudo-identity in each
conversation. Forking makes this structural: the user's post sits in the
shared prefix of two forks, and each fork answers it in its own branch; a
single global user branch could not represent "you-in-fork-A" and
"you-in-fork-B" as different participants, and would need two `Result`s
for one `Invoke`.

So the user is the second blanked column in the exchange table (the host
is the first): no program to `Send` with, no context to `Post` into.

- **The user asks:** right column only — `Post { from: User,
  expects_reply: true }` on the branch; its `Answer` is read inline. No
  `Send`, no `Result`. **The user tells** the same way with
  `expects_reply: false` — "FYI" lands, wakes the branch, and owes
  nothing; the TUI exposes it as a modifier on send.
- **An agent asks the user:** left column only — `Send { to: user }` on
  the branch, pending until the human's `Reply` produces its `Result`.
  No `Post` anywhere. The question renders **inline in that branch's
  chat**, highlighted, with the input switched to reply mode; the
  navigator highlights the branch.
- **The inbox is a view**: every live branch with a pending ask-to-user.
  **The timeline is a filter**: every `Post { from: User }` in the tree, by
  id. Both are one query over what the TUI already holds.
- **Reconciliation covers the human** through the branch: a `Send { to:
  user, expects_reply: true }` with no `Result` reopens as a branch still
  asking.
- **`ask` with `to` omitted** means *the author of the question you are
  answering*; when that author is `User`, the invoke is addressed to the
  human. A program never needs to know which.

## Exchanges

Every exchange is four events, two per side, each side reconstructible
from its own path:

| | asker's branch | answerer's branch |
|---|---|---|
| the question | `Send { to, text, input, expects_reply: true, site }` | `Post { from: Agent(asker), origin: Sent(send) }` |
| the answer | `Result { call: send, outcome }` | `Answer { question, value }` |

**What records "I asked" is the `Send`** — logged at dispatch, on the
asker's branch, the same shape a host tool call gets. The four events
form a closed loop of ids — `Post.origin → Send`, `Result.call → Send`,
`Answer.question → Post` — so from any one the other three are one index
lookup away, which is what reconciliation walks, what a renderer resolves
a body through, and how an answer finds the *branch* that asked.

**A fork inherits history, not obligations.** A post is *open on a
branch* iff it is unanswered and sits at or after that branch's root
event. Pre-fork posts are before a fork's `Fork` root, so they stay the
original branch's to answer; pre-fork calls are the original's to
receive (the VM is never copied). The fork sees all of it as context and
owes none of it — so there is exactly one owner for every open post and
every pending call, and "which branch delivers?" is never a race.
`replay_event` clears `open` when it crosses a `Fork`; nothing else is
needed. A fork's bare turn that "answers" a pre-fork question logs no
`Answer`; the user reads it inline, which for exploration is the point.
To make a fork's answer *the* answer, the user takes the original
branch's turn with `answer(#post, value)` (see `Restart`).

**A body is stored once.** The `Send` holds the question and the `Post`
names it; the `Answer` holds the value and the `Result` names it. Neither
side copies the other. `replay_event` therefore resolves through the
`Tree`, which holds every event anyway — the "each branch reconstructs
from its own path" purity was never required by the code, and paying for
it in bytes would contradict the by-reference discipline the design
applies everywhere else. What each delivery-side event contributes is
**position**: that this message landed here, in this branch, at this
point in its transcript.

Everything else is the same table with a column blanked:

- **Host tool call:** left column only — `Invoke { name, args }` at
  issue, `Result` at landing. This splits today's single
  `Invoke{name,args,result}` (logged at resolution); the reason is
  effectful tools across a crash — a
  `send_email` issued a millisecond before `kill -9` is currently
  invisible in the log.
- **Tell:** `Send { expects_reply: false }` on the sender, `Post {
  expects_reply: false }` on the recipient, and the sender's `Result {
  value: { post } }` — the delivery receipt — appended in the same step.
  Three events, no `Answer`. Delivery is rule B like any post (it *does* cost the
  recipient a turn to know it); what a Tell spares is the answer, not the
  attention. For high-volume status, pull with `agents()` instead of
  pushing tells.
- **The user:** see above — no program, no context.
- **Spawn:** `tools.spawn({ name?, charter, tools? })` logs `Spawn` whose
  `Result` is `{ agent }`; it roots `Agent { name, charter, tools, system }`
  as a child of that `Spawn` (the caller's branch continues as a sibling
  — today's call-site parenting on an honest anchor). The two events are
  the two ends of one act: `Spawn` is the caller's request, settled by a
  `Result`; `Agent` is the agent's own root and outlives the caller,
  its program, and often the conversation that created it. It carries
  `tools` in machine-readable form — the registry enforces the child's
  allowlist from the agent's own root, not from an event on its parent's
  branch — and the root agent, which has no `Spawn`, is not a special
  case. `tools.agent({ prompt, input, budget? })` is
  sugar for spawn + ask and keeps its schema and card line verbatim.
- **Anywhere:** `tools.ask({ to })` accepts any address the program holds
  — a branch id, or an agent id with one live branch (an ambiguous agent
  id is a rejected call naming its branches; `agents()` rows carry both).
  The harness does not police topology, because rule B makes every
  topology deadlock-free.

Clean-room (8_HARNESS dec. 3) is untouched: an agent's *context* is built
from `spine.context()` — the innermost agent's posts and turns — never
from the caller's prefix on its path.

**A turn can post; only a program can await.** The call kinds record
anything a branch *waits on*, whoever answers it (a host worker,
another agent's LLM, the human); a send that does not wait is a post issued from a turn
(`answer` is one). The line is structural: awaiting needs a resumable
suspension point — somewhere a reply lands, an interrupting post becomes
a condition, and execution resumes — and only the VM has one. A waiting
turn would need a branch-level phase, interruption rule, and `resume`: a
second VM. So an agent asks by running a program, `return await
tools.ask({ text })`, and inherits parking, interruption, and re-attach
for free. A *notification* is `tools.tell` — not an un-awaited `ask`,
which would leave the log claiming someone waits for an answer nobody
wants.

## Orchestration: six tools, and composition is the program

The whole model-facing surface for running a tree of workers:

| tool | what it is |
|---|---|
| `tools.spawn({ name?, charter, tools? }) → { agent }` | a new agent under you; `tools` narrows the child's allowlist (default: yours) |
| `tools.ask({ to?, text, input? }) → value` | one question, one answer; `to` omitted = whoever asked you |
| `tools.tell({ to?, text, input? }) → { post }` | inform without asking; resolves with a delivery receipt as soon as the post lands |
| `tools.agents({ under?, deep? }) → [{ agent, branch, name, charter, parent, status, open, last_answer }]` | **discovery** — your subagents (or the whole subtree), one row per branch, with live status (`idle` / `thinking` / `running` / `suspended` / `dormant`), open-question count, and each one's last answer id |
| `tools.tool_result(id)` | reuse: a done result, or re-attach to a pending one |
| `tools.agent({ prompt, input, budget? })` | sugar: spawn + ask, unchanged |

Everything else is JS. *Talk to all*: `Promise.all(rows.map(r =>
tools.ask({ to: r.branch, text })))`. *Relay*: ask a worker, return the
answer. *Stop a worker*: ask it to stop — a post to a running branch is
delivered at its next slice, and its LLM abandons or rewrites. *Check on
everyone*: `agents({ deep: true })`. No broadcast, relay, kill, or
subscribe primitive exists, because each would be a tool doing what a
line of program already does, and the dialect's thesis is that the
program is the orchestration layer.

`agents()` is the one addition, and it is what makes long-running
orchestration possible: **agents outlive programs, but a program's
handles to them do not.** An `{ agent }` value dies with the VM that held
it; the orchestrator's next program — an hour later, after a crash, after
the user asked something — re-discovers its workers by query, not by
memory. It is a host tool served from the tree plus live session state
(nondeterministic, which is fine).

The card teaches the pattern in one paragraph, because the model has to
know the shape to use it well:

> **Orchestrating.** Split work by spawning workers: `const w = await
> tools.spawn({ name, charter })` gives you an agent you can ask
> repeatedly — `await tools.ask({ to: w.agent, text, input })` — and it
> keeps its context between questions. Fan out with `Promise.all`.
> Workers outlive your programs: in a later program, `await
> tools.agents()` lists them (`{ deep: true }` for the whole subtree) with
> status and open questions, so you can pick up where you left off. To
> inform a worker without needing an answer, `tools.tell` it. A worker
> that is stuck asks *you*: `await tools.ask({ text })` with no
> `to` reaches whoever asked it; you will see it as a condition — answer
> with `answer(...)`, then `resume()`. The person driving the session may
> speak to you, or to any worker directly, at any time. **Each request's
> last line says whether anyone is attached right now**: if no one is,
> prefer proceeding on a stated assumption (`tools.tell` it) over waiting
> on a question that may sit for hours. Waiting is free either way — a
> parked ask burns no fuel and is re-attachable by id — so the choice is
> yours to make against what that line says. Nothing re-prompts you when
> you stop talking — if there is more to do, keep doing it in the
> program.

## Answers

- **Binding.** A `Post` with `expects_reply` is *open* until an `Answer`
  names it. A `Turn` with no tool calls answers the **oldest post that
  was open when its request was rendered** (the runner's `shown`
  high-water mark fixes this at request time, so a post arriving during
  generation can never steal the binding); the harness logs `Answer {
  value: String(text) }`. A bare turn with nothing open logs no `Answer`
  and the branch goes idle. Posts with `expects_reply: false` — from an
  agent's `tell`, from the harness (an unawaited `Result` surfaced, a
  "may have happened" note), or from the user — are never open: the LLM reacts to them with a turn, it does not
  owe them an answer. The restart
  `answer(question, value)` binds explicitly and carries JSON — for
  structured values, for a specific one of several open posts, or for
  answering an interrupting post without ending the program. It never
  changes the program's state; `resume` and `run_program` do, and one turn
  may carry both.
- **Agents never close.** After answering, a branch is `Idle` — what the
  root does today when it "yields." `Phase::Done`, `Yielded` vs
  `FrameDone`, `is_root`, `Spine::is_complete` all go. A subagent that has
  answered stops (idle costs nothing) and stays addressable; a later
  question to it from anyone is just another post, answered to whoever
  asked *that* question.
- **Budget is a rendering rule.** `Answer.value` / `Result.value` are
  stored whole; reports clip to `answer_budget` naming the id, as they
  already do for `Return`. The stored-truncated `FrameResult` and
  the "tighten it" re-prompt (12_ANSWERS Step 2) are deleted — the value
  reaches the asking *program* whole; only the context copy clips.
- **JSON.** 8_HARNESS decision 3 says a subagent's result is a JSON value;
  `finish_frame` (`machine.rs:1059`) can only produce a string. Closed.

## The post-condition report: rewrite as a copy-edit

The product surface of this phase is the report a running branch gets
when someone speaks to it. Its expected outcome is one of three, and the
report is shaped so each is one obvious move:

- *answer and carry on* — `answer(#post, …)` + `resume()`;
- *change course* — `run_program(source)` reusing what is done;
- *stop* — a bare turn (abandons the program, answers the post).

For the second to be a copy-edit rather than a reconstruction, the
**where** section renders the program source with **every call site
annotated by its artifact**:

```
const plan  = await tools.read_file("PLAN.md");        // → #12  done
const files = await Promise.all(names.map(n =>          // → #14 #15 done, #16 pending
  tools.read_file(n)));                                 //   await tools.tool_result(#16)
const out   = await tools.agent({ prompt, input });     // → #19 pending (agent 7 thinking)
```

`site` on every call is what makes this possible. Pending sends are
re-awaitable by id (below); pending host calls are not, and say so.

## What the log costs

Every payload has a live consumer, and a fan-out turn writes on the order
of a dozen or two events, so volume is not about count. It is about three
things, each with a bounded fix:

- **Durability granularity.** `log_event` currently `flush`es and
  `sync_all`s *per event*, so a 50-call fan-out is 100 fsyncs on the loop
  thread — the same thread that owes ≤ms post delivery. Sync once per
  **loop step** instead: crashing mid-step loses that step's tail, which
  is what reconciliation already repairs, so the guarantee weakens from
  "only between events" to "only between steps" and nothing downstream
  changes.
- **Copied bodies — none.** A message body is stored once, in the `Send`,
  and the `Post` names it; likewise `Answer` and `Result`. The card's own
  pattern hands the same plan to every worker, so copying would write
  twenty bodies for a ten-way fan-out. Resolution costs a map lookup in
  `replay_event`, which needs `Tree` access regardless.
- **Unbounded console.** A chatty loop can write megabytes of
  diagnostics. `Console` is capped (bytes and lines, named consts) with an
  explicit truncation marker — it is a diagnostic stream, not data, and
  the program's own `return` is the channel for anything that must
  survive whole.

**Growth, and why nothing is ever deleted.** Trimming old `Console`
events is tempting and is not available: reports are *derived*, so a
completion report renders from its `Return` and `Console` — delete either
and an old report changes or fails, which breaks the rendered prefix of
every branch that still walks through it. `tool_result(#id)` and forking
from an old point address events by id for the same reason. The tree
represents everything, always.

The answer when a tree gets big is storage, not forgetting. All of it is
**deferred until a real session proves the need**, and deferring is safe
because none of it is visible above the storage layer — event ids stay
the addressing model, so adopting any of this later requires no change to
the vocabulary or to anything a model sees:

- **Chunked log.** The JSONL becomes a directory of chunks named by their
  first event id — no manifest, self-describing by listing. Appends go to
  a plain tail chunk; when it grows past a threshold the chunk is sealed
  (compressed) on a worker, since it is immutable by then. Sealing is
  atomic the usual way: write a temp file, fsync, `rename`, unlink the
  plain one; on reopen, if both exist prefer the sealed one when it parses
  fully, else keep the plain and re-seal. Nothing on the loop thread
  blocks.
- **Content-addressed blobs.** A value over a threshold is written to a
  blob directory named by its checksum and referenced from the event;
  smaller values stay inline. One `Blobbed<T>` type, serde-transparent,
  covers the half-dozen fields that can be large (`Return.value`,
  `Answer.value`, `Result` outcomes, `Send.text`/`input`,
  `Console.lines`), so nothing above the storage layer changes — programs
  and models still address work by **event id**, and blobs are invisible
  to them. It dedups what references cannot: ten workers independently
  reading the same 50 KB file produce ten distinct `Result`s with one
  blob. Ordering makes it crash-safe — write the blob, fsync, then append
  the event — so a crash leaves an orphan blob, never a dangling
  reference; and GC is a log scan, correct precisely because events are
  never deleted. This likely subsumes much of the chunking above: with
  bulk out of the log, the log stays mostly metadata and "load on demand"
  is just reading a blob.
- **Lazy bodies.** Most traversal needs *structure*, not content:
  `spine_at` wants `parent_id`, `branch_of` wants parent plus payload
  kind, the navigator and leaf lists want neither body. So the resident
  map becomes fixed-size metadata — `{parent_id, kind, chunk, idx}` —
  and bodies load on demand behind an LRU. The working set is then
  roughly one branch's context, which is what is being sent anyway.

What is *not* worth optimising: the number of events. Each one is a fact
some reader needs — the menu, reconciliation, the navigator, the
annotated source, or M5's evals — and merging any two would put a
distinction inside a flag that the design keeps in the type.

## Crash recovery is reconciliation

`log_event` already `flush`es and `sync_all`s per event, so a crash can
only fall *between* the halves of an exchange, never inside one. On open,
scan the log and repair every unmatched half. No in-memory table is
consulted because none is needed — `parents` is deleted; the wait table is
rebuilt from the `origin`/`call`/`question` ids:

| found on open | meaning | repair |
|---|---|---|
| open `Post` with no `Answer` | the branch owes a reply | branch becomes live; if its last event is an unanswered `run_program` `Turn`, `synthesize_if_interrupted` applies |
| `Send { to: user }` with no `Result` | the human owes a reply | branch reopens live and highlighted, question inline |
| `Send` with no `Post` | crashed between the halves of the message | append the `Post` naming it (idempotent — the `Send` is the message) |
| `Send { expects_reply: false }` with a `Post` but no `Result` | crashed before the receipt | append the receipt |
| `Send { expects_reply: true }` with no `Result`; its `Post` has an `Answer` | delivery lost | append `Result { call: send, value: answer.value }` |
| `Send { expects_reply: true }` with no `Result`; its `Post` has no `Answer` | callee still owes it | callee is live (row 1); the sender's menu lists it *pending — re-awaitable* |
| `Spawn` with no `Agent` | crashed mid-spawn | nothing to repair: nothing was created, re-execution re-spawns |
| `Spawn` with an `Agent` but no `Result` | the agent exists; its handle never reached the caller | append `Result { call: spawn, value: { agent } }` — otherwise re-execution spawns a second agent and orphans the first |
| `Invoke` with no `Result` | lost in flight | the report lists it *issued; no result recorded; may have happened* |
| `Turn(run_program)` with an outcome (`Return` or `Condition`) | the run finished or produced something to handle — nothing was lost | nothing to repair: the report renders from the log and the branch prompts normally |
| `Turn(run_program)` with **no outcome** | interrupted mid-program; the VM is gone | append `Condition{cause: Interrupted}` so the run has an outcome like any other, and its report renders from it |

Deriving reports simplifies this table: there is no "the report was
lost" case, because a report is never a thing that can be lost. Either a
run has an outcome — in which case its report renders — or it does not,
and the one repair is to give it one.

The guarantee — the right one, because recovery is re-execution and
determinism is a non-goal — is: **after a resume, no completed work is
invisible.** A re-run program may ask again; that is the LLM's informed
choice against a full menu, never an accident. (An "already asked" dedup
would be the implicit args-matching cache 8_HARNESS decision 6 rejects.)

### Re-attaching instead of re-asking

A re-entered branch whose ask is still pending must be able to *wait for
it* rather than repeat it, or "pending" in the menu is amnesia with extra
steps. So `tools.tool_result(id)` extends from completed artifacts to
in-flight exchanges: on a pending `Send` it returns a promise that
resolves when its `Result` lands (served by the session). The
menu renders the two pending kinds differently, because only one can be
re-attached:

- `[#N] ask(researcher, "which file?") → pending — await tools.tool_result(#N)`
- `[#N] send_email(...) → issued; no result recorded; may have happened`

Not enforced — a rewrite may still ask again — but the obvious path is
the correct one (12_ANSWERS dec. 2). The same re-attach serves the
no-crash case: a program rewritten mid-flight can `await
tools.tool_result(#N)` for a worker already asked, instead of losing its
answer to the dead VM.

## Live branches

- **`BranchId = EventId`** — the branch's root event. `states:
  HashMap<BranchId, Runner>`. Two forks of one agent are two branches,
  both live. Nothing is minted; nothing session-local enters the log.
- **Every command names its branch.** `UserTurn`, `Reply`, `Interrupt`,
  `Restart`, `Fork`, `Resume`, `Spawn`, `Rename` carry a `BranchId`.
  `Session.root` and the `idle_root()` / `"frame is busy"` family are
  deleted. A UI holds a selection; the loop holds no cursor.
- **Fork adds a branch; it never moves you.** `Fork { from, name }` logs
  `Fork { name }` as the new branch's root, returns its id, and leaves
  every branch live. A fork inherits *history, not obligations*: pre-fork
  open posts and pending calls stay the original's, and the fork is born
  idle (`shown` = its root). "Ask a running agent something without
  pausing it" is `Fork` at its current leaf; no separate gesture, the
  sidebar owes the original's questions nothing, and the rendered `Fork`
  line tells the model so.
- **The user takes a turn.** `Restart { branch, call }` — where `call` is
  any tool the branch's menu would offer: `resume(value?)`,
  `run_program(source)`, `answer(question, value)` — on a suspended *or
  idle* branch cancels any in-flight LLM turn and logs a **`Turn {
  author: User }`** carrying that one call. The report that follows
  answers it with a normal `call_id`, so nothing downstream is special;
  the LLM's later history shows the branch resumed with 5, which is
  true. This is the handler hierarchy's outermost layer made literal —
  the user supplies a value, pastes a rewrite, or answers a pending
  question on the original branch after exploring in a fork, without
  spending an LLM turn.
- **Bounded by permits, not structure.** `llm_permits` is the only
  throttle and the cost control. The single inbox gives one arrival order;
  fuel slices round-robin across branches.
- **Quiet, not "awaiting user."** The session is quiet when no branch has
  work in flight; `run()` returns on quiet.

## The removal ledger

- Event shapes: `Message::System` (a stored copy of something
  regenerable), `FrameResult`, `FrameStart.prompt/input`; `User`/`Tool`/
  `Assistant` become `Post`/`Turn` plus derived reports.
- `Phase::Done`, `StepOutput::Yielded`, `StepOutput::FrameDone`, `is_root`,
  `Spine::is_complete`, `Frame.result`, `Tree::append`'s seal assert,
  `Tree::fork`'s completed-spine error.
- `Session.root`, `reanchor_root`, `idle_root`, `awaiting_user`, `parents`,
  the three "spine is complete" rejections, the two `on_user_turn` panics.
- `ANSWER_RETRY_LIMIT`, `answer_retries`, the tighten-it re-prompt,
  delivery-time `truncate_answer`.
- `EventPayload::Label`, `SessionCommand::Label`, `cmd_label`,
  `LeafInfo.label` and its "nearest label on the spine" search —
  superseded by a `name` on every branch root.
- "The root yields" and the root-only address — the user is present in
  every branch the same way.
- The word "frame" for anything but the VM.
- From earlier drafts (never built): descendants-only asks,
  one-open-question-per-target, parked-vs-computing delivery,
  `is_parked()`, a pending queue, `Sidebar`, the user as frame 0, a
  session-minted `SpineId`, "first `Answer` wins" across forks.

## What this deliberately does not add

- **No turn-level `ask`.** See "a turn can post; only a program can await."
  If transcripts show models fumbling the one-liner, add sugar that
  desugars to exactly it — never a branch-level wait.
- **No per-post urgency flag.** Every post is delivered at the next safe
  point; `tell` changes what is owed, never when it is seen.
- **No second delivery per question.** A `Send` gets exactly one
  `Result`. Multiple answers are multiple questions.
- **No broadcast / relay / kill / subscribe tools.** Composition is the
  program's job. The one discovery tool, `agents`, exists only because
  handles die with the VM.
- **No program-level post listener yet.** A `tools.next_post()` that let
  an orchestrator's *program* consume agent-authored tells (rule C applied
  to posts: if the program awaits them, they are data) would let routine
  progress reports be handled in code and only anomalies escalate via
  `raise`. Right shape if the LLM-turn-per-notification cost proves real;
  user posts would stay conditions regardless (the user is an authority in
  the handler hierarchy, not a data source). Not until a transcript shows
  the need.
- **No per-request rewriting of the prefix.** Presence, status counts, or
  any other "right now" fact goes in the trailing line or in a tool
  result — never into the system prompt or a rendered message, which
  would invalidate every cached branch on every flip.
- **No interactive/autonomous branch mode, and no idle re-prompting.**
  See "How it is driven": autonomy is the program running; presence is a
  per-request fact. A branch is never woken without a cause event.
- **No ask deadline or timeout.** A parked ask costs nothing, and the
  agent that knows no one is attached can choose to proceed instead. If
  transcripts show agents stuck on questions nobody will answer *despite*
  knowing it, the fix is card wording first; a `timeout` on `ask` is the
  fallback, not the opener.
- **No bookmarks.** `Label` is deleted, not replaced: marking a *point*
  mid-branch has no consumer left now that branches are named and the
  navigator is a branch tree. If a "jump back to here" gesture is ever
  wanted, it is UI-local state over event ids and needs no log event at
  all; a `Note` payload would only be right if annotations must survive
  for *other* readers.
- **No cross-branch context sharing, no scheduler, no async runtime.**
- **No eviction policy yet.** Idle runners hold no VM but keep `last_vm`;
  `states` grows monotonically. Cap by recency once a real session hits a
  number worth capping; the shape is the re-hydration reconciliation
  already needs.
- **No migration of pre-17 logs.** The log format changes (Part A); old
  logs are versioned and refused with a clear message.

## Ground rules

- Commit per step, `branches:` prefix.
- Gate after every step: `cargo fmt && cargo clippy --workspace
  --all-targets && cargo test` — all three green, zero warnings.
- **Finish a step before starting the next.** Part A is the foundation:
  A1–A5 are behavior-preserving, A6 is the first behavior change; do not
  start Part B on a half-migrated vocabulary.
- No network in any test; `ScriptedLlm` drives everything new. Live
  verification is the user's to run and is called out where it matters.
- `14_CLEANUP.md`'s lint rules still apply.

---

## Part A — Vocabulary

### Step A0 — DESIGN.md amendment

- [x] DESIGN.md gains "Exchanges": the vocabulary (agent / branch / spine /
      context / frame), rules A–C, the four-event exchange, the user as an
      author who borrows the branch's context (no user branch — forking
      is why), and that branch ids are root event ids (concurrency leaves
      the log unchanged). Roadmap range `0_…`–`17_…`.
- [x] The dependency-spine paragraph adds to link 3: after a resume no
      completed work is invisible (reconciliation), and reuse-by-id covers
      in-flight exchanges (a pending ask is re-awaited, not repeated).
- [x] The condition table's "user interrupt / steering" row is marked as
      built here, and gains the annotated-source report.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A1 — Rename: `frame` → `agent` everywhere but the VM (`agent/src/**`)

Mechanical, no behavior change, its own commit so the diff is reviewable
by eye.

- [x] `FrameStart` → `Agent`, `FrameId` → `AgentId`, `Frame` (tree) →
      `Context`, `frame()` → `context()`, `frame_list` → `agent_list`,
      `enclosing_frame` → `enclosing_agent`, `Session::frames()` →
      `agents()`, `AgentState` → `Runner`, `spawn_child`'s "child frame"
      comments, `SessionEvent::Event { frame }` → `{ agent }`, the TUI's
      "frames pane" → navigator. `VM::frames()`, `CallFrame`, and the
      stack pane keep their names.
- [x] `grep -rn "frame" agent/src` shows only VM-stack uses, the
      `ratatui::Frame` render type, and this phase's doc references.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A2 — Typed calls: `Send` / `Spawn` / `Invoke`, `Result`, `site` (`interp`, `types.rs`, `machine.rs`, `report.rs`)

Before the message reshape, because `Post.origin` names a `Send`.

- [x] `InvokeCall` gains `site: Span` (the call instruction's
      `spans[ip]`); one interp test asserts it for a nested call.
- [x] `Send { to, text, input, expects_reply, site }`, `Spawn { name,
      charter, tools, site }`, `Invoke { name, args, site }` logged at
      dispatch —
      `dispatch_calls` is the one place a `tools.*` name becomes a
      variant; `Result { call, outcome }` at landing (resolution order, as
      today). `tools.tool_result` accepts a `Result` id, or a call id that
      resolves to its `Result` if one exists. No harness code outside
      `dispatch_calls` matches on the strings `"ask"`/`"tell"`.
- [x] Menu rows read their label from the call variant (`ask(to, "…")` /
      `tell(to, "…")` by `expects_reply`, `spawn(name)`, `name(args)`),
      value from the `Result`; a `Result`-less `Send` renders *pending —
      await tools.tool_result(#N)*,
      a `Result`-less `Invoke` *issued; no result recorded; may have
      happened*. Golden reports updated.
- [x] `fanout_logs_in_completion_order` asserts on `Result` order;
      `oversized_result_is_guarded_before_the_log` on the `Result`.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A3 — Rendered messages: `Message::{Post, Turn}`, `Agent`, `Rename` (`types.rs`, `tree.rs`, `machine.rs`, `deepseek.rs`)

`Message::Tool` stays for now and is deleted in A4, so this step is a
reshape of the two kinds that survive.

- [x] `Message` reshaped: `Post { from, origin }` where `origin` is
      `Sent(id)` or `Direct{text, input, expects_reply}`, and `Turn {
      author, text, thinking, tool_calls }`; `Author` enum added; doc
      comments give each parent rule, branch, and render role. No body is
      ever copied — resolution goes through the `Tree`, so `replay_event`
      takes it. One question with a large `input` fanned to three workers
      is stored once (`fanned_body_is_stored_once`); a user post with no
      send side round-trips (`direct_post_carries_its_own_body`).
      Mechanically: `replay_event` gains `&HashMap<EventId, Event>` beside
      its `&mut Vec<Context>` — `spine_at` already holds `&self`, and the
      two borrows are disjoint, so no restructuring is needed. Resolved
      bodies are cloned into the in-memory `Context`; only the *log* is
      free of copies.
- [x] `Agent { name, charter, tools, system }`; `ensure_system` deleted;
      the system message is rebuilt by `render_request` from
      `Agent.system` (the stored-prompt test becomes "the request's first
      message equals `Agent.system`").
- [x] `EventPayload::Label`, `SessionCommand::Label`, `cmd_label` and
      `LeafInfo.label` deleted; `Rename { name }` added; `tree::tests`'
      label cases become branch-name cases, plus: a branch's name is the
      last `Rename` at or after its root, a rename on the original leaves
      a fork's name alone, and a `Rename` triggers no prompt
      (`rename_folds_from_the_branch_root`, `rename_does_not_wake`).
- [x] `replay_event`: `Message`s → `messages`; `Context.prompt/input` →
      `Context.charter/system`; `input` renders as a bounded preview while
      the whole value reaches the program
      (`large_input_previews_in_context_and_binds_whole`).
- [x] `deepseek.rs` request builder maps the new shapes; fixture tests
      updated, still pure.
- [x] Every existing test passes with assertions re-expressed and **not
      weakened**.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A4 — Outcomes and derived reports (`types.rs`, `machine.rs`, `report.rs`)

The load-bearing step of Part A: after it, no renderer touches the `VM`.

- [x] `EventPayload::ProgramResult` renamed `Return`; `Condition { cause,
      site, stack }` added (`machine::Suspension` renamed to match,
      gaining `CompileFailed` and `Interrupted`) — exactly one outcome per
      handback, carrying every input its report needs.
      `ProgramStatus` becomes derivable from a reopened log
      (`program_status_survives_reopen`); a run that raises, resumes,
      traps, resumes and returns logs **three** outcomes — one per
      handback, and the arithmetic in the original box was off by one
      (`one_outcome_per_handback_not_per_run`).
- [x] `Message::Tool` **deleted, not ported**: tool-role messages are
      rendered by `report.rs` from the run's outcome and the events
      around it, and paired to their call positionally.
- [x] **Every tool call has exactly one outcome event**, so no report is
      derived from recomputed history: `run_program`/`resume` →
      `Return`/`Condition`, `answer` → `Answer`, and an ineligible call →
      `Condition{cause: Refused{reason}}` (a new cause beside
      `CompileFailed`, which likewise never ran a VM). Without it, a
      refusal's tool message would have to be rebuilt by replaying
      eligibility to that path position — derivable, but fragile
      (`every_tool_call_has_an_outcome_event`).
- [x] `report.rs` renderers become pure functions of the log — no `VM`
      access. Concretely each takes `(&Tree, leaf: EventId, turn:
      EventId)` and reads forward from `turn` to its outcome: the source
      from the turn's tool-call args, the outcome, the `Console`, and the
      `Result`s and menu rows on the path. A
      golden test renders the same log twice and byte-compares
      (`derived_reports_are_stable_for_a_renderer`); another renders a
      completion, a condition, a post-condition, a compile error, an ack
      and a refusal from fixtures.
      *(The **ack** was the one this box promised and did not deliver:
      `is_outcome` covered `Return` and `Condition` only, so an `answer`
      tool call had no outcome, no derived tool message, and a dangling
      `tool_call_id` in the next request — the M2 400, latent since B2.
      Built in C2, where `Restart{Answer}` reached it deterministically:
      `Answer` is an outcome, and its report says what was answered and
      where it went, walked out of the same closed loop of ids
      (`an_answer_call_is_replied_to_like_any_other`).)*
- [x] Derived reports are **memoised** on the `Tree` (`HashMap<EventId,
      String>` keyed by the outcome event id, cleared wholesale on
      renderer change) — never logged. The `Tree` is the right home
      because renders happen per branch per request and the memo must
      outlive any one `Runner`. Without it every request re-derives every
      report on the path and a session is quadratic in branch length; with
      it re-derivation is amortised O(1), and the memo is dropped when the
      renderer changes (`report_memo_avoids_rederiving_history`).
- [x] `Console` capped by named byte/line consts with a truncation marker
      naming the event, so the rest stays fetchable
      (`oversized_console_is_capped_and_marked`).
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A5 — Durability granularity (`tree.rs`)

Small and independent; land it before the log grows a fan-out's worth of
events per step.

- [x] `Tree` syncs once per loop step rather than per event; a fixture cut
      mid-step reopens to the step's start and reconciles
      (`sync_per_step_survives_a_torn_tail`).
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A6 — `Answer`; agents never close (`types.rs`, `tree.rs`, `machine.rs`, `host/mod.rs`)

- [x] `Answer { question, value }` added; `FrameResult` deleted;
      `Context.result` → `Context.open: Vec<EventId>` maintained by
      `replay_event`.
- [x] `Spine::is_complete`, the seal assert, `fork`'s completed-spine
      error, `Phase::Done`, `Yielded`, `is_root`: deleted. `finish_frame`
      → `answer_open`: logs `Answer` for the oldest open post, emits
      `StepOutput::Answered { question, value }`, phase → `Idle`.
- [x] `UserTurn { branch, text }` logs `Post { from: User, origin:
      Direct{…} }` on the branch; `Answered` for a user-authored post emits a
      `SessionEvent` only (the answer is read inline); for an
      agent-authored post it routes to a `Result` on the asking branch.
      *(Landed via `waits`, not `origin`: A6's agent-authored posts are
      still `Direct` — nothing issues a `Send` until B1 — so there is no
      `origin` to route by yet. The `Result` lands on the asking branch
      either way; B1 switches the lookup.)* `Context.open` counts only posts at or after the branch's
      root: `replay_event` clears it on `Fork`, so a fork's bare turn
      logs no `Answer` for a pre-fork post while the original's does and
      delivers (`fork_does_not_owe_prefork_posts`). `Reply { branch,
      call, value }` logs `Result` for a pending `Send { to: user }`.
      `parents` deleted; a `waits: HashMap<EventId /*post*/, (BranchId,
      promise)>` is populated live for now (C2 rebuilds it from the log
      on open).
- [x] `pick_resume_leaf` picks the lowest leaf with an open post or an
      unanswered `run_program`, else the lowest-id leaf.
- [x] After a child answers and the parent joins, a second post to the
      child gets a second `Answer` and the parent's branch is untouched
      (`answered_agent_stays_addressable`). A user turn is exactly one
      `Post` and its answer exactly one `Answer` — no `Invoke`/`Result`
      anywhere (`user_turn_is_a_post_on_the_branch`).
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

---

## Part B — Exchange semantics (machine-level, scripted)

### Step B1 — `spawn` / `ask` / `agents`; `agent` as sugar (`host/tools.rs`, `machine.rs`, `dialect.rs`)

- [x] `tools.spawn({ name?, charter, tools? }) → { agent }` (`name` lands
      on the `Agent`; `tools` is enforced by the registry as the
      child's allowlist, default the caller's); `tools.tell({ to?,
      text, input? }) → { post }` resolves in the same step its `Post`
      lands, and the recipient owes no `Answer`
      (`tell_delivers_receipt_and_opens_nothing`); `tools.ask({ to?,
      text, input? }) → value` where `to` is a branch id or an
      unambiguous agent id (an ambiguous one rejects the call naming the
      live branches); `tools.agent(...)` desugars to both in
      `dispatch_calls`, schema and card line verbatim.
- [x] `tools.agents({ under?, deep? })` served from tree + session state:
      direct children by default, the subtree with `deep`, one row per
      branch with `agent`/`branch`/`name`/`charter`/`parent`/`status`/
      `open`/`last_answer`. Tests: a program spawns three workers and a
      later program (new VM) lists them with correct status
      (`agents_survive_across_programs`); `deep` reaches a grandchild; a
      forked worker lists as two rows sharing `agent`
      (`forked_agent_lists_two_branches`); `Promise.all` over `agents()`
      fans a question out and joins (`broadcast_is_promise_all_over_agents`).
- [x] A spawned agent's `Agent` is a child of its `Spawn`. A question
      logs `Send` **first**, then `Post { source: <that send> }` on the
      callee; the callee's `Answer { question: <that post> }` produces
      `Result { call: <that send> }` on the asking branch. A test
      walks the loop from each event to the other three
      (`exchange_ids_form_a_closed_loop`).
- [x] `to` omitted resolves to the author of the oldest open post — the
      human for a root conversation (the invoke is `to: user`), the parent
      for a subagent; a test covers both (`default_to_is_the_current_asker`).
- [x] Two sequential asks to one agent accumulate context
      (`second_question_sees_first_exchange`).
- [x] Card: an **eligibility** paragraph — `run_program` always;
      `resume` only after a condition report; `answer(q)` only for a post
      open on this branch; an ineligible call costs a turn and tells you
      what is valid. Then the "Orchestrating" paragraph above, verbatim,
      beside the tool lines; `agent` stated as the one-shot form.
      *(Two wording changes, both because the paragraph as drafted
      described things that are not built: the presence sentence points at
      a system-prompt line, which the doc's own cache discipline forbids
      and C1 puts in the trailing line instead — so it states the decision
      it exists to drive ("a question to the human may sit unanswered, so
      prefer a stated assumption") and C1 adds where to look.)* The
      static-text bound in `dialect::tests` is **not** raised: it is
      30,720 bytes against 12,076 of static text, so the paragraphs come
      nowhere near it and there is nothing to raise. Tightening it to
      binding is a change of that test's meaning, not this step's work.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step B2 — The `answer` restart; explicit binding (`machine.rs`, `report.rs`)

- [x] The tool list becomes **constant** — `run_program`, `resume`,
      `answer` on every request, replacing `render_request`'s
      phase-dependent `match` (cache discipline). The M1-era test that
      asserts different tool names for idle vs suspended is re-pointed to
      assert the list never varies
      (`tool_schemas_are_constant_across_phases`).
- [ ] **(Live, user-driven)** measure it: two DeepSeek requests on one
      branch differing only in the tool list, comparing
      `usage.prompt_cache_hit_tokens`. Record the number here. If tools
      do not affect its cache, keep the constant list anyway (simpler)
      and strike the cache justification from the doc.
      *(Left unchecked: this one is yours to run — it needs the network.
      The constant list landed regardless, per this box's own
      instruction, so nothing waits on the number.)*
- [x] `answer(question, value)` logs `Answer`, routes as in A6, leaves
      `phase` unchanged. `resume_spec()`'s value becomes optional. *(A
      turn may also carry `answer`s **ahead of** one program-driving
      call, which B3's `answer` + `resume` needs: they settle
      synchronously, so outcomes stay in call order.)*
- [x] One `eligible(&self, call) -> Result<(), Refusal>` check, used for
      every restart. An ineligible call is answered with a self-sufficient
      refusal — what is true, what is valid now — changing no state:
      `resume` with nothing suspended — including a branch reopened at a
      `Condition` whose VM did not survive, whose refusal says so and
      points at the artifact menu — `answer` with nothing open, and
      `answer` for a post open only before this branch's `Fork` root,
      and `tool_result` on a `Send` still pending from before that root,
      both refusals naming the owning branch
      (`ineligible_restart_reports_and_recovers`,
      `fork_cannot_answer_a_prefork_post`). This check is the *only*
      enforcement of the fork-obligations rule.
- [x] The report's **restarts** section lists what is eligible for this
      suspension with a one-line reminder each; the *rules* live in the
      card (cache discipline). Golden reports updated — the three
      existing ones are byte-identical (their branches have nothing open)
      and `golden_condition_report_with_an_open_question` pins the new
      `answer(#N, value)` line.
      *(C3 takes the open-post lines back out, and the `run_program`
      reminder with them: an open post is a **now**-fact and a rendered
      message says it forever, and the reminder was a rule the same
      cache-discipline split puts in the card. What is left is what
      genuinely varies per handback — the `resume` wording.)*
- [x] `llm.rs` gains `scripted_answer(call_id, question, value)` beside
      `scripted_program`/`scripted_resume`/`scripted_text`.
- [x] Two open posts: a bare turn binds to the older, `answer` to the
      named one (`explicit_answer_binds_the_named_post`); a JSON object
      reaches the asking program as an object
      (`structured_answer_reaches_the_program`).
- [x] Requests with >1 open post carry a bounded one-line note listing
      them by id. *(It rides a new `LlmRequest.tail` — the trailing
      ephemeral line after the newest message, which is the only place a
      right-now fact may go. C1's presence line lands in the same field;
      C3 drops the `>1` threshold, because a single open post is exactly
      the one that gets stranded.)*
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step B3 — Rule B: posts on arrival, `Condition::Posted`, the annotated report (`machine.rs`, `report.rs`)

- [x] `on_user_turn`'s panics deleted *(in B1, when agent-authored posts
      started flowing through the same door)*; the host's busy-rejection
      of a `UserTurn` is deleted here. A post is appended in **every**
      phase; `Runner.shown: u64` (event-id high-water mark at the last
      request render) replaces any queue. The trigger rule is one
      function, `Runner::needs_prompt(&self, tree) -> bool` — no VM held
      and an unseen `Post`, or a `Turn` whose calls have outcomes — and every request is
      rendered through it (`prompt_iff_unseen_post_and_no_vm`,
      `fork_is_born_idle`). *(A `Runner` opened on an existing branch
      starts `shown` at its leaf, so a re-opened branch waits to be
      spoken to and a fork is born idle by the same line; lowering it
      where reconciliation owes a prompt is C2's. And C3 adds the clause
      this was missing: an **open** post is a cause whether or not it has
      been shown. A bare turn answers the oldest and no more, so a branch
      shown several questions answered one and stranded the rest past the
      mark — each stranded agent-authored one a `Send` that never
      settles.)*
- [x] Request rendering places each derived tool message immediately after
      the `Turn` whose call it answers, regardless of log position; a user post logged mid-run
      renders after the report, and the report's *what happened* names
      its arrival (`mid_run_post_renders_after_the_report`; the
      `deepseek.rs` request-body fixture covers the adjacency).
- [x] `Running` + unseen post at `Tick` → `Condition::Posted`: report
      whose *what happened* is the message(s), author-labelled and marked
      *asks you* / *tells you*; *where* is
      the **annotated source** (every call site → `#id done` / `#id
      pending — await tools.tool_result(#id)` / `issued; may have
      happened`) plus console tail; restarts `answer` / `resume` /
      `run_program`. `resume()` re-enters the same VM; outstanding results
      still land (`post_to_running_program_resumes_cleanly`).
      *(A **parked** program has no next slice of its own — it is
      awaiting a call and burning no fuel — so `deliver` asks for one.
      That is what makes "at the next slice" a guarantee rather than a
      hope, and it is why an upward question cannot deadlock.)*
- [x] `AwaitingLlm` + post: the post is already logged; on `LlmDone`, a
      text answer → idle → the post starts the next turn; a `run_program`
      → first slice suspends with the post
      (`post_during_generation_lands_after_it`,
      `post_during_generation_suspends_the_program_it_started`).
- [x] Suspended-on-a-condition + post → appended beside the report, re-request with the
      same menu (`post_while_suspended_is_shown_beside_the_report`).
      *(No **immediate** re-request: a suspended branch has its condition
      report out and is awaiting the restart choice, so a second request
      would double-prompt. The next request shows both, and the pending
      report re-renders byte-identically — which is the responsiveness
      table's own wording ("appended beside the pending report; the next
      request shows both"). `Interrupt` (C1) is the override that makes a
      post land now. **Confirmed in C1**, and it is what
      `Runner::interrupt` does for a suspended branch: it cancels the
      request in flight and re-renders — not a second prompt for the same
      cause, but the same one, carrying what has arrived since.)*
- [x] Upward round trip, scripted: parent awaits child; child program
      `tools.ask({ text })` posts to the running parent; parent's LLM
      `answer`s + `resume`s in one turn; child gets its `Result`, answers;
      parent's ask resolves (`upward_clarification_does_not_deadlock`).
- [x] Rewrite-with-reuse: a user post mid-fan-out; the scripted rewrite
      reuses two done results by id and `await`s one pending by id; no
      call is re-issued (`post_rewrite_reuses_done_and_pending_by_id`).
      *(Re-attach is by "this session still has it in flight", not by
      call variant: the **menu's** wording is derived from the log, which
      cannot tell a live worker from one that died with the process, so
      it stays cautious for an `Invoke`. The session knows better.)*
- [x] Golden render for the post-condition report, annotated source
      included; every section bounded by named consts
      (`golden_post_condition_report`; `POST_MAX_BYTES`,
      `ANNOTATED_SOURCE_MAX_BYTES`, `ANNOTATIONS_PER_LINE`).
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step B4 — Budget as a rendering rule (`machine.rs`, `report.rs`, `dialect.rs`)

- [x] Every clip in a report names a fetchable id, not just a count: a
      clipped `returned:` names its `Return`, and a clipped console tail
      names its `Console` (today it says only `last X of Y`, so the rest
      is unreachable — the one truncation in the system with no way back
      to the whole). `tools.tool_result` accepts a `Console` id and
      returns its lines (`clipped_console_is_fetchable_by_id`).
- [x] Because reports are derived, a large return is stored exactly once
      (`log_holds_the_returned_value_once`); the report clips it to the
      budget at render time and names its `Return` id.
- [x] `Console` capped by named byte/line consts with a truncation marker;
      the marker names the event so the rest is fetchable
      (`oversized_console_is_capped_and_marked`) — landed in A4; B4 adds
      the *report*-side half (`clipped_console_is_fetchable_by_id`).
- [x] `Result { call, outcome }` with `Delivered(value)` / `Failed(message)`;
      a `Failed` rejects the program's promise, and an uncaught rejection
      traps into `Condition{Trapped}` while a caught one leaves no
      condition at all (`failed_call_rejects_then_traps_only_if_uncaught`).
      The oversized-result guard produces a `Failed`, not a substituted
      value (`oversized_result_guard_is_a_failure_not_a_substitution`).
- [x] The completion report states how many of the run's calls failed, so
      a program that swallowed errors and returned a thin result does not
      read as clean success (`completion_report_counts_failed_calls`).
- [x] A `run_program` that raises, resumes, traps, resumes and returns
      logs four outcomes and renders four reports — one per handback —
      with `Return` only on the last
      (`one_outcome_per_handback_not_per_run`,
      `each_handback_renders_its_own_report`). *(The arithmetic in this
      box is off by one: raise → resume → trap → resume → return is
      **three** handbacks, which is what the A4 test pins. Four needs a
      fourth stop, so the new test's program traps twice.)*
- [x] Values stored whole; reports clip to `answer_budget` naming the id.
      `ANSWER_RETRY_LIMIT`, `answer_retries`, the tighten-it post,
      delivery-time `truncate_answer`: deleted. The over-budget test
      asserts "stored whole, rendered clipped with id"
      (`over_budget_answer_is_stored_whole_and_rendered_clipped`).
      *(The `mind_bound` computation went with them: after B1 an answer
      crosses to the asker as a `Result` — machine-bound, into its
      **program** — so there was no longer a context copy for it to
      guard. The remaining clips are the report's own.)*
- [x] Card: the re-prompt sentence removed; "returns and answers are
      budgeted in *your context*, delivered whole to the asking program"
      stated once; "a pending ask in the menu is awaited with
      `tools.tool_result(#id)`, never re-asked" added under reuse.
      *(The card never carried a re-prompt sentence — the "tighten it"
      nudge was a runtime harness post, not card text. Nothing to
      remove; the budget sentence is rewritten in place.)*
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

---

## Part C — Live branches, interrupt, restart, reconciliation (host-level)

### Step C1 — `BranchId`; addressed commands; fork adds; interrupt; restart (`host/mod.rs`, `protocol.rs`, `tree.rs`, `main.rs`)

- [x] `BranchId = EventId` in `protocol.rs`; `Tree::branch_of(leaf)` walks
      up to the nearest `Agent` or `Fork`;
      `states`/`paused`/`starved` and every `LoopMsg` keyed by
      `BranchId`; `SessionEvent` variants carry `branch` alongside `agent`.
      *(The box also listed `waits`. It was deleted in B1, not re-keyed —
      every call is settled through its logged `Call` id — so there is
      nothing there to key.)*
- [x] `Fork { from, name }` logs `Fork { name }` as the new branch's root
      (empty name allowed) and returns its id via
      `SessionEvent::BranchOpened`; `Resume(leaf)` returns
      `branch_of(leaf)`. Commands: `UserTurn { branch, text, expects_reply }`, `Reply {
      branch, call, value }`, `Rename { branch, name }`, `Interrupt {
      branch }`, `Restart { branch, restart }`, `Spawn { parent, name,
      charter, text }`. `Session.root`, `reanchor_root`, `idle_root`, the
      busy rejections: deleted. `Branches(Vec<BranchInfo { branch, agent,
      leaf, name, parent_branch, status, open, asking_user: Option<EventId>, thinking
      }>)`; `ListLeaves` remains the log projection.
      *(`Session::conversation_branch()` replaces `root` for the CLI and
      the UI's initial selection: the root agent's first branch, read off
      `Tree::branches()` — a fact about the log, not a cursor. And
      `LeafInfo.active` went with the cursor it named. The only rejection
      left is "there is no such branch in this log".)*
- [x] Two forks of one agent both grow under concurrent `UserTurn`s; the
      original leaf is untouched; `agents()` from the parent lists both
      (`two_forks_of_one_agent_run_concurrently`).
- [x] A fork is born idle: creating one issues no request; its first
      `UserTurn` does (`fork_is_born_idle`). `Fork` renders as a harness
      line, or — when the fork point's last `Turn` has an unanswered tool
      call — as that call's tool result naming the original branch and
      the artifacts so far; golden renders for both
      (`fork_line_renders`, `mid_program_fork_answers_the_dangling_call`).
- [x] `Interrupt` on an awaiting-LLM branch cancels the worker's stream
      (the `LlmClient` trait gains a cancellation token; the scripted
      client honours it), logs nothing for the cancelled turn, and the
      pending post starts a fresh turn (`interrupt_cancels_generation`);
      on a running branch it pauses at the next slice and the post lands
      (`interrupt_pauses_program`). *(Cancellation is enforced by a
      per-branch **epoch**, not by the client: a `LlmDone` whose epoch is
      stale is dropped whatever the client returned, so "from the API's
      view it did not happen" holds even against a client that ignores
      its token. And when there is no post to land, the **harness authors
      one** — see the unawaited-`Result` box below; one mechanism, both
      wakes.)*
- [x] `Restart { branch, call }` with `resume`, `run_program`, or `answer`
      on a suspended or idle branch: any in-flight LLM turn cancelled, a
      `Turn { author: User }` with that one call logged (synthetic
      `call_id`), applied exactly as if the LLM had made it, the next
      report answering that `call_id` like any other
      (`user_resumes_and_user_rewrites`, `user_answers_on_the_original_after_forking`).
      The TUI renders user-authored turns distinctly.
- [x] `awaiting_user` → `quiet()`; `run()` returns on quiet
      (`run_returns_when_every_branch_is_quiet`); two hot programs
      interleave and a third branch's turn is served
      (`hot_programs_do_not_starve_other_branches`). *(Quiet is "no
      **worker** in flight and no branch thinking", counted rather than
      derived from phases: a worker that has produced its answer but has
      not yet been drained is still work in flight, and no phase shows
      that. B's `run_routed` helper now calls `run()`, and its `QUIET`
      const is deleted — as intended.)*
- [x] Presence: `Session` tracks whether a client is attached (the TUI is;
      `--headless` without `--turn` is not) and `render_request` emits it
      as a **trailing ephemeral line after the newest message** — never in
      the system prompt, never logged. Attaching mid-run changes only the
      next request's tail; the prefix before it is byte-identical across
      the flip (`presence_flip_does_not_disturb_the_prefix`). The
      Orchestrating paragraph's presence sentence is restored to point at
      that line.
- [x] **Rule C's other half: an unawaited `Result` is surfaced as a
      harness post.** A `Result` that lands with no program awaiting it —
      the run was rewritten away, or the branch holds no VM — is logged as
      an artifact *and* delivered as `Post { from: Harness, expects_reply:
      false }`, which wakes the branch through the existing trigger rule
      and owes no answer
      (`unawaited_result_wakes_the_branch_as_a_harness_post`). This is
      what the Open Questions section already decides ("default to
      waking"), and it is a **legal** wake because the rule is *never wake
      a branch without a cause event you can name in the log* — not
      "never wake". The cause is logged, visible, auditable, and renders
      identically forever. Without it, an orchestrator whose program ended
      while its workers ran would get their answers logged and nothing
      else.
- [x] `agent session --list-branches`; `--turn` addressed to the
      conversation branch; `--fork` prints the new id.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step C2 — Reconciliation on open (`host/mod.rs`, `tree.rs`)

- [x] `Tree::unmatched()` returns the rows of the reconciliation table
      from a scan.
      *(The box added "`waits` is rebuilt from it and no longer
      maintained separately". `waits` was deleted in B1 — every call
      settles through its logged `Call` id — so this reduces to
      `unmatched()` alone, exactly as the C-part preamble said.)*
- [x] `Session::open` re-hydrates every branch with an open post or a
      pending ask-to-user as live and applies the repairs; a fixture cut
      after each of the four exchange events and after a host `Invoke`
      reopens to the expected state
      (`reconcile_after_cut_at_each_exchange_event`).
      *(And every branch the table names at all — a `LostInvoke` needs no
      repair but its menu still has to say *may have happened*.)*
- [x] A cut after `Return` but before the next turn reopens with the
      completion report rendered from the log and the branch idle — the
      program is **not** re-run and no rewrite is requested
      (`crash_after_return_completes_the_run`). *("Idle" is the phase:
      the branch holds no VM. It does take a turn, because the trigger
      rule's own crash-recovery clause says so — a run whose report was
      never sent still has its outcome, so the report renders and the
      branch prompts normally. That is a **cause event**, which is why
      lowering `shown` here is legal and re-prompting a branch that
      merely stopped talking is not.)*
- [x] A cut between `Agent` and its `Spawn`'s `Result` reopens with
      the handle delivered and **no second agent** created on re-execution
      (`interrupted_spawn_does_not_orphan_its_agent`).
- [x] A cut between `Answer` and `Result` reopens with the `Result`
      appended and listed in the asker's menu; a cut after a host `Invoke`
      lists *may have happened*; a cut with a question pending for the
      user reopens with that branch live, highlighted, question inline
      (`user_owed_answer_survives`).
- [x] `tools.tool_result(id)` on a pending ask returns a promise resolved
      by its `Result`. A child cut after its upward `Send`
      re-enters, its scripted rewrite awaits by id, the parent answers,
      the child completes — the parent holds **one** `Post`, not two
      (`reentered_child_reattaches_instead_of_reasking`).
      *(Two seams this needed. `reattachable` was gated on this session
      still holding the call in `pending`, which after a crash is never
      true; it now also accepts a `Send` the **log** says is unsettled,
      because a `Send`'s answer routes home by logged ids alone while an
      `Invoke`'s worker died with the process — which is the distinction
      the menu already draws. And `on_tool_results` dropped any `Result`
      with no `pending` entry, so an answer arriving home after a reopen
      was logged nowhere; it now logs one for any unsettled call on the
      branch's own path, which is what "after a resume, no completed work
      is invisible" actually requires.)*
- [x] Log versioning **added** (none exists today): the first line of a
      log is a `{"version": N}` header; `Tree::open` refuses a missing or
      older version with a message naming both. `Tree::new` writes it.
      *(`Tree::new` has never been handed a file — production creates
      through `Tree::open`, which now writes the header when it finds an
      empty file. Putting it in `Tree::new` would make the in-memory
      constructor fallible for no gain.)*
- [ ] Live verification (user-driven, DeepSeek): a two-worker task,
      `Ctrl-C` mid-flight, reopen; both workers finish, both answers
      appear in the parent's menu, the parent's rewrite reuses them by id.
      Record the transcript note here.
      *(Left unchecked: it needs the network and is yours to run.)*
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step C3 — Obligations wake, and ride the tail (`machine.rs`, `report.rs`, `dialect.rs`)

Found by C2, which made the disagreement observable: the in-session
trigger rule and the recovery table said different things about the same
state. A branch shown three questions answered one and went idle owing
two — while row one of the reconciliation table said an open post means
the branch becomes live. Reopening the log healed a state the running
session could not leave.

The rule that settles it: **`shown` and `open` answer different
questions** — *what have I rendered?* and *what do I still owe?* — and
reading the render mark as the obligation is what stranded them.

- [x] **An open post is a cause.** `needs_prompt` wakes an idle branch
      while anything is open, shown or not. It terminates by
      construction: a bare turn discharges exactly one open post, so the
      count strictly decreases, and "woke with nothing to discharge" is
      precisely `open.is_empty()`. A fan-in of *n* asks is answered to
      the last one and the branch then owes nothing
      (`a_fan_in_of_asks_is_answered_to_the_last_one`); a **tell** wakes
      once and never again, because it was never open
      (`a_tell_wakes_once_and_owes_nothing`); a fork is still born idle,
      because `replay_event` clears `open` at its root.
      *(This is not a re-prompt. The `Post` is a cause event — logged,
      visible, auditable, rendering identically forever — which is the
      whole of what "never wake a branch without a cause event you can
      name in the log" asks. What remains forbidden is the wake whose
      only cause is the absence of one.)*
- [x] **Obligations ride the trailing ephemeral line, not a rendered
      message.** Every open post is listed, one included — the old
      `>= 2` threshold rested on "a bare reply answers it, which is the
      default anyway", which is exactly the assumption the stranded case
      breaks. The line states the batching rule where the model will act
      on it, and **presence still goes last**
      (`many_open_posts_are_noted_in_the_request_tail`).
- [x] **The condition report stops listing open posts.** Whether a
      question is still owed is a fact about the branch *now*, not about
      that handback, and a rendered message keeps saying it forever:
      `answer(#4, value)` stays correct for the moment it describes while
      becoming a standing invitation to make an ineligible call, one
      refusal turn at a time. `Restarts` keeps only the `resume` wording,
      which genuinely differs by suspension kind.
      `golden_condition_report_with_an_open_question` pins the same fact
      in the tail instead of in the report. `Handback.open` and `open_at`
      are deleted with it.
- [x] **Every artifact appears in exactly one report.** The condition
      report listed the whole menu (`menu_since(h, 0)`) while the
      completion report listed only its own run's, so a branch with N
      conditions carried N near-identical menus in a prefix it can never
      shed — at the boundary this project crosses most. Both are now
      bounded below by the **previous outcome**, so the reports on a
      branch partition its artifacts with no gaps and no repetition
      (`reports_partition_the_artifacts_they_list`), and the tail carries
      the pointer back: how many rows exist and the ids they span
      (`the_tail_points_at_every_artifact_a_report_no_longer_lists`).
- [x] **The `run_program` reminder moves to the card.** It was forty
      constant words in every condition report ever rendered — a *rule*,
      and the cache-discipline split puts rules in the card where they
      are cached for the branch's life. The restart's **name** stays,
      because which restarts are eligible is still a report-level fact.
- [x] `REPORT_FORMAT_VERSION` bumped to 2; the report memo drops
      wholesale, which is what that counter is for. Golden reports
      updated — the change is deliberate, not incidental.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

**What this leaves open, deliberately.** Whether an agent may *decline*
to answer a question it has been shown. C3 says no by construction — an
open post is owed until an `Answer` names it — and that is the reading
the recovery table already had. If declining is ever wanted it needs its
own **event**, for exactly the reason `Failed` is one: an absence cannot
be told apart from a crash, an interruption, or a model that lost track.

---

## Part D — The TUI: dancing (`debug/attach.rs`, `debug/chat.rs`)

Split into two steps rather than one box list with one gate, which gave no
checkpoint across a large surface: **D1** re-keys selection, the chat
pane, and the navigator from `AgentId` to `BranchId`, preserving
behaviour — no new gestures. **D2** builds the gestures dancing actually
needs: inline questions, the waiting-on-you header and jump key,
fork/spawn/interrupt/restart keys, the reply-vs-turn input mode, the
timeline filter, and the live annotated source.

*(The heading originally also named `debug/app.rs` — that module is the
standalone `agent debug` debugger's app state (9_TUI Step 2), untouched
by this phase. The attached-session TUI's app state is `AttachedApp` in
`attach.rs`, which is what D1 and D2's boxes below actually change; fixed
here.)*

### Step D1 — the navigator is a branch tree

- [x] The navigator is **one tree of branches**, nested exactly as the log
      nests them (`parent_branch`): a fork hangs under the branch it
      diverged from; an agent's first branch hangs under the branch that
      spawned it. Every node is named by its root — `Agent.name` or
      `Fork.name`, falling back to a derived label — with status (`idle` /
      `thinking` / `running` / `suspended` / `asking you`) and open count.
      The **two edge kinds are visually distinct** (a `⑂` marks a fork;
      an unmarked edge is a spawn — a new clean-room context), because
      that is the difference `context()` turns on. Selection is by
      branch; Tab cycles live branches; `1`–`9` select.
      *(Built from `Session::branch_infos()` — already everything the
      navigator needs; made `pub` so the same-thread TUI can call it
      directly instead of round-tripping `ListBranches` through the
      command queue, the way a UI on a different thread would.)*
- [x] A nameless branch displays a derived label (the first line of its
      own first post — at or after its root, so a fork's label is about
      what makes *it* different rather than the shared prefix — clipped)
      that is never logged; naming it replaces the derived label
      everywhere. *(`report::derived_branch_label`. The **rename key**
      itself is a gesture — a new one — and moves to D2 with the rest.)*
- [x] The tree shows **where a branch came from, not who can reach it**:
      a subagent spawned before a fork point appears under the original
      branch, though the fork can address it too (it is in the fork's
      inherited history). Reachability is `agents()`, and the two answers
      differing is correct, not a bug. *(Falls out of nesting by
      `parent_branch` rather than by agent — no separate mechanism
      needed.)*
- [x] `chat.rs` is re-keyed from `AgentId` to `BranchId`: `streaming`,
      `current_program`, `program_status`, and every transcript `Entry`
      now key on the branch an event landed on (`SessionEvent`'s `branch`
      field, carried alongside `agent` since C1) rather than the agent —
      so two forks of one agent no longer interleave into one stream. A
      forked branch's rows are its own events *plus* the shared prefix it
      inherited, reconstructed from the event stream alone (`fork_parent`,
      populated from each `Fork` event's `parent_id`) — never from the
      `Tree`, which this module is deliberately isolated from
      (`fork_inherits_prefix_not_the_original_s_future`).
- [x] `attach.rs`'s selection (`AttachedApp.selected`), `find_leaf`,
      `vm_for_program`, and `resolve_program` are re-keyed to `BranchId`.
      This is the bug C1 flagged, fixed: a forked branch now resolves its
      **own** `Runner`/VM instead of silently falling through to whichever
      branch happens to share the agent id ("it works only because an
      agent's first branch id **is** its `Agent` event id, so an unforked
      tree looks correct").
- [x] `Tree::agent_list`/`AgentView` had no consumer left once the
      navigator moved to `branch_infos`; deleted, along with the one
      other production caller of `Session::branches()` (the navigator's
      old status overlay) — see below.
- [x] `attach.rs` tests cover selection over multiple branches per agent
      (`concurrent_agents_list_and_retarget`, re-pointed at
      `branch_infos`); `chat.rs` gains
      `fork_inherits_prefix_not_the_original_s_future`, pinning that a
      fork sees the shared prefix and nothing the original does
      afterward.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

**An incidental deletion: `Session::branches()`.** Its one production
caller was the navigator's old `live` status overlay, which this step
replaced with `branch_infos` — a strict superset (it also covers dormant
branches, name, parent, and open count, from one query instead of two).
With that caller gone, the method had no reachable caller outside
`#[cfg(test)]`, which a binary crate's dead-code lint catches even though
several `host::tests` still wanted exactly that narrower query; they now
call a same-shaped `live_branches` helper kept in `host::tests` itself.
Flagging this because the doc's own seam list ("Session::branches() for
live status") named it something not to rebuild — it wasn't rebuilt,
just narrowed to the one place still wanting it.

### Step D2 — dancing

- [x] **There is no home; the tree comes to you.** A branch waiting on
      you renders its question above the input line, switching Enter to
      reply mode (`asking_question_text`), and the navigator already
      highlights it (D1's yellow row). The navigator's title is the
      header: counts of branches waiting on you and branches thinking
      (`branch_counts`); `w` jumps to the next one, cyclically
      (`next_waiting`). `t` opens the timeline — every `Post { from: User
      }` tree-wide, oldest first (`timeline_rows`) — a keyboard-driven
      overlay (`j`/`k` move, Enter jumps to that row's branch and closes
      it, `t`/`Esc` closes without jumping): a filter you open, not a
      place you live, exactly as the box asks — not mouse-clickable in
      this pass, since that needed plumbing a second click-target list
      through `on_mouse` for a gesture the doc itself calls secondary.
- [x] A rename key (`r`) arms an explicit input mode that sends `Rename`
      on the next Enter, replacing the derived label (D1) everywhere it
      was standing in.
- [x] The input line always sends to the selected branch: `UserTurn` by
      default, Alt+Enter as the tell modifier (`expects_reply: false`);
      `Reply` when the branch has a pending ask to you, resolved fresh
      each keypress from `branch_infos` (`resolve_submit`) so it can
      never race a question that just arrived. Enter never does nothing
      — an empty input line simply doesn't submit, same as before.
- [x] Keys: `f` fork-here (the "ask without pausing" gesture — forks at
      the branch's current leaf), `F` fork-at-clicked event (the last
      chat row clicked, falling back to fork-here if none has been), `p`
      spawn-from-selection (arms an input mode for the charter, sends
      `Spawn { parent: selected }`), `x` interrupt. Restart keys: `v`
      resume-with-value (the typed text parsed as JSON, falling back to
      a bare string) and `e` rewrite (the typed text sent whole as
      `RunProgram { source }`) — both `Restart` on the selected branch.
      Eligibility is never pre-checked here (`Runner::eligible` already
      renders a self-sufficient refusal for a call that doesn't apply),
      so these five keys work from any view without asking the branch's
      state first.
      *(**Simplified from the box's "rewrite-from-editor":** the box
      named shelling out to `$EDITOR` on a temp file. That is a real,
      separable feature — terminal raw-mode suspend/resume, `$EDITOR`
      absence, temp-file cleanup — disproportionate to what pasting a
      rewrite needs day to day, and it would be the TUI's only gesture
      that doesn't go through the input line's existing mode system. `e`
      reuses that system instead: single-line rewrites work today: a
      real multi-line paste does too, since the input line does not
      strip newlines — only the terminal's paste path is what's
      untested. If transcripts show that binding cramped, promoting it to
      an editor shell-out is additive, not a rewrite of this one.)*
- [x] A running branch's chat shows the annotated source live (the same
      renderer as the report), so "how far along" never costs a turn.
      Built exactly as flagged: `report::annotate_calls(source, calls,
      settled)` is the pure core now, over `CallSite { site, id, is_send
      }`; `annotated_source` (a finished handback) and
      `report::annotate_program` (a live `ProgramView`, over
      `Tree::programs_for` — never a VM) both call it.
      `InvokeView` gained `site`/`is_send` so `programs_for`'s existing
      log-only reconstruction carries what annotation needs; nothing new
      touches the `Tree`. The Source pane calls `annotate_program` in
      Chat/Running (FullDebug keeps the raw IP-highlighted view — real
      instruction stepping wants the VM, not a call menu).
      `live_annotation_agrees_with_the_report_it_stands_in_for`
      (`machine.rs`) pins the invariant directly: the same two calls'
      annotation lines, byte-identical, from the live projection while
      the program is still running and from the settled report after.
- [x] Agent and branch references in chat (`agent 7`, an `agents()` row, a
      `spawn` result) are clickable and select that branch — the
      orchestrator can say "see the researcher" and you are there.
      *(Scoped to prose: a plain chat line naming `agent N`
      (`agent_reference_in`) selects that agent's own branch — its id
      **is** its first branch's id, so no lookup is needed. The `spawn`/
      `agents()` **rows** already have their own click behaviour, pinning
      an invoke's args/result in the side panel; making the same click
      also jump the selection would be one gesture doing two
      contradictory things, so those keep their existing meaning and only
      free-text mentions are addressed-navigable.)*
- [x] `attach.rs` tests cover the reply-vs-turn input mode
      (`reply_mode_wins_over_ask_or_tell_when_a_branch_is_waiting_on_you`,
      `alt_enter_is_the_tell_modifier`,
      `a_pending_ask_to_user_shows_above_the_input_as_reply_mode`) and the
      restart keys
      (`restart_keys_arm_an_explicit_mode_and_submit_the_right_command`,
      `fork_interrupt_and_jump_keys`, `next_waiting_cycles_and_wraps`,
      `agent_references_in_prose_are_recognized`).
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

## Part E — Docs sweep

- [x] 8_HARNESS: Step 1 vocabulary rewritten; decisions 2–4 amended
      (subagents are tools at the API and typed `Send`/`Spawn` in the log;
      results are
      JSON; fork adds a branch); "frame" → "agent"/"branch" throughout;
      the "user interruption" hole marked built; decision 7 gains
      reconciliation and re-attach.
      *(Decisions 2–4 and 7 amended in place with the specifics named
      above; Step 1's event list gains a "vocabulary rewritten" note
      pointing at the current primitives rather than being rewritten
      wholesale, since it's also the historical record of what Step 1
      literally shipped. The "frame" pass covers the decisions, Step 1's
      note, and the "user interruption" hole directly; Steps 0–6, Known
      holes' other bullets, and the M0–M4 milestone narratives are Phase
      8's own build log — real dates, real bugs found, literal
      then-current identifiers and test names — so rather than rewriting
      93 more historical citations with uncertain fidelity, a single note
      right after the decisions section translates the word for
      everything that follows: frame → agent (or branch, where the text
      is about the tree), `FrameStart` → `Agent`, `FrameResult` → the
      `Return`/`Condition`/`Answer` split, `AgentState` → `Runner`. Same
      policy applied to 9_TUI and 11_INTROSPECT below.)*
- [x] 12_ANSWERS: Step 2's stored truncation and retry superseded by B4;
      decision 4 rewritten as "budget is a rendering rule."
- [x] 9_TUI: decision 6 extended — branch navigator, waiting-on-you
      highlighting, the input line always live, restart keys; "frame
      list" pane renamed; "frame" kept only for the stack pane.
      *(Step 4's own heading and body — "frame switcher," "frame-list
      pane," `AgentState` — updated too, since unlike 8_HARNESS's
      milestones this is the step the extended decision 6 directly
      describes; a note marks what changed vs. what M0 originally
      shipped. The stale acceptance-box test name
      `concurrent_frames_list_and_retarget` is corrected to
      `concurrent_agents_list_and_retarget`, its actual current name
      after 17_BRANCHES Step D1.)*
- [x] 11_INTROSPECT / 10_EDITING: `FrameResult`, `Message::*`,
      `PushFrame`-era wording, and conversational "frame" updated.
      *(11_INTROSPECT gets a banner after "Sequencing" naming the three
      concrete corrections a reader needs before the decisions section:
      `Message::System` no longer exists as a stored chat event —
      17_BRANCHES A3 made `system` a field on the `Agent` event itself,
      closing this doc's own decision 4 gap; `FrameStart`/`FrameResult`
      are `Agent`/`Fork` roots and outcome events, since agents never
      close and so have no terminal event to log; `AgentState` is
      `Runner`. 10_EDITING is lighter (a handful of mentions, not a
      whole phase built on the word): decision 6 gets the same
      `Runner`/branch correction plus a note that 17_BRANCHES C1–C2
      built the reconciliation this decision only named; Step 7 — whose
      `synthesize_if_interrupted` and its test are gone from the code,
      superseded by `Tree::unmatched()`'s general reconciliation — gets
      a superseded note rather than a rewrite, since every specific in
      its body is now historical; D2's "per-frame epoch" becomes
      "per-agent.")*
- [x] DESIGN.md: A0 re-read against what landed; sentences using "the
      agent" for the product reworded to "the harness"/"the system", so
      "agent" means one context everywhere.
      *(The Exchanges section A0 added is unchanged by D1/D2 — they
      built a TUI over that vocabulary, not a vocabulary change. Fixed:
      goal 1's "**The agent.**" → "**The harness.**", "the agent
      product" → "the harness product" (×2), "the agent thesis" → "the
      harness thesis", "the *agent* side of this project" → "the
      *harness* side." Left alone: "code-mode agent system" in the
      opening line and every use of "agent"/"subagent" meaning one
      context — both already read the new way.)*
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

## Open questions (decide on contact)

- **Should an unawaited `Result` wake an idle branch?** Rule C says the
  harness posts a notice; whether that starts a turn immediately is a
  cost/UX call. Default to waking — an orchestrator should react to a
  finished worker.
- **Context growth, now per worker too.** Same open question as
  8_HARNESS, wider; design from multi-branch transcripts.
- **Does the model use `ask`, or route around it?** `agent()` stays the
  obvious path; watch whether follow-ups happen or models respawn. If
  they respawn, the card is wrong before the design is.
- **Should anything rename automatically?** Deliberately not built: the
  derived display label covers the nameless case without drift. If names
  still feel absent in practice, the honest form is a **one-shot** rename
  when a branch produces its first answer — logged as an ordinary
  `Rename`, visible, and overridable — never a name that keeps changing.
  Whether agents should rename themselves (`tools.rename`) is the same
  question one layer down; `spawn({ name })` already covers the moment
  that matters.
- **Eviction cap** — see "does not add."

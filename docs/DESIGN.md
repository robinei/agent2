# Design north star

A code-mode agent system: the LLM writes JS programs that orchestrate tool
calls; the programs run on a bespoke VM; and a Lisp-style condition system
makes the LLM (and above it, the user) the interactive restart handler. The
numbered plan files (`0_…` – `23_…`) are the roadmap; this file is the
rationale they all serve. Where a plan file and this file disagree, surface
it — that's a design change, not a detail.

This project now pursues **two first-class goals**:

1. **The harness.** The condition-system runtime — the original thesis and
   the product. Everything below about suspension, the event log, and "LLM
   as restart handler" serves this.
2. **JavaScript compatibility, as an end in itself.** As the VM has grown
   into a capable language runtime, JS compat is pursued for its own sake —
   a deliberate hobby-project decision (`15_COMPAT.md`), not only where a
   program demands it. It is fenced *only* by what the harness product
   actually needs: the two guardrails in "Compatibility as a terminal goal"
   below. **Determinism, once load-bearing here, is now a non-goal** — see
   the dependency spine.

## The thesis: everything that happens to a running program is the same event

The program suspends at a well-defined point; a report renders; some
authority picks a restart. Every interaction is one instance of that shape:

| Suspension | Report | Typical restart | Handler |
|---|---|---|---|
| `Invoke` (tool call) | the call(s) | deliver result(s) | host, automatically |
| `raise(name, payload)` | condition report | a handler program returning `resume(value)` or `abandon()` | LLM |
| trapped runtime error | rendered diagnostic + artifact menu | the same, or a rewrite | LLM |
| user interrupt / steering (17_BRANCHES) | condition report + user's message + the **annotated source** (every call site labelled with its artifact id and state) | a program — the user's, typed or synthesized | user → LLM |
| context-budget headroom (20_CODE_MODE Part E) | the history itself, every row by id and label | a compaction program: `remove_history` / `rewrite_history`, then `resume()` | LLM, then host policy |
| `OutOfFuel` / memory budget | report | top up / abort | host policy |
| crash / version mismatch | interruption + artifact menu | rewrite with artifacts | LLM |

Tool execution is not a separate mechanism from the condition system — it
is the most common condition, one with an automatic handler. The handler
*hierarchy* (host policy → LLM → user) is the Lisp condition system's
nesting with the debugger replaced by progressively smarter authorities.
In-program `try`/`catch` (6_LANGUAGE Part B) is the innermost layer of the
same hierarchy; `raise` deliberately bypasses it.

**"LLM as restart handler" is literal, not a metaphor.** The handler is not
a turn picking a restart off a menu — it is a *program the LLM writes*,
which runs while the signalling frame is still live and whose return value
**is** the restart. That is Lisp's actual semantics rather than an analogy
to it, and it is what forces the handler stack (20_CODE_MODE Part D): a
host-side structure of independently-stepped VMs, no VM ever on another
VM's stack, the load-bearing property intact.

## The load-bearing property

**Suspension with total state visibility at zero-cost safe points** — the
one thing a sandboxed real runtime cannot offer (kill the process, or wait;
never "stop between two instructions with the heap, log, await-chain, and
console buffer inspectable"). It exists here by construction: effects are
`StepResult` returns, never host callbacks, so the VM is never on anyone's
stack when a decision is needed. Everything else is downstream of this one
property: fuel is "interrupt on budget," a lost VM is "the run gets an
outcome and the next program starts fresh," stackless async is "suspension as a
value," steering is "interrupt with a restart menu." Protect this property
in every design decision; features that would require the VM to call back
into the host break the architecture.

Note this property is the **condition system's** requirement and is
**independent of determinism**. The VM may be freely nondeterministic
(`Date.now`, `Math.random`, unseeded iteration order) without weakening it:
suspension is about *where* control can stop and hand out, not about a
reproducible execution trace.

## The dependency spine

**Nothing is ever re-executed, and there is no path that could be.** The
VM is never serialized, so state that was only in it is gone when it is
gone — there is no replay to re-reach a suspension point, deterministic
or otherwise. Two cases, and only two:

- **The VM is still live** (a trapped error, an explicit `raise`). It
  stays suspended exactly where it stopped, and a handler program
  continues it: `resume(value)` injects a value in place of the failed
  operation or the raise expression, and the raising program carries on
  beneath it with every variable and completed step intact. `abandon()`
  discards it instead. This is the only continuation there is.
- **The VM is gone** (crash, version mismatch). The run gets an outcome
  like any other, and whatever happens next is a **new program**, written
  by a mind, with everything the dead run completed reachable as
  artifacts by id. Not a rerun of the old source — a new one.

Each layer's hard problem is solved by a property the layer below
guarantees — keep the directions intact:

1. The VM holds all in-flight state **in memory only** and is never
   persisted; when it dies, everything that lived only inside it dies with
   it, and no replay brings it back →
2. so completed work must live **outside** the VM — the append-only
   **event log** records every tool result as it lands →
3. so reuse is **explicit artifacts by event id** (`artifact(id)`), the
   next program fetching prior results rather than recomputing them. **This
   is exactly why determinism is unnecessary:** nothing retraces a previous
   control flow, so there is nothing for a reproducible trace to make line
   up. A fresh program written against the same log reaches the same
   completed work by naming its id. Two consequences the log must earn
   (17_BRANCHES): **after a resume, no completed work is invisible** — every
   half-finished exchange in the log is reconciled on open, so a call that
   landed is an artifact and a call that was merely issued says so; and
   **reuse by id covers in-flight exchanges too** — `artifact(id)` on
   a still-pending ask returns a promise that resolves when its result lands,
   so a new program written after a cut **awaits the existing exchange**
   rather than asking again →
4. so programs need **no durable `state`** — they are functions
   `(input, tools, artifacts) → returned JSON + effects` →
5. so the **condition report's artifact menu** is the complete restart
   interface →
6. which is what "LLM as restart handler" needed to be cheap: out of the
   loop on the happy path, entering exactly at decision points, with all
   completed work preserved in the log.

(This supersedes two earlier framings in turn. First **"deterministic
positional replay"**, in which determinism was the root of the spine —
`1. VM deterministic → 2. positional replay sound → …`. Then
**"recovery is re-execution"**, which dropped the determinism root but
still had the harness re-running a stored program to re-reach where it
had been. Neither survives: nothing re-runs at all, so the chain
re-anchors on the load-bearing
suspension property. The explicit-artifact layer (then item 5, now item 3)
was always the real reuse mechanism — "never an implicit args-matching
cache" — and it carries recovery on its own without determinism.)

**The program's vocabulary is split by who knows it.** The closed,
harness-defined set — `tell`/`ask`/`answer`/`spawn`/`fork`/
`append_history`/`artifact`, plus the decision constructors
`resume`/`abandon`, joining `raise` — is **bare-global** and known to the
compiler statically, the way `raise` already is. `tools.*` remains the
surface for a *specific agent's configured capabilities* (`read_file`,
`bash`, whatever the registry holds), which varies per agent and which
the compiler has no static view of. The line is language versus library.

Parallel spine for concurrency: it lives in the **program layer**
(promises + outbox, 7_ASYNC), so the conversation tree never needs a
concurrency mechanism — subagents are tools, transcripts are branches,
the tree just allows multiple active leaves.

## Exchanges

A conversation is a tree, and every message in it is an event on a path
through that tree. The vocabulary (`17_BRANCHES.md`), fixed here because
five earlier plan files used one word — "frame" — for four things:

- **Agent** — a clean-room context with a charter: the thing you spawn,
  ask, and list. Identified by its `Agent` event id (`AgentId`).
- **Branch** — an addressable conversation: one path from a root to a
  leaf. An agent has one branch until someone forks it; then it has two,
  both live, both its own. Identified by its **root event**
  (`BranchId = EventId`) — the `Agent` for an agent's first branch, a
  `Fork` for a divergent one.
- **Spine** — the code's handle for a branch's path: leaf id plus the
  reconstructed chain of contexts. Internal.
- **Context** — the reconstructed conversation of one agent along a
  spine: charter, system prompt, posts, turns, open questions. What is
  rendered into an LLM request.
- **Frame** — reserved for the **VM call stack** (`CallFrame`,
  `VM::frames()`, the debugger's stack pane) and nothing else.

**Branch ids are root event ids, so concurrency leaves the log
unchanged.** Nothing session-local is minted and nothing about "which
branches are live right now" is written down: live-ness is session state,
identity is in the log. Reopening a log re-derives the set of branches
from the events it already holds.

### Three rules

**A. The branch is the address.** A post is logged on the branch it is
delivered into; a record is logged on the branch whose state it changes.
There is no routing table — the tree *is* the routing.

**B. A post is logged on arrival and delivered at the recipient's next
safe point — and every fuel-slice boundary is a safe point.** Nothing
anyone says is rejected, queued invisibly, or lost to a crash: it is
visible immediately, and it reaches the LLM at the next slice (a running
program suspends into a condition whose report is the message), beside a
pending report (suspended), or when the current generation lands
(thinking). This is the load-bearing suspension property spent on
responsiveness, and it is why upward questions cannot deadlock: a parent
awaiting its child is one slice from being told.

**C. Waiting is a property of the awaiting program, never of the
message.** A value someone's program awaits arrives as a `Result` and
resolves the promise — machine-bound, never entering a context. A message
nobody's program awaits arrives as a `Post` — mind-bound, delivered into
the context because otherwise no one would see it. This is "the one
exception" above restated as a mechanism: **data crosses into a mind
exactly when no program is waiting to receive it.**

### The four-event exchange

One question and its answer are four events, two on each side, each side
reconstructible from its own path:

| | asker's branch | answerer's branch |
|---|---|---|
| the question | `Send { to, text, input, expects_reply, site }` | `Post { from, origin: Sent(send) }` |
| the answer | `Result { call: send, outcome }` | `Answer { question: post, value }` |

The four form a closed loop of ids — `Post.origin → Send`,
`Result.call → Send`, `Answer.question → Post` — so from any one the
other three are one lookup away. That loop is what reconciliation walks
after a crash, how a renderer resolves a body, and how an answer finds
the branch that asked. **A body is stored once**: the `Send` holds the
question and the `Post` names it; the `Answer` holds the value and the
`Result` names it. What a delivery-side event contributes is *position* —
that this message landed here, in this branch, at this point.

Everything else is the same table with a column blanked: a host tool call
is the left column only (`Invoke` … `Result`); a `tell` blanks the
`Answer`; and the user blanks both program columns.

**A bare turn answers nothing.** `open` — the posts a branch has not yet
closed — is discharged only by `answer(question, value)`; a plain reply
with no tool call is text, full stop, and a branch may sit idle while
still owing one (`18_TARGETING.md`). The reason is the same one C3 used
to refuse a silent decline: an implicit answer and an oblivious one
produce identical bytes, so nothing short of the explicit call can be
trusted to mean "this is the answer."

### The user is an author, not an agent

The user has no branch. They speak *inside* branches: an utterance in
branch X is a `Post { from: User }` on X, X's reply is X's own answer,
and the user reads it there — **borrowing the context of whichever branch
they are in**, which is exactly the experience of holding a different
pseudo-identity in each conversation.

**Forking is why this must be so.** The user's post sits in the shared
prefix of two forks, and each fork answers it in its own branch. A single
global user branch could not represent "you-in-fork-A" and
"you-in-fork-B" as different participants, and it would owe two results
for one call. So the user is a blanked column in the exchange table: no
program to `Send` with, no context to `Post` into. The same reading makes
the user the outermost restart handler literal rather than metaphorical —
they take a branch's turn directly, supplying a value, a rewrite, or an
answer without spending an LLM turn. **Their default send is a tell, not
an ask** — a deliberate gesture, not the ordinary case — because a human
who gets no reply can just ask again, and a suspended program cannot.

## No exception: nothing enters a context unchosen

The spine keeps the LLM out of the *data* path — tool output lives in
variables and the log, reachable by id, never re-sent in context. Under
code mode that invariant has **no exception**, where it once had one.

The retired exception was the **answer**: a program's `return` rendered
into a completion report, and a subagent's final turn rendered to its
caller, budgeted generously so an ordinary read or summary landed in one
shot. There is now no answer that crosses. A root program returns nothing
to anyone — it reaches the user through `tell()`. A handler's value goes
to the raising *program*, as a value, not into a context. What enters a
history is exactly two things: **what nobody was waiting for** (an
arriving post), and **what a mind explicitly appended**
(`append_history`). The answer-budget machinery — `DEFAULT_ANSWER_BUDGET`,
the answer clipping in `report.rs` — has no subject under this design and
goes with it (23_ONE_AGENT, Pass B).

So the rule is the whole rule: **machine-bound data travels by reference
and never enters a context; mind-bound data is what a mind chose to put
there.** Sharpened one level down (22_ONE_VOCABULARY): *ids are for data a
program consumes; inline text is for data a mind consumes.* `artifact(id)`
can never feed a judgment in the same turn — a program can fetch bytes and
branch on them mechanically, but an actual judgment call requires a
completion, and a completion sees only what was rendered into the
document.

Two earlier positions this supersedes, in order. First, **"`return` only
small, status-shaped values"** — right about orchestration data, wrong
about deliverables, and it forced read/summarize tasks to smuggle content
through `/tmp` chunking (12_ANSWERS). Then **the budgeted answer** above,
which fixed that by opening one deliberate channel. Code mode dissolves
the problem instead of channelling it: a program that reads a file holds
the content in a variable and acts on it, with no turn boundary to carry
it across, so the deliverable never needed to enter a context in the first
place.

The test for any mechanism on this path is unchanged, and it is the reason
this direction is the right one: if the model has to *know a mechanism
exists* to get the obvious task right, that is a smell, not a feature.

## Compatibility as a terminal goal

JS compatibility is now pursued for its own sake (`15_COMPAT.md`), measured
against a conformance corpus rather than admitted feature-by-feature on
program need. The bar for a compat feature is simply "real JS does it and
it clears the two guardrails below." Everything else in JS is fair game,
including everything that was once a `4_FUTURE` non-goal (mutable
prototypes, descriptors, getters/setters, `Symbol`, iterators,
`ToPrimitive`, `Proxy`).

The fence is short, and it is exactly what the *harness* product needs:

1. **No host callback mid-instruction.** A feature that would make the VM
   call back into the host between two instructions breaks the suspension
   property and is rejected regardless of spec fidelity. (Nothing in the
   prototype/descriptor/iterator/symbol surface needs this.)
2. **The JSON boundary is invariant.** Prototypes, descriptors, methods,
   symbols, bound functions — every reflective artifact has **no JSON
   form**, exactly as `Closure`/`Promise`/`RegExp` already do.

**Determinism is not on the fence** — it is a non-goal (see the spine). A
nondeterministic builtin is freely compatible: nothing ever retraces a
previous execution, so there is no trace for it to make diverge. This
*shrinks* what compat must honor rather than enlarging it.

## The model-facing surface is chosen by behaviour, not by symmetry

**Every name, shape and spelling the model sees is decided by what it
acts best on. Nothing about it is decided by how this codebase reads.**
The model never sees `types.rs`. An internal name matching a card verb
is worth exactly nothing to the only reader that matters, and reaching
for that symmetry is how a surface drifts away from the thing it is
supposed to be tuned against.

This is easy to violate while sounding principled, so the tell is
concrete: **if an argument for a name cites an identifier in this
repository, it is not an argument.** Three were made in one sitting —
that a verb should be `handover()` because `Disposition::Handover`
exists, that it should be `next_turn()` because `Message::Turn` does,
that `say` should become `tell` because `Call::Send`'s doc comment
already said "tell". All three may reach a defensible answer; none of
them is a reason.

The arguments that do count are about the reader's priors and its
observed behaviour:

- `exec(payload)` was rejected because a model's dominant prior for
  `exec(x)` is **run x as code** (Python, shell, `child_process`), not
  POSIX's replace-this-process — a name whose common meaning is
  actively wrong is worse than a vague one.
- `handover(payload)` was rejected because "hand over to whom?" reads
  as **delegation**, which is `spawn`/`fork` — a different mechanism on
  a different branch.
- `next_turn(payload)` was rejected because "turn" in the wider
  literature usually means a whole user-request-to-answer cycle with
  tool calls inside it, so the name suggests **ending the exchange**.
  That it is exactly right *inside this codebase*, where `Message::Turn`
  is one assistant message and roles strictly alternate, is precisely
  the irrelevant kind of correctness.

And behaviour outranks reasoning about behaviour. The card was wrong
about `//:` for months on an argument that sounded good; two exemplars
shipped traps because they were written from a doc rather than from
what the model does with them. When a live run disagrees with a
well-argued surface, the run is right.

## The UI boundary, and the one carve-out

**Every consumer talks to the session loop through the same
serializable pair** — `SessionCommand` in, `SessionEvent` out
(`host/protocol.rs`). The CLI, the TUI's chat pane, and any future
remote client over a socket are the same consumer; the loop has no
second entrance. This is what makes server mode a thing the code
already supports rather than a rewrite waiting to happen, and it is
worth defending on purpose, because it is the kind of property that
erodes one convenient direct call at a time.

**One carve-out**: the attached debugger borrows the `Tree` and the
live `Runner`s directly (`Session::tree`, `Session::state`) rather than
going through the protocol, because encoding VM internals into the
event vocabulary would have meant a protocol that grows every time the
debugger wants to show something new. It renders on the loop thread,
so the borrow is sound.

**That carve-out has been shrinking, and should keep shrinking.**
Derived-not-stored moved most of what once needed a live VM into the
log: `ProgramView` — source, invokes, console, result, outcome, depth —
is a fold over events (`tree::programs_for`); `ProgramStatus` is
explicit that it is "no longer live-only"; reports are folds over
`Return`/`Condition` rather than rows handed to a renderer. What still
genuinely needs the VM is live introspection *during* a run — variable
state, the current stack, fuel — and nothing post-hoc. **A new use of
the carve-out is a smell**: check first whether the log already answers
it, because it usually does now.

**The conformance witness is `--headless`**, and that is its second
job. It prints `SessionEvent`s off the channel and never reaches for
`tree()` or `state()`, so it demonstrates in product code that the
protocol is sufficient to follow a session. Keep it that way: the day
headless needs privileged access is the day the protocol has a hole,
and it should be fixed in the protocol rather than by reaching around
it. This is why a *separate* client written to prove the boundary is
not needed — it would be a second thing to keep honest, testing the
boundary instead of the product.

**Known gap for a remote client.** Because reports are derived rather
than stored, a thin client receiving `Event`s cannot render one without
`report::derive_report` and `document::render`. Either it ships the
same fold logic, or the server sends rendered rows. That is a real
decision for whoever builds the socket layer, not an oversight — and
better known now than discovered halfway in.

## Confinement, not permission

**The harness does not gate the call, and does not confine the process
either — it is *run inside* a confinement chosen by whoever runs it.**
An agent's tools are unrestricted: no allowlist of permitted commands,
no per-call confirmation prompt, no "this looks dangerous"
interstitial, no `effectful` flag on a tool definition. And no
sandboxing code — the harness contains no `bwrap` invocation, no check
for whether it is confined, and no refusal to start when it isn't.
Whoever runs an agent decides what it can reach, by the ordinary means
their operating system already provides.

This is a single decision with a lot of consequences, so it is worth
stating why rather than only what.

**Permission gating does not survive code mode.** The unit a
permission prompt is built for is one tool call, reviewed before it
runs. Here the unit is a *program*: a hundred statements, branching on
results the reviewer cannot see yet, whose interesting calls are
constructed at runtime. Asking a human to approve that call-by-call
either interrupts constantly — reintroducing exactly the per-step round
trip this whole design exists to remove — or degrades into approving
the program wholesale, which is confinement with extra steps and worse
ergonomics.

**It is also the honest boundary.** A call-site check asks "is this
command dangerous?", which is undecidable in general and a guess in
practice. A process boundary asks "what can this process reach?",
which is a fact. The blast radius is a property of the sandbox, not of
anyone's judgment about a string.

**And it keeps the harness out of the model's decisions.** `8_HARNESS`
once carried an `effectful` flag and a warning rendered into the
report; both were removed, and the test `menu_has_no_effectful_warning`
exists to keep them removed. Confinement is the generalisation of that
removal: the harness decides *where* a program may act, and the model
decides *what to do there* — including, as `22_ONE_VOCABULARY.md`'s
"when to ask" says, stopping to ask a person before something
irreversible. That judgment stays in the program, where it can be
measured, rather than in a dialog the harness raises on the model's
behalf.

The eval inherits this directly, and the inheritance is the point:
tasks run real tools against real files inside a real sandbox, so the
only thing it simulates is the **absent human**. A fixture that fakes
a tool cannot represent two semantically different commands, and has
already produced a false *proceed* — a query returning unexpected text
parsed as zero rows, skipping a safety gate the model had written for
itself. A sandbox cannot lie that way.

So the eval is *launched* confined rather than confining itself —
a script that runs it under `bwrap`, which is the only place the
sandbox is named. The obligation that does fall on the task set is to
need **nothing exotic**: a shell, coreutils, and real files. A task
requiring a database daemon is one that gets run outside the sandbox
eventually, whatever the script says.

## Product surface

The condition report (8_HARNESS Step 4) is where the harness thesis succeeds
or fails — it is a prompt-engineering artifact with golden-render tests, not
an error string. Its quality, and the M5 eval (conditions vs. plain tool
loop vs. atomic code mode under injected failures), are how the *harness*
side of this project is judged; the **conformance corpus** (`15_COMPAT.md`)
is how the *language* side is judged. The debugger TUI (9_TUI) is the
observation instrument for both: attached mode *is* the harness frontend,
and the report iterates against live transcripts watched there.

### How M5 gets run, so it stops being deferred

M5 has been named and postponed repeatedly. It is the measurement most
likely to be unflattering, which is exactly why the method belongs here
in advance rather than being designed at the moment it is needed.

**The baseline is a third-party coding agent on the same model**, not a
tool-calling loop written here. Building our own control invites
building a weak one, and neither the author nor the reader could tell
whether that had happened. Same weights, same provider, same tasks, an
implementation nobody here is invested in defending.

**The tasks must not all be code-mode-shaped.** The current five were
designed for this harness, and `fan-out` — three parallel reads — is a
loop in one program and three round trips in a tool loop. That is a
real advantage and the most flattering possible framing of it. A fair
comparison needs tasks weighted the other way: heavy on judgement
*between* steps, where a tool loop's per-step reasoning is an asset
rather than a tax. If code mode still wins there, the number means
something.

**Those tasks double as the held-out set.** The card has been tuned
against the fixed five while watching their pass rate, which is
overfitting however careful the checks are — the discipline that a
check never gates on which verb fired does not protect against
rewriting card prose while watching a task shaped for that prose.
Tasks authored for the comparison are written to a different criterion
and never used for tuning, which is what makes them a real holdout.

**Report round trips per task and task success, per agent.** Not
program length — that is not a thing the baseline has, and a metric
only one side can post is not a comparison.

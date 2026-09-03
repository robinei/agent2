# Design north star

A code-mode agent system: the LLM writes JS programs that orchestrate tool
calls; the programs run on a bespoke VM; and a Lisp-style condition system
makes the LLM (and above it, the user) the interactive restart handler. The
numbered plan files (`0_…` – `17_…`) are the roadmap; this file is the
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
| `raise(name, payload)` | condition report | resume(value) / rewrite | LLM |
| trapped runtime error | rendered diagnostic + artifact menu | resume(value) / rewrite | LLM |
| user interrupt / steering (17_BRANCHES) | condition report + user's message + the **annotated source** (every call site labelled with its artifact id and state) | answer / resume / rewrite | user → LLM |
| `OutOfFuel` / memory budget | report | top up / abort | host policy |
| crash / version mismatch | interruption + artifact menu | rewrite with artifacts | LLM |

Tool execution is not a separate mechanism from the condition system — it
is the most common condition, one with an automatic handler. The handler
*hierarchy* (host policy → LLM → user) is the Lisp condition system's
nesting with the debugger replaced by progressively smarter authorities.
In-program `try`/`catch` (6_LANGUAGE Part B) is the innermost layer of the
same hierarchy; `raise` deliberately bypasses it.

## The load-bearing property

**Suspension with total state visibility at zero-cost safe points** — the
one thing a sandboxed real runtime cannot offer (kill the process, or wait;
never "stop between two instructions with the heap, log, await-chain, and
console buffer inspectable"). It exists here by construction: effects are
`StepResult` returns, never host callbacks, so the VM is never on anyone's
stack when a decision is needed. Everything else is downstream of this one
property: fuel is "interrupt on budget," crash recovery is "re-reach the
suspension point" by re-execution, stackless async is "suspension as a
value," steering is "interrupt with a restart menu." Protect this property
in every design decision; features that would require the VM to call back
into the host break the architecture.

Note this property is the **condition system's** requirement and is
**independent of determinism**. The VM may be freely nondeterministic
(`Date.now`, `Math.random`, unseeded iteration order) without weakening it:
suspension is about *where* control can stop and hand out, not about a
reproducible execution trace.

## The dependency spine

Recovery does **not** restore VM state — it **re-executes**. The VM is
never serialized; on a crash, version mismatch, or resume, the program is
rerun to re-reach its suspension point (and is often *rewritten* first by
the LLM, with prior results already in hand). Each layer's hard problem is
solved by a property the layer below guarantees — keep the directions
intact:

1. The VM holds all in-flight state **in memory only** and is never
   persisted; recovery re-reaches the suspension point by **re-execution**
   (well-defined precisely because of the load-bearing suspension property
   above) →
2. so completed work must live **outside** the VM — the append-only
   **event log** records every tool result as it lands →
3. so reuse is **explicit artifacts by event id** (`tools.tool_result(id)`),
   the program re-fetching prior results rather than recomputing them. **This
   is exactly why determinism is unnecessary:** reuse is keyed by an explicit
   id, not by a rerun retracing the original control flow position-for-
   position — so a nondeterministic rerun, or an LLM-rewritten program, still
   reuses the right completed work. Two consequences the log must earn
   (17_BRANCHES): **after a resume, no completed work is invisible** — every
   half-finished exchange in the log is reconciled on open, so a call that
   landed is an artifact and a call that was merely issued says so; and
   **reuse by id covers in-flight exchanges too** — `tools.tool_result(id)` on
   a still-pending ask returns a promise that resolves when its result lands,
   so a re-entered or rewritten program **re-awaits** rather than re-asks →
4. so programs need **no durable `state`** — they are functions
   `(input, tools, artifacts) → returned JSON + effects` →
5. so the **condition report's artifact menu** is the complete restart
   interface →
6. which is what "LLM as restart handler" needed to be cheap: out of the
   loop on the happy path, re-entering exactly at decision points, with all
   completed work preserved in the log.

(This supersedes the earlier **"deterministic positional replay"** framing,
in which determinism was the root of the spine — `1. VM deterministic →
2. positional replay sound → …`. Recovery is now re-execution plus
explicit artifact reuse, **not** a deterministic retrace, so the
determinism root is dropped and the chain re-anchors on the load-bearing
suspension property. The explicit-artifact layer (then item 5, now item 3)
was always the real reuse mechanism — "never an implicit args-matching
cache" — and it carries recovery on its own without determinism.)

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
answer without spending an LLM turn.

## The one exception: the answer crosses into context

The spine keeps the LLM out of the *data* path — tool output lives in
variables and the log, reachable by id, never re-sent in context. That
invariant holds for everything except the one value that is the LLM's own
deliverable: the **answer** a frame was asked to produce. Reading and
summarizing are not orchestration; their product *is* data the model must
take into its head and re-author. So the answer — a program's `return`
rendered into the completion report, and a subagent's final turn rendered
to its caller — is the sole channel by which bytes deliberately enter a
context. It is **budgeted, not clipped to a token**: generous enough that
an ordinary file read or summary lands in one shot, with the full value
always kept as a fetchable artifact and only the context copy truncated
(naming its id) past the budget.

This supersedes the earlier "`return` only small, status-shaped values"
discipline, which was right about orchestration data and wrong about
deliverables — it forced read/summarize tasks to smuggle content through
`/tmp` chunking (12_ANSWERS). The rule that replaces it: **machine-bound
data travels by reference and never enters a context; mind-bound data is
exactly the answer, and its budget rides in the request.** A model never
opts into this — the obvious path (return what you want to read; write a
large product with `create_file`) *is* the correct one, and the budget
only fails safe when an answer is genuinely oversized. The test for any
mechanism on this path: if the model has to *know it exists* to get the
obvious task right, that is a smell, not a feature.

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
nondeterministic builtin is freely compatible; recovery by re-execution +
explicit artifact reuse does not depend on a reproducible trace. This
*shrinks* what compat must honor rather than enlarging it.

## Product surface

The condition report (8_HARNESS Step 4) is where the harness thesis succeeds
or fails — it is a prompt-engineering artifact with golden-render tests, not
an error string. Its quality, and the M5 eval (conditions vs. plain tool
loop vs. atomic code mode under injected failures), are how the *harness*
side of this project is judged; the **conformance corpus** (`15_COMPAT.md`)
is how the *language* side is judged. The debugger TUI (9_TUI) is the
observation instrument for both: attached mode *is* the harness frontend,
and the report iterates against live transcripts watched there.

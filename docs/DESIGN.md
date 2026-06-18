# Design north star

A code-mode agent system: the LLM writes JS programs that orchestrate tool
calls; the programs run on a bespoke VM; and a Lisp-style condition system
makes the LLM (and above it, the user) the interactive restart handler. The
numbered plan files (`0_…` – `15_…`) are the roadmap; this file is the
rationale they all serve. Where a plan file and this file disagree, surface
it — that's a design change, not a detail.

This project now pursues **two first-class goals**:

1. **The agent.** The condition-system runtime — the original thesis and
   the product. Everything below about suspension, the event log, and "LLM
   as restart handler" serves this.
2. **JavaScript compatibility, as an end in itself.** As the VM has grown
   into a capable language runtime, JS compat is pursued for its own sake —
   a deliberate hobby-project decision (`15_COMPAT.md`), not only where a
   program demands it. It is fenced *only* by what the agent product
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
| user interrupt / steering | condition report + user's message | resume / rewrite | user → LLM |
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
   reuses the right completed work →
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

The fence is short, and it is exactly what the *agent* product needs:

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

The condition report (8_HARNESS Step 4) is where the agent thesis succeeds
or fails — it is a prompt-engineering artifact with golden-render tests, not
an error string. Its quality, and the M5 eval (conditions vs. plain tool
loop vs. atomic code mode under injected failures), are how the *agent*
side of this project is judged; the **conformance corpus** (`15_COMPAT.md`)
is how the *language* side is judged. The debugger TUI (9_TUI) is the
observation instrument for both: attached mode *is* the harness frontend,
and the report iterates against live transcripts watched there.

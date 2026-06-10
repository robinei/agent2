# Design north star

A code-mode agent system: the LLM writes JS programs that orchestrate tool
calls; the programs run on a bespoke deterministic VM; and a Lisp-style
condition system makes the LLM (and above it, the user) the interactive
restart handler. The numbered plan files (`0_…` – `8_…`) are the roadmap;
this file is the rationale they all serve. Where a plan file and this file
disagree, surface it — that's a design change, not a detail.

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
suspension point," stackless async is "suspension as a value," steering is
"interrupt with a restart menu." Protect this property in every design
decision; features that would require the VM to call back into the host
break the architecture.

## The dependency spine

Each layer's hard problem is solved by a property the layer below
guarantees — keep the directions intact:

1. The VM is **deterministic by construction** (fuel-bounded, no ambient
   I/O, logged resolution order) →
2. so **positional replay** is sound (crash recovery without VM
   serialization) →
3. so the append-only **event log preserves all completed work** →
4. so programs need **no durable `state`** — they are functions
   `(input, tools, artifacts) → returned JSON + effects` →
5. so reuse is **explicit artifacts by event id** (`tools.tool_result`),
   never an implicit args-matching cache →
6. so the **condition report's artifact menu** is the complete restart
   interface →
7. which is what "LLM as restart handler" needed to be cheap: out of the
   loop on the happy path, re-entering exactly at decision points, with
   all completed work preserved.

Parallel spine for concurrency: it lives in the **program layer**
(promises + outbox, 7_ASYNC), so the conversation tree never needs a
concurrency mechanism — subagents are tools, transcripts are branches,
the tree just allows multiple active leaves.

## Product surface

The condition report (8_HARNESS Step 4) is where the thesis succeeds or
fails — it is a prompt-engineering artifact with golden-render tests, not
an error string. Its quality, and the M5 eval (conditions vs. plain tool
loop vs. atomic code mode under injected failures), are how this project
is judged.

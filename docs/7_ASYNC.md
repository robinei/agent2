# Phase 7 — Async/await

> **Status: COMPLETE.** Tier 1 landed 2026-06-11 (`async:` commits), Tier 2
> landed 2026-06-12, after 6B/6B2 (try/catch/finally) per the sequencing
> decision below — continuation records were built once against the final
> frame shape, handler-stack entries designed in. `Promise.allSettled`
> (gated on 6B, per the Tier 1 section) landed 2026-06-12 as the
> `__allSettled` prelude helper. See `compiler/tests/async_await.rs`
> (Tier 2 section) for the executable contract. Deliberate additions
> beyond this plan:
>
> - `new Promise(...)` gets a targeted compile diagnostic (commitment 4
>   made visible to the LLM), and `Promise.resolve`/`reject` likewise.
> - **Promise adoption**: an async function that returns a promise (after
>   suspending) chains it, as in JS — a promise never observably resolves
>   to a promise. Implemented at the two consumption sites (`Await`
>   follows resolved-to-promise chains in place; the scheduler re-waits a
>   continuation woken with a promise payload), not at the resolution
>   site. A self-cycle is the JS "chaining cycle" TypeError.
> - A rejection delivered to a continuation with no handler around its
>   await propagates to that call's own promise without materializing the
>   frame at all.
> - The await-chain diagnostic renders in the `Deadlock` error message
>   (the one place a suspended chain produces an error with no stack).
>
> The remaining banner content below is kept for the design rationale;
> the section bodies match what was built.
>
> **Sequencing decision: 6B (try/catch) lands BEFORE any Tier 2 work.**
> 6B is unblocked (Phase 3 done), immediately lets programs catch rejected
> awaits locally instead of escalating to the host, unlocks
> `Promise.allSettled`, and means Tier 2's continuation records would be
> built once against the final frame shape (handler-stack entries
> designed in, not retrofitted). When 6B lands, extend the `Await`
> rejected arm: dispatch to an active handler if one covers the frame,
> else escalate via the Phase 3 path as today; update the resume-audit
> table row accordingly. *(Done: the rejected arm dispatches to a
> reachable handler, rejects the enclosing strand's promise inside a
> resumed continuation, and escalates resumably only at the root.)*

Real async/await, replacing both today's synchronous `Invoke` batching and
the "transparent await" stopgap (4_FUTURE item 1, superseded by this file).
Two tiers: Tier 1 needs no per-task execution state and delivers flat
fan-out; Tier 2 adds green-thread strands for interleaved async-function
chains, reusing everything Tier 1 builds.

**Sequencing:** after Phase 3 (rejection paths consume enriched errors and
the resume machinery). Better with 6B (try/catch) but not blocked on it —
without try/catch, a rejected `await` escalates through the Phase 3 error
path as a resumable error (the LLM may substitute a value). Tier 2 strictly
after Tier 1.

## Design commitments

1. **Tool calls return promises; creation does not yield.** `tools.f(args)`
   allocates a `Pending` entry in a new `promises` heap, appends
   `(PromiseId, InvokeCall)` to a VM-side **outbox**, pushes
   `Value::Promise(id)`, and continues. The host is involved only when the
   program awaits a still-pending promise; that yield carries the whole
   accumulated outbox. This replaces the consecutive-`Invoke` batching (and
   `StepResult::Invoke`) entirely — fan-out now composes across arbitrary
   control flow (`urls.map(u => tools.fetch(u))`), not just adjacent
   instructions. Delete the old batching machinery (`invoke_at`, the
   gather loop) when Tier 1 lands.
2. **`Await` is a re-executing instruction (no continuation capture for the
   main strand).** Resolved → push value, advance. Rejected → throw (6B) or
   escalate via the Phase 3 path (`PushValueThenContinue`-resumable).
   Pending → `StepResult::Pending { calls: <drained outbox> }` with `ip`
   unchanged (`RetrySameInstr` shape); host calls
   `vm.resolve_promise(id, value)` / `vm.reject_promise(id, errval)` for at
   least one promise, then `step()`; the `Await` re-executes.
3. **Cascading is VM-side.** Promises carry waiter lists; resolution wakes
   waiters inside the VM. The host only ever resolves **leaf tool
   promises** — it never sees the program's dependency graph.
4. **No `new Promise`.** Promises originate only from tool calls and (Tier
   2) async function calls. No executor pattern → no user-constructed
   never-resolving promises; the only deadlock source is Tier 2 circular
   awaits, which is detected (see below).
5. **Promises are transient values.** No JSON form (error at the
   persistence boundary, exactly like `Fn`/`Closure`); `typeof` is
   "object"; identity comparison only. Property access on a promise is a
   TypeError whose message includes a "did you forget `await`?" hint — the
   misuse LLMs will actually commit under this dialect.
6. **Determinism for replay.** Scheduling (Tier 2) is a deterministic FIFO
   ready queue, so the only nondeterminism source is the host's resolution
   order. The host must record promise resolution order in its event log
   and re-feed the same order on replay/restart — with shared mutable
   state, interleaving is observable. Note this in the host-side design doc
   (4_FUTURE item 3).

## Tier 1 — promises + outbox + blocking await

No per-task state. Components:

- `promises: Vec<PromiseState>` heap;
  `enum PromiseState { Pending { waiters: Vec<TaskId> }, Resolved(Value), Rejected(Value) }`
  (waiters unused until Tier 2; keep the field from the start).
- `Value::Promise(PromisePtr)` variant; outbox `Vec<(PromisePtr, InvokeCall)>`.
- `Instr::Invoke` semantics change to allocate-and-record (no yield);
  `Instr::Await` per commitment 2. Compile `await x` to `Await`; `await` of
  a non-promise value pushes it unchanged (JS-faithful enough; document).
- Top-level `await` is the primary pattern (the program is the main task);
  verify oxc accepts it under the module source type.
- `async` on function declarations/expressions/arrows is accepted and
  ignored in Tier 1 (body runs synchronously on the caller's stack; an
  `await` inside blocks the program — document as the Tier 1 limitation
  that Tier 2 removes).
- Prelude: `__all(ps)` (serial awaits over already-started promises = full
  fan-out concurrency, zero VM machinery), lowering `Promise.all(x)`.
  `Promise.allSettled` follows once 6B lands (needs try/catch in the
  helper). `Promise.race`/`any` need wait-any VM support — defer, reject
  with a clear diagnostic.
- Program end with unresolved promises: the main strand returning resolves
  the program — outstanding tool calls are reported to the host in the
  final `Done` (host decides cancel/ignore). Define and test.
- Update: divergence list, COMPILER_PLAN banner note, and 4_FUTURE item 1
  (superseded — transparent-await stopgap no longer needed; do not
  implement both).

Tests: fan-out via map (one `Pending` yield carrying N calls), out-of-order
host resolution, await-after-resolve (no yield), rejected promise →
escalation/resume, promise in a returned/JSON-bound value → error, property
access hint, fire-and-forget call still executes.

## Tier 2 — stackless continuations (interleaved async functions)

**Decided: stackless, not green threads.** The VM keeps its single
`stack`/`callstack`/`ip`/`fp` untouched; suspended async calls live entirely
in the heap. (The stackful alternative — per-task stacks — was considered
and rejected: it complicates the runtime's execution state, costs idle
stacks per in-flight call, and is far less friendly to snapshot/
introspection. This VM's pattern is analysis + codegen contract over a dumb
runtime; stackless follows it.)

The load-bearing invariant: **a suspended computation occupies zero stack.**
Established at runtime by **frame snapshotting** — NOT by a compile-time
state-machine/linearization transform. (Textbook stackless — ANF
linearization + hoisting locals into heap cells — exists because native
code cannot treat a stack frame as a copyable value. This VM can: a frame
is a slice of `Vec<Value>` whose elements are cheap clones, plus a small
`CallFrame`. Snapshotting moves ~all of the work out of the compiler; the
linearization approach was considered and rejected as the largest, most
regression-prone compiler feature in the codebase for no semantic gain.)

Soundness rests on one parser-guaranteed fact: `await` is syntactically
confined to async function bodies (and top level), so suspension only ever
happens in the **immediate** async frame — callers received their promise
and moved on. Exactly one frame ever needs saving.

1. **Suspend = early-return with a stash.** At a pending await in an async
   frame: copy the frame's stack region (args, locals, temps — whatever is
   live) plus the `CallFrame` metadata (`arguments_cache`, and any 6B
   handler-stack entries belonging to this frame) into a **continuation
   record** `(resume_ip, saved_region, frame_meta, await_span)`; register
   it as a waiter on the promise; pop the frame, reusing the `Return`
   machinery. On **first** suspension (frame entered by direct call), push
   a fresh promise onto the caller's stack as the return value — the
   caller, sync or async, just continues. On a **re-suspension** (frame
   entered by scheduler resume), fall through to the scheduler.
2. **Resume = re-push and jump.** Push the saved region at the current
   stack top, recompute `fp` (everything is fp-relative — `Local`/`Pick`/
   `Dig` survive relocation; no instruction stores absolute stack
   addresses), re-base and re-push any saved handler entries, push the
   resolved value, jump to `resume_ip`. No `EnterFrame` runs on resume.
   A rejected promise instead dispatches to the re-based handler (6B) or
   rejects this frame's own promise (propagation).
3. **Completion modes on the frame.** A resumed frame has no caller below
   it: `CallFrame` gains `completion: Normal | ResolvePromise(PromiseId)`.
   `Return` in `ResolvePromise` mode resolves the promise with the return
   value (waking waiters) and falls through to the scheduler; an uncaught
   throw rejects it.

The **root strand** parks in place: top-level await leaves the root region
(locals + temps) where it is, and ready continuations execute above it —
the invariant guarantees they leave nothing behind, so the root's `Await`
re-executes against an intact region.

Accepted divergences (document in the divergence list): an async function
that completes without ever suspending returns its plain value, not a
wrapped promise (`await` passes non-promises through and `await` is the
only promise consumer in this dialect — observationally invisible,
including to the `__all` prelude); a throw before the first suspension
propagates synchronously to the caller instead of rejecting.

Resulting work split: compiler ≈ accept `async` flags + emit `Await` +
prelude/method-table rows (small); analyzer untouched (existing capture
analysis already boxes exactly what closures share); optimizer ≈ classify
`Await` as an effect barrier like `Invoke`; **the VM carries the feature**
(promise heap, outbox, suspend/resume copy, completion modes, scheduler,
deadlock detection, host resolve API). Suspension cost is one copy of one
frame — noise against tool latency.

Scheduler/cascade (unchanged from commitments 3 and 6): async return
resolves its promise → waiters enqueue FIFO; an uncaught throw/rejection
in a continuation rejects its promise. Drain the ready queue at await
points only (no preemption — sync code keeps exact current semantics).
Ready queue empty + outbox non-empty → yield `Pending` to host; both
empty → deadlock, a dedicated non-resumable error naming the awaited
promises. Fuel stays global and uncatchable.

Interaction with 6B (try/catch): handled entirely by the snapshot — the
suspending frame's handler-stack entries are saved into the continuation
record and re-based (their `stack_len`/`fp` fields adjusted to the new
base) on resume. No compiler involvement. A rejection delivered on resume
dispatches to the re-based handler — this is how `try { await p } catch`
works. Test explicitly: catch across await, nested try with only the inner
spanning the await, rejection delivered on resume, handler entries NOT
leaking when a frame suspends inside a try and never resumes.

Diagnostics: there is no stack to read for a suspended chain — Phase 3's
`render_error` gains an **await-chain** view reconstructed from promise
waiter links and the continuation records' await-spans ("while awaiting
tools.fetch at 12:9, awaited by __all at 3:14, awaited at top level 3:1").

Note: the "run synchronously until the first pending await" optimization
falls out of this design for free — an async call IS an ordinary call on
the caller's stack, and nothing is heap-materialized unless it actually
suspends. The common case (async fn whose awaits all hit already-resolved
promises) allocates nothing.

Tests: the canonical chain pattern
(`Promise.all(items.map(async it => tools.g(await tools.f(it))))` resolves
with maximal batching — first yield carries all `f` calls, second all `g`
calls); deterministic interleaving given a fixed resolution order; deadlock
detection; awaits with rich live state (mid-expression temps: await in
loop heads, ternaries, short-circuit operands, call argument lists —
verifying the snapshot/restore preserves evaluation order and stack
contents exactly); two concurrent in-flight calls to the same async fn not
sharing state; suspend → resume → re-suspend cycles; rejection propagating
through an awaiting chain; await-chain rendering.

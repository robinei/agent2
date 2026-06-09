# Constant / copy propagation — plan to "fully realized minus full dataflow"

Builds on the shipped piece (`PERF.md §4b`): write-once `const` → literal
propagation, composing with the optimizer's const-folding. This plan closes the
remaining gaps **except** full intra-procedural dataflow (lattice meets at
branch joins, loop back-edge fixpoint). That one exclusion is what keeps every
phase here sound *without* a CFG value-solver.

## The unifying model: a two-tier known-value environment

Generalize the compiler's current `const_env: HashMap<u32, Instr>` into one
environment with two tiers, both keyed by frame slot and both **Plain-slot only**
(a `Boxed`/by-reference slot can be mutated by a closure call, so its value is
never statically known; `const` and by-value captures are already Plain):

```rust
enum Known {
    Const(Instr), // slot holds a compile-time constant (a const-push)
    Copy(u32),    // slot holds the same value as another (Plain) slot
}
// permanent: immutable for the whole frame (never invalidated)
const_env: HashMap<u32, Instr>            // unchanged
// transient: valid only within a straight-line region (single-entry run)
local_env:  HashMap<u32, Known>           // new
```

**Reference to slot `s`** resolves in order: `const_env[s]` → emit the literal;
else `local_env[s]` → `Const(lit)` emits the literal, `Copy(t)` resolves `t`
(chase, then emit `Local(t)` or `t`'s literal); else `Local(s)`.

**Why two tiers.** A `const` (and an effectively-const `let`, Phase C) is
immutable, so its known value survives branches and loops — it must *not* be
discarded at control-flow boundaries (that's the cross-CF precision we already
have). A mutable `let`'s known value is only valid along a straight-line path, so
it lives in `local_env` and is discarded conservatively.

**The soundness rule that replaces dataflow.** `local_env` is killed by:
1. **any write to the slot** — `SetLocal`/`TeeLocal`/`IncLocal`/`FreshCell(s)`
   removes `s`; for a live `Copy(t)` entry, a write to `t` also removes it;
2. **every `Label` emission** — a label is the only kind of multi-entry / join /
   loop-top point in our codegen (all jump targets are labels), so clearing
   `local_env` at each label means knowledge never crosses a merge. Between
   labels the code is single-entry straight-line, where forward propagation with
   kill-on-write is sound.

This is *extended-basic-block* propagation. It deliberately loses precision at
joins/loops (e.g. the first-iteration value of a loop variable) — recovering
that is exactly the excluded full-dataflow work. Plain-only gating means
`Call`/`Invoke`/closure calls need **no** special handling: a Plain slot is
frame-private and invisible to any callee.

---

## Phase A — Constant branch folding (optimizer only)  ✅ DONE

**Goal.** A conditional jump whose condition is a known constant is resolved
statically; the dead arm is then removed by the existing `simplify_cfg` DCE.
Turns `const DEBUG = false; if (DEBUG) { … }` (DEBUG already propagates to
`PushBool(false)`) into nothing.

**Design.** Add `pe_const_truthy(&Instr) -> Option<bool>` for const-pushes (JS
falsy set: `false`, `0`/`-0`, `NaN`, `""`, `null`, `undefined`). Then in
`pe_reduce` (these pop the condition, so the pair collapses cleanly):

- `<const c>; JFalse(L)` → `Jump(L)` if `c` is falsy, else `Cancel` (fallthrough)
- `<const c>; JTrue(L)`  → `Jump(L)` if `c` is truthy, else `Cancel`

The `optimize` fixpoint then re-runs `simplify_cfg`, which threads/prunes the
unreachable arm. `JNotNullish` *peeks* (no pop), so it can't fold via the
pairwise `Replace` (which would drop the push); handle it as a tiny dedicated
rewrite (`<const c>; JNotNullish(L)` → keep the push, and `Jump(L)` if `c` is
non-nullish else drop the jump) or defer it (literal `??`/`?.` is rare).

**Correctness.** Pure local rewrite; truthiness is computed from the literal.
Independent of the compiler env work but composes with it.
**Risk.** Low. **Tests.** `if(false){}`/`if(true){}` bodies eliminated;
`while(false)` body gone; folds only literal conditions; runtime unchanged for
non-constant conditions.

---

## Phase B — Transient `local_env`: reassignment + straight-line let + copy prop  ⏳ DEFERRED

**Goal.** Handle the mutable cases the permanent tier can't:
`let x = 5; use(x); x = 10; use(x)` (→ 5 then 10), and `const a = b` / `let a = b`
copy propagation.

**Design.**
1. Add `local_env: HashMap<u32, Known>` (above), Plain-slot gated via the
   analyzer's `slot_kinds`.
2. **Record.** At a `let`/`var` (or `const`) decl `x = init` for a Plain slot:
   - if `init`'s emitted instructions `const_eval` to a literal → `local_env[x] =
     Const(lit)` (unless it qualifies for the permanent tier, Phase C);
   - else if `init` is exactly `Local(t)` (a copy) → `local_env[x] = Copy(t)`;
   - else → ensure `x` is absent.
   Same at a plain assignment `x = …`.
3. **Kill.** Route every local-store emission through one helper
   (`SetLocal`/`TeeLocal`/`IncLocal`/`FreshCell`) that removes the target slot
   from `local_env`, and removes any `Copy(t)` entry whose `t` is the target.
4. **Clear at labels.** Route `Label` emission through a helper that clears
   `local_env` (keeps `const_env`). Audit: every jump target is a `Label`
   (guaranteed by codegen), so this covers all joins/loop-tops.
5. **Resolve.** Extend `compile_identifier` to consult `const_env` then
   `local_env` (chasing `Copy`).

**Correctness.** The two kill rules + clear-at-label give single-entry-region
soundness; Plain-only gating removes the call/closure hazard. The crux is
*completeness of the kill/clear hooks* — centralizing store and label emission
through helpers (step 3/4) is what makes that auditable.
**Risk.** Medium (broadest change; the invalidation discipline is load-bearing).
**Tests.** reassignment sequences; copy prop (`const a=b; use(a)` → `use(b)`);
value not propagated across an `if`/loop join (cleared at the label); a `let`
captured-by-ref (`Boxed`) is never tracked; a Plain local is still propagated
across a `Call` (callee can't touch it).

---

## Phase C — Effectively-const `let` (analyzer + compiler)  ✅ DONE

**Goal.** A `let`/`var` assigned exactly once (its initializer) and never
reassigned is immutable in fact — promote it to the **permanent** tier so it
propagates across branches/loops like a `const`, not just within an EBB.

**Design.** The analyzer already visits every binding and assignment for capture
analysis; add an `assigned_once: bool` per binding (initializer is the sole
write — no other `SetLocal`/`++`/`--`/compound-assign targets it). Surface it on
the binding (alongside `is_const`). In `compile_var_decl`, treat an
`assigned_once` Plain binding with a constant initializer exactly like a `const`:
record into `const_env`. References already resolve through `const_env`; gate the
propagation on `is_const || assigned_once` at the ref site.

**Correctness.** "Assigned once, ever" ⇒ immutable ⇒ permanent-tier-safe (same
argument as `const`). The analyzer count must be *exact* — a missed reassignment
would be unsound, so this is the one place that needs careful auditing/tests.
**Risk.** Medium (analyzer precision). **Tests.** assigned-once `let` folds
across an `if`; a `let` with a second assignment is **not** promoted; `+=`/`++`
count as assignments.

---

## Phase D — Closure-capture const seeding (compiler)  ✅ DONE

**Goal.** Propagate a captured constant *into* the closure body:
`const k = 7; const f = () => k * 2` → fold `k * 2` to `14` inside `f`.

**Design.** A capture of a `const` is by-value (const ⇒ Plain ⇒ by-value
snapshot), so the upval holds a fixed value. In `emit_function_def`, instead of
fully resetting `const_env` for the child frame, **seed** it: for each capture
`i` (parent slot `p` from the analyzer's `captures` list), if `const_env[p]`
exists in the *parent*, set the child's `const_env[nparams + i]` to that literal.
The closure body's upval references (which carry `is_const` via `upval_by_name`)
then propagate.

**Correctness.** By-value capture freezes the value; the parent const is
immutable. Per-frame save/restore is already in place — this just changes the
reset to a seed.
**Risk.** Low. **Tests.** captured const folds inside the closure; a captured
*mutable* (`Boxed`) binding is not seeded; nested closures thread transitively.

---

## Phase E — Constant binding elimination (slot + capture)

**Goal.** A `const` bound to a literal that is fully propagated has a dead slot:
no direct read survives (4b), and ideally no captured read either. Eliminate the
**store**, the **slot reservation**, and — the real prize — the **capture**, so a
closure that captured only constants demotes from a heap `MakeClosure` to a bare
`PushFn` (zero heap alloc).

### E.1 — Dead-store elimination  ✅ DONE

The safe, bounded slice, needing no resolution/capture rework. When a binding is
recorded for propagation (immutable + `const_eval`-able initializer) **and** is
not captured (analyzer `binding_captured`), its initializer store is dead — every
read is a literal and `MakeClosure` never reads the slot. `compile_var_decl`
truncates the just-emitted initializer (which `const_eval` success guarantees is
pure const-pushes/ops — no labels/effects) and skips the `SetLocal`. The slot is
still *reserved* by `EnterFrame` (harmless `Undefined`); removing the reservation
is E.2.

### E.2 — Slot + capture elimination (Fn demotion)  ✅ DONE (via "erase at resolution")

Implemented with the **principled model**: a `const x = <literal>` is a
*compile-time binding*, not a runtime variable — it occupies no slot, is never
captured, and every reference resolves to the value. This models the construct
for what it actually is (like `constexpr`), rather than allocating-then-pruning.

The machinery (analyzer):

- **`ConstValue`** + `literal_const_value` recognize literal initializers (number,
  string, bool, null, unary-minus number — v1 scope; non-literal const-foldable
  initializers still get a slot and ordinary `const_env` propagation).
- **`NameRes { Slot | Const }`** is the value type of the lexical `block_scopes`
  stack (`type BlockScopes`). `analyze_register_const` binds a const's *name* (no
  slot) in the current block scope and in the function's `const_names`. Intra-
  function references resolve to `NameRes::Const` → recorded as a const ref;
  shadowing falls out of the innermost-first block-scope walk.
- **`resolve_captures`** is the cross-function piece. A free var is resolved
  nearest-first: if the parent declares it as a const (`const_names`) — or itself
  resolved it to a const (`const_by_name`, the **transitive** case) — it becomes a
  const ref, **never a capture**; only slot bindings become captures. This is the
  key: because const refs never enter capture lists, a closure that referenced
  only consts has empty captures and the existing `emit_closure_value` already
  emits a bare `PushFn` instead of `MakeClosure` — Fn demotion for free, no
  capture-list surgery or upval renumbering.
- **`finalize`** emits `const_refs: span → ConstValue` (intra-function + free
  refs resolved to consts). The compiler emits the literal at each, errors on a
  write, and (since there's no slot) emits no store. Slots/captures/`frame_abs`
  are untouched — consts simply never consumed a slot number.

The earlier worry (cross-function refs breaking, shadowing precedence) is handled
by doing const resolution *inside* the same nearest-first walks that resolve
everything else, so it composes correctly rather than as a parallel layer.

This subsumes E.1 for literal consts (they have no slot at all, captured or not);
E.1's dead-store elimination still applies to *non-literal* immutable bindings
(slotted, e.g. `const o = {…}` or an effectively-const `let`) that aren't
captured.

---

## Phase F — Constant functions (self / mutual recursion)  ✅ DONE

**Goal.** A function *declaration* that is non-reassigned and non-capturing is a
compile-time `Fn(label)` constant — exactly the Phase E idea for function-valued
bindings. References emit `PushFn(label)`, calls are static `Call(label)`, and it
needs no slot or binding store.

**The payoff is structural, not just slot-saving.** Probing the old codegen:
`function a(){b()} function b(){a()}` emitted **two `MakeClosure`** — each
captured the other; self-recursion went through a live self-slot via `CallDyn`.
The only reason recursive functions looked "capturing" is that they reference
each other / themselves, which capture resolution treated as a free variable.
Recognizing them as **constants** removes those captures: a function capturing
only constants captures *nothing* → bare `Fn` + static calls + no slot.

**The analysis (`resolve_const_functions`) is a greatest-fixpoint over capture
resolution itself** — which is what makes transitive captures correct. Naively
checking a function's *direct* free vars is wrong: `middle` below captures `x`
only transitively, by forwarding it to a nested `inner`:

```js
function outer(){ let x=10; function middle(){ function inner(){return x;} return inner(); } return middle(); }
```

So instead of re-deriving capture propagation, we reuse it: optimistically assume
every non-reassigned function declaration is constant; register them (so their
references resolve to `Fn` values, not captures); run `resolve_captures`; **demote**
any that still ended up with a real capture; repeat. Monotone (demote-only) ⇒
converges, and the final run leaves `scopes` correctly resolved. There `middle`/
`inner` keep their captures (they reference the real `let x`), while mutual/self-
recursive functions drop theirs.

**Wiring.** `ConstValue::Fn { label, arity }`; `register_const_fns` puts each
constant function in its enclosing scope's `const_names` (→ `resolve_captures`
resolves references to it as a value) and `const_fn_slots` (→ `finalize`
rewrites same-scope refs to the `Fn` literal); a self-reference in a constant
function resolves to its own `Fn(label)` (static self-recursion, dead self-slot).
The compiler emits `PushFn` for value-refs, a static `Call` for call-refs
(`compile_user_call` checks `const_ref` first), and skips the binding store
(`hoist_function_decl_in_stmt`) and self-slot setup (`emit_function_def`).

**Coverage.** Both function *declarations* and function *expressions* (arrows and
named) assigned to a `const` qualify — the latter via a `const_binding_name` link
from the expression's scope to its `const` binding (set in `analyze_var_decl`),
so `const f = (x) => x * K` and `const fact = function f(n){ … f(n-1) … }` are
constant functions too. The external binding (`const_binding_name`) is distinct
from a named expression's internal `self_name` (its recursion name).

**Truly zero-cost (slot reclamation).** A constant function occupies **no frame
slot**: after the fixpoint, `compact_const_fn_slots` removes each scope's
const-function binding slots and renumbers the surviving own-locals down (same-
scope references are rewritten to the `Fn` constant; cross-scope ones already
resolved via `const_names`), then the final `resolve_captures` recomputes captures
against the compacted numbers. The dead self-reference slot of a constant
function is likewise not allocated. (Compaction is sound because nothing captures
a constant function — its slot is never a capture source — so only same-frame
survivors shift.)

---

## Ordering & independence

```
A (branch folding, optimizer)          ── independent; highest value/effort ratio; ship first
C (effectively-const let, analyzer)    ── extends the permanent tier; independent of B
D (closure-const seeding)              ── extends the permanent tier; independent of B
B (transient local_env)                ── the substantial one; do after A/C/D so the
                                          permanent tier is settled and B only adds the
                                          mutable/copy cases on top
```

A, C, D each touch the *permanent* tier or the optimizer and are small and
independent. B introduces the *transient* tier and the invalidation discipline;
sequencing it last keeps the permanent-tier semantics fixed while B layers the
straight-line mutable tracking over it.

## Explicit non-goal (the excluded item)

Full intra-procedural dataflow — a real CFG with a value lattice, `meet` at
branch joins (`if (c) x=1 else x=2; use(x)` recovering "x ∈ {1,2}" or ⊤), and an
iterative fixpoint over loop back-edges. Every phase above instead **discards**
transient knowledge at control-flow boundaries (Phase B) or relies on
immutability (A/C/D), so none needs a solver. This is the precision ceiling we
accept: we will reload a value after a join/loop that full dataflow could keep.
Revisit only if profiling on real programs shows it matters.
```

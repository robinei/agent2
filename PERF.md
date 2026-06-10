> **Status: HISTORICAL.** This document records the analysis at the time.
> The code has since evolved; where they disagree, the code and its module
> docs win. Known drift: instruction names have been canonicalized
> (`Dup`/`Swap`/`Rot` → `Pick(0)`/`Dig(1)`/`Dig(2)`); the `RcStr` inline-
> string work (not `ThinString`) delivered the Phase 3–4 allocation wins.

# Performance analysis & optimization opportunities

Investigation of the bytecode compiler (`compiler.rs`) and VM (`vm.rs`) for
lowering inefficiencies, suboptimal VM dispatch, and residual allocations.
This is *not* a "speed first" codebase, but the items below are low-hanging
fruit. Status of each is marked `[done]` / `[noted]`.

Baseline (release, counting allocator):
- `alloc_baseline_hot_loop`: **201 allocs / 100 iters** (`Math.abs` + string concat).
- `alloc_baseline_makecounter`: **213 allocs / 100 iters** (closure-heavy).

---

## 1. Allocations on the regular interpretation path — verified clean

The recent `RcStr` inline-string work (commits `8ca5ea2`/`41db524`) and the
`ALLOCS.md` phases did their job. The `alloc_breakdown` test confirms the
residual ~2 allocs/iter in the hot loop are *semantically required*:

- `Math.abs(...)` (a `CallBuiltin`): **0 allocs** — `take_args` is a stack
  memcpy, no arg `Vec`.
- String literal push (`PushStr`): **0 allocs** — refcount bump on the interned
  `RcStr`.
- `s += "x"` (`Add`): **1 alloc** — a new, longer `RcStr`. Unavoidable; the
  result string genuinely differs each iteration.

`to_js_string` on a string is 0-alloc (clones the `RcStr`). Builtin dispatch
allocates nothing. **No action needed on the alloc front** — leaving this as the
verified baseline.

---

## 2. VM: cache the current frame's `local_count` (hottest path)  `[done]`

`frame_floor()` (`vm.rs`) and the bounds check in `Local` / `SetLocal` /
`TeeLocal` / `IncLocal` / `FreshCell` all did `self.callstack.last()` — a
bounds-checked `Vec` indirection + `Option` unwrap — *per instruction*. These
are the most frequently executed instructions (every variable read/write, every
`Pop`/`Dup`/`Swap`/`Pick`/`Dig`/`Nip`/`Rot` goes through `frame_floor`).

`fp` was already a hoisted `VM` field but `local_count` was not, even though it
only changes on `Call`/`CallDyn`/`EnterFrame`/`Return`. Added a
`cur_local_count: u32` field mirrored at those four sites; the hot instructions
now read a plain field and `Local` no longer touches `callstack` at all on the
common (non-boxed) path.

Returns are far rarer than locals, so paying one `callstack.last()` on `Return`
to refresh the cache is a strict win.

---

## 3. VM: `Return` moved return values instead of cloning  `[done]`

`Return(n)` copied each return value down to the frame base with `.clone()`
(`vm.rs`), then truncated the source away. For a returned string or heap pointer
that clone is a refcount bump immediately discarded by the truncate. Switched to
`std::mem::replace` (move, not clone). Proven safe for all `n`: destination
indices (`fp..`) are `<=` source indices (`len-n..`) and we move ascending, so a
source slot is never read after being overwritten. Returns are frequent, so this
removes a per-return refcount churn for string/heap returns.

---

## 4. Optimizer: CFG simplification + peephole + const-fold  `[done]`

There were **no optimization passes** — `backpatch` only stripped `Label`
markers and resolved jump targets. The passes now live in their own
**`optimizer.rs`** (entry point `finalize` = `optimize` fixpoint → `backpatch`).
All run on the *label-form* code (before backpatch): labels are symbolic, so
instructions can be deleted/rewritten freely and `backpatch` recomputes offsets
afterward. The trio is iterated to a fixpoint by `optimize()`:

**`invert_branches()`** — `JFalse(L); Jump(M); Label(L)` → `JTrue(M)` (and the
mirror): a conditional that branches *around* an unconditional jump is inverted
so the jump folds away. Commonly arises once peephole empties an `if`'s
then-branch (`if (c) {} else { … }`).

**`simplify_cfg()` — a single reachability walk, no fixpoint.** The control-flow
transforms are reachability analysis, so they're done in one DFS with a visited
set rather than iterating a rebuild to a fixpoint:

- **Jump threading.** A `Jump`/`JFalse`/`JTrue`/`JNotNullish` whose target label
  resolves (chain-followed once) to an unconditional `Jump(M)` is retargeted to
  `M`; self-loop guarded against `while(true){}`.
- **Dead-code pruning.** A DFS from the program entry *and* every function/
  closure entry (labels named by `Call`/`PushFn`/`MakeClosure`, reached by call
  rather than a control-flow edge) marks all reachable instructions; the rest —
  including whole unreachable labeled blocks — are dropped. This is *strictly
  stronger* than the linear "after an unconditional transfer until the next
  label" scan it replaces, which keeps any block behind a label.
- **Jump-to-next elimination.** A jump whose target `Label(L)` is the next real
  instruction targets where control falls through anyway: an unconditional
  `Jump(L)` is dropped; `JFalse/JTrue(L)` degenerates to `Pop(1)` (both arms
  reach the next instr, but the condition is still consumed); `JNotNullish(L)`
  is dropped (it *peeks*, so there's nothing to consume). The conditional cases
  fire once peephole has emptied an `if`/loop body.

Chains of empty jump-only blocks collapse in one shot: threading follows the
whole `Jump→Jump→…` chain, the DFS prunes the skipped blocks, jump-to-next drops
the remainder, and the zero-width labels are stripped by backpatch.

**`peephole()` — a genuine local-window pass (shift-reduce).** Push each incoming
instruction, then reduce the top two of the output until no rule applies. Re-
reducing after every push means a reduction's *result* is re-examined against
the element now beneath it, so cascades collapse in *any* direction, not just
toward the next incoming instruction. A `Label` is never a rule operand, and the
two reduced instrs are adjacent, so no rewrite crosses a jump target. Rules:

- `Pop(0)` → ∅ (no-op, dropped outright; exposes its neighbours to reduce)
- `Swap; Swap` → ∅; `Swap; Pop(1)` → `Nip(1)`; `Swap; Pop(n≥2)` → `Pop(n)`
- `Dup; SetLocal(x)` → `TeeLocal(x)`
- `Not; Not` → `ToBool` (double negation is boolean coercion)
- `Not; JFalse(L)` → `JTrue(L)`, `Not; JTrue(L)` → `JFalse(L)` (`if(!x)` etc.)
- `ToBool; {JFalse|JTrue|Not}` → drop the `ToBool` (the consumer re-coerces)
- `<bool-producer>; ToBool` → drop the `ToBool` (already a bool — covers
  `Eq;ToBool`, `Lt;ToBool`, `Not;ToBool`, `ToBool;ToBool`, …)
- `Pop(a); Pop(b)` → `Pop(a+b)`
- `<pure-push>; Pop(n)` → `Pop(n-1)` (∅ when `n==1`; a bare `x;` / `5;` value
  statement vanishes — the pushed value is one of the discarded)

**Constant folding** is part of the peephole's reduce loop: when the output tail
is `[<const-push>… , <foldable-op>]`, the window is evaluated in a **throwaway
`VM`** and replaced with a push of the result. Running the real VM gives perfect
semantic fidelity — no re-implemented coercions, and `1/0`→`Infinity` etc. match
runtime exactly. A window that *errors* at runtime (e.g. an out-of-range shift)
is left unfolded so the error is preserved. Operands must be literal pushes
(`PushPosInt`/`Str`/… — not `Local`/`PushPtr`/`PushFn`), so a `Label` among them
blocks the fold. Because the result is itself a const-push and the loop re-reduces
the top, nested expressions cascade: `1 + 2 * 3` → `2*3`=`6.0` → `1+6.0`=`7.0`,
and string concat / comparisons / unary ops fold too.

**Why a fixpoint over the pair (`optimize()`).** The two passes *mutually*
enable each other: `simplify_cfg`'s dead-code pruning makes cancelling
instructions adjacent for `peephole`, **and** `peephole` emptying a block to a
bare `Jump` exposes new threading/jump-to-next for `simplify_cfg`. Neither order
alone is complete, so `optimize()` iterates the pair until the code stops
changing. Each pass is still internally single-shot (DFS / shift-reduce); only
the *interaction* iterates. Instruction count is monotonically non-increasing
(every transform removes/fuses or is an idempotent operand rewrite), so it
terminates quickly — the iteration cap is just a safety bound.

---

## 4b. Compiler: `const` constant propagation  `[done]`

Const folding (§4) deliberately excludes `Local` — the peephole has no idea what
a slot holds, that's frame/runtime state. So `const N = 5; foo(N * 2)` wouldn't
fold on its own. Constant *propagation* closes that: the compiler tracks a
per-frame `slot → const-push` map (`const_env`); a reference to a propagated
`const` emits the literal instead of a `Local` load, which then composes with
§4's folding (`N * 2` → `10`).

This is **sound with no dataflow** — the crucial enabler being two existing
invariants:

- a `const` is **write-once** (reassignment is a compile error), and
- every `let`/`const` gets a **unique slot** (no reuse, even when shadowing —
  `analyzer.rs`), and a captured `const` is captured *by value* (Plain), so its
  value is immutable everywhere.

Hence an entry never goes stale: no kill on reassignment, no control-flow-join
invalidation, no loop back-edge hazard. The map is reset per function body
(slots are frame-relative) and only literal/const-foldable `const` initializers
are recorded (`optimizer::const_eval` evaluates them in a throwaway VM, reusing
§4's machinery); `let`/`var`, destructured, captured-upval, and `for-of` const
loop vars are never recorded. References are gated on the analyzer's per-ref
`is_const`. The initializer's store is still emitted (a by-value closure capture
reads the slot); eliminating it would need dead-store/liveness analysis.

Three follow-ons extend this (see `PROPAGATION_PLAN.md`, all shipped):
- **Constant branch folding** (Phase A): a conditional jump on a known constant
  is resolved statically (`pe_const_truthy` + `pe_reduce`), so the dead arm of
  `if (FLAG)` is pruned by `simplify_cfg`. Feature-flag elimination now works.
- **Effectively-const `let`** (Phase C): the analyzer tracks reassignment
  (`reassigned` set) and exposes per-binding/ref `immutable`; a `let`/`var` never
  reassigned and never captured is propagated like a `const`.
- **Captured-const seeding** (Phase D): `emit_function_def` seeds a closure's
  frame with constants captured by value, so a captured const folds inside the
  closure body.
- **Dead-store elimination** (Phase E.1): a fully-propagated, non-captured const
  has a dead initializer store — `compile_var_decl` drops it (truncates the pure
  initializer and skips the `SetLocal`). Gated on `binding_captured` (a captured
  const's slot is still read by `MakeClosure`).

- **Constant functions** (Phase F): a non-reassigned, non-capturing function —
  declaration *or* a function/arrow expression bound to a `const` — is a compile-
  time `Fn(label)` constant: references emit `PushFn`, calls are static `Call`,
  and it occupies **no frame slot at all** (its dead slot is compacted away and
  surviving locals renumbered). Because recognizing functions as constants
  removes the self/mutual references that *looked* like captures, this turns
  **mutual recursion from two heap closures into zero** (`function a(){b()}
  function b(){a()}`) and self-recursion from dynamic `CallDyn`-through-a-slot
  into a static call with no slot. Computed as a greatest-fixpoint over capture
  resolution (`resolve_const_functions`), so transitive captures stay correct (a
  function forwarding a captured var to a nested closure remains a real closure).
- **Constant binding elimination** (Phase E.2): a `const` bound to a literal is
  modeled as a *compile-time binding* — no slot, no store, never captured.
  References resolve to the literal (intra- and cross-function, via the analyzer's
  `NameRes::Const`/`const_names` and a const-aware `resolve_captures`). A closure
  that referenced only consts therefore captures nothing and emits a bare `PushFn`
  instead of `MakeClosure` — **eliminating the per-creation heap allocation** for
  const-only callbacks (e.g. `arr.map(x => x * K)`). Transitive captures through
  nested closures fold too.

**Not done (deliberately): the transient tier — straight-line reassignment and
copy propagation of mutable locals** (`PROPAGATION_PLAN.md` Phase B), and beyond
it, full intra-procedural dataflow (⊤/unknown meets at branch joins, loop
back-edge fixpoint). The boxing model helps (a `Plain` local is frame-private, so
values needn't be killed across calls) but reassignment kills and control-flow
joins/loops still need either an invalidate-at-boundary transient env (Phase B)
or a real solver. Deferred until there's evidence it matters on real programs.

## 5. Compiler: void `x++;` / `x--;` lowering  `[done]`

A bare update *statement* on a local (`compile_update` with `value_needed =
false`) lowered to four instructions — `Local; PushFloat; Sub; SetLocal` —
whereas the value-needed path uses the single `IncLocal`. Switched the void path
to `IncLocal(...); Pop(1)` (2 instrs). (For-loop `i++` updates already route
through the value-needed path + the loop's own `Pop`, so they were fine.)

---

## 6. Noted, not changed (lower value / out of scope)

- **Dispatch double bounds-check.** The loop does `if ip >= code.len()` then
  indexes `self.code[ip]` (a second, implicit bounds check). Eliding it needs
  `get_unchecked` (unsafe) for a marginal gain on an already-cheap branch.
  Left as-is.
- **`EnterFrame` copies `local_kinds` into a `SmallVec` each call** purely to
  release the `self.code` borrow before pushing locals. Stack-only for ≤32
  locals; the work is O(locals) per call. Restructuring to avoid the copy is
  possible but fiddly for little gain.
- **`for-of`/`for-in` index loop recomputes `ArrLength` every iteration and
  churns the counter through `f64`** (`PushPosInt(1); Add`). Semantically
  faithful (JS arrays can grow mid-loop) and cheap; not worth a dedicated
  counter-increment op.
- **`for-of` over a non-ASCII string** iterates UTF-8 *bytes* and errors on a
  mid-codepoint `IndexGet` boundary — a latent *semantic* divergence from JS
  (which iterates code points), not a perf issue. Flagging for awareness.
- **`in` operator emits a `Swap`** to honor left-to-right eval order before
  `ObjHas`. Correct; the `Swap` is the price of eval order.

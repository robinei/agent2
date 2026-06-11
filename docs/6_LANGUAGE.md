# Phase 6 — Language surface growth

Two parts: (A) syntax additions that are pure compiler/VM work and can land
any time after Phase 1; (B) try/catch, which is a design commitment and
sequences strictly **after Phase 3** (it consumes the enriched-error and
resume machinery).

Already supported, for orientation (don't re-add, do test if untested):
default parameters, destructuring with defaults (objects + arrays),
optional chaining, logical assignment (`&&=`/`||=`/`??=`), template
literals, `for-of`/`for-in`, switch, `arguments`. Deliberately excluded and
staying out: `this`/`class`/`new` (architectural — see 4_FUTURE), getters/
setters, generators, BigInt, labeled break/continue (revisit on evidence),
regex (evidence-gated; if forced, the `regress` crate is a JS-compatible
engine — pairs with the regex-pattern overloads Phase 5 rejects).

## Ground rules

- Per feature: compile tests (rejection cases that now compile), behavioral
  tests in the Phase 2 harness, divergence-list update if semantics differ
  from JS. Commit per feature, `lang:` prefix.
- Every lowering must keep the optimizer's invariants (label-form jumps,
  spans in lockstep). New instructions need: doc comment with stack effect,
  `step()` arm, optimizer purity classification (`pe_is_pure_push` etc. —
  check each `pe_*` table), and a Phase 3 resume classification.

## Part A — syntax additions

### A1. Object and array literal spread (highest prior)

`{...a, k: v, ...b}` and `[x, ...xs, y]`. Two new instructions:

- `ObjExtend // obj, src -> obj`: copy all fields of `src` into `obj`
  (insertion order, later wins — IndexMap gives this for free). JS detail
  that **must** be honored: a `null`/`undefined` src is a no-op, not an
  error (`{...maybeObj}` is everywhere). Non-object src: TypeError
  (JS would copy index keys from arrays/strings — document divergence).
- `ArrExtend // arr, src -> arr`: append all elements of `src` (array src
  only; string/iterable srcs are a TypeError, documented).

Lowering builds the literal incrementally: start from `ObjNew`/`ArrNew` of
the leading static segment, then alternate `ObjSet`/`ArrExtend`-style steps.
Note `ObjSet` pops the object and leaves the *value* — the lowering needs
`Pick` to keep the object on top between segments, or (cleaner) add a
`SetMode`-like "leave object" variant; decide when writing it, with alloc
tests confirming no regression on plain literals (the no-spread path must
compile byte-for-byte unchanged).

Array *holes* (`[1, , 3]`) stay rejected.

### A2. Call spread

`f(...args)`, `Math.max(...nums)`, mixed `f(a, ...b, c)`. One new
instruction: `CallSpread // args_arr, callable -> result` — pops the
callable and a single args array, then dispatches exactly like `CallDyn`
with the array's elements as the arguments. Because `Value::Builtin` is
already callable through the dynamic path, this uniformly covers user
functions, closures, **and** builtins (`Math.max(...nums)` lowers to
`PushBuiltin(MathMax)` + `CallSpread`) — no per-builtin special case.

Lowering: compile the argument list as an array literal (reusing A1's
`ArrExtend` for the spread segments), push the callable, `CallSpread`.
Only use this path when a spread is present — plain calls keep the static
`Call`/`CallBuiltin` fast paths untouched.

### A3. Computed object keys

`{[k]: v}` — currently rejected (`compile_object`), high LLM prior
(`{[id]: value}`). No new instructions: lower the computed segment via
`IndexSet` on the object under construction (same object-on-top bookkeeping
as A1). Key coercion follows the existing `IndexSet` ToString rule.

### A4. Object method shorthand

`{ run(x) { … } }` — currently rejected, but it is *exactly*
`run: function(x) { … }` in a world without `this` (and `this` inside still
errors with its existing message). Route `ObjectPropertyKind`'s method case
through the normal function-expression path. Also verify shorthand
*properties* (`{a, b}`) compile — believed working, add a test.

### A5. Rest elements (lowest tier)

- Rest params `function f(a, ...rest)`: lower to a prologue that builds
  `rest` from the `arguments` array minus the leading `nparams` (needs
  `arr.slice` from Phase 5, or a small dedicated instruction — prefer
  reusing the builtin).
- Array destructuring rest `[x, ...xs]`: same slice approach.
- Object destructuring rest `{a, ...rest}`: copy-minus-keys; small helper
  builtin or instruction. Least common of the three — fine to defer if it
  drags.

Status: all three rest forms landed (object rest is copy-minus-keys via
`ObjExtend` + `ObjDelete`, no new primitives; keys — computed included —
evaluate exactly once). Destructuring also works in `for-of`/`for-in`
declaration heads (`for (const [k, v] of Object.entries(o))`), reusing the
normal pattern lowering per iteration. Function-*parameter* destructuring
(`function f({a, b})`, pattern rest `f(...[a, b])` included) also landed:
the analyzer keeps one anonymous param slot per declared pattern (empty
name — not a legal identifier, so never referenced or captured) and declares
the pattern's bindings as ordinary own locals; the prologue loads the slot,
applies the whole-pattern default, and reuses the normal pattern lowering.
No new instructions; identifier-only param lists compile unchanged.

## Part B — try / catch / throw (after Phase 3)

Position: in-program error handling is the **inner layer of the condition
system**, not a rival to it (CL: `handler-case` inside, the debugger — here,
the LLM — outermost). Code mode's value is fewer LLM round-trips; tool
failures are routine and often locally handleable (fallback, retry, skip),
and `try { JSON.parse(s) } catch` is among the strongest trained idioms.

Locked design decisions:

1. **`raise()` is NOT catchable.** Conditions are addressed to the LLM; a
   program must not be able to swallow them. `raise` unwinds nothing and
   yields `StepResult::Raise` exactly as today, regardless of enclosing
   `try`. This keeps exceptions (program-level) and conditions
   (operator-level) crisply separate.
2. **Catchable:** explicit `throw`; runtime errors Phase 3 classified as
   resumable (TypeError/ValueError class); tool-call failures the host
   chooses to throw in. **Not catchable:** `OutOfFuel`, the memory budget,
   and `NotResumable` internal errors — a buggy retry loop must not trap
   its own kill switch. These continue to surface as `Err` from `step()`.
3. **Error values are plain objects** `{ name, message }`, JSON-shaped like
   everything else. A caught VM error materializes with `name` from the
   error kind and `message` = the Phase 3 rendered diagnostic (line/col +
   source line), so `e.message` is genuinely useful. Special-case
   `new Error(msg)` (and `TypeError` etc.) in the compiler to build that
   object — LLMs universally write `throw new Error("…")`; no general `new`
   machinery is implied.
4. **Uncaught propagation is unchanged:** an uncaught throw becomes the
   Phase 3 `Err(VMError)` at the `step()` boundary (kind/ip/message/resume
   filled in), so the LLM escalation path is identical with or without
   try/catch in the program.

Mechanics:

- VM grows a handler stack: `{ catch_ip, stack_len, callstack_len, fp }`
  snapshots. New instructions `TryEnter(label)` / `TryExit` push/pop an
  entry; throwing truncates `stack` and `callstack` to the snapshot,
  restores `fp` (and the `cur_local_count` mirror), jumps to `catch_ip`,
  and pushes the thrown value (the catch binding; an unused binding gets
  `Pop`). Cells stay (monotonic, by design).
- Host API: `vm.throw_value(v) -> ThrowOutcome` — unwinds to the nearest
  handler or reports "uncaught" so the host can convert a failed `Invoke`
  into a program-visible exception *or* escalate, per its policy. This is
  the bridge between tool failures and program-level handling.
- `finally`: implement via codegen duplication (run the block on both the
  normal and unwind paths), or explicitly defer it — `catch` is the prior
  that matters; an honest "finally not supported" diagnostic is acceptable
  for v1. Do not attempt JS's full completion-value semantics.
- Optimizer: `TryEnter`/`TryExit` are barriers (not pure, not movable);
  audit the CFG pass — the catch label must be treated as reachable.
- The swallowed-error risk (broad `catch` hiding bugs the LLM would have
  fixed) is accepted: mitigated by error objects carrying full rendered
  diagnostics, by `raise` being unswallowable, and by uncatchable resource
  errors. Revisit only with evidence.

Update on completion: divergence list (remove "no exceptions" bullet),
`try`/`throw` compile errors deleted, 4_FUTURE non-goals already amended to
point here.

Status: **landed.** All four locked decisions implemented as written. Notes
on the open choices and deltas:

- `finally` is implemented via the blessed codegen duplication:
  `try B catch C finally F` lowers as `try { try B catch C } finally F`
  (an outer handler, so an exception in `C` still runs `F`), with `F`
  emitted twice — normal path, and unwind path followed by a rethrow
  `Throw`. JS's completion-value semantics are not attempted: a `break`/
  `continue`/`return` that would cross a `finally` boundary (or escape the
  `finally` block itself) is a compile error with an honest message.
  Function bodies inside a duplicated `F` are emitted once per copy under
  the same entry label; label resolution is last-wins and the earlier,
  never-targeted copy is pruned as unreachable, so closures in `finally`
  are safe.
- Mechanics as specced: VM handler stack + `TryEnter(label)`/`TryExit`/
  `Throw`; `vm.throw_value(v) -> ThrowOutcome` host API; optimizer treats
  `TryEnter` as a two-way branch (catch label reachable) and threads/
  backpatches its operand; all `pe_*` tables exclude the new instructions
  by construction (allow-lists).
- Catchability is exactly the Phase 3 `PushValueThenContinue` class. To
  make `try { x.foo }` on null/undefined useful, `ObjGet`/`ObjSet`'s
  non-object error arms were pop-first-normalized (they were peek-style
  `NotResumable`); the audit table was updated.
- `await` of a rejected promise inside `try` delivers the **raw rejection
  value** to `catch` (JS semantics), not a wrapped `{name, message}`; the
  no-handler escalation path is byte-for-byte unchanged.
- An uncaught `throw` escalates as the dedicated
  `ErrorKind::UncaughtException` (not `ValueError`): the program produced
  this error value deliberately, and the host's policy differs from a VM
  failure. The thrown value is preserved structurally in
  `VMError::payload`; the message carries the `uncaught …` rendering.
- `new Error(msg)` plus `TypeError`/`RangeError`/`SyntaxError`/
  `ReferenceError`/`EvalError` are special-cased; message coerces ToString
  at construction; a second argument (`{cause}`) is a compile error.
- The compiler tracks per-function `try` depth and emits the balancing
  `TryExit`s when `break`/`continue`/`return` jump out of a `try` block,
  so a frame never leaves stale handlers behind.

## Part B2 — `finally` completion: full completion-value semantics

Removes the three Part B compile-error edges (`break`/`continue`/`return`
crossing a `finally` boundary; `break`/`continue` escaping the `finally`
block; `return` inside `finally`) and replaces them with full JS semantics,
including `finally` overriding a pending completion. **Compiler-only: zero
new instructions, zero VM/optimizer changes.** This is the duplication
scheme extended to every exit edge — the approach javac settled on after
abandoning JSR/RET subroutines — with each copy of `F` statically knowing
which completion is pending, so the spec's completion-record dance becomes
straight-line specialization.

### Locked design

1. **Exit stubs, one per `(finalizer try, ultimate exit kind)`, shared by
   all exit sites.** A `break`/`continue`/`return` inside a try-with-finally
   does not inline `F` at the exit site; it emits its balancing `TryExit`s
   and a `Jump` to a *stub* requested on the innermost crossed finalizer
   entry. `compile_try` emits the requested stubs right after the unwind
   copy (after the rethrow `Throw`, before `Label(end)`): each stub is
   `Label(stub) ; F copy ; onward transfer`, where the onward transfer
   re-runs the same exit walk in the post-`try` compile context — so outer
   finallys chain by requesting stubs on *their* entries, recursively.
   Stub identity is the ultimate kind: `Return`, or
   `Jump { target_label, try_floor }` (the loop ctx's recorded absolute
   `try_depth`, still valid at stub-emission time). Two `break`s to the
   same loop share one stub; `break` vs `continue` get two. No AST
   references are stored anywhere: requests accrue on the entry during body
   compilation only (exits inside `F` copies target *outer* entries — this
   one is already popped), and `compile_try` drains them with `fin` in
   scope.
2. **Pending exception stays on the operand stack (status quo); pending
   return value moves to a reserved frame local (the spill slot).** The
   unwind copy keeps today's shape: thrown value beneath, rethrow `Throw`
   after. A `return` that crosses a finalizer evaluates its value, then
   `SetLocal(spill)`, then walks; the Return stub's onward transfer (when
   no further finalizer is crossed) is `Local(spill) ; Return(1)`. A
   `return` crossing only catch-only trys keeps today's value-on-stack fast
   path — slot untouched, codegen unchanged. The slot is appended after the
   self-reference slot (index `nparams + upval_count + own_local_count +
   self?1:0`, known before body compilation); the emitted `EnterFrame` is
   patched post-body to allocate it only when some return actually used it
   (the root program's conditional `EnterFrame` must be forced/inserted
   when needed — insertion before backpatch is safe, labels are positional
   markers). Override semantics for nested pending returns are automatic:
   the inner `return` overwrites the shared slot, which is exactly JS's
   "finally's return wins".
3. **Copy frames replace `finally_loops_floor`.**
   `finally_copies: Vec<CopyCtx { try_floor: usize, unwind_pending: bool }>`
   — one frame pushed around each `F` copy compilation (`try_floor` =
   `try_stack.len()` at copy start; `unwind_pending` only for the unwind
   copy, whose thrown value occupies one stack slot beneath it). Saved and
   reset per function body alongside `try_stack` (the existing
   `mem::take`). Whether an exit escapes a copy needs no loops-floor check:
   it is implicit in the walk floor — if the target sits inside the copy,
   the walk stops before the copy boundary.
4. **The exit walk (soundness-critical ordering).** From
   `depth = try_stack.len()` down to the exit's floor, interleaving two
   kinds of crossings in strict depth order: when the next boundary is a
   copy frame with `try_floor == depth`, emit `Pop(1)` if `unwind_pending`
   (crossing the copy discards the pending exception — JS's override);
   when it is a try entry, emit `TryExit`, and if the entry has a
   finalizer, `Jump(stub(entry, kind))` and **stop** — the stub's onward
   transfer continues the walk from `depth = entry index` with exactly the
   copies whose `try_floor ≤` that index, which are precisely the ones
   still active in its compile context (copies nest within try bodies, so
   any copy above the entry ended before stub emission). **Invariant: a
   pending slot is popped only after every handler entered above it has
   been `TryExit`ed.** A handler opened inside an `F` copy snapshots a
   stack that *includes* the pending slot beneath; popping that slot while
   such a handler is live would desynchronize the snapshot — a later throw
   would truncate to a length where the slot's position holds garbage, and
   the rethrow would throw that garbage. This is also why the return value
   is spilled to a local rather than popped-under on the stack: there is no
   sound emission point for a pop-under, and no stack-shuffle instruction
   exists (or is wanted).
5. **Override semantics fall out; no dedicated machinery.** A jump out of
   an `F` copy simply never reaches the copy's trailing epilogue (rethrow /
   `Local(spill); Return(1)` / `Jump(target)`) — the CFG pass prunes it.
   `throw` inside `F` needs nothing at all: the unwinder truncates to the
   outer handler's snapshot, which predates the pending slot, discarding it
   automatically. `return` inside `F` is just an exit initiation like any
   other.

Closures inside `F` now duplicate 2 + #stubs times; the last-wins label +
unreachable-pruning argument is copy-count-agnostic but gets a ≥3-copy
test. Code growth is stubs-per-exit-kind, not per-exit-site — strictly
better than javac — and only when the crossing exits exist; programs
without them compile byte-for-byte unchanged.

### Semantics pins (behavioral tests, expected = Node)

| Program (sketch) | Expected |
|---|---|
| `while(1){ try { break } finally { log } }` | `F` runs once, loop exits |
| same with `continue`, bounded loop | `F` runs per iteration |
| `break` crossing two nested finallys | both run, innermost first |
| exit-path `F` throws, outer `try/catch` | exception caught, `break` abandoned |
| `let x=1; try { return x } finally { x=2 }` | returns `1` |
| `return` crossing two finallys | both run, value preserved |
| `try { return 1 } finally { return 2 }` | `2` |
| `try { throw e } finally { return 2 }` | returns `2`, exception swallowed |
| `try { throw e } finally { break }` | swallowed, loop exits |
| `for(…){ try { break } finally { continue } }` (bounded) | `continue` wins |
| `try { continue } finally { break }` | `break` wins |
| `try { return 1 } finally { throw e }` | `e` thrown |
| `try {} finally { return 7 }` (normal path) | `7` |
| `try { throw "t" } finally { try { return g() } catch {} }`, `g` throws | `g`'s error caught, `"t"` rethrown intact |
| `while(1){ try { throw "t" } finally { try { break } catch {} } }` | loop exits, no corruption |
| `switch` `break` crossing a finally | `F` runs, switch exits |
| `break` from a `catch` block crossing the finalizer entry | `F` runs, then break |
| closure defined in `F` duplicated ≥3× (normal + unwind + stub) | works on every path |
| uncaught rethrow after finally (existing pin) | payload preserved |

### Steps — finish each gate before proceeding

**Step 1 — `break`/`continue` across and out of `finally` (stub
machinery).** `try_stack: Vec<bool>` → `Vec<TryCtx { has_finalizer, stubs }>`;
`finally_loops_floor` → `finally_copies`; the exit walk (design pt. 4) with
`Jump`-kind stubs; stub emission in `compile_try`. Delete the
`break`/`continue` arms of `rejected_finally_exit` (and their compile-error
tests); `return` rejections stay for Step 2. Acceptance:

- [ ] All `Jump`-kind pins above land as tests in
      `interp/src/compiler/tests/exceptions.rs` (break/continue rows,
      override rows not involving `return`, nested-handler soundness rows,
      catch-block exit, switch).
- [ ] `break_crossing_finally_rejected` / `continue_crossing_finally_rejected`
      deleted; `return_*_rejected` still green.
- [ ] No-crossing programs compile unchanged: existing
      `codegen_shape.rs` + `exceptions.rs` suites untouched and green.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

**Step 2 — `return` across and inside `finally` (spill slot).** Spill-slot
reservation + `EnterFrame` patching (design pt. 2), `Return`-kind stubs,
`emit_return_try_exits` folded into the shared exit walk. Delete the two
`return` rejections and their tests. Acceptance:

- [ ] All `Return`-kind pins land as tests (value preservation, double
      finally, overrides in both directions, throw-replaces-return, the
      `g()`-throws snapshot-soundness pin, normal-path `return` in `F`,
      top-level `return` crossing a finally in the root frame).
- [ ] `return_crossing_finally_rejected` / `return_inside_finally_rejected`
      deleted; `return_in_function_inside_finally_is_fine` still green
      (closure bodies reset the copy context).
- [ ] Returns crossing only catch-only trys compile byte-for-byte as today
      (no spill slot allocated — assert via existing shape tests or a new
      one).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

**Step 3 — docs + divergence sweep.** Acceptance:

- [ ] `interp/src/vm/mod.rs` divergence bullet rewritten: exceptions
      including `finally` completion semantics are JS-faithful; remaining
      caveats (uncatchable `raise`/`OutOfFuel`, error-object shape) kept.
- [ ] This file's Part B status note updated to point here; Part B2 status
      line added.
- [ ] `interp/docs/COMPILER_PLAN.md` stale "out of scope: try/catch/…"
      line fixed (already wrong since 6B).
- [ ] Compiler comment sweep: no remaining "compile error" /
      "completion-value semantics out of scope" claims.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green; commit per
      step, `lang:` prefix.

Status: **not started.**

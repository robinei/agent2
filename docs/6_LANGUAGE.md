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

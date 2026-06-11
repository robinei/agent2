> **Status: HISTORICAL.** This document records the plan as designed at the
> time. The code has since evolved; where they disagree, the code and its
> module docs win. Known drift: `heap[0]`/`HeapValue::Object` → typed heaps with `objects[0]`; `ObjGetDyn`/`ObjSetDyn` → `IndexGet`/`IndexSet`; `Alloc` → `EnterFrame`; `PushStr(String)` → `PushStr(RcStr)`; `StackValue` → `Value`; `Read`/`Write`/`variables` removed; `Invoke` no longer yields per call (7_ASYNC Tier 1: tool calls return promises, the host sees the accumulated outbox at a blocking `await` via `StepResult::Pending`, and `async`/`await` are no longer excluded syntax). Still authoritative: §1 (variable model — everything → frame locals), §2 (state is the only durable surface, `state` is a blessed Object), §3 (tool calls → `Invoke`), §4 (raise → `Raise`), rewrite-from-top model, short-circuit/assignment lowering, label/backpatch mechanism, code layout.

# Compiler Plan — `compile.rs` (JS source → VM instructions)

Compiles a subset of JS into the stack VM defined in `agent/src/vm.rs`. Reads
the source with `oxc_parser`, traverses the AST, compiles supported constructs,
and emits informative errors for the rest.

## Locked design decisions

### 1. Variable model: everything → frame locals
- The whole program *is* the root frame's body. The VM starts with a root frame
  (`fp=0, ip=0, local_count=0`), so top-level `let`/`const`/`var` hoist into a
  root-frame `Alloc` exactly like any function's locals.
- Top-level code ends with an explicit `Return(0)` → pops the root frame →
  `StepResult::Done`. This also prevents execution falling through into the
  function bodies appended after it.
- Capture analysis is uniform: a top-level function closing over a top-level
  binding just boxes that root-frame slot. No special-cased top-level path.
- A bare identifier that resolves to no local/param is a **compile error**
  (undeclared variable). All durable/host-context access goes through `state`
  (see §2). This keeps typos from silently becoming persistent state.

### 2. Durable state: `state` is a blessed heap Object (no `Read`/`Write`)
- `state` is a single `HeapValue::Object` living at a fixed, known heap address
  (`heap[0]`), allocated when the VM is constructed for a program.
- The whole durable surface lowers to **existing** object instructions:

  | JS | Lowering |
  |---|---|
  | `state.foo` | `Push(Ptr(0))` · `ObjGet("foo")` |
  | `state.foo = x` | `Push(Ptr(0))` · `<x>` · `ObjSet("foo")` |
  | `state[expr]` | `Push(Ptr(0))` · `<expr>` · `ObjGetDyn` |
  | `state[expr] = x` | `Push(Ptr(0))` · `<expr>` · `<x>` · `ObjSetDyn` |
  | `state.foo += …` | `Push(Ptr(0))` · `ObjGet` · `<…>` · op · `Push(Ptr(0))` · `ObjSet` |
  | `Object.keys(state)` / `Object.values(state)` | `Push(Ptr(0))` · `ObjKeys` / `ObjValues` |
  | `k in state` / `delete state.foo` | `ObjHas` / `ObjDelete` |
  | `JSON.stringify(state)` | `Push(Ptr(0))` · `StrFromJson` |

- This **drops `Read`/`Write`/`VarName`/the `variables` HashMap** from the VM
  (already dead-code-flagged) and gets dynamic keys, enumeration, membership,
  delete, and whole-bag JSON **for free** as plain `Object` ops.
- **`state` is a const reference**: `state.foo = …` mutates; rebinding
  `state = …` is an error (can't move `heap[0]`). Bare `state` is a valid value
  (`Ptr(0)`), so `JSON.stringify(state)` etc. work.
- **Don't make `state` cyclic** (`state.self = state`): JSON serialization hits
  the existing `MAX_JSON_DEPTH` guard and errors rather than hanging. Document;
  no special handling.
- **Persistence boundary:** the host extracts `heap[0]` via the existing
  `stack_value_to_json` and re-seeds it into the next VM's `heap[0]`.
  Host-injected inputs are simply the initial contents of `state` — one channel
  for inputs *and* durable storage.
- Tell the LLM: **store only JSON-able data in `state`** (data, not
  functions/closures — those have no JSON form and won't survive a rewrite).

### 3. Tool calls: `tools.name(args)` → `Invoke(name, argc)`
- `tools` is a reserved identifier valid **only** as the receiver of a method
  call: `tools.foo(a, b)` lowers to `Invoke("foo", 2)`.
- Bare `tools`, `tools.foo` without a call, and computed `tools[x](...)` are
  errors.

### 4. Raise: `raise("...")` → `Raise(String)`
- The `Raise` instruction carries a compile-time `String`, so `raise` requires a
  **string-literal argument**; a non-literal argument is an error.
- `raise(...)` is an expression (`() -> any`): the host pushes the resumed value
  back onto the stack.

### 5. Error reporting: custom, decent
- `Diagnostic { span, message }`, rendered as `line:col` plus the offending
  source line and a caret.
- Collect-many: gather all diagnostics, then abort before producing a `Program`
  if any exist.
- oxc's own syntax errors are converted into the same `Diagnostic` shape.

## Replan / rewrite model: rewrite-from-top (no checkpoints, no ip-teleport)

On a `Raise`, the host (LLM) may rewrite the program. We deliberately do **not**
implement checkpoints or `ip` teleport. Instead:

- The new program is compiled and run **from the top** (`ip=0`, fresh frame,
  fresh stack/heap), with `state` (`heap[0]`) re-seeded from the previous run.
- Only `state` persists; locals/stack/heap are transient — which is already the
  model's invariant, so "start fresh and rehydrate from `state`" is nothing new.
- The LLM writes the rewritten program to **omit or guard work already recorded
  in `state`** — typically reading cached tool results from `state`, or guarding
  effectful calls:

  ```js
  if (!state.emailSent) { tools.sendEmail(...); state.emailSent = true; }
  ```

  Idempotent/read-only tools can simply re-run. This is strictly more flexible
  than checkpoints (per-call granularity, arbitrary control-flow changes) and
  deletes a whole subsystem from the compiler.
- "Where was the condition raised" comes from `spans[ip_at_raise]` → line:col +
  surrounding source. No checkpoint machinery needed.

Why this fits: programs here are short, fuel-bounded orchestration flows, so
re-running from the top is cheap; the only thing worth avoiding is redoing
expensive/effectful **tool calls**, whose results are already in `state`.

## Required VM changes

1. **Add `Instr::PushStr(String)`** — allocate a heap string at runtime and push
   its `Ptr`. Today there is no way to introduce a string constant (`Push` is
   `Copy`, can't carry a `String`), so string literals, string-valued keys, and
   template-literal fragments have nothing to lower to. `PushStr` is consistent
   with array/object literals (built fresh each execution via `ArrNew`/`ObjNew`),
   keeps the compiled `Program` as just `{ code, spans }` (no constant pool /
   heap blob to serialize), and re-allocs per execution — fine for short, no-GC
   programs. With `PushStr`, runtime strings land at `heap[1+]`, leaving `Ptr(0)`
   (state) forever stable.
2. **Add `IndexGet` / `IndexSet`, replacing the dynamic indexing variants.**
   Runtime-polymorphic computed access for `x[i]`: there is no static type to
   choose between array, object, and string access, so a variable-keyed `x[i]`
   cannot otherwise be lowered. They inspect the container at runtime: array+int
   → element (OOB read → `undefined`, OOB write → error, negative → error),
   object → string-key property (`ToString` the key; missing → `undefined`),
   string+int → char (1-char string), mirroring JS key coercion. These
   **replace and remove** `ArrGet`, `ArrSet`, `ObjGetDyn`, `ObjSetDyn` (a strict
   superset; the compiler never has the static type to pick among the old four).
   **Keep** the static-name `ObjGet(FieldName)`/`ObjSet(FieldName)` as the fast
   path for `obj.foo`/`state.foo` (no `PushStr` + heap-string alloc per access).
   Existing `ArrGet`/`ObjGetDyn`/… tests get rewritten onto `IndexGet`/`IndexSet`.
3. **Add a program-running constructor** (e.g. `VM::for_program(code, state_json)`)
   that allocates `heap[0] = Object` (empty or seeded from prior state) before
   running. Keep `VM::new` as the raw constructor so existing `run_heap`-based
   tests keep their current heap addressing untouched. Compiled programs always
   go through `for_program`.
4. **Remove** `Read`, `Write`, `VarName`, and the `variables` HashMap (superseded
   by blessed `state`).
5. **Add `Pick`/`Dig`/`JTrue`/`JNotNullish`** for assignment lowering and
   short-circuit operators. `Pick(n)` duplicates the n-th-from-top value
   (workhorse for read-then-write lvalue lowering); `Dig(n)` reorders it without
   copying. `JTrue` is the truthy-branch mirror of `JFalse`, letting `||` lower
   without an extra `Jump`. `JNotNullish` **peeks** (does not pop) the top value
   and jumps when it is neither null nor undefined, leaving it in place — the
   single-instruction nullish test that lowers `??`, optional chaining (`?.`),
   and optional method calls without a `Dup`+`Push(Null)`+`LooseEq` per check.
6. **Add `ToNum`/`ToBool` coercion instructions** — `+x` emits `ToNum`;
   `Boolean(x)` emits `ToBool`. Keep `ToStr` (was already present).
7. **Store-returns-value:** `ObjSet`/`IndexSet` now leave the assigned value (the
   RHS) on the stack, so assignment is a well-formed expression without extra
   stack shuffling. Statement callers follow with `Pop(1)`.
8. **Builtin infrastructure** (`builtin.rs`, `Builtin` enum, `CallBuiltin` instr,
   `Value::Builtin`). Call-shaped intrinsics (`arr.push(x)`, `s.split(",")`,
   `Math.max(a,b)`, `Object.keys(o)`, `JSON.parse(s)`, …) are now lowered to
   `Instr::CallBuiltin(Builtin, argc)`. The `Builtin` enum is the id + registry:
   `meta()` provides arity bounds (compiler reads for arity checks/errors),
   `call()` does the dispatch. Builtins are also first-class values
   (`Value::Builtin`) for passing as callbacks (`arr.map(Math.sqrt)`).
   Calling convention: `argc` args on stack L→R (arg 0 deepest; receiver is arg 0
   for methods); builtin pops exactly `argc` and pushes exactly one result.
   **Removed instructions** (superseded by builtins): `ArrPush`, `ArrPop`,
   `ArrShift`, `ArrUnshift`, `ArrJoin`, `StrSplit`, `StrIncludes`,
   `StrStartsWith`, `StrEndsWith`, `StrIndexOf`, `StrLastIndexOf`, `StrSlice`,
   `StrTrim`, `StrToInt`, `StrToFloat`, `StrToJson`, `StrFromJson`, `ObjKeys`,
   `ObjValues`, `IsArr`, `IsInt`, `Abs`, `Sqrt`, `Ceil`, `Floor`, `Round`,
   `Sign`, `Min`, `Max`.

## Pipeline

```
source (String)
  └─ oxc Parser ──► Program AST (arena-allocated)        [syntax errors → Diagnostic]
       └─ Pass 1: Resolve/Analyze (scopes, slots, capture/boxing, validation) [semantic errors]
            └─ Pass 2: Codegen → Vec<Instr> with Label markers + parallel Vec<span>
                 └─ Pass 3: Backpatch → strip Labels, rewrite addresses, compact spans
                      └─ Program { code, spans, source }
```

Two passes per function are unavoidable: capture analysis must see nested
functions' free-variable use *before* we can decide which slots are `Boxed` and
emit the correct prologue `Alloc`.

**Pass 1 is the single source of truth for names.** It walks the AST once,
records every binding occurrence and identifier reference keyed by source span,
then resolves captures bottom-up (free variables propagate up the scope tree;
the resolvable ones become upvals, boxing the captured owner slot). A final step
turns the recorded own-slots into absolute frame slots, yielding three
span-keyed tables — `binding_slot` (declaration → slot), `ref_resolution`
(reference → slot + const-ness), `scope_by_span` (function node → scope). **Pass
2 (codegen) keeps no scope state of its own**: it looks every binding/reference/
function up by span. This avoids re-deriving slot assignment in codegen (the
source of an earlier crop of cursor-synchronization bugs).

## Output artifact

```rust
pub struct Program {
    pub code: Vec<Instr>,
    pub spans: Vec<u32>,    // spans[ip] = source byte offset of the instr at ip
    pub source: Arc<str>,   // (or a precomputed line-start table)
}
```

The host runs the VM; on `Err(VMError)` at `vm.ip` it does `spans[ip]` → line:col
via binary search over line starts. The same lookup serves compile-time errors.
Storing the **byte offset** (not a precomputed line) keeps it flexible and cheap.

## Label / backpatch mechanism

- A monotonic `next_label: u32` allocator. Block/function entries emit
  `Label(id)`.
- `Jump`, `JFalse`, `Call`, `MakeClosure` carry a **label id** in their
  `CodeAddr` field during codegen (same `u32`, reinterpreted until Pass 3).
- A **function used as a value** (`map(foo, …)`, storing a function in a var)
  lowers to `Push(Value::Fn(label_id))`, so backpatch must also rewrite
  `Fn` addresses buried inside `Push`. This is safe because the compiler never
  emits a real code address pre-backpatch — every `Fn`/`Jump`/`Call`/
  `MakeClosure` address is a label id until Pass 3.
- Backpatch (single linear pass): scan once counting non-`Label` instrs to build
  `label_offset[id]`; then emit, dropping `Label`s, rewriting
  `Jump`/`JFalse`/`Call`/`MakeClosure` and `Push(Fn(_))`, and copying `spans` in
  lockstep so the table stays aligned with the compacted code.

## Code layout in the flat vector

- Top-level code occupies offset 0 (where `ip` starts) and ends with `Return(0)`.
- Function bodies are emitted **inline at their definition site**, guarded by a
  jump-over: `Jump(after) · Label(entry) · prologue · body · Return · Label(after)`.
  Sequential execution hits the `Jump` and skips the body; `Call`/`CallDyn` enter
  at `Label(entry)`. This was chosen over a deferred worklist
  (`pending_functions`) of appended bodies: the worklist would have to store
  borrowed AST nodes, forcing the whole `Compiler` to carry the arena lifetime,
  whereas inline emission needs none of that and costs only one `Jump`+`Label`
  per function (negligible under the fuel budget). Either way the body is
  reachable only via `Call`/`CallDyn`, and all addresses resolve in Pass 3.

## Calls — lower everything to `CallDyn` initially

- User functions: push args left-to-right, push the callee value, emit
  `CallDyn(n)`. Named functions become `Push(Fn(label))` then `CallDyn`. Uniform;
  handles closures, callbacks, and recursion with no static call-graph
  resolution.
- Static `Call(addr, n)` is a later optimization for directly-named callees.
- Intrinsic methods, `tools.*`, and `raise` are recognized **structurally** and
  lowered to their dedicated Instrs, never `CallDyn`.

### Reclaiming static calls (`CallDyn` → `Call`/`CallBuiltin`)

The reclaim is **codegen-directed, not a peephole pass** — the callee shape is
known syntactically, which is exactly the information a flat-stream peephole
lacks. Two cases:

- **Directly-named callees** (`f(x)`, `Math.max(x)`): emit the static form at
  codegen. Builtins already do (`CallBuiltin`); named user functions will emit
  `Call(addr, n)` in Phase 3. No `CallDyn` is produced in the first place.
- **Constant callee through the value path** (what `?.()` produces): a
  `Builtin`/`Fn` constant is never nullish, so the `?.` guard is provably dead —
  `Math.max?.(a, b)` is identical to `Math.max(a, b)`. Codegen detects a constant
  non-nullish callee (today: a `namespace_builtin` reference; Phase 3: named
  function refs) and emits the static `CallBuiltin`/`Call`, skipping the guard,
  `Dig`, and `CallDyn` entirely. **Done** for builtin refs.

`CallDyn` therefore survives only for genuinely dynamic callees (`state.fn?.(x)`
— value not statically known, guard genuinely needed). A small adjacent-pattern
peephole (`Push(Fn|Builtin)` immediately before `CallDyn` → static form) is a
*possible* later supplement, but it can't see the args-then-`Dig` shape, so it
adds little over the codegen path.

## Intrinsics (method / static-call recognition)

The VM has no prototype/method objects, so calls like `arr.push(x)`,
`s.split(",")`, `Math.max(a,b)`, `Object.keys(o)`, `JSON.parse(s)` are matched
by **callee shape + arity** and lowered to `Instr::CallBuiltin(Builtin, argc)`.
The `Builtin` enum (in `builtin.rs`) is the single registry: `meta()` provides
arity bounds the compiler reads for arity checks and error messages; `call()`
handles the runtime dispatch. This replaces the old one-instruction-per-method
approach — the instruction set is now true VM primitives, and the builtin
registry gives variadic/optional-argument support for free.

Consequences:
- Those method names are effectively reserved.
- A builtin can be passed as a first-class value (`Value::Builtin`) for
  callback use (`arr.map(Math.sqrt)`).
- **Dispatch is purely syntactic and assumes the conventional receiver type**
  (no static types). `.length` → `ArrLength` (arrays/strings); an object property
  literally named `length` accessed via `.length` is an accepted divergence.
  Method names like `.push`/`.split` assume array/string receivers; a mismatch
  is a runtime `TypeError`. Computed `x[i]` (non-literal key) lowers to the
  polymorphic `IndexGet`/`IndexSet`.

## Stack-discipline invariants

- Every expression leaves exactly one value; every expression *statement* is
  followed by `Pop(1)`.
- `Alloc` requires `sp == fp + local_count` (no temporaries above locals). So
  **all** of a function's locals are allocated in the prologue, before any
  expression temporaries: hoist every `let`/`const`/`var`/function-decl binding
  in a function into one prologue `Alloc`, with per-slot `Plain`/`Boxed` chosen
  by capture analysis. Lexical block scoping is enforced in the resolver (name
  visibility); slots are function-wide. Slots are **not** reused across exited
  scopes — one slot per unique binding. Reuse is a possible later size
  optimization, but is constrained by each slot's fixed `Plain`/`Boxed` storage
  kind (a slot could only be shared between same-kind bindings), so it is
  deliberately skipped for now; it is never a correctness concern.
- Captured/reassigned **parameters** are copied in the prologue from `Arg(i)`
  into a `Boxed` local (VM closure contract, point 6).

### Accepted divergences (initial)
- TDZ (temporal dead zone) is not enforced.
- Per-iteration loop bindings (`for (let i…)` where the body captures a fresh
  `i` each iteration) are **deferred, not precluded** by the slot model. The
  up-front single-slot `Alloc` reserves a stack *position*; it does not force
  shared bindings. Per-iteration freshness is a matter of allocating a fresh
  `Boxed` *cell* at each iteration boundary (copying the prior value in, per the
  spec's per-iteration environment) and having body-created closures capture
  that cell — the slot is unchanged. It only matters when a `let`/`const` loop
  variable is captured by a closure created in the body; for non-captured loop
  vars, function-wide single-slot is observationally identical. Until
  implemented, closures over a loop var share one binding (last-value
  semantics).
- **`arguments`** is supported: inside any frame it is an array of that frame's
  arguments (including those past the declared parameters), built lazily and
  cached per frame (`CallFrame::arguments_cache`, the `Instr::Arguments` op) so
  repeated references reuse one array. A real binding named `arguments` shadows
  it. **Divergence:** an arrow's `arguments` is its *own* frame's args, not the
  enclosing function's (JS arrows inherit `arguments` lexically); at the module
  top level it is the empty array (JS has no top-level `arguments`).
- **Builtin surplus args are ignored, not rejected** (JS semantics): a builtin
  consumes all `argc` arguments but reads only those up to its `meta()`
  `max_args`. This is what lets a builtin be used as a first-class callback
  (`arr.map(Math.sqrt)`); enforcement of the *lower* bound stays central in
  `Builtin::call`. (Direct `Namespace.method(...)` calls keep their strict
  compile-time arity check, so `Math.abs(1, 2)` is still a compile error.)
- `Math.max`/`Math.min` are variadic builtins (0..N args), matching JS spec.
- `f64::max`/`f64::min` semantics: a NaN operand is ignored (divergence from
  JS `Math.max`/`Math.min` which return NaN if any arg is NaN).
- `s.slice(start[, end])` operates on a half-open byte range and rejects
  negative indices (`ValueError`) instead of counting them from the end.
  `s.indexOf`/`includes`/`lastIndexOf` and `Number.parseInt` (incl. radix and
  `0x` prefix) otherwise follow JS semantics.

## Codegen correctness notes

- **Short-circuit `&&` / `||` / `??` / `?.` are branch-compiled, NOT the
  `And`/`Or` instructions.** `And`/`Or` pop *both* operands (already evaluated),
  so they do not short-circuit — using them for JS `cond && tools.x()` would run
  the tool unconditionally. `&&`/`||` lower with `Dup` + `JFalse`/`JTrue` +
  `Jump`; `??` and `?.` lower with the peeking `JNotNullish` (no `Dup`/`LooseEq`
  needed — it keeps the value on the not-nullish path and falls through to the
  short-circuit tail when nullish). `And`/`Or` are usable only when the RHS is
  provably side-effect-free.
- **Optional method calls (`recv?.method(args)`)** guard the receiver with the
  same `JNotNullish`: a nullish receiver short-circuits the whole call to
  `undefined` and the arguments are *not* evaluated (the guard sits between the
  receiver and the args). Per-link, consistent with `?.` member access.
- **Optional invocation (`callee?.(args)`)** lowers generically: evaluate the
  callee as a *value*, `JNotNullish`-guard it (nullish → `undefined`, args
  skipped), then `Dig(argc)` to put the callee back above its args and `CallDyn`.
  A non-nullish non-callable callee is a runtime `TypeError`, as in JS. This is
  the first compiler use of `CallDyn`; non-optional dynamic calls (`f(x)` on a
  value) still await user functions in Phase 3. The callable values that exist
  today are **first-class builtin references**: a namespaced builtin named but
  not called (`Math.sqrt`, `JSON.parse`) lowers to `Push(Value::Builtin)`
  via the shared `namespace_builtin` map, so `Math.max?.(a, b)` and
  `state.fn?.(x)` (after `state.fn = Math.sqrt`) both work end-to-end.
- **Assignment is an expression.** `a = b`, `obj.f = v`, `arr[i] = v`, compound
  `+=` etc., and logical-assignment `??=`/`&&=`/`||=` (short-circuiting) must
  leave the correct value on the stack (`Dup` before the storing op, which
  itself yields nothing). Common pattern: `state.x ??= []`.
- **`++`/`--` are numeric, not `+= 1`.** `Add` concatenates on strings
  (`"5" + 1` → `"51"`), but `"5"++` is `6`, so `++`/`--` must force `ToNumber`.
  `Sub` is numeric (`binary_num!`), so the general lowering is `<read lvalue> ·
  Push(∓1) · Sub · <store>` (`++` uses `Push(-1)`, i.e. `x − (−1)`); pre vs post
  is handled by `Dup` placement, and the store is the local / `ObjSet` /
  `IndexSet` sequence for that lvalue. Zero new instructions. A fused
  `IncLocal(idx, delta, mode)` (read slot, write ±1 in place, push old/new/none)
  is an optional later fast path for loop counters — there `mode` (Pre/Post)
  genuinely belongs in the instruction because it owns the lvalue; it does not
  generalize to `obj.foo++`/`arr[i]++`.
- **Function declarations are hoisted.** Their binding slot is initialized in the
  prologue (`Push(Fn)`/`MakeClosure` → `SetLocal`) before body code, so forward
  references and mutual recursion work.
- **Integer literals** lower to canonical `PosInt`/`NegInt`. Fold unary-minus on
  a numeric literal (`-5` → `NegInt(-5)`) at compile time; otherwise `Neg`
  promotes it to `Number(-5.0)` (functionally equal, but not canonical).
- **`undefined`/`NaN`/`Infinity`** are treated as literals (`Push(Undefined)` /
  `Push(Number(...))`), not lookups.

## Higher-order methods (`map`/`filter`/`reduce`/…) — prelude

> **Status: IMPLEMENTED (Phase 4.0).** Helpers live in `agent/src/prelude.rs`
> (`__map`/`__filter`/`__reduce`/`__reduce1`/`__forEach`/`__some`/`__every`/
> `__find`/`__findIndex`). They are appended to the user source (tree-shaken:
> only the helpers whose method appears are included, so HOF-free programs
> compile unchanged), hoisted like any top-level function, and called via a
> static `Call` from `compile_method_call`. A bare builtin callback
> (`arr.map(Math.sqrt)`) works: builtins ignore surplus args (flexible arity
> from `Builtin::meta`, enforced centrally in `Builtin::call`).

Higher-order array methods take callbacks and need loop-local state (index,
accumulator). **Decision: prelude.** Small helpers (`__map(arr, cb)`, …) are
written in JS, compiled by our own compiler, prepended once, and referenced by
label. The call site stays tiny (`eval arr · eval cb · Call(__fn, n)`); each
helper's loop temps live in its own frame, so there is no enclosing-prologue
temp-slot bookkeeping. The extra `Call` layer is irrelevant under the fuel
budget. The user callback is invoked per element via `CallDyn`. Loop temps in a
helper are *plain* locals (`reduce` passes the accumulator to the callback as an
argument, so it isn't captured).

Spread (`[...a]`, `{...o}`, `f(...args)`) and similar loop-shaped desugarings use
the same prelude mechanism. The prelude is compiled in the same unit as the user
program (so its helpers resolve to real labels) and is built in Phase 3+ — it
needs functions, closures, and loops working first; `sort` comes last.

Bootstrap note: the prelude is just more JS source compiled alongside the user
program, so there is no chicken-and-egg problem — `arr.map(cb)` lowers to a
`Call` of the `__map` label, resolved by the normal backpatch pass.

## Deferred built-ins (add as demand shows; compile-error meanwhile)

Additive later via new `Builtin` variants or prelude — not blocking:
- **String:** `toLowerCase`/`toUpperCase`, `replace`/`replaceAll`, `repeat`,
  `padStart`/`padEnd`, `substring`, `charAt`/`charCodeAt`, `at`.
- **Array:** `sort`, `reverse`, `splice`, `flat`, `concat`, `slice`,
  `indexOf`/`includes` (on arrays), `find`/`findIndex`, `entries`, `Array.from`/`of`.
- **Object:** `entries`, `assign`, `fromEntries`, spread `{...o}`.
- **Math/Number:** `trunc`/`log`/`exp`/`hypot`/trig/`PI`/`E`, `isNaN`/`isFinite`,
  `toFixed`.
- **Misc:** `JSON.stringify` pretty-print (indent arg); a `console.log` output
  channel (likely an `Invoke`/effect — decide when needed).

Now implemented as builtins (`CallBuiltin`): `arr.push`/`pop`/`shift`/`unshift`
(ret length/element), `arr.join`, `s.split`/`includes`/`startsWith`/`endsWith`/
`indexOf`/`lastIndexOf`/`slice`/`trim`, `Object.keys`/`values`,
`JSON.parse`/`stringify`, `Math.abs`/`sqrt`/`ceil`/`floor`/`round`/`sign`/
`min`/`max`/`pow`, `Number.isInteger`/`parseInt`/`parseFloat`, `Array.isArray`.

## Out of scope (intentional, informative errors)

`class`/`new`/`this`/`instanceof`, `async`/`await`/`Promise`, generators/`yield`,
`try`/`catch`/`throw`/`finally` (use `raise`), regex literals, `Map`/`Set`,
`Date` (use a tool), `BigInt`, `with`/`eval`, tagged templates, getters/setters,
spread/rest **in calls and params** (clashes with the VM's strict arity).
Destructuring and default parameters are **in** scope via desugaring (Phase 2/3).

## Phasing

0. **Skeleton:** parse, `Compiler`, `Program`/`Diagnostic` types, label
   allocator, backpatch pass, span table, error type, the VM changes
   (`PushStr`, `IndexGet`/`IndexSet`, `for_program`, remove `Read`/`Write`), and
   a VM round-trip test on a literal+arithmetic program (e.g. `1 + 2 * 3;`).
1. **Expressions:** literals (incl. strings via `PushStr`), all operators,
   arrays/objects, member get/set + computed `x[i]` (`IndexGet`/`IndexSet`),
   ternary, short-circuit `&&`/`||`/`??`/`?.` (branch-compiled), `typeof`,
   template literals (`PushStr` + `ToStr` + concat), intrinsics, `state.*`.
2. **Statements / control flow:** declarations + assignment / compound /
   `++`/`--` / logical-assignment, destructuring (desugar), `if`, `while`,
   `for`, `do/while`, `break`/`continue` (loop-context stack), blocks,
   expression-statement `Pop`.
3. **Functions / closures — DONE:** declarations (hoisted), expressions, arrows,
   params + defaults, `return`, capture analysis, `MakeClosure`, recursion.
   Pass 1 is now an authoritative span-keyed resolver (`analyzer.rs`); codegen
   (`compiler.rs`) keeps no scope state. **The prelude/stdlib mechanism
   (`map`/`filter`/`reduce`/…) slipped — it is now the first task of Phase 4.**
4. **Effects & remainder — DONE:** the prelude (`map`/`filter`/`reduce`/…,
   §4.0), `tools.*` → `Invoke` and `raise` → `Raise` (§4.1), `for-of` /
   `for-in` (§4.2), and `switch` (§4.3) all land. Deferred built-ins remain
   additive (§4.5). Unsupported nodes → informative errors throughout. See
   **Phase 4 — starting guide** below for the design notes and traps.

## Phase 4 — starting guide

Everything needed to begin Phase 4, in priority order. The recurring rule:
**Pass 1 (`analyzer.rs`) and Pass 2 (`compiler.rs`) must be extended in
lockstep.** Codegen resolves every binding/reference/function by source span via
the analyzer's tables and keeps no scope state of its own — so a construct that
codegen newly *supports* must also be *resolved* by the analyzer in the same
change, or its references resolve to "undeclared" and its bindings get no slot.

### 4.0 Prelude / higher-order methods (carried over from Phase 3) — do first
`map`/`filter`/`reduce`/`forEach`/… are **not implemented** (the `Builtin` enum
has only `push`/`pop`/`shift`/`unshift`/`join`; `arr.map(cb)` currently errors).
The prerequisites (functions, closures, loops) now all work, so the planned
approach is unblocked: write the helpers in JS (`__map(arr, cb)`, …), compile
them in the **same unit** as the user program (prepended once), and lower
`arr.map(cb)` to a `Call` of the helper's label. Call sites stay tiny; each
helper's loop temps live in its own frame. The user callback is invoked per
element via `CallDyn`. See **Higher-order methods — prelude** above for the
rationale. This is probably the highest-value capability for orchestration
flows (mapping/filtering over tool results), so land it before the rest.

### 4.1 `tools.*` → `Invoke`, `raise` → `Raise` (small; analyzer already ready)
The analyzer already handles `tools` and `raise` correctly: both flow through as
free/global names (dropped from `ref_resolution`), recognized **structurally**
in codegen. Phase 4 just replaces the two "until Phase 4" error stubs in
`compiler.rs` with emission:
- `tools.foo(a, b)` → `Invoke("foo", 2)` (bare `tools`, `tools.foo` without a
  call, and computed `tools[x](...)` stay errors — see Locked decision §3).
- `raise("...")` → `Raise(String)`; the argument **must** be a string literal
  (a non-literal argument is an error — §4). `raise(...)` is an expression: the
  host pushes the resumed value back on the stack.
The real substance is the host's **rewrite-from-top** resume loop (see "Replan /
rewrite model"), which is VM/host, not compiler — `compile()` is stateless per
call, so re-running from the top is free. VM support is partly exercised already
(`resume_after_invoke`, `raise_yields`).

### 4.2 `for-of` / `for-in` — the lockstep trap
The analyzer currently gives these only a **shallow** walk (body only —
`analyzer.rs`, the `ForOfStatement`/`ForInStatement` arms of `analyze_stmt`). It
does **not** walk the iterable expression nor declare the loop binding. This is
harmless *today* because codegen errors on them first, but turning codegen on
without upgrading the analyzer will produce exactly the "undeclared variable" /
missing-slot failures the span-keyed refactor was meant to kill. When
implementing, in the **same change**:
- analyzer: walk the iterable/object RHS expression (`analyze_expr`), and
  declare the loop binding — `let`/`const` → fresh block slot recorded in
  `binding_slot`; `var` → hoist via `analyze_hoist`/`hoist_var_pattern`.
- codegen: emit the iteration, binding the loop variable each step via
  `binding_slot[binding.span]`.
- `break`/`continue` work via the existing loop-context stack.
- **Per-iteration binding stays deferred:** a closure capturing a `for-of`
  `const x` shares one binding (last-value), same accepted divergence as the
  C-style `for`.
- `for (… of …)` and `for (… in …)` over `state` enumerate the blessed object's
  keys/values via the existing `Object` ops.

### 4.3 `switch` — needs a non-loop break target
- `break` in a `switch` jumps to the switch end; `continue` is **not** valid in
  a switch. The current `LoopCtx` stack models loops only — add a break-only
  context entry (or push a `LoopCtx` whose `continue_label` is guarded/unused)
  so `break` resolves but `continue` still escapes to the enclosing loop.
- Lexical `let`/`const` in `case` clauses share **one** block (the switch body):
  push a single block scope for the whole switch in the analyzer. (`analyze_stmt`
  already walks the discriminant, case tests, and consequents for references;
  only the block scope + binding slots need adding when codegen lands.)

### 4.4 Desugaring discipline (applies to prelude, spread, exotic destructuring)
All resolution is keyed on `span.start`, which assumes every binding/reference/
function maps to a **unique real source span**. The prelude is safe (it is real
JS text compiled alongside the user program). **Do not** hand-fabricate synthetic
AST nodes with zero/duplicated spans — they would collide in `binding_slot` /
`ref_resolution` / `scope_by_span`. Prefer "lower to source text + recompile"
(the prelude approach) over building AST nodes by hand.

### 4.5 Deferred built-ins — additive, non-blocking
Add `Builtin` variants on demand (the `meta()`/`call()` registry gives arity and
dispatch for free); compile-error until then. See **Deferred built-ins** above.

### 4.6 Testing
All current coverage is integration-style via `compile()` + the VM (the closure/
capture/shadowing tests in `compiler.rs` are the template). Keep adding edge
cases as for-of/switch/prelude land — especially capture/shadowing interactions
with the new binding forms. A few analyzer-level asserts are worth it only if a
bug is hard to trigger end-to-end.

## Dependencies

Add `oxc_ast`, `oxc_allocator`, `oxc_span` as direct deps in `agent/Cargo.toml`
(currently transitive via `oxc_parser`).

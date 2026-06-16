# Phase 13 — Object orientation: `this`, prototypes, `new`, `bind`

This phase reverses a recorded non-goal. `4_FUTURE.md` lists `this`/`class`/
`new`/prototype chains as **architectural** exclusions (not evidence-gated),
on the grounds that the data model — input, tool args, artifacts — is
JSON-shaped and methods/prototypes don't survive serialization. That reasoning
still holds for *data*: nothing here changes the JSON boundary (methods, bound
functions, and prototype links have **no JSON form**, exactly like `Closure`
and `RegExp` today). What changed is the evidence: LLM-authored glue code
reaches for `class`/`new`/`this` reflexively, and the existing "name the
alternative" diagnostics are friction, not a fix. This phase adds a *minimal,
in-program* object system that never crosses the JSON boundary.

One keystone makes the whole thing cohere: **`this`**. JS does not bind `this`
into a method value — `obj.m()` binds `this = obj` from the *call form*, while
`const f = obj.m; f()` has `this = undefined`. So a method is just a function,
and the only thing that welds a receiver to a function is `.bind()`. Model
`this` correctly and the rest (method dispatch, prototypes, `new`, method
values) falls out; get it wrong and you end up inventing auto-binding "method
objects" the VM has deliberately avoided.

**Already shipped, and load-bearing here:** builtin-method shadowing
(commit `4ea0249`). A method-builtin call on an `Object` receiver raises the
`ErrorKind::MethodOnObject` signal at the receiver check
(`VM::method_receiver_error`), intercepted at the two `b.call` sites and
re-routed by `VM::reroute_method_to_object` to the object's own same-named
property. That reroute **becomes the object-method dispatch arm** in Step 3 —
it is not throwaway. Do not duplicate it; generalize it.

## Ground rules

- Per feature: compile tests (rejection cases that now compile), behavioral
  tests in the Phase 2 harness, and a divergence-list update where semantics
  differ from JS (identity of bound/method values is the big one). Commit per
  step, `oo:` prefix.
- **Test harness (use these exact helpers).** `testutil::run_val(src) -> Value`
  runs a whole program and returns its `return` value; `testutil::run_ret(src)`
  returns JSON; `testutil::run_runtime_err(src) -> VMError` for error cases;
  `testutil::num(f)` builds a numeric `Value` (arithmetic yields `Float`, so
  `5+100` is `num(105.0)`, not `PosInt`). Object/method tests live in
  `interp/src/compiler/tests/objects_arrays.rs`; extend the existing
  `object_property_shadows_method_builtin` / `shadowing_is_uniform_across_receiver_types`
  there rather than starting a new file.
- **Finish a step before starting the next.** Each step's `Acceptance` box is a
  gate; do not begin step N+1 with step N's box unchecked. The steps are
  ordered by dependency, not preference.
- Every new instruction needs: a doc comment with its stack effect, a `step()`
  arm, an optimizer purity classification (walk each `pe_*` table — `Bound`/
  method dispatch is **not** pure: it can call user code), and a Phase 3 resume
  classification (`ResumeMode`).
- The JSON boundary is invariant. `Bound`, method-carrying values, and the
  prototype link have no JSON form; `stack_value_to_json` rejects them exactly
  as it does `Closure`/`Promise`/`RegExp`. Add the rejection + a test in the
  step that introduces each.
- Prototype chains are **`Object`-only**. Primitives, arrays, maps, sets, and
  regexps keep their structural builtin dispatch; only `Value::Object` carries
  a `proto` and walks a chain. This boundary is the reason the `Value` enum
  stays direct (see the String-needs-no-prototype reasoning that motivated it).

## The unified picture (read before the steps)

`this` lives at **slot 0 of a user frame, but only when the function uses it**
(its ABI class; Step 1). Class-P functions (no `this`) reserve no slot and are
byte-for-byte what they are today; class-T functions put `this` at slot 0 and
params at slot 1. The receiver is routed to wherever the *resolved* callee
expects it:

| resolved `m`                         | receiver delivered as | path                                      |
|--------------------------------------|-----------------------|-------------------------------------------|
| builtin (arr/str/map/set/regexp)     | arg 0 (today)         | structural `CallBuiltin`, unchanged       |
| object **own** property that's a fn  | slot 0 (if class T)   | keep recv, dispatch via provided-receiver |
| object **prototype-chain** fn        | slot 0 (if class T)   | same, found by walking `proto`            |
| not found / non-callable             | `TypeError`           | —                                         |

`f(args)` with no receiver → `this = undefined` (a class-T callee gets it from
frame setup; a class-P callee has no `this` to set). `f.bind(x)(args)` →
`this = x`, ignoring the call-site receiver. Builtins keep "receiver = arg 0";
class-T user functions read slot 0; the dispatch layer (a compile-time layout for
static calls, `EnterFrame` reconciliation for dynamic ones) is the only place
that knows the difference, so neither convention leaks to the surface language.

---

## Step 1 — `this` at slot 0, gated by ABI class (keystone)

`ThisExpression` currently hard-errors (`compiler/stmt.rs`/`expr.rs`,
"`this` is not supported"). The design (settled across the slot-0 / this-last /
frame-field evaluation — do not re-litigate) keeps the one property worth having
from slot-0 — **`this` is a genuine local, so reads and arrow-capture are
free** — while honoring the hard constraint that **non-OO code pays no extra
instruction**. Both come from making the `this` slot *conditional* on use.

**ABI class, per function, decided at compile time (a `uses_this` bit in
function metadata):**

- **Class P (plain)** — the body does not reference `this` and no nested arrow
  references `this`. Params/locals start at **slot 0**; there is **no `this`
  slot**. This is every function that exists today, so the convention move is a
  no-op for them (slot 0 = arg 0 = today).
- **Class T (this-bearing)** — the body references `this`, *or* a nested arrow
  does (lexical `this` is reified into this function's slot 0 so the arrow can
  capture it). `this` is **slot 0**; params/locals start at **slot 1**.
- **Arrows are always class P** — an arrow has no own `this`; `this` inside an
  arrow is the *enclosing* non-arrow function's `this`, captured lexically. So
  an arrow never reserves slot 0 for itself; it forces its nearest enclosing
  non-arrow function to class T.

Because the class is exactly "does this function use `this`," the slot-0
reservation is paid only where it's used. **Capture stays free:** `this` is
local 0 of a class-T function, so an arrow referencing it captures local 0
through the existing `MakeClosure`/`ClosureNew` + `Upval` path — no bespoke
capture, no `CallFrame.this_val` field. **Reads are `Instr::Local(0)`** — no new
instruction. These two are the entire reason to prefer slot-0 over a frame
field; the ABI class is what buys them back without taxing plain calls.

(Determining `uses_this` — own `this`-reference *or* nested-arrow
`this`-reference — must run in the analysis pass *before* slot allocation, since
it shifts params to slot 1. The capture pass already walks nested references;
fold the `this`-reference flag into it.)

The receiver/`this` funnels to **one place** (slot 0) for class-T user methods,
`new`, and `bind`; method builtins keep receiver = arg 0; class-P functions are
untouched. It is the most coupled change in the phase, so it splits into **1a**
(class machinery + dynamic ABI, no `this` reads, every function still class P →
suite green) and **1b** (mint class-T functions, read `this`). Land 1a green
first.

### Establishing the receiver — static vs dynamic

The receiver must reach slot 0 of a class-T frame (and be absent from a class-P
frame) without taxing plain calls.

**Static `Call` (callee known — the `call.rs` const-fn / prelude / non-capturing
named-fn sites).** The compiler knows the callee's class *and* the call shape, so
it emits the exact layout with no runtime reconciliation:

| call                      | layout emitted                                              |
|---------------------------|-------------------------------------------------------------|
| plain call, **P** callee  | push args only — **no `this`, zero tax** (most code)        |
| plain call, **T** callee  | push `undefined`, then args (rare: this-using fn, no recv)  |
| method call, **T** callee | push receiver, then args (receiver = slot 0)                |
| method call, **P** callee | evaluate receiver for effect, discard; push args            |

**Dynamic `CallDyn`/`CallSpread` (callee unknown — the reason it's dynamic).**
Two coordinated changes:

1. **Callee-first.** `CallDyn`/`CallSpread` expect the callee *below* the args
   (`top - argc - 1`), pushed before them. This removes the `Dig(argc)` in
   `compile_dynamic_method_call:521` and unifies both dynamic emit paths to
   *push callee → [push receiver] → push args → call*. (`CallBuiltin` is
   unaffected — its callee is baked into the instruction, nothing is on the
   stack.)
2. **`has_receiver` flag.** `CallDyn(ArgCount, has_receiver)` /
   `CallSpread(has_receiver)` carry one compile-time bit: was a receiver value
   pushed between callee and args (method-call site → `true`, plain → `false`).
   Encode it as a **field, not** a separate `CallMethodDyn`/`CallMethodSpread`
   opcode — the bit reaches the callee either way, a field avoids the 2×2 opcode
   explosion, `Instr` size is unaffected (the bool fits existing padding — verify
   against `EnterFrame`'s `ThinVec` variant), and the branch is per-site
   consistent. Do **not** encode it by pushing a sentinel value — that is a
   per-call push, the very tax we are removing.

`EnterFrame` reconciles its own class (a runtime fact it knows) against
`has_receiver` — the only place they can disagree and the only place a shift
occurs:

| dynamic site            | callee P          | callee T                       |
|-------------------------|-------------------|--------------------------------|
| `has_receiver` (method) | drop receiver     | receiver = slot 0 ✓            |
| no receiver (plain)     | nothing ✓         | insert `undefined` at slot 0   |

Diagonal cells are free; off-diagonal cells do a one-slot shift folded into the
arg-region normalization `EnterFrame` already performs. Both off-diagonal cases
are semantic mismatches (method-calling a non-`this` function; plainly calling a
`this`-using function) and rare. There is deliberately **no `PushThis`/pre-args
instruction**: `EnterFrame` already holds the callee, so a second inspection only
adds a dispatch to the common plain→P (arrow-callback) case it cannot help.

**Builtins unchanged.** Method builtins read receiver = arg 0 positionally; they
have no user frame and no slot 0. `arr.push(x)` pushes `[arr, x]`, `CallBuiltin`
reads `arg0 = arr` — exactly as today. Namespace builtins (`Math.max`) have no
receiver.

### Step 1a — class machinery + dynamic ABI (no behavior change)

Every function that exists today is class P (none reference `this`), so this step
**must not move any existing function's slots** — slot 0 stays arg 0. The new
machinery is dormant until 1b mints the first class-T function, and
`has_receiver` is plumbed but always `false` in 1a (no method call sets it yet).
This is what makes 1a low-risk — contrast the abandoned "shift every function to
slot 1" attempt, which changed every frame and crashed.

- **`compiler/analysis.rs`** — compute the `uses_this` bit (own `this` OR
  nested-arrow `this`) in the existing capture pass, *before* slot allocation.
  Class-P functions allocate params from slot 0 (unchanged); class-T from slot 1
  (none exist in 1a).
- **`compiler/call.rs`** — callee-first for both `CallDyn`/`CallSpread` paths:
  the dynamic slot-call path (`compile_user_call:695`) loads the callee *before*
  the args; the dynamic-method path (`compile_dynamic_method_call`) simply drops
  its `Dig` (the callee already sits below the args after `ObjGet`). Thread
  `has_receiver` = `false` everywhere in 1a. No `this` push on plain calls.
- **`vm/instr.rs`** — `CallDyn(ArgCount, bool)`, `CallSpread(bool)`; doc the
  stack effect (callee at `top - argc - 1`) and the `has_receiver` bit. Verify
  `Instr` size unchanged.
- **`vm/dispatch.rs` / `vm/methods.rs`** — `CallDyn`/`CallSpread` locate the
  callee below the args; `call_function`/`EnterFrame` carry the reconciliation
  table but, with every callee class P and `has_receiver` always `false`, only
  the "plain → P: nothing" cell is ever hit — i.e. today's behavior exactly.

Acceptance (1a):
- [ ] Full suite green, including the dynamic-method path (`state.add5(3)` where
      `add5` is a stored function) now that its `Dig` is gone: default params,
      `arguments`, destructured params, varargs, closures over params, method
      builtins (`arr.push`, `s.trim`), namespace builtins (`Math.max`).
- [ ] No `this`-reads anywhere; method/namespace builtins untouched.
- [ ] No `PushUndefined`/`PushThis`/sentinel added to plain-call lowering
      (codegen-shape test); `Dig` no longer emitted by
      `compile_dynamic_method_call` (codegen-shape test).
- [ ] `Instr` size unchanged by the added `bool` fields (size assertion).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

### Step 1b — class T + read `this`

- A function whose `uses_this` bit is set is class T: slot 0 = `this`, params
  from slot 1. `Local`/`SetLocal`, default params, the optimizer's
  index-relative analysis, and debuginfo's slot→name map ride the allocator's
  assignment (offset by one **for that function only**).
- `ThisExpression` → `Instr::Local(0)` (no new instruction; already classified
  in the `pe_*` tables and `ResumeMode` as a plain `Local`). Reads `undefined`
  until Step 3/4 supply a real receiver.
- Top-level `this` = `undefined` (the root frame is class T iff top-level code
  references `this`; slot 0 = `undefined`).
- An arrow referencing `this` forces its enclosing non-arrow function to class T
  (1a's analysis already set the bit) and captures local 0 via the existing
  `Upval` path — no new code. Becomes *observable* once Step 3 sets a
  non-undefined `this`; for now it captures `undefined`.

Acceptance (1b):
- [ ] `this` at top level / in a plain call is `undefined`, not a compile error.
- [ ] A class-T function with params reads its params correctly at slot 1
      (regression guard for the one-slot offset): a `this`-referencing function
      with two params returns them unchanged.
- [ ] An arrow referencing `this` compiles to an `Upval` capture of local 0
      (codegen-shape test), reading `undefined` at this step.
- [ ] A class-P function is byte-for-byte unchanged vs. pre-Phase-13 codegen.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 2 — object prototype representation

Change the object heap entry from a bare map to:

```rust
struct ObjData { proto: Option<ObjectPtr>, map: IndexMap<FieldName, Value> }
```

- `vm.objects: Vec<ObjData>`. Almost every touch point uses `.map`; the rest
  (the two init pushes, `ObjNew`, `ObjGet`, `ObjSet`, `ObjExtend`, the reads in
  `methods.rs`) are mechanical.
- `ObjGet` walks **own `map` → `proto` chain → `Undefined`** — but the walk
  **must short-circuit**, because `ObjGet` is the hot property-read path. Own-hit
  returns immediately; a `proto: None` object returns `Undefined` with a single
  predictable branch and **never enters the loop**. Only `proto: Some(_)` walks.
  Concretely, the overwhelmingly common plain object (`proto: None`) pays *zero*
  extra on a hit (just a `.map` field-access vs. today's bare `IndexMap`) and
  *one branch* on a miss — no regression. Do **not** write it as an
  unconditional `while let`; structure it own-first with the `None`
  short-circuit. Guard the `Some` walk against cycles (depth cap or visited
  check — a malformed chain must not hang the VM).
- `ObjSet`/`delete`/`in` operate on the **own** map only (JS `[[Set]]` creates
  an own property; `in` is own-or-proto — match JS: `in` walks, assignment does
  not).
- Plain object literals get `proto: None`. No behavior change until something
  sets a proto (Step 4).

Acceptance:
- [ ] Plain-object behavior byte-for-byte unchanged (alloc tests, existing
      object/array suite green).
- [ ] A manually proto-linked object resolves inherited reads and shadows them
      with own properties; `in` walks the chain, `delete`/assignment do not.
- [ ] A self-referential proto chain terminates (cap/visited), no hang.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 3 — unified method dispatch (generalize the reroute)

Make `recv.m(args)` bind `this` for user methods, across both call paths:

- **Builtin-named methods** (`recv.push(…)`): the existing
  `reroute_method_to_object` already has `[recv, args…]` on the stack and today
  *removes* `recv` before dispatching the object's own property. The change is to
  dispatch that property via Step 1's provided-receiver path (`has_receiver`),
  passing `recv` as the receiver instead of removing it: reconciliation then
  binds it as `this` (slot 0) for a class-T method or drops it for a class-P one.
  Shadowing becomes "call the user method with `this`", reusing the one
  reconciliation path rather than doing bespoke stack surgery.
- **Non-builtin method names** (`recv.greet(…)`): today this lowers to
  `ObjGet(greet)` + `CallDyn(has_receiver=false)`, which calls the property with
  no receiver. **Decision: reuse Step 1's dynamic ABI — no new *call* opcode.**
  Add a receiver-preserving *read*, `GetMethod(FieldName)`: it takes `recv` on
  top, resolves `name` on it (own map → proto chain, Step 2), and leaves
  `[callee, recv]` (callee-first) so the receiver survives as the `this` the
  property read would otherwise consume. `compile_method_call` then emits
  `recv` → `GetMethod(name)` → args → `CallDyn(argc, has_receiver=true)`. With
  Step 1's reconciliation a class-T `greet` gets `recv` at slot 0; a class-P
  `greet` ignores it (dropped) — the *same* reconciliation the builtin reroute
  rides. Classify `GetMethod`'s purity by its resolution only (a bounded proto
  walk, no user code) and give it a `ResumeMode`. (No parallel `this`-setting
  call opcode — the `has_receiver` bit is the one mechanism, shared by this path,
  the reroute, `new`, and `bind`.)
- `f(args)` (no receiver, `CallDyn`) stays `this = undefined`.

Acceptance:
- [ ] `obj.greet()` where `greet` is an own function property runs with
      `this === obj`; a prototype-chain `greet` likewise.
- [ ] An own property still shadows a builtin method name *and* now sees `this`
      (extend the Phase-`4ea0249` shadow tests to assert `this`).
- [ ] `const f = obj.greet; f()` runs with `this === undefined` (detachment).
- [ ] Method dispatch is classified impure in the `pe_*` tables (calls user
      code) and given a `ResumeMode`.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 4 — `new F(args)`

Constructors. Functions are not property-bearing (`Value::Fn`/`Closure` are a
code address / heap index), so a constructor's `.prototype` lives in a lazy
side table rather than on the value:

```rust
proto_of: HashMap<CodeAddr, ObjectPtr>   // F.prototype, created on first access
```

- `F.prototype` (read) → look up or lazily allocate the prototype object.
- `new F(args)` → alloc object `O` with `proto = proto_of[F]`; `call_function`
  with `this = O` and `args`; the result is `O` unless `F` returned an object
  (JS: a constructor returning a non-object is ignored).
- `ThisExpression` inside `F` now reads `O`. `this.x = …` writes own properties
  on `O` (Step 2's own-`ObjSet`).
- Shared methods: `F.prototype.m = function(){…}` puts `m` on the shared
  prototype; `new F().m()` finds it via Step 3's chain walk with `this` bound.

Update the `new` diagnostic: `new F(...)` for a user `F` now compiles; keep
rejecting `new Map()`/`new Set()`/`new Date()` with their existing
alternative-naming messages (those remain special-cased per `4_FUTURE`).

Acceptance:
- [ ] `function P(x){ this.x = x } new P(5).x === 5`.
- [ ] `P.prototype.get = function(){ return this.x }; new P(7).get() === 7`
      (shared prototype method, `this` bound to the instance).
- [ ] A constructor returning an object yields that object; returning a
      primitive yields the new instance.
- [ ] `new Map()`/`new Set()` still rejected with the alternative-naming
      diagnostic; the `Object`/`Error` constructors unaffected.
- [ ] Instances and prototypes have **no JSON form** (`JSON.stringify` of an
      instance serializes its own enumerable data only — confirm/define).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 5 — `bind` (the only receiver-carrying value), plus `call`/`apply`

`f.bind(obj)` is the explicit, on-demand way to weld a receiver to a function —
the JS-faithful replacement for the auto-"BoundMethod" idea, which we
explicitly rejected. JS `bind` is also **partial application**:
`f.bind(thisArg, a, b)` pre-pends `a, b` to whatever args the eventual call
supplies, so `BoundFn` carries a pre-bound args vector, not just a receiver.

```rust
Value::Bound(Rc<BoundFn>)
// BoundFn { this_val: Value, bound_args: ThinVec<Value>, callable: Value }
```

- **Immutable** `Rc`, inline like `RcStr`/`RcRegExp` (not an arena entry):
  bound functions are transient, and the arena never reclaims. The `Rc` graph
  is acyclic **by construction** — `BoundFn` has no interior mutability and
  references heap aggregates only by arena index (a `u32`), never by a strong
  `Rc`. The `bound_args` vector does **not** weaken this: it is fixed at bind
  time, and any `Rc`-bearing element (`RcStr`/`RcRegExp`/another `Bound`) is
  itself immutable and pre-existing, so no element can close a cycle back to
  this `BoundFn`. Cement the invariant in the type's doc comment; it is the
  entire safety argument. (`ThinVec` keeps the common `f.bind(obj)` case — empty
  `bound_args` — to a single pointer, no heap alloc.)
- `dispatch_call` gains a `Value::Bound` arm: dispatch `inner.callable` via
  Step 1's **provided-receiver path** — `inner.this_val` is supplied as the
  receiver (`has_receiver` semantics) and `inner.bound_args` are prepended to the
  call-site args. Because the receiver funnels to one place, this is **one path
  with no callable-kind fork**: a bound class-T user function gets `this_val` at
  slot 0; a bound *builtin* (`[].push.bind(arr)`) reads the same `this_val` as
  arg 0; a bound class-P function simply ignores it (dropped by reconciliation) —
  all automatic. The Bound arm *overrides* a call-site receiver (`obj.g()` where
  `g` is bound ignores `obj`), since it supplies the receiver itself; a method
  call (`GetMethod` + `CallDyn(has_receiver=true)`) that resolves to a `Bound`
  must therefore defer to the Bound's `this_val` rather than binding
  `this = recv`. Mechanic: splice `[this_val (as receiver), bound_args…]` below
  the call-site args (rare path; cost irrelevant). Binding is composable:
  `g = f.bind(a, x); g.bind(b, y)` pre-pends `x` then `y` and keeps the *first*
  `this` (JS: re-binding `this` is a no-op) — implement by flattening into a
  fresh `BoundFn` over `f`.
- **`Value::Bound` match-arm checklist.** Adding the variant makes the compiler
  flag every exhaustive `match` on `Value`. Most need a *specific* arm, not a
  catch-all — fill exactly these (the compiler will point at each; this is the
  full list so nothing is guessed):
  - `vm/value.rs` `is_truthy` → `true`.
  - `vm/value.rs` `to_number` → `None` (join the heap-types `None` arm).
  - `vm/value.rs` `strict_equal` → `(Bound(a), Bound(b)) => Rc::ptr_eq(&a.0,&b.0)`
    *before* the `_ => false` (so `f.bind(x) !== f.bind(x)`, identity only).
  - `vm/value.rs` `type_name` → `"function"` (this also drives `typeof`).
  - `vm/value.rs` `MapKey`'s `Hash` → a **new tag byte** + `std::ptr::hash`
    on the `Rc` pointer (mirror the `RegExp` arm).
  - `vm/methods.rs` `write_js_string` → `"function () { [native code] }"`
    (same as `Fn`/`Builtin`/`Closure`).
  - `vm/methods.rs` `stack_value_to_json` → reject, exactly like `Closure`.
  - Arms with a `_` catch-all (`compare`, `loose_equal`, `is_string`,
    `as_f64`/`as_i64`/`is_number`, `str_byte_len`) need **no** edit — verify,
    don't add.
  - `Bound`'s payload is `Rc<BoundFn>` (a thin pointer), so `Value` stays
    **16 bytes** — do not let it widen (no inline `BoundFn`).

**`.call` / `.apply` (eager siblings of `bind`).** Same `this`-threading,
*no new value type* — they invoke immediately rather than producing a value, so
they are strictly cheaper than `bind`:

- `f.call(thisArg, a, b)` → `dispatch_call(f, this = thisArg, [a, b])`.
- `f.apply(thisArg, argsArray)` → spread `argsArray` (reuse `CallSpread`), then
  the same. A non-array (and non-nullish) `argsArray` is a `TypeError`.
- Register both as `Method`-kind builtins whose receiver is callable
  (`Fn`/`Closure`/`Builtin`/`Bound`); a non-callable receiver is a `TypeError`.
- Note the `this`-free use of `.apply` (variadic spread, `f.apply(null, xs)`)
  is **already** covered by `f(...xs)` (Phase 6 `CallSpread`); these earn their
  keep only once `this` exists.

Acceptance:
- [ ] `const g = obj.greet.bind(obj); g() === obj.greet()` (carried `this`).
- [ ] Partial application: `const add = (a, b) => a + b; add.bind(null, 2)(3)
      === 5`; pre-bound args precede call-site args.
- [ ] A bound function ignores a later call-site receiver
      (`other.h = g; other.h()` still uses the bound `this`).
- [ ] Re-binding composes args and keeps the first `this`
      (`f.bind(a, x).bind(b, y)` → `this = a`, args `x, y, …`).
- [ ] `typeof`/equality/hash/JSON-rejection covered.
- [ ] `f.call(t, a)` and `f.apply(t, [a])` both run `f` with `this === t`;
      `.apply` with a non-array second arg is a `TypeError`.
- [ ] `.call`/`.apply` on a non-callable receiver is a `TypeError`.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 6 — method values off a receiver (`[].push`, `obj.m` uncalled)

With Steps 3 and 5 in place, reading a method *as a value* becomes coherent.
`recv.m` (uncalled) where `m` is a builtin-method name currently errors at the
`ObjGet` array/string fallthrough.

- For an **`Object`** receiver: `ObjGet` already returns the own/proto property
  (a plain function, this-less) — unchanged.
- For an **array/string/map/set/regexp** receiver and a method name valid for
  that type: yield the plain `Value::Builtin` (a this-less function value),
  resolved by a receiver-type-aware `(type, name) → Builtin` lookup. Calling it
  bare gives `this = undefined`/missing receiver (→ the builtin's own
  `TypeError`); the user reaches for `.bind(recv)` to make it callable — exactly
  JS.
- The compiler emits a method-aware read instruction (e.g.
  `GetMethodOrProp(builtin, name)`) only when `Builtin::for_method(name)` is
  `Some`, so `obj.has` reading a *data* property named `has` is **not**
  regressed (the `Object` branch still reads the property).

This step is genuinely optional and orthogonal to the rest; defer it unless
real programs pass builtin methods as callbacks. Captured here so it lands on
the same `this`/`bind` substrate rather than as a one-off.

**`fn.length` (arity).** Once functions are routinely read as values, `.length`
on a callable should return its **expected parameter count** (JS `fn.length`),
not error. Extend `Instr::GetLength`'s polymorphic dispatch with a callable arm:

- `Fn`/`Closure` → declared param count *before the first default/rest param*
  (JS semantics). The compiler knows this; expose it via a `CodeAddr → arity`
  lookup (a slot in the existing function metadata / debug table, not a new
  heap).
- `Builtin` → derive from `meta()`: `min_args` minus 1 for `Method`-kind (the
  receiver isn't a declared param), `min_args` for `Namespace`.
- `Bound` → `max(0, target_arity - bound_args.len())` (a bound function's arity
  drops by the number of pre-bound args, clamped at 0).

`GetLength` already has the right shape for this — it's the single polymorphic
`.length` site — so this is one more match arm, not a new mechanism, and it
keeps `.length` off the call path (it's a property, not a method; see the
GetLength-vs-builtin rationale).

Acceptance:
- [ ] `const f = [].push.bind(arr); f(3)` pushes to `arr`.
- [ ] `obj.has` where `obj` has a data property `has` still reads the data
      (no regression).
- [ ] `[].push` (bare) is a `"function"` value, not a read error.
- [ ] `((a, b) => a + b).length === 2`; a default/rest param stops the count
      (`((a, b = 1) => 0).length === 1`); `f.bind(null, x).length` is
      `f.length - 1` (clamped at 0).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 7 — `class` sugar

No new runtime concepts. **Direct codegen, not AST rewriting:** add a
`compile_class` that walks the `ClassDeclaration`/`ClassExpression` node and
*emits* the same instruction sequence the hand-written `function C(){…};
C.prototype.m = function(){…}` form compiles to — by reusing the existing
codegen helpers (`compile_function` for the constructor and method bodies, plus
Step 4's prototype-assignment path). The two forms **converge at the bytecode
level**; nothing materializes an intermediate ES5 AST.

This matches the established idiom — every sugar here is direct emission (spread
→ `ArrExtend`, optional chaining → branches, the `new` special-forms
`compile_map_ctor`/`compile_regexp_ctor`/… in `expr.rs`) — and avoids
synthesizing arena-bound oxc nodes (lifetimes + fabricated spans), keeping the
class node's real spans for diagnostics. (`class` is currently `self.error(…)`
at `stmt.rs:128`; replace that arm.)

- **Constructor + methods:** `compile_function` for the constructor body
  (becomes `C`); each method → `ClosureNew` + an `ObjSet` onto `C.prototype`.
- **Fields** (`x = 1`): emit `this.x = 1` into the *front* of the constructor's
  instruction stream, in field order (no synthetic AST — just prepend the
  store sequence).
- **`extends` / `super`:** set `C.prototype`'s proto to the parent prototype;
  `super(...)`/`super.m(...)` emit a *direct* parent-constructor / parent-proto
  lookup (these have no clean ES5-AST twin, so direct codegen is the natural
  form, not a fallback).
- **`static` / getters / setters:** **reject in the MVP** with an
  alternative-naming diagnostic (do not implement them this phase). This keeps
  the surface small; revisit on evidence.

Acceptance:
- [ ] A `class` with a constructor and a method compiles to bytecode equivalent
      to (and behaves identically to) its hand-written `function`+`prototype`
      form — assert via a shared test body run both ways.
- [ ] `extends` + `super(...)` + `super.m()` resolve to the parent.
- [ ] Diagnostics carry the original class-node spans (not fabricated ones).
- [ ] Rejected sugar (whatever the MVP omits) has an alternative-naming
      diagnostic, not a parser panic.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

---

## Sequencing

**1a → 1b** is a hard order. 1a is now the *low-risk* half — every function is
class P, so slot 0 = arg 0 and no frame layout moves; its only observable change
is callee-first (the `Dig` removal), which is behavior-preserving. The risk moved
to **1b** (class-T params shift to slot 1) and **3** (receiver plumbing through
`GetMethod` + `has_receiver`), so keep each behind its acceptance gate. 1b and 2
are independent and can land in either order, but both precede 3. 3 depends on
1b+2. 4 depends on 3. 5 depends on 3. 6 depends on 3+5. 7 depends on 4 (and 5 if
methods-as-values appear in class bodies). The **minimum coherent system** is
Steps 1–4; 5–7 are the deferred items folded into the same substrate so they
never become one-off bolt-ons.

## Divergences from JS (record in the divergence list as they land)

- **Identity of method/bound values.** `f.bind(x) !== f.bind(x)`; a bare method
  read (`[].push`) mints a fresh value, so `arr.push !== arr.push`. JS shares
  the prototype function. Accepted.
- **No `[[Set]]` traps / accessors.** Own-property assignment only; no
  getters/setters in the MVP.
- **`ToPrimitive` on objects stays unperformed** (the existing divergence):
  arithmetic/`==` against a plain or constructed object is still a `TypeError`,
  not a `toString`/`valueOf` coercion.
- **JSON boundary unchanged.** Instances serialize own enumerable data only;
  `Bound`, prototype links, and methods have no JSON form.

## Non-goals retained

Getters/setters, generators, `Symbol`/well-known symbols, `Proxy`/`Reflect`,
property descriptors/`Object.defineProperty`, UTF-16 string semantics. `Map`/
`Set` remain the transient-only special-cased values from `4_FUTURE` — `new F()`
here is for *user* constructors, not a general `new` over builtins.

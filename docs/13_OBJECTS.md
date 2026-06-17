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

`this` lives in a **per-frame field, `CallFrame.this_val`**, outside the
local-slot space (Step 1). The call machinery sets it; the bytecode and slot
layout of every call are otherwise **unchanged from today**. The receiver is
routed to wherever the *resolved* callee reads `this`:

| resolved `m`                         | receiver delivered as | path                                  |
|--------------------------------------|-----------------------|---------------------------------------|
| builtin (arr/str/map/set/regexp)     | arg 0 (today)         | structural `CallBuiltin`, unchanged   |
| object **own** property that's a fn  | `this_val` field      | `ObjPeek` + `has_this` call → `this_val = recv` |
| object **prototype-chain** fn        | `this_val` field      | same, found by walking `proto`        |
| not found / non-callable             | `TypeError`           | —                                     |

`f(args)` with no receiver → `this_val = undefined` (the frame default).
`f.bind(x)(args)` → `this_val = x`, ignoring the call-site receiver. The split is
the cost of this representation: a **builtin reads its receiver as arg 0 on the
stack**, a **user function reads `this_val` off the frame**. Neither convention
leaks to the surface language.

**One chokepoint (the generality invariant).** `dispatch_call`
(`methods.rs:1184`) already forks on callable kind (`Fn`/`Closure`/`Builtin`,
plus `Bound` in Step 5) and is where the reroute funnels back (`:1171`). Thread
**`this_val: Value` through `dispatch_call` and `call_function`** and that fork
becomes the *sole* place the receiver routing lives:

```
dispatch_call(callable, this_val, nargs)
  Fn/Closure → call_function(addr, nargs, upvals, this_val)  // → frame field
  Builtin    → splice this_val as arg 0, then b.call
  Bound      → dispatch_call(inner.callable, inner.this_val, …)
```

Every call form then reduces to *compute `this_val`, dispatch once*: plain call →
`Undefined`; `has_this` method call → the popped receiver; the shadow reroute →
`recv`; `new` → the fresh instance; `bind` → `BoundFn.this_val`; `.call`/`.apply`
→ `thisArg`. The user-fn-vs-builtin distinction thus exists in **exactly one
function**, not re-derived per call site. Implementing `this` is then "the JS
Reference rule": `this_val` = the base of the callee's member reference,
`Undefined` when it has none.

**Args are uniform too.** `dispatch_call` consumes the **top `nargs` stack
values** as the argument list, so each call form's only job is to compute
`(callable, this_val)` and leave its final args as that top-`N` region — using at
most one shared transform: the **array-expansion helper shared by `CallSpread`
and `.apply`** (a Rust fn, not the bytecode instruction), or `bind`'s **prepend**
(insert below the current top-`n` region, so it composes with an already-spread
region as in `g(...xs)`). No form re-implements spreading or invents its own
argc/return discipline.

`dispatch_call` is the sole site of the *runtime* kind fork — for a callable
**value** of unknown kind. **Compile-time-resolved calls deliberately bypass it,
and should**: static `Call`/`new` go straight to `call_function` (kind is a
known user `Fn`), and `CallBuiltin` / namespace calls go straight to `b.call`
(kind is a known builtin, receiver at arg 0). These aren't exceptions to "one
fork site" — they're the "kind already known, nothing to fork" fast paths.
`CallBuiltin` in particular stays **essential, not just an optimization**:
builtins aren't stored properties, so `ObjPeek`/`ObjGet` can't reach them; the
compiler resolving `arr.push`/`Math.max` by name to a `Builtin` is the *only*
path to a builtin method. (`dispatch_call` keeps a `Builtin` arm only for the
rarer builtin-arrived-as-a-value case — a bound builtin, or `[].push` read as a
value in Step 6.)

---

## Step 1 — `this` as a frame field (keystone)

`ThisExpression` currently hard-errors (`compiler/stmt.rs`/`expr.rs`,
"`this` is not supported"). The design (settled across the slot-0 / this-last /
frame-field evaluation — do not re-litigate) stores `this` in a **new
`CallFrame` field**, outside the capturable local-slot space:

```rust
struct CallFrame {
    // … existing fields …
    this_val: Value,   // NEW — defaults to Undefined
}
```

The entire appeal is that **the frame model is unchanged and there is no ABI
split**: `call_function` writes `this_val = Undefined` as one unconditional store
during frame construction — no `PushUndefined`, no slot move, no `EnterFrame`
reconciliation, no per-function ABI class. Params stay at slot 0 for *every*
function; the frame just carries one more `Value` field. (Method calls do adopt a
callee-first operand order and a `has_this` routing bit on `CallDyn`/`CallSpread`
— Step 3 — but that is operand-stack arrangement plus a dispatch-time fork, **not**
a change to how the frame itself is built. The plain-call frame is untouched.)

The price — paid entirely by OO code — is twofold, and both are spelled out
below: reads need a dedicated instruction (`LoadThis`), and arrow `this`-capture
needs a bespoke **reify-on-capture** path, because `this_val` is not a local slot
the `Upval` machinery can reach.

It splits into **1a** (the field + direct `this` reads) and **1b** (reify so
arrows capture lexical `this`). The reify path is the novel, risky part, so it is
isolated in 1b and must land before Step 3 (which sets the first non-undefined
`this`).

### Reading `this`

- A new `Instr::LoadThis` pushes `self.frames.last().this_val.clone()`. It is the
  only way to read `this`; emitted only in functions that lexically reference it.
  New instruction → give it a doc comment with its stack effect (`-> any`), a
  `step()` arm, a `pe_*` classification (**pure** — reads a frame field, runs no
  user code; classify with the `Local` family: frame-relative but
  side-effect-free), and a `ResumeMode` (same as `Local`).
- **Method dispatch / `new` / `bind` set the field, not the stack** (Steps
  3/4/5): the method path (`ObjPeek` + a `has_this` call, Step 3) sets
  `this_val = recv` for a user-fn callee; `new` sets it to the fresh instance; a
  `Bound` user-fn call sets it from `BoundFn.this_val`. Plain `Call`/`CallDyn`
  leave the `Undefined` default.
- **Builtins are untouched.** A method builtin reads its receiver as arg 0 on the
  stack exactly as today; it has no frame and never touches `this_val`.

### The capturing story (the part that earns its own section)

Arrow functions have no own `this`; an arrow's `this` is the *lexical* (enclosing
non-arrow function's) `this`. In a slot-0 design `this` is a real local, so an
arrow captures it for free through the existing `Upval`-by-slot machinery. Here
`this_val` is a **frame field, not a local slot**, so `Upval` (which captures
cells that back local slots) cannot reach it. Capture therefore needs a bridge —
**reify-on-capture**.

This is not a novel mechanism: it is the classic `var self = this;` idiom, and
*precisely* Babel's arrow-function lowering — emit `var _this = this` once at
function entry, rewrite each `this` inside the arrow to `_this`, and let ordinary
closure capture take `_this`. We do exactly that, only (a) by the compiler, (b)
*conditionally* — solely when a nested arrow needs it (a direct `this` use stays a
bare `LoadThis`, no slot) — and (c) into a hidden synthetic slot (no name to
shadow). So the *concept* is battle-tested at industrial scale; the only work
here is detecting the condition and threading the synthetic binding through the
existing capture pass.

When analysis finds that a function has a **nested arrow that references `this`**,
the compiler reifies `this` into a synthetic local of that (nearest enclosing
non-arrow) function:

1. Allocate a hidden local slot, `this_slot`, in the enclosing function.
2. Emit a prologue `LoadThis; SetLocal(this_slot)` at function entry — copy the
   frame field into the slot once.
3. Mark `this_slot` **captured**, so it is boxed into a `cell` like any captured
   local; arrows capture it through the **standard `MakeClosure`/`ClosureNew` +
   `Upval` path** — no new closure machinery.
4. Inside such an arrow, a `this` reference compiles to the captured-cell read
   (`GetUpval`/the existing captured-`Local` path) — **not** `LoadThis` (an arrow
   has no own `this_val` worth reading).

Properties to get right:
- The reify target is the **nearest enclosing non-arrow function**; intermediate
  arrows just pass the binding through, so arrow-within-arrow rides the existing
  transitive-capture logic with no special case.
- An arrow referencing `this` at module top level reifies into the **root frame**
  (whose `this_val` is `Undefined`) — same path, yielding `undefined`.
- A function that references `this` *directly* (not via a nested arrow) needs
  **no reify** — it just emits `LoadThis` at the use site. Reify is only for
  functions whose *arrows* need the lexical `this`.

The cost is two prologue instructions, emitted only in functions whose nested
arrows reference `this` — and it funnels arrow capture back onto the one capture
path that already exists, rather than adding a second capture kind (capturing a
frame field directly). That trade is the entire reason to accept a bespoke path
here.

### Step 1a — the field + direct `this` reads

Additive and behavior-preserving: nothing sets a non-undefined `this` yet, so
every read is `undefined`. No slot layout and no call convention change.

- **`vm/mod.rs`** — add `CallFrame.this_val: Value`. **Every `CallFrame {…}`
  literal must initialize it**; the compiler will flag each. The sites are the
  root frame (`methods.rs:26`, `callstack: vec![CallFrame{…}]`), `call_function`'s
  frame push (`methods.rs:1228`), the continuation/strand frame (`methods.rs:658`),
  and any in `dispatch.rs`. Default `Undefined` everywhere; only the method /
  `new` / `bind` paths (Steps 3/4/5) write a non-undefined value.
- **`vm/methods.rs` / `vm/dispatch.rs`** — `Instr::LoadThis` reads
  `self.callstack.last().this_val`. **No empty-frame case to handle:** the VM is
  constructed with the root frame already on `callstack` (`methods.rs:26`), and
  the callstack is empty only *after* the root returns (`dispatch.rs:357`), when
  no instruction runs — so `LoadThis` always has a frame. Top-level `this` is the
  root frame's `Undefined`.
- **`vm/instr.rs` / `vm/dispatch.rs`** — `Instr::LoadThis` + its `step()` arm,
  `pe_*` entry, and `ResumeMode`.
- **`compiler/stmt.rs`/`expr.rs`** — `ThisExpression` lowers to `LoadThis`
  (replace the hard-error arm). In 1a this is correct for direct `this`; an arrow
  referencing `this` *also* lowers to `LoadThis` here and reads its own frame's
  `this_val` — which is **coincidentally** `undefined` (the right lexical answer)
  only because nothing sets a real `this` yet. 1b fixes the wiring before that
  coincidence breaks (Step 3).

Acceptance (1a):
- [ ] `this` at top level / in a plain call / in a method (pre-Step-3) is
      `undefined`, not a compile error.
- [ ] Full suite green; no slot layout changed, no call convention changed
      (default params, `arguments`, destructured params, varargs, closures,
      method/namespace builtins all unaffected — this is the no-op guarantee).
- [ ] `LoadThis` classified in the `pe_*` tables and `ResumeMode`.
- [ ] `CallFrame` size delta recorded (one `Value` = 16 bytes added per frame).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

### Step 1b — reify-on-capture (arrows capture lexical `this`)

Lexical `this` rides the **existing name-keyed capture machinery** — `analyzer.rs`
scopes carry `names` / `free` / `captures`, capture propagates bottom-up by free
*name*, and each function scope is a binding boundary — with two additions:

- **`is_arrow: bool` on `Scope`.** The analyzer already distinguishes
  `ArrowFunctionExpression` from `FunctionExpression` at scope creation
  (`analyzer.rs:1590`/`1846`); record the flag there.
- **A synthetic binding name for `this`** — a reserved sentinel that cannot
  collide with a user identifier (e.g. `"<this>"`; `<`/`>` are illegal in JS
  identifiers). It flows through `free`/`captures` like any name, with the **one**
  difference being the boundary:
  - a `ThisExpression` in scope `S` contributes `"<this>"` to `S.free`;
  - `"<this>"` is treated as **declared by every non-arrow scope (and the root),
    never by an arrow scope.** So `this` used *directly* in a non-arrow function
    resolves to that function's own synthetic binding → **not captured** → emits
    `LoadThis`. `this` used inside one or more nested arrows passes *through* the
    arrow scopes (they don't declare it) and resolves to the **nearest enclosing
    non-arrow** scope → **captured**, flowing down as an upval exactly like a
    captured `let`, via the existing `MakeClosure`/`ClosureNew` + `Upval` path.

  That boundary rule is the *only* deviation from normal name resolution (normal
  names stop at any declaring scope; `"<this>"` stops only at non-arrow scopes —
  arrows are transparent to it).

**Reify** is then exactly "a non-arrow scope has `"<this>"` in its captured set":

- **`compiler/analysis.rs`** — allocate a synthetic own-local `this_slot` in that
  scope and mark it captured, so the existing rule boxes it (captured slots are
  `Boxed`; include `this_slot` in the scope's `SlotKind` list so `EnterFrame`
  allocates its cell). Map the scope's `"<this>"` references to `this_slot`.
- **`compiler/function.rs`** — prepend `LoadThis; SetLocal(this_slot)` to the
  body, emitted **after** `EnterFrame` (the cell must exist) and before user code.
  `SetLocal` into a `Boxed` slot writes the cell, exactly like storing a captured
  `let`.
- **`compiler/expr.rs`** — `ThisExpression` lowering forks on the resolution
  above: direct/own-scope → `LoadThis`; captured → the existing captured-binding
  read (`ref_slot`/`emit_slot_read`, which reads the installed upval local).

The root scope is non-arrow, so a top-level arrow capturing `this` reifies the
root's `this_slot` from its `Undefined` `this_val`. Still reads `undefined` until
Step 3, but the wiring is in place and testable now.

Acceptance (1b):
- [ ] An arrow referencing `this` reads via the captured-binding path (an
      installed upval local) of the **nearest non-arrow** enclosing scope's
      reified slot — **not** a `LoadThis` in the arrow body (codegen-shape).
- [ ] A non-arrow function that references `this` directly, with no
      this-capturing nested arrow, emits **no** reify prologue and reads via
      `LoadThis` (codegen-shape).
- [ ] Arrow-within-arrow over `this` resolves transitively through the inner
      arrow to the nearest non-arrow owner, reading `undefined` at this step.
- [ ] A top-level arrow referencing `this` reads `undefined` (root reify).
- [ ] A function with no `this` (neither direct nor via a nested arrow) is
      byte-for-byte unchanged vs. pre-Phase-13 codegen.
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

Make `recv.m(args)` bind `this` for user methods. It splits into **3a** (the
callee-below-args convention move — behavior-preserving, suite green) and **3b**
(the `this`-binding semantics on top), mirroring 1a/1b: isolate the mechanical
stack-layout change from the new semantics so a suite break localizes cleanly.

### Step 3a — callee-below-args convention (no behavior change)

Today `CallDyn`/`CallSpread` expect the callee on *top* of the args, so the
dynamic-method path (`compile_dynamic_method_call`) emits `Dig(argc)` to lift the
callee back over the args. Flip the convention — callee *below* the args — and
add a `has_this` bool that is **always `false`** in 3a:

- **`vm/instr.rs`** — `CallDyn(ArgCount, has_this)`, `CallSpread(has_this)`; doc
  the layout (callee at depth `argc`, with `recv` at depth `argc+1` once
  `has_this` is used in 3b). Verify `Instr` size unchanged by the bool.
- **`vm/dispatch.rs` / `vm/methods.rs`** — `CallDyn`/`CallSpread` locate the callee
  *below* the args. With `has_this = false` there is no receiver to route, so
  dispatch is identical to today (no `dispatch_call` signature change yet).
- **`compiler/call.rs`** — push the callee *before* the args at every emit site:
  the `compile_user_call` dynamic path loads the callee first; the dynamic-method
  path **drops its `Dig`** (the callee already sits below the args after
  `ObjGet`). Thread `has_this = false`.

Behavior-preserving — same callee, same args, same dispatch, just no `Dig`. This
is the riskiest mechanical change in the step, isolated as its own green
checkpoint.

Acceptance (3a):
- [ ] Full suite green, incl. the dynamic-method path (`state.add5(3)` with
      `add5` a stored function) now that its `Dig` is gone.
- [ ] `Dig` no longer emitted by `compile_dynamic_method_call` (codegen-shape).
- [ ] No `this` bound anywhere (`has_this` always `false`; `dispatch_call`
      unchanged).
- [ ] `Instr` size unchanged by the added `has_this` bool.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

### Step 3b — method `this`-binding (the semantics)

3b sets `has_this = true` on the method paths and threads `this_val` through
`dispatch_call`/`call_function` (the chokepoint from the unified picture) so the
popped receiver reaches the callee.

**How the keep-receiver form is selected** — purely by syntactic position, no new
logic. `compile_call` already matches on `call.callee` (`call.rs:73`): a
`StaticMemberExpression` / `ComputedMemberExpression` callee routes to the
method path (emit `ObjPeek`/`ObjPeekDyn` + `has_this=true`); an `Identifier` or
any other callee routes to a plain call (`this = undefined`). A member expression
that is *not* a call's callee (`const f = obj.greet;`, an argument, …) goes
through `compile_expr` → plain `ObjGet`, **consuming** the receiver — which is
exactly what makes `const f = obj.greet; f()` detach to `this = undefined`. So the
read never decides on its own; the call site decides, because only it knows the
result is about to be invoked with the receiver. (Edge: a *parenthesized*
member callee `(obj.m)()` still binds `this` in JS — ensure the match sees
through a `ParenthesizedExpression`, or rely on oxc having stripped it.)

- **Builtin-named methods** (`recv.push(…)`): the existing
  `reroute_method_to_object` already has `[recv, args…]` on the stack and today
  *removes* `recv` before dispatching the object's own property. Under
  frame-field the reroute **forks on the shadowing property's kind**: a user
  function/closure is dispatched with `this_val = recv` and `recv` removed from
  the arg region (it is not an arg to a user method); if it resolves back to the
  builtin, `recv` stays as arg 0 exactly as today. Shadowing becomes "call the
  user method with `this`".
- **Non-builtin method names** (`recv.greet(…)`): today this lowers to
  `ObjGet(greet)` + `CallDyn`, calling the property with no receiver. **Decision:
  fuse the receiver-keeping read into one instruction `ObjPeek(name)` and set the
  3a `has_this` bit `true`.** No `CallMethod`/`CallMethodSpread` opcodes, and
  spread/optional fall out for free.
  - **`ObjPeek(FieldName)` = `Pick(0); ObjGet` fused** (`obj -> obj, any`): read
    property `name` off the receiver but **keep the receiver** below the result
    rather than consuming it. Semantically *identical* to `ObjGet` — same
    own → proto walk (Step 2), same `Undefined` on a miss, same non-`Object` read
    error — so it **shares `ObjGet`'s resolution helper** (it is `ObjGet` minus
    the `pop`) and inherits its `pe_*`/`ResumeMode` classification; a fusion, not
    a second resolver. This matches the codebase's existing `Pick(0); SetLocal`
    fusion (`instr.rs:151`), and the *same* pattern is the compound-assignment
    load (`obj.x += 1` does `Pick(0); ObjGet`), so `ObjPeek` earns its keep on two
    hot paths. Because resolution stays `ObjGet`'s, a missing `greet` yields
    `Undefined` and the *call* raises "not a function" — JS-faithful (JS errors at
    the call, not the read).
  - **`ObjPeekDyn` = `ObjGetDyn` minus the obj-pop** (`obj, key -> obj, value`):
    the dynamic-key counterpart, for **computed** method calls `recv[expr](…)`.
    It consumes the key, keeps the receiver, pushes the property — shares
    `ObjGetDyn`'s helper/classification, same fusion rationale as `ObjPeek`.
    **This lifts a current hard error:** `call.rs:106` rejects computed method
    calls outright ("`computed method calls (obj[expr](...)` are not supported");
    replace that arm. Computed method calls are real method calls and **must bind
    `this`** — `has_this = true`, the same as the static form.
  - **The `has_this` fork** (the bit added in 3a, now exercised). Because the args
    are pushed last, they are already the contiguous top-`N` region = the frame
    (`fp = len - argc`, **no shift**); the callee sits at `fp-1` and the receiver
    at `fp-2`, both *below* `fp` as caller-pushed dead space. So the receiver is
    **read in place** (`this_val = stack[fp-2]`), not popped — the args never
    move. On return, teardown truncates to the bottom of the call group (`fp-2`
    for `has_this`, else `fp-1`) and pushes the result; `has_this` only changes
    that count, never a shift. Routing then happens at the **one chokepoint** (see
    the unified picture): a **user function/closure** gets `this_val` in its frame
    field (recv is not an `arguments` entry); a **builtin** gets it spliced as
    arg 0; a **`Bound`** defers to its own `this_val` (Step 5). A dispatch-time
    routing fork only — **no `EnterFrame` reconciliation, no slot move**; the
    frame model is untouched, which is the point of frame-field.
  - Lowerings: `recv.greet(a,b)` → `ObjPeek(greet)` + `CallDyn(2, true)`;
    `recv.greet(...xs)` → `ObjPeek(greet)` + `CallSpread(true)`;
    `recv[k](a)` → `<recv>; <k>; ObjPeekDyn` + `CallDyn(1, true)` (and the
    spread/optional variants likewise);
    `recv?.greet(…)` guards with the existing `begin_optional` after evaluating
    `recv`, exactly like the `ObjGet` path today.
- `f(args)` (no receiver) stays `has_this = false` from 3a; `this_val` is the
  frame default `undefined`. Unchanged.

Acceptance (3b):
- [ ] `obj.greet()` where `greet` is an own function property runs with
      `this === obj`; a prototype-chain `greet` likewise.
- [ ] An own property still shadows a builtin method name *and* now sees `this`
      (extend the Phase-`4ea0249` shadow tests to assert `this`).
- [ ] `const f = obj.greet; f()` runs with `this === undefined` (detachment).
- [ ] `recv.greet(...xs)` (spread) binds `this === recv` via `ObjPeek` +
      `CallSpread(has_this=true)` — **no `CallMethodSpread` opcode exists**.
- [ ] `recv[k]()` (computed) now compiles (the `call.rs:106` hard error is gone)
      and binds `this === recv` via `ObjPeekDyn` + `CallDyn(has_this=true)`;
      `recv[k](...xs)` likewise via `CallSpread`.
- [ ] A computed key naming a *builtin* method (`arr["push"](x)`) resolves to
      `undefined` → "not a function" (documented divergence — builtins aren't
      stored properties).
- [ ] `recv?.greet()` on a nullish `recv` short-circuits (no `ObjPeek`, no call,
      no arg evaluation); on a present `recv` binds `this`.
- [ ] A missing / non-callable `greet` raises "not a function" **at the call**
      (not at the read); a non-`Object` receiver inherits `ObjGet`'s existing
      read error (`ObjPeek` shares it).
- [ ] `ObjPeek` is `ObjGet` minus the `pop` (shared helper, same `pe_*`/
      `ResumeMode`); the `has_this` dispatch fork (user-fn → `this_val`, builtin →
      arg 0) is exercised both ways.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 4 — `new F(args)` (on a unified function representation)

A function needs a per-*value* home for its prototype, which `Value::Fn(CodeAddr)`
— a bare address — doesn't have. So Step 4 first **unifies the function
representation** (4a, behavior-preserving), then builds `new` + `F.prototype` on
it (4b). This drops the `proto_of` side table entirely and gives each function
value its own prototype (JS-faithful: two closures from the same code get
distinct prototypes, where a `CodeAddr`-keyed table would have shared one).

### Step 4a — unify `Fn` into `Closure` (no behavior change)

Today there are two callable code-values: `Value::Fn(CodeAddr)` (no captures,
addr inline, no heap entry) and `Value::Closure(ClosurePtr)` (heap entry
`{ addr, upvals }`, `mod.rs:486`). Collapse them into one.

- **`Value::Closure { addr: CodeAddr, ptr: ClosurePtr }`** — both `u32`, 8 bytes,
  so `Value` stays **16**; remove `Value::Fn`. The inline `addr` means `CallDyn`
  jumps directly; `ptr` is dereferenced only to install upvals, and only when the
  function captures (the count is known from the `addr`'s metadata, so a
  non-capturing call never touches `ptr`). This *removes* the deref `Closure`
  pays today just to fetch its `addr`.
- **Heap `Closure` becomes `{ upvals: ThinVec<Value>, prototype: Option<ObjectPtr> }`**
  — drop `addr` (now in the `Value`); add a **dormant** `prototype` (`None` until
  4b; most functions never allocate one).
- **One canonical `Closure` per non-capturing function.** Identity requires
  `f === f`, so a non-capturing function gets a single canonical `Closure` (same
  `ptr` on every push). Allocate it at link/init time and bake the `ptr` into
  `PushFn`'s lowering (→ push `Closure { addr, canonical_ptr }`) — no runtime
  `addr→ptr` map. Capturing functions keep allocating per-instantiation via
  `ClosureNew` (distinct `ptr` → distinct identity, also correct). Static
  `Call(addr)` is untouched (addr baked in the instruction); the canonical
  `Closure` is materialized only when a function is used as a *value*.
- **Match-arm migration** (the compiler flags each): collapse every `Fn | Closure`
  arm to a single `Closure` (`value.rs:218/247/440`); `strict_equal` becomes
  `Closure` `ptr`-equality (the canonical-per-addr scheme preserves today's
  `Fn(a)==Fn(b)` ⇔ same addr); `type_name` → `"function"`; the `dispatch_call`
  `Fn`/`Closure` arms **merge into one**; `EnterFrame` reads upvals from
  `closures[ptr]`.

Behavior-preserving — functions dispatch, compare, and print exactly as before;
`prototype` is dormant. Isolated as its own green checkpoint.

Acceptance (4a):
- [ ] Full suite green: first-class functions, closures, recursion, `typeof`,
      and equality (`f === f`, `f !== g`, a captured closure `!==` another
      instance) all unchanged.
- [ ] `Value` still 16 bytes (size assertion); `Value::Fn` removed.
- [ ] A non-capturing function pushed twice has the **same** `ptr` (identity);
      `dispatch_call` no longer derefs to fetch `addr` (it's inline).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

### Step 4b — `new F(args)` + `F.prototype`

With the unified `Closure`, the prototype is a field on the function value — no
side table.

- **`F.prototype` (read):** `ObjGet` on a `Closure` receiver + `"prototype"` →
  `closures[ptr].prototype`, lazily allocating an empty object on first access and
  storing it back. (`ObjGet` *errors* on a callable receiver today,
  `dispatch.rs:42` — add this arm.) `F.prototype.m = fn` is then an ordinary
  `ObjSet` on that object. **`F.prototype = wholeObj` reassignment is not
  supported** (recorded divergence — classes forbid it anyway).
- **`new F(args)`** needs a `New` mechanism (none exists — `NewExpression` today
  only routes to the `Error`/`RegExp`/`Map`/`Set` special forms, `expr.rs:114`):
  1. resolve `F` to its `Closure` value; alloc instance `O` with
     `O.proto = F.prototype` (lazily allocated as above);
  2. `dispatch_call(F, this_val = O, args)` — the unified chokepoint, `O` as
     `this_val`;
  3. **post-check the return** — if `F` returned an object, that is the result;
     else the result is `O` (JS ignores a non-object return). This runs *after*
     the call, so it is the one genuinely new bit: a `New`-wrapping op that does
     the alloc + `this_val` set-up and the post-return fixup.
- `this.x = …` inside `F` writes own properties on `O` via `LoadThis` + `ObjSet`
  (Steps 1 + 2). Shared methods: `F.prototype.m = …`, then `new F().m()` resolves
  `m` by Step 2's chain walk with `this = O` (Step 3b).
- Update the diagnostic: `new F(...)` for a user `F` now compiles; keep rejecting
  `new Map()`/`new Set()`/`new Date()` (special-cased per `4_FUTURE`). Dynamic
  `new (expr)()` over a non-constant callee: the MVP **requires a statically
  resolvable `F`** (reject otherwise) unless `dispatch_call` already holds the
  value — decide and state.

Acceptance (4b):
- [ ] `function P(x){ this.x = x } new P(5).x === 5`.
- [ ] `P.prototype.get = function(){ return this.x }; new P(7).get() === 7`
      (shared prototype method, `this` bound to the instance).
- [ ] A constructor returning an object yields that object; returning a
      primitive/undefined yields the new instance.
- [ ] `F.prototype` lazily-allocates once (same object on repeat reads); two
      functions from the **same code** (distinct closure values) get **distinct**
      prototypes.
- [ ] `new Map()`/`new Set()` still rejected with the alternative-naming
      diagnostic; `Object`/`Error` constructors unaffected.
- [ ] Instances serialize own enumerable data only (no JSON form for the
      prototype link or methods).
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
- `dispatch_call` gains a `Value::Bound` arm: prepend `inner.bound_args` to the
  call-site args (whichever they are — fixed, or already spread by `g(...xs)`,
  since prepend inserts below the current top-`n` arg region) and **recurse** —
  `dispatch_call(inner.callable, inner.this_val, …)`. No new routing: the
  recursion lands on the same chokepoint, so a bound user
  function/closure gets `this_val = inner.this_val` in its frame field and a bound
  *builtin* (`[].push.bind(arr)`) gets it spliced as arg 0 — automatically. The
  Bound arm *overrides* a call-site receiver (`obj.g()` where `g` is bound ignores
  `obj`), since it supplies its own `this_val`; a method call (`ObjPeek` +
  `has_this`) that resolves to a `Bound` therefore defers to the Bound's
  `this_val` rather than `recv` — which is automatic once `dispatch_call` takes
  `this_val`, because the `Bound` arm overwrites it on recursion. Binding is
  composable: `g = f.bind(a, x); g.bind(b, y)` pre-pends
  `x` then `y` and keeps the *first* `this` (JS: re-binding `this` is a no-op) —
  implement by flattening into a fresh `BoundFn` over `f`.
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
    (same as the other callables — `Closure`/`Builtin`; `Fn` is gone after 4a).
  - `vm/methods.rs` `stack_value_to_json` → reject, exactly like `Closure`.
  - Arms with a `_` catch-all (`compare`, `loose_equal`, `is_string`,
    `as_f64`/`as_i64`/`is_number`, `str_byte_len`) need **no** edit — verify,
    don't add.
  - `Bound`'s payload is `Rc<BoundFn>` (a thin pointer), so `Value` stays
    **16 bytes** — do not let it widen (no inline `BoundFn`).

**`.call` / `.apply` (eager siblings of `bind`).** Same `this`-threading,
*no new value type* — they invoke immediately rather than producing a value, so
they are strictly cheaper than `bind`:

- Both terminate at the **one tail** `dispatch_call(callable, this_val, nargs)`,
  so they re-derive **no** routing — they only extract
  `(callable = receiver, this_val = thisArg)` and arrange the args:
  - `f.call(thisArg, a, b)` → `dispatch_call(f, thisArg, 2)` over the `[a, b]`
    already on the stack above `thisArg`.
  - `f.apply(thisArg, argsArray)` → expand `argsArray` via the **same
    array-expansion helper `CallSpread` uses** ("reuse `CallSpread`" = reuse its
    shared Rust helper; a builtin can't emit the bytecode instruction, so do
    *not* re-implement spreading), then `dispatch_call(f, thisArg,
    spread_count)`. A non-array, non-nullish `argsArray` is a `TypeError`.
- Register both as `Method`-kind builtins whose receiver is callable
  (`Closure`/`Builtin`/`Bound` — `Fn` is unified into `Closure` in 4a); a
  non-callable receiver is a `TypeError`.
  Like the shadow reroute (`reroute_method_to_object`, `methods.rs:1142`), these
  are builtins that **re-enter `dispatch_call`** — reuse that established hand-off
  (the invoked callee's frame yields the result in the `.call`/`.apply`
  expression's place; `f` and `thisArg` sit below the args and are consumed)
  rather than inventing a new builtin return discipline.
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
- [ ] Bound ∘ spread composes: a bound function called with a spread
      (`g(...xs)` where `g = f.bind(null, p)`) runs `f(p, ...xs)` — bound args
      precede the spread args.
- [ ] `f.apply(null, xs)` yields the same result as `f(...xs)` for the same `xs`
      (confirms `.apply` and `CallSpread` share one array-expansion helper, not
      two implementations).
- [ ] `f.call`/`f.apply` route through `dispatch_call` (the callee's result is
      the `.call`/`.apply` value; no separate return path) — exercised with both
      a user-fn and a builtin `f`.
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

- `Closure` → declared param count *before the first default/rest param*
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

Both **1a → 1b** and **3a → 3b** are hard orders that isolate a behavior-
preserving mechanical change (1a: add the field + `LoadThis`, every read
`undefined`; 3a: callee-below-args + `has_this` plumbed-`false`, suite green)
from the semantics that follow (1b: reify-on-capture; 3b: bind `this`). The
novel/risky surface is concentrated in **1b** (reify-on-capture for arrows) and
**3b** (the user-fn-vs-builtin receiver fork + threading `this_val` through the
chokepoint), so keep each behind its gate.

Two steps are **independent, behavior-preserving refactors that can land first,
even before 1/2**, as warm-ups: **3a** (callee-below-args, touches neither `this`
nor the proto chain) and **4a** (unify `Fn`→`Closure`, touches only the function
value model). 1b and 2 are independent and can land in either order, but both
precede **3b** (3b sets the first non-undefined `this`, which is when 1b's capture
wiring must already be correct): 3b depends on 1b + 2 + 3a. **4b** depends on
4a + 3b. **5** depends on 3b + 4a (its `Bound` checklist and `.call`/`.apply`
receiver test assume the unified `Closure`). 6 depends on 3b + 5. 7 depends on 4b
(and 5 if methods-as-values appear in class bodies). The **minimum coherent
system** is Steps 1–4; 5–7 are the deferred items folded into the same substrate
so they never become one-off bolt-ons.

## Divergences from JS (record in the divergence list as they land)

- **Identity of method/bound values.** `f.bind(x) !== f.bind(x)`; a bare method
  read (`[].push`) mints a fresh value, so `arr.push !== arr.push`. JS shares
  the prototype function. Accepted.
- **No `[[Set]]` traps / accessors.** Own-property assignment only; no
  getters/setters in the MVP.
- **`F.prototype` is mutable but not reassignable.** `F.prototype.m = …` works
  (mutating the prototype object); `F.prototype = wholeObj` is unsupported. JS
  allows the latter for plain functions but forbids it for classes (non-writable),
  so the MVP follows the class rule uniformly.
- **Prototype methods are enumerable.** `ObjData.map` has no enumerability flag,
  so methods placed on a prototype are enumerable — unlike JS class methods
  (non-enumerable). Invisible to `JSON.stringify` (methods aren't *own*
  properties), but a chain-walking `for-in`/key enumeration over an instance would
  surface them, where JS hides them.
- **Computed keys reach only own/proto properties.** `recv[k](…)` binds `this`
  and resolves user methods on `Object` receivers, but a computed key naming a
  *builtin* method (`arr["push"]()`) fails — builtins aren't stored properties,
  so only the static form (`arr.push()`, compiler-resolved) reaches them.
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

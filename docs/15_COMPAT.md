# Phase 15 — JavaScript compatibility as a first-class goal

Phases 0–13 treated JS compatibility as **instrumental**: features were
admitted on evidence (LLM-authored glue code reaches for them and the
"name the alternative" diagnostics became friction), and recorded
non-goals (`4_FUTURE`) fenced off everything the data model didn't need.
That served the agent product well and is not being thrown away.

This phase records a **deliberate goal shift**: as the VM has become a
capable language runtime in its own right, JS compatibility becomes a
**terminal** goal — worth pursuing for its own sake, not only where a
specific program demands it. This is a hobby-project decision, made with
eyes open, and it is written down here so future work treats it as
intent rather than drift.

What the shift changes: the bar for admitting a compat feature drops from
"a real program needs it" to "real JS does it and it doesn't violate a
guardrail below." Mutable builtin prototypes, property descriptors,
getters/setters, the iterator protocol, `Symbol`, `ToPrimitive` — all of
these move from "probably never" (their `4_FUTURE` status) to "scheduled,
ordered by a conformance scoreboard."

What the shift does **not** change: the condition system, the JSON
boundary, and "suspension at zero-cost safe points" — these make the
*agent* product work and are not negotiable for compat.
**Determinism is explicitly *not* on that list.** Crash/resume recovery
no longer depends on deterministic positional replay: the agent either
rewrites the program or reruns it on resume, so the VM is free to be
nondeterministic. That makes full JS compat *easier*, not harder —
`Date.now`, `Math.random`, unseeded iteration order, and the rest are
simply compatible, with no seeded/logged routing to honor. The remaining
collisions with the agent spine are narrow, named, and permanent.

---

## The compat north star + permanent guardrails

Compatibility is the goal **up to** two invariants — the short fence that
keeps the *agent* product intact. A feature that violates one is rejected
on principle; everything else in JS is fair game.

1. **No host callback mid-instruction.** The load-bearing property
   (`DESIGN.md`): effects are `StepResult` returns, so the VM is never on
   anyone's stack at a decision point and can suspend to hand a condition
   out to the LLM restart handler. This is the *condition system's*
   requirement, **not determinism's** — a compat feature that made the VM
   call back into the host between two instructions would break suspension
   and is out regardless of spec fidelity. (Nothing in the prototype /
   descriptor / iterator / symbol surface needs this; it is a guardrail
   against future temptations like synchronous host hooks.)
2. **The JSON boundary is invariant.** Prototypes, descriptors, methods,
   symbols, bound functions, and every reflective artifact this phase
   adds have **no JSON form**, exactly as `Closure`/`Promise`/`RegExp`/the
   Phase-13 proto link already do. `stack_value_to_json` rejects them; a
   test rides along with each feature that introduces a non-serializable
   value or slot.

**Determinism is not a guardrail.** It was load-bearing for positional
replay in `DESIGN.md`'s dependency spine; with recovery now by
rerun/rewrite, that dependency is dropped, and nondeterministic builtins
(`Date.now`, `Math.random`, unseeded iteration order) are simply
compatible. This *shrinks* the fence — there is less to honor, not more —
and is the one place this phase contradicts `DESIGN.md` (surfaced
deliberately, per that file's own "disagreement is a design change" rule).

The one spot that needs a design note rather than a free pass is
**getters/setters**: once a property *read* can run user code,
`ObjGet`/`IndexGet` become call sites that can suspend, throw, and consume
fuel — which touches the optimizer's purity tables (`pe_*`) and the resume
classification. That is mechanism to get right, not an invariant to
defend, and it is called out in the step that introduces it.

## The scoreboard: a conformance corpus

A terminal goal needs a metric, the way the condition report has
golden-render tests and M5 has its eval. "More compatible" must become a
number, or it dissolves into vibes and bikeshedding over which feature is
"real JS."

- Stand up the scoreboard on **test262 directly** (Step 0): vendor the
  official suite, run it broadly against today's VM, and record an honest
  baseline — pass/fail/skip/known-divergence against a checked-in expectations
  file, with failures **bucketed by cause and feature** so the histogram
  sequences the later tiers. A small hand-written corpus complements it for
  *this dialect's* own surface (`raise`, conditions) that test262 can't see.
- The corpus is the **sequencer**: the expensive tiers below
  (descriptors, iterators, `ToPrimitive`) are ordered by *what the corpus
  fails on*, not by spec chapter order or by which feature feels
  canonical. Build the substrate (Steps 1–3), then let measured failures
  pick what comes next.
- Each deliberate divergence (see the ledger at the end) is a **named,
  asserted** corpus entry — a test that pins the divergence, so a
  divergence is a recorded decision, never a silent gap.

This corpus is the highest-leverage thing to build first; it is cheap and
it makes every later decision evidence-driven again — the same discipline
as before, now measuring compat instead of program need.

## The keystone: a reflective prototype/constructor model

The single structure that unlocks most of the rest is **real,
introspectable per-type prototypes and real constructor objects**,
populated from one registry. It is the substrate `instanceof`,
`.constructor`, `Object.create`/`getPrototypeOf`, getters/setters,
descriptors, and eventual prototype mutability all stand on.

The pivotal design choice — the one that lets this be cheap and lets the
hot path stay untouched — is that the model arrives **frozen first**:

> The compiler already resolves `arr.push(x)` by *optimistic name-selection
> + runtime receiver-check* (`CallBuiltin(ArrayPush)` for any receiver, then
> `array_receiver()` validates, with the `MethodOnObject` reroute catching
> the Object-with-own-`push` case). That optimistic selection **is an inline
> cache** — one that is permanently valid *iff* the builtin prototype cannot
> change. Frozen builtin prototypes therefore make today's `CallBuiltin` the
> IC, with **no guard to check, ever**. The prototype object is the
> reflective mirror of the same data, not a participant on the call path.

So Steps 1–2 add JS-shape *compatibility* (reflection, `instanceof`,
`constructor`, method-values, real `Math`/`JSON`) with **zero hot-path
regression**, and Step 3 installs the *seam* for mutability without yet
paying for it. If and when the corpus demands monkeypatchable builtins,
the V8-style "is this prototype pristine?" guard is added with its default
wired to "pristine" — the frozen design is forward-compatible with the
mutable one by construction, because frozen is just "guard ≡ true."

This is the honest version of "eat your cake and have it too": you get
compatibility of the observable shape and internal uniformity, and you
*avoid* the dynamism tax for as long as the corpus lets you, because the
one operation that forces the tax (mutating a builtin prototype) is the
last thing added, not the first.

## The convergence target: one `get`, one `set`

The point of this phase — beyond JS shape — is that property **lookup** and
property **modification** each end with **one canonical definition**, and
everything else becomes a thin adapter over it. Today the same resolution
logic is smeared across the dispatch loop and re-implemented per call site:

- **Reads** live in `resolve_property_from_top` (ObjGet/ObjPeek),
  `resolve_computed_property` (IndexGet/ObjPeekDyn), `resolve_closure_prototype`
  (Closure `prototype`), `regexp_prop` (RegExp fields), `GetMethodOrProp` →
  `method_for_receiver` (method-value reads), `resolve_proto_chain` (the Object
  own→proto walk), **plus inline copies** inside `IndexGet`/`IndexSet`'s `Old`
  mode and `ObjHas` — each re-deciding "what does this name mean on this
  receiver."
- **Writes** live in `ObjSet` (with a bespoke RegExp `lastIndex` arm and the
  Object-map arm), `IndexSet` (array-index arm + object-map arm), `ObjDelete`,
  and `ObjExtend` — each re-deciding "where does this name go on this receiver."

The end state is exactly two functions:

```
get_property(receiver, key) -> Result<Value>      // the read ladder, one place
set_property(receiver, key, value, mode) -> Result // the write ladder, one place
```

plus the two thin relatives that are the *same* ladder stopping early:
`has_property` (read that asks presence, not value) and `delete_property`
(write that removes). Every instruction above is reduced to **stack
choreography around one of these calls** — pop vs. peek, static `field` vs.
computed `key`, keep-receiver vs. consume — and **nothing else**. The
difference between `ObjGet` and `ObjPeek` is one `pop`; the difference between
`ObjGet` and `IndexGet` is where the key comes from; none of them re-derives
resolution.

Two consequences make this load-bearing rather than cosmetic:

1. **Builtins call the same `get`/`set` on their fallback path.** When a
   builtin's fast-path receiver-check misses (the polymorphic / reflective
   case, and the retired `MethodOnObject` reroute), it funnels into
   `get_property`/`set_property` instead of hand-rolling another lookup. One
   definition means a builtin can never disagree with the dispatch loop about
   what a name resolves to.
2. **The ladder rungs (Step 2e) are the body of these two functions, not a
   third place.** "Primary representation → user bag → virtual own props →
   type prototype → undefined" *is* `get_property`; the write-enforcement
   checks (extensibility/seal/frozen from Step 2d) *are* `set_property`. The
   per-type specialization lives **inside** the one function as match arms, so
   "one definition" and "no-cost fast paths" are the same claim: a fast path
   is just an early arm that returns before consulting later rungs.
3. **The signature is shaped for accessors from day one — they resolve, they
   do not *run*, inside the pair.** This is the read/write analog of
   "frozen first": just as method dispatch gets the `overridden` seam (Step 3)
   so mutability is a localized later change, `get_property`/`set_property`
   must not be designed as "returns a `Value`" only to have Step 4's
   getters/setters force a re-signature. The reason is the load-bearing
   property itself: a getter is **user code**, and running it means pushing a
   frame and returning to the dispatch loop — the loop owns frame-pushing, and
   "no host callback mid-instruction" forbids the resolver from running it on
   its own stack. So the pair resolves to a **slot** — *data value* **or**
   *"this is an accessor, here is the closure"* — and the **instruction**
   (which owns the stack) acts on it: data → push/store; accessor → set up the
   call exactly like `CallDyn`/`ObjSet`-to-a-setter, on the StepResult path
   where suspension lives. Frozen-first means **no accessor exists yet**, so
   today every resolution takes the data arm at zero cost; the slot return
   type is the cheap seam that keeps Step 4 from reopening Step 2. (Concretely:
   `get_property -> Result<Slot>` where `Slot = Data(Value) | Accessor(Closure)`;
   while no descriptors exist, only `Data` is ever constructed.)

This is the simplification the whole phase is in service of: not "more
helpers," but **fewer** — the scattered resolvers above are *deleted* into
these two, and each new step is judged partly on whether it moved logic *into*
the canonical pair or smeared a new copy beside them.

---

## Ground rules

- Per step: corpus entries (new passes that previously failed, plus any
  pinned divergence), behavioral tests in the Phase-2 harness, and a
  divergence-ledger update where semantics still differ from JS. Commit
  per step, `compat:` prefix.
- **Finish a step before starting the next.** Each `Acceptance` box is a
  gate; do not begin step N+1 with step N's box unchecked. Steps are
  ordered by dependency.
- Every new instruction needs a doc comment with its stack effect, a
  `step()` arm, an optimizer purity classification (walk each `pe_*`
  table), and a Phase-3 resume classification (`ResumeMode`).
- `Value` stays **16 bytes** (the size assertion in `vm/value.rs`).
  Per-type prototypes and constructors live in a static side table keyed
  by the value's discriminant — **never** a per-value field — so arrays,
  strings, and numbers do not grow, allocate, or box.
- The JSON boundary (guardrail 3) is invariant. Add the
  `stack_value_to_json` rejection + test in the step that introduces each
  new non-serializable value or slot.

This phase builds **on** Phase 13 (proto chains on `Value::Object` —
`ObjData.proto`, `vm/mod.rs:278`; `instanceof` + `resolve_prototype`,
`vm/methods.rs:939`; the `TypeTag` structural fast-path, `vm/instr.rs:59`;
`GetMethodOrProp`, `compiler/member.rs:52`; `class` codegen; `bind`) and
**after** Phase 14's cleanup. It does not revisit the `MethodOnObject`
re-route as a smell to tolerate — Step 2c **retires** it.

---

## Step 0 — run test262 and record a real baseline

The scoreboard is the highest-leverage thing to build first, and the
decision (made here) is to **point it at the real suite immediately**:
vendor test262, build the runner, and get an honest pass/fail number against
today's VM. A baseline is not a measure of success — it is a *map*. The point
is to discover, against the official spec, exactly how much already works and
which features dominate the failures, so Steps 1–4 are sequenced by measured
reality instead of guesswork. This is front-loaded effort, deliberately.

**Precondition:** the workspace builds and the existing suite is green (it is,
as of this writing — the Phase-14 analyzer split has landed). The runner is a
new harness target; it must not regress the existing tests.

**Vendor + run.**

- **Vendor test262 as a git submodule** (≈50k files — never copy it into the
  tree). Pin a commit.
- **Build a runner** that, per test file:
  - parses the `/*--- … ---*/` YAML frontmatter (`flags`, `includes`,
    `features`, `negative`);
  - prepends the harness includes — `harness/sta.js` + `assert.js` always,
    plus each name in `includes:` (`propertyHelper.js`, `compareArray.js`,
    …). **test262 asserts by running JS** (`throw new Test262Error`,
    `assert.sameValue`), so getting the *harness subset itself* to run is the
    true unlock — the first concrete sub-target below;
  - honors `negative:` (a test that must throw a given error type/phase — the
    expected outcome comes free from metadata) and `flags`.
- **Classify each result** into `pass` / `fail` / `skip` — and crucially
  **bucket the `fail`s by cause**: parse/syntax error vs. runtime throw
  (which `ErrorKind`) vs. wrong value vs. harness-failed-to-load, **and tag by
  the test's `features`**. The bucketed histogram *is* the sequencer: "N
  thousand failures are `Symbol`, M are descriptors" tells Step 4 what to do
  first. An unbucketed count is just a number; the buckets are the map.

**Baseline policy: run broadly, skip only the structurally impossible.** For a
*baseline* a `features` **allowlist** is the wrong instrument — it pre-decides
what we support and hides the unknowns. Use a **skip list** instead: exclude
only what the VM structurally cannot represent (`module`, `raw`,
`SharedArrayBuffer`/`Atomics`, anything needing realms/`eval`), and **let
everything else run and fail honestly**. The failures are the deliverable. (A
narrowing allowlist is the right instrument *later*, per Step-4 tier, to keep
each tier's signal clean — not now.)

**The first sub-target is the harness, not the tests.** Expect the order of
work to be: (1) make `sta.js` + `assert.js` + `compareArray.js` load and run
on the VM — that alone unlocks the bulk of non-`propertyHelper` tests; (2)
note that `propertyHelper.js` leans on `Object.defineProperty`/descriptors and
will stay red until the Step-4 descriptor tier — so its dependents are
*expected* baseline failures, not surprises. Record that expectation so the
baseline reads as a map, not a panic.

**The expectations file is the ledger, mechanized.** Check in one
**expectations file** (`pass` / `fail` / `skip(reason)` / `known-divergence`
per test path), diffed each run — the model Boa and QuickJS use. A test that
*should* fail forever (a deliberate divergence — `[5] == 5`, frozen builtin
prototypes) is recorded as **`known-divergence` with a ledger reference**, not
`skip` and not a silent red. So the ledger and the suite agree from day one,
and any later run that flips a cell (a regression, or an auto-resolved
divergence) surfaces as a diff against the file.

**Keep the hand-written/differential corpus as a complement, not the primary.**
test262 does not cover *our dialect's* own surface — `raise`, the condition
system, tool-call/`await` shapes, the JSON boundary. A small hand-written
corpus (optionally differential-vs-Node, reusing `2_TESTS.md`'s harness) holds
those. test262 measures JS conformance; the local corpus measures the parts of
*this* language that test262 doesn't know exist.

Acceptance:
- [ ] test262 is vendored as a pinned submodule; the runner is a **new
      workspace-member binary crate** (e.g. `conformance/`, sibling of
      `interp`/`agent`, depending on `interp`'s public API — `compile` +
      `VM::for_program_with` + `StepResult`/`ErrorKind` are sufficient, no
      crate-internal access). Runner-only deps (frontmatter YAML, dir walk,
      arg parsing) stay out of `interp`. It is a CLI (`run` / filter /
      `--update` to bless the expectations file), **not** part of the default
      `cargo test` loop. It parses frontmatter, prepends `includes`, and honors
      `flags`/`negative`.
- [ ] The runner emits a `pass`/`fail`/`skip`/`known-divergence` tally **and**
      a failure histogram bucketed by cause and by `features`; the baseline is
      committed to the expectations file.
- [ ] The test262 harness subset (`sta.js`, `assert.js`, `compareArray.js`)
      loads and runs; `propertyHelper.js`-dependent failures are recorded as
      *expected* (descriptor tier), not investigated as regressions.
- [ ] Each existing ledger divergence with a test262 analogue is pinned as
      `known-divergence` referencing its ledger entry; dialect-only divergences
      live in the local corpus.
- [ ] The existing suite stays green; the `conformance` crate is additive. A
      thin CI `#[test]` runs the bin in "compare to committed expectations,
      fail on diff" mode — the bin is the engine, the test is the one-line gate
      (the 50k-file sweep itself is **not** behind `cargo test`).
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`
      (plus a `conformance` run to produce/refresh the baseline).

## Step 1 — the `builtins!` registry as single source of truth

Behavior-preserving. Today `Builtin::method_for_receiver` (`builtin/mod.rs:380`)
is an ~80-line hand-maintained `(receiver-type, name) → Builtin` match that
**duplicates** information already in the `builtins!` table; the
`Namespace(&'static str)` kind already encodes the owning namespace for
static builtins. The hand-maintained map is the concrete thing that *feels*
non-uniform, and it is the seam everything else generates from.

- Add a **receiver-type column** to each `Method`-kind row of the
  `builtins!` macro (`builtin/mod.rs`): the set of value types the method
  is valid for (`Array`, `String`, `Map`, `Set`, `RegExp`, `Function`,
  `Object`, or `Any` for the universal `ToString`). Polymorphic rows
  (`slice`/`includes`/`at`/`concat` for string+array; `has`/`delete`/…
  for map+set) list multiple types — exactly the arms `method_for_receiver`
  hand-writes today.
- **Generate** `method_for_receiver` from the macro instead of hand-listing
  it. The match arms in `builtin/mod.rs:380` collapse into a table walk
  over the registry; delete the duplication.
- No runtime behavior changes: same `(type, name) → Builtin` answers, same
  dispatch, same arities. This is the registry refactor that makes Step 2's
  prototype population a *projection* of one table rather than a second
  hand-maintained list.

Acceptance:
- [ ] `method_for_receiver` is derived from the `builtins!` rows; no
      hand-maintained per-type match remains (codegen-shape / inspection).
- [ ] Full suite green; method dispatch, shadowing, and method-value reads
      (`[].push`) unchanged (the no-op guarantee).
- [ ] Alloc-count tests unchanged.
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

## Step 2 — frozen builtin prototypes + real constructors

The keystone. Introduce real, **immutable** per-type prototype objects and
real constructor values, populated from the Step-1 registry, wired into
reflection — with call dispatch untouched.

### Step 2a — representation (per-type prototype side table + constructor values)

This step does two separable things — **commit them as two parts**, each with
its own gate below, so neither lands half-done. Part 1 is the prototype side
table + the `IntegrityLevel` field (pure representation, no name resolution
changes); Part 2 promotes the namespace fictions to real values and rebinds
the identifiers that named them.

**Part 1 — prototype objects + the integrity field.**

- **Per-type prototype objects.** At link/init time, allocate one frozen
  `Object` per builtin type (`Array.prototype`, `String.prototype`,
  `Number.prototype`, `Boolean.prototype`, `Function.prototype`,
  `Map.prototype`, `Set.prototype`, `RegExp.prototype`, `Object.prototype`),
  each populated by `Value::Builtin` entries projected from the Step-1
  registry (`(type → [methods])`). Store the type→prototype mapping in a
  **static side table keyed by `TypeTag`** (extend the existing enum,
  `vm/instr.rs:59`) — not a per-value field, so `Value` stays 16 bytes and
  primitives never box. Allocation may be lazy (first reflective touch) to
  keep startup cheap; if lazy, the table holds `Option<ObjectPtr>`.
- **Frozen — via a *shared* integrity field, not a private bool.** Add an
  **`IntegrityLevel { Extensible, Sealed, Frozen }`** field to `ObjData`
  (`vm/mod.rs:278`); the builtin prototypes are constructed at `Frozen`, so
  `ObjSet`/`delete`/`defineProperty`/`setPrototypeOf` on them is a
  `TypeError`. This is the **same** field user `Object.freeze`/`seal` writes
  in Step 2d — one mechanism, dogfooded, so the internal guarantee and the
  public surface are provably the same code. A three-state enum (not a
  bool), because `seal` sits between `preventExtensions` and `freeze`.
  Freezing is what makes the compiler's `CallBuiltin` a permanently-valid IC
  (see keystone), so it is load-bearing, not decoration.

Acceptance (2a, Part 1):
- [ ] Each builtin type has one frozen prototype object reachable via a
      `TypeTag`-keyed table; `Value` still 16 bytes (size assertion).
- [ ] `ObjData` carries an `IntegrityLevel` field (the one Step 2d reuses);
      builtin prototypes are `Frozen`, and writing to one is a `TypeError`.
- [ ] Prototypes have no JSON form (`stack_value_to_json` rejects; test).
- [ ] No name resolution changed yet (`for_namespace` still in place);
      method-call fast paths untouched (codegen-shape).
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

**Part 2 — constructor objects + identifier rebinding.**

**Constructors are *functions*, namespaces are *objects* — do not conflate
them.** This is the JS object model and test262 enforces it hard (whole
`built-ins/Map/`, `built-ins/Array/` subtrees gate on it):

| | what it is | `typeof` | callable / `new` | examples |
|---|---|---|---|---|
| **Constructor** | callable function value, proto-chains to `Function.prototype` | `"function"` | yes | `Object`, `Array`, `Function`, `Map`, `Set`, `RegExp`, `Number`, `Boolean`, `String`, `Error` |
| **Namespace** | non-callable frozen plain object | `"object"` | no | `Math`, `JSON`, `Reflect` |

Representing a constructor as a frozen `Value::Object` is **wrong** and fails
basic, feature-independent tests (`typeof Map === "function"`, `new Map()`,
`Map instanceof Function`, `Array(3)`, `Object.getPrototypeOf(Map) ===
Function.prototype`) — a *wrong-model* failure that pollutes the baseline, not
an honest "unimplemented" one. So:

- **Constructors → callable values.** Promote them to **callable** builtins:
  add a **`BuiltinKind::Constructor { type_tag }`** (today's kinds are only
  `Method`/`Namespace`, `builtin/mod.rs:67`). `typeof` already yields
  `"function"` for `Value::Builtin` (`dispatch.rs` `TypeOf` arm), so the type
  tag is free; their proto chain reaches `Function.prototype` (2b) so
  `instanceof Function`/`getPrototypeOf` hold. Static methods
  (`Array.isArray`, `Object.keys`) and `.prototype`/`.name`/`.length` are
  **virtual rungs** off the constructor (resolved from the Step-1 registry),
  *not* materialized into a map — see the enumerability note below.
- **`New` must accept native constructors.** Today `New` only routes
  `Value::Closure` (`dispatch.rs:465`, else "cannot call … with `new`"). A
  `new Map()`/`new Set()`/`new RegExp()` must dispatch to the type's native
  constructor (folding in today's `MapNew`/`SetNew`/`RegExpNew`/array paths).
  This **retires** the ledger's "`new Map()` rejected" divergence. Mark which
  constructors *require* `new` (`Map`/`Set` throw without it) vs. are callable
  as plain functions (`Array`/`Object`/`Number`/`String`/`Boolean`).
- **Namespaces → frozen plain objects** (the doc was already right here).
  `Math`/`JSON` (and later `Reflect`) stay **non-callable** frozen
  `Value::Object`s carrying their statics/constants (`Math.max`, `Math.PI`,
  `JSON.parse`) as own properties, retiring `namespace_constant`
  (`compiler/member.rs:16`). `typeof Math === "object"`; `Math()` / `new Math`
  throw, as in JS.
- **Enumerability falls out of "methods are virtual."** Builtin prototype
  methods and constructor statics must be **non-enumerable** —
  `Object.keys(Array.prototype) === []`, `for-in` shows no `push`. Keeping
  them as virtual rungs (registry-resolved) with the prototype's own `map`
  **empty** gives this for free; materializing methods into the map would
  wrongly make them enumerable. So "methods are virtual" is a correctness
  requirement, not just an optimization. (Descriptor-accurate
  `writable`/`configurable` on these — what `propertyHelper.js`'s
  `verifyProperty` checks — is the Step-4 tier and an *expected* baseline-red
  bucket, not a Step-2 failure.)
- **The bare identifier rebinds, not just the `.member` path.** Today
  `Array`/`Object`/`Math`/… are *not* values — they exist only as the head of
  a `for_namespace` member/call lowering, so `let f = Array` has nothing to
  bind. Promoting them to real values changes **identifier resolution**: the
  names must resolve to the actual constructor `Value`s. Decide and state
  where that binding lives — a small **frozen global environment** the
  compiler resolves these reserved names against (the same place a future
  `globalThis` would expose them), so `Array`, `globalThis.Array`, and
  `Object.keys` passed as a callback all reach one object. This is the part
  that is *not* covered by the `.member` reflective path and must be designed,
  not assumed.
- **The compiler keeps its fast paths.** `Math.max(…)`, `arr.push(…)`,
  `JSON.parse(…)` still lower to `CallBuiltin`/namespace calls (frozen ⇒
  the static answer is permanently correct). The real objects exist for the
  *value/reflective* path: `Math.max` read as a value, `Object.keys` passed
  as a callback, the bare `Array` identifier, etc. — these resolve as ordinary
  reads off the real objects (or the global binding) instead of
  `for_namespace`. No new dispatch on the hot path.

Acceptance (2a, Part 2):
- [ ] **Constructors are callable functions:** `typeof Array === "function"`,
      `typeof Map === "function"`; `new Map()` / `new Set()` / `new RegExp()`
      construct (not "cannot call … with `new`"); `Array(3)` / `Number("5")`
      call as plain functions; `Map()` without `new` throws. The "`new Map()`
      rejected" ledger divergence is retired.
- [ ] **Namespaces are non-callable objects:** `typeof Math === "object"`,
      `Math()` / `new Math` throw; `Math.PI`, `JSON.parse` resolve as own
      properties. `for_namespace`/`namespace_constant` are gone (or reduced to
      the reflective lookup).
- [ ] **Enumerability:** `Object.keys(Array.prototype) === []` and
      `for-in` over `[]` shows no method names (methods are virtual rungs; the
      prototype's own map is empty).
- [ ] The **bare identifier** resolves: `let f = Array; f === globalThis.Array`,
      `const k = Object.keys; k({a:1})` work via the global binding, not a
      member fiction.
- [ ] `CallBuiltin` / namespace-call fast paths unchanged (codegen-shape);
      no proto-walk added to any method call. Alloc tests reflect only the
      one-time (or lazy) prototype/constructor allocation, asserted exactly.
- [ ] Constructors/namespaces have no JSON form (`stack_value_to_json`
      rejects; test).
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

### Step 2b — reflection wired to the real objects

With real prototypes/constructors, the JS reflective surface becomes
ordinary lookups rather than special cases:

- **`instanceof` for builtins.** `[] instanceof Array`, `m instanceof Map`,
  `f instanceof Function`, `x instanceof Object` resolve by the existing
  proto-chain walk (`vm/methods.rs:939` + `resolve_prototype`) now that the
  RHS constructors are real and their `.prototype` is the type's frozen
  object — folding the `TypeTag` structural fast-path and the user-class
  walk into one path.
- **`.constructor`** on any value reads through its type prototype to the
  real constructor object (`[].constructor === Array`,
  `(5).constructor === Number`).
- **Constructors chain to `Function.prototype`.** Each constructor value's
  `[[Prototype]]` is `Function.prototype`, so `Map instanceof Function`,
  `Object.getPrototypeOf(Array) === Function.prototype`, and the shared
  `Function.prototype` methods (`.call`/`.bind`/`.apply` read as values) hold —
  the constructor is a *function* in the proto graph, not a one-off object.
- **`Object.getPrototypeOf`** returns the real prototype for primitives and
  builtins (today it returns `null` for non-Object — `builtin/object.rs:171`);
  **`Object.create(proto)`**, `Object.getOwnPropertyNames`, and the
  `__proto__`-free reflective reads become uniform.
- **`Number.prototype` method resolution without boxing.** `(5).toFixed(2)`
  resolves `toFixed` via `Number.prototype` for *method lookup only* — the
  receiver stays an unboxed number value handed to the builtin as arg 0.
  **No primitive is ever wrapped** (no `Value` allocation for `"x".length`),
  matching how engines treat primitive method calls and sidestepping the
  `valueOf`/wrapper-identity footguns (see the ledger).

Acceptance (2b):
- [ ] `[] instanceof Array`, `new Map() instanceof Map`,
      `(()=>0) instanceof Function`, `{} instanceof Object` are all `true`;
      cross-type negatives are `false`; the user-class `instanceof` from
      Phase 13 still holds (one walk, no `TypeTag` special-case left).
- [ ] `[].constructor === Array`, `"".constructor === String`,
      `(5).constructor === Number`.
- [ ] `Map instanceof Function` is `true`;
      `Object.getPrototypeOf(Array) === Function.prototype`.
- [ ] `Object.getPrototypeOf([])` / `("")` returns the real prototype, not
      `null`; `Object.create(proto)` links it.
- [ ] A primitive method call (`(5).toFixed`, `"x".at`) resolves via the
      type prototype with **no boxing allocation** (alloc test).
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

### Step 2c — unify method resolution, retire `MethodOnObject`

The real uniformity prize. Today object-methods walk a proto chain while
builtin-methods are structural, bridged by the `MethodOnObject`
**error-kind-used-as-control-flow** reroute (`VM::method_receiver_error` →
the two `b.call` sites → `VM::reroute_method_to_object`) — defended in
13_OBJECTS, fenced off in 14_CLEANUP, but still a signal-as-control smell.
With frozen type prototypes, a *single* "resolve method for receiver"
helper consults own properties **and** the type's prototype, so the two
dispatch paths collapse and the reroute disappears.

- Introduce one resolution helper — **the seed of `get_property`** (the
  convergence target): own-property lookup (Object receivers) → type-prototype
  lookup (all receivers) → `Undefined`. For a frozen-prototype builtin hit it
  still yields the `Builtin` the compiler would have picked, so the
  `CallBuiltin` fast path remains the common case; the helper is the *general*
  fallback and the value/reflective path. `GetMethodOrProp`'s structural arm
  (`method_for_receiver`) and `resolve_property_from_top`'s Object arm both
  call **it**, not their own copies.
- **Delete the `MethodOnObject` signal** and its two interception sites: an
  Object with own `push` is now just "own property shadows the prototype
  method" through the unified helper — no error raised, no reroute. The
  builtin's missed-receiver fallback funnels into the *same* helper (see the
  convergence target), so dispatch and builtins share one answer.
- This is the one place this phase removes mechanism rather than adding it,
  and it is the concrete answer to "make dispatch more uniform." Step 2e
  finishes the job by folding the *computed*-read and *write* paths into the
  same pair, so this helper is not a fourth resolver but the first installment
  of the single `get_property`.

Acceptance (2c):
- [ ] `ErrorKind::MethodOnObject` and `reroute_method_to_object` are gone;
      one resolution helper serves own-property and type-prototype lookup.
- [ ] Object-own shadowing of a builtin name still works **and** binds
      `this` (the Phase-13 shadow tests pass unchanged).
- [ ] The `CallBuiltin` fast path is still emitted for `arr.push(…)` etc.
      (codegen-shape); the unified helper is the fallback/value path.
- [ ] Full suite + corpus green; alloc tests unchanged.
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

### Step 2d — expose `Object.freeze`/`seal`/extensibility (dogfood the field)

The integrity field that freezes builtin prototypes (2a) *is* the JS
freeze/seal mechanism — so expose it, rather than building a second one.
This is the uniformity payoff: the internal "this prototype can't be
written" guarantee and the user-facing `Object.freeze` are the same code
operating on the same `ObjData.IntegrityLevel`.

- **Six namespace builtins** on the real `Object` constructor (Step 2a):
  `freeze`, `isFrozen`, `seal`, `isSealed`, `preventExtensions`,
  `isExtensible`. Each reads/writes the receiver's `IntegrityLevel`:
  `preventExtensions → Extensible`-floor raised to no-extend, `seal →
  Sealed`, `freeze → Frozen`; the `is*` predicates report the level.
  `freeze`/`seal`/`preventExtensions` return the (now-restricted) object.
- **Enforcement** lives at the write paths already touched by 2a: `ObjSet`
  (new key) checks extensibility; `delete` checks `Sealed`; `ObjSet`
  (existing key) checks `Frozen`. A violation is a **`TypeError`** (matches
  JS strict mode and the VM's TypeError-on-misuse style; the sloppy-mode
  silent no-op is the pinned divergence).
- **Shallow**, exactly like JS (`Object.freeze` does not deep-freeze) —
  state it; it is a common wrong assumption.
- **Coarse / whole-object only.** This is the integrity *level*, not
  per-property descriptors: correct for `freeze`/`seal`/`preventExtensions`
  on a whole object and their predicates — the 99% case — without the
  descriptor machinery. Descriptor-accurate freeze (`isFrozen` deriving from
  per-property `writable`/`configurable`, partial sealing via
  `defineProperty`, accessor interaction) defers to the Step-4 descriptor
  tier; the edge cases are pinned in the ledger.
- **MVP receiver = `Object`.** Arrays/maps/sets are separate `Value`
  variants without an `ObjData` integrity field; `Object.freeze([…])` is
  deferred (pinned) until those heap structs carry the level too. The
  builtin prototypes/constructors are Objects, so the dogfooding case is
  fully covered.
- **JSON boundary:** the integrity level is metadata with **no JSON form** —
  a frozen object still serializes its data normally; drop the level on
  serialize. (No new `stack_value_to_json` rejection — unlike
  prototypes/methods, a frozen *plain* object is still data.)

Acceptance (2d):
- [ ] `Object.freeze(o)` then `o.x = 1` is a `TypeError`; `o` is returned;
      `Object.isFrozen(o)` is `true`. `seal` blocks add/delete but allows
      writing existing keys; `preventExtensions` blocks only new keys; the
      `is*` predicates agree.
- [ ] `freeze` is shallow (a nested object stays mutable).
- [ ] The user builtins and the 2a builtin-prototype freeze write the **same**
      `ObjData.IntegrityLevel` field (inspection / shared-helper).
- [ ] A frozen object round-trips through JSON as its plain data (level
      dropped), no rejection.
- [ ] `Object.freeze` on a non-`Object` receiver (array/map/set) is the
      documented "deferred" diagnostic, not a panic.
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

### Step 2e — own-property extensibility + one `get_property` / `set_property`

Two things land together because they share one resolution path: making the
heap "object" types **extensible** (arbitrary own props, as JS allows on
every object), and collapsing every type's property read onto **one ladder**
— which *absorbs* the bespoke RegExp field interception (`regexp_prop`,
`dispatch.rs:17`) rather than keeping it special. This extends Step 2c's
unified *method* resolution to the full *property* read **and write**, so the
phase lands on the [convergence target](#the-convergence-target-one-get-one-set):
one `get_property`, one `set_property`.

**The read ladder *is* `get_property`.** A named property read on any value
resolves through:

```
primary representation → user own-prop bag (if any) → virtual own props
  (computed, per-type) → type prototype (2a) → undefined
```

This one body subsumes and **deletes**: `resolve_property_from_top`,
`resolve_computed_property`, `resolve_closure_prototype`, `regexp_prop`, the
Step-2c method helper, and the inline read copies in `IndexGet`/`IndexSet`-`Old`
and `ObjHas`. `ObjGet`/`ObjPeek`/`IndexGet`/`ObjPeekDyn`/`GetMethodOrProp`
become stack-shape adapters that each call it once (static `field` vs.
computed `key`; pop vs. peek). `has_property` is the same walk returning a
bool at the first non-`undefined` rung.

**The write ladder *is* `set_property`** — the symmetric collapse, the half
2c/2d only gestured at. A named write on any value resolves the *destination*
through the matching rungs:

```
primary representation (in-place: array index, object map) → integrity gate
  (Step 2d: extensible/seal/frozen) → virtual own props with a setter
  (RegExp lastIndex, …) → user own-prop bag (materialize) → reject/no-op
  (primitives, non-extensible natives)
```

This subsumes and **deletes** the per-site write logic: `ObjSet`'s RegExp
`lastIndex` arm and Object-map arm, `IndexSet`'s array-index and object-map
arms, and the Step-2d enforcement checks (which become the integrity gate
*inside* `set_property`, not a fourth thing bolted onto each arm).
`ObjSet`/`IndexSet` become stack-shape adapters; `delete_property` (the
`ObjDelete` body) and `ObjExtend` are the same write path in bulk/remove form.
The `SetMode::Old`/`New` distinction (return previous vs. assigned value) is a
parameter of the one function, not a reason to fork it.

**`Object` already *is* the bag.** Its `ObjData.map` is the canonical
own-property store — the whole point of `Object` — so for an `Object` the
ladder's *primary representation* and *user own-prop bag* are the **same
structure** (the map), and a read stays *exactly* today's own-map → proto.
Extensibility is making the *other* object-types carry that same store as a
**second** structure, since their primary representation is something else
(array `elems`, the called code, the compiled regex).

`regexp_prop`'s `source`/`flags`/`global`/`lastIndex` become RegExp's
**virtual-own-prop rung**, not an `ObjGet` arm (`lastIndex` stays writable
via its existing `Cell` — a virtual prop with a setter). Function
`name`/`length`/`prototype`, array `length`, and collection `size` are the
same rung for their types. Rungs are **type-specialized**: a type pays only
for rungs it has, so the `Object` read above adds **no branch** over today.

**The object/primitive boundary** decides who can hold a bag at all — it is
the JS object/primitive line, nothing more:

| can carry own props (objects) | cannot (primitives) |
|---|---|
| `Object`, `Array`, `Closure`, `Bound`, `Builtin`, `Promise`, `RegExp`, `Map`, `Set` | `String`, `Bool`, number (`PosInt`/`NegInt`/`Float`), `Null`, `Undefined` |

`String` is the trap: `"x".foo = 1` is a no-op (sloppy) / `TypeError`
(strict) and `"x".foo` is `undefined`. Boxed-primitive prototypes (2b) give
primitives *method resolution*, never a writable bag.

**No-cost fast-path invariants (the load-bearing requirement).** The ladder
and the bags must never touch a hot path. Guaranteed by construction:

1. **Primary operations are the *first arm* of the canonical function, not a
   pre-check beside it.** This reconciles "adapters call `get_property` once"
   with "fast paths bypass the ladder": the integer-index/`Map.get`/`Set.has`
   case is rung 0 — `get_property` matches "array + integer key" (or string +
   integer), indexes `elems`, and returns *before constructing the ladder
   walk*. It is written to inline to exactly today's code, so `arr[0]` pays no
   function-call or key-type-dispatch penalty; the named-string path is the
   only one that descends the later rungs, and for collections it is already
   cold. (`length`/`size` keep their dedicated `GetLength`/`GetSize`
   instructions and do not enter `get_property` at all.) The rule is "one
   definition," not "one branch": the fast path lives *inside* the one
   definition as its first, returns-early arm.
2. **Bags are absent by default and never touched on a hit.** A value with no
   user props pays one `is_none()`/absent-lookup branch on a property *miss*,
   never on a hit, and never an allocation — the lazy pattern `ObjData.proto`
   / `Closure.prototype` already use.
3. **Rare-prop types use a side table, not a struct change.** `Array`/`Map`/
   `Set`/`Promise` keep their **bare** heap representation
   (`Vec<ThinVec<Value>>`, `Vec<IndexMap<…>>`, …) byte-for-byte; their rare
   own props live in a VM-side `HashMap<Ptr, Bag>` consulted only on the cold
   named-access path. The hot integer-index/`get`/`has` paths and the
   **alloc-count tests** stay untouched, with no per-instance memory added.
   `Object` needs nothing — its `ObjData.map` already *is* the bag — and
   `Closure`, where props are common and the heap struct is already rich,
   gets an **inline** `Option<Box<Bag>>`. (That same side table later
   carries an optional per-instance `proto` for collections —
   `Object.setPrototypeOf(arr,…)` — so the two ride together if ever wanted.)
4. **`Value` stays 16 bytes** — every bag lives on a heap entry or the side
   table, never in `Value`.
5. **The frozen-prototype IC is untouched** — method dispatch resolves via
   `CallBuiltin` (2a), never through the ladder, so extensibility adds zero
   cost to any call.

   The single rule behind all five: **the bag/ladder is consulted only after
   the operation's own primary representation has already missed** — which,
   on every fast path, it never does.

**Per-type bag rollout** (mechanism shared; placement by frequency):
- **`Object`: already a bag (`ObjData.map`) — the model, nothing to add.**
- **`Closure`: inline bag, now.** Gets `props: Option<Box<…>>` beside its
  `prototype`; `prototype`/`name`/`length` are virtual rungs, materialized
  into the bag only on reassign. With option (b) below, each function *value*
  has its own entry, so its bag and `.prototype` are correctly per-instance.
- **`Array`: side-table bag, corpus-gated.** Highest-value collection; until
  then `length` is virtual and a non-index named write is a `TypeError`
  (pinned).
- **`Map`/`Set`/`Promise`/`RegExp` arbitrary props: non-extensible (pinned).**
  Virtual rungs route through the ladder; a user-bag write is a `TypeError`.
- **`Builtin`/`Bound`: non-extensible native functions** — write `TypeError`,
  `Object.keys` → `[]`.
- **Primitives: never** — a write no-ops or `TypeError`s (pin one).

**Function identity — adopt JS semantics (option (b)).** Each *evaluation* of
a function literal produces a distinct object, exactly as JS does, so `===`,
own props, and `.prototype` are per-instance. This is **not** the hard path it
first looks like — the code makes it a near-simplification:

- The current sharing is a **load-time trick**, not a deep invariant:
  `for_program_with` (`methods.rs:300`) assigns one canonical `Closure` per
  unique `PushFn` address and bakes the ptr into the instruction. Capturing
  functions **already** get fresh-per-evaluation identity via `ClosureNew` +
  `alloc_closure`; option (b) just makes the non-capturing path behave the
  same.
- **Mechanism:** make the `PushFn` arm allocate a fresh closure entry (it
  already carries `addr`/`arity`/`prototype`) instead of pushing the baked
  ptr, and delete the canonical-patching loop — or, cleaner, **fold `PushFn`
  into `ClosureNew(addr, arity, [])`** and drop the instruction entirely, one
  fewer special case (on-theme with this phase). `strict_equal` already
  compares `addr && ptr` (`value.rs:328`), so distinct entries ⇒ distinct
  identity for free.
- **Two divergences fixed at once:** identity *and* the quieter shared-
  `.prototype` bug (today `f1.prototype === f2.prototype` for two evaluations
  of one literal, because the prototype is cached on the shared entry).
- **Recursion is unaffected** — a self-reference reads its binding *slot* (the
  stored `Value`), not a re-executed `PushFn`, so it still sees one object.
- **The only cost** is one allocation per function-*expression* evaluation
  (a `Vec` push), where non-capturing literals are free today. That is the
  *same* cost object/array/capturing-closure literals already pay, and the
  `closures` arena grows exactly as `objects`/`arrays` already do under the
  VM's accepted no-GC model — not a new unboundedness class. The visible
  fallout is **alloc-count tests**: the ones asserting the zero-alloc
  canonical scheme are updated to expect the per-evaluation alloc (call it out
  in the acceptance box; this is the one place 2e *does* change alloc counts).

The hard version we are **not** doing is "(b) without the alloc" — escape
analysis to keep sharing where identity provably never escapes. That is a
separate optimization, unnecessary for correctness, and corpus-gated.

**JSON boundary.** Functions/regexps/promises already have no JSON form
(reject, unchanged). An `Array` with non-index own props **drops** them on
`stack_value_to_json` (elements only, matching `JSON.stringify([1,2])`
ignoring `arr.foo`) — drop, not error. A plain `Object`'s props serialize as
today.

Acceptance (2e):
- [ ] **One `get_property` and one `set_property`** are the only property
      resolvers. `resolve_property_from_top`, `resolve_computed_property`,
      `resolve_closure_prototype`, `regexp_prop`, the Step-2c method helper,
      and the inline read/write copies in `IndexGet`/`IndexSet`/`ObjHas` are
      **gone** — folded into the pair (inspection / grep shows no second
      resolver). `ObjGet`/`ObjPeek`/`IndexGet`/`ObjPeekDyn`/`GetMethodOrProp`/
      `ObjSet`/`IndexSet`/`ObjDelete`/`ObjExtend` are stack-shape adapters that
      each call one of the pair exactly once.
- [ ] A builtin's missed-receiver fallback path calls `get_property`/
      `set_property` (not its own lookup) — one definition, shared by dispatch
      and builtins (inspection).
- [ ] One property ladder serves all heap types; `regexp_prop` is folded in
      as RegExp's virtual rung (the bespoke `ObjGet` arm is gone). An
      `Object` named read is byte-for-byte today's own→proto — no added
      branch on the hot path (codegen-shape / inspection).
- [ ] `f.x = 1; f.x === 1`; `Object.keys(f)` excludes
      `name`/`length`/`prototype`; `'x' in f`, `f.hasOwnProperty('x')`,
      `delete f.x` work.
- [ ] `"s".foo = 1` does not persist (`"s".foo === undefined`); no
      primitive bag is ever allocated.
- [ ] Integer array indexing, `Map.get`/`Set.has`, method calls, and
      `length`/`size` consult **no** bag or ladder rung — each is the
      returns-early first arm / a dedicated instruction (inspection).
      **Collection alloc-counts unchanged** (bare heap entries; the side table
      allocates only for a propped instance). The *one* deliberate
      alloc-count change is function creation (option (b), next bullet).
- [ ] **Function identity follows JS (option (b)):** two evaluations of one
      literal are `!==`, have independent own props, and independent
      `.prototype`; a self-recursive function still sees one object. The
      former zero-alloc-canonical alloc-count tests are updated to expect one
      alloc per function-value; `PushFn` is folded into `ClosureNew` (or made
      to allocate) and the canonical-patching loop is gone.
- [ ] `Math.max.foo = 1` and `boundFn.foo = 1` are `TypeError`s.
- [ ] An array with a non-index own prop serializes as its elements only.
- [ ] `Value` still 16 bytes.
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

## Step 3 — the mutability seam (still frozen, but the door)

Install the forward-compatibility seam for monkeypatchable builtins
**without** paying for it yet. This is what makes "frozen first" honest: a
later thaw is a localized change, not a redesign.

- Add a per-type `overridden: bool` (in the `TypeTag`-keyed table),
  default `false`. While `false`, the compiler's `CallBuiltin` is valid by
  construction — **no guard emitted, no runtime check**.
- Specify (do not yet implement) the thaw: when a builtin prototype is
  mutated, set its `overridden` flag; method calls on that type consult a
  V8-style guard (`if overridden[type] { proto-walk } else { CallBuiltin }`).
  The guard is a single predictable branch, off by default, and only ever
  flips for a type the program actually patches.
- Keep prototypes frozen (writes are `TypeError`) until the corpus shows a
  real need. This step is **documentation + the flag**, so the eventual
  mutable phase is a known, small diff rather than a fork in the road.

Acceptance:
- [ ] The `overridden` flag exists, defaults `false`, and is read nowhere
      on the hot path (no guard branch emitted while frozen).
- [ ] A doc comment at the table + a ledger entry specify the thaw path
      (set flag on mutation → guarded dispatch) so it is forward-compatible.
- [ ] No behavior change; prototypes remain frozen; suite + corpus green.
- [ ] Gate: `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

## Step 4+ — corpus-gated compat tiers (sketch, not yet specified)

From here, the **conformance corpus sequences the work** — build the
substrate above, then let measured failures choose the order. Each tier is
its own numbered sub-plan when it is picked up; sketched here only so the
shape of the iceberg is on record.

- **Property descriptors + getters/setters.** `Object.defineProperty`,
  enumerable/writable/configurable, accessor properties. The gateway tier
  (much else depends on a real property model). **Design note:** an accessor
  makes property *read/write* a call site — it can run user code, suspend,
  throw, and consume fuel. The structural seam for this is **already in place**
  from Step 2e: `get_property`/`set_property` return a *slot* (`Data` |
  `Accessor`), and the instruction — which owns frame-pushing — invokes the
  accessor on the StepResult path. So this tier *populates* the `Accessor`
  arm (and adds the descriptor model behind it) rather than re-signaturing the
  canonical pair. What it still must revisit: the `pe_*` purity classification
  and `ResumeMode` of the adapter instructions, now that their resolved slot
  can be a call. This remains the one tier with a real mechanism cost beyond
  "add a table entry," but the cost is bounded by the day-one seam.
- **Iterator protocol + `Symbol.iterator` + generators.** `for…of` over
  user iterables, spread of arbitrary iterables, `[...map.keys()]`. Needs
  `Symbol` (at least well-known symbols) first.
- **`Symbol`.** Well-known symbols (`Symbol.iterator`, `Symbol.toPrimitive`)
  before user symbols; a new primitive `Value` variant (watch the 16-byte
  budget) or an interned-handle representation.
- **`ToPrimitive` / `valueOf` / `Symbol.toPrimitive`.** The attitude-shift
  tier: `loose_equal` (`vm/value.rs:371`) **deliberately** refuses
  object↔primitive coercion (`[5] == 5` is `false` here, `true` in JS). Terminal
  compat re-litigates that and builds the coercion machinery. Highest
  semantic surface, lowest glue-code value — almost certainly corpus-late.
- **`Proxy` / `Reflect`.** Large surface, JSON-safe; a late but
  unobjectionable tier.
- **Boxed primitive identity** (`Object(5)`, `new Number(5)`, the
  `typeof === "object"` quirks). Step 2b gives method compat *without*
  boxing; true wrapper identity is a separate, low-value chunk.

---

## Divergence ledger (revisit under the terminal goal)

These are the deliberate divergences `value.rs`/earlier phases recorded as
*intentional* under the instrumental goal. Under the terminal goal each
becomes **debt with a pinned corpus test**, scheduled (or explicitly kept)
rather than silently tolerated:

- **`==` object↔primitive coercion** skipped (`[5] == 5` false) — value.rs:371.
  Revisited by the `ToPrimitive` tier.
- **No boxed primitives** — Step 2b deliberately keeps this (method compat
  without wrappers); full wrapper identity is a separate low-priority tier.
- **`Number` formatting** diverges on exponential form (`1e21`) and the
  `PosInt`/`NegInt`/`Float` split has accepted edge cases beyond the f64
  mantissa — value.rs:478. Audit when the corpus hits number-stringify.
- **`Builtin` `.length`** approximates JS fixed `.length` (`min_args − 1`),
  no "params before first default" — recorded in 13_OBJECTS Step 6.
- **Class field init scope** sees constructor params (direct-prepend
  lowering) where JS uses a separate scope — 13_OBJECTS Step 7a.
- **`static` / computed / private (`#x`) class members** — rejected with
  alternative-naming diagnostics under the instrumental goal; under the
  terminal goal these are *schedulable* (corpus-gated), no longer
  architectural exclusions.
- **`new Map()`/`new Set()`/`new RegExp()` — resolved in Step 2a Part 2**, not
  pinned: once constructors are callable function values and `New` accepts
  native constructors, `new Map()` constructs normally (this is the
  representation fix that makes `typeof Map === "function"` etc. hold). This
  flips the old `4_FUTURE` "no `new Map()`" decision deliberately. `new Date()`
  remains schedulable separately (no `Date` type yet).
- **`Object.isFrozen(Array.prototype)` is `true`** here vs `false` in JS
  (real-JS builtin prototypes are mutable) — Step 2d. Not a new divergence:
  it is the same deferred "builtin prototypes are frozen, not yet
  monkeypatchable" fact that already makes a write to `Array.prototype`
  throw, now observable through `isFrozen` too. **Auto-resolves when Step 3
  thaws** (extensible builtin prototypes → `isFrozen` reports `false`).
- **`freeze`/`seal` are coarse (whole-object integrity level, no
  descriptors)** — Step 2d. A freeze violation is a `TypeError` (JS strict
  mode) rather than a sloppy-mode silent no-op; `isFrozen`/`isSealed` report
  the coarse level, not a per-property-descriptor derivation; `freeze` on a
  non-`Object` (array/map/set) is deferred. Descriptor-accurate semantics
  land with the Step-4 descriptor tier.
- **Function identity now follows JS (option (b)) — divergence resolved, not
  pinned** — Step 2e. The earlier shared-non-capturing-closure scheme (one
  baked `ClosurePtr` per `PushFn` address) is dropped: each evaluation
  allocates a distinct closure entry, so `===`, own props, and `.prototype`
  are per-instance, matching JS. The only residual is a **performance** note,
  not a semantic one — one alloc per function-value (uniform with object/array/
  capturing-closure literals) and updated alloc-count tests. The corpus-gated
  *optimization* (not a correctness fix) is escape analysis to recover the
  zero-alloc share where identity provably never escapes.
- **Arbitrary own props are non-extensible on `Array`/`Map`/`Set`/`Promise`/
  `RegExp`/`Builtin`/`Bound`** — Step 2e. Legal in JS, rare in practice; a
  user-bag write is a `TypeError` (native functions resisting extension is
  arguably *more* faithful). `Array` is the first to get a real (side-table)
  bag if the corpus warrants. Primitive prop writes (`"x".foo = 1`) do not
  persist (no-op / `TypeError` — pin one).

## Out of scope (permanent — the guardrails, restated)

- Any feature requiring a **host callback mid-instruction** (breaks the
  condition system's suspension property). Compat does not buy this.
- **JSON forms** for prototypes, descriptors, methods, symbols, bound
  functions, or any reflective artifact — the boundary is invariant.

(Determinism is **not** here — it is a non-goal, see the guardrails
section. Nondeterministic builtins are freely compatible.)

# Phase 4 — Future work (curated backlog)

Not a phase to execute wholesale. Each item stands alone, with rationale and
rough size, ordered by expected value. Pick items individually; do not start
one without checking it still matches the code (Phases 0–3 may have shifted
details).

## 1. `await` / `async` — superseded

Superseded by `7_ASYNC.md` (real promises: outbox-batched tool calls,
blocking re-executing `Await`, VM-side cascading, optional task strands).
The transparent-await stopgap described here previously should **not** be
implemented — it would change `await tools.f()` from "unsupported" to
"silently synchronous" and then change semantics *again* when Phase 7
lands. Go straight to 7_ASYNC Tier 1. (Done: Tier 1 landed 2026-06-11.)

## 2. Memory bounds (medium)

Fuel bounds instructions, not bytes. A short loop can exhaust memory within
budget (string doubling `s = s + s`, `arr.push(big)`), and tool results can
be large. The heaps never reclaim, so a byte budget is a true high-water
bound.

- Add `VM::mem_budget: usize` (default on the order of 64–256 MB) and a
  running approximate counter, charged at allocation sites: array/object
  creation and growth (`ArrNew`, `ObjNew`, push/unshift, `IndexSet`
  inserting), string allocation (`RcStr` construction in Add-concat, ToStr,
  split/slice/join, JSON parse), closure/cell creation, and
  `json_to_stack_value` seeding.
- Charge approximations are fine (element count × `size_of::<Value>()` +
  string byte lengths); precision is not the point, the backstop is.
- Exceeding the budget is a new error kind, `RetrySameInstr`-style resumable
  only if the host raises the budget (mirrors `OutOfFuel`).
- Tests: string-doubling loop and array-push loop both hit the budget error
  well before the process allocates gigabytes; alloc-count tests unchanged.

## 3. Host-side rewrite-restart orchestration (design doc first)

The condition system's "rewrite program" restart, per the contract locked in
Phase 3: persist `state_to_json()`, recompile the rewritten source, seed a
fresh VM. The open work is host-side and belongs in the `agent` crate:

- A replay/cache layer so tool calls already resolved in the failed run are
  served from cache during re-execution (keyed by tool name + canonicalized
  args), making rewrite-restart cheap. This implies the rewriting prompt
  must tell the LLM which results exist in `state`/cache so it writes code
  that reuses them.
- Determinism requirements fall out of this: no `Math.random`/`Date.now`
  unless they are host-provided tools (and therefore cached). Decide and
  document before adding either as a builtin.
- Deliverable for this item is a short design doc in `agent/docs/`, not code.

## 4. Targeted JS-compat upgrades (small each — only on evidence)

Each is in the documented divergence list and deliberately deferred. Adopt
one only when a real LLM-written program is observed tripping on it, and
update the divergence list when done:

- Exponential float formatting (`1e21`-style) in `js_number_to_string`.
- Relational coercion across types (`1 < "2"`).
- `>>>` and ToInt32 bitwise semantics.
- NaN propagation in `Math.min`/`Math.max`; `Math.sign(±0)`.
- Out-of-range array write growing with holes instead of erroring (likelier
  fix: better error message, not JS semantics — the error catches real bugs).

**JS-named error kinds for the program-visible `e.name` (post-6B).**
`try`/`catch` (6_LANGUAGE Part B) materializes a caught VM error as
`{ name, message }` with `name` taken from the `ErrorKind` Debug name — and
`ValueError` is a *Python* name, not a JS one. JS-trained programs that
branch on `e.name` expect `RangeError` (negative/OOB index, shift count out
of range), `SyntaxError` (`JSON.parse` failure), etc.; only `TypeError`
already matches. When a transcript shows a program actually branching on
`e.name` (rather than inspecting `e.message` or catching blindly), split the
catchable `ValueError` sites into JS-named kinds — `RangeError`,
`SyntaxError`, keeping `ValueError` or an `Internal` kind for the
corrupt-pointer invariant sites that are `NotResumable` anyway — or, more
cheaply, map kinds to JS names per-site in `error_to_thrown`. Until that
evidence exists the coarse kinds stand: the message carries the
specificity, `ResumeMode` carries the actionability, and nothing host-side
branches on `kind` yet. (Uncaught program throws already have their own
`UncaughtException` kind, carrying the thrown value in `VMError::payload` —
that split was structural, not cosmetic, and is done.)

**Stdlib lang-items: move `Error`/`TypeError`/… into the prelude as classes
(post-Phase-13).** Once Phase 13's `class` (Step 7) and `instanceof` (Step 8)
land, the `new Error` compiler special-form (`compile_error_ctor` → bare
`ObjNew{name, message}`) and the `{name, message}` shape built by
`error_to_thrown` can be replaced by **prelude classes** —
`class Error { constructor(message){ this.name="Error"; this.message=message } }`,
`class TypeError extends Error {…}`, etc. `new Error(...)` then takes the normal
`New`/`NewReturn` path, `e instanceof Error`/`TypeError` works via the proto
chain, and `class AppError extends Error {}` falls out of Step 7b. This closes
the **`instanceof Error`** gap (the one divergence Step 8 leaves) and subsumes
the JS-named-error-kinds note above (the names become real subclasses).
The link from library entity to VM-internal concept is a **lang-item**
mechanism (cf. Rust `#[lang = "…"]`, Swift's underscored attributes, the JVM's
well-known classes): a fixed table of slots (`Error`, `TypeError`, …) that the
prelude fills — the compiler records each prelude class's prototype by
recognizing its known name (the prelude is trusted code) or a lightweight
`//@lang error` pragma, and `error_to_thrown` proto-links the object it
materializes to `lang_items[Error].prototype` (a cached ptr, lazily allocated
like any `F.prototype`) rather than special-casing the shape. Boundary:
lang-items suit *library-definable* concepts (Error is just an object shape +
proto); genuinely native types (`Map`/`Set`, arena-backed) can't move to the
prelude — a lang-item could still *name* their prototype for `instanceof`, but
the data structure stays native.

**`Map`/`Set` (evidence-gated, unlike `this`/`class` which are architectural):
start without them.** IndexMap-backed plain objects already cover ordered
string-keyed lookup, and Set has no stable JSON form at the program's
output boundary (a returned Set would come back as an array, or error).
The prior most likely to force the issue is `[...new Set(arr)]` as
dedup. If the repair-hint diagnostic plus the dialect prompt don't hold,
add them as **transient-only** values: compiler special-cases `new Map()` /
`new Set(x)` (no general `new`), full in-program behavior, but no JSON form —
erroring at the persistence boundary exactly as `Fn`/`Closure` values
already do.

Related small item, unconditional (not evidence-gated): **repair-hint
diagnostics for rejected syntax.** LLMs reflexively write `new Map()` /
`new Set()` / `new Date()` in glue code (`new Error(...)` is supported as
of 6_LANGUAGE Part B). Keep rejecting them, but make the `new` diagnostic
name the alternative per constructor: Map/Set → plain object or array,
Date → ISO strings or a host time tool.
(The `this`/`class` repair-hint applied *pre-Phase-13*: that decision is now
reversed — see `13_OBJECTS.md`, which adds an in-program-only object system
(`this`/methods/prototypes/`new`/`bind`/`instanceof`/`class`) that never crosses
the JSON boundary, exactly as `Closure`/`RegExp` don't today.)

Non-goals, recorded so they aren't relitigated: `ToPrimitive` on objects,
UTF-16 string semantics, generators,
a JS event loop / microtask queue (promises themselves exist as of 7_ASYNC
Tier 1, but scheduling is the deterministic strand model, not an event
loop), heap reclamation / GC (programs are short-lived;
memory is bounded by item 2). (`try`/`catch` was originally a non-goal; that
decision is reversed in `6_LANGUAGE.md` Part B — in-program handlers are the
inner layer of the condition system, with `raise` explicitly uncatchable.
Spread/rest, computed keys, and method shorthand are planned there too.)

## 5. Builtin surface growth — moved

Superseded by `5_BUILTINS.md`, which covers the calling-convention and
metadata cleanup, JS-contract fixes, and the full coverage backlog
(String/Array/Math/Object/Number/JSON). The only piece that stays here:
`Math.random` is blocked on the determinism decision in item 3.

## 6. Label-bound prelude helpers (small–medium)

Replace the prelude's textual `uses_method` source scan with exact,
by-construction usage tracking: bind each HOF helper (`__map`, …) to a label
id at its first call site, and compile the helper bodies at the end of the
unit, before `optimizer::finalize`. Call sites already keep label ids in
`Call` until the single backpatch pass, so referencing a helper's label
before its body exists resolves through the normal path — no relocation.

Design (settled 2026-06-10; the helper **JS sources stay exactly as they
are** in `interp/src/prelude.rs` — only the front end changes):

- **Lazy label memo, not a reserved id block.** Add a
  `[Option<u32>; N_HELPERS]` memo to `Compiler`. First time `compile_hof`
  needs a helper, allocate via `new_label()` and stash; later call sites
  reuse it. A reserved block 0..N also works but couples to the shared
  analyzer/compiler allocator (`compiler.next_label = result.next_label`,
  compiler/mod.rs) and to enum discriminant order — avoid.
- **Resolution by label, not name.** `emit_prelude_call` emits
  `Call(memo_label, n)` instead of resolving `__map` via
  `find_callee_label`. The `__`-named functions disappear from the
  program's scope: a user `function __map()` can no longer collide/shadow.
- **End-of-unit body emission.** After main compilation, for each used
  helper: parse + analyze its source as a standalone mini-unit (helpers are
  documented self-contained — no captures, no cross-helper calls — which is
  the invariant making standalone analysis sound), thread `next_label`
  through so mini-unit labels don't collide, emit `Instr::Label(memo_label)`
  then the body into the same stream. Split `reduce`/`sort` source blocks
  (currently two functions per block) into one source string per helper;
  call-site argc dispatch in `compile_reduce`/`compile_sort` is unchanged.
- **Span rebasing for diagnostics.** Append each compiled helper's source
  text to the stored full source and rebase that helper's spans by the
  append offset (known at emission time), so runtime errors inside a helper
  still render against real text — same end state as today's concatenation.
- **Delete** `prelude::assemble` and `uses_method`, and the pre-parse
  source concatenation in `compile`.

Wins: exact usage (no dead helper from `.map` in a comment or string
literal), no shadowable `__` names, user source parsed once. Execution
semantics byte-identical: same helper bodies, same static `Call` sites.

Acceptance checklist (finish all before declaring done):

- [ ] `cargo test -p interp` passes; `prelude.rs` tests for `assemble`/
      `uses_method` replaced by: program with `.map` in a comment/string
      compiles **without** the `__map` body; `findIndex` does not pull in
      `find`; `reduce` one-arg vs two-arg forms both work.
- [ ] New test: user program defining `function __map(a, f)` and also
      calling `arr.map(cb)` — both work, no collision.
- [ ] New test: runtime error raised inside a helper body (e.g. callback
      arity abuse or receiver mutation) renders a diagnostic pointing into
      the helper's source text (span rebasing correct).
- [ ] A program using no HOFs compiles byte-for-byte unchanged
      (existing tree-shaking guarantee preserved).
- [ ] `compiler/tests/hof.rs` passes unchanged.

## 7. Differential testing oracle (stretch)

Already sketched as the stretch goal of Phase 2 step 3: run a snippet corpus
through `node --eval` when available, compare final `state`. Valuable
precisely because of the divergence list — the harness needs a per-snippet
allowlist of expected divergences. Gate behind an env var so CI without node
skips it.

## 8. Optimizer pass: sort blocks by function (small, presentational)

The compiler emits nested function bodies inline (`Jump(after); Label(f);
body…; after:`), so a function's instructions are interleaved with its
nested functions' bodies; the optimizer passes preserve order, so this
survives to the final stream. The debugger's span-based attribution
(9_TUI Step 1) renders the interleaving correctly — repeated `── name ──`
headers where a parent resumes — but a contiguous-per-function layout
would give one header per function and a disasm window that shows more
of the *current* function.

Mechanism (in `optimizer::finalize`, before backpatch, while targets are
still label ids): stable-sort instructions by owning function — root
first, then functions by source-span start — preserving original order
within each function. Correct because:

- within a function, the only discontinuities in the emitted stream are
  exactly the nested bodies being cut out; at each seam the parent's
  `Jump(after)` and `Label(after)` become adjacent — a degenerate
  jump-to-next that `simplify_cfg` already deletes;
- the `Jump(after)` carries the function-node span, so attribution drags
  it along with the extracted child body, where it lands unreachable
  (the preceding block ends in `Return`) — so the pipeline is
  sort → re-run `simplify_cfg` → backpatch;
- root sorts first, keeping the entry at address 0.

Cost: `codegen_shape.rs` asserts exact instruction sequences, so every
expectation involving a function needs regenerating — mechanical churn,
which is why this should land in its own commit, ideally when shape
tests are being touched anyway. Acceptance: a new shape test asserting
each function's instructions form one contiguous range (derivable from
the debug table); disasm of the demo shows exactly one header per
function; full suite green.

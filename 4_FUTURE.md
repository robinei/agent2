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
lands. Go straight to 7_ASYNC Tier 1.

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

**`Map`/`Set` (evidence-gated, unlike `this`/`class` which are architectural):
start without them.** IndexMap-backed plain objects already cover ordered
string-keyed lookup, and Set's JSON lowering is type-unstable across the
persist/recompile restart (a Set stored in `state` would come back as an
array). The prior most likely to force the issue is `[...new Set(arr)]` as
dedup. If the repair-hint diagnostic plus the dialect prompt don't hold,
add them as **transient-only** values: compiler special-cases `new Map()` /
`new Set(x)` (no general `new`), full in-program behavior, but no JSON form —
erroring at the persistence boundary exactly as `Fn`/`Closure` values
already do.

Related small item, unconditional (not evidence-gated): **repair-hint
diagnostics for rejected syntax.** LLMs reflexively write `new Map()` /
`new Set()` / `new Date()` / `new Error(...)` in glue code. Keep rejecting
them, but make the `new` diagnostic name the alternative per constructor:
Map/Set → plain object or array, Date → ISO strings or a host time tool,
Error → `raise`. Same treatment for `this`/`class`: suggest plain objects +
functions, and note that durable `state` must be JSON-shaped (methods and
prototypes would not survive the persist/recompile restart cycle anyway —
that is *why* they are excluded, not just implementation cost).

Non-goals, recorded so they aren't relitigated: `ToPrimitive` on objects,
UTF-16 string semantics, `class`/`new`/`this`, prototype chains, generators,
real promises/event loop, heap reclamation / GC (programs are short-lived;
memory is bounded by item 2). (`try`/`catch` was originally a non-goal; that
decision is reversed in `6_LANGUAGE.md` Part B — in-program handlers are the
inner layer of the condition system, with `raise` explicitly uncatchable.
Spread/rest, computed keys, and method shorthand are planned there too.)

## 5. Builtin surface growth — moved

Superseded by `5_BUILTINS.md`, which covers the calling-convention and
metadata cleanup, JS-contract fixes, and the full coverage backlog
(String/Array/Math/Object/Number/JSON). The only piece that stays here:
`Math.random` is blocked on the determinism decision in item 3.

## 6. Differential testing oracle (stretch)

Already sketched as the stretch goal of Phase 2 step 3: run a snippet corpus
through `node --eval` when available, compare final `state`. Valuable
precisely because of the divergence list — the harness needs a per-snippet
allowlist of expected divergences. Gate behind an env var so CI without node
skips it.

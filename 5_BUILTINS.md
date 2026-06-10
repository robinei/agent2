# Phase 5 — Builtins overhaul

Unify the builtin calling convention, collapse the triple-maintained metadata
into one declarative table, fix the places where existing builtins diverge
from the JS contract, and grow coverage on String/Array/Math/Object/Number/
JSON.

**Sequencing:** flexible. Ideally after Phase 1 (paths below assume
`interp/src/builtin/`), and Phase 2's `run_state` harness makes the tests
here much cheaper to write. If Phase 3 has landed, error sites use
`vm.fail(kind, msg)`; if not, keep plain `VMError` variants and let Phase 3
sweep this file with the others.

## Ground rules

- Each step is a separate commit (`builtins:` prefix), green
  `cargo fmt && cargo clippy && cargo test` after each.
- For every behavior listed as a bug fix in Step 3: first write the test
  asserting the **JS** behavior (verify against node if unsure), watch it
  fail, then fix. Behavior changes and their tests land in the same commit.
- Update the JS-divergence doc block (top of `vm/instr.rs`) whenever a
  documented divergence is removed or a new deliberate one is added.

## Step 1: One argument-consumption mode

Today three styles coexist: `take_args::<N>` (clone + truncate, pads missing
slots with `Value::Null`), the `check_arity!` macro (a misnamed wrapper that
no longer checks arity), and raw `arg_base` + manual indexing + truncate
(`math_min`/`math_max`). Replace all of them with one shape:

- `Builtin::call` pops the args **once**, before dispatch:
  `let args: SmallVec<[Value; 8]> = pop_args(vm, argc)?;` (drain the stack
  region in push order; inline capacity 8 keeps every realistic call
  allocation-free — confirm the alloc-count tests in the perf suite still
  pass).
- Handler signature becomes
  `fn(vm: &mut VM, args: &[Value]) -> Result<Value, VMError>`; `call` pushes
  the returned value. Handlers never touch `vm.stack`.
- Absent optional args: handlers use a tiny accessor,
  `fn arg(args: &[Value], i: usize) -> &Value`, returning `&Value::Undefined`
  for out-of-range `i` — *undefined*, not `Null`, matching JS, where an
  absent parameter is `undefined`. Delete the `argc`-matching ladders
  (`array_join`, `str_split`, `str_slice`, …): a handler reads `arg(args, 2)`
  and applies the JS default-for-undefined rule.
- Variadic handlers just read `&args[1..]`.
- Delete `take_args`, `check_arity!`, and `arg_base`. Keep the central
  `min_args` check in `call` (Step 2 may relax individual minimums).

This also locks in the invariant Phase 3 wants: by the time any builtin
error is constructed, all operands are already consumed, so every builtin
failure can be `PushValueThenContinue`.

## Step 2: One declarative metadata table

The builtin list is currently maintained in three or four places: the
`Builtin` enum, the 170-line `meta()` match, the `call()` dispatch match, and
the compiler's lookup tables (`namespace_builtin` and the method tables in
`compile_method_call` — verify the full set by grepping the compiler for
`Builtin::`). Replace with a single `macro_rules!` table where each row
declares everything:

```
builtins! {
    // variant     kind                    name          min  max       handler
    ArrayPush,     method,                 "push",       1,   VARARG,   array_push;
    StrTrim,       method,                 "trim",       1,   1,        str_trim;
    Slice,         method,                 "slice",      1,   3,        slice;      // polymorphic
    MathAbs,       ns("Math"),             "abs",        1,   VARARG_OK, math_abs;
    ObjKeys,       ns("Object"),           "keys",       1,   1,        obj_keys;
    ...
}
```

The macro generates: the enum, `meta()` (now including the kind), `call()`
dispatch, and a lookup function the compiler consumes —
`Builtin::for_namespace(ns, name)` and `Builtin::for_method(name)` — so
adding a builtin is one table row plus one handler function. Rewire the
compiler's `namespace_builtin` and method-call lookup to these functions and
delete its local tables. Keep `min_args`/`max_args` counting the receiver,
as today.

**Method dispatch note:** method lookup is by *name only* (receiver types
are not statically known). Today array and string method names are disjoint;
Step 4 introduces collisions (`slice`, `indexOf`, `lastIndexOf`, `includes`,
`at`, `concat`). For those, the table has **one** receiver-polymorphic
builtin whose handler switches on the receiver value (string vs array →
TypeError otherwise). Merge rather than special-case: this is JS's own
shape (dynamic dispatch on the receiver).

## Step 3: Correct existing builtins to the JS contract

Catalog of known divergences to fix (audit the whole file for more while in
there). For each: test first, asserting what node does.

| Builtin | Today | JS contract |
|---|---|---|
| `arr.push(a, b, …)` | only first value pushed, surplus silently dropped (despite `meta` saying variadic) | append **all** arguments, return new length |
| `arr.unshift(a, b, …)` | same bug | prepend all (preserving argument order), return new length |
| `arr.pop()` / `arr.shift()` on empty | `ValueError` | return `undefined` |
| `s.split(d, limit)` | Rust `splitn`: remainder stays unsplit in last entry | JS: split fully, then truncate the array to `limit` entries |
| `s.split("")` | Rust `split("")` (empty leading/trailing entries) | array of single characters (per UTF-8 char here, documented) |
| `s.split()` (no/undefined delim) | arity error | `[s]` (one-element array) |
| split `limit` negative / non-int | `ValueError` | ToUint32 coercion (negative → huge → effectively no limit) |
| `parseFloat("3.14abc")` / `parseFloat("abc")` | `ValueError` (whole-string `str::parse`) | longest numeric prefix → `3.14`; no prefix → `NaN`. Accept `Infinity`/`-Infinity`. Never errors on a string |
| `Math.round(-0.5)` | Rust `round` = half away from zero → `-1` | JS rounds half toward +∞ → `-0`. Use `(n + 0.5).floor()` shape (mind ties and large values) |
| `Math.sign(±0)` | `signum` → `±1` | `±0` (remove this line from the divergence list when fixed) |
| `Math.min/max` with NaN operand | ignored (`f64::min/max`) | NaN propagates (returns NaN) |
| `s.slice(-3)` / `start > end` / OOB | `ValueError` | negative indices count from the end; everything clamps; `start ≥ end` → `""`. Keep only mid-codepoint as an error (byte-string divergence stays documented) |
| `push`/`unshift`/`indexOf` return values | `Value::Float` | use `int_value` consistently for integer-valued results (lengths, indices, `-1`) |
| optional-arg defaults via `Null` padding | `take_args` pads `Null` | gone after Step 1; absent → `Undefined`, and each handler applies the spec's undefined-default (`join(undefined)` → `","`, `indexOf` start undefined → `0`, …) |

Arity: `min_args`/`max_args` are **compile-time lint bounds**, not a runtime
contract — their consumer is `compile_builtin_call`'s static-call diagnostic
(`` `split` expects 1 to 2 argument(s), got 3 ``). Keep that strict: a
surplus or missing arg at a static call site is almost always a misremembered
API, and a compile error is the cheapest repair point for the LLM. What gets
**relaxed is the runtime check** in `Builtin::call`, which also guards the
dynamic `CallDyn` path (first-class builtins, HOF callbacks receiving
`(elem, i, arr)`): drop the runtime minimum to "receiver present" and let
handlers apply JS undefined-coercion for whatever is missing (`includes` /
`indexOf` / `startsWith` / `endsWith` coerce an absent needle to the string
`"undefined"`; `slice` start defaults to 0). Net effect: strict statically,
JS-faithful dynamically, one metadata table serving both.

## Step 4: Coverage growth

Follow the (new) one-row-plus-handler pattern. Suggested set, in priority
order — LLM-likelihood weighted. Receiver-polymorphic where marked (P):

- **String:** `replace` / `replaceAll` (string patterns only; a regex or
  function argument is a TypeError with a message saying regex/function
  replacers are unsupported), `toLowerCase` / `toUpperCase`,
  `padStart` / `padEnd`, `repeat`, `trimStart` / `trimEnd`, `charAt`,
  `at` (P), `concat` (P), `codePointAt` only if trivially defensible given
  byte-string semantics — otherwise skip.
- **Array:** `slice` (P), `indexOf` / `lastIndexOf` (P, `===` comparison),
  `includes` (P), `concat` (P), `reverse` (in place, returns receiver),
  `flat` (depth arg, default 1), `fill`, `at` (P), `join` exists. `splice`
  is genuinely useful but the trickiest (mutating, variadic, returns removed
  elements) — implement last, test hardest. `sort`: needs the comparator
  callback, so it goes through the **prelude** (`__sort(a, f)` helper JS,
  like `map`) with a default-comparator form `__sortDefault` (JS default sort
  is *string* comparison — implement faithfully); insertion sort is fine
  (short programs, and it's stable).
- **Prelude additions** (no Builtin rows; extend `prelude.rs` HOFS and the
  compiler method table): `flatMap`, `findLast` / `findLastIndex`.
- **Math:** `trunc`, `cbrt`, `exp`, `log` / `log2` / `log10`, `hypot`
  (variadic), `atan2`, the trig set (`sin` `cos` `tan` `asin` `acos` `atan`)
  — all one-liners via `math_unary`. **Not** `Math.random` (determinism —
  see 4_FUTURE item 3).
- **Math/Number constants:** `Math.PI`, `Math.E`, `Number.MAX_SAFE_INTEGER`,
  `Number.EPSILON`, `Infinity`, `NaN`. These are *member reads, not calls* —
  builtin rows don't cover them. Lower them in the compiler as constants
  (fold to `PushFloat`/`PushPosInt` where the namespace member is read;
  `compile_static_member` / `compile_identifier` are the sites). Check
  `Infinity`/`NaN`/`undefined` bare identifiers — some may already work.
- **Number / globals:** `Number.isFinite`, `Number.isNaN`, and bare-global
  `parseInt` / `parseFloat` / `isNaN` / `isFinite` aliases (LLMs write both
  forms; `compile_global_call` is the hook).
- **Object:** `Object.entries` (pairs nest as 2-element arrays — verify
  `for (const [k, v] of Object.entries(o))` destructuring works end-to-end
  and add the test), `Object.fromEntries`, `Object.assign` (variadic,
  returns the target).
- **JSON:** `JSON.stringify(value, replacer, space)` — LLMs write
  `JSON.stringify(x, null, 2)` constantly. Support `space` as number (clamp
  0–10) or string via `serde_json` pretty printing with a custom indent;
  `replacer` must be `null`/`undefined` (anything else: TypeError,
  "replacer is not supported").
- **console:** `console.log` / `warn` / `error` / `info` (all variadic,
  all appending to one stream). NOT an `Invoke` effect: a builtin that
  formats each arg (strings verbatim, everything else JSON-ish, depth- and
  length-truncated) into a size-capped `Vec<String>` buffer on the VM,
  returning `undefined`. The host reads the buffer for completion and
  condition reports (see 8_HARNESS Step 4) — it is a diagnostic stream,
  never a result channel, and never parsed for data. Exceeding the buffer
  cap drops oldest lines with a marker (do not error: logging must never
  kill a program). This is among the strongest LLM priors — prioritize it.

Each addition: table row, handler, behavioral tests including the
undefined-arg and wrong-receiver-type cases. For polymorphic ones, test both
receiver types plus the TypeError case.

## Step 5: Documentation sync

- Refresh the divergence list in `vm/instr.rs`: remove entries fixed in
  Step 3 (`Math.sign`, min/max NaN), keep byte-string and slice
  mid-codepoint notes accurate.
- Add a short "adding a builtin" recipe at the top of `builtin/mod.rs`:
  table row → handler → tests → (if a member-read constant or prelude HOF,
  where to go instead).

# Phase 5 — Builtins overhaul

Unify the builtin calling convention, collapse the triple-maintained metadata
into one declarative table, fix the places where existing builtins diverge
from the JS contract, and grow coverage on String/Array/Math/Object/Number/
JSON/console.

**Prerequisite status (verified against the code 2026-06-10):** Phases 1–3
have landed. Builtins live in `interp/src/builtin/mod.rs`. Error sites use
`vm.fail(ErrorKind::…, msg)`. The shared test harness is
`interp/src/testutil.rs` — key helpers: `run_ret(src) -> serde_json::Value`,
`run_val(src) -> Value`, `eval(expr)`, `eval_str(expr)`,
`run_err_kind(src) -> ErrorKind`. The JS-divergence doc block is at the top
of `interp/src/vm/mod.rs` (header "JS semantic compatibility — known
divergences", around line 113) — **not** `vm/instr.rs`.

## Ground rules — the per-commit gate

Every step below (and every Step 4 sub-step) is one commit and ends with
this gate. **Do not start the next step until every box is checked:**

- [ ] `cargo fmt && cargo clippy && cargo test` — all three green.
- [ ] Behavior changes and their tests are in the same commit. For every
      JS-contract fix: write the test first asserting the **JS** behavior,
      watch it fail, then fix. When unsure what JS does, check with
      `node -e 'console.log(<expr>)'` — do not guess.
- [ ] Commit message starts with `builtins:`.
- [ ] If a documented divergence was removed (or a new deliberate one
      added), the doc block in `interp/src/vm/mod.rs` was updated in the
      same commit.

## Step 1: One argument-consumption mode

**Current state (verified):** three styles coexist in
`interp/src/builtin/mod.rs`: `take_args::<N>` (line ~332; clones args,
truncates the stack, pads missing slots with `Value::Null`), the
`check_arity!` macro (line ~355; a misnamed wrapper that just delegates to
`take_args`), and raw `arg_base` + manual indexing (`math_min`/`math_max`).
`Builtin::call` (line ~257) checks `min_args`, then dispatches to handlers
of shape `fn(vm: &mut VM, argc: u32) -> Result<(), VMError>` which push
their own result.

**Do:** arguments are read **in place** on the stack — never moved, cloned,
or collected. No allocation at any argc.

1. Add a tiny `Copy` token plus accessors (in `builtin/mod.rs`):

   ```rust
   #[derive(Clone, Copy)]
   struct Args { base: usize, argc: usize }

   impl Args {
       /// Arg `i`, or `undefined` if absent — *Undefined*, not `Null`:
       /// in JS an absent parameter is `undefined`.
       fn get<'a>(&self, vm: &'a VM, i: usize) -> &'a Value {
           if i < self.argc { &vm.stack[self.base + i] } else { &Value::Undefined }
       }
       /// All args (arg 0 = receiver, deepest) as a read-only slice.
       fn slice<'a>(&self, vm: &'a VM) -> &'a [Value] {
           &vm.stack[self.base..self.base + self.argc]
       }
   }
   ```

   Each accessor call is a short-lived borrow of `vm`, so handlers can
   freely interleave reads with `&mut vm` uses; a value that must survive
   a `&mut vm` use (or be stored, e.g. `push` appending elements) is
   cloned individually — a refcount bump, only for the values actually
   kept.
2. `Builtin::call` does the underflow check (today's `arg_base` check:
   `vm.stack.len() >= argc`), builds `Args` once, checks `min_args`
   exactly as today (Step 3 relaxes it), and dispatches. Handler
   signature becomes
   `fn(vm: &mut VM, args: Args) -> Result<Value, VMError>`.
3. Epilogue in `call`, on **both** the `Ok` and `Err` paths:
   `vm.stack.truncate(args.base)`; on `Ok`, additionally push the
   returned value. This is the **single** place arguments are consumed —
   handlers never touch `vm.stack` directly.
4. Optional args: handlers read `args.get(vm, i)` and apply JS's
   default-for-undefined rule (`join` → `","`, `indexOf` start → `0`,
   `slice` end → length, `split` limit → none). Delete the
   `argc`-matching ladders in `array_join`, `str_split`, `str_slice`, and
   anywhere else they appear. Variadic handlers iterate
   `args.slice(vm)[1..]` (read-only) or index in a loop.
5. Delete `take_args`, `check_arity!`, and `arg_base` (its underflow
   check moved into `call`).
6. Keep the existing ``in `name`:`` error-prefix wrapper in `call` (line
   ~308) — it now wraps the new handler results the same way.

**Resume-invariant note:** Phase 3's pop-first invariant
(`vm/mod.rs:282`) operatively requires that when a resumable error
reaches `resume_with`, the failed instruction's operands are already off
the stack. The truncate-on-both-paths epilogue preserves this even though
the error object is now *constructed* while args are still on the stack:
`call` consumes them before the error propagates out of `step()`. In this
commit, update two comments in `vm/mod.rs` to match: the invariant
wording at line ~284 (say "consumed before the error propagates out of
the instruction", not "before the error is constructed") and the
classification-audit row at line ~300 that names
`take_args`/`check_arity!` (now: "args truncated by `Builtin::call`
epilogue on the error path").

**Intended behavior change in this step:** absent optional args switch from
`Null`-padding to `Undefined` + per-handler JS defaults. Everything else is
a pure refactor.

**Acceptance (plus the gate):**

- [ ] `grep -n "take_args\|check_arity\|arg_base" interp/src/builtin/mod.rs`
      → zero matches.
- [ ] `grep -n "vm\.stack" interp/src/builtin/mod.rs` → matches only in
      the `Args` accessors and in `Builtin::call` (underflow check +
      epilogue truncate/push).
- [ ] Tests (in `builtin` tests, via `testutil`): `[1,2,3].join()` →
      `"1,2,3"`; `"a,b".split(",")` → `["a","b"]`; `"abc".slice(1)` →
      `"bc"` — i.e. optional-arg defaults still work, now via the
      undefined rule.
- [ ] The alloc-count tests in `interp/src/compiler/tests/perf_allocs.rs`
      pass unchanged; add one in the same style asserting that a
      high-argc variadic call (e.g. `Math.max` with 12 arguments, in a
      loop) performs zero allocations per call.
- [ ] Resumability test: a builtin failure (e.g. `[].pop()` while it is
      still a `ValueError`, or a TypeError from a wrong-receiver call)
      leaves the stack with the operands consumed — `resume_with` a
      replacement value and run to completion successfully.

## Step 2: One declarative metadata table

**Current state (verified):** the builtin list is maintained in four
places: the `Builtin` enum, the ~170-line `meta()` match
(`builtin/mod.rs`), the `call()` dispatch match (`builtin/mod.rs:267`), and
two compiler tables — the method-name match in `compile_method_call`
(`compiler/mod.rs:2256`) and `namespace_builtin` (`compiler/mod.rs:2860`).
Grep the compiler for `Builtin::` to confirm the full set before starting.

**Do:** replace all of it with a single `macro_rules!` table where each row
declares everything:

```
builtins! {
    // variant     kind          name      min  max      handler
    ArrayPush,     method,       "push",   1,   VARARG,  array_push;
    StrTrim,       method,       "trim",   1,   1,       str_trim;
    MathAbs,       ns("Math"),   "abs",    2,   2,       math_abs;
    ObjKeys,       ns("Object"), "keys",   1,   1,       obj_keys;
    ...
}
```

The macro generates: the enum, `meta()` (now including the kind), the
`call()` dispatch, and the lookup functions the compiler consumes —
`Builtin::for_namespace(ns: &str, name: &str) -> Option<Builtin>` and
`Builtin::for_method(name: &str) -> Option<Builtin>`. Rewire
`compile_method_call` and the namespace path to these and delete the
compiler's local tables. Keep `min_args`/`max_args` counting the receiver,
as today.

**Method dispatch note:** method lookup is by *name only* (receiver types
are not statically known). Today array and string method names are
disjoint; Step 4 introduces collisions (`slice`, `indexOf`, `lastIndexOf`,
`includes`, `at`, `concat`). For those, the table has **one**
receiver-polymorphic builtin whose handler switches on the receiver value
(string vs array → TypeError otherwise). Merge rather than special-case:
this is JS's own shape (dynamic dispatch on the receiver).

**Acceptance (plus the gate):**

- [ ] `grep -n "namespace_builtin" interp/src/compiler/mod.rs` → zero
      matches; the `"push" => Builtin::ArrayPush`-style match in
      `compile_method_call` is gone; both sites call
      `Builtin::for_namespace` / `Builtin::for_method`.
- [ ] No hand-maintained `meta()` or `call()` dispatch match remains —
      both are emitted by the macro from the one table.
- [ ] Existing compiler diagnostics are unchanged: the static arity error
      (`` `split` expects 1 to 2 argument(s), got 3 `` style) and
      unknown-method handling still produce the same messages (existing
      compiler tests pass without edits to their expectations).
- [ ] Adding a builtin now means: one table row + one handler `fn` (Step 4
      will exercise this repeatedly).

## Step 3: Correct existing builtins to the JS contract

Work through this checklist one row at a time; each row = failing test
first, then the fix. Audit the whole file for further divergences while in
there. `int_value(f64) -> Value` already exists (`builtin/mod.rs:381`) —
use it for every integer-valued result (lengths, indices, `-1`).

- [ ] `arr.push(a, b, …)`: today only the first value is pushed, surplus
      silently dropped (despite `meta` saying variadic). Fix: append
      **all** arguments, return the new length.
      Test: `const a=[1,2]; const n=a.push(3,4); return [n, a];` →
      `[4, [1,2,3,4]]`.
- [ ] `arr.unshift(a, b, …)`: same bug. Prepend all, preserving argument
      order, return new length.
      Test: `const a=[3,4]; const n=a.unshift(1,2); return [n, a];` →
      `[4, [1,2,3,4]]`.
- [ ] `arr.pop()` / `arr.shift()` on empty: today `ValueError`; JS returns
      `undefined`. Test: `return [[].pop(), [].shift()];` →
      `[null, null]` via `run_ret` (undefined → JSON null), or assert
      `Value::Undefined` via `run_val`.
- [ ] `s.split(d, limit)`: today Rust `splitn` (remainder stays unsplit in
      the last entry); JS splits fully then truncates to `limit`.
      Test: `"a,b,c".split(",", 2)` → `["a","b"]`.
- [ ] `s.split("")` → array of single characters (per UTF-8 char here —
      documented byte-string divergence): `"abc".split("")` →
      `["a","b","c"]`, no empty leading/trailing entries.
- [ ] `s.split()` / `s.split(undefined)`: today an arity error at the
      dynamic level; JS returns `[s]`. (Static call sites: see arity note
      below.)
- [ ] `split` limit coercion: JS applies ToUint32 unless the limit is
      undefined. `"a,b".split(",", 0)` → `[]`;
      `"a,b".split(",", -1)` → `["a","b"]` (negative wraps huge →
      effectively no limit); `"a,b".split(",", 2.9)` → `["a","b"]`.
- [ ] `parseFloat`: today whole-string `str::parse` → `ValueError` on
      trailing garbage. JS: skip leading whitespace, take the longest
      numeric prefix; no prefix → `NaN`; accept `Infinity`/`-Infinity`;
      **never** errors on a string. Tests: `parseFloat("3.14abc")` →
      `3.14`; `parseFloat("abc")` → NaN; `parseFloat("  2.5")` → `2.5`;
      `parseFloat("Infinity")` → Infinity.
- [ ] `Math.round`: Rust `round` is half-away-from-zero; JS rounds half
      toward +∞. Tests: `Math.round(2.5)` → `3`; `Math.round(-2.5)` →
      `-2`; `Math.round(3.4)` → `3`; `Math.round(-0.5)` → `-0` (assert in
      Rust: `Value::Float(f)` with `f == 0.0 && f.is_sign_negative()`).
      Use the `(n + 0.5).floor()` shape but mind ties and magnitudes where
      adding 0.5 loses precision (|n| ≥ 2^52 is already integral — return
      it unchanged).
- [ ] `Math.sign(±0)`: today `signum` → `±1`; JS returns `±0`. Return the
      input unchanged when `n == 0.0`, else `signum` (NaN → NaN already).
- [ ] `Math.min`/`Math.max` with a NaN operand: today ignored
      (`f64::min/max`); JS propagates NaN. Test: `Math.min(1, NaN)` → NaN.
- [ ] `s.slice`: today negative/OOB/start>end are `ValueError`. JS:
      negative indices count from the end, everything clamps,
      `start ≥ end` → `""`. Tests: `"abcdef".slice(-3)` → `"def"`;
      `"abc".slice(2, 1)` → `""`; `"abc".slice(0, 99)` → `"abc"`. Keep
      only mid-codepoint as an error (the byte-string divergence stays
      documented).
- [ ] Integer-valued returns (`push`/`unshift` lengths, `indexOf` results
      including `-1`) come back as `int_value`, not `Value::Float` —
      assert variants via `run_val`.

**Arity model (do this last in the step):** `min_args`/`max_args` are
**compile-time lint bounds**, not a runtime contract — their consumer is
`compile_builtin_call`'s static diagnostic. Keep that strict: a surplus or
missing arg at a static call site is almost always a misremembered API, and
a compile error is the cheapest repair point for the LLM. What gets
**relaxed is the runtime check in `Builtin::call`**, which also guards the
dynamic `CallDyn` path (first-class builtins, HOF callbacks receiving
`(elem, i, arr)`): drop the runtime minimum to "receiver present" and let
handlers apply JS undefined-coercion for whatever is missing (`includes` /
`indexOf` / `startsWith` / `endsWith` coerce an absent needle to the string
`"undefined"`; `slice` start defaults to 0). Net effect: strict statically,
JS-faithful dynamically, one metadata table serving both.

- [ ] Static strictness kept: `compile_errs` test — `"a,b".split(",", 2, 3)`
      is still a compile error.
- [ ] Runtime relaxation: unit test in `builtin` tests that sets up a VM,
      pushes only a receiver `"undefined!"`, invokes
      `Builtin::StrIncludes.call(vm, 1)` directly, and gets `true`
      (absent needle coerced to `"undefined"`); same shape for `slice`
      with no args returning the whole string.

## Step 4: Coverage growth

Each sub-step is its own commit with the standard gate. Order is
LLM-likelihood priority; sub-steps are independent, but do them in order
unless blocked. For **every** added builtin, tests must cover: the normal
case, the absent-arg (undefined-default) case, and the wrong-receiver-type
TypeError case. For polymorphic (P) builtins: test both receiver types
plus the TypeError case. Verify expected values against node.

### 4a. console (strongest LLM prior — first)

`console.log` / `warn` / `error` / `info` — all variadic, all appending to
**one** stream. NOT an `Invoke` effect: a builtin that formats each arg and
appends one line per call to a size-capped `Vec<String>` buffer on the VM,
returning `undefined`. Formatting: strings verbatim, everything else
JSON-ish, depth- and length-truncated. Defaults (use these literally unless
something forces otherwise): per-line cap 4 KiB (truncate with a trailing
`…`), buffer cap 256 lines; when full, drop oldest and replace with a
single `[… N lines dropped]` marker line. `warn`/`error` lines get a
`[warn] `/`[error] ` prefix; `log`/`info` none. Exceeding the cap **never
errors** — logging must never kill a program. The host reads the buffer
for completion and condition reports (see 8_HARNESS Step 4); it is a
diagnostic stream, never a result channel, never parsed for data.

- [ ] `console.log("a", 1, [2])` produces one line `a 1 [2]`; returns
      undefined.
- [ ] Cap test: a loop logging >256 lines ends with 256 lines, the first
      being the dropped-marker; no error raised.
- [ ] A public accessor on `VM` exposes the buffer to the host (test reads
      it after `run`).

### 4b. String methods

`replace` / `replaceAll` (string patterns only — a non-string pattern or
replacement is a TypeError whose message says regex/function replacers are
unsupported), `toLowerCase` / `toUpperCase`, `padStart` / `padEnd`,
`repeat`, `trimStart` / `trimEnd`, `charAt`, `at` (P — array half lands in
4c; the handler switches on receiver from day one), `concat` (P, same).
Skip `codePointAt` unless trivially defensible given byte-string semantics.

- [ ] JS quirks covered by tests: `replace` replaces only the **first**
      occurrence, `replaceAll` all; `"ab".repeat(0)` → `""`,
      `repeat(-1)` → error in JS (RangeError → our ValueError);
      `"5".padStart(3, "0")` → `"005"`; `"abc".at(-1)` → `"c"`,
      `"abc".at(5)` → undefined; `charAt(5)` → `""` (note: `at` and
      `charAt` differ here — test both).

### 4c. Array methods

`slice` (P — **merge** with the existing `StrSlice` into one polymorphic
`Slice` row), `indexOf` / `lastIndexOf` (P, merge with existing `Str*`
variants; `===` comparison for arrays), `includes` (P, merge), `concat`
(P), `at` (P), `reverse` (in place, returns the receiver), `flat` (depth
arg, default 1), `fill`. `splice` is genuinely useful but the trickiest
(mutating, variadic, returns the removed elements) — implement it **last**
in this sub-step and test it hardest (insert-only, delete-only,
replace, negative start, OOB counts).

- [ ] Polymorphic merges done: `grep -n "StrSlice\|StrIncludes\|StrIndexOf\|StrLastIndexOf"`
      finds no separate string-only variants for the merged names.
- [ ] `[1,[2,[3]]].flat()` → `[1,2,[3]]`; `.flat(2)` → `[1,2,3]`;
      `[1,2,3].fill(0,1)` → `[1,0,0]`; `[1,2].concat(3,[4])` →
      `[1,2,3,4]`; `[1,2,3].at(-1)` → `3`.
- [ ] `splice`: `const a=[1,2,3,4]; const r=a.splice(1,2,9); return [a,r];`
      → `[[1,9,4],[2,3]]`, plus negative-start and overlong-delete cases.

### 4d. Prelude additions (no Builtin rows)

Extend the `HOFS` table in `interp/src/prelude.rs` and the compiler method
routing: `flatMap`, `findLast`, `findLastIndex`, and **sort**. `sort` needs
the comparator callback, so it goes through the prelude like `map`:
`__sort(a, f)` plus a default-comparator form `__sortDefault(a)` — the JS
default sort compares **as strings** (implement faithfully:
`[10, 9, 1].sort()` → `[1, 10, 9]`). Insertion sort is fine (short
programs, and it's stable). Sort is in-place and returns the receiver.

- [ ] `[10,9,1].sort()` → `[1,10,9]`; `[10,9,1].sort((x,y)=>x-y)` →
      `[1,9,10]`; sort returns the same (mutated) array.
- [ ] `[[1],[2,3]].flatMap(x=>x)` → `[1,2,3]`;
      `[1,2,3,2].findLast(x=>x<3)` → `2`;
      `findLastIndex` same predicate → `3`.

### 4e. Math functions

All one-liners via the existing `math_unary` shape: `trunc`, `cbrt`,
`exp`, `log` / `log2` / `log10`, the trig set (`sin` `cos` `tan` `asin`
`acos` `atan`); plus `atan2` (binary) and `hypot` (variadic). **Not**
`Math.random` — blocked on the determinism decision (4_FUTURE item 3).

- [ ] Spot-check tests: `Math.trunc(-1.9)` → `-1`; `Math.log(Math.E)` →
      `1`; `Math.hypot(3,4)` → `5`; `Math.atan2(1,1)` → π/4.

### 4f. Constants (member reads, not calls)

Bare `Infinity` / `NaN` / `undefined` identifiers **already work**
(`compile_identifier`, compiler/mod.rs:1096–1098) — don't redo them. What's
missing is namespace members: `Math.PI`, `Math.E`,
`Number.MAX_SAFE_INTEGER`, `Number.EPSILON`. Builtin rows don't cover
these — lower them in the compiler as constants (fold to
`PushFloat`/`PushPosInt` at the member-read site; `compile_static_member`,
compiler/mod.rs:1394, is the hook).

- [ ] `Math.PI` → 3.141…; `Number.MAX_SAFE_INTEGER` → 9007199254740991
      (assert integer `Value` variant via `run_val`).
- [ ] A member read in non-call position works: `const x = Math.PI; return x * 2;`.

### 4g. Number / globals

`Number.isFinite`, `Number.isNaN` (note: the `Number.` forms do **not**
coerce — `Number.isNaN("x")` → false, unlike bare `isNaN("x")` → true);
bare-global `parseInt` / `parseFloat` / `isNaN` / `isFinite` aliases (LLMs
write both forms; `compile_global_call`, compiler/mod.rs:2182, is the
hook).

- [ ] `Number.isNaN("x")` → false; `isNaN("x")` → true;
      `parseInt("42px")` → 42 via the bare alias.

### 4h. Object statics

`Object.entries` (pairs as 2-element arrays), `Object.fromEntries`,
`Object.assign` (variadic, returns the **target**, mutated).

- [ ] End-to-end destructuring test:
      `let r=[]; for (const [k, v] of Object.entries({a:1,b:2})) { r.push(k, v); } return r;`
      → `["a",1,"b",2]`.
- [ ] `Object.fromEntries([["a",1]])` → `{a:1}`; round-trip
      `fromEntries(entries(o))` equals `o`.
- [ ] `Object.assign(t, s1, s2)` returns `t` itself (later sources win).

### 4i. JSON.stringify with space

`JSON.stringify(value, replacer, space)` — LLMs write
`JSON.stringify(x, null, 2)` constantly. `space` as number (clamp to 0–10,
truncate fractional) or string (use its first 10 chars as the indent), via
`serde_json` pretty printing with a custom indent. `replacer` must be
`null`/`undefined` — anything else is a TypeError with message
"replacer is not supported".

- [ ] `JSON.stringify({a:1}, null, 2)` matches node's output exactly
      (two-space indent, `": "` separator).
- [ ] `JSON.stringify({a:1})` (no space) stays compact, unchanged from
      today.
- [ ] `JSON.stringify({a:1}, x => x, 2)` → TypeError.

## Step 5: Documentation sync

- [ ] Refresh the divergence list in `interp/src/vm/mod.rs` (the "JS
      semantic compatibility" doc block): remove entries fixed in Step 3
      (`Math.sign(±0)`, min/max NaN, slice clamping if listed); keep the
      byte-string and slice mid-codepoint notes accurate.
- [ ] Add a short "adding a builtin" recipe at the top of
      `interp/src/builtin/mod.rs`: table row → handler → tests → and where
      to go instead if it's a member-read constant (compiler) or a
      callback-taking method (prelude HOF).
- [ ] Standard gate.

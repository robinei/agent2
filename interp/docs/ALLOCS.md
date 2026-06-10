> **Status: HISTORICAL.** This document records the plan as designed at the
> time. The code has since evolved; where they disagree, the code and its
> module docs win. Known drift: Phase 3's `ThinString` newtype design was
> superseded by the `RcStr` approach (reference-counted inline string); same
> allocation-reduction goals achieved through a different mechanism. Phases
> 1, 2, 3, 4 all shipped; the post-phase baseline numbers reflect the
> `RcStr` implementation, not `ThinString`.

# Allocation reduction plan

Goal: cut runtime heap allocations in the VM and builtins, building on the
`HeapVal 72→16` / `Instr 32→16` size refactor (commit `e16f0d8`). That commit
achieved the layout win but forced a set of `String ⇄ ThinVec<u8>` conversions
that allocate-and-copy, and it sits alongside pre-existing
`collect::<Vec>().into()` double-allocations. This plan removes them in phases,
ordered low-risk-mechanical → structural.

Each phase is independently shippable and ends green on `cargo test`. Measure
between phases (see Phase 0).

---

## Invariants this plan relies on

- **Heap strings are immutable and always valid UTF-8.** Strings are JS
  primitives here: compared by content, never mutated in place (`Add` and the
  string builtins always allocate a *new* `HeapValue::String`; grep confirms no
  in-place mutation). This is what makes Phase 3 (skip revalidation) and Phase 4
  (share/intern constant strings) sound.
- **`Value` is `Copy` and 16 bytes.** Reading args by value off the stack
  is cheap and releases the borrow on `vm.stack`, which is what lets Phase 1
  read args directly then call `&mut self` heap allocators.
- **`heap[0]` is always the `state` object** (a placeholder is pushed even when
  state is `Null`). `state` is `Ptr(0)`. This anchors Phase 4's constant
  addressing.

---

## Phase 0 — Baseline & guardrail

Before touching anything:

1. Confirm `cargo test` is green.
2. Add a representative allocation-heavy benchmark to measure against — e.g. a
   program with a hot loop doing arithmetic builtins, string concatenation, and
   array/object construction. Either a `cargo bench` (criterion) or a cheap
   counting allocator wrapper (`#[global_allocator]` that bumps an atomic) gated
   behind a test/feature, reporting allocs-per-run.
3. Record the baseline numbers in this file so later phases can show deltas.

No production code changes in this phase.

**Baseline (2026-06-08):** 707 allocs for 100 iterations of Math.abs + string
concat (`alloc_baseline_hot_loop` test). Counting allocator: `CountingAlloc` in
`main.rs`, test-gated (`#[cfg(test)]` + `#[global_allocator]`).

**After Phase 1:** 607 allocs (−100, −14%). Eliminated per-builtin-call Vec
allocation (`pop_args` → `take_args`).

**After Phase 2:** 607 allocs (no change in hot-loop test; double-allocations
were in object/array construction and JSON paths not exercised by this
benchmark). Eliminated `collect::<Vec>().into()` and `as_bytes().to_vec().into()`
patterns.

**After Phase 3:** 410 allocs (−197 from Phase 1, −42% from baseline).
Eliminated UTF-8 revalidation on every string read (`heap_str`, `is_string`),
the `String` round-trip in `to_js_string`/`Add`/`ToStr`, and `as_bytes().to_vec().into()`
in string builtins. Created `ThinString` newtype with `Deref<str>` and
hand-written `Hash`/`Eq`/`Borrow<str>` for correct `IndexMap` key lookup.

**After Phase 4:** 410 allocs (no change in hot-loop test; Phase 4 affects
compiled programs, not raw-instruction tests). String literals are now interned
at compile time and pre-allocated at `heap[1..=N]`, sharing deduplicated slots.
`PushStr` is no longer emitted by the compiler; it remains available for
hand-written tests.

---

## Phase 1 — Builtins read args from the stack (no arg `Vec`)

**Target:** `pop_args` (`builtin.rs:299`) allocates a `Vec<Value>` on
*every* builtin call. In a hot loop (`Math.abs`, `arr.push`, …) that's one heap
alloc per call. Eliminate it.

**Design.** Since `Value: Copy`, a builtin can copy the few args it needs
out by value, truncate the stack to drop the arg region, then do its work
(including `&mut self` heap allocation) with no outstanding borrow.

Replace `pop_args` with two helpers:

```rust
/// Index of the first argument (deepest). Validates argc against stack depth.
fn arg_base(vm: &VM, argc: u32) -> Result<usize, VMError>;

/// Copy the top `argc` args into a fixed array (for fixed-arity builtins) and
/// truncate the stack. `Value: Copy`, so this is a memcpy of ≤N*16 bytes,
/// no heap allocation. Min arity is already guaranteed by `Builtin::call`.
fn take_args<const N: usize>(vm: &mut VM, argc: u32) -> Result<[Value; N], VMError>;
```

- **Fixed-arity builtins** (the majority: math ops, `str_slice`, `str_trim`,
  `json_*`, …): rewrite `let args = pop_args(...)?` →
  `let [a, b] = take_args(vm, argc)?` and index the array. No allocation.
- **Variadic builtins** (`Math.max`/`min`, `array_join`): fold over the slice
  `&vm.stack[base..]` by value (each element is `Copy`) without collecting, then
  `vm.stack.truncate(base)`. `array_join` only takes 1–2 *stack* args (it reads
  the array elements from the heap), so it's effectively fixed-arity too.
- Keep the `check_arity!` debug assertion behavior; it just routes to the new
  helpers.

**Borrow note:** read every needed arg by value *before* the first
`vm.alloc_*`/`&mut self` call. Don't hold a `&vm.stack[..]` slice across a heap
allocation.

**Risk:** low. Pure-local rewrite of each builtin body; semantics unchanged.
Touches `builtin.rs` only. Verify with the existing `call_builtin_*` tests.

---

## Phase 2 — Collapse `collect::<Vec>().into()` double-allocations

**Target:** several sites collect into a `Vec` and then `.into()` a `ThinVec`,
which **reallocates and copies** (ThinVec ≠ Vec layout, the buffer can't be
reused). `ThinVec: FromIterator`, so collect straight into it.

Sites:
- `vm.rs:1956`, `vm.rs:2148` — `ArrNew`: `self.stack.drain(split..).collect::<ThinVec<_>>()`.
- `vm.rs:1550`, `vm.rs:1608`, `vm.rs:2231` — `CallDyn`/`Arguments`/Raise arg
  regions: build the `ThinVec` directly from the slice instead of
  `.to_vec()`/`collect().into()`.
- `builtin.rs:707-708` (`obj_keys`): currently clones keys into
  `Vec<ThinVec<u8>>`, maps through `alloc_string` into `Vec<Value>`, then
  `.into()`. Collect the final `Value`s straight into `ThinVec`, dropping
  the intermediate `Vec`.
- `builtin.rs:726` (`obj_values`), `builtin.rs:490` (`array_join` parts) — same
  treatment.
- `builtin.rs:524,529,679,688` (`str_split`/`str_slice`/`str_trim`):
  `p.as_bytes().to_vec().into()` → `ThinVec::from(p.as_bytes())` (one alloc, not
  two). Folds into Phase 3 once stringify returns `ThinString`.
- `vm.rs:1131,1142` (`json_to_stack_value` array/object) — collect into
  `ThinVec` directly.

**Risk:** low, mechanical. Each change is allocation-count only, no semantics.

---

## Phase 3 — `ThinString`: a real string type in its own module

**Target:** every `heap_str` call does `str::from_utf8(s).unwrap()` —
revalidating known-valid UTF-8 on every string read (`IndexGet` on a string,
`to_number`, comparisons, …). `is_string` (`vm.rs:833`) validates an entire
string just to check the enum variant. And `to_js_string` returns an owned
`String` via `String::from_utf8(s.to_vec())` (alloc + copy), which callers then
re-wrap with `.into_bytes().into()` (another alloc + copy).

There's also a latent wart: `ThinStr` (`= ThinVec<u8>`) serves *two* roles —
string content (`HeapValue::String`, `PushStr`) and object keys
(`FieldName = ThinStr`). Because keys are raw bytes, every map lookup casts via
`field.as_bytes()` / `field.as_ref()` (vm.rs:1971, 1986, 2023, 2084, 2104,
2119, 2131).

**Decision: replace the `ThinStr` alias entirely with a `ThinString` newtype in
its own module, used for both roles.** A single UTF-8-guaranteeing string type
removes the revalidation, removes the stringify round-trip, *and* turns the
key-lookup casts into natural `&str` lookups.

**Module `thin_string.rs`.** Keep the surface focused: the type owns
*construction* + trait impls; read methods come for free via `Deref<str>`.

```rust
pub struct ThinString(ThinVec<u8>);

impl ThinString {
    pub fn new() -> Self;                         // empty
    pub fn with_capacity(n: usize) -> Self;
    pub fn from_utf8(v: ThinVec<u8>) -> Result<Self, _>;  // validates once
    /// Caller guarantees valid UTF-8. Used only where bytes provably came from
    /// a `&str`/existing `ThinString`.
    pub unsafe fn from_utf8_unchecked(v: ThinVec<u8>) -> Self;
    pub fn as_str(&self) -> &str;                 // no validation, zero-copy
    pub fn as_bytes(&self) -> &[u8];
    pub fn into_bytes(self) -> ThinVec<u8>;
    pub fn push_str(&mut self, s: &str);          // for Add/concat builders
}

impl Deref for ThinString { type Target = str; /* → split/trim/contains/… */ }
impl From<&str> for ThinString { /* the common constructor */ }
impl<'a> FromIterator<&'a str> for ThinString { /* join/concat without a temp String */ }
impl Borrow<str> for ThinString { /* enables IndexMap::get(&str) */ }
// Hash / PartialEq / Eq / Ord: HAND-IMPL, delegating to `self.as_str()`.
```

**Correctness pitfall (must not derive):** `str` and `[u8]` hash differently
(`str` writes a trailing terminator byte). For `IndexMap<ThinString, _>` lookups
keyed by `&str` to work, `Hash`/`Eq` **must** delegate to `as_str()`, matching
the `Borrow<str>` contract. Deriving `Hash`/`Eq` on the inner `ThinVec<u8>`
would silently break key lookup — call this out in review and add a unit test
asserting `map.get("k")` finds a key inserted as `ThinString::from("k")`.

Then:
- `pub type FieldName = ThinString;` and `HeapValue::String(ThinString)`. Drop
  the `ThinStr` alias in `vm.rs`/`compiler.rs`.
- `heap_str` → `Some(s.as_str())`, no validation.
- `is_string` → match the `HeapValue::String` variant only; no decode.
- Object lookups (`ObjGet`/`ObjSet`/`IndexGet`/`ObjHas`/`ObjDelete`): replace
  `obj.get(field.as_bytes())` / `field.as_ref()` with `obj.get(field.as_str())`
  / `obj.get(key_str)` — the byte casts disappear.
- `to_js_string` returns `ThinString` (or writes into a caller `&mut
  ThinString` via `push_str`). This removes the `String` round-trip at
  `vm.rs:1837` (`Add`), `2050`, `2173` (`ToStr`), and the builtin string ops —
  the produce-`String`-then-rewrap pattern collapses to one `ThinString`.
- Builtin string ops construct results with the module API:
  `ThinString::from(&s[start..end])`, `parts.iter().map(..).collect::<ThinString>()`
  for `join`, etc. — replacing the `as_bytes().to_vec().into()` dances
  (builtin.rs:524,529,679,688) flagged in Phase 2.
- `stack_value_to_json` / `state_to_json` (`vm.rs:1081,1098`): allocate a
  `String` only at the actual serde boundary (serde needs `String`), via
  `s.as_str().to_owned()` — no more `from_utf8(to_vec())`.

**Risk:** medium. Wide but mechanical; the compiler flags every site. Judgment
is confined to each `from_utf8_unchecked` use (restrict to provably-valid bytes)
and the hand-written `Hash`/`Eq`. Land as one atomic refactor.

---

## Phase 4 — (F) String literals as pre-allocated heap constants

**Target:** `PushStr` allocates a fresh heap string *every execution*
(documented at the `PushStr` definition). In loops and re-run programs, every
string literal / template fragment re-allocates each time. Pre-allocate them
once into the heap and reference by `Ptr`.

**Approach (per the agreed design — no separate "constants vector" instruction;
constants live in the heap and reuse `PushPtr`):**

1. **Program carries a constants table.** Add to `Program` (`compiler.rs:29`):
   ```rust
   pub constants: Vec<ThinString>,  // string literals; the loader wraps each
                                    // in HeapValue::String at heap[1..=N]
   ```
   (`ThinString` from Phase 3 makes the immutability/UTF-8 contract self-evident
   in the type; if the pool later needs non-string constants, widen to
   `Vec<HeapValue>` then.)
2. **Compiler interns string literals.** In `compile_expr`:
   - `StringLiteral` (`compiler.rs:~795`), template quasis
     (`compile_template`, `compiler.rs:~890`): intern the string into
     `constants` (dedup by content via a `HashMap<ThinString, u32>` on the
     `Compiler`), get its index `idx`, and emit
     `Instr::PushPtr(1 + idx)` instead of `Instr::PushStr(...)`.
   - Addressing: `heap[0]` is always `state`, constants occupy `heap[1..=N]`, so
     a constant's heap address is `1 + idx`, fully known at emit time. No
     backpatch needed (these are absolute addrs, not labels).
   - **Stop emitting `Instr::PushStr` from the compiler entirely.**
3. **Keep the `PushStr` instruction and its `step()` arm.** Existing
   `builtin.rs` / `vm.rs` unit tests construct `PushStr` by hand; leave it
   working. It just becomes dead in compiled output.
4. **Loader pre-populates the heap, in order** (`for_program`, `vm.rs:679`):
   1. push `heap[0]` = state placeholder object (as today),
   2. **append `constants` → `heap[1..=N]`** (compile-time addresses),
   3. seed state's nested values (these now start at `heap[N+1..]` — still
      dynamic, addresses are computed at runtime and stored in the state map, so
      the shift is transparent),
   4. fill `heap[0]` with the state entries (as today).
5. **Signature change.** `for_program` needs the constants. Change it to take
   the `Program` (or `code` + `constants`). Only test call sites are affected
   (`main.rs` doesn't execute programs); update them. Prefer
   `VM::for_program(program: Program, state)` consuming the program.

**Scope guard:** object keys and method names (`ObjNew`, `ObjGet`, `Invoke`,
`Raise`) are *not* `PushStr` and are out of scope here — they carry their
strings inline in the instruction. Interning those into the constant pool is a
candidate for Phase 5.

**Safety:** constants are immutable and may be freely shared across many
`PushPtr` references — strings are never mutated in place and compare by
content, so a shared literal slot is indistinguishable from per-push copies.
Interning (dedup) is therefore also safe and shrinks the heap.

**Risk:** medium. Confined blast radius (compiler emit + `for_program` + test
call sites), but it changes the heap layout contract, so land it on its own and
re-run the full suite, especially `for_program_seeds_state_at_heap0` and the
state-roundtrip tests.

---

## Phase 5 — Stretch / follow-ups (optional)

- **Intern object keys and `Invoke`/`Raise` names** into the constant pool too
  (extends Phase 4). Since Phase 3 unifies `FieldName` with `ThinString`, these
  reuse the same interning path; removes the inline string clones from
  `ObjNew`/`ObjGet`/`Invoke`/`Raise` and can shrink those instructions further.
- **`Add` concat writes directly into a `ThinString` buffer** instead of going
  through `format!` + reallocation (builds on Phase 3).
- **`json_to_stack_value` / `stack_value_to_json`**: avoid intermediate
  `String`s at the serde boundary where `&str` suffices.

---

## Suggested landing order

1. Phase 0 (baseline) →
2. Phase 1 (builtin args) + Phase 2 (collect→ThinVec) — mechanical, high-traffic,
   measure the delta →
3. Phase 3 (`ThinString`) — removes revalidation + stringify round-trips →
4. Phase 4 (constant pool) — removes per-run literal allocation →
5. Phase 5 as appetite allows.

Phases 1–2 are independent of 3–4 and can ship first for an early win.

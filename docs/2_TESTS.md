# Phase 2 — Test suite maintainability

Make the ~330-test suite cheap to extend and resilient to optimizer/codegen
changes, without losing any coverage. Runs after Phase 1 (tests already live
in `*/tests.rs` files inside the `interp` crate).

## Ground rules

- **Coverage is preserved, never traded for brevity.** Before starting,
  capture the baseline: `cargo test -- --list > /tmp/tests-before.txt`.
  Every deleted `#[test]` must be accounted for by a converted test or a
  table row; keep a running mapping in the final commit message.
- Production code does not change in this phase (except adding `#[cfg(test)]`
  helpers).
- After each step: `cargo test` green. Commit per step, prefix `tests:`.

## Step 1: Shared test harness

Add `interp/src/testutil.rs`, declared `#[cfg(test)] pub(crate) mod testutil;`
in `lib.rs`. Consolidate the helpers currently duplicated across the test
mods (`run_program`, `run_effect`, `run_err`, `run_last_str`, `run_vm`,
`state_val`, `num`, …) into one place:

- `compile_ok(src) -> Program` — compile or panic with rendered diagnostics.
- `compile_errs(src) -> Vec<String>` — rendered diagnostic strings.
- `run(src) -> VM` — compile + run to `Done`, panicking on any effect/error.
- `run_ret(src) -> serde_json::Value` — `run` + the program's top-level
  return value as JSON. This is the workhorse: most end-to-end tests
  become `assert_eq!(run_ret("return 1 + 2;"), json!(3))`.
  (8_HARNESS Step 0 replaces the `state` object with top-level `return`;
  if that hasn't landed yet, land it first — do not build the harness on
  `state_to_json` and convert twice.)
- `run_to_effect(src) -> (VM, StepResult)` — for Invoke/Raise tests.
- `run_runtime_err(src) -> VMError` — for runtime-error tests.

Then delete the per-file duplicates and point existing tests at `testutil`.
No assertion changes yet.

## Step 2: Convert end-to-end tests to `run_state` form and regroup

The bulk of `compiler/tests.rs` and `vm/tests.rs` follows compile → run →
inspect-heap. Convert these to `run_state` + `json!` assertions and regroup
into focused submodules (directory `compiler/tests/` with `mod.rs` if a
single file stays unwieldy):

- language basics (literals, operators, coercion, control flow)
- functions & closures (capture kinds, per-iteration cells, `arguments`)
- objects & arrays (literals, member/index access, destructuring, delete/in)
- builtins (string/array/object/Math/JSON/Number methods)
- higher-order methods (prelude lowering + behavior)
- state / invoke / raise (host boundary)
- diagnostics (unsupported syntax, undeclared vars — assert on rendered
  message substrings)

Conversion rules:

- A test asserting **behavior** (values in `state`, returned effects, errors)
  → `run_state` / `run_to_effect` / `run_runtime_err` form. Several small
  same-shape tests may merge into one table-driven test
  (`for (src, expected) in [...]`), provided each row's source still appears
  verbatim so failures are greppable.
- A test asserting **instruction shape** (exact `prog.code` sequences,
  "no `Instr::Add` remains", alloc counts) is an optimizer/perf test: keep
  it, but prefer the negative/contains form over exact full-stream equality.
  Full-stream `assert_eq!(prog.code, vec![…])` is allowed only where the
  stream is ≤ ~5 instructions; otherwise assert on the property the
  optimization guarantees. Move these into `optimizer/tests.rs` (or a
  `codegen_shape` submodule) so behavior suites contain no instruction
  assertions at all.
- Alloc-count tests (exact-count assertions against the counting allocator)
  move to their own submodule `perf_allocs`, unchanged — they are precise by
  design and must not be "simplified".
- VM-level tests that hand-assemble `Vec<Instr>` (no compiler involved) stay
  in `vm/tests.rs` — they pin the instruction contract and are immune to
  compiler churn. Keep them; don't convert them to source form.

**Acceptance:** `cargo test -- --list` count is ≥ the baseline minus merged
table tests, and the commit message maps every removed test name to its new
home; total test LOC drops noticeably (expect roughly 30–50%).

## Step 3 (separate, final): Coverage gaps

Only after steps 1–2 land. Add behavioral tests (in the new harness form) for
areas with thin or missing coverage. Check each against existing tests first
— some may already be covered:

- **Analyzer-sensitive behavior** (analyzer.rs has zero direct tests): capture
  of parameters, transitive capture through two closure levels, const-fn
  resolution edge cases (recursion, mutual reference), shadowing across block
  scopes, `var` vs `let` hoisting differences the analyzer models.
- **Documented JS divergences** (documented inline on the relevant
  instructions in `vm/instr.rs`; originally a header list there): each
  bullet should have a test pinning the *divergent* behavior so an accidental
  "fix" is caught — relational non-coercion, no ToPrimitive, negative-index
  and OOB-write array errors, strict arity, UTF-8 `.length`, i64 bitwise,
  NaN in Math.min/max, no `>>>`.
- **Resource guards**: OutOfFuel on an infinite loop; MAX_JSON_DEPTH on deep
  state seed and on `state_to_json` of a deep structure; cyclic `state`
  erroring (not hanging) on serialization.
- **Effect boundary**: multi-Invoke fan-out batching (order of calls and of
  pushed results), Invoke interleaved with Raise, fuel charged per batched
  invoke, resuming after Raise by manual ip/stack fixup (pins current
  semantics ahead of Phase 3 changing them).
- **String edge cases**: mid-codepoint indexing error, `split` with limit,
  empty-string needles for indexOf/includes/startsWith.
- **Control-flow corners**: `switch` fallthrough and `break`-only LoopCtx
  with `continue` skipping to the enclosing loop; `continue` in `do-while`;
  labeled break/continue rejection messages.
- Stretch (skip unless everything above is done): a differential harness that
  runs a corpus of snippets through `node --eval` when available and compares
  `state` — gated behind an env var so CI without node skips it.

**Acceptance:** each bullet either gets tests or a one-line note in the
commit message saying where it was already covered.

---

# Follow-up — close the gaps left by the Steps 1–3 landing (a1493da)

Steps 1–3 landed in a single commit (a1493da), together with 8_HARNESS
Step 0 as the Step 1 parenthetical directed (`state` → `input`, top-level
`return`, `Done { value }`). The harness, regrouping, and new coverage
tests are in; what did *not* happen:

- Step 2's **conversion** half: most migrated tests still use the
  pre-Step-0 idiom (write into `input.<key>`, read back via `input_val`)
  instead of `run_ret` + `json!`.
- Step 2's rule that **behavior suites contain no instruction assertions**
  — `Instr::` asserts remain in `effects.rs`, `objects_arrays.rs`,
  `functions_closures.rs`, and `coverage.rs`.
- Step 3's **audit trail** ("each bullet gets tests or a note saying where
  it was already covered") was never recorded, and several bullets got
  neither.
- The per-step-commit ground rule and the removed-test-name mapping in
  commit messages.

Terminology mapping against the steps above: `state` is now `input`,
`run_state` became `run_ret`, `state_to_json` is gone.

Ground rules are unchanged: coverage never drops, one commit per step
(prefix `tests:`), `cargo test` green after each step, and every
converted/moved/merged test is mapped in its step's commit message.

## Step 4: Finish the `run_ret` conversion

Convert the legacy `input.<key>` writeback tests to top-level `return`.
Approximate idiom counts by file (grep `input\.`): `functions_closures` 25,
`objects_arrays` 15, `codegen_shape` 6, `lang_basics` 5, `perf_allocs` 5,
`control_flow` 3, plus stragglers in `hof`, `effects`, `coverage`.

Conversion rules:

- `input.r = <expr>;` → `return <expr>;`, asserted with
  `assert_eq!(run_ret(...), json!(...))`. Tests checking several keys
  return one object: `return { i, s };`.
- Tests pinning exact `Value` variants (`PosInt` vs `Float`, `RcStr`
  identity) use `run_val`/`eval`, not `run_ret` — JSON erases the
  distinction. Don't silently weaken a variant assertion into a JSON one.
- **Keep the idiom, with a comment, where `input` itself is the subject**:
  `input_is_ptr_zero`, `for_in_over_input`, `for_program_seeds_*`,
  `input_with_null_seed_is_empty_object`, and member-assignment-through-
  `input` tests. These pin the input-object lowering and must keep
  exercising it.
- **`perf_allocs` is exempt.** Alloc counts are source-shape-sensitive;
  converting forces re-baselining for zero benefit. Leave it.
- **`codegen_shape` converts case-by-case.** Source text determines the
  emitted stream; a trailing store-to-`input` vs `Return` changes the
  instruction tail. Convert only where the shape assertion is unaffected.
- Afterwards, `run_program`/`input_val` in `compiler/tests/mod.rs` should
  only have the deliberate input-lowering tests as callers. Keep them
  `pub(super)` with a comment saying so.

**Acceptance:** in the behavior suites (`lang_basics`, `control_flow`,
`hof`, `objects_arrays`, `functions_closures`, `effects`), `input.` appears
only in the deliberate input-subject tests; test count unchanged; commit
message maps each converted test.

## Step 5: Purge instruction assertions from behavior suites

Enforce the Step 2 rule. Current violations
(`grep -l 'Instr::' interp/src/compiler/tests/*.rs`, minus `codegen_shape`):

- `effects.rs`: `tools_call_lowers_to_invoke`, `tools_call_with_no_args`,
  and `raise_lowers_to_raise_instr` are pure lowering tests → move to an
  "effects" section of `codegen_shape.rs`. The behavioral `*_yields_*`
  tests stay.
- `objects_arrays.rs`: `optional_call_reclaims_static_builtin` is a shape
  test → move to `codegen_shape.rs`.
- `functions_closures.rs`: the boxed-slot/`FreshCell` assertions
  (around lines 271–279) are shape tests → move to `codegen_shape.rs`.
  Where a test mixes behavioral and shape assertions, split it in two.
- `coverage.rs`: `string_mid_codepoint_index_errors`,
  `string_empty_needle_index_of`, and `out_of_fuel_stops_infinite_loop`
  hand-assemble `Vec<Instr>` with no compiler involved — per Step 2 those
  belong in `vm/tests.rs`. Move them there.

**Acceptance:** `grep -l 'Instr::' interp/src/compiler/tests/*.rs` hits
`codegen_shape.rs` only.

## Step 6: Dissolve `coverage.rs`

`coverage.rs` groups tests by when they were written, not by what they
test, and its doc comment points back at this plan. File each test into
its topical home and delete the module:

- `top_level_return_void_yields_undefined`, `top_level_return_object_values`
  → `lang_basics.rs`, next to `top_level_return_value` (currently in
  `functions_closures.rs` — move it too; top-level return is not a closure
  feature).
- `for_program_seeds_input_object`, `input_with_null_seed_is_empty_object`
  → `effects.rs`. Consider renaming `effects.rs` → `host_boundary.rs`:
  per Step 2's grouping it owns input + invoke + raise, and after this
  step it actually does.
- `continue_in_do_while_retests_condition`,
  `switch_break_only_continue_skips_to_enclosing_loop` → `control_flow.rs`.
- `labeled_statement_is_rejected` asserts on a rendered message →
  `diagnostics.rs`.
- The hand-assembled VM tests already moved to `vm/tests.rs` in Step 5.
- `compile_errs_renders_diagnostics`, `run_to_effect_yields_invoke`,
  `run_runtime_err_catches_type_error` test the harness itself → move into
  `testutil.rs` as a `#[cfg(test)] mod tests` at the bottom of that file.

**Acceptance:** `coverage.rs` deleted, `mod.rs` updated, test count
unchanged.

## Step 7: Step 3 audit trail + remaining coverage bullets

Record the audit Step 3's acceptance demanded. The commit-message ship has
sailed, so the mapping goes in an appendix table at the bottom of this
file: one row per Step 3 bullet → existing test name(s) and/or tests added.
Per bullet, current known state:

- **Analyzer-sensitive:** audit `functions_closures.rs` against the list —
  transitive capture through two closure levels, const-fn recursion and
  mutual reference, shadowing across block scopes, `var` vs `let`
  hoisting. Add tests for anything unpinned.
- **Documented JS divergences** (documented inline per instruction in
  `vm/instr.rs`; the header list no longer exists): cross-check
  each bullet against `vm/tests.rs` — coercion and bitwise pins exist
  (`loose_eq_*`, `bitwise`, `bit_shift_rejects_bad_count`, NegInt
  round-trip). Add pins for any divergence without one: relational
  non-coercion, no ToPrimitive, negative-index/OOB-write array errors,
  strict arity, UTF-8 `.length`, NaN in Math.min/max, no `>>>`.
- **Resource guards:** `OutOfFuel` landed in Step 3. Still missing:
  MAX_JSON_DEPTH on a deep `input` seed, and deep/cyclic top-level return
  values through `stack_value_to_json` (must error, not hang or panic).
- **Effect boundary:** batching order is already pinned in `vm/tests.rs`
  (`invoke_batches_consecutive`, `invoke_does_not_batch_across_other_ops`)
  — record that. `raise_yields_effect_and_resumes_as_expression` partially
  pins resume-after-Raise — record it, and add the missing pieces: fuel
  charged per batched invoke, and Invoke interleaved with Raise.
- **String edge cases / control-flow corners:** landed in Step 3; note
  that `split` with limit was already pinned by
  `builtin::call_builtin_str_split_with_limit`.

**Acceptance:** appendix table exists in this doc; every Step 3 bullet has
a row pointing at concrete test names.

## Step 8 (cosmetic, last): structural consistency

- `vm/tests.rs` wraps everything in an inner `mod tests` even though
  `vm/mod.rs` already declares it `#[cfg(test)] mod tests;`, yielding the
  redundant path `vm::tests::tests::*`. Drop the inner wrapper and
  de-indent.
- `builtin/` keeps its 51 tests inline in `mod.rs` while `compiler/` uses
  a test directory. Inline is acceptable at this size — if keeping it,
  add a one-line note here and stop. Don't reshuffle for symmetry alone;
  do move the tests to `builtin/tests.rs` if `mod.rs` grows past ~1000
  lines of production code.

**Acceptance:** test count unchanged; `cargo test` paths no longer contain
`tests::tests`.

---

# Appendix: Step 3 audit trail

Each Step 3 bullet mapped to its existing test(s) and/or tests added in
this follow-up (commit `tests:` after Step 7).

## Analyzer-sensitive behavior

| Bullet | Status | Test(s) | Location |
|--------|--------|---------|----------|
| Capture of parameters | ✅ Covered | `closure_captures_local` | `compiler/tests/functions_closures.rs` |
| Transitive capture (2+ closure levels) | ✅ Covered | `capture_through_intermediate_function` | `compiler/tests/functions_closures.rs` |
| Const-fn recursion | ✅ Covered | `self_recursion_is_static_with_no_slot` | `compiler/tests/codegen_shape.rs` |
| Const-fn mutual reference | ✅ Covered | `mutual_recursion_allocates_no_closures` | `compiler/tests/codegen_shape.rs` |
| Const-fn named-expr self-recursion | ✅ Covered | `const_named_fn_expr_self_recursion_is_static` | `compiler/tests/codegen_shape.rs` |
| Shadowing across block scopes | ✅ Covered | `sibling_block_shadowing_uses_distinct_bindings`, `const_propagation_respects_shadowing`, `const_elimination_respects_shadowing` | `compiler/tests/functions_closures.rs`, `compiler/tests/codegen_shape.rs` |
| `var` vs `let` hoisting (analyzer model) | ✅ Covered | `var_is_function_scoped_and_hoisted`, `captured_var_in_loop_is_shared_not_per_iteration` | `compiler/tests/control_flow.rs`, `compiler/tests/functions_closures.rs` |

## Documented JS divergences

| Bullet | Status | Test(s) | Location |
|--------|--------|---------|----------|
| Relational non-coercion | ✅ Covered | `relational_non_coercion` (cross-type) | `vm/tests.rs` |
| No ToPrimitive | ✅ Covered | `loose_eq_object_vs_primitive_not_coerced` | `vm/tests.rs` |
| Negative-index array errors | ✅ Covered | `arr_index_negative_errors` | `vm/tests.rs` |
| OOB-write array errors | ✅ Covered | `arr_set_oob` | `vm/tests.rs` |
| Strict arity (builtins only; user fns pad/drop, JS-like) | ✅ Covered | `builtin_arity_is_enforced_from_meta`, `missing_args_pad_to_undefined`, `arguments_beyond_declared_params` | `compiler/tests/diagnostics.rs`, `compiler/tests/functions_closures.rs` |
| UTF-8 `.length` | ✅ Covered | `string_utf8_length` | `vm/tests.rs` |
| i64 bitwise | ✅ Covered | `bitwise`, `bit_shift_rejects_bad_count` | `vm/tests.rs` |
| NaN in Math.min/max | ✅ Covered | `math_min_max_nan_ignored` | `vm/tests.rs` |
| No `>>>` | ✅ Covered | `unsigned_right_shift_is_rejected` | `compiler/tests/diagnostics.rs` |

## Resource guards

| Bullet | Status | Test(s) | Location |
|--------|--------|---------|----------|
| OutOfFuel on infinite loop | ✅ Covered | `fuel_stops_infinite_loop`, `out_of_fuel_stops_infinite_loop` | `vm/tests.rs` |
| MAX_JSON_DEPTH on deep `input` seed | ✅ Covered | `json_to_value_depth_limit` | `vm/tests.rs` |
| Deep return value through `stack_value_to_json` | ✅ Covered | `stack_value_to_json_depth_limit` | `vm/tests.rs` |
| Cyclic input/return erroring on serialization | ✅ Covered | `cyclic_value_serialization_errors` | `vm/tests.rs` |

## Effect boundary

| Bullet | Status | Test(s) | Location |
|--------|--------|---------|----------|
| Multi-Invoke batching order | ✅ Covered | `invoke_batches_consecutive`, `invoke_does_not_batch_across_other_ops` | `vm/tests.rs` |
| Invoke interleaved with Raise | ✅ Covered | `invoke_interleaved_with_raise` | `vm/tests.rs` |
| Fuel charged per batched invoke | ✅ Covered | `fuel_charged_per_batched_invoke` | `vm/tests.rs` |
| Resuming after Raise (manual ip/stack) | ✅ Covered | `raise_yields_effect_and_resumes_as_expression` | `compiler/tests/effects.rs` |

## String edge cases

| Bullet | Status | Test(s) | Location |
|--------|--------|---------|----------|
| Mid-codepoint indexing error | ✅ Covered | `string_mid_codepoint_index_errors` | `vm/tests.rs` |
| `split` with limit | ✅ Covered | `call_builtin_str_split_with_limit` | `builtin/mod.rs` |
| Empty-string needle indexOf | ✅ Covered | `string_empty_needle_index_of` | `vm/tests.rs` |
| Empty-string needle includes | ✅ Covered | `string_empty_needle_includes_starts_with` | `vm/tests.rs` |
| Empty-string needle startsWith | ✅ Covered | `string_empty_needle_includes_starts_with` | `vm/tests.rs` |

## Control-flow corners

| Bullet | Status | Test(s) | Location |
|--------|--------|---------|----------|
| `switch` fallthrough | ✅ Covered | `switch_basic_and_fallthrough` | `compiler/tests/control_flow.rs` |
| `break`-only LoopCtx, `continue` to enclosing loop | ✅ Covered | `switch_break_only_continue_skips_to_enclosing_loop` | `compiler/tests/control_flow.rs` |
| `continue` in `do-while` | ✅ Covered | `continue_in_do_while_retests_condition` | `compiler/tests/control_flow.rs` |
| Labeled break/continue rejection | ✅ Covered | `labeled_statement_is_rejected` | `compiler/tests/diagnostics.rs` |

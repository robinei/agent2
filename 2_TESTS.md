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
- `run_state(src) -> serde_json::Value` — `run` + `state_to_json`. This is
  the workhorse: most end-to-end tests become
  `assert_eq!(run_state("state.x = …"), json!({"x": …}))`.
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
- **Documented JS divergences** (the list at the top of `vm/instr.rs`): each
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

# Phase 14 — Post-OO cleanup

After the object-orientation push (Phase 13: `this`, prototypes, `new`,
`bind`, plain classes), the front-end and VM have accumulated the usual
post-feature residue: mechanical lints, a handful of now-dead symbols, and
one structural asymmetry — the analyzer is still a single 2,265-line file
while the compiler it feeds was split into a 14-file directory. None of this
is load-bearing; all of it is safe to clean before the next feature phase.

This phase is **behavior-preserving end to end.** Every task is a refactor,
a deletion of dead code, or a lint fix. If a change here alters observable
program behavior, it is wrong — back it out. The OO machinery itself is
explicitly out of scope: the `MethodOnObject` re-route signal
(`VM::method_receiver_error` → the two `b.call` sites →
`VM::reroute_method_to_object`) and the class→prototype codegen
(`compiler/class.rs`, which deliberately emits the same bytecode as the
hand-written `function C(){}; C.prototype.m = …` form) are correct and
documented. Do not "simplify" them.

## Ground rules (apply to every task)

- **No behavior change.** All three tasks are refactors / deletions / lint
  fixes. No new instructions, no semantics changes, no JSON-boundary changes.
- After each task: `cargo fmt && cargo clippy --workspace --all-targets &&
  cargo test` must pass **clean** (zero warnings, zero errors). Clippy is
  currently *not* clean — Task 1 is what makes it so; Tasks 2–3 must keep it
  so.
- One commit per task, message prefixed `cleanup:`.
- The alloc-count tests (exact-count assertions) must pass **unchanged**. A
  shifted alloc count means a refactor changed behavior — stop and find why.
- **Finish a task before starting the next.** Each `Acceptance` box is a
  gate. Tasks are independent, but landing them in order keeps each commit
  reviewable.

---

## Task 1: Mechanical lint sweep (`clippy --fix`)

Clippy reports ~110 lints across the workspace, almost all auto-fixable. The
test-build lints include hard **errors** (`never_loop`), so clippy does not
currently pass on `--all-targets` — this task is the prerequisite for the
"must pass clean" gate the other tasks rely on.

Known categories (counts as of this writing — treat as a guide, re-run to get
the live list):

- **89 collapsible-`if`** — `if a { if b { … } }` → `if a && b { … }`.
  Concentrated in `vm/` (~47), `analyzer.rs` (13), `compiler/call.rs` (6),
  `compiler/destructure.rs` (3), scattered elsewhere.
- **`never_loop` (errors, ~22 sites)** — test helpers and `vm/tests.rs` use
  `loop { match vm.step(…) { … => break } }` where the loop runs once.
  Clippy's own suggestion applies: `if let StepResult::Done { .. } =
  vm.step(u64::MAX).unwrap() { break }`. Primary site: `testutil.rs:136`.
- **Unnecessary same-type casts** (`usize -> usize`, `u32 -> u32`, ~8),
  **redundant closures** (2), **`push_str` of a single-char literal**
  (`vm/methods.rs` — use `push(',')`), **`map_or` simplifications** (2),
  **`&**v` / `as_ref().map(|v| &**v)` simplifications**, and a few doc-comment
  formatting lints (`doc list item without indentation`, `empty lines after
  doc comment`).

Steps:

1. Run `cargo clippy --workspace --all-targets --fix --allow-dirty` to apply
   the machine-fixable subset. Review the diff — clippy's autofix is reliable
   for these categories, but read it; do not commit blind.
2. Hand-fix the residue clippy won't auto-apply (the `never_loop` rewrites
   may need a manual touch where the loop body has more than the match).
3. For the `f32/f64::consts::PI` lint in `vm/tests.rs` (approximate-constant):
   either use `std::f64::consts::PI` or `#[allow]` it locally with a comment
   if the test deliberately wants a literal — your call, but leave it clean.
4. `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

**Acceptance:** `cargo clippy --workspace --all-targets` emits **zero
warnings and zero errors**. `cargo test` passes. The diff touches only the
lint sites — no logic changes. Alloc-count tests unchanged.

---

## Task 2: Delete dead symbols left by recent work

Clippy's `dead_code` pass flags symbols that are defined but never
constructed/read/called. Each below is a real orphan; delete it (and any
now-unreachable helper it was the sole caller of). Two subsets:

**Clear deletes (interp — no external consumer):**

- `analyzer.rs:23` — `ConstValue::Undefined` variant, **never constructed**.
  Remove the variant and any match arm that handled it.
- `compiler/mod.rs:247` — `Compiler.source` field, **never read**. Remove the
  field and its initializer.
- `vm/methods.rs` — `reroute_method_to_object`'s `_this_val: Value` parameter
  is threaded from both call sites (`dispatch.rs` `CallBuiltin` arm and
  `dispatch_call`'s `Builtin` arm) but **never used**. Drop the parameter and
  update both call sites. (The receiver is recovered from the stack inside the
  function; the param was vestigial.)

**Confirm-then-delete (agent crate — may be scaffolding for a later plan
step; grep the numbered docs before removing):**

- `host/mod.rs:351` — `is_done` / `is_awaiting_user` methods, never used.
- `host/registry.rs:21` — `is_done`, never used.
- `host/llm.rs:65` — `scripted_resume`, never used.
- `types.rs:186` — `output_schema` field, never read.
- `get_config_path` — never used.

For each agent-crate item: `grep -rn <name> docs/ agent/` — if a future plan
step (e.g. 8_HARNESS, 4_FUTURE) names it as an intended surface, leave it and
add `#[allow(dead_code)]` with a `// reserved for <doc/step>` note instead of
deleting. Otherwise delete.

Steps:

1. Delete the interp orphans (no consumers possible — internal crate).
2. For each agent orphan, run the grep, then delete-or-annotate.
3. `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

**Acceptance:** no `dead_code` warnings remain (every survivor carries an
explicit `#[allow(dead_code)]` + reason). `cargo test` passes. No behavior
change.

---

## Task 3: Split `analyzer.rs` into an `analyzer/` module (structural)

This is the one judgment-call task and the only one that is *uniformity*
rather than *correctness*. `interp/src/analyzer.rs` is a single 2,265-line
file; the compiler it feeds is a 14-file `compiler/` directory. The analyzer
already has clean internal phase boundaries — the static-analysis pipeline
(free functions) is separable from the AST walk (the `Analyzer` impl) — so
the split is mostly mechanical: move blocks, fix `use`/visibility, no logic
changes.

Suggested layout (adjust if a cluster doesn't fall cleanly):

- `analyzer/mod.rs` — `Analyzer` struct, `new`, `analyze_program`,
  `analyze_top_level`, `error`/`new_label`, the public entry point, and
  `pub(crate)` re-exports so `analyzer::Foo` paths elsewhere keep working.
- `analyzer/scope.rs` — `FuncScope` + impl, `SlotInfo`, `push_scope`, and the
  scope builders (`build_function_scope`, `build_arrow_scope`,
  `build_class_scopes`, `build_constructor_scope`).
- `analyzer/const_fns.rs` — `ConstValue`, `literal_const_value`,
  `resolve_const_functions`, `const_fn_binding_name`, `compact_const_fn_slots`,
  `register_const_fns`.
- `analyzer/captures.rs` — `resolve_captures`, `finalize_tables`.
- `analyzer/walk.rs` — the AST traversal: `analyze_hoist*`, `analyze_stmt*`,
  `analyze_var_decl`, `analyze_declare_pattern`, `analyze_register_name`,
  `analyze_expr`, `analyze_ref`, `analyze_assign*`, `analyze_chain_element`,
  `analyze_resolve_name`, `analyze_function_body`, `collect_params`.

While moving the walk, fold the two **`too many arguments (8/7)`** functions
(`analyze_declare_pattern` at `analyzer.rs:1439`, `analyze_register_name` at
`:1534`) onto a small `BindingCtx` carrier struct rather than an 8-ary
signature. This is the only signature change permitted in this phase, and it
is still behavior-preserving — verify by the unchanged test suite.

Steps:

1. Create `analyzer/` and move blocks one cluster at a time, compiling between
   moves (`cargo build -p interp`) so a broken `use` surfaces immediately.
2. Keep every item's visibility exactly as wide as it was — no narrower (other
   modules reach in), no wider.
3. Introduce `BindingCtx` for the two 8-ary functions during the `walk.rs`
   move.
4. `cargo fmt && cargo clippy --workspace --all-targets && cargo test`.

**Acceptance:** `analyzer.rs` no longer exists as a single file; `analyzer/`
mirrors the `compiler/` module shape. `cargo clippy` is clean (the two
`too_many_arguments` warnings are gone). `cargo test` passes with the **same
test count** as before the split — no test was dropped or duplicated by the
move. Alloc-count tests unchanged. No behavior change.

---

## Out of scope (deliberately not in this phase)

- The `MethodOnObject` error-as-control-signal mechanism. It is intentional,
  documented at every site, and re-routed at exactly two `b.call` locations.
  Phase 13 (Step 3) explicitly designated it the object-method dispatch arm —
  "do not duplicate it; generalize it." It is already generalized.
- Class / prototype codegen (`compiler/class.rs`). It adds zero new runtime
  paths by construction; there is nothing to consolidate.
- Any change that crosses the JSON boundary or adds/removes an instruction —
  those are feature work, not cleanup, and belong in a numbered feature phase.

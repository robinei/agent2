# Phase 1 — Project reorganization

Split the workspace into an interpreter crate and the agent crate, and break
the largest modules into directory modules with tests in sibling files.

**Decided:** one interpreter crate containing vm + compiler + analyzer +
optimizer + builtins + prelude + rc_str + diag. Do NOT split compiler and VM
into separate crates — they intentionally share crate-private internals.

## Ground rules

- **Purely mechanical.** No logic changes, no renames of types/functions, no
  test rewrites (that is Phase 2). Moves and visibility adjustments only.
- Use `git mv` so history follows files.
- After each step: `cargo fmt && cargo clippy && cargo test` green.
- One commit per step, message prefixed `reorg:`.

## Step 1: Create the `interp` crate

Target layout:

```
Cargo.toml                  (workspace: members = ["interp", "agent"])
interp/Cargo.toml
interp/src/lib.rs
interp/src/{vm.rs, compiler.rs, analyzer.rs, optimizer.rs,
            builtin.rs, prelude.rs, rc_str.rs, diag.rs}
agent/Cargo.toml            (depends on interp = { path = "../interp" })
agent/src/{main.rs, types.rs, tree.rs}
```

1. `git mv` the eight interpreter modules from `agent/src/` to `interp/src/`.
2. `interp/src/lib.rs` declares the modules and re-exports the public API.
   The public surface is exactly what a host needs; keep everything else
   `pub(crate)`:
   - `compile`, `Program`, `Diagnostic`
   - `VM`, `Value`, `Instr` (hosts may inspect/patch code), `StepResult`,
     `InvokeCall`, `VMError`, `DEFAULT_FUEL`
   - `RcStr` (exposed through `Value::String` and `Instr` operands)
3. Move the `alloc_counter` module and the `#[global_allocator]` from
   `agent/src/main.rs` into `interp/src/lib.rs`, still under `#[cfg(test)]`.
   It is only used by interp's unit tests; `agent` should not keep it.
4. Move the dependencies the interpreter uses (`oxc_*`, `indexmap`,
   `thin-vec`, `smallvec`, `serde_json`, `serde`) into `interp/Cargo.toml`.
   `agent` keeps `uuid`, `jiff`, `serde`, `serde_json`, `tempfile` and gains
   `interp`. Check with `cargo build -p agent` and `cargo build -p interp`
   that neither crate carries dependencies it doesn't use.
5. Fix `use crate::…` paths inside the moved files (they stay `crate::…`
   since they all moved together) and any `agent`-side references.

**Acceptance:** workspace builds; `cargo test` runs the same set of tests as
before the move (compare `cargo test -- --list | wc -l` before and after);
`agent/src/main.rs` is a thin stub.

## Step 2: Split `vm.rs` into a directory module

`interp/src/vm.rs` (~4.2k lines) becomes:

```
interp/src/vm/mod.rs      VM struct, CallFrame, step() and its macros,
                          StepResult, InvokeCall, VMError, DEFAULT_FUEL,
                          heap/frame helpers
interp/src/vm/value.rs    Value, SlotKind, Closure, FieldName, and the free
                          coercion helpers: js_number_to_string, float_is_int,
                          as_f64, as_i64, is_number, js_str_to_number,
                          num_loose_eq_str, small_to_thin
interp/src/vm/instr.rs    Instr, UpdateMode, SetMode, the type aliases
                          (CodeAddr, StackAddr, …) and the two big doc blocks
                          (JS-divergence list, stack-layout/closure contract)
interp/src/vm/tests.rs    the existing #[cfg(test)] mod tests, verbatim
```

- Cut/paste only; adjust `use super::…` / `pub(crate)` as needed so nothing
  becomes more public than before (items used across the vm submodules can be
  `pub(crate)` or `pub(super)`).
- `mod.rs` declares `mod value; mod instr; #[cfg(test)] mod tests;` and
  re-exports so external paths (`crate::vm::Value` etc.) are unchanged —
  no other file should need its imports edited.

**Acceptance:** no import changes outside `vm/`; test count unchanged.

## Step 3: Split `compiler.rs`, `builtin.rs`, `optimizer.rs` the same way

For each, the split is **code vs tests only** (further code splits are out of
scope unless trivially clean):

```
interp/src/compiler/mod.rs + compiler/tests.rs   (~2.9k + ~2.3k lines)
interp/src/builtin/mod.rs  + builtin/tests.rs
interp/src/optimizer/mod.rs + optimizer/tests.rs
```

`analyzer.rs`, `prelude.rs`, `rc_str.rs`, `diag.rs`, `tree.rs` stay single
files (their inline test mods are small; Phase 2 may regroup them).

Optional, only if it falls out cleanly while moving: extract the call-lowering
cluster (`compile_call`, `compile_args`, `compile_builtin_call`,
`compile_namespace_call`, `compile_global_call`, `compile_method_call`,
`compile_dynamic_method_call`, `compile_hof`, `compile_reduce`,
`emit_prelude_call`, `compile_user_call`) into `compiler/calls.rs` as
`impl Compiler` blocks. If it requires loosening visibility of more than a
couple of fields, skip it and note that in the commit message.

**Acceptance:** test count unchanged; `wc -l` of every non-test source file
is under ~3000.

## Step 4: Relocate design docs

Move `COMPILER_PLAN.md`, `PERF.md`, `ALLOCS.md`, `PROPAGATION_PLAN.md` into
`interp/docs/`. They document the interpreter, not the workspace. Leave the
numbered phase files (`0_…` – `4_…`) at the root.

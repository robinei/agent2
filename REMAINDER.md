# Remaining VM / instruction-set work (before Phase 2)

Pre-Phase-2 refactor that rectifies the issues found during Phase 1 (see the
discussion that produced this plan). Sequenced so each step builds + tests +
commits independently.

## Locked design decisions

- **Builtins are a stdlib layer, not instructions.** Everything invoked with
  *call* syntax (`arr.push(x)`, `Math.max(...)`, `Object.keys(o)`,
  `JSON.parse(s)`, string methods, …) becomes a `Builtin`, not a dedicated
  `Instr`. Operator/member/coercion primitives stay instructions.
- **`Builtin` enum is the id + registry.** `enum Builtin { ArrayPush, MathMax, … }`
  with `#[derive(Copy,Clone,Debug,PartialEq,Eq)]`, defined in `builtin.rs`. Two
  impls form the "static registry":
  - `const fn meta(self) -> BuiltinMeta { name, min_args, max_args }` —
    arity bounds **count the receiver** for methods; single source of truth the
    compiler reads for arity checks + error messages.
  - `fn call(self, &mut VM, argc) -> Result<(), VMError>` — dispatch.
- **Two call paths, sharing the registry:**
  - `Instr::CallBuiltin(Builtin, argc)` — the compiler's static fast path (what
    it emits in Phase 1, since the id is always known syntactically). No frame.
  - `StackValue::Builtin(Builtin)` invoked via `CallDyn` — first-class value for
    callbacks (`arr.map(Math.sqrt)`), needed by the Phase-3 prelude. Mirrors
    `Fn`: callable, `typeof` "function", identity equality, no JSON form.
- **Calling convention:** `argc` args on the stack left-to-right (arg 0 deepest;
  receiver is arg 0 for methods). The builtin pops exactly `argc` and pushes
  exactly one result, so every builtin call is a well-formed expression. This
  is the same shape the old instruction bodies already had, so migration is
  near-mechanical.
- **Mutators are value-producing.** `ObjSet`/`IndexSet` now leave the assigned
  value (DONE, step 3). Builtin mutators follow JS returns: `push`/`unshift` →
  new length, `pop`/`shift` → element.
- **Coercions are explicit instructions:** `ToStr` (kept), `ToNum`, `ToBool`
  (DONE, step 2). `String()/Number()/Boolean()` emit these — they are NOT
  builtins.

## Status

| Step | What | State |
|---|---|---|
| 1 | `Dig`/`Pick`/`JTrue` + `\|\|` rewrite | **DONE** — commit `f5458a1` |
| 2 | `ToNum`/`ToBool` + rewire `+x`/`Number`/`Boolean` | **DONE** — commit `37cf6bf` |
| 3 | `ObjSet`/`IndexSet` leave the value + simplify assignment | **DONE** — commit `dfd3843` |
| 4 | `Builtin` infra (`StackValue::Builtin`, `CallBuiltin`, `builtin.rs`) | **DONE** |
| 5 | Migrate the whole stdlib to builtins; remove instructions | **DONE** |
| 6 | `VM::state_to_json()` host boundary | **DONE** |
| 7 | Update `COMPILER_PLAN.md` | **DONE** |

## Done — ready for Phase 2

All steps complete. 164 tests pass (`cargo test --bin agent`).

### What changed

- **`builtin.rs`**: `Builtin` enum with 27 variants covering all call-shaped
  intrinsics (array/string methods, Object/JSON/Number/Array statics, Math).
  Each has `meta()` (arity bounds) and `call()` (runtime dispatch). Includes
  comprehensive tests covering every builtin via `CallBuiltin` and `CallDyn`.

- **`vm.rs`**: 
  - Added `StackValue::Builtin(Builtin)`, `Instr::CallBuiltin(Builtin, u32)`.
  - `CallBuiltin` execution arm in `step()`.
  - `CallDyn` restructured to dispatch `Builtin` without frame.
  - `Builtin` integrated into `is_truthy`, `to_number`, `to_js_string`,
    `stack_value_to_json`, `typeof`, `values_equal` value-shape arms.
  - Made `heap_arr`, `heap_obj`, `alloc_string`, `alloc_array`,
    `to_js_string`, `stack_value_to_json`, `json_to_stack_value`,
    `as_i64`, `float_is_int` `pub(crate)` for builtin access.
  - Added `pop_string_from` helper, `state_to_json()` public method.
  - Removed 29 dead `Instr` variants (array/string/object/math/JSON ops,
    `IsArr`/`IsInt`, `Abs`/`Sqrt`/`Ceil`/`Floor`/`Round`/`Sign`/`Min`/`Max`,
    `StrToInt`/`StrToFloat`) and their `step()` arms.

- **`compiler.rs`**: All intrinsic call sites now emit `CallBuiltin(Builtin, argc)`.
  `Math.max`/`Math.min` are variadic. `Number.parseInt`/`parseFloat` wired.
  Removed `compile_str_optarg` helper.

- **`COMPILER_PLAN.md`**: Updated Required VM changes, Intrinsics section,
  Accepted divergences, and Deferred built-ins.

/*
JS semantic compatibility — known divergences
=============================================

This VM models JS runtime semantics closely so that LLM-written JS lowers to it
without surprises. The following behaviors are JS-faithful and worth keeping in
mind: `undefined` is distinct from `null` (property/index/var misses yield
`undefined`); `==`/`!=` (LooseEq/LooseNeq) coerce while `===`/`!==` (Eq/Neq) are
strict; truthiness uses the JS falsy set (`false`, `0`, `NaN`, `""`, `null`,
`undefined`); `+` concatenates when either operand is a string and otherwise
adds with ToNumber coercion; `-`/`*`/`/`/`%` coerce ToNumber; division/modulo by
zero yield `Infinity`/`NaN` rather than erroring; objects/arrays compare by
reference identity under `===`.

The remaining intentional divergences from JS — deferred or accepted, NOT bugs:

  • Relational operators (`<` `>` `<=` `>=`) do NOT coerce across types: a
    number-vs-string or null-vs-number comparison is `false` (JS would coerce,
    e.g. `1 < "2"` is `true` in JS). String-vs-string and number-vs-number work.
  • `ToPrimitive` on objects is never performed. Arithmetic/`==` against a plain
    object or array is a TypeError / `false` (JS would call `toString`/`valueOf`,
    so `[5] == 5` and `[] + 1` differ here). `+` against a string still works,
    because that path uses ToString, which IS implemented.
  • Array bounds: an out-of-range read yields `undefined` (JS-faithful), but a
    negative index errors and an out-of-range *write* (`arr[len+k] = x`) errors
    rather than growing the array with holes as JS does.
  • Function arity is strict: reading an argument past those passed is an error,
    not `undefined`. The compiler is expected to pass exact arity (no implicit
    `arguments`, default, or rest-param holes).
  • Strings are UTF-8 byte sequences: `.length` and all index/offset string ops
    count/use UTF-8 *bytes*, not UTF-16 code units (`"é".length` is 2 here, 1 in
    JS; "😀" is 4 here, 2 in JS). ASCII text is identical.
  • Bitwise ops (`& | ^ << >> ~`) operate on full i64, not JS's 32-bit ToInt32
    semantics, and there is no unsigned right shift (`>>>`). Shift counts must be
    0..63 (JS masks to 0..31).
  • `Math.min`/`Math.max` (Min/Max) follow Rust's `f64::min`/`max`, which ignore
    a NaN operand; JS propagates NaN. `Math.sign` of ±0 is ±1 here (JS gives ±0).
  • Number→string uses Rust's float formatting for the non-integer path, so very
    large/small magnitudes are not rendered in JS's exponential form (`1e21`).
  • No exceptions/try/catch/throw: the `Raise` condition mechanism is for host
    (LLM) intervention, not JS error handling.
*/

use thin_vec::ThinVec;
use crate::builtin::Builtin;
use crate::rc_str::RcStr;
use super::value::{FieldName, SlotKind};

pub type CodeAddr = u32;
pub type StackAddr = u32;
pub type ArrayPtr = u32;
pub type ObjectPtr = u32;
pub type ClosurePtr = u32;
pub type LocalIndex = u32;
pub type ArgCount = u32;
/// Index into the VM's `cells` side table (the store of captured bindings).
pub type CellIndex = u32;

/// Whether an `IncLocal` is prefix (`++x`) or postfix (`x++`), controlling
/// whether the old or new value is left on the stack.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum UpdateMode {
    Prefix,
    Postfix,
}

/// Whether `ObjSet`/`IndexSet` leaves the new value (normal assignment) or
/// the old value (postfix `++`/`--` on non-local targets).
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SetMode {
    New,
    Old,
}

// Instructions for a stack based language used for LLM composition of complex tool flows.
#[derive(Clone, Debug, PartialEq)]
pub enum Instr {
    PushUndefined,
    PushNull,
    PushBool(bool),
    PushPosInt(u64),
    PushNegInt(i64),
    PushFloat(f64), // () -> any
    PushStr(RcStr), // () -> str
    PushArray(ArrayPtr),
    PushObject(ObjectPtr),
    PushFn(CodeAddr),
    PushBuiltin(Builtin),

    Pop(usize),

    // Generalized stack reach (Forth-like), counting from the top (0 = top).
    // Rejects reaching below the current frame's temporaries (frame_floor).
    // Pick is the read-modify-write workhorse (duplicate an lvalue's object/key
    // for a load-then-store); Dig reorders without copying. These subsume the
    // former Dup/Swap/Rot (Pick(0) ≈ Dup, Dig(1) ≈ Swap, Dig(2) ≈ Rot).
    //
    // Pick(n): copy the n-th-from-top value to the top.
    Pick(usize), // any^(n+1) -> any^(n+1), any
    // Dig(n): move the n-th-from-top value to the top, removing it from its
    // old position. Dig(0) is a no-op. Includes fast paths for n ≤ 2.
    Dig(usize), // any^(n+1) -> any^(n+1)

    // Drop `n` values directly below the top, leaving the top in place.
    // Nip(1) ≡ Dig(1); Pop; Nip(n) is the symmetric inverse of Dig(n).
    Nip(usize), // any^(n+1) -> any

    // calls function starting at address. the N arguments are passed on the
    // stack in left-to-right order (arg 0 pushed first / deepest), and become
    // the new frame's args. depends on function whether or not a result is left
    // on stack after it returns.
    Call(CodeAddr, u32), // any, ... -> [any]

    // indirect call: the callable sits on top, above its N args (left-to-right,
    // so arg 0 is deepest). The callable is either a bare `Fn` value or a `Ptr`
    // to a `Value::Closure`; pops it and calls with the same convention as
    // Call. For a closure, its captured environment is installed as the
    // callee's leading locals (slots 0..K) before the body runs. Errors if the
    // top value is neither a Fn nor a closure.
    CallDyn(u32), // any, ..., fn -> [any]

    // static call to a known builtin (the compiler's fast path, analogous to
    // Call for user functions). The N arguments sit on the stack left-to-right
    // (arg 0 deepest; receiver is arg 0 for methods); the builtin pops them and
    // pushes exactly one result. No call frame is created. See builtin.rs.
    CallBuiltin(Builtin, u32), // any, ... -> any

    // build a closure over the listed local slots of the current frame and push
    // a Closure to the resulting value. Each captured slot is copied
    // verbatim: a Boxed slot yields its Upval handle (shared, by-reference), a
    // Plain slot yields its current value (a by-value snapshot — which the
    // compiler only emits when the binding is provably never reassigned). The
    // captures are listed in the order the target body expects its upvals.
    MakeClosure(CodeAddr, ThinVec<LocalIndex>), // () -> fn

    // return from in-program function Call, returning the top N values (in
    // push order, so the first-pushed return value stays first).
    Return(usize),

    // Prologue frame setup, emitted as the first instruction of every function/
    // arrow body. Arguments arrive in place as the leading locals (the caller
    // pushed them; `fp` points at arg 0), so there is no per-argument copy.
    // `EnterFrame(nparams, build_args, local_kinds)`:
    //   - if `build_args`, eagerly materialize the `arguments` array from the
    //     actual args (before they are normalized) and cache it in the frame;
    //   - normalize the arg region to exactly `nparams` slots (drop surplus args
    //     / pad missing params with Undefined);
    //   - install the closure's captured environment (stashed by `CallDyn`) as
    //     the upval locals at slots [nparams, nparams + K);
    //   - allocate the declared (non-param) own locals from `local_kinds`, the
    //     Undefined for Plain, a fresh cell + Upval for Boxed, so the final
    //     layout is [params | upvals | locals]. The self-reference slot
    //     (named/recursive functions) is the last kind.
    // This is the sole frame-setup instruction: there is no separate per-arg
    // copy or local-allocation step.
    EnterFrame(u16, bool, ThinVec<SlotKind>),

    // Push the `arguments` array for the current frame: a fresh heap array of
    // all `arg_count` arguments (arg 0 first). Built lazily and cached per
    // frame (`CallFrame::arguments_cache`), so repeated references reuse the
    // same array rather than re-materializing it. Lowers the `arguments`
    // identifier. () -> arr
    Arguments,

    // load the local variable at the local index of the current stack frame, and push it onto the stack
    Local(LocalIndex),

    // pops the topmost value from the stack and writes to the local at the given index
    SetLocal(LocalIndex), // any -> ()

    // Stores the top of stack to a local without popping (like WASM's
    // `local.tee`): the value stays on the stack AND is written to the local
    // slot. Replaces the common `Pick(0); SetLocal` (formerly `Dup; SetLocal`).
    TeeLocal(LocalIndex), // any -> any

    // Re-box a (Boxed) local: allocate a fresh `cells` entry seeded with the
    // slot's current value and store a new `Upval` marker into the slot. Used to
    // give each loop iteration its own captured cell, so closures created in
    // different iterations capture distinct bindings (JS `let`/`const`
    // per-iteration semantics) even though the stack slot is reused. Seeding the
    // new cell with the current value carries a for-head variable forward to the
    // next iteration; for a fresh declaration the following `SetLocal` overwrites
    // it. () -> ()
    FreshCell(LocalIndex),

    // Increments or decrements a local variable in place. `p` is the value to
    // *subtract* from the variable: NegInt(-1) increments (sub −1 = +1),
    // PosInt(1) decrements (sub 1 = −1). Prefix mode leaves the new value on
    // the stack; Postfix leaves the old value. Only emitted for `++`/`--` on
    // local variables; member/index targets fall back to load-sub-store.
    IncLocal(u16, f64, UpdateMode), // () -> any

    // JS `typeof`: pops a value and pushes its type tag as a string. Tags match
    // JS exactly, so they are coarse: "undefined", "object" (covers Null, arrays
    // AND plain objects), "boolean", "number" (int or float), "string",
    // "function" (Fn or Closure). The fine-grained Is* predicates below stay for
    // the distinctions typeof erases (array-vs-object, int-vs-float, null) — they
    // are the lowering targets for Array.isArray, Number.isInteger, x === null.
    TypeOf, // any -> str

    // type predicates
    IsNull,  // any -> bool
    IsBool,  // any -> bool
    IsFloat, // any -> bool
    IsNum,   // any -> bool
    IsStr,   // any -> bool
    IsObj,   // any -> bool

    // temporary block markers. initially Jump and JFalse Addr refer to specific Label Addr(id),
    // but will get rewritten as code offset in a pass which eliminates Label instructions
    Label(CodeAddr), // () -> ()

    // unconditional jump to address
    Jump(CodeAddr), // () -> ()

    // pops the topmost value from the stack. jumps to the address if false
    JFalse(CodeAddr), // () -> ()

    // pops the topmost value from the stack. jumps to the address if truthy.
    // The truthy-mirror of JFalse, so `||` lowers without an extra Jump.
    JTrue(CodeAddr), // () -> ()

    // PEEKS (does NOT pop) the topmost value; jumps to the address when it is
    // neither null nor undefined, leaving the value in place. The "not nullish"
    // jump that lowers `??`, optional chaining (`?.`), and optional calls in one
    // instruction, instead of the former Dup + Push(Null) + LooseEq per check.
    JNotNullish(CodeAddr), // any -> any (peek)

    // EFFECT: invokes the named tool or function.
    // pops N arguments off the stack; args are taken in push order, so with
    // left-to-right codegen arg 0 is the deepest of the group (the first one
    // pushed). step() batches a run of consecutive Invoke instructions into one
    // StepResult::Invoke (fan-out); the host runs them concurrently and pushes
    // one result per call, in call order.
    Invoke(RcStr, u32), // any, ... -> any

    // EFFECT: raise condition (like Lisp condition system). used to ask LLM in calling frame
    // to decide how to proceed, using restarts like returning a value, aborting,
    // and even rewriting the program preserving already written variables with execution starting at arbitrary point.
    Raise(RcStr), // () -> any

    // pops N values where N is the number of field names, then pushes an
    // object with each field set to its corresponding value. Left-to-right:
    // field 0's value is the first/deepest pushed.
    ObjNew(ThinVec<FieldName>), // [any, ...] -> obj
    ObjGet(FieldName),          // obj -> any
    /// Sets the field and leaves a value on the stack (assignment is an
    /// expression). In `New` mode leaves the assigned value; in `Old` mode
    /// reads and leaves the previous value. Statement-context callers follow
    /// `New` mode with `Pop(1)`.
    ObjSet(FieldName, SetMode), // obj, any -> any

    // Runtime-polymorphic computed access `x[k]` / `x[k] = v`. A variable-keyed
    // index has no static type to choose array/object/string access, so these
    // inspect the container at runtime: array+int -> element (OOB read ->
    // undefined, OOB write -> error, negative -> error); object -> string-key
    // property (ToString the key; missing -> undefined); string+int -> the
    // character at that UTF-8 byte offset as a 1-char string (OOB -> undefined,
    // mid-codepoint -> error). They supersede the old type-specific
    // ArrGet/ArrSet/ObjGetDyn/ObjSetDyn. The static-name ObjGet/ObjSet remain
    // the fast path for `obj.foo`/`state.foo` (no per-access heap-string alloc).
    IndexGet, // container, key -> any
    /// Like `ObjSet` with `SetMode`: `New` leaves the assigned value, `Old`
    /// reads and leaves the previous value. Statement callers `Pop` the `New`
    /// result.
    IndexSet(SetMode), // container, key, value -> any
    // object enumeration / membership (JS Object.keys / Object.values,
    // `key in obj`, `delete obj[key]`). Keys/values are returned in insertion
    // order (IndexMap-backed). ObjDelete pushes whether the key was present.
    ObjHas,    // obj, str -> bool
    ObjDelete, // obj, str -> bool

    // pops N values and pushes an array with them as initial values.
    // Left-to-right: the first/deepest pushed becomes element 0.
    ArrNew(u32), // [any, ...] -> arr
    ArrLength,   // arr|str -> int

    // JS `String(x)` / ToString: pops any value, pushes its string form. Unlike
    // StrFromJson (which emits JSON, and rejects non-JSON values), this matches
    // template-literal / string-coercion semantics: numbers print without a
    // trailing ".0", arrays join with "," (null/undefined holes → ""), plain
    // objects → "[object Object]", null/undefined → "null"/"undefined".
    ToStr, // any -> str

    // JS `Number(x)` / ToNumber: pops any value, pushes its numeric form
    // (null→0, undefined→NaN, bool→0/1, strings parse, unparseable→NaN). The
    // coercion target for unary `+x`, mirroring the arithmetic operators'
    // implicit ToNumber. An array/object/function is a TypeError (no ToPrimitive).
    ToNum, // any -> num
    // JS `Boolean(x)` / ToBoolean: pops any value, pushes its truthiness as a
    // bool. The coercion target for `Boolean(x)` and `!!x`.
    ToBool, // any -> bool

    // unary operators. pops the topmost value from the stack,
    // operates on it and then pushed the result to the stack
    Neg,    // num -> num
    Not,    // any -> bool
    BitNot, // int -> int

    // binary operators. first pops rhs then lhs off the stack,
    // then operates on them pushing result to the stack
    Add,      // num|str, num|str -> num|str
    Sub,      // num, num -> num
    Mul,      // num, num -> num
    Div,      // num, num -> num
    Mod,      // int, int -> int
    Eq,       // any, any -> bool  (JS `===`: strict, structural, no coercion)
    Neq,      // any, any -> bool  (JS `!==`)
    LooseEq,  // any, any -> bool  (JS `==`:  coercing — see VM::loose_equal)
    LooseNeq, // any, any -> bool  (JS `!=`)
    Lt,       // any, any -> bool
    Gt,       // any, any -> bool
    LtEq,     // any, any -> bool
    GtEq,     // any, any -> bool
    And,      // any, any -> any
    Or,       // any, any -> any
    BitAnd,   // int, int -> int
    BitOr,    // int, int -> int
    BitXor,   // int, int -> int
    BitLhs,   // int, int -> int
    BitRhs,   // int, int -> int
    Pow,      // num, num -> num
}

/*

Stack layout:
    higher addresses
    ┌──────────────────────┐
    │  expr temporaries    │  ← sp
    ├──────────────────────┤
    │  declared locals     │  fp + nparams + K ..
    │  upvals (K)          │  fp + nparams .. fp + nparams + K - 1
    │  params (= args)     │  fp .. fp + nparams - 1
    │  local 0 / arg 0     │  fp
    ├──────────────────────┤
    │  caller's temps      │
    └──────────────────────┘
    lower addresses

There is no separate "argument" region: `fp` points at arg 0, and the arguments
ARE the leading locals (slots `0..nparams`), so a parameter reference is just a
`Local`. The caller pushes args left-to-right (arg 0 deepest at `fp`); the
prologue `EnterFrame` then normalizes the region to exactly `nparams` (dropping
surplus / padding missing), installs the closure's upvals at `[nparams, nparams
+ K)`, and allocates the declared locals above them. The whole frame —
including args — is reclaimed by `Return`, whose result(s) land at `fp`.


Closures — the compiler contract
================================

The VM gives you capture-by-reference (JS `let`/`var` semantics) via three
moving parts: `Boxed` local slots, the `cells` side table, and `MakeClosure` /
`CallDyn`. The runtime stays dumb; the analysis and slot bookkeeping below are
the compiler's job. A future codegen MUST uphold all of this:

1. Capture analysis (who gets boxed).
   A variable that is captured by any nested function AND is ever reassigned
   (by its owner or any closure) must be `Boxed` in its OWNING frame's slot
   kinds. Everything else stays `Plain`. A captured-but-never-reassigned
   variable may stay `Plain` and be captured by value — see point 4.

2. Boxing is per-binding and eager.
   `EnterFrame`'s `local_kinds` declares each declared slot's storage class
   (and a captured *parameter* is boxed in place by a prologue `FreshCell`). A
   `Boxed` slot is backed by a fresh `cells` entry from birth; `Local`/`SetLocal`
   transparently route through it. There is no "open upvalue" / close step —
   the cell already has identity and outlives the frame, so a returned closure
   keeps working after its defining frame is gone. (Cost: one indirection per
   access and a permanent cell. Acceptable under this VM's no-GC, short-program
   design.)

3. Frame slot layout (the ABI).
   Arguments arrive in place as the leading locals, so the layout is:
       slot 0 .. nparams-1            = params (= the call's arguments)
       slot nparams .. nparams+K-1    = captured upvals (MakeClosure order)
       slot nparams+K ..              = the body's own declared locals
   The prologue `EnterFrame(nparams, build_args, local_kinds)` establishes all
   of this: it normalizes the incoming args to `nparams`, installs the closure's
   captured environment (which `CallDyn` stashed in the frame) as the upval
   locals, and allocates the declared locals from `local_kinds`. Emit
   `MakeClosure(addr, captures)` at the definition site with `captures` ordered
   to match exactly the upval slot order the body expects.

4. `MakeClosure(addr, captures)` capture kinds, by value vs by reference.
   Each entry of `captures` is a slot index in the ENCLOSING frame; the slot is
   copied verbatim into the new closure. A `Boxed` slot copies its `Upval`
   handle → shared, by-reference (mutations are mutually visible). A `Plain`
   slot copies its value → an immutable by-value snapshot. Only capture a
   `Plain` slot when the analysis in point 1 proved the binding is effectively
   `const` (assigned once, before every capturing `MakeClosure`, and never
   after). A `Plain` capture of a `Ptr` still shares the heap object — it
   freezes the binding, not the object, which is correct JS semantics.

5. Transitive / nested capture is free.
   A closure capturing a variable owned several scopes up just lists its own
   (installed-upval) slot; copying that slot forwards the SAME cell handle. The
   flat `cells` index threads through every intermediate closure unchanged.

6. Captured parameters.
   Arguments arrive in place as the leading locals (slots `0..nparams`, set up
   by `EnterFrame`), holding plain values. To capture (or reassign) a parameter,
   the prologue boxes its slot in place with `FreshCell` (plain value → fresh
   cell); closures then capture that boxed local.

7. Non-capturing functions stay cheap.
   A lambda/function with no captures should remain a bare `Value::Fn`
   (zero heap allocation). Only emit `MakeClosure` when there is something to
   capture.

Closure values are first-class: callable via `CallDyn`, compared by reference
identity, and (like `Fn`) have no JSON representation.

*/

use indexmap::IndexMap;
use thin_vec::ThinVec;

use crate::builtin::Builtin;
use crate::compiler::Program;

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

pub use crate::thin_string::ThinString;
pub type FieldName = ThinString;
pub type CodeAddr = u32;
pub type HeapAddr = u32;
pub type StackAddr = u32;
pub type ArgIndex = u32;
pub type LocalIndex = u32;
pub type ArgCount = u32;
/// Index into the VM's `cells` side table (the store of captured bindings).
pub type CellIndex = u32;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum StackValue {
    Null,
    /// JS `undefined`: the value of an absent thing, as distinct from `null`
    /// (a present, intentionally-empty value). Produced internally — never by
    /// JSON, which only yields `Null` — by a missing object property, an
    /// out-of-bounds array index, a read of an unset variable, and an
    /// uninitialized local. `null`/`undefined` thus mirror JS's data-vs-absence
    /// split. Falsy, has no JSON form of its own (see `stack_value_to_json`),
    /// and `=== undefined` only (strict): `undefined !== null`.
    Undefined,
    Bool(bool),
    Number(f64),
    /// A non-negative integer (0 ..= u64::MAX) and a negative integer
    /// (i64::MIN ..= -1). Together they mirror serde_json's internal number
    /// representation (`PosInt(u64) | NegInt(i64) | Float(f64)`) exactly, which
    /// is what we round-trip through — so JSON integers map losslessly in both
    /// directions, and every integer has a single *canonical* form (its sign
    /// chooses the variant; there is no overlapping range).
    ///
    /// These are a transport/identity type, not an arithmetic peer: any
    /// arithmetic op promotes them to `Number(f64)`, so the combinatorics of
    /// mixed int/float math never arise. They are only ever *produced* by
    /// literals, `StrToInt`, JSON parsing, and tool results — never by
    /// computation.
    PosInt(u64),
    NegInt(i64),
    /// A first-class function value: just a code address, with no captured
    /// environment. Covers non-capturing lambdas and named functions passed as
    /// values (dispatch tables, `map`/`filter` callbacks, etc.). Capturing
    /// lambdas instead become a `HeapValue::Closure` (a code address plus a
    /// captured environment), built by `MakeClosure` and likewise called
    /// through `CallDyn`.
    Fn(CodeAddr),
    Ptr(HeapAddr),
    /// Internal indirection for a captured *by-reference* binding: indexes the
    /// VM's `cells` side table, which has identity and outlives stack frames.
    /// Only ever stored in a frame's local (or captured-arg) slots;
    /// `Local`/`SetLocal` dereference it transparently, so the marker never
    /// surfaces in expression temporaries, heap collections, or variables.
    Upval(CellIndex),
    /// A builtin stdlib function as a first-class value (`Math.max`, `arr.push`
    /// passed as a callback). Like `Fn`, it is callable (via `CallDyn`), is a
    /// "function" under `typeof`, compares by identity, and has no JSON form.
    /// The compiler's common path uses the static `Instr::CallBuiltin` instead;
    /// this variant exists for the rarer higher-order/callback use.
    Builtin(Builtin),
}

#[derive(Clone, Debug, PartialEq)]
pub enum HeapValue {
    String(ThinString),
    Array(ThinVec<StackValue>),
    Object(Box<IndexMap<FieldName, StackValue>>),
    /// A closure: a code address plus its captured environment. Each upval is
    /// either a plain value (an immutable / by-value capture) or an `Upval`
    /// handle (a shared, mutable by-reference capture). Built by `MakeClosure`,
    /// called via `CallDyn`, which installs `upvals` as the callee's leading
    /// locals. Like `Fn`, it has no JSON form and compares by identity.
    Closure {
        addr: CodeAddr,
        upvals: ThinVec<StackValue>,
    },
}

/// Storage class for a local slot declared by `EnterFrame`. A `Plain` slot is an
/// ordinary stack local; a `Boxed` slot is captured by reference, so it is
/// backed by a `cells` entry and addressed through an `Upval` marker.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SlotKind {
    Plain,
    Boxed,
}

pub struct CallFrame {
    arg_count: u32,
    local_count: u32,
    return_addr: CodeAddr,
    prev_fp: StackAddr,
    /// A closure's captured environment, stashed by `CallDyn` and installed as
    /// the callee's upval locals by the prologue `EnterFrame` (after the arg
    /// region is normalized to `nparams`, so the upvals land at the right slots).
    /// Empty for static `Call` and bare-`Fn` calls (no captures).
    pending_upvals: ThinVec<StackValue>,
    /// Lazily-built, per-frame cache for the `arguments` array (its heap
    /// address). Built on the first `Instr::Arguments` in this frame and reused
    /// by later references, so repeated `arguments` uses don't re-materialize
    /// the array. `None` until first use (and for frames that never use it).
    arguments_cache: Option<HeapAddr>,
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
   A lambda/function with no captures should remain a bare `StackValue::Fn`
   (zero heap allocation). Only emit `MakeClosure` when there is something to
   capture.

Closure values are first-class: callable via `CallDyn`, compared by reference
identity, and (like `Fn`) have no JSON representation.

*/
pub struct VM {
    pub code: Vec<Instr>,
    pub heap: Vec<HeapValue>,
    /// Side table of captured bindings (cells). A `Boxed` local lives here so
    /// it has identity and outlives its frame; `StackValue::Upval` indexes it.
    /// Grows monotonically (no reclamation), like `heap`.
    pub cells: Vec<StackValue>,
    pub stack: Vec<StackValue>, // sp == stack.len()
    pub callstack: Vec<CallFrame>,
    pub ip: CodeAddr,
    pub fp: StackAddr,
    /// Remaining instruction budget. Decremented once per executed
    /// instruction across all `step()` calls; reaching zero yields
    /// `VMError::OutOfFuel`. The heap grows monotonically (no reclamation,
    /// by design — programs are expected to be short-lived), so this is the
    /// primary backstop against runaway execution.
    pub fuel: u64,
}

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
    PushNull,
    PushUndefined,
    PushBool(bool),
    PushFloat(f64), // () -> any
    PushPosInt(u64),
    PushNegInt(i64),
    PushFn(CodeAddr),
    PushPtr(HeapAddr),
    PushBuiltin(Builtin),
    PushStr(ThinString), // () -> str

    Pop(usize),
    Dup,
    Swap, // any, any -> any, any
    Rot,  // any, any, any -> any, any, any

    // Generalized stack reach (Forth-like), counting from the top (0 = top).
    // Both reject reaching below the current frame's temporaries (frame_floor),
    // like Dup/Swap/Rot. Pick is the read-modify-write workhorse (duplicate an
    // lvalue's object/key for a load-then-store); Dig reorders without copying.
    //
    // Pick(n): copy the n-th-from-top value to the top. Pick(0) == Dup.
    Pick(usize), // any^(n+1) -> any^(n+1), any
    // Dig(n): move the n-th-from-top value to the top, removing it from its
    // old position. Dig(0) is a no-op, Dig(1) == Swap, Dig(2) == Rot.
    Dig(usize), // any^(n+1) -> any^(n+1)

    // Drop `n` values directly below the top, leaving the top in place.
    // Nip(1) ≡ Swap; Pop, Nip(n) is the symmetric inverse of Dig(n).
    Nip(usize), // any^(n+1) -> any

    // calls function starting at address. the N arguments are passed on the
    // stack in left-to-right order (arg 0 pushed first / deepest), and become
    // the new frame's args. depends on function whether or not a result is left
    // on stack after it returns.
    Call(CodeAddr, u32), // any, ... -> [any]

    // indirect call: the callable sits on top, above its N args (left-to-right,
    // so arg 0 is deepest). The callable is either a bare `Fn` value or a `Ptr`
    // to a `HeapValue::Closure`; pops it and calls with the same convention as
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
    // a Ptr to the resulting HeapValue::Closure. Each captured slot is copied
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
    // slot. Replaces the common `Dup; SetLocal` pair.
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
    // instruction, instead of a Dup + Push(Null) + LooseEq + branch per check.
    JNotNullish(CodeAddr), // any -> any (peek)

    // EFFECT: invokes the named tool or function.
    // pops N arguments off the stack; args are taken in push order, so with
    // left-to-right codegen arg 0 is the deepest of the group (the first one
    // pushed). step() batches a run of consecutive Invoke instructions into one
    // StepResult::Invoke (fan-out); the host runs them concurrently and pushes
    // one result per call, in call order.
    Invoke(ThinString, u32), // any, ... -> any

    // EFFECT: raise condition (like Lisp condition system). used to ask LLM in calling frame
    // to decide how to proceed, using restarts like returning a value, aborting,
    // and even rewriting the program preserving already written variables with execution starting at arbitrary point.
    Raise(ThinString), // () -> any

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
    // then operates on them pushing result to stack
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

#[derive(Debug)]
pub enum VMError {
    StackUnderflow,
    BadReturn,
    BadCall,
    BadAlloc,
    BadArg,
    BadLocal,
    TypeError,
    ValueError,
    /// Instruction budget exhausted (guards against infinite loops in
    /// LLM-generated programs).
    OutOfFuel,
}

/// Default instruction budget for a freshly constructed VM. The host can
/// override `VM::fuel` before/after stepping. Chosen high enough that any
/// realistic orchestration program completes, low enough that a runaway
/// loop is caught in well under a second.
pub const DEFAULT_FUEL: u64 = 10_000_000;

/// Maximum nesting depth for JSON <-> value conversion. Bounds native
/// recursion so adversarial tool output cannot overflow the Rust stack.
const MAX_JSON_DEPTH: usize = 128;

/// A single tool/function call requested by the program.
#[derive(Debug)]
pub struct InvokeCall {
    pub name: String,
    /// Arguments in call order (`args[0]` is the first argument).
    pub args: Vec<StackValue>,
}

#[derive(Debug)]
pub enum StepResult {
    /// Program completed (root frame returned).
    Done,
    /// One or more tool/function calls to perform. `step()` batches a run of
    /// consecutive `Invoke` instructions into a single fan-out request so the
    /// host can run them concurrently. The host must push exactly one result
    /// per call back onto `vm.stack`, in the SAME order as `calls` (calls[0]'s
    /// result first/deepest, calls.last()'s result on top), then call step()
    /// again. A lone `Invoke` is just the one-element case.
    Invoke { calls: Vec<InvokeCall> },
    /// A condition was raised; host (LLM) decides how to proceed.
    /// Host may inspect/modify vm state (including ip, code, stack) before
    /// calling step() again.
    Raise { condition: String },
}

// ── free helper functions ─────────────────────────────────────────────

/// JS `Number.prototype.toString` for a finite-or-not f64. Integers print
/// without a decimal point; NaN/±Infinity get their JS spellings (Rust's
/// `Display` would otherwise emit "NaN"/"inf"). Diverges from JS only for the
/// very large/small magnitudes JS renders in exponential form (e.g. `1e+21`),
/// which don't arise from tool/JSON data here.
fn js_number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if float_is_int(n) && n >= (i64::MIN as f64) && n <= (i64::MAX as f64) {
        (n as i64).to_string()
    } else {
        format!("{n}")
    }
}

pub(crate) fn float_is_int(n: f64) -> bool {
    n.fract() == 0.0
}

/// Coerce a numeric value (`Number` or `Int`) to f64 for arithmetic. Returns
/// None for non-numeric values. This is the single coercion point that keeps
/// `Int` from multiplying the arithmetic match arms: ops just `as_f64` their
/// operands and always produce `Number`.
fn as_f64(val: &StackValue) -> Option<f64> {
    match val {
        StackValue::Number(n) => Some(*n),
        StackValue::PosInt(u) => Some(*u as f64),
        StackValue::NegInt(i) => Some(*i as f64),
        _ => None,
    }
}

/// Coerce a numeric value to i64 for integer-only ops (mod, bitwise, shifts,
/// indices). `NegInt` is taken directly; a `PosInt` must fit in i64; a
/// `Number` must be integer-valued. Returns None otherwise.
pub(crate) fn as_i64(val: &StackValue) -> Option<i64> {
    match val {
        StackValue::NegInt(i) => Some(*i),
        StackValue::PosInt(u) => i64::try_from(*u).ok(),
        StackValue::Number(n) if float_is_int(*n) => Some(*n as i64),
        _ => None,
    }
}

fn is_number(val: &StackValue) -> bool {
    matches!(
        val,
        StackValue::Number(_) | StackValue::PosInt(_) | StackValue::NegInt(_)
    )
}

/// JS `ToNumber` applied to a string, as used when a loose `==` compares a
/// number to a string. Trims whitespace, treats the empty string as 0, and
/// otherwise parses as f64 — yielding NaN (which is never equal to anything)
/// when unparseable. Diverges from spec ToNumber on a few literal forms it
/// would accept (hex `0x…`, etc.), which don't arise from tool/JSON data here.
fn js_str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        0.0
    } else {
        t.parse::<f64>().unwrap_or(f64::NAN)
    }
}

// ── VM impl ───────────────────────────────────────────────────────────

impl VM {
    pub fn new(code: Vec<Instr>) -> Self {
        VM {
            code,
            heap: Vec::new(),
            cells: Vec::new(),
            stack: Vec::new(),
            // Root frame so that Local is valid from the start.
            callstack: vec![CallFrame {
                arg_count: 0,
                local_count: 0,
                return_addr: 0,
                prev_fp: 0,
                arguments_cache: None,
                pending_upvals: ThinVec::new(),
            }],
            ip: 0,
            fp: 0,
            fuel: DEFAULT_FUEL,
        }
    }

    /// Construct a VM to run a compiled `Program`, with the blessed `state`
    /// object installed at `heap[0]`. `state` is seeded from the prior run's
    /// durable JSON (an object); `Null`/non-object seeds yield an empty `state`.
    /// All durable/host-context access lowers to ordinary object ops on
    /// `Ptr(0)`, so the host persists by extracting `heap[0]` after the run and
    /// re-seeding it here next time. `PushStr`/literals alloc at `heap[1+]`, so
    /// `Ptr(0)` stays stable for the whole program.
    /// Construct a VM to run a compiled `Program`, pre-allocating its string
    /// constants at `heap[1..=N]` and installing the blessed `state` object at
    /// `heap[0]`. `state` is seeded from the prior run's durable JSON (an
    /// object); `Null`/non-object seeds yield an empty `state`. All
    /// durable/host-context access lowers to ordinary object ops on `Ptr(0)`,
    /// so the host persists by extracting `heap[0]` after the run and
    /// re-seeding it here next time. Constants are immutable and freely shared
    /// across many `PushPtr` references.
    pub fn for_program(program: Program, state: serde_json::Value) -> Result<Self, VMError> {
        let mut vm = VM::new(program.code);
        // Reserve heap[0] for `state` (empty placeholder — filled after
        // constants).
        vm.heap.push(HeapValue::Object(Box::new(IndexMap::new())));
        // Pre-allocate constants at heap[1..=N]. Each constant's address is
        // 1 + index, as emitted by the compiler.
        for s in program.constants {
            vm.heap.push(HeapValue::String(s));
        }
        // Seed state's nested values (they land at heap[N+1..], but addresses
        // are computed at runtime and stored in the state map — the shift is
        // transparent).
        if let serde_json::Value::Object(map) = state {
            let mut entries = IndexMap::with_capacity(map.len());
            for (k, v) in &map {
                let sv = vm.json_to_stack_value(v, 0)?;
                entries.insert(ThinString::from(k.as_str()), sv);
            }
            if let Some(HeapValue::Object(o)) = vm.heap.get_mut(0) {
                *o = Box::new(entries);
            }
        }
        Ok(vm)
    }

    /// Extract the blessed `state` object (heap[0]) as a JSON value. This is the
    /// persistence boundary the host uses to save/restore durable state between
    /// runs.
    pub fn state_to_json(&self) -> Result<serde_json::Value, VMError> {
        self.stack_value_to_json(&StackValue::Ptr(0), 0)
    }

    // ── heap access helpers ──────────────────────────────────────────

    /// Lowest stack index the current frame's expression temporaries may
    /// occupy. Args live below `fp`, locals in `[fp, fp + local_count)`, and
    /// temporaries above that. Stack-manipulation ops (Dup/Swap/Rot/Pop) must
    /// not reach below this floor into locals, args, or the caller's stack.
    fn frame_floor(&self) -> usize {
        let local_count = self.callstack.last().map_or(0, |f| f.local_count) as usize;
        self.fp as usize + local_count
    }

    /// If the instruction at `ip` is an `Invoke`, return its name and arg
    /// count as owned values (releasing the borrow on `self.code` so the
    /// caller can mutate the stack while batching consecutive invokes).
    fn invoke_at(&self, ip: CodeAddr) -> Option<(String, u32)> {
        match self.code.get(ip as usize) {
            Some(Instr::Invoke(name, nargs)) => Some((name.as_str().to_owned(), *nargs)),
            _ => None,
        }
    }

    /// Bounds-checked heap read. A correct program never produces a
    /// dangling pointer (the heap only ever grows), but a value pushed as a
    /// literal `Ptr` could be out of range — surface that as an error rather
    /// than panicking and taking down the host.
    fn heap_get(&self, ptr: HeapAddr) -> Result<&HeapValue, VMError> {
        self.heap.get(ptr as usize).ok_or(VMError::ValueError)
    }

    fn heap_str(&self, ptr: HeapAddr) -> Option<&str> {
        match self.heap.get(ptr as usize) {
            Some(HeapValue::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    pub(crate) fn heap_arr(&self, ptr: HeapAddr) -> Option<&ThinVec<StackValue>> {
        match self.heap.get(ptr as usize) {
            Some(HeapValue::Array(a)) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn heap_arr_mut(&mut self, ptr: HeapAddr) -> Option<&mut ThinVec<StackValue>> {
        match self.heap.get_mut(ptr as usize) {
            Some(HeapValue::Array(a)) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn heap_obj(&self, ptr: HeapAddr) -> Option<&IndexMap<FieldName, StackValue>> {
        match self.heap.get(ptr as usize) {
            Some(HeapValue::Object(o)) => Some(&**o),
            _ => None,
        }
    }

    fn heap_obj_mut(&mut self, ptr: HeapAddr) -> Option<&mut IndexMap<FieldName, StackValue>> {
        match self.heap.get_mut(ptr as usize) {
            Some(HeapValue::Object(o)) => Some(&mut **o),
            _ => None,
        }
    }

    pub(crate) fn alloc_string(&mut self, s: ThinString) -> StackValue {
        let addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::String(s));
        StackValue::Ptr(addr)
    }

    pub(crate) fn alloc_array(&mut self, arr: ThinVec<StackValue>) -> StackValue {
        let addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::Array(arr));
        StackValue::Ptr(addr)
    }

    fn alloc_object(&mut self, obj: IndexMap<FieldName, StackValue>) -> StackValue {
        let addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::Object(Box::new(obj)));
        StackValue::Ptr(addr)
    }

    fn alloc_closure(&mut self, addr: CodeAddr, upvals: ThinVec<StackValue>) -> StackValue {
        let heap_addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::Closure { addr, upvals });
        StackValue::Ptr(heap_addr)
    }

    /// JS truthiness. The falsy set is exactly `false`, `0`/`-0`, `NaN`, `""`,
    /// `null`, and `undefined`; everything else (incl. empty arrays/objects and
    /// the string "0") is truthy. Needs heap access to detect the empty string,
    /// hence a method.
    fn is_truthy(&self, val: &StackValue) -> bool {
        match val {
            StackValue::Bool(b) => *b,
            StackValue::Null | StackValue::Undefined => false,
            StackValue::Number(n) => *n != 0.0 && !n.is_nan(),
            StackValue::PosInt(u) => *u != 0,
            // NegInt is always negative (i64::MIN..=-1), hence never zero.
            StackValue::NegInt(_) => true,
            // Empty string is falsy; any other string and all arrays/objects/
            // closures/functions are truthy.
            StackValue::Ptr(p) => !matches!(self.heap_str(*p), Some("")),
            StackValue::Fn(_) | StackValue::Builtin(_) => true,
            // Internal indirection; never a legitimate operand.
            StackValue::Upval(_) => false,
        }
    }

    /// JS `ToNumber` for the arithmetic operators. `null`→0, `undefined`→NaN,
    /// booleans→0/1, numbers pass through, strings parse (`ToNumber`, NaN when
    /// unparseable). Returns None for values JS would route through `ToPrimitive`
    /// first — arrays, objects, closures, functions — which this VM deliberately
    /// does not coerce (see the divergence note on `loose_equal`); arithmetic on
    /// those is a TypeError.
    pub(crate) fn to_number(&self, val: &StackValue) -> Option<f64> {
        match val {
            StackValue::Number(n) => Some(*n),
            StackValue::PosInt(u) => Some(*u as f64),
            StackValue::NegInt(i) => Some(*i as f64),
            StackValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            StackValue::Null => Some(0.0),
            StackValue::Undefined => Some(f64::NAN),
            StackValue::Ptr(p) => self.heap_str(*p).map(js_str_to_number),
            StackValue::Fn(_) | StackValue::Builtin(_) | StackValue::Upval(_) => None,
        }
    }

    /// Whether a value is a heap string (used to pick `+`'s concat vs add path).
    fn is_string(&self, val: &StackValue) -> bool {
        matches!(val, StackValue::Ptr(p) if matches!(self.heap.get(*p as usize), Some(HeapValue::String(_))))
    }

    /// JS `String(x)` / `ToString`. Arrays stringify like `Array.prototype.join(",")`
    /// (null/undefined elements → ""), plain objects → "[object Object]", and
    /// functions/closures → a generic function tag. `depth` bounds recursion
    /// through nested arrays so adversarial nesting can't overflow the stack.
    pub(crate) fn to_js_string(&self, val: &StackValue, depth: usize) -> ThinString {
        if depth > MAX_JSON_DEPTH {
            return ThinString::new();
        }
        match val {
            StackValue::Undefined => ThinString::from("undefined"),
            StackValue::Null => ThinString::from("null"),
            StackValue::Bool(b) => ThinString::from(b.to_string().as_str()),
            StackValue::PosInt(u) => ThinString::from(u.to_string().as_str()),
            StackValue::NegInt(i) => ThinString::from(i.to_string().as_str()),
            StackValue::Number(n) => ThinString::from(js_number_to_string(*n).as_str()),
            StackValue::Fn(_) | StackValue::Builtin(_) => {
                ThinString::from("function () { [native code] }")
            }
            StackValue::Upval(_) => ThinString::new(),
            StackValue::Ptr(p) => match self.heap.get(*p as usize) {
                Some(HeapValue::String(s)) => s.clone(),
                Some(HeapValue::Array(arr)) => {
                    let mut out = ThinString::new();
                    for (i, v) in arr.iter().enumerate() {
                        if i > 0 {
                            out.push_str(",");
                        }
                        match v {
                            StackValue::Null | StackValue::Undefined => {}
                            _ => {
                                let s = self.to_js_string(v, depth + 1);
                                out.push_str(s.as_str());
                            }
                        }
                    }
                    out
                }
                Some(HeapValue::Object(_)) => ThinString::from("[object Object]"),
                Some(HeapValue::Closure { .. }) => ThinString::from("function () { [native code] }"),
                None => ThinString::from("null"), // dangling pointer
            },
        }
    }

    /// Reference/value equality matching JS `===`. Primitives compare by value;
    /// strings, though heap-allocated here, are primitives and so compare by
    /// *content*. Arrays, objects, and closures compare by *reference identity*
    /// (same heap address) — `{a:1} === {a:1}` is false, as in JS.
    fn values_equal(&self, lhs: &StackValue, rhs: &StackValue) -> bool {
        match (lhs, rhs) {
            (StackValue::Null, StackValue::Null) => true,
            // Strict (===): undefined equals only itself; undefined !== null.
            // (Loose `null == undefined` would need a separate op; Eq is ===.)
            (StackValue::Undefined, StackValue::Undefined) => true,
            (StackValue::Bool(a), StackValue::Bool(b)) => a == b,
            (StackValue::Number(a), StackValue::Number(b)) => {
                if a.is_nan() && b.is_nan() {
                    false // NaN != NaN per IEEE 754
                } else {
                    a == b
                }
            }
            // Integers compare exactly within the same variant; PosInt and
            // NegInt never overlap (different sign) so they're never equal.
            // Comparison to Number is by f64 value (so 1 == 1.0); huge ints
            // beyond f64's mantissa are an accepted edge case.
            (StackValue::PosInt(a), StackValue::PosInt(b)) => a == b,
            (StackValue::NegInt(a), StackValue::NegInt(b)) => a == b,
            (StackValue::PosInt(_), StackValue::NegInt(_))
            | (StackValue::NegInt(_), StackValue::PosInt(_)) => false,
            (StackValue::PosInt(a), StackValue::Number(b)) => !b.is_nan() && (*a as f64) == *b,
            (StackValue::Number(a), StackValue::PosInt(b)) => !a.is_nan() && *a == (*b as f64),
            (StackValue::NegInt(a), StackValue::Number(b)) => !b.is_nan() && (*a as f64) == *b,
            (StackValue::Number(a), StackValue::NegInt(b)) => !a.is_nan() && *a == (*b as f64),
            // Function values are equal iff they point at the same code address.
            (StackValue::Fn(a), StackValue::Fn(b)) => a == b,
            // Builtins compare by identity, like Fn.
            (StackValue::Builtin(a), StackValue::Builtin(b)) => a == b,
            (StackValue::Ptr(p), StackValue::Ptr(q)) => {
                match (self.heap.get(*p as usize), self.heap.get(*q as usize)) {
                    // Same heap address is the same object (JS reference identity
                    // — the only equality arrays/objects/closures get). Distinct
                    // pointers are equal only when both are strings with equal
                    // content, since strings are primitives despite being heap-
                    // allocated. A dangling pointer matches nothing (no panic).
                    (Some(a), Some(b)) => {
                        p == q
                            || matches!(
                                (a, b),
                                (HeapValue::String(x), HeapValue::String(y)) if x == y
                            )
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// JS Abstract Equality Comparison (`==`). Differs from `values_equal`
    /// (`===`) only by coercion, applied in spec order:
    ///   • `null` and `undefined` are loosely equal to each other and to
    ///     nothing else;
    ///   • a boolean coerces to a number (false→0, true→1) and the comparison
    ///     re-runs;
    ///   • a number vs a string coerces the string with `ToNumber`;
    ///   • any other pairing falls through to the strict structural compare
    ///     (so two numbers, two strings, or two heap collections behave exactly
    ///     as `===` does here).
    ///
    /// One deliberate divergence: an object/array vs a primitive is NOT coerced
    /// via `ToPrimitive` (so `[5] == 5` is false here, true in JS). Loose
    /// object↔primitive equality is never an intentional pattern in this DSL,
    /// where heap values are data containers; skipping it avoids the
    /// `toString`/`valueOf` machinery and the footguns it brings.
    fn loose_equal(&self, lhs: &StackValue, rhs: &StackValue) -> bool {
        use StackValue::*;
        // null / undefined: loosely equal to each other, to nothing else.
        let l_nullish = matches!(lhs, Null | Undefined);
        let r_nullish = matches!(rhs, Null | Undefined);
        if l_nullish || r_nullish {
            return l_nullish && r_nullish;
        }
        match (lhs, rhs) {
            // Boolean → number, then re-run the comparison.
            (Bool(b), _) => self.loose_equal(&Number(if *b { 1.0 } else { 0.0 }), rhs),
            (_, Bool(b)) => self.loose_equal(lhs, &Number(if *b { 1.0 } else { 0.0 })),
            // Number vs string (either order): coerce the string with ToNumber.
            (l, Ptr(p)) if is_number(l) => self.num_loose_eq_str(l, *p),
            (Ptr(p), r) if is_number(r) => self.num_loose_eq_str(r, *p),
            // No further coercion: same-type primitives and heap-vs-heap defer
            // to the strict structural comparison.
            _ => self.values_equal(lhs, rhs),
        }
    }

    /// Helper for `loose_equal`: a numeric value vs a heap pointer. Coerces the
    /// pointee only if it is a string (`ToNumber`); arrays/objects/closures are
    /// not coerced (see the divergence note on `loose_equal`).
    fn num_loose_eq_str(&self, num: &StackValue, ptr: HeapAddr) -> bool {
        match (as_f64(num), self.heap_str(ptr)) {
            (Some(a), Some(s)) => a == js_str_to_number(s),
            _ => false,
        }
    }

    /// Total ordering for comparable types. Returns None for incomparable types.
    fn compare(&self, lhs: &StackValue, rhs: &StackValue) -> Option<std::cmp::Ordering> {
        match (lhs, rhs) {
            (StackValue::Null, StackValue::Null) => Some(std::cmp::Ordering::Equal),
            (StackValue::Bool(a), StackValue::Bool(b)) => Some(a.cmp(b)),
            (StackValue::Number(a), StackValue::Number(b)) => a.partial_cmp(b),
            (StackValue::PosInt(a), StackValue::PosInt(b)) => Some(a.cmp(b)),
            (StackValue::NegInt(a), StackValue::NegInt(b)) => Some(a.cmp(b)),
            // Sign decides cross-variant ordering with no value juggling.
            (StackValue::PosInt(_), StackValue::NegInt(_)) => Some(std::cmp::Ordering::Greater),
            (StackValue::NegInt(_), StackValue::PosInt(_)) => Some(std::cmp::Ordering::Less),
            (StackValue::PosInt(a), StackValue::Number(b)) => (*a as f64).partial_cmp(b),
            (StackValue::Number(a), StackValue::PosInt(b)) => a.partial_cmp(&(*b as f64)),
            (StackValue::NegInt(a), StackValue::Number(b)) => (*a as f64).partial_cmp(b),
            (StackValue::Number(a), StackValue::NegInt(b)) => a.partial_cmp(&(*b as f64)),
            (StackValue::Ptr(p), StackValue::Ptr(q)) => {
                match (self.heap.get(*p as usize), self.heap.get(*q as usize)) {
                    (Some(HeapValue::String(a)), Some(HeapValue::String(b))) => Some(a.cmp(b)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Pop a value and require it to be a heap pointer.
    pub(crate) fn pop_ptr(&mut self) -> Result<HeapAddr, VMError> {
        match self.stack.pop().ok_or(VMError::StackUnderflow)? {
            StackValue::Ptr(p) => Ok(p),
            _ => Err(VMError::TypeError),
        }
    }

    fn pop_number(&mut self) -> Result<f64, VMError> {
        as_f64(&self.stack.pop().ok_or(VMError::StackUnderflow)?).ok_or(VMError::TypeError)
    }

    fn pop_int(&mut self) -> Result<i64, VMError> {
        as_i64(&self.stack.pop().ok_or(VMError::StackUnderflow)?).ok_or(VMError::TypeError)
    }

    /// Pop a pointer and require it to point to a String; return the string.
    fn pop_string(&mut self) -> Result<ThinString, VMError> {
        let ptr = self.pop_ptr()?;
        match self.heap.get(ptr as usize) {
            Some(HeapValue::String(s)) => Ok(s.clone()),
            _ => Err(VMError::TypeError),
        }
    }

    /// Extract a string from a StackValue that has already been popped.
    pub(crate) fn pop_string_from(&self, val: &StackValue) -> Result<ThinString, VMError> {
        match val {
            StackValue::Ptr(p) => match self.heap.get(*p as usize) {
                Some(HeapValue::String(s)) => Ok(s.clone()),
                _ => Err(VMError::TypeError),
            },
            _ => Err(VMError::TypeError),
        }
    }

    // ── JSON conversion helpers ──────────────────────────────────────

    pub(crate) fn stack_value_to_json(
        &self,
        val: &StackValue,
        depth: usize,
    ) -> Result<serde_json::Value, VMError> {
        if depth > MAX_JSON_DEPTH {
            return Err(VMError::ValueError);
        }
        Ok(match val {
            StackValue::Null => serde_json::Value::Null,
            StackValue::Bool(b) => serde_json::Value::Bool(*b),
            // Integers carry through losslessly — both map onto a native
            // serde_json::Number (this is the whole point of mirroring it).
            StackValue::PosInt(u) => serde_json::Value::Number(serde_json::Number::from(*u)),
            StackValue::NegInt(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
            // A function/closure has no JSON representation, and an Upval marker
            // is an internal indirection that should never reach here: fail
            // loudly rather than silently dropping it.
            StackValue::Fn(_) | StackValue::Builtin(_) | StackValue::Upval(_) => {
                return Err(VMError::ValueError);
            }
            // `undefined` has no JSON form. Like JS `JSON.stringify`, it is
            // *dropped* in an object and coerced to *null* in an array (handled
            // at those parent sites below); reaching here means it is the root
            // value, where JS.stringify returns the JS value `undefined` — no
            // JSON — so we surface an error rather than inventing one.
            StackValue::Undefined => return Err(VMError::ValueError),
            StackValue::Number(n) => {
                // Preserve integer formatting when possible (f64-only VM
                // internals, but JSON consumers care about int vs float).
                if float_is_int(*n) && *n >= (i64::MIN as f64) && *n <= (i64::MAX as f64) {
                    serde_json::Value::Number(serde_json::Number::from(*n as i64))
                } else {
                    // NaN/Infinity have no JSON representation -> null, rather
                    // than silently coercing to 0.
                    match serde_json::Number::from_f64(*n) {
                        Some(num) => serde_json::Value::Number(num),
                        None => serde_json::Value::Null,
                    }
                }
            }
            StackValue::Ptr(p) => match self.heap_get(*p)? {
                HeapValue::String(s) => serde_json::Value::String(s.as_str().to_owned()),
                HeapValue::Array(arr) => serde_json::Value::Array(
                    arr.iter()
                        .map(|v| match v {
                            // JS: `undefined` array slots stringify to `null`.
                            StackValue::Undefined => Ok(serde_json::Value::Null),
                            _ => self.stack_value_to_json(v, depth + 1),
                        })
                        .collect::<Result<_, _>>()?,
                ),
                HeapValue::Object(obj) => {
                    let mut map = serde_json::Map::new();
                    for (k, v) in obj.iter() {
                        // JS: properties whose value is `undefined` are omitted.
                        if matches!(v, StackValue::Undefined) {
                            continue;
                        }
                        map.insert(k.as_str().to_owned(), self.stack_value_to_json(v, depth + 1)?);
                    }
                    serde_json::Value::Object(map)
                }
                // A closure has no JSON representation (see Fn above).
                HeapValue::Closure { .. } => return Err(VMError::ValueError),
            },
        })
    }

    pub(crate) fn json_to_stack_value(
        &mut self,
        json: &serde_json::Value,
        depth: usize,
    ) -> Result<StackValue, VMError> {
        if depth > MAX_JSON_DEPTH {
            return Err(VMError::ValueError);
        }
        Ok(match json {
            serde_json::Value::Null => StackValue::Null,
            serde_json::Value::Bool(b) => StackValue::Bool(*b),
            serde_json::Value::Number(n) => {
                // Mirror serde's own split: non-negative -> PosInt (full u64),
                // negative -> NegInt, fractions -> Number. Check as_u64 first so
                // non-negatives become canonical PosInt.
                if let Some(u) = n.as_u64() {
                    StackValue::PosInt(u)
                } else if let Some(i) = n.as_i64() {
                    StackValue::NegInt(i)
                } else {
                    StackValue::Number(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => self.alloc_string(ThinString::from(s.as_str())),
            serde_json::Value::Array(arr) => {
                let vals: ThinVec<StackValue> = arr
                    .iter()
                    .map(|v| self.json_to_stack_value(v, depth + 1))
                    .collect::<Result<_, _>>()?;
                self.alloc_array(vals)
            }
            serde_json::Value::Object(obj) => {
                let mut map = IndexMap::new();
                for (k, v) in obj {
                    map.insert(ThinString::from(k.as_str()), self.json_to_stack_value(v, depth + 1)?);
                }
                self.alloc_object(map)
            }
        })
    }

    // ── step ─────────────────────────────────────────────────────────

    pub fn step(&mut self) -> Result<StepResult, VMError> {
        // ── macros for repetitive instruction shapes ─────────────────

        /// Pop one operand, coerce ToNumber (JS), apply f64→f64, push Number.
        /// A non-coercible operand (array/object/function) is a TypeError; an
        /// `undefined` or unparseable string coerces to NaN and propagates.
        macro_rules! unary_num {
            ($op:expr) => {{
                let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                match self.to_number(&val) {
                    Some(n) => {
                        self.stack.push(StackValue::Number($op(n)));
                        self.ip += 1;
                    }
                    None => return Err(VMError::TypeError),
                }
            }};
        }

        /// Pop rhs then lhs, coerce both ToNumber (JS), apply f64→f64→f64, push
        /// Number. Strings/booleans/null coerce; arrays/objects/functions are a
        /// TypeError; undefined/unparseable strings become NaN.
        macro_rules! binary_num {
            ($op:expr) => {{
                let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                match (self.to_number(&lhs), self.to_number(&rhs)) {
                    (Some(a), Some(b)) => {
                        self.stack.push(StackValue::Number($op(a, b)));
                        self.ip += 1;
                    }
                    _ => return Err(VMError::TypeError),
                }
            }};
        }

        /// Pop rhs then lhs (integer-valued: Int, or integer-valued Number),
        /// apply i64→i64→i64, push Number.
        macro_rules! binary_int {
            ($op:expr) => {{
                let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                match (as_i64(&lhs), as_i64(&rhs)) {
                    (Some(a), Some(b)) => {
                        self.stack.push(StackValue::Number($op(a, b) as f64));
                        self.ip += 1;
                    }
                    _ => return Err(VMError::TypeError),
                }
            }};
        }

        /// Pop rhs then lhs, compare with self.compare(), push Bool.
        macro_rules! cmp_op {
            ($expected:ident) => {{
                let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                let result = self
                    .compare(&lhs, &rhs)
                    .map(|ord| ord == std::cmp::Ordering::$expected)
                    .unwrap_or(false);
                self.stack.push(StackValue::Bool(result));
                self.ip += 1;
            }};
        }

        // ── main dispatch loop ───────────────────────────────────────

        loop {
            if self.ip as usize >= self.code.len() {
                return Ok(StepResult::Done);
            }
            if self.fuel == 0 {
                return Err(VMError::OutOfFuel);
            }
            self.fuel -= 1;
            match &self.code[self.ip as usize] {
                // ── stack manipulation ───────────────────────────
                Instr::PushNull => {
                    self.stack.push(StackValue::Null);
                    self.ip += 1;
                }
                Instr::PushUndefined => {
                    self.stack.push(StackValue::Undefined);
                    self.ip += 1;
                }
                Instr::PushBool(b) => {
                    self.stack.push(StackValue::Bool(*b));
                    self.ip += 1;
                }
                Instr::PushFloat(f) => {
                    self.stack.push(StackValue::Number(*f));
                    self.ip += 1;
                }
                Instr::PushPosInt(u) => {
                    self.stack.push(StackValue::PosInt(*u));
                    self.ip += 1;
                }
                Instr::PushNegInt(i) => {
                    self.stack.push(StackValue::NegInt(*i));
                    self.ip += 1;
                }
                Instr::PushFn(addr) => {
                    self.stack.push(StackValue::Fn(*addr));
                    self.ip += 1;
                }
                Instr::PushPtr(h) => {
                    self.stack.push(StackValue::Ptr(*h));
                    self.ip += 1;
                }
                Instr::PushBuiltin(b) => {
                    self.stack.push(StackValue::Builtin(*b));
                    self.ip += 1;
                }

                Instr::PushStr(s) => {
                    let s = s.clone(); // release the borrow on self.code
                    let ptr = self.alloc_string(s);
                    self.stack.push(ptr);
                    self.ip += 1;
                    continue;
                }

                Instr::Pop(n) => {
                    // Only expression temporaries may be popped, never locals
                    // or args belonging to the current/caller frame.
                    if self.stack.len() < self.frame_floor() + *n {
                        return Err(VMError::StackUnderflow);
                    }
                    self.stack.truncate(self.stack.len() - n);
                    self.ip += 1;
                }

                Instr::Dup => {
                    if self.stack.len() < self.frame_floor() + 1 {
                        return Err(VMError::StackUnderflow);
                    }
                    let top = *self.stack.last().unwrap();
                    self.stack.push(top);
                    self.ip += 1;
                }

                Instr::Swap => {
                    let len = self.stack.len();
                    if len < self.frame_floor() + 2 {
                        return Err(VMError::StackUnderflow);
                    }
                    self.stack.swap(len - 1, len - 2);
                    self.ip += 1;
                }

                Instr::Rot => {
                    let len = self.stack.len();
                    if len < self.frame_floor() + 3 {
                        return Err(VMError::StackUnderflow);
                    }
                    self.stack.swap(len - 3, len - 2);
                    self.stack.swap(len - 2, len - 1);
                    self.ip += 1;
                }

                Instr::Pick(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    // The picked value sits at len-1-n; it (and everything above)
                    // must be a temporary, not a local/arg.
                    if len < self.frame_floor() + n + 1 {
                        return Err(VMError::StackUnderflow);
                    }
                    let val = self.stack[len - 1 - n];
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::Dig(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    if len < self.frame_floor() + n + 1 {
                        return Err(VMError::StackUnderflow);
                    }
                    // Remove the n-th-from-top value and re-push it on top.
                    let val = self.stack.remove(len - 1 - n);
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::Nip(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    if len < self.frame_floor() + n + 1 {
                        return Err(VMError::StackUnderflow);
                    }
                    // Remove n values directly below the top, leaving the top
                    // in place. Nip(1) ≡ Swap; Pop, Nip(n) is the inverse of
                    // Dig(n): where Dig moves element len-1-n to the top,
                    // Nip drops it.
                    let start = len - 1 - n;
                    self.stack.drain(start..start + n);
                    self.ip += 1;
                }

                // ── control flow ─────────────────────────────────
                Instr::Call(addr, nargs) => {
                    if *addr as usize >= self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    if *nargs as usize > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    // `fp` points at arg 0: the args ARE the callee's leading
                    // locals (slots 0..nargs). The prologue `EnterFrame` then
                    // normalizes them to exactly `nparams`. No copy.
                    self.callstack.push(CallFrame {
                        arg_count: *nargs,
                        local_count: *nargs,
                        return_addr: self.ip + 1,
                        prev_fp: self.fp,
                        arguments_cache: None,
                        pending_upvals: ThinVec::new(),
                    });
                    self.ip = *addr;
                    self.fp = (self.stack.len() as u32) - *nargs;
                }

                Instr::CallDyn(nargs) => {
                    let nargs = *nargs;
                    // The callable is on top, above its args; pop it, then the
                    // args sit exactly where a static Call expects them. The
                    // callable is either a bare Fn, a Builtin, or a Ptr to a
                    // Closure (code + captures).
                    let callable = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match callable {
                        StackValue::Builtin(b) => {
                            // No-frame call: pop args, push result, advance ip.
                            b.call(self, nargs)?;
                            self.ip += 1;
                        }
                        StackValue::Fn(addr) => {
                            if addr as usize >= self.code.len() {
                                return Err(VMError::BadCall);
                            }
                            if nargs as usize > self.stack.len() {
                                return Err(VMError::StackUnderflow);
                            }
                            self.callstack.push(CallFrame {
                                arg_count: nargs,
                                local_count: nargs,
                                return_addr: self.ip + 1,
                                prev_fp: self.fp,
                                arguments_cache: None,
                                pending_upvals: ThinVec::new(),
                            });
                            self.fp = (self.stack.len() as u32) - nargs;
                            self.ip = addr;
                        }
                        StackValue::Ptr(p) => {
                            let (addr, upvals) = match self.heap_get(p)? {
                                HeapValue::Closure { addr, upvals } => (*addr, upvals.clone()),
                                _ => return Err(VMError::TypeError),
                            };
                            if addr as usize >= self.code.len() {
                                return Err(VMError::BadCall);
                            }
                            if nargs as usize > self.stack.len() {
                                return Err(VMError::StackUnderflow);
                            }
                            // Stash the captured environment; `EnterFrame` installs
                            // it as the upval locals after normalizing the args, so
                            // it lands at slots [nparams, nparams + K).
                            self.callstack.push(CallFrame {
                                arg_count: nargs,
                                local_count: nargs,
                                return_addr: self.ip + 1,
                                prev_fp: self.fp,
                                arguments_cache: None,
                                pending_upvals: upvals,
                            });
                            self.fp = (self.stack.len() as u32) - nargs;
                            self.ip = addr;
                        }
                        _ => return Err(VMError::TypeError),
                    }
                }

                Instr::CallBuiltin(b, argc) => {
                    let b = *b;
                    let argc = *argc;
                    b.call(self, argc)?;
                    self.ip += 1;
                }

                Instr::MakeClosure(addr, captures) => {
                    let addr = *addr;
                    if addr as usize >= self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    let captures = captures.clone(); // release the borrow on self.code
                    let local_count = self.callstack.last().ok_or(VMError::BadLocal)?.local_count;
                    let mut upvals = ThinVec::with_capacity(captures.len());
                    for slot in captures {
                        if slot >= local_count {
                            return Err(VMError::BadLocal);
                        }
                        // Copy the slot verbatim: a Boxed slot carries its Upval
                        // handle (shared, by-reference), a Plain slot its value
                        // (a by-value snapshot).
                        upvals.push(self.stack[(self.fp + slot) as usize]);
                    }
                    let closure = self.alloc_closure(addr, upvals);
                    self.stack.push(closure);
                    self.ip += 1;
                }

                Instr::Return(nrets) => {
                    let frame = self.callstack.pop().ok_or(VMError::BadReturn)?;
                    // `fp` points at the frame base (arg 0 / local 0), which is
                    // where the caller pushed the args — so the return value(s)
                    // replace the whole frame, restoring the caller's stack.
                    let keep_below = self.fp as usize;
                    let n = *nrets;
                    if self.stack.len() < keep_below + n {
                        return Err(VMError::StackUnderflow);
                    }
                    let ret_start = self.stack.len() - n;
                    for i in 0..n {
                        self.stack[keep_below + i] = self.stack[ret_start + i];
                    }
                    self.stack.truncate(keep_below + n);
                    self.ip = frame.return_addr;
                    self.fp = frame.prev_fp;
                    if self.callstack.is_empty() {
                        return Ok(StepResult::Done);
                    }
                }

                Instr::Jump(addr) => {
                    if *addr as usize > self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    self.ip = *addr;
                }

                Instr::JFalse(addr) => {
                    if *addr as usize > self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    if !self.is_truthy(&val) {
                        self.ip = *addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::JTrue(addr) => {
                    if *addr as usize > self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    if self.is_truthy(&val) {
                        self.ip = *addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::JNotNullish(addr) => {
                    if *addr as usize > self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    // Peek: leave the value for the branch that proceeds with it.
                    let val = self.stack.last().ok_or(VMError::StackUnderflow)?;
                    if !matches!(val, StackValue::Null | StackValue::Undefined) {
                        self.ip = *addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::Label(_) => {
                    // Eliminated in a pre-pass; no-op at runtime.
                    self.ip += 1;
                }

                // ── frame access ────────────────────────────────
                Instr::EnterFrame(nparams, build_args, local_kinds) => {
                    let nparams = *nparams;
                    let build_args = *build_args;
                    let local_kinds = local_kinds.clone(); // release borrow on self.code
                    let frame = self.callstack.last().ok_or(VMError::BadArg)?;
                    let argc = frame.arg_count;
                    // The args arrived as the leading locals at [fp, fp + argc).
                    // 1. Materialize the `arguments` array (from the actual args)
                    //    BEFORE normalizing, if the body uses it.
                    if build_args {
                        let base = self.fp as usize;
                        if base + argc as usize > self.stack.len() {
                            return Err(VMError::StackUnderflow);
                        }
                        let args: ThinVec<StackValue> = self.stack[base..base + argc as usize]
                            .iter()
                            .copied()
                            .collect();
                        let arr = self.alloc_array(args);
                        if let StackValue::Ptr(p) = arr {
                            self.callstack.last_mut().unwrap().arguments_cache = Some(p);
                        }
                    }
                    // 2. Normalize the arg region to exactly `nparams` slots:
                    //    drop surplus args, or pad missing params with Undefined.
                    let want = self.fp as usize + nparams as usize;
                    if self.stack.len() > want {
                        self.stack.truncate(want);
                    } else {
                        self.stack.resize(want, StackValue::Undefined);
                    }
                    // 3. Install the closure's captured environment as the upval
                    //    locals, now landing at [fp + nparams, fp + nparams + K).
                    let upvals =
                        std::mem::take(&mut self.callstack.last_mut().unwrap().pending_upvals);
                    let k = upvals.len() as u32;
                    for uv in upvals {
                        self.stack.push(uv);
                    }
                    // 4. Allocate the declared (non-param) own locals + self-ref
                    // slot (Boxed → fresh cell + Upval).
                    for kind in &local_kinds {
                        let slot = match kind {
                            SlotKind::Plain => StackValue::Undefined,
                            SlotKind::Boxed => {
                                let idx = self.cells.len() as CellIndex;
                                self.cells.push(StackValue::Undefined);
                                StackValue::Upval(idx)
                            }
                        };
                        self.stack.push(slot);
                    }
                    self.callstack.last_mut().unwrap().local_count =
                        nparams as u32 + k + local_kinds.len() as u32;
                    self.ip += 1;
                }

                Instr::Arguments => {
                    let frame = self.callstack.last().ok_or(VMError::BadArg)?;
                    // Reuse the cached array when this frame already built one
                    // (functions that use `arguments` build it eagerly in the
                    // prologue's EnterFrame; the lazy path here serves the root
                    // frame, which has no args).
                    if let Some(ptr) = frame.arguments_cache {
                        self.stack.push(StackValue::Ptr(ptr));
                        self.ip += 1;
                        continue;
                    }
                    // Build it from the frame's args (arg 0 at fp). Copy them out
                    // before touching the heap.
                    let argc = frame.arg_count;
                    let base = self.fp as usize;
                    if base + argc as usize > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    let args: ThinVec<StackValue> = self.stack[base..base + argc as usize]
                        .iter()
                        .copied()
                        .collect();
                    let arr = self.alloc_array(args);
                    let ptr = match arr {
                        StackValue::Ptr(p) => p,
                        _ => unreachable!("alloc_array returns a Ptr"),
                    };
                    self.callstack.last_mut().unwrap().arguments_cache = Some(ptr);
                    self.stack.push(arr);
                    self.ip += 1;
                }

                Instr::Local(local) => {
                    let frame = self.callstack.last().ok_or(VMError::BadLocal)?;
                    if *local >= frame.local_count {
                        return Err(VMError::BadLocal);
                    }
                    // A Boxed slot holds an Upval marker; dereference it so the
                    // value — never the marker — reaches the expression stack.
                    let val = match self.stack[(self.fp + local) as usize] {
                        StackValue::Upval(c) => {
                            *self.cells.get(c as usize).ok_or(VMError::ValueError)?
                        }
                        other => other,
                    };
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::SetLocal(local) => {
                    let frame = self.callstack.last().ok_or(VMError::BadLocal)?;
                    if *local >= frame.local_count {
                        return Err(VMError::BadLocal);
                    }
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let slot = (self.fp + local) as usize;
                    // Write through a Boxed slot to its shared cell; a Plain slot
                    // is overwritten in place.
                    match self.stack[slot] {
                        StackValue::Upval(c) => {
                            *self.cells.get_mut(c as usize).ok_or(VMError::ValueError)? = val;
                        }
                        _ => self.stack[slot] = val,
                    }
                    self.ip += 1;
                }

                Instr::TeeLocal(local) => {
                    let frame = self.callstack.last().ok_or(VMError::BadLocal)?;
                    if *local >= frame.local_count {
                        return Err(VMError::BadLocal);
                    }
                    let val = *self.stack.last().ok_or(VMError::StackUnderflow)?;
                    let slot = (self.fp + local) as usize;
                    // Like SetLocal but peeks: the value stays on the stack
                    // (assignment is an expression) while still writing to the
                    // local. Replaces Dup; SetLocal.
                    match self.stack[slot] {
                        StackValue::Upval(c) => {
                            *self.cells.get_mut(c as usize).ok_or(VMError::ValueError)? = val;
                        }
                        _ => self.stack[slot] = val,
                    }
                    self.ip += 1;
                }

                Instr::FreshCell(local) => {
                    let frame = self.callstack.last().ok_or(VMError::BadLocal)?;
                    if *local >= frame.local_count {
                        return Err(VMError::BadLocal);
                    }
                    let slot = (self.fp + local) as usize;
                    // Read the current value, dereferencing an existing Upval.
                    let val = match self.stack[slot] {
                        StackValue::Upval(c) => {
                            *self.cells.get(c as usize).ok_or(VMError::ValueError)?
                        }
                        other => other,
                    };
                    // Allocate a fresh cell seeded with that value and point the
                    // slot at it, so subsequent captures see a per-iteration cell.
                    let idx = self.cells.len() as CellIndex;
                    self.cells.push(val);
                    self.stack[slot] = StackValue::Upval(idx);
                    self.ip += 1;
                }

                Instr::IncLocal(local, p, mode) => {
                    let frame = self.callstack.last().ok_or(VMError::BadLocal)?;
                    if u32::from(*local) >= frame.local_count {
                        return Err(VMError::BadLocal);
                    }
                    // Read current value (dereferencing boxed slots).
                    let old = match self.stack[(self.fp + u32::from(*local)) as usize] {
                        StackValue::Upval(c) => {
                            *self.cells.get(c as usize).ok_or(VMError::ValueError)?
                        }
                        other => other,
                    };
                    let old_num = self.to_number(&old).ok_or(VMError::TypeError)?;
                    // Compute new value: subtract p (p = -1 for ++, p = 1 for --).
                    let new_num = old_num - *p;
                    let new_val = StackValue::Number(new_num);
                    // Store the new value.
                    let slot = (self.fp + u32::from(*local)) as usize;
                    match self.stack[slot] {
                        StackValue::Upval(c) => {
                            *self.cells.get_mut(c as usize).ok_or(VMError::ValueError)? = new_val;
                        }
                        _ => self.stack[slot] = new_val,
                    }
                    // Push the appropriate result: old for postfix, new for prefix.
                    let result = match mode {
                        UpdateMode::Prefix => new_val,
                        UpdateMode::Postfix => StackValue::Number(old_num),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                // ── type queries ────────────────────────────────
                Instr::TypeOf => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    // JS typeof tags. Note the coarseness: null/array/object all
                    // report "object"; int and float both "number".
                    let tag = match val {
                        StackValue::Undefined => "undefined",
                        StackValue::Null => "object",
                        StackValue::Bool(_) => "boolean",
                        StackValue::Number(_) | StackValue::PosInt(_) | StackValue::NegInt(_) => {
                            "number"
                        }
                        StackValue::Fn(_) | StackValue::Builtin(_) => "function",
                        StackValue::Ptr(p) => match self.heap_get(p)? {
                            HeapValue::String(_) => "string",
                            HeapValue::Array(_) | HeapValue::Object(_) => "object",
                            HeapValue::Closure { .. } => "function",
                        },
                        // Internal indirection; never a legitimate operand.
                        StackValue::Upval(_) => return Err(VMError::ValueError),
                    };
                    let s = self.alloc_string(ThinString::from(tag));
                    self.stack.push(s);
                    self.ip += 1;
                }

                // ── type predicates ─────────────────────────────
                Instr::IsNull => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(StackValue::Bool(matches!(val, StackValue::Null)));
                    self.ip += 1;
                }
                Instr::IsBool => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(StackValue::Bool(matches!(val, StackValue::Bool(_))));
                    self.ip += 1;
                }
                Instr::IsFloat => {
                    // True only for a Number with a fractional part (an Int is
                    // never a float). Use IsNum to test "is any number".
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let is_float = matches!(val, StackValue::Number(n) if !float_is_int(n));
                    self.stack.push(StackValue::Bool(is_float));
                    self.ip += 1;
                }
                Instr::IsNum => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack.push(StackValue::Bool(matches!(
                        val,
                        StackValue::Number(_) | StackValue::PosInt(_) | StackValue::NegInt(_)
                    )));
                    self.ip += 1;
                }
                Instr::IsStr => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let is_str = matches!(
                        val,
                        StackValue::Ptr(p)
                            if matches!(self.heap.get(p as usize), Some(HeapValue::String(_)))
                    );
                    self.stack.push(StackValue::Bool(is_str));
                    self.ip += 1;
                }
                Instr::IsObj => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let is_obj = matches!(
                        val,
                        StackValue::Ptr(p)
                            if matches!(self.heap.get(p as usize), Some(HeapValue::Object(_)))
                    );
                    self.stack.push(StackValue::Bool(is_obj));
                    self.ip += 1;
                }

                // ── unary operators ─────────────────────────────
                Instr::Neg => unary_num!(|n: f64| -n),

                Instr::Not => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack.push(StackValue::Bool(!self.is_truthy(&val)));
                    self.ip += 1;
                }

                Instr::BitNot => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match as_i64(&val) {
                        Some(i) => {
                            self.stack.push(StackValue::Number(!i as f64));
                            self.ip += 1;
                        }
                        None => return Err(VMError::TypeError),
                    }
                }

                // ── binary operators ────────────────────────────
                Instr::Add => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    // JS `+`: if either operand is a string, concatenate (ToString
                    // both); otherwise add numerically (ToNumber both). An
                    // array/object/function in the numeric path is a TypeError
                    // (we do not ToPrimitive it — see the note on `loose_equal`).
                    let result = if self.is_string(&lhs) || self.is_string(&rhs) {
                        let mut s = self.to_js_string(&lhs, 0);
                        s.push_str(self.to_js_string(&rhs, 0).as_str());
                        self.alloc_string(s)
                    } else {
                        match (self.to_number(&lhs), self.to_number(&rhs)) {
                            (Some(a), Some(b)) => StackValue::Number(a + b),
                            _ => return Err(VMError::TypeError),
                        }
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::Sub => binary_num!(|a: f64, b: f64| a - b),
                Instr::Mul => binary_num!(|a: f64, b: f64| a * b),
                // JS `/`: never throws — a zero divisor yields ±Infinity (or NaN
                // for 0/0), which f64 division produces directly.
                Instr::Div => binary_num!(|a: f64, b: f64| a / b),
                // JS `%`: float remainder with the dividend's sign; `x % 0` is
                // NaN. Rust's f64 `%` matches this exactly.
                Instr::Mod => binary_num!(|a: f64, b: f64| a % b),
                Instr::Pow => binary_num!(|a: f64, b: f64| a.powf(b)),

                Instr::Eq => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(StackValue::Bool(self.values_equal(&lhs, &rhs)));
                    self.ip += 1;
                }
                Instr::Neq => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(StackValue::Bool(!self.values_equal(&lhs, &rhs)));
                    self.ip += 1;
                }
                Instr::LooseEq => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(StackValue::Bool(self.loose_equal(&lhs, &rhs)));
                    self.ip += 1;
                }
                Instr::LooseNeq => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(StackValue::Bool(!self.loose_equal(&lhs, &rhs)));
                    self.ip += 1;
                }

                Instr::Lt => cmp_op!(Less),
                Instr::Gt => cmp_op!(Greater),
                Instr::LtEq => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let result = self
                        .compare(&lhs, &rhs)
                        .map(|ord| ord != std::cmp::Ordering::Greater)
                        .unwrap_or(false);
                    self.stack.push(StackValue::Bool(result));
                    self.ip += 1;
                }
                Instr::GtEq => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let result = self
                        .compare(&lhs, &rhs)
                        .map(|ord| ord != std::cmp::Ordering::Less)
                        .unwrap_or(false);
                    self.stack.push(StackValue::Bool(result));
                    self.ip += 1;
                }

                Instr::And => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(if self.is_truthy(&lhs) { rhs } else { lhs });
                    self.ip += 1;
                }
                Instr::Or => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack
                        .push(if self.is_truthy(&lhs) { lhs } else { rhs });
                    self.ip += 1;
                }

                Instr::BitAnd => binary_int!(|a: i64, b: i64| a & b),
                Instr::BitOr => binary_int!(|a: i64, b: i64| a | b),
                Instr::BitXor => binary_int!(|a: i64, b: i64| a ^ b),
                // Shift count must be in [0, 63]; anything else would overflow
                // (a panic in debug builds) so reject it as a ValueError.
                Instr::BitLhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(VMError::ValueError);
                    }
                    self.stack.push(StackValue::Number((a << b) as f64));
                    self.ip += 1;
                }
                Instr::BitRhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(VMError::ValueError);
                    }
                    self.stack.push(StackValue::Number((a >> b) as f64));
                    self.ip += 1;
                }

                // ── object operations ───────────────────────────
                Instr::ObjNew(fields) => {
                    let n = fields.len();
                    if n > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    let split = self.stack.len() - n;
                    let vals: ThinVec<StackValue> = self.stack.drain(split..).collect();
                    let mut obj = IndexMap::new();
                    // Left-to-right: field 0's value is the deepest (first
                    // pushed), so values line up with fields in order.
                    for (i, field) in fields.iter().enumerate() {
                        obj.insert(field.clone(), vals[i]);
                    }
                    let obj_ptr = self.alloc_object(obj);
                    self.stack.push(obj_ptr);
                    self.ip += 1;
                }

                Instr::ObjGet(field) => {
                    let field = field.clone();
                    let obj_ptr = self.pop_ptr()?;
                    // JS: a missing property reads as `undefined`, not `null`.
                    let val = self
                        .heap_obj(obj_ptr)
                        .and_then(|obj| obj.get(field.as_ref()).copied())
                        .unwrap_or(StackValue::Undefined);
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::ObjSet(field, mode) => {
                    let field = field.clone();
                    let mode = *mode;
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let obj_ptr = self.pop_ptr()?;
                    let obj = self.heap_obj_mut(obj_ptr).ok_or(VMError::TypeError)?;
                    // In Old mode, read the previous value before overwriting
                    // (for postfix `++`/`--` on member targets).
                    let old = match mode {
                        SetMode::Old => obj
                            .get(field.as_ref())
                            .copied()
                            .unwrap_or(StackValue::Undefined),
                        SetMode::New => StackValue::Undefined, // placeholder, unused
                    };
                    obj.insert(field, val);
                    match mode {
                        SetMode::New => self.stack.push(val),
                        SetMode::Old => self.stack.push(old),
                    }
                    self.ip += 1;
                }

                // Runtime-polymorphic computed read. Dispatch on the container
                // type: array (int index), object (ToString key), or string
                // (byte-offset char). A char result needs a fresh allocation, so
                // it is computed under the heap borrow and allocated after.
                Instr::IndexGet => {
                    let key = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let container = self.pop_ptr()?;
                    let mut to_alloc: Option<ThinString> = None;
                    let val = match self.heap_get(container)? {
                        HeapValue::Array(arr) => {
                            let idx = as_i64(&key).ok_or(VMError::TypeError)?;
                            if idx < 0 {
                                return Err(VMError::ValueError);
                            }
                            // JS: an out-of-bounds index reads as `undefined`.
                            arr.get(idx as usize)
                                .copied()
                                .unwrap_or(StackValue::Undefined)
                        }
                        HeapValue::Object(obj) => {
                            // JS coerces a computed key with ToString.
                            let field = self.to_js_string(&key, 0);
                            // JS: a missing property reads as `undefined`.
                            obj.get(field.as_str()).copied().unwrap_or(StackValue::Undefined)
                        }
                        HeapValue::String(s) => {
                            let s = s.as_str();
                            let idx = as_i64(&key).ok_or(VMError::TypeError)?;
                            if idx < 0 {
                                return Err(VMError::ValueError);
                            }
                            let idx = idx as usize;
                            if idx >= s.len() {
                                // JS: an out-of-range char index is `undefined`.
                                StackValue::Undefined
                            } else if !s.is_char_boundary(idx) {
                                return Err(VMError::ValueError);
                            } else {
                                // The codepoint starting at this byte, as a
                                // 1-char string (built after the borrow ends).
                                to_alloc = Some(ThinString::from(
                                    s[idx..].chars().next().unwrap().to_string().as_str(),
                                ));
                                StackValue::Undefined // placeholder, replaced below
                            }
                        }
                        _ => return Err(VMError::TypeError),
                    };
                    let result = match to_alloc {
                        Some(s) => self.alloc_string(s),
                        None => val,
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                // Runtime-polymorphic computed write. Arrays index by int (OOB or
                // negative is an error — no hole-growing); objects key by the
                // ToString'd key; strings are immutable (TypeError).
                Instr::IndexSet(mode) => {
                    let mode = *mode;
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let key = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let container = self.pop_ptr()?;
                    // Determine the container kind, releasing the borrow before
                    // taking the mutable one below.
                    let is_array = match self.heap_get(container)? {
                        HeapValue::Array(_) => true,
                        HeapValue::Object(_) => false,
                        // Strings are immutable; closures aren't indexable.
                        _ => return Err(VMError::TypeError),
                    };
                    let old = if matches!(mode, SetMode::Old) {
                        // Read the previous value before the write (for postfix
                        // `++`/`--` on computed targets).
                        if is_array {
                            let idx = as_i64(&key).ok_or(VMError::TypeError)?;
                            if idx < 0 {
                                return Err(VMError::ValueError);
                            }
                            self.heap_arr(container)
                                .and_then(|a| a.get(idx as usize).copied())
                                .unwrap_or(StackValue::Undefined)
                        } else {
                            let field = self.to_js_string(&key, 0);
                            self.heap_obj(container)
                                .and_then(|o| o.get(field.as_str()).copied())
                                .unwrap_or(StackValue::Undefined)
                        }
                    } else {
                        StackValue::Undefined // placeholder, unused
                    };
                    if is_array {
                        let idx = as_i64(&key).ok_or(VMError::TypeError)?;
                        if idx < 0 {
                            return Err(VMError::ValueError);
                        }
                        let idx = idx as usize;
                        let arr = self.heap_arr_mut(container).ok_or(VMError::TypeError)?;
                        if idx >= arr.len() {
                            return Err(VMError::ValueError);
                        }
                        arr[idx] = val;
                    } else {
                        let field = self.to_js_string(&key, 0);
                        let obj = self.heap_obj_mut(container).ok_or(VMError::TypeError)?;
                        obj.insert(field, val);
                    }
                    match mode {
                        SetMode::New => self.stack.push(val),
                        SetMode::Old => self.stack.push(old),
                    }
                    self.ip += 1;
                }

                Instr::ObjHas => {
                    let field = self.pop_string()?;
                    let obj_ptr = self.pop_ptr()?;
                    let has = self
                        .heap_obj(obj_ptr)
                        .ok_or(VMError::TypeError)?
                        .contains_key(field.as_str());
                    self.stack.push(StackValue::Bool(has));
                    self.ip += 1;
                }

                Instr::ObjDelete => {
                    let field = self.pop_string()?;
                    let obj_ptr = self.pop_ptr()?;
                    // shift_remove keeps the remaining keys in insertion order.
                    let existed = self
                        .heap_obj_mut(obj_ptr)
                        .ok_or(VMError::TypeError)?
                        .shift_remove(field.as_str())
                        .is_some();
                    self.stack.push(StackValue::Bool(existed));
                    self.ip += 1;
                }

                // ── array operations ────────────────────────────
                Instr::ArrNew(n) => {
                    let n = *n as usize;
                    if n > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    let split = self.stack.len() - n;
                    // Left-to-right: first pushed becomes element 0.
                    let vals: ThinVec<StackValue> = self.stack.drain(split..).collect();
                    let arr_ptr = self.alloc_array(vals);
                    self.stack.push(arr_ptr);
                    self.ip += 1;
                }

                Instr::ArrLength => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let len = match val {
                        StackValue::Ptr(p) => match self.heap_get(p)? {
                            HeapValue::Array(a) => a.len(),
                            // String length is in UTF-8 *bytes* (consistent with
                            // the byte-offset string ops below).
                            HeapValue::String(s) => s.len(),
                            _ => return Err(VMError::TypeError),
                        },
                        _ => return Err(VMError::TypeError),
                    };
                    self.stack.push(StackValue::Number(len as f64));
                    self.ip += 1;
                }

                Instr::ToStr => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let s = self.to_js_string(&val, 0);
                    let ptr = self.alloc_string(s);
                    self.stack.push(ptr);
                    self.ip += 1;
                }

                Instr::ToNum => {
                    // ToNumber, matching the arithmetic operators' coercion: an
                    // array/object/function has no numeric form (TypeError).
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match self.to_number(&val) {
                        Some(num) => self.stack.push(StackValue::Number(num)),
                        None => return Err(VMError::TypeError),
                    }
                    self.ip += 1;
                }

                Instr::ToBool => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack.push(StackValue::Bool(self.is_truthy(&val)));
                    self.ip += 1;
                }

                // ── external effects ───────────────────────────
                Instr::Invoke(..) => {
                    // Gather the run of consecutive Invoke instructions into one
                    // fan-out request. Args are laid out left-to-right (normal
                    // codegen): across the batch, call 0's args are deepest and
                    // the last call's args on top; within a call, arg 0 is
                    // deepest. So we partition the arg region front-to-back in
                    // call order — no reversal. The host resolves all calls and
                    // pushes one result per call (in call order) before resuming.
                    let mut sigs: Vec<(String, u32)> = Vec::new();
                    let mut ip = self.ip;
                    while let Some((name, nargs)) = self.invoke_at(ip) {
                        // The outer loop already charged fuel for the first
                        // invoke; charge each additional one here so a large
                        // batch can't bypass the budget.
                        if !sigs.is_empty() {
                            if self.fuel == 0 {
                                return Err(VMError::OutOfFuel);
                            }
                            self.fuel -= 1;
                        }
                        sigs.push((name, nargs));
                        ip += 1;
                    }
                    self.ip = ip; // resume after the batch once host resolves

                    let total: usize = sigs.iter().map(|(_, n)| *n as usize).sum();
                    if total > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    // Region in push order: region[0] is call 0's arg 0.
                    let region = self.stack.split_off(self.stack.len() - total);
                    let mut calls = Vec::with_capacity(sigs.len());
                    let mut idx = 0;
                    for (name, nargs) in sigs {
                        let n = nargs as usize;
                        let args = region[idx..idx + n].to_vec();
                        idx += n;
                        calls.push(InvokeCall { name, args });
                    }
                    return Ok(StepResult::Invoke { calls });
                }

                Instr::Raise(condition) => {
                    // Do NOT advance ip — host may choose a different
                    // restart point (see Raise doc comment).
                    return Ok(StepResult::Raise {
                        condition: condition.as_str().to_owned(),
                    });
                }
            }
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::Instr::*;
    use super::*;

    // ── harness ──────────────────────────────────────────────────

    /// Run code in a fresh VM (no initial heap) to completion; return final stack.
    fn run(code: Vec<Instr>) -> Vec<StackValue> {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm.stack.clone(),
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    /// Run code with pre-allocated heap strings (addr 0, 1, 2, …).
    fn run_heap(code: Vec<Instr>, strings: &[&str]) -> Vec<StackValue> {
        let mut vm = VM::new(code);
        for s in strings {
            vm.alloc_string((*s).into());
        }
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm.stack.clone(),
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    /// Run code to the first effect (Invoke/Raise), returning the StepResult.
    fn run_effect(code: Vec<Instr>) -> StepResult {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => panic!("unexpected completion"),
                effect => return effect,
            }
        }
    }

    /// Run code that is expected to error; return the error.
    fn run_err(code: Vec<Instr>) -> VMError {
        let mut vm = VM::new(code);
        loop {
            match vm.step() {
                Err(e) => return e,
                Ok(StepResult::Done) => panic!("unexpected completion"),
                Ok(_) => panic!("unexpected effect"),
            }
        }
    }

    // ── helpers ───────────────────────────────────────────────────

    fn n(v: f64) -> StackValue {
        StackValue::Number(v)
    }
    /// Canonical integer value: non-negative -> PosInt, negative -> NegInt.
    fn i(v: i64) -> StackValue {
        if v < 0 {
            StackValue::NegInt(v)
        } else {
            StackValue::PosInt(v as u64)
        }
    }
    /// A PosInt directly (for values above i64::MAX).
    fn u(v: u64) -> StackValue {
        StackValue::PosInt(v)
    }
    /// A function value pointing at a code address.
    fn f(addr: u32) -> StackValue {
        StackValue::Fn(addr)
    }
    fn b(v: bool) -> StackValue {
        StackValue::Bool(v)
    }
    fn null() -> StackValue {
        StackValue::Null
    }
    fn undef() -> StackValue {
        StackValue::Undefined
    }
    /// Heap pointer to string at the given index (pre-loaded via run_heap).
    fn s(addr: u32) -> StackValue {
        StackValue::Ptr(addr)
    }
    /// `n` plain (unboxed) local slots, for `EnterFrame`.
    fn plain(n: usize) -> Vec<SlotKind> {
        vec![SlotKind::Plain; n]
    }

    // ── stack manipulation ────────────────────────────────────────

    #[test]
    fn push_and_pop() {
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(2.0), Pop(1)]), vec![n(1.0)]);
        assert_eq!(run(vec![PushFloat(1.0), Pop(1)]), vec![]);
        assert!(matches!(run_err(vec![Pop(1)]), VMError::StackUnderflow));
    }

    #[test]
    fn dup_swap_rot() {
        assert_eq!(run(vec![PushFloat(1.0), Dup]), vec![n(1.0), n(1.0)]);
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), Swap]),
            vec![n(2.0), n(1.0)]
        );
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Rot]),
            vec![n(2.0), n(3.0), n(1.0)]
        );
        assert!(matches!(run_err(vec![Swap]), VMError::StackUnderflow));
        assert!(matches!(run_err(vec![Rot]), VMError::StackUnderflow));
    }

    #[test]
    fn pick() {
        // Pick(0) is Dup; Pick(n) copies the n-th-from-top value to the top.
        assert_eq!(run(vec![PushFloat(1.0), Pick(0)]), vec![n(1.0), n(1.0)]);
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), Pick(1)]),
            vec![n(1.0), n(2.0), n(1.0)]
        );
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Pick(2)]),
            vec![n(1.0), n(2.0), n(3.0), n(1.0)]
        );
        // Cannot reach below the frame's temporaries.
        assert!(matches!(
            run_err(vec![PushFloat(1.0), Pick(1)]),
            VMError::StackUnderflow
        ));
    }

    #[test]
    fn dig() {
        // Dig(0) no-op, Dig(1) == Swap, Dig(2) == Rot.
        assert_eq!(run(vec![PushFloat(1.0), Dig(0)]), vec![n(1.0)]);
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), Dig(1)]),
            vec![n(2.0), n(1.0)]
        );
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Dig(2)]),
            vec![n(2.0), n(3.0), n(1.0)]
        );
        assert!(matches!(
            run_err(vec![PushFloat(1.0), Dig(1)]),
            VMError::StackUnderflow
        ));
    }

    #[test]
    fn nip() {
        // Nip(1) drops the value below top, leaving top in place.
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(2.0), Nip(1)]), vec![n(2.0)]);
        // Nip(2) drops two values below top.
        assert_eq!(
            run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Nip(2)]),
            vec![n(3.0)]
        );
        // Nip rejects reaching below frame_floor.
        assert!(matches!(
            run_err(vec![PushFloat(1.0), Nip(1)]),
            VMError::StackUnderflow
        ));
    }

    #[test]
    fn tee_local() {
        // TeeLocal writes top to a local without popping.
        // EnterFrame allocates Undefined as local 0; Push pushes 5.0 on top;
        // TeeLocal writes 5.0 into local 0 (replacing Undefined) and
        // leaves it on the stack. Result: [5.0, 5.0].
        assert_eq!(
            run(vec![
                EnterFrame(0, false, vec![SlotKind::Plain].into()),
                PushFloat(5.0),
                TeeLocal(0),
            ]),
            vec![n(5.0), n(5.0)]
        );
        // Verify the local was actually written.
        assert_eq!(
            run(vec![
                EnterFrame(0, false, vec![SlotKind::Plain].into()),
                PushFloat(7.0),
                TeeLocal(0),
                Pop(1),
                Local(0),
            ]),
            vec![n(7.0), n(7.0)]
        );
        // TeeLocal on out-of-range slot errors.
        assert!(matches!(
            run_err(vec![PushFloat(1.0), TeeLocal(0)]),
            VMError::BadLocal
        ));
    }

    #[test]
    fn inc_local() {
        use crate::vm::UpdateMode;
        // Prefix ++ in place: new value on stack AND in local.
        assert_eq!(
            run(vec![
                EnterFrame(0, false, vec![SlotKind::Plain].into()),
                PushFloat(5.0),
                SetLocal(0),
                IncLocal(0, -1.0, UpdateMode::Prefix),
            ]),
            vec![n(6.0), n(6.0)]
        );
        // Postfix ++: old value on stack, local updated to new.
        assert_eq!(
            run(vec![
                EnterFrame(0, false, vec![SlotKind::Plain].into()),
                PushFloat(5.0),
                SetLocal(0),
                IncLocal(0, -1.0, UpdateMode::Postfix),
            ]),
            vec![n(6.0), n(5.0)]
        );
        // Postfix --: old value pushed, local decremented.
        assert_eq!(
            run(vec![
                EnterFrame(0, false, vec![SlotKind::Plain].into()),
                PushFloat(5.0),
                SetLocal(0),
                IncLocal(0, 1.0, UpdateMode::Postfix),
                Pop(1), // drop old value
                Local(0),
            ]),
            vec![n(4.0), n(4.0)]
        );
        // IncLocal on out-of-range slot errors.
        assert!(matches!(
            run_err(vec![IncLocal(0, -1.0, UpdateMode::Prefix)]),
            VMError::BadLocal
        ));
    }

    #[test]
    fn jtrue() {
        // Truthy takes the jump (skipping the Push); falsy falls through.
        assert_eq!(run(vec![PushBool(true), JTrue(3), PushFloat(9.0)]), vec![]);
        assert_eq!(
            run(vec![PushBool(false), JTrue(3), PushFloat(9.0)]),
            vec![n(9.0)]
        );
    }

    #[test]
    fn jnotnullish() {
        // Not nullish: takes the jump and LEAVES the value (peek, no pop).
        assert_eq!(
            run(vec![PushFloat(5.0), JNotNullish(3), PushFloat(9.0)]),
            vec![n(5.0)]
        );
        // null / undefined: fall through; the value stays for the short-circuit.
        assert_eq!(
            run(vec![PushNull, JNotNullish(3), PushFloat(9.0)]),
            vec![null(), n(9.0)]
        );
        assert_eq!(
            run(vec![PushUndefined, JNotNullish(3), PushFloat(9.0)]),
            vec![undef(), n(9.0)]
        );
    }

    // ── type predicates ───────────────────────────────────────────

    #[test]
    fn is_null() {
        assert_eq!(run(vec![PushNull, IsNull]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(0.0), IsNull]), vec![b(false)]);
    }

    #[test]
    fn is_bool() {
        assert_eq!(run(vec![PushBool(true), IsBool]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(0.0), IsBool]), vec![b(false)]);
    }

    // ── unary operators ───────────────────────────────────────────

    #[test]
    fn not_bitnot() {
        assert_eq!(run(vec![PushBool(false), Not]), vec![b(true)]);
        assert_eq!(run(vec![PushNull, Not]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(1.0), Not]), vec![b(false)]);
        assert_eq!(run(vec![PushFloat(5.0), BitNot]), vec![n(-6.0)]);
        assert!(matches!(
            run_err(vec![PushFloat(3.14), BitNot]),
            VMError::TypeError
        ));
    }

    // ── binary operators ──────────────────────────────────────────

    #[test]
    fn add_sub_mul_div() {
        assert_eq!(run(vec![PushFloat(2.0), PushFloat(3.0), Add]), vec![n(5.0)]);
        assert_eq!(run(vec![PushFloat(10.0), PushFloat(3.0), Sub]), vec![n(7.0)]);
        assert_eq!(run(vec![PushFloat(4.0), PushFloat(5.0), Mul]), vec![n(20.0)]);
        assert_eq!(run(vec![PushFloat(10.0), PushFloat(4.0), Div]), vec![n(2.5)]);
        // JS: x/0 -> ±Infinity, 0/0 -> NaN (never an error).
        assert!(matches!(
            run(vec![PushFloat(1.0), PushFloat(0.0), Div]).as_slice(),
            [StackValue::Number(x)] if x.is_infinite() && *x > 0.0
        ));
        assert!(matches!(
            run(vec![PushFloat(0.0), PushFloat(0.0), Div]).as_slice(),
            [StackValue::Number(x)] if x.is_nan()
        ));
    }

    #[test]
    fn mod_op() {
        assert_eq!(run(vec![PushFloat(10.0), PushFloat(3.0), Mod]), vec![n(1.0)]);
        // JS %: float remainder (5.5 % 2 == 1.5), dividend's sign (-5 % 3 == -2).
        assert_eq!(run(vec![PushFloat(5.5), PushFloat(2.0), Mod]), vec![n(1.5)]);
        assert_eq!(run(vec![PushFloat(-5.0), PushFloat(3.0), Mod]), vec![n(-2.0)]);
        // x % 0 -> NaN, not an error.
        assert!(matches!(
            run(vec![PushFloat(1.0), PushFloat(0.0), Mod]).as_slice(),
            [StackValue::Number(x)] if x.is_nan()
        ));
    }

    #[test]
    fn add_strings() {
        // heap[0]="hello ", heap[1]="world"
        assert_eq!(
            run_heap(vec![PushPtr(0), PushPtr(1), Add], &["hello ", "world"]),
            vec![s(2)] // new string at heap[2]
        );
        // Verify the concatenated string
        let mut vm = VM::new(vec![Add]);
        vm.heap.push(HeapValue::String("hello ".into()));
        vm.heap.push(HeapValue::String("world".into()));
        vm.stack.push(s(0));
        vm.stack.push(s(1));
        match vm.step().unwrap() {
            StepResult::Done => {}
            _ => panic!(),
        }
        assert_eq!(vm.heap[2], HeapValue::String("hello world".into()));
    }

    /// Run `code` (with preloaded heap strings) and return the last heap value
    /// as a String, panicking if it isn't one. Handy for ops that allocate a
    /// result string (Add concat, ToStr, ArrJoin).
    fn run_last_str(code: Vec<Instr>, strings: &[&str]) -> String {
        let mut vm = VM::new(code);
        for s in strings {
            vm.alloc_string((*s).into());
        }
        while !matches!(vm.step().unwrap(), StepResult::Done) {}
        match vm.heap.last() {
            Some(HeapValue::String(s)) => s.as_str().to_owned(),
            other => panic!("expected a string result, got {other:?}"),
        }
    }

    #[test]
    fn add_concat_coerces() {
        // `+` concatenates when either side is a string, coercing the other.
        assert_eq!(
            run_last_str(vec![PushPtr(0), PushFloat(5.0), Add], &["x="]),
            "x=5"
        );
        assert_eq!(
            run_last_str(vec![PushFloat(5.0), PushPtr(0), Add], &["!"]),
            "5!"
        );
        assert_eq!(
            run_last_str(vec![PushPtr(0), PushNull, Add], &["v="]),
            "v=null"
        );
        assert_eq!(
            run_last_str(vec![PushPtr(0), PushBool(true), Add], &["b="]),
            "b=true"
        );
        // An array operand stringifies like join(",") on the concat path.
        assert_eq!(
            run_last_str(
                vec![PushFloat(1.0), PushFloat(2.0), ArrNew(2), PushPtr(0), Add],
                &["!"]
            ),
            "1,2!"
        );
    }

    #[test]
    fn arithmetic_coerces() {
        // ToNumber coercion on -, *, /, % (strings, bools, null).
        assert_eq!(
            run_heap(vec![PushPtr(0), PushFloat(1.0), Sub], &["6"]),
            vec![n(5.0)]
        );
        assert_eq!(run(vec![PushBool(true), PushFloat(2.0), Mul]), vec![n(2.0)]);
        assert_eq!(run(vec![PushNull, PushFloat(1.0), Add]), vec![n(1.0)]);
        assert_eq!(
            run_heap(vec![PushPtr(0), PushPtr(1), Mul], &["6", "2"]),
            vec![n(12.0)]
        );
        // undefined -> NaN propagates.
        assert!(matches!(
            run(vec![PushUndefined, PushFloat(1.0), Sub]).as_slice(),
            [StackValue::Number(x)] if x.is_nan()
        ));
        // An unparseable string -> NaN.
        assert!(matches!(
            run_heap(vec![PushPtr(0), PushFloat(1.0), Mul], &["abc"]).as_slice(),
            [StackValue::Number(x)] if x.is_nan()
        ));
    }

    #[test]
    fn truthiness_matches_js() {
        // Falsy: false, 0, NaN, "", null, undefined.
        for code in [
            vec![PushBool(false), Not],
            vec![PushFloat(0.0), Not],
            vec![PushPosInt(0), Not],
            vec![PushFloat(f64::NAN), Not],
            vec![PushNull, Not],
            vec![PushUndefined, Not],
        ] {
            assert_eq!(run(code), vec![b(true)], "expected falsy");
        }
        assert_eq!(run_heap(vec![PushPtr(0), Not], &[""]), vec![b(true)]); // "" falsy
        // Truthy: nonzero, "0", non-empty string, [], {}.
        assert_eq!(run(vec![PushFloat(1.0), Not]), vec![b(false)]);
        assert_eq!(run_heap(vec![PushPtr(0), Not], &["0"]), vec![b(false)]); // "0" truthy
        assert_eq!(run(vec![ArrNew(0), Not]), vec![b(false)]); // [] truthy
        assert_eq!(run(vec![ObjNew(vec![].into()), Not]), vec![b(false)]); // {} truthy
        // And JFalse on 0 takes the branch (0 is falsy).
        assert_eq!(run(vec![PushFloat(0.0), JFalse(3), PushFloat(9.0)]), vec![]);
        // || picks the second operand when the first is 0 (falsy).
        assert_eq!(run(vec![PushFloat(0.0), PushFloat(7.0), Or]), vec![n(7.0)]);
    }

    #[test]
    fn to_num_instruction() {
        // JS ToNumber: strings parse, bools→0/1, null→0, undefined/garbage→NaN.
        assert_eq!(run_heap(vec![PushPtr(0), ToNum], &["42"]), vec![n(42.0)]);
        assert_eq!(run(vec![PushBool(true), ToNum]), vec![n(1.0)]);
        assert_eq!(run(vec![PushNull, ToNum]), vec![n(0.0)]);
        assert!(matches!(
            run(vec![PushUndefined, ToNum]).as_slice(),
            [StackValue::Number(x)] if x.is_nan()
        ));
        // An array/object has no numeric form.
        assert!(matches!(
            run_err(vec![ArrNew(0), ToNum]),
            VMError::TypeError
        ));
    }

    #[test]
    fn to_bool_instruction() {
        assert_eq!(run(vec![PushFloat(0.0), ToBool]), vec![b(false)]);
        assert_eq!(run(vec![PushFloat(1.0), ToBool]), vec![b(true)]);
        assert_eq!(run(vec![PushNull, ToBool]), vec![b(false)]);
        assert_eq!(run_heap(vec![PushPtr(0), ToBool], &[""]), vec![b(false)]);
        assert_eq!(run(vec![ArrNew(0), ToBool]), vec![b(true)]); // [] is truthy
    }

    #[test]
    fn to_str_instruction() {
        assert_eq!(run_last_str(vec![PushFloat(5.0), ToStr], &[]), "5");
        assert_eq!(run_last_str(vec![PushFloat(1.5), ToStr], &[]), "1.5");
        assert_eq!(run_last_str(vec![PushNegInt(-3), ToStr], &[]), "-3");
        assert_eq!(run_last_str(vec![PushNull, ToStr], &[]), "null");
        assert_eq!(run_last_str(vec![PushUndefined, ToStr], &[]), "undefined");
        assert_eq!(run_last_str(vec![PushBool(true), ToStr], &[]), "true");
        // NaN / Infinity get JS spellings.
        assert_eq!(run_last_str(vec![PushFloat(f64::NAN), ToStr], &[]), "NaN");
        assert_eq!(
            run_last_str(vec![PushFloat(f64::INFINITY), ToStr], &[]),
            "Infinity"
        );
        // Array -> join(","), object -> "[object Object]".
        assert_eq!(
            run_last_str(vec![PushFloat(1.0), PushFloat(2.0), ArrNew(2), ToStr], &[]),
            "1,2"
        );
        assert_eq!(
            run_last_str(vec![PushFloat(1.0), ObjNew(vec!["a".into()].into()), ToStr], &[]),
            "[object Object]"
        );
    }

    // ── comparisons ───────────────────────────────────────────────

    #[test]
    fn eq_neq() {
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(1.0), Eq]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(2.0), Eq]), vec![b(false)]);
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(2.0), Neq]), vec![b(true)]);
        // NaN != NaN
        assert_eq!(
            run(vec![PushFloat(f64::NAN), PushFloat(f64::NAN), Eq]),
            vec![b(false)]
        );
        // different types are not equal
        assert_eq!(run(vec![PushFloat(0.0), PushNull, Eq]), vec![b(false)]);
    }

    #[test]
    fn loose_eq_nullish() {
        // null == undefined (and reflexively), but neither == anything else.
        assert_eq!(
            run(vec![PushNull, PushUndefined, LooseEq]),
            vec![b(true)]
        );
        assert_eq!(
            run(vec![PushUndefined, PushNull, LooseEq]),
            vec![b(true)]
        );
        assert_eq!(
            run(vec![PushNull, PushNull, LooseEq]),
            vec![b(true)]
        );
        assert_eq!(
            run(vec![PushNull, PushFloat(0.0), LooseEq]),
            vec![b(false)]
        );
        assert_eq!(
            run(vec![PushUndefined, PushBool(false), LooseEq]),
            vec![b(false)]
        );
        // strict still distinguishes them
        assert_eq!(run(vec![PushNull, PushUndefined, Eq]), vec![b(false)]);
        // LooseNeq is the negation
        assert_eq!(
            run(vec![PushNull, PushUndefined, LooseNeq]),
            vec![b(false)]
        );
        assert_eq!(
            run(vec![PushNull, PushFloat(0.0), LooseNeq]),
            vec![b(true)]
        );
    }

    #[test]
    fn loose_eq_boolean_coercion() {
        // booleans coerce to 0/1.
        assert_eq!(
            run(vec![PushBool(true), PushFloat(1.0), LooseEq]),
            vec![b(true)]
        );
        assert_eq!(
            run(vec![PushBool(false), PushFloat(0.0), LooseEq]),
            vec![b(true)]
        );
        assert_eq!(
            run(vec![PushBool(true), PushFloat(2.0), LooseEq]),
            vec![b(false)]
        );
    }

    #[test]
    fn loose_eq_number_string_coercion() {
        // heap[0]="1", [1]="", [2]="abc", [3]="1.5"
        let strings = ["1", "", "abc", "1.5"];
        // 1 == "1"
        assert_eq!(
            run_heap(vec![PushFloat(1.0), PushPtr(0), LooseEq], &strings),
            vec![b(true)]
        );
        // "1" == 1 (other order)
        assert_eq!(
            run_heap(vec![PushPtr(0), PushFloat(1.0), LooseEq], &strings),
            vec![b(true)]
        );
        // 0 == "" (empty string ToNumber is 0)
        assert_eq!(
            run_heap(vec![PushFloat(0.0), PushPtr(1), LooseEq], &strings),
            vec![b(true)]
        );
        // false == "" via double coercion
        assert_eq!(
            run_heap(vec![PushBool(false), PushPtr(1), LooseEq], &strings),
            vec![b(true)]
        );
        // 1 == "abc" -> NaN -> false
        assert_eq!(
            run_heap(vec![PushFloat(1.0), PushPtr(2), LooseEq], &strings),
            vec![b(false)]
        );
        // 1.5 == "1.5"
        assert_eq!(
            run_heap(vec![PushFloat(1.5), PushPtr(3), LooseEq], &strings),
            vec![b(true)]
        );
    }

    #[test]
    fn loose_eq_strings_not_coerced_to_each_other() {
        // Two strings still compare as strings (no numeric coercion): "1" vs "1.0".
        assert_eq!(
            run_heap(vec![PushPtr(0), PushPtr(1), LooseEq], &["1", "1.0"]),
            vec![b(false)]
        );
    }

    #[test]
    fn loose_eq_object_vs_primitive_not_coerced() {
        // Documented divergence: [5] == 5 is false here (true in JS).
        assert_eq!(
            run(vec![PushFloat(5.0), ArrNew(1), PushFloat(5.0), LooseEq]),
            vec![b(false)]
        );
    }

    #[test]
    fn string_eq() {
        // heap[0]="abc", heap[1]="abc", heap[2]="xyz"
        let code = vec![PushPtr(0), PushPtr(1), Eq, PushPtr(0), PushPtr(2), Eq];
        let mut vm = VM::new(code);
        vm.heap.push(HeapValue::String("abc".into()));
        vm.heap.push(HeapValue::String("abc".into()));
        vm.heap.push(HeapValue::String("xyz".into()));
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(vm.stack, vec![b(true), b(false)]);
    }

    #[test]
    fn ordering() {
        // Numbers
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(2.0), Lt]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(2.0), PushFloat(1.0), Gt]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(2.0), PushFloat(2.0), LtEq]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(2.0), PushFloat(2.0), GtEq]), vec![b(true)]);
        // Incomparable types → false
        assert_eq!(run(vec![PushFloat(1.0), PushNull, Lt]), vec![b(false)]);
    }

    #[test]
    fn and_or() {
        // truthy && rhs → rhs
        assert_eq!(run(vec![PushBool(true), PushFloat(42.0), And]), vec![n(42.0)]);
        // falsy && rhs → falsy
        assert_eq!(
            run(vec![PushBool(false), PushFloat(42.0), And]),
            vec![b(false)]
        );
        // truthy || rhs → truthy
        assert_eq!(run(vec![PushFloat(42.0), PushBool(false), Or]), vec![n(42.0)]);
        // falsy || rhs → rhs
        assert_eq!(run(vec![PushNull, PushFloat(99.0), Or]), vec![n(99.0)]);
    }

    // ── bitwise ops ───────────────────────────────────────────────

    #[test]
    fn bitwise() {
        assert_eq!(
            run(vec![PushFloat(10.0), PushFloat(12.0), BitAnd]),
            vec![n(8.0)] // 0b1010 & 0b1100 = 0b1000
        );
        assert_eq!(
            run(vec![PushFloat(10.0), PushFloat(12.0), BitOr]),
            vec![n(14.0)] // 0b1010 | 0b1100 = 0b1110
        );
        assert_eq!(
            run(vec![PushFloat(10.0), PushFloat(12.0), BitXor]),
            vec![n(6.0)] // 0b1010 ^ 0b1100 = 0b0110
        );
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(3.0), BitLhs]), vec![n(8.0)]);
        assert_eq!(run(vec![PushFloat(8.0), PushFloat(2.0), BitRhs]), vec![n(2.0)]);
    }

    // ── control flow ──────────────────────────────────────────────

    #[test]
    fn jump_and_jfalse() {
        // Jump over a Push: should only leave n(1.0) on stack
        assert_eq!(
            run(vec![PushFloat(1.0), Jump(3), PushFloat(999.0)]),
            vec![n(1.0)]
        );
        // JFalse with false → jump over Push
        assert_eq!(run(vec![PushBool(false), JFalse(3), PushFloat(999.0)]), vec![]);
        // JFalse with true → don't jump, execute Push
        assert_eq!(
            run(vec![PushBool(true), JFalse(3), PushFloat(42.0)]),
            vec![n(42.0)]
        );
        // Null is falsy
        assert_eq!(run(vec![PushNull, JFalse(3), PushFloat(999.0)]), vec![]);
    }

    #[test]
    fn label_noop() {
        // Label should be a no-op at runtime
        assert_eq!(
            run(vec![PushFloat(1.0), Label(42), PushFloat(2.0)]),
            vec![n(1.0), n(2.0)]
        );
    }

    #[test]
    fn call_and_return() {
        // Main calls a function at index 4 that adds its two args and
        // returns the sum.  Main then returns that value.
        // Left-to-right: first pushed is arg 0.
        //
        // [0] Push(10)   -- arg 0
        // [1] Push(20)   -- arg 1
        // [2] Call(4, 2) -- call fn at 4 with 2 args
        // [3] Return(1)  -- main returns 1 value
        // [4] Local(0)   -- fn: args arrive in place as locals (10)
        // [5] Local(1)   -- fn: local 1 (20)
        // [6] Add        -- fn: 10 + 20 = 30
        // [7] Return(1)  -- fn: return 1 value
        assert_eq!(
            run(vec![
                PushFloat(10.0),
                PushFloat(20.0),
                Call(4, 2),
                Return(1),
                Local(0),
                Local(1),
                Add,
                Return(1),
            ]),
            vec![n(30.0)]
        );
    }

    #[test]
    fn arg_order_is_left_to_right() {
        // Non-commutative op pins the convention: fn computes arg0 - arg1.
        // Push 10 then 3 -> arg0=10, arg1=3 -> 10 - 3 = 7.
        assert_eq!(
            run(vec![
                PushFloat(10.0),
                PushFloat(3.0),
                Call(4, 2),
                Return(1),
                Local(0),
                Local(1),
                Sub,
                Return(1),
            ]),
            vec![n(7.0)]
        );
    }

    #[test]
    fn call_dyn_indirect() {
        // Indirect call through a Fn value. Function at [4] computes arg0 - arg1.
        // Layout: push args left-to-right, then the callable on top.
        // [0] Push(10)       arg 0
        // [1] Push(3)        arg 1
        // [2] Push(Fn(5))    callable on top
        // [3] CallDyn(2)
        // [4] Return(1)      main returns the result
        // [5] Local(0)       fn body: args arrive in place as locals
        // [6] Local(1)
        // [7] Sub            10 - 3
        // [8] Return(1)
        assert_eq!(
            run(vec![
                PushFloat(10.0),
                PushFloat(3.0),
                PushFn(5),
                CallDyn(2),
                Return(1),
                Local(0),
                Local(1),
                Sub,
                Return(1),
            ]),
            vec![n(7.0)]
        );
    }

    #[test]
    fn call_dyn_requires_fn() {
        // Top of stack must be a Fn, not some other value.
        let code = vec![PushFloat(1.0), PushFloat(2.0), CallDyn(1)];
        assert!(matches!(run_err(code), VMError::TypeError));
    }

    #[test]
    fn arguments_builds_array_of_frame_args() {
        // Call a fn with 3 args; its body builds `arguments` and returns it.
        // [0..2] push args, [3] callable, [4] CallDyn(3), [5] Return(1)
        // [6] Arguments (fn body), [7] Return(1)
        let mut vm = VM::new(vec![
            PushFloat(10.0),
            PushFloat(20.0),
            PushFloat(30.0),
            PushFn(6),
            CallDyn(3),
            Return(1),
            Arguments,
            Return(1),
        ]);
        while !matches!(vm.step().unwrap(), StepResult::Done) {}
        match vm.stack.as_slice() {
            [StackValue::Ptr(p)] => match &vm.heap[*p as usize] {
                HeapValue::Array(a) => assert_eq!(a, &vec![n(10.0), n(20.0), n(30.0)]),
                other => panic!("expected array, got {other:?}"),
            },
            other => panic!("expected one Ptr, got {other:?}"),
        }
    }

    #[test]
    fn arguments_is_cached_within_a_frame() {
        // Two `Arguments` in the same frame yield the SAME heap pointer (the
        // per-frame cache), so `Eq` (reference equality for arrays) is true.
        let out = run(vec![
            PushFloat(1.0),
            PushFn(4),
            CallDyn(1),
            Return(1),
            Arguments, // fn body: build (and cache)
            Arguments, // reuse the cached array
            Eq,        // same Ptr → true
            Return(1),
        ]);
        assert_eq!(out, vec![b(true)]);
    }

    #[test]
    fn call_dyn_bad_addr() {
        let code = vec![PushFn(999), CallDyn(0)];
        assert!(matches!(run_err(code), VMError::BadCall));
    }

    #[test]
    fn fn_value_equality_and_json() {
        // Same address -> equal; different -> not.
        assert_eq!(run(vec![PushFn(3), PushFn(3), Eq]), vec![b(true)]);
        assert_eq!(run(vec![PushFn(3), PushFn(4), Eq]), vec![b(false)]);
    }

    // ── closures ──────────────────────────────────────────────────

    /// Append a `makeCounter` to `code`: a function that boxes a `count` local
    /// (slot 0), initializes it to 0, and returns a closure that increments and
    /// returns `count`. Returns makeCounter's code address.
    fn append_counter(code: &mut Vec<Instr>) -> u32 {
        let mc = code.len() as u32;
        code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into())); // slot 0 = count (by-ref)
        code.push(PushFloat(0.0));
        code.push(SetLocal(0)); // count = 0 (writes through the cell)
        let mk = code.len();
        code.push(MakeClosure(0, vec![0].into())); // patched: capture count
        code.push(Return(1));
        let inner = code.len() as u32;
        // 0 params, 1 upval → EnterFrame installs the captured cell at slot 0.
        code.push(EnterFrame(0, false, vec![].into()));
        code.push(Local(0)); // count  (slot 0 = captured upval)
        code.push(PushFloat(1.0));
        code.push(Add);
        code.push(SetLocal(0)); // count = count + 1 (through the shared cell)
        code.push(Local(0));
        code.push(Return(1)); // return count
        code[mk] = MakeClosure(inner, vec![0].into());
        mc
    }

    /// Run to completion and return the finished VM (to inspect heap/cells).
    fn run_vm(code: Vec<Instr>) -> VM {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    #[test]
    fn closure_captures_by_reference_across_calls() {
        // c = makeCounter(); c() + c()  →  1 + 2 = 3. The captured cell
        // persists between calls (and outlives makeCounter's frame), so the
        // count is not reset — that's capture by reference.
        let mut code: Vec<Instr> = Vec::new();
        let call_mc = code.len();
        code.push(Call(0, 0)); // patched → makeCounter; leaves a closure
        code.push(Dup);
        code.push(CallDyn(0)); // first call → 1
        code.push(Swap);
        code.push(CallDyn(0)); // second call → 2
        code.push(Add);
        code.push(Return(1));
        let mc = append_counter(&mut code);
        code[call_mc] = Call(mc, 0);
        assert_eq!(run(code), vec![n(3.0)]);
    }

    #[test]
    fn closures_have_independent_cells() {
        // Two makeCounter() results must not share state: c1(), c1(), c2()
        // → [1, 2, 1].
        let mut code: Vec<Instr> = Vec::new();
        code.push(EnterFrame(0, false, plain(2).into())); // local 0 = c1, local 1 = c2
        let call1 = code.len();
        code.push(Call(0, 0));
        code.push(SetLocal(0));
        let call2 = code.len();
        code.push(Call(0, 0));
        code.push(SetLocal(1));
        code.push(Local(0));
        code.push(CallDyn(0)); // c1() → 1
        code.push(Local(0));
        code.push(CallDyn(0)); // c1() → 2
        code.push(Local(1));
        code.push(CallDyn(0)); // c2() → 1
        code.push(ArrNew(3));
        code.push(Return(1));
        let mc = append_counter(&mut code);
        code[call1] = Call(mc, 0);
        code[call2] = Call(mc, 0);

        let vm = run_vm(code);
        let StackValue::Ptr(p) = vm.stack[0] else {
            panic!("expected array pointer");
        };
        assert_eq!(
            vm.heap[p as usize],
            HeapValue::Array(vec![n(1.0), n(2.0), n(1.0)].into())
        );
    }

    #[test]
    fn closure_captures_plain_slot_by_value() {
        // A Plain (unboxed) slot is captured by *value*: the closure snapshots
        // the value at capture time, so mutating the local afterward is not
        // observed. maker(): x=5; f=closure-over-x; x=99; return f. f() → 5.
        let mut code: Vec<Instr> = Vec::new();
        let call = code.len();
        code.push(Call(0, 0)); // patched → maker; leaves a closure
        code.push(CallDyn(0));
        code.push(Return(1));
        // maker
        let maker = code.len() as u32;
        code.push(EnterFrame(0, false, plain(1).into())); // slot 0 = x (NOT boxed)
        code.push(PushFloat(5.0));
        code.push(SetLocal(0));
        let mk = code.len();
        code.push(MakeClosure(0, vec![0].into())); // snapshot x = 5
        code.push(PushFloat(99.0));
        code.push(SetLocal(0)); // x = 99 AFTER capture (must not be seen)
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(EnterFrame(0, false, vec![].into())); // install the by-value upval at slot 0
        code.push(Local(0)); // return captured snapshot
        code.push(Return(1));
        code[call] = Call(maker, 0);
        code[mk] = MakeClosure(inner, vec![0].into());
        assert_eq!(run(code), vec![n(5.0)]);
    }

    #[test]
    fn two_closures_share_one_cell() {
        // A getter and a setter closing over the same boxed `x` must see each
        // other's writes. setter(42) then getter() → 42.
        let mut code: Vec<Instr> = Vec::new();
        // main: arr = maker(); setter = arr[1]; setter(42); getter = arr[0]; getter()
        code.push(EnterFrame(0, false, plain(1).into())); // local 0 = [getter, setter]
        let call = code.len();
        code.push(Call(0, 0));
        code.push(SetLocal(0));
        code.push(PushFloat(42.0)); // setter's arg
        code.push(Local(0));
        code.push(PushFloat(1.0));
        code.push(IndexGet); // setter
        code.push(CallDyn(1)); // setter(42) → (no result)
        code.push(Local(0));
        code.push(PushFloat(0.0));
        code.push(IndexGet); // getter
        code.push(CallDyn(0)); // getter() → 42
        code.push(Return(1));
        // maker
        let maker = code.len() as u32;
        code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into())); // slot 0 = x (by-ref)
        code.push(PushFloat(0.0));
        code.push(SetLocal(0));
        let mk_get = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        let mk_set = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        code.push(ArrNew(2)); // [getter, setter]
        code.push(Return(1));
        let getter = code.len() as u32;
        code.push(EnterFrame(0, false, vec![].into())); // upval x at slot 0
        code.push(Local(0));
        code.push(Return(1));
        let setter = code.len() as u32;
        // 1 param (slot 0) + 1 upval x (slot 1): write the param into x's cell.
        code.push(EnterFrame(1, false, vec![].into()));
        code.push(Local(0)); // the arg
        code.push(SetLocal(1)); // x = arg (through the shared cell)
        code.push(Return(0));
        code[call] = Call(maker, 0);
        code[mk_get] = MakeClosure(getter, vec![0].into());
        code[mk_set] = MakeClosure(setter, vec![0].into());
        assert_eq!(run(code), vec![n(42.0)]);
    }

    #[test]
    fn nested_capture_forwards_same_cell() {
        // outer boxes x=7 and returns `middle`; middle returns `inner`; inner
        // reads x. The cell threads through both closure levels unchanged.
        // outer()()() → 7.
        let mut code: Vec<Instr> = Vec::new();
        let call = code.len();
        code.push(Call(0, 0)); // → middle closure
        code.push(CallDyn(0)); // → inner closure
        code.push(CallDyn(0)); // → 7
        code.push(Return(1));
        let outer = code.len() as u32;
        code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into()));
        code.push(PushFloat(7.0));
        code.push(SetLocal(0));
        let mk_mid = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        code.push(Return(1));
        let middle = code.len() as u32;
        // middle's slot 0 is x (installed upval); forward it to inner.
        code.push(EnterFrame(0, false, vec![].into()));
        let mk_in = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(EnterFrame(0, false, vec![].into()));
        code.push(Local(0));
        code.push(Return(1));
        code[call] = Call(outer, 0);
        code[mk_mid] = MakeClosure(middle, vec![0].into());
        code[mk_in] = MakeClosure(inner, vec![0].into());
        assert_eq!(run(code), vec![n(7.0)]);
    }

    #[test]
    fn closure_identity_equality() {
        // The same closure object equals itself (reference identity)…
        let same = vec![
            EnterFrame(0, false, vec![SlotKind::Boxed].into()),
            PushFloat(1.0),
            SetLocal(0),
            MakeClosure(6, vec![0].into()),
            Dup,
            Eq,
            Return(1), // addr 6: also a valid (never-called) closure target
        ];
        assert_eq!(run(same), vec![b(true)]);
        // …but two distinct closure objects do not (no content equality).
        let distinct = vec![
            EnterFrame(0, false, vec![SlotKind::Boxed].into()),
            PushFloat(1.0),
            SetLocal(0),
            MakeClosure(7, vec![0].into()),
            MakeClosure(7, vec![0].into()),
            Eq,
            Return(1),
            Return(1), // addr 7
        ];
        assert_eq!(run(distinct), vec![b(false)]);
    }

    #[test]
    fn make_closure_rejects_out_of_range_capture() {
        // Capturing a slot the frame doesn't have is a compiler bug → BadLocal.
        let code = vec![
            Call(2, 0),
            Return(0),
            EnterFrame(0, false, plain(1).into()),
            MakeClosure(0, vec![5].into()), // only slot 0 exists
            Return(1),
        ];
        assert!(matches!(run_err(code), VMError::BadLocal));
    }

    #[test]
    fn call_dyn_rejects_non_closure_pointer() {
        // A Ptr to a non-closure heap value (here an array) is not callable.
        let code = vec![ArrNew(0), CallDyn(0)];
        assert!(matches!(run_err(code), VMError::TypeError));
    }

    #[test]
    fn call_with_locals() {
        // Function allocates a local, stores arg+arg in it, returns it. Args
        // arrive in place as locals 0,1; the declared local is allocated at
        // slot 2 (after the two params).
        // [4] EnterFrame(2, false, plain(1)) -- 2 params + 1 local at slot 2
        // [5] Local(0)                        -- arg 0 (7)
        // [6] Local(1)                        -- arg 1 (8)
        // [7] Add
        // [8] SetLocal(2)
        // [9] Local(2)
        // [10] Return(1)
        assert_eq!(
            run(vec![
                PushFloat(7.0),
                PushFloat(8.0),
                Call(4, 2),
                Return(1),
                EnterFrame(2, false, plain(1).into()),
                Local(0),
                Local(1),
                Add,
                SetLocal(2),
                Local(2),
                Return(1),
            ]),
            vec![n(15.0)]
        );
    }

    // ── frame access validation ───────────────────────────────────

    #[test]
    fn local_oob() {
        // Called with one arg (→ local 0); reading local 1 is out of range.
        let code = vec![
            PushFloat(1.0),
            Call(3, 1),
            Return(0),
            Local(1), // only local 0 (the arg) exists
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::BadLocal));
    }

    #[test]
    fn call_bad_addr() {
        assert!(matches!(run_err(vec![Call(999, 0)]), VMError::BadCall));
    }

    // ── array operations ──────────────────────────────────────────

    #[test]
    fn arr_new_and_length() {
        assert_eq!(
            run(vec![
                PushFloat(1.0),
                PushFloat(2.0),
                PushFloat(3.0),
                ArrNew(3),
                ArrLength
            ]),
            vec![n(3.0)]
        );
    }

    #[test]
    fn arr_get_set() {
        // Create [10, 20, 30], set arr[1] = 99, read it back.
        let code = vec![
            PushFloat(10.0),
            PushFloat(20.0),
            PushFloat(30.0),
            ArrNew(3),
            PushFloat(1.0),           // index
            PushFloat(99.0),          // value
            IndexSet(SetMode::New), // pops value, index, arr_ptr; leaves the value
        ];
        // IndexSet leaves the assigned value (assignment is an expression).
        assert_eq!(run(code), vec![n(99.0)]);
    }

    #[test]
    fn arr_get_set_with_dup() {
        // Keep ptr around with Dup before mutation.
        let code = vec![
            PushFloat(10.0),
            PushFloat(20.0),
            PushFloat(30.0),
            ArrNew(3),
            Dup,                    // save ptr for later
            PushFloat(1.0),           // index
            PushFloat(99.0),          // value
            IndexSet(SetMode::New), // pops value, index, ptr_copy; leaves value → [ptr, 99]
            Pop(1),                 // drop the assigned-value result → [ptr]
            PushFloat(1.0),           // index
            IndexGet,               // pops index, ptr → pushes arr[1]
        ];
        assert_eq!(run(code), vec![n(99.0)]);
    }

    #[test]
    fn arr_get_oob() {
        let code = vec![
            PushFloat(10.0),
            ArrNew(1),
            PushFloat(5.0), // index 5, out of bounds
            IndexGet,     // JS: out-of-bounds reads as undefined
        ];
        assert_eq!(run(code), vec![undef()]);
    }

    #[test]
    fn arr_set_oob() {
        let code = vec![
            PushFloat(10.0),
            ArrNew(1),
            PushFloat(5.0),           // index
            PushFloat(99.0),          // value
            IndexSet(SetMode::New), // pops: value, index, arr_ptr
        ];
        assert!(matches!(run_err(code), VMError::ValueError));
    }

    // ── object operations ─────────────────────────────────────────

    #[test]
    fn obj_new_get_set() {
        // Left-to-right: fields ["a","b"], values pushed in field order.
        // Push a-val (20), push b-val (10) → a=20, b=10
        let mut vm = VM::new(vec![
            PushFloat(20.0), // "a" value (first field, pushed first)
            PushFloat(10.0), // "b" value (second field)
            ObjNew(vec!["a".into(), "b".into()].into()),
            PushPtr(0), // field "a" (heap[0]="a")
            IndexGet,   // pops key, obj_ptr → pushes obj["a"]
        ]);
        vm.alloc_string("a".into());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        // IndexGet consumes obj_ptr, so stack only has the retrieved value.
        assert_eq!(vm.stack, vec![n(20.0)]);
    }

    #[test]
    fn obj_get_set_dynamic() {
        // Computed get via IndexGet with a heap-allocated string key.
        let mut vm = VM::new(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            ObjNew(vec!["x".into(), "y".into()].into()), // x=1, y=2
            PushPtr(0),                           // field "x" (heap[0]="x")
            IndexGet,                             // → 1
        ]);
        vm.alloc_string("x".into());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(vm.stack, vec![n(1.0)]);

        // Computed set via IndexSet: set a field, verify with ObjGet.
        let mut vm = VM::new(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            ObjNew(vec!["x".into(), "y".into()].into()), // x=1, y=2
            Dup,                                  // keep ptr for verification
            PushPtr(0),                           // field "y" (heap[0]="y") — pushed before val
            PushFloat(99.0),                        // val — on top
            IndexSet(SetMode::New),               // obj.y = 99; leaves val → [ptr, 99]
            Pop(1),                               // drop the result → [ptr]
            ObjGet("y".into()),                   // → 99
        ]);
        vm.alloc_string("y".into());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(vm.stack, vec![n(99.0)]);
    }

    #[test]
    fn obj_get_known_set_known() {
        // Test ObjSet (static set) and ObjGet (static get).
        // Create {x:2, y:1}, modify x=99 with ObjSet, verify with ObjGet.
        let code = vec![
            PushFloat(2.0),                         // x value
            PushFloat(1.0),                         // y value
            ObjNew(vec!["x".into(), "y".into()].into()), // x=2, y=1
            Dup,           // keep ptr for verification after ObjSet consumes one
            PushFloat(99.0), // value to set
            ObjSet("x".into(), SetMode::New), // obj.x = 99; leaves value → [ptr, 99]
            Pop(1),        // drop the result → [ptr]
            ObjGet("x".into()), // → 99
        ];
        assert_eq!(run(code), vec![n(99.0)]);
    }

    #[test]
    fn obj_get_missing_key() {
        let code = vec![
            PushFloat(1.0),
            ObjNew(vec!["x".into()].into()),
            ObjGet("no_such_key".into()), // JS: missing key reads as undefined
        ];
        assert_eq!(run(code), vec![undef()]);
    }

    // ── undefined & typeof ────────────────────────────────────────

    #[test]
    fn undefined_is_falsy() {
        assert_eq!(run(vec![PushUndefined, Not]), vec![b(true)]);
        // Branches like null: JFalse on undefined takes the jump.
        assert_eq!(run(vec![PushUndefined, JFalse(3), PushFloat(9.0)]), vec![]);
    }

    #[test]
    fn undefined_strict_equality() {
        // undefined === undefined, but undefined !== null (Eq is strict ===).
        assert_eq!(run(vec![PushUndefined, PushUndefined, Eq]), vec![b(true)]);
        assert_eq!(run(vec![PushUndefined, PushNull, Eq]), vec![b(false)]);
        assert_eq!(run(vec![PushNull, PushUndefined, Neq]), vec![b(true)]);
    }

    #[test]
    fn undefined_is_not_comparable() {
        // Relational ops on undefined are all false (compare() yields None),
        // matching JS `undefined < 1 === false`, `undefined >= undefined === false`.
        assert_eq!(run(vec![PushUndefined, PushFloat(1.0), Lt]), vec![b(false)]);
        assert_eq!(
            run(vec![PushUndefined, PushUndefined, GtEq]),
            vec![b(false)]
        );
    }

    #[test]
    fn uninitialized_local_is_undefined() {
        // `let x;` then read x -> undefined.
        let code = vec![
            EnterFrame(0, false, vec![SlotKind::Plain].into()),
            Local(0),
            Return(1),
        ];
        assert_eq!(run(code), vec![undef()]);
    }

    #[test]
    fn typeof_tags() {
        // typeof returns JS strings; check each via a heap-string comparison.
        let cases: &[(StackValue, &str)] = &[
            (undef(), "undefined"),
            (null(), "object"),
            (b(true), "boolean"),
            (n(3.5), "number"),
            (i(7), "number"),
            (f(0), "function"),
        ];
        for (val, tag) in cases {
            let instr = match val {
                StackValue::Undefined => PushUndefined,
                StackValue::Null => PushNull,
                StackValue::Bool(b) => PushBool(*b),
                StackValue::Number(f) => PushFloat(*f),
                StackValue::PosInt(u) => PushPosInt(*u),
                StackValue::NegInt(i) => PushNegInt(*i),
                StackValue::Fn(a) => PushFn(*a),
                _ => panic!("unexpected stack value"),
            };
            let out = run_heap(vec![instr, TypeOf, PushPtr(0), Eq], &[tag]);
            assert_eq!(out, vec![b(true)], "typeof {val:?} should be {tag:?}");
        }
    }

    #[test]
    fn typeof_heap_values() {
        // string -> "string", array/object -> "object", closure -> "function".
        let str_tag = run_heap(vec![PushPtr(0), TypeOf, PushPtr(1), Eq], &["hi", "string"]);
        assert_eq!(str_tag, vec![b(true)]);
        let arr_tag = run_heap(
            vec![PushFloat(1.0), ArrNew(1), TypeOf, PushPtr(0), Eq],
            &["object"],
        );
        assert_eq!(arr_tag, vec![b(true)]);
        let obj_tag = run_heap(
            vec![
                PushFloat(1.0),
                ObjNew(vec!["a".into()].into()),
                TypeOf,
                PushPtr(0),
                Eq,
            ],
            &["object"],
        );
        assert_eq!(obj_tag, vec![b(true)]);
        // typeof a missing property is "undefined".
        let miss_tag = run_heap(
            vec![
                PushFloat(1.0),
                ObjNew(vec!["a".into()].into()),
                ObjGet("b".into()),
                TypeOf,
                PushPtr(0),
                Eq,
            ],
            &["undefined"],
        );
        assert_eq!(miss_tag, vec![b(true)]);
    }

    // ── effects ───────────────────────────────────────────────────

    #[test]
    fn invoke_yields() {
        match run_effect(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            Invoke("my_tool".into(), 2),
        ]) {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "my_tool");
                // push order = arg order: Push(1), Push(2) -> args [1, 2]
                assert_eq!(calls[0].args, vec![n(1.0), n(2.0)]);
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    #[test]
    fn raise_yields() {
        match run_effect(vec![Raise("something_broke".into())]) {
            StepResult::Raise { condition } => {
                assert_eq!(condition, "something_broke");
            }
            other => panic!("expected Raise, got {other:?}"),
        }
    }

    #[test]
    fn resume_after_invoke() {
        let mut vm = VM::new(vec![
            PushFloat(10.0),
            PushFloat(3.0),
            Invoke("add".into(), 2),
            Return(1), // return the result
        ]);
        // First step should yield Invoke
        match vm.step().unwrap() {
            StepResult::Invoke { .. } => {}
            other => panic!("expected Invoke, got {other:?}"),
        }
        // Host pushes result
        vm.stack.push(n(13.0));
        // Resume — should complete with the result on stack
        match vm.step().unwrap() {
            StepResult::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(vm.stack, vec![n(13.0)]);
    }

    #[test]
    fn invoke_batches_consecutive() {
        // Two consecutive Invokes fan out in one step. Left-to-right codegen:
        // evaluate/push all calls' args in order — call 0 (a) deepest, and
        // within a multi-arg call, arg 0 deepest. Here: a(1, 2), b(3).
        let mut vm = VM::new(vec![
            PushFloat(1.0), // a's arg 0
            PushFloat(2.0), // a's arg 1
            PushFloat(3.0), // b's arg 0
            Invoke("a".into(), 2),
            Invoke("b".into(), 1),
        ]);
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "a");
                assert_eq!(calls[0].args, vec![n(1.0), n(2.0)]);
                assert_eq!(calls[1].name, "b");
                assert_eq!(calls[1].args, vec![n(3.0)]);
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
        // Host pushes one result per call, in call order.
        vm.stack.push(n(100.0)); // a's result
        vm.stack.push(n(200.0)); // b's result
        match vm.step().unwrap() {
            StepResult::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(vm.stack, vec![n(100.0), n(200.0)]);
    }

    #[test]
    fn invoke_does_not_batch_across_other_ops() {
        // A non-Invoke instruction between two Invokes breaks the batch.
        let mut vm = VM::new(vec![
            PushFloat(1.0),
            Invoke("a".into(), 1),
            PushFloat(2.0),
            Invoke("b".into(), 1),
        ]);
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "a");
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
        vm.stack.push(n(11.0)); // a's result
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "b");
                assert_eq!(calls[0].args, vec![n(2.0)]);
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    // ── edge cases ────────────────────────────────────────────────

    #[test]
    fn empty_program_done() {
        let mut vm = VM::new(vec![]);
        assert!(matches!(vm.step().unwrap(), StepResult::Done));
    }

    #[test]
    fn number_signed_zero() {
        // -0.0 and +0.0 should be equal for Eq
        assert_eq!(run(vec![PushFloat(-0.0), PushFloat(0.0), Eq]), vec![b(true)]);
    }

    // ── robustness / regression ───────────────────────────────────

    #[test]
    fn fuel_stops_infinite_loop() {
        // [Jump(0)] loops forever; the fuel budget must break it.
        let mut vm = VM::new(vec![Jump(0)]);
        vm.fuel = 1000;
        assert!(matches!(vm.step().unwrap_err(), VMError::OutOfFuel));
        assert_eq!(vm.fuel, 0);
    }

    #[test]
    fn fuel_is_consumed_per_instruction() {
        let mut vm = VM::new(vec![PushFloat(1.0), PushFloat(2.0), Add]);
        let before = vm.fuel;
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(before - vm.fuel, 3); // three instructions executed
    }

    #[test]
    fn dangling_pointer_does_not_panic() {
        // A Ptr literal with no backing heap cell must error, not panic.
        assert!(matches!(
            run_err(vec![PushPtr(99), ArrLength]),
            VMError::ValueError
        ));
        // Type predicates stay total (false) on a dangling pointer.
        assert_eq!(run(vec![PushPtr(99), IsStr]), vec![b(false)]);
        // Equality with a dangling pointer is simply not-equal, no panic.
        assert_eq!(run(vec![PushPtr(99), PushPtr(99), Eq]), vec![b(false)]);
    }

    #[test]
    fn object_array_equality_is_by_reference() {
        // JS ===: two distinct arrays/objects are never equal, even with
        // identical content.
        let code = vec![PushPtr(0), ArrNew(1), PushPtr(1), ArrNew(1), Eq];
        assert_eq!(run_heap(code, &["abc", "abc"]), vec![b(false)]);
        // But the SAME array (one allocation, duplicated handle) is equal.
        let code = vec![PushPtr(0), ArrNew(1), Dup, Eq];
        assert_eq!(run_heap(code, &["abc"]), vec![b(true)]);
        // Strings remain primitives: distinct heap strings compare by content.
        assert_eq!(
            run_heap(vec![PushPtr(0), PushPtr(1), Eq], &["abc", "abc"]),
            vec![b(true)]
        );
    }

    #[test]
    fn frame_allocates_multiple_locals() {
        // A frame allocates all its locals at once (EnterFrame), yielding
        // independent slots.
        let code = vec![
            Call(2, 0),
            Return(1), // propagate the function's result to the final stack
            EnterFrame(0, false, plain(2).into()),
            PushFloat(7.0),
            SetLocal(0),
            PushFloat(8.0),
            SetLocal(1),
            Local(0),
            Local(1),
            Add,
            Return(1),
        ];
        assert_eq!(run(code), vec![n(15.0)]);
    }

    #[test]
    fn bit_shift_rejects_bad_count() {
        assert!(matches!(
            run_err(vec![PushFloat(1.0), PushFloat(64.0), BitLhs]),
            VMError::ValueError
        ));
        assert!(matches!(
            run_err(vec![PushFloat(1.0), PushFloat(-1.0), BitRhs]),
            VMError::ValueError
        ));
        // Valid shifts still work.
        assert_eq!(run(vec![PushFloat(1.0), PushFloat(3.0), BitLhs]), vec![n(8.0)]);
    }

    #[test]
    fn pop_respects_frame_floor() {
        // fn: 1 arg, 1 local, no temporaries. Pop must not steal a local.
        let code = vec![
            PushFloat(1.0),
            Call(3, 1),
            Return(0),
            EnterFrame(0, false, plain(1).into()), // local 0; sp == frame floor
            Pop(1),                         // nothing above the floor -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn dup_cannot_duplicate_local() {
        let code = vec![
            PushFloat(1.0),
            Call(3, 1),
            Return(0),
            EnterFrame(0, false, plain(1).into()), // local 0; sp == floor
            Dup,                            // nothing above the floor -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn swap_cannot_cross_frame_floor() {
        // One local + one temporary: Swap needs two temporaries above the
        // floor, but only one exists.
        let code = vec![
            PushFloat(1.0),
            Call(3, 1),
            Return(0),
            EnterFrame(0, false, plain(1).into()), // local 0
            PushFloat(9.0),                   // single temporary
            Swap,                           // would swap the temp with the local -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn rot_cannot_cross_frame_floor() {
        let code = vec![
            PushFloat(1.0),
            Call(3, 1),
            Return(0),
            EnterFrame(0, false, plain(1).into()), // local 0
            PushFloat(8.0),                   // two temporaries (need three for Rot)
            PushFloat(9.0),
            Rot,
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn stack_ops_work_within_frame() {
        // Sanity: with enough temporaries above the floor, the ops succeed
        // and leave locals/args untouched.
        let code = vec![
            PushFloat(5.0),
            Call(3, 1),
            Return(1),
            EnterFrame(0, false, plain(1).into()), // local 0
            PushFloat(10.0),
            SetLocal(0),  // local 0 = 10
            PushFloat(1.0), // temporaries: [1, 2]
            PushFloat(2.0),
            Swap,     // -> [2, 1]
            Pop(1),   // -> [2]
            Local(0), // -> [2, 10]
            Add,      // -> [12]
            Return(1),
        ];
        assert_eq!(run(code), vec![n(12.0)]);
    }

    // ── Int transport type ────────────────────────────────────────

    #[test]
    fn negative_integers_are_negint_and_roundtrip() {
        // PosInt and NegInt never compare equal even at the boundary value 0
        // representations (different sign domains).
        assert_eq!(run(vec![PushPosInt(5), PushNegInt(-5), Eq]), vec![b(false)]);
        // Ordering across the sign boundary is structural.
        assert_eq!(run(vec![PushNegInt(-1), PushPosInt(u64::MAX), Lt]), vec![b(true)]);
    }

    #[test]
    fn posint_too_large_for_index_errors() {
        // A PosInt beyond i64::MAX can't be an array index -> error, no panic.
        let code = vec![PushFloat(1.0), ArrNew(1), PushPosInt(u64::MAX), IndexGet];
        assert!(matches!(run_err(code), VMError::TypeError));
    }

    #[test]
    fn int_arithmetic_degrades_to_number() {
        // The transport guarantee is identity-preservation, NOT integer math:
        // any arithmetic promotes Int -> Number(f64).
        assert_eq!(run(vec![PushPosInt(2), PushPosInt(3), Add]), vec![n(5.0)]);
        assert_eq!(run(vec![PushPosInt(10), PushFloat(4.0), Sub]), vec![n(6.0)]);
        assert_eq!(run(vec![PushPosInt(10), PushPosInt(3), Mod]), vec![n(1.0)]);
        assert_eq!(run(vec![PushPosInt(5), Neg]), vec![n(-5.0)]);
    }

    #[test]
    fn int_number_cross_comparison() {
        // 1 == 1.0, ordering works across Int/Number.
        assert_eq!(run(vec![PushPosInt(1), PushFloat(1.0), Eq]), vec![b(true)]);
        assert_eq!(run(vec![PushPosInt(2), PushFloat(2.5), Lt]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(3.0), PushPosInt(3), GtEq]), vec![b(true)]);
        assert_eq!(run(vec![PushPosInt(2), PushPosInt(2), Eq]), vec![b(true)]);
    }

    #[test]
    fn int_indices_and_bitops() {
        // Int works directly as an array index (left-to-right: first = arr[0]).
        let code = vec![
            PushFloat(10.0),
            PushFloat(20.0),
            ArrNew(2), // [10, 20]
            PushPosInt(1),
            IndexGet,
        ];
        assert_eq!(run(code), vec![n(20.0)]);
        // ...and as a bitwise operand.
        assert_eq!(run(vec![PushPosInt(10), PushPosInt(12), BitAnd]), vec![n(8.0)]);
    }

    // ── Phase 0: allocation baseline ────────────────────────────

    /// Run a representative hot-loop workload and record the allocation count.
    /// Each iteration does Math.abs + string concat + array push.
    #[test]
    fn alloc_baseline_hot_loop() {
        use crate::alloc_counter;

        // Build a loop that does builtin calls + string concat (the hot paths).
        // Each iteration: Math.abs, string concat (s += "x").
        let mut code = Vec::new();
        // s = "hello"
        code.push(PushStr("hello".into()));
        // 100 iterations
        for _ in 0..100 {
            // Math.abs(-42) → drop result (just measuring the call overhead)
            code.push(PushFloat(-42.0));
            code.push(CallBuiltin(Builtin::MathAbs, 1));
            code.push(Pop(1));
            // s += "x" (string concat, the main allocator)
            code.push(PushStr("x".into()));
            code.push(Add);
        }
        // drop s
        code.push(Pop(1));

        alloc_counter::reset();
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
        let allocs = alloc_counter::count();
        eprintln!("BASELINE hot_loop_100_iter: {allocs} allocs");
        assert!(allocs > 0, "should have some allocations");
    }
}

use crate::builtin::Builtin;
use crate::rc_str::RcStr;
use thin_vec::ThinVec;

pub type CodeAddr = u32;
pub type StackAddr = u32;
pub type ArrayPtr = u32;
pub type ObjectPtr = u32;
pub type ClosurePtr = u32;
pub type LocalIndex = u16;
pub type LocalCount = u16;
pub type ArgCount = u32;
/// Index into the VM's `cells` side table (the store of captured bindings).
pub type CellIndex = u32;

/// Object keys and string-valued instruction operands. A thin, refcounted,
/// immutable string: cloning a key (`ObjNew`/`ObjSet`) or pushing a literal
/// (`PushStr`) is a refcount bump, and identical interned names share one
/// allocation.
pub type FieldName = RcStr;

/// Storage class for a local slot declared by `EnterFrame`. A `Plain` slot is an
/// ordinary stack local; a `Boxed` slot is captured by reference, so it is
/// backed by a `cells` entry and addressed through an `Upval` marker.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SlotKind {
    Plain,
    Boxed,
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
    PushUndefined,         // () -> undefined
    PushNull,              // () -> null
    PushBool(bool),        // () -> bool
    PushPosInt(u64),       // () -> num
    PushNegInt(i64),       // () -> num
    PushFloat(f64),        // () -> num
    PushStr(RcStr),        // () -> str
    PushArray(ArrayPtr),   // () -> arr
    PushObject(ObjectPtr), // () -> obj
    PushFn(CodeAddr),      // () -> fn
    PushBuiltin(Builtin),  // () -> builtin

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
    Call(CodeAddr, ArgCount), // any, ... -> [any]

    // indirect call: the callable sits on top, above its N args (left-to-right,
    // so arg 0 is deepest). The callable is either a bare `Fn` value or a `Ptr`
    // to a `Value::Closure`; pops it and calls with the same convention as
    // Call. For a closure, its captured environment is installed as the
    // callee's leading locals (slots 0..K) before the body runs. Errors if the
    // top value is neither a Fn nor a closure.
    CallDyn(ArgCount), // any, ..., fn -> [any]

    // static call to a known builtin (the compiler's fast path, analogous to
    // Call for user functions). The N arguments sit on the stack left-to-right
    // (arg 0 deepest; receiver is arg 0 for methods); the builtin pops them and
    // pushes exactly one result. No call frame is created. See builtin.rs.
    CallBuiltin(Builtin, ArgCount), // any, ... -> any

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
    EnterFrame(LocalCount, bool, ThinVec<SlotKind>),

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
    IncLocal(LocalIndex, f64, UpdateMode), // () -> any

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
    Invoke(RcStr, ArgCount), // any, ... -> any

    // EFFECT: raise condition (like Lisp condition system). used to ask LLM in calling frame
    // to decide how to proceed, using restarts like returning a value, aborting,
    // and even rewriting the program preserving already written variables with execution starting at arbitrary point.
    Raise(RcStr, ArgCount), // (payload?) -> result

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
    ArrNew(ArgCount), // [any, ...] -> arr
    ArrLength,        // arr|str -> int

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

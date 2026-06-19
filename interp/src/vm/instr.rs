use crate::builtin::Builtin;
use crate::rc_str::RcStr;
use thin_vec::ThinVec;

pub type CodeAddr = u32;
pub type StackAddr = u32;
pub type ArrayPtr = u32;
pub type ObjectPtr = u32;
pub type ClosurePtr = u32;
/// Index into the VM's `promises` heap. Promises originate only from tool
/// calls (`Instr::Invoke`) — there is no `new Promise` — and are transient
/// values: no JSON form, identity comparison only.
pub type PromisePtr = u32;
/// Index into the VM's `maps` heap.
pub type MapPtr = u32;
/// Index into the VM's `sets` heap.
pub type SetPtr = u32;
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

/// Builtin-type tag for the per-type prototype side table (Step 2a) and
/// constructor identity (Step 2a Part 2). Maps to the JS builtin
/// constructors. The `TypeTag`-keyed prototype side table
/// (`VM::prototypes`, `VM::prototype_for`) is the substrate `instanceof`,
/// `Object.getPrototypeOf`, `.constructor`, and primitive method resolution
/// all walk (Step 2b). Step 2b folded the former structural `instanceof`
/// fast path (`Instr::TypeCheck`) into the proto-chain walk; the enum stays
/// as the side-table key and constructor-identity tag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeTag {
    Array,
    Object,
    Map,
    Set,
    RegExp,
    Function,
    String,
    Number,
    Boolean,
}

impl TypeTag {
    /// Every variant, in discriminant order. The single source of truth for
    /// "what type tags exist" — iterate this instead of hand-listing variants
    /// or mapping side-table indices back to tags by literal number (which
    /// silently breaks if the enum is reordered). `COUNT` is derived from it.
    pub const ALL: [TypeTag; 9] = [
        TypeTag::Array,
        TypeTag::Object,
        TypeTag::Map,
        TypeTag::Set,
        TypeTag::RegExp,
        TypeTag::Function,
        TypeTag::String,
        TypeTag::Number,
        TypeTag::Boolean,
    ];

    /// Number of variants — sizes the prototype side table.
    pub const COUNT: usize = Self::ALL.len();

    /// The JS constructor name for this type (`Array`, `Object`, …). The
    /// `BuiltinKind::Constructor { type_tag }` rows use this to derive their
    /// owning-namespace name, so a constructor's static methods
    /// (`Array.isArray`, `Object.keys`) resolve via `for_namespace(name, …)`
    /// — the same registry lookup the compiler's fast path uses.
    pub fn name(&self) -> &'static str {
        match self {
            TypeTag::Array => "Array",
            TypeTag::Object => "Object",
            TypeTag::Map => "Map",
            TypeTag::Set => "Set",
            TypeTag::RegExp => "RegExp",
            TypeTag::Function => "Function",
            TypeTag::String => "String",
            TypeTag::Number => "Number",
            TypeTag::Boolean => "Boolean",
        }
    }
}

/// A single entry in the per-type builtin prototype side table
/// (`VM::prototypes`, indexed by `TypeTag as usize`). Holds the lazily-allocated
/// frozen prototype object ptr and the mutability seam flag (Step 3).
///
/// ## The `overridden` seam — forward-compatible mutability
///
/// `overridden` defaults to `false`. While `false`, the compiler's `CallBuiltin` is
/// permanently valid — no guard branch is emitted and no runtime check runs. The
/// frozen prototype guarantee makes the compiled method selection an inline cache
/// that never invalidates.
///
/// **Thaw path (specified, not yet implemented):** when a builtin prototype is
/// eventually mutated (descriptor write, `delete`, `setPrototypeOf`),
/// set its `overridden` flag to `true`. The `CallBuiltin` instruction gains a V8-style
/// guard: `if overridden[type] { proto-walk } else { CallBuiltin }` — a single
/// predictable branch, taken only for types the program *actually* patches.
/// Prototypes remain frozen (write → `TypeError`) until the corpus shows a real
/// need, so today `overridden` is read nowhere on the hot path. This struct is the
/// cheap seam that keeps a later thaw a localized change rather than a redesign.
#[derive(Clone, Debug, Default)]
pub struct TypePrototype {
    /// The lazily-allocated frozen prototype object, or `None` if never
    /// reflectively touched.
    pub ptr: Option<ObjectPtr>,
    /// Forward-compatibility flag for monkeypatchable builtins (Step 3).
    /// `false` means the frozen `CallBuiltin` IC is permanently valid.
    pub overridden: bool,
}

/// A global builtin value identifier for `Instr::PushGlobal` (Step 2a Part 2).
/// Covers the non-callable namespace objects (`Math`, `JSON`) — frozen plain
/// `Value::Object`s the VM lazily allocates from a side table. Constructor
/// values (`Array`, `Map`, …) are `Value::Builtin` and pushed via the existing
/// `Instr::PushBuiltin`; they don't need an entry here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GlobalId {
    Math,
    JSON,
}

/// Instructions for a stack based language used for LLM composition of complex tool flows.
#[derive(Clone, Debug, PartialEq)]
pub enum Instr {
    PushUndefined,                     // () -> undefined
    PushNull,                          // () -> null
    PushBool(bool),                    // () -> bool
    PushPosInt(u64),                   // () -> num
    PushNegInt(i64),                   // () -> num
    PushFloat(f64),                    // () -> num
    PushStr(RcStr),                    // () -> str
    PushArray(ArrayPtr),               // () -> arr
    PushObject(ObjectPtr),             // () -> obj
    PushFn(CodeAddr, ClosurePtr, u16), // () -> fn ; const-fn canonical push (u16 = JS arity for fn.length)
    PushBuiltin(Builtin),              // () -> builtin
    /// Push a global namespace object (`Math`, `JSON`) — a frozen, non-callable
    /// `Value::Object` lazily allocated by the VM. The reflective/value path for
    /// the bare identifier (`let m = Math`) and indirect member reads
    /// (`globalThis.Math.PI`); the compiler's fast path (`Math.max(…)`) still
    /// lowers to `CallBuiltin` directly. () -> object
    PushGlobal(GlobalId),

    /// Push a globally-scoped name resolved at runtime — the reflective fallback
    /// for bare identifiers the compiler does not statically recognize
    /// (`TypeError`, `Symbol`, `Date`, …). The string is already interned and
    /// cheap to compare. The VM checks the builtin registry, error-constructor
    /// set, and hardcoded globals; everything else resolves to a `ReferenceError`.
    /// () -> any
    PushName(RcStr),

    Pop(usize),

    /// Generalized stack reach (Forth-like), counting from the top (0 = top).
    /// Rejects reaching below the current frame's temporaries (frame_floor).
    /// Pick is the read-modify-write workhorse (duplicate an lvalue's object/key
    /// for a load-then-store); Dig reorders without copying. These subsume the
    /// former Dup/Swap/Rot (Pick(0) ≈ Dup, Dig(1) ≈ Swap, Dig(2) ≈ Rot).
    //
    /// Pick(n): copy the n-th-from-top value to the top.
    Pick(usize), // any^(n+1) -> any^(n+1), any
    /// Dig(n): move the n-th-from-top value to the top, removing it from its
    /// old position. Dig(0) is a no-op. Includes fast paths for n ≤ 2.
    Dig(usize), // any^(n+1) -> any^(n+1)

    /// Drop `n` values directly below the top, leaving the top in place.
    /// Nip(1) ≡ Dig(1); Pop; Nip(n) is the symmetric inverse of Dig(n).
    Nip(usize), // any^(n+1) -> any

    /// calls function starting at address. the N arguments are passed on the
    /// stack in left-to-right order (arg 0 pushed first / deepest), and become
    /// the new frame's args. depends on function whether or not a result is left
    /// on stack after it returns.
    Call(CodeAddr, ArgCount), // any, ... -> [any]

    /// indirect call: the callable sits *below* its N args (left-to-right,
    /// arg 0 deepest, callable at depth N). The callable is either a bare `Fn`
    /// value or a `Ptr` to a `Value::Closure`; pops it and calls with the same
    /// convention as Call. For a closure, its captured environment is installed
    /// as the callee's leading locals (slots 0..K) before the body runs. Errors
    /// if the callable is neither a Fn nor a closure. When `has_this` is true,
    /// a receiver sits one deeper (depth N+1) and is threaded as `this`.
    CallDyn(ArgCount, bool), // any, ..., fn -> [any]

    /// static call to a known builtin (the compiler's fast path, analogous to
    /// Call for user functions). The N arguments sit on the stack left-to-right
    /// (arg 0 deepest; receiver is arg 0 for methods); the builtin pops them and
    /// pushes exactly one result. No call frame is created. See builtin.rs.
    CallBuiltin(Builtin, ArgCount), // any, ... -> any

    /// Dynamic call with spread arguments. The callable sits *below* the args
    /// array; pops both, expands the array elements over the callable's old
    /// position, then dispatches exactly like `CallDyn`. When `has_this` is
    /// true, a receiver sits one deeper below the callable and is threaded
    /// as `this`. Covers user functions, closures, and builtins uniformly.
    CallSpread(bool), // args_arr, callable -> result

    /// `new F(args)`: the callable (a Closure) sits *below* its N args. Allocates
    /// a fresh object `O` with `O.proto = F.prototype` (creating the prototype
    /// lazily on first access), stores `O` on the *caller* frame's `new_obj`, and
    /// dispatches the callee with `this_val = O`. The post-return fixup happens
    /// in `NewReturn` which must be the next instruction.
    /// Stack: callee, args… -> (next instr: NewReturn)
    New(ArgCount),
    /// Post-return fixup for `New`: peeks at the top-of-stack return value. If it
    /// is an `Object`, keep it (the constructor returned an object explicitly).
    /// Otherwise consume the return value and push the allocated instance object
    /// stored in the caller frame's `new_obj`. Always clears `new_obj`.
    /// Stack: return_val -> result
    NewReturn,

    /// return from in-program function Call, returning the top N values (in
    /// push order, so the first-pushed return value stays first).
    Return(usize),

    /// Prologue frame setup, emitted as the first instruction of every function/
    /// arrow body. Arguments arrive in place as the leading locals (the caller
    /// pushed them; `fp` points at arg 0), so there is no per-argument copy.
    /// `EnterFrame(nparams, build_args, local_kinds)`:
    ///   - if `build_args`, eagerly materialize the `arguments` array from the
    ///     actual args (before they are normalized) and cache it in the frame;
    ///   - normalize the arg region to exactly `nparams` slots (drop surplus args
    ///     / pad missing params with Undefined);
    ///   - install the closure's captured environment (stashed by `CallDyn`) as
    ///     the upval locals at slots [nparams, nparams + K);
    ///   - allocate the declared (non-param) own locals from `local_kinds`, the
    ///     Undefined for Plain, a fresh cell + Upval for Boxed, so the final
    ///     layout is [params | upvals | locals]. The self-reference slot
    ///     (named/recursive functions) is the last kind.
    ///     This is the sole frame-setup instruction: there is no separate per-arg
    ///     copy or local-allocation step.
    EnterFrame(LocalCount, bool, ThinVec<SlotKind>),

    /// Push the `arguments` array for the current frame: a fresh heap array of
    /// all `arg_count` arguments (arg 0 first). Built lazily and cached per
    /// frame (`CallFrame::arguments_cache`), so repeated references reuse the
    /// same array rather than re-materializing it. Lowers the `arguments`
    /// identifier. () -> arr
    Arguments,

    /// load the local variable at the local index of the current stack frame, and push it onto the stack
    GetLocal(LocalIndex),

    /// pops the topmost value from the stack and writes to the local at the given index
    SetLocal(LocalIndex), // any -> ()

    /// Stores the top of stack to a local without popping (like WASM's
    /// `local.tee`): the value stays on the stack AND is written to the local
    /// slot. Replaces the common `Pick(0); SetLocal` (formerly `Dup; SetLocal`).
    TeeLocal(LocalIndex), // any -> any

    /// Re-box a (Boxed) local: allocate a fresh `cells` entry seeded with the
    /// slot's current value and store a new `Upval` marker into the slot. Used to
    /// give each loop iteration its own captured cell, so closures created in
    /// different iterations capture distinct bindings (JS `let`/`const`
    /// per-iteration semantics) even though the stack slot is reused. Seeding the
    /// new cell with the current value carries a for-head variable forward to the
    /// next iteration; for a fresh declaration the following `SetLocal` overwrites
    /// it. () -> ()
    FreshCell(LocalIndex),

    /// Increments or decrements a local variable in place. `p` is the value to
    /// *subtract* from the variable: NegInt(-1) increments (sub −1 = +1),
    /// PosInt(1) decrements (sub 1 = −1). Prefix mode leaves the new value on
    /// the stack; Postfix leaves the old value. Only emitted for `++`/`--` on
    /// local variables; member/index targets fall back to load-sub-store.
    IncLocal(LocalIndex, f64, UpdateMode), // () -> any

    /// Push the `this` binding from the current call frame. This is the only
    /// way to read `this`; emitted only in functions that lexically reference
    /// it. () -> any
    LoadThis,

    /// temporary block markers. initially Jump and JFalse Addr refer to specific Label Addr(id),
    /// but will get rewritten as code offset in a pass which eliminates Label instructions
    Label(CodeAddr), // () -> ()

    /// unconditional jump to address
    Jump(CodeAddr), // () -> ()

    /// pops the topmost value from the stack. jumps to the address if false
    JFalse(CodeAddr), // () -> ()

    /// pops the topmost value from the stack. jumps to the address if truthy.
    /// The truthy-mirror of JFalse, so `||` lowers without an extra Jump.
    JTrue(CodeAddr), // () -> ()

    /// Jumps to the address when the topmost value is neither null nor
    /// undefined, leaving the value in place; on fall-through (nullish) the
    /// value is POPPED. Asymmetric on purpose: every emitter keeps the value
    /// when proceeding with it and discards it when short-circuiting, so the
    /// pop is folded in. The "not nullish" jump that lowers `??`, optional
    /// chaining (`?.`), and optional calls in one instruction.
    JNotNullish(CodeAddr), // taken: any -> any; fall-through: any -> ()

    /// EFFECT: starts the named tool call. Pops N arguments (push order: arg 0
    /// deepest), allocates a Pending entry in the `promises` heap, records the
    /// call in the VM-side outbox, and pushes the promise — WITHOUT yielding
    /// to the host. The host sees the accumulated outbox only when the program
    /// awaits a still-pending promise (`Await` → `StepResult::Pending`), so
    /// fan-out composes across arbitrary control flow, not just adjacent
    /// instructions.
    Invoke(RcStr, ArgCount), // any, ... -> promise

    /// Await the top of stack. A non-promise passes through unchanged (JS
    /// `await x` on a plain value). A Resolved promise is replaced by its
    /// value (a promise resolved with a promise is adopted: the Await
    /// re-executes on the innermost one; a cycle is the JS "chaining cycle"
    /// TypeError). A Rejected one consumes the promise and unwinds to a
    /// reachable `try` handler, rejects the enclosing strand's promise
    /// (inside a resumed continuation), or escalates the rejection value as
    /// a resumable error (Phase 3 path — the host may substitute a value).
    /// A Pending promise depends on where the Await sits (7_ASYNC Tier 2):
    ///  - below top level it is inside an async function's own frame (the
    ///    parser confines `await` there) — that one frame is suspended into
    ///    a continuation record (zero stack left behind) and registered as
    ///    a waiter; a first suspension pushes a fresh promise to the caller
    ///    as the call's return value, a re-suspension falls through to the
    ///    scheduler;
    ///  - at top level the root strand parks in place: ready continuations
    ///    run above the parked region, and with nothing ready it yields
    ///    `StepResult::Pending` carrying the drained outbox with ip
    ///    UNCHANGED (the blocking `Await` re-executes on the next `step`) —
    ///    or fails with the dedicated `Deadlock` error when nothing is in
    ///    flight either.
    Await, // promise|any -> any

    /// EFFECT: raise condition (like Lisp condition system). used to ask LLM in calling frame
    /// to decide how to proceed, using restarts like returning a value, aborting,
    /// and even rewriting the program preserving already written variables with execution starting at arbitrary point.
    /// NOT catchable by `try`: conditions are addressed to the LLM, and a
    /// program must not be able to swallow them (6_LANGUAGE Part B).
    Raise(RcStr, ArgCount), // (payload?) -> result

    /// Enter a `try` block: push an entry onto the VM's handler stack,
    /// snapshotting the current stack height, call depth, and frame pointer.
    /// The operand is the catch handler's address (a label id until
    /// backpatch). A throw unwinds to the innermost handler: the entry is
    /// popped, `stack`/`callstack` are truncated to the snapshot, `fp` (and
    /// the local-count mirror) restored, the thrown value pushed, and control
    /// jumps to the handler. Paired with `TryExit` on the normal path; the
    /// compiler emits the matching `TryExit`s when `break`/`continue`/
    /// `return` jump out of the block. Optimizer: a barrier (in no `pe_*`
    /// allow-list); the CFG pass treats the handler address as a reachable
    /// branch target.
    TryEnter(CodeAddr), // () -> ()

    /// Leave a `try` block on the normal (no-throw) path: pop the innermost
    /// handler entry. An empty handler stack is a compiler bug (BadArg).
    TryExit, // () -> ()

    /// `throw expr`: pop the thrown value and unwind to the innermost handler
    /// (see `TryEnter`). With no active handler the throw escalates as an
    /// `UncaughtException` at the `step()` boundary, with the thrown value
    /// preserved in `VMError::payload` — NotResumable, because a `throw` has
    /// no result slot a substituted value could fill.
    Throw, // any -> ()

    /// build a closure over the listed local slots of the current frame and push
    /// a Closure to the resulting value. Each captured slot is copied
    /// verbatim: a Boxed slot yields its Upval handle (shared, by-reference), a
    /// Plain slot yields its current value (a by-value snapshot — which the
    /// compiler only emits when the binding is provably never reassigned). The
    /// captures are listed in the order the target body expects its upvals.
    ClosureNew(CodeAddr, u16, ThinVec<LocalIndex>), // () -> fn (u16 = JS arity for fn.length)

    /// pops N values where N is the number of field names, then pushes an
    /// object with each field set to its corresponding value. Left-to-right:
    /// field 0's value is the first/deepest pushed.
    ObjNew(ThinVec<FieldName>), // [any, ...] -> obj
    ObjGet(FieldName), // obj -> any
    /// Read property `name` off the receiver but **keep the receiver** below the
    /// result. Semantically identical to `ObjGet` (same own→proto walk, same
    /// `Undefined` on a miss), sharing `ObjGet`'s resolution helper. Used by
    /// method calls (`recv.m(args)`) so the receiver remains available for
    /// has_this binding. Also emitted for compound-assignment loads
    /// (`obj.x += 1`). obj -> obj, any
    ObjPeek(FieldName),
    /// `ObjGetDyn` minus the obj-pop: keep the receiver below the resolved
    /// property. obj, key -> obj, value
    ObjPeekDyn,
    /// Method-aware property read (Step 6): reads `recv.name` as a value,
    /// dispatching on the receiver type. For an `Object`, behaves exactly like
    /// `ObjGet` (own→proto property read — so an own data property shadows any
    /// builtin name). For an array/string/map/set/regexp/closure/builtin/bound
    /// receiver, yields the plain `Value::Builtin` for a method name valid for
    /// that type (`[].push` → the array-push builtin), or `Undefined` when the
    /// name is not valid for the type (JS-faithful: the call errors, not the
    /// read). `null`/`undefined` receivers are a `TypeError`. Only emitted when
    /// `Builtin::for_method(name)` is `Some`, so `obj.has` (a data property
    /// named `has`) is not regressed — the `Object` branch reads the property.
    /// recv -> any
    GetMethodOrProp(FieldName),
    /// Sets the field and leaves a value on the stack (assignment is an
    /// expression). In `New` mode leaves the assigned value; in `Old` mode
    /// reads and leaves the previous value. Statement-context callers follow
    /// `New` mode with `Pop(1)`.
    ObjSet(FieldName, SetMode), // obj, any -> any
    /// Copy all fields of `src` into `obj` (insertion order, later wins).
    /// null/undefined src is a no-op. Non-object src is a TypeError
    /// (documented divergence from JS, which would copy index keys from
    /// arrays/strings).
    ObjExtend, // obj, src -> obj
    /// `key in obj`
    ObjHas, // obj, str -> bool
    /// `delete obj[key]`
    ObjDelete, // obj, str -> bool

    /// Runtime-polymorphic computed access `x[k]` / `x[k] = v`. A variable-keyed
    /// index has no static type to choose array/object/string access, so these
    /// inspect the container at runtime: array+int -> element (OOB read ->
    /// undefined, OOB write -> error, negative -> error); object -> string-key
    /// property (ToString the key; missing -> undefined); string+int -> the
    /// character at that UTF-8 byte offset as a 1-char string (OOB -> undefined,
    /// mid-codepoint -> error). They supersede the old type-specific
    /// ArrGet/ArrSet/ObjGetDyn/ObjSetDyn. The static-name ObjGet/ObjSet remain
    /// the fast path for `obj.foo`/`state.foo` (no per-access heap-string alloc).
    IndexGet, // container, key -> any
    /// Like `ObjSet` with `SetMode`: `New` leaves the assigned value, `Old`
    /// reads and leaves the previous value. Statement callers `Pop` the `New`
    /// result.
    IndexSet(SetMode), // container, key, value -> any

    /// pops N values and pushes an array with them as initial values.
    /// Left-to-right: the first/deepest pushed becomes element 0.
    ArrNew(ArgCount), // [any, ...] -> arr
    /// Push a single value to the end of an array. Pops the value, pops
    /// the array, pushes the array back.
    ArrPush, // arr, val -> arr
    /// Append all elements of `src` to `arr`. `src` must be an array;
    /// string/iterable srcs are a TypeError (documented divergence).
    ArrExtend, // arr, src -> arr

    /// `.length` for str or array, or `length` property for objects
    GetLength, // str|arr -> num
    /// `.size` for Map or Set, or `size` property for objects
    GetSize, // map|set -> num

    /// `x instanceof F` — walks `x`'s prototype chain looking for
    /// `F.prototype`. LHS (value) then RHS (callable) on stack; pops both,
    /// pushes bool. RHS must be a Closure, Bound, or constructor Builtin;
    /// else TypeError. Step 2b: the structural `TypeTag` fast path
    /// (`Instr::TypeCheck`) is folded into this walk — the RHS is always
    /// evaluated to a real constructor value, so `[] instanceof Array`
    /// resolves `Array` to `Value::Builtin(ArrayCtor)` and walks the
    /// chain here, with no special-case instruction.
    /// Stack: value, callable -> bool
    InstanceOf,

    /// JS `String(x)` / ToString: pops any value, pushes its string form. Unlike
    /// StrFromJson (which emits JSON, and rejects non-JSON values), this matches
    /// template-literal / string-coercion semantics: numbers print without a
    /// trailing ".0", arrays join with "," (null/undefined holes → ""), plain
    /// objects → "[object Object]", null/undefined → "null"/"undefined".
    ToStr, // any -> str
    /// JS `Number(x)` / ToNumber: pops any value, pushes its numeric form
    /// (null→0, undefined→NaN, bool→0/1, strings parse, unparseable→NaN). The
    /// coercion target for unary `+x`, mirroring the arithmetic operators'
    /// implicit ToNumber. An array/object/function is a TypeError (no ToPrimitive).
    ToNum, // any -> num
    /// JS `Boolean(x)` / ToBoolean: pops any value, pushes its truthiness as a
    /// bool. The coercion target for `Boolean(x)` and `!!x`.
    ToBool, // any -> bool

    // type predicates
    IsNull,  // any -> bool
    IsBool,  // any -> bool
    IsFloat, // any -> bool
    IsNum,   // any -> bool
    IsStr,   // any -> bool
    IsObj,   // any -> bool
    IsMap,   // any -> bool
    IsSet,   // any -> bool

    /// JS `typeof`: pops a value and pushes its type tag as a string. Tags match
    /// JS exactly, so they are coarse: "undefined", "object" (covers Null, arrays
    /// AND plain objects), "boolean", "number" (int or float), "string",
    /// "function" (Fn or Closure). The fine-grained Is* predicates below stay for
    /// the distinctions typeof erases (array-vs-object, int-vs-float, null) — they
    /// are the lowering targets for Array.isArray, Number.isInteger, x === null.
    TypeOf, // any -> str

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
    BitURhs,  // int, int -> int (unsigned/zero-fill)
    Pow,      // num, num -> num
}

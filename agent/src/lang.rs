use indexmap::IndexMap;
use std::collections::HashMap;

pub type VarName = String;
pub type FieldName = String;
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
}

#[derive(Clone, Debug, PartialEq)]
pub enum HeapValue {
    String(String),
    Array(Vec<StackValue>),
    Object(IndexMap<String, StackValue>),
    /// A closure: a code address plus its captured environment. Each upval is
    /// either a plain value (an immutable / by-value capture) or an `Upval`
    /// handle (a shared, mutable by-reference capture). Built by `MakeClosure`,
    /// called via `CallDyn`, which installs `upvals` as the callee's leading
    /// locals. Like `Fn`, it has no JSON form and compares by identity.
    Closure {
        addr: CodeAddr,
        upvals: Vec<StackValue>,
    },
}

/// Storage class for a local slot declared by `Alloc`. A `Plain` slot is an
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
}

/*

Stack layout:
    higher addresses
    ┌──────────────────────┐
    │  expr temporaries    │  ← sp
    ├──────────────────────┤
    │  local M-1           │  fp + M-1
    │  ...                 │
    │  local 1             │  fp + 1
    │  local 0             │  fp
    ├──────────────────────┤
    │  arg_N-1             │  fp - 1
    │  ...                 │
    │  arg_1               │  fp - (N-1)
    │  arg_0               │  fp - N
    ├──────────────────────┤
    │  caller's temps      │
    └──────────────────────┘
    lower addresses

Args follow the uniform left-to-right convention: the caller pushes them
left-to-right, so arg_0 (the first argument) is pushed first and sits deepest
(fp - N), and arg_N-1 is on top (fp - 1).


Closures — the compiler contract
================================

The VM gives you capture-by-reference (JS `let`/`var` semantics) via three
moving parts: `Boxed` local slots, the `cells` side table, and `MakeClosure` /
`CallDyn`. The runtime stays dumb; the analysis and slot bookkeeping below are
the compiler's job. A future codegen MUST uphold all of this:

1. Capture analysis (who gets boxed).
   A variable that is captured by any nested function AND is ever reassigned
   (by its owner or any closure) must be `Boxed` in its OWNING frame's `Alloc`.
   Everything else stays `Plain`. A captured-but-never-reassigned variable may
   stay `Plain` and be captured by value — see point 4.

2. Boxing is per-binding and eager.
   `Alloc(Vec<SlotKind>)` declares each new slot's storage class. A `Boxed`
   slot is backed by a fresh `cells` entry from birth; `Local`/`SetLocal`
   transparently route through it. There is no "open upvalue" / close step —
   the cell already has identity and outlives the frame, so a returned closure
   keeps working after its defining frame is gone. (Cost: one indirection per
   access and a permanent cell. Acceptable under this VM's no-GC, short-program
   design.)

3. Closure-frame slot layout (the ABI).
   `CallDyn` installs a closure's captured environment as the callee's LEADING
   locals: captured upvals occupy slots 0..K (in `MakeClosure`'s capture
   order), and `local_count` is preset to K. Therefore, when compiling a
   function that will be reached as a closure, lay out:
       slot 0 .. K-1  = captured upvals  (DO NOT re-`Alloc` these)
       slot K, K+1 .. = the body's own locals (its first `Alloc` starts here)
   and emit `MakeClosure(addr, captures)` at the definition site with `captures`
   ordered to match exactly the slot order the body expects.

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
   `Arg` reads plain values below `fp` and there is no `SetArg`. To capture (or
   reassign) a parameter, the prologue must copy it into a `Boxed` local
   (`Arg(i)` then `SetLocal(boxed_slot)`); capture that local, not the arg.

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
    pub variables: HashMap<VarName, StackValue>,
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

// Instructions for a stack based language used for LLM composition of complex tool flows.
pub enum Instr {
    Push(StackValue), // () -> any

    // Grow the current frame's locals region by one slot per kind. A `Plain`
    // slot is initialized to Null (an ordinary local); a `Boxed` slot allocates
    // a fresh cell (init Null) in the `cells` side table and stores an `Upval`
    // marker, so that binding is captured by reference — every Local/SetLocal
    // routes through the shared cell. Successive Allocs each append more slots,
    // but only when no expression temporaries sit above the locals.
    Alloc(Vec<SlotKind>),
    Pop(usize),
    Dup,
    Swap, // any, any -> any, any
    Rot,  // any, any, any -> any, any, any

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

    // build a closure over the listed local slots of the current frame and push
    // a Ptr to the resulting HeapValue::Closure. Each captured slot is copied
    // verbatim: a Boxed slot yields its Upval handle (shared, by-reference), a
    // Plain slot yields its current value (a by-value snapshot — which the
    // compiler only emits when the binding is provably never reassigned). The
    // captures are listed in the order the target body expects its upvals.
    MakeClosure(CodeAddr, Vec<LocalIndex>), // () -> fn

    // return from in-program function Call, returning the top N values (in
    // push order, so the first-pushed return value stays first).
    Return(usize),

    // load the argument at the argument index of the current stack frame, and
    // push it onto the stack. Arg(0) is the first argument (see Call).
    Arg(ArgIndex),

    // load the local variable at the local index of the current stack frame, and push it onto the stack
    Local(LocalIndex),

    // pops the topmost value from the stack and writes to the local at the given index
    SetLocal(LocalIndex), // any -> ()

    // type predicates
    IsNull,  // any -> bool
    IsBool,  // any -> bool
    IsInt,   // any -> bool
    IsFloat, // any -> bool
    IsNum,   // any -> bool
    IsStr,   // any -> bool
    IsArr,   // any -> bool
    IsObj,   // any -> bool

    // reads variable and pushes the contained value to the stack
    Read(VarName), // () -> any

    // pops the topmost value from the stack and writes to the variable
    Write(VarName), // any -> ()

    // temporary block markers. initially Jump and JFalse Addr refer to specific Label Addr(id),
    // but will get rewritten as code offset in a pass which eliminates Label instructions
    Label(CodeAddr), // () -> ()

    // unconditional jump to address
    Jump(CodeAddr), // () -> ()

    // pops the topmost value from the stack. jumps to the address if false
    JFalse(CodeAddr), // () -> ()

    // EFFECT: invokes the named tool or function.
    // pops N arguments off the stack; args are taken in push order, so with
    // left-to-right codegen arg 0 is the deepest of the group (the first one
    // pushed). step() batches a run of consecutive Invoke instructions into one
    // StepResult::Invoke (fan-out); the host runs them concurrently and pushes
    // one result per call, in call order.
    Invoke(String, u32), // any, ... -> any

    // EFFECT: raise condition (like Lisp condition system). used to ask LLM in calling frame
    // to decide how to proceed, using restarts like returning a value, aborting,
    // and even rewriting the program preserving already written variables with execution starting at arbitrary point.
    Raise(String), // () -> any

    // pops N values where N is the number of field names, then pushes an
    // object with each field set to its corresponding value. Left-to-right:
    // field 0's value is the first/deepest pushed.
    ObjNew(Vec<FieldName>), // [any, ...] -> obj
    ObjGetDyn,              // obj, str -> any
    ObjSetDyn,              // obj, str, any -> ()
    ObjGet(FieldName),      // obj -> any
    ObjSet(FieldName),      // obj, any -> ()

    // pops N values and pushes an array with them as initial values.
    // Left-to-right: the first/deepest pushed becomes element 0.
    ArrNew(u32), // [any, ...] -> arr
    ArrLength,   // arr|str -> int
    ArrGet,      // arr, int -> any
    ArrSet,      // arr, int, any -> ()
    ArrPush,     // arr, any -> ()    (append to end)
    ArrPop,      // arr -> any        (remove & return end)
    ArrShift,    // arr -> any        (remove & return front, like JS)
    ArrUnshift,  // arr, any -> ()    (prepend to front, like JS)
    ArrJoin,     // arr, str -> str

    StrSplit(ArgCount),       // str, str[, int] -> arr(str)
    StrIncludes(ArgCount),    // str, str[, int] -> bool
    StrStartsWith,            // str, str -> bool
    StrEndsWith,              // str, str -> bool
    StrIndexOf(ArgCount),     // str, str[, int] -> int
    StrLastIndexOf(ArgCount), // str, str[, int] -> int
    StrSlice,                 // str, int, int -> str
    StrTrim,                  // str -> str
    StrToInt,                 // str -> int
    StrToFloat,               // str -> float
    StrToJson,                // str -> any
    StrFromJson,              // any -> str

    // unary operators. pops the topmost value from the stack,
    // operates on it and then pushed the result to the stack
    Abs,    // num -> num
    Neg,    // num -> num
    Sqrt,   // num -> num
    Ceil,   // num -> int
    Floor,  // num -> int
    Round,  // num -> int
    Sign,   // num -> int
    Not,    // any -> bool
    BitNot, // int -> int

    // binary operators. first pops rhs then lhs off the stack,
    // then operates on them pushing result to stack
    Add,    // num|str, num|str -> num|str
    Sub,    // num, num -> num
    Mul,    // num, num -> num
    Div,    // num, num -> num
    Mod,    // int, int -> int
    Eq,     // any, any -> bool
    Neq,    // any, any -> bool
    Lt,     // any, any -> bool
    Gt,     // any, any -> bool
    LtEq,   // any, any -> bool
    GtEq,   // any, any -> bool
    And,    // any, any -> any
    Or,     // any, any -> any
    BitAnd, // int, int -> int
    BitOr,  // int, int -> int
    BitXor, // int, int -> int
    BitLhs, // int, int -> int
    BitRhs, // int, int -> int
    Min,    // num, num -> num
    Max,    // num, num -> num
    Pow,    // num, num -> num
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

fn is_truthy(val: &StackValue) -> bool {
    !matches!(val, StackValue::Bool(false) | StackValue::Null)
}

fn float_is_int(n: f64) -> bool {
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
fn as_i64(val: &StackValue) -> Option<i64> {
    match val {
        StackValue::NegInt(i) => Some(*i),
        StackValue::PosInt(u) => i64::try_from(*u).ok(),
        StackValue::Number(n) if float_is_int(*n) => Some(*n as i64),
        _ => None,
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
            variables: HashMap::new(),
            // Root frame so that Arg/Local/Alloc are valid from the start.
            callstack: vec![CallFrame {
                arg_count: 0,
                local_count: 0,
                return_addr: 0,
                prev_fp: 0,
            }],
            ip: 0,
            fp: 0,
            fuel: DEFAULT_FUEL,
        }
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
            Some(Instr::Invoke(name, nargs)) => Some((name.clone(), *nargs)),
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
            Some(HeapValue::String(s)) => Some(s),
            _ => None,
        }
    }

    fn heap_arr(&self, ptr: HeapAddr) -> Option<&Vec<StackValue>> {
        match self.heap.get(ptr as usize) {
            Some(HeapValue::Array(a)) => Some(a),
            _ => None,
        }
    }

    fn heap_arr_mut(&mut self, ptr: HeapAddr) -> Option<&mut Vec<StackValue>> {
        match self.heap.get_mut(ptr as usize) {
            Some(HeapValue::Array(a)) => Some(a),
            _ => None,
        }
    }

    fn heap_obj(&self, ptr: HeapAddr) -> Option<&IndexMap<String, StackValue>> {
        match self.heap.get(ptr as usize) {
            Some(HeapValue::Object(o)) => Some(o),
            _ => None,
        }
    }

    fn heap_obj_mut(&mut self, ptr: HeapAddr) -> Option<&mut IndexMap<String, StackValue>> {
        match self.heap.get_mut(ptr as usize) {
            Some(HeapValue::Object(o)) => Some(o),
            _ => None,
        }
    }

    fn alloc_string(&mut self, s: String) -> StackValue {
        let addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::String(s));
        StackValue::Ptr(addr)
    }

    fn alloc_array(&mut self, arr: Vec<StackValue>) -> StackValue {
        let addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::Array(arr));
        StackValue::Ptr(addr)
    }

    fn alloc_object(&mut self, obj: IndexMap<String, StackValue>) -> StackValue {
        let addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::Object(obj));
        StackValue::Ptr(addr)
    }

    fn alloc_closure(&mut self, addr: CodeAddr, upvals: Vec<StackValue>) -> StackValue {
        let heap_addr = self.heap.len() as HeapAddr;
        self.heap.push(HeapValue::Closure { addr, upvals });
        StackValue::Ptr(heap_addr)
    }

    /// Structural comparison of two stack values, recursing through the heap
    /// so that equality is by *content* at every level. (The derived
    /// `PartialEq` on `HeapValue` would compare nested `Ptr`s by address, so
    /// `["a"] == ["a"]` with distinct inner strings would wrongly be false.)
    fn values_equal(&self, lhs: &StackValue, rhs: &StackValue) -> bool {
        match (lhs, rhs) {
            (StackValue::Null, StackValue::Null) => true,
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
            (StackValue::Ptr(p), StackValue::Ptr(q)) => {
                match (self.heap.get(*p as usize), self.heap.get(*q as usize)) {
                    // Same live heap object is always equal (this also gives
                    // closures reference identity, as they have no content
                    // equality of their own).
                    (Some(a), Some(b)) => p == q || self.heap_values_equal(a, b),
                    _ => false, // dangling pointer: treat as not-equal rather than panic
                }
            }
            _ => false,
        }
    }

    fn heap_values_equal(&self, a: &HeapValue, b: &HeapValue) -> bool {
        match (a, b) {
            (HeapValue::String(x), HeapValue::String(y)) => x == y,
            (HeapValue::Array(x), HeapValue::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y.iter()).all(|(u, v)| self.values_equal(u, v))
            }
            (HeapValue::Object(x), HeapValue::Object(y)) => {
                // Order-independent: same keys, recursively equal values.
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, v)| y.get(k).is_some_and(|w| self.values_equal(v, w)))
            }
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
    fn pop_ptr(&mut self) -> Result<HeapAddr, VMError> {
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
    fn pop_string(&mut self) -> Result<String, VMError> {
        let ptr = self.pop_ptr()?;
        match self.heap_str(ptr) {
            Some(s) => Ok(s.to_string()),
            None => Err(VMError::TypeError),
        }
    }

    // ── JSON conversion helpers ──────────────────────────────────────

    fn stack_value_to_json(
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
            StackValue::Fn(_) | StackValue::Upval(_) => return Err(VMError::ValueError),
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
                HeapValue::String(s) => serde_json::Value::String(s.clone()),
                HeapValue::Array(arr) => serde_json::Value::Array(
                    arr.iter()
                        .map(|v| self.stack_value_to_json(v, depth + 1))
                        .collect::<Result<_, _>>()?,
                ),
                HeapValue::Object(obj) => {
                    let mut map = serde_json::Map::new();
                    for (k, v) in obj {
                        map.insert(k.clone(), self.stack_value_to_json(v, depth + 1)?);
                    }
                    serde_json::Value::Object(map)
                }
                // A closure has no JSON representation (see Fn above).
                HeapValue::Closure { .. } => return Err(VMError::ValueError),
            },
        })
    }

    fn json_to_stack_value(
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
            serde_json::Value::String(s) => self.alloc_string(s.clone()),
            serde_json::Value::Array(arr) => {
                let vals: Vec<StackValue> = arr
                    .iter()
                    .map(|v| self.json_to_stack_value(v, depth + 1))
                    .collect::<Result<_, _>>()?;
                self.alloc_array(vals)
            }
            serde_json::Value::Object(obj) => {
                let mut map = IndexMap::new();
                for (k, v) in obj {
                    map.insert(k.clone(), self.json_to_stack_value(v, depth + 1)?);
                }
                self.alloc_object(map)
            }
        })
    }

    // ── step ─────────────────────────────────────────────────────────

    pub fn step(&mut self) -> Result<StepResult, VMError> {
        // ── macros for repetitive instruction shapes ─────────────────

        /// Pop one Number, apply f64→f64, push Number.
        macro_rules! unary_num {
            ($op:expr) => {{
                let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                match as_f64(&val) {
                    Some(n) => {
                        self.stack.push(StackValue::Number($op(n)));
                        self.ip += 1;
                    }
                    None => return Err(VMError::TypeError),
                }
            }};
        }

        /// Pop rhs then lhs (numeric: Number or Int), apply f64→f64→f64, push
        /// Number. `Int` operands promote to f64 (arithmetic degrades Int).
        macro_rules! binary_num {
            ($op:expr) => {{
                let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                match (as_f64(&lhs), as_f64(&rhs)) {
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
                Instr::Push(val) => {
                    self.stack.push(*val);
                    self.ip += 1;
                }

                Instr::Alloc(kinds) => {
                    // Locals occupy [fp, fp + local_count). Allocation is only
                    // valid when no expression temporaries sit above them, i.e.
                    // sp == fp + local_count. This permits multiple successive
                    // Alloc instructions (each grows the locals region) while
                    // still rejecting an Alloc issued mid-expression.
                    let kinds = kinds.clone(); // release the borrow on self.code
                    let frame = self.callstack.last().ok_or(VMError::BadAlloc)?;
                    let locals_top = self.fp as usize + frame.local_count as usize;
                    if self.stack.len() != locals_top {
                        return Err(VMError::BadAlloc);
                    }
                    // A Plain slot is just Null; a Boxed slot allocates a fresh
                    // cell and stores an Upval marker pointing at it, so the
                    // binding is captured by reference.
                    for kind in &kinds {
                        let slot = match kind {
                            SlotKind::Plain => StackValue::Null,
                            SlotKind::Boxed => {
                                let idx = self.cells.len() as CellIndex;
                                self.cells.push(StackValue::Null);
                                StackValue::Upval(idx)
                            }
                        };
                        self.stack.push(slot);
                    }
                    let frame = self.callstack.last_mut().ok_or(VMError::BadAlloc)?;
                    frame.local_count += kinds.len() as u32;
                    self.ip += 1;
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

                // ── control flow ─────────────────────────────────
                Instr::Call(addr, nargs) => {
                    if *addr as usize >= self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    if *nargs as usize > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    self.callstack.push(CallFrame {
                        arg_count: *nargs,
                        local_count: 0,
                        return_addr: self.ip + 1,
                        prev_fp: self.fp,
                    });
                    self.ip = *addr;
                    self.fp = self.stack.len() as StackAddr;
                }

                Instr::CallDyn(nargs) => {
                    let nargs = *nargs;
                    // The callable is on top, above its args; pop it, then the
                    // args sit exactly where a static Call expects them. It is
                    // either a bare Fn or a Ptr to a Closure (code + captures).
                    let callable = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let (addr, upvals) = match callable {
                        StackValue::Fn(addr) => (addr, None),
                        StackValue::Ptr(p) => match self.heap_get(p)? {
                            HeapValue::Closure { addr, upvals } => (*addr, Some(upvals.clone())),
                            _ => return Err(VMError::TypeError),
                        },
                        _ => return Err(VMError::TypeError),
                    };
                    if addr as usize >= self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    if nargs as usize > self.stack.len() {
                        return Err(VMError::StackUnderflow);
                    }
                    self.callstack.push(CallFrame {
                        arg_count: nargs,
                        local_count: 0,
                        return_addr: self.ip + 1,
                        prev_fp: self.fp,
                    });
                    self.fp = self.stack.len() as StackAddr;
                    // A closure's captured environment becomes the callee's
                    // leading locals (slots 0..K), so the body reaches them via
                    // the same Local/SetLocal indirection as any other local;
                    // its own Allocs append after these.
                    if let Some(upvals) = upvals {
                        let k = upvals.len() as u32;
                        for uv in upvals {
                            self.stack.push(uv);
                        }
                        self.callstack.last_mut().unwrap().local_count = k;
                    }
                    self.ip = addr;
                }

                Instr::MakeClosure(addr, captures) => {
                    let addr = *addr;
                    if addr as usize >= self.code.len() {
                        return Err(VMError::BadCall);
                    }
                    let captures = captures.clone(); // release the borrow on self.code
                    let local_count = self.callstack.last().ok_or(VMError::BadLocal)?.local_count;
                    let mut upvals = Vec::with_capacity(captures.len());
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
                    if frame.arg_count > self.fp {
                        return Err(VMError::StackUnderflow);
                    }
                    let keep_below = (self.fp - frame.arg_count) as usize;
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
                    if !is_truthy(&val) {
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
                Instr::Arg(arg) => {
                    let frame = self.callstack.last().ok_or(VMError::BadArg)?;
                    if *arg >= frame.arg_count {
                        return Err(VMError::BadArg);
                    }
                    // Left-to-right: arg 0 is the deepest (first pushed) at
                    // fp - arg_count, arg_count-1 is on top at fp - 1.
                    let slot = (self.fp - frame.arg_count + arg) as usize;
                    self.stack.push(self.stack[slot]);
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

                // ── variables ───────────────────────────────────
                Instr::Read(name) => {
                    let val = self
                        .variables
                        .get(name)
                        .copied()
                        .unwrap_or(StackValue::Null);
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::Write(name) => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.variables.insert(name.clone(), val);
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
                Instr::IsInt => {
                    // True for an integer value, or an integer-valued Number.
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let is_int = matches!(val, StackValue::PosInt(_) | StackValue::NegInt(_))
                        || matches!(val, StackValue::Number(n) if float_is_int(n));
                    self.stack.push(StackValue::Bool(is_int));
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
                Instr::IsArr => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let is_arr = matches!(
                        val,
                        StackValue::Ptr(p)
                            if matches!(self.heap.get(p as usize), Some(HeapValue::Array(_)))
                    );
                    self.stack.push(StackValue::Bool(is_arr));
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
                Instr::Abs => unary_num!(|n: f64| n.abs()),
                Instr::Neg => unary_num!(|n: f64| -n),
                Instr::Sqrt => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match as_f64(&val) {
                        Some(n) if n >= 0.0 => {
                            self.stack.push(StackValue::Number(n.sqrt()));
                            self.ip += 1;
                        }
                        Some(_) => return Err(VMError::ValueError),
                        None => return Err(VMError::TypeError),
                    }
                }
                Instr::Ceil => unary_num!(|n: f64| n.ceil()),
                Instr::Floor => unary_num!(|n: f64| n.floor()),
                Instr::Round => unary_num!(|n: f64| n.round()),
                Instr::Sign => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match as_f64(&val) {
                        Some(n) => {
                            self.stack.push(StackValue::Number(n.signum()));
                            self.ip += 1;
                        }
                        None => return Err(VMError::TypeError),
                    }
                }

                Instr::Not => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack.push(StackValue::Bool(!is_truthy(&val)));
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
                    // Numeric add (Int promotes to f64) or string concatenation.
                    let result = if let (Some(a), Some(b)) = (as_f64(&lhs), as_f64(&rhs)) {
                        StackValue::Number(a + b)
                    } else if let (StackValue::Ptr(p), StackValue::Ptr(q)) = (lhs, rhs) {
                        match (self.heap_get(p)?, self.heap_get(q)?) {
                            (HeapValue::String(a), HeapValue::String(b)) => {
                                self.alloc_string(format!("{}{}", a, b))
                            }
                            _ => return Err(VMError::TypeError),
                        }
                    } else {
                        return Err(VMError::TypeError);
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::Sub => binary_num!(|a: f64, b: f64| a - b),
                Instr::Mul => binary_num!(|a: f64, b: f64| a * b),
                Instr::Div => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match (as_f64(&lhs), as_f64(&rhs)) {
                        (Some(_), Some(b)) if b == 0.0 => {
                            return Err(VMError::ValueError);
                        }
                        (Some(a), Some(b)) => {
                            self.stack.push(StackValue::Number(a / b));
                            self.ip += 1;
                        }
                        _ => return Err(VMError::TypeError),
                    }
                }
                Instr::Mod => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match (as_i64(&lhs), as_i64(&rhs)) {
                        (Some(_), Some(0)) => return Err(VMError::ValueError),
                        (Some(a), Some(b)) => {
                            self.stack.push(StackValue::Number((a % b) as f64));
                            self.ip += 1;
                        }
                        _ => return Err(VMError::TypeError),
                    }
                }
                Instr::Pow => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    match (as_f64(&lhs), as_f64(&rhs)) {
                        (Some(a), Some(b)) => {
                            self.stack.push(StackValue::Number(a.powf(b)));
                            self.ip += 1;
                        }
                        _ => return Err(VMError::TypeError),
                    }
                }

                Instr::Min => binary_num!(|a: f64, b: f64| a.min(b)),
                Instr::Max => binary_num!(|a: f64, b: f64| a.max(b)),

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
                    self.stack.push(if is_truthy(&lhs) { rhs } else { lhs });
                    self.ip += 1;
                }
                Instr::Or => {
                    let rhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let lhs = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    self.stack.push(if is_truthy(&lhs) { lhs } else { rhs });
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
                    let vals: Vec<StackValue> = self.stack.drain(split..).collect();
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

                Instr::ObjGetDyn => {
                    let field_ptr = self.pop_ptr()?;
                    let field = self
                        .heap_str(field_ptr)
                        .ok_or(VMError::TypeError)?
                        .to_string();
                    let obj_ptr = self.pop_ptr()?;
                    let val = self
                        .heap_obj(obj_ptr)
                        .and_then(|obj| obj.get(&*field).copied())
                        .unwrap_or(StackValue::Null);
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::ObjSetDyn => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let field_ptr = self.pop_ptr()?;
                    let field = self
                        .heap_str(field_ptr)
                        .ok_or(VMError::TypeError)?
                        .to_string();
                    let obj_ptr = self.pop_ptr()?;
                    let obj = self.heap_obj_mut(obj_ptr).ok_or(VMError::TypeError)?;
                    obj.insert(field, val);
                    self.ip += 1;
                }

                Instr::ObjGet(field) => {
                    let field = field.clone();
                    let obj_ptr = self.pop_ptr()?;
                    let val = self
                        .heap_obj(obj_ptr)
                        .and_then(|obj| obj.get(&field).copied())
                        .unwrap_or(StackValue::Null);
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::ObjSet(field) => {
                    let field = field.clone();
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let obj_ptr = self.pop_ptr()?;
                    let obj = self.heap_obj_mut(obj_ptr).ok_or(VMError::TypeError)?;
                    obj.insert(field, val);
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
                    let vals: Vec<StackValue> = self.stack.drain(split..).collect();
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

                Instr::ArrGet => {
                    let index = self.pop_int()?;
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr(arr_ptr).ok_or(VMError::TypeError)?;
                    if index < 0 {
                        return Err(VMError::ValueError);
                    }
                    let val = arr.get(index as usize).copied().unwrap_or(StackValue::Null);
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::ArrSet => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let index = self.pop_int()?;
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
                    if index < 0 || index as usize >= arr.len() {
                        return Err(VMError::ValueError);
                    }
                    arr[index as usize] = val;
                    self.ip += 1;
                }

                Instr::ArrPush => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
                    arr.push(val);
                    self.ip += 1;
                }

                Instr::ArrPop => {
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
                    let val = arr.pop().ok_or(VMError::ValueError)?;
                    self.stack.push(val);
                    self.ip += 1;
                }

                // JS semantics: shift removes & returns the front element.
                Instr::ArrShift => {
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
                    if arr.is_empty() {
                        return Err(VMError::ValueError);
                    }
                    let val = arr.remove(0);
                    self.stack.push(val);
                    self.ip += 1;
                }

                // JS semantics: unshift prepends an element to the front.
                Instr::ArrUnshift => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
                    arr.insert(0, val);
                    self.ip += 1;
                }

                Instr::ArrJoin => {
                    let sep = self.pop_string()?;
                    let arr_ptr = self.pop_ptr()?;
                    let arr = self.heap_arr(arr_ptr).ok_or(VMError::TypeError)?;
                    let parts: Vec<String> = arr.iter().map(|v| self.stringify_value(v)).collect();
                    let s = self.alloc_string(parts.join(&sep));
                    self.stack.push(s);
                    self.ip += 1;
                }

                // ── string operations ───────────────────────────
                //
                // All character offsets/indices below are UTF-8 *byte*
                // offsets (as in Rust/Go), and lengths are byte lengths. Any
                // offset that is out of range or lands in the middle of a
                // multi-byte codepoint yields a ValueError rather than
                // panicking. (ASCII text behaves exactly as expected.)
                Instr::StrSplit(n) => {
                    let limit = if *n >= 1 {
                        let lim = self.pop_int()?;
                        if lim < 0 {
                            return Err(VMError::ValueError);
                        }
                        Some(lim as usize)
                    } else {
                        None
                    };
                    let delim = self.pop_string()?;
                    let s = self.pop_string()?;
                    let mut parts = Vec::new();
                    match limit {
                        Some(lim) => {
                            for p in s.splitn(lim, &delim) {
                                parts.push(self.alloc_string(p.to_string()));
                            }
                        }
                        None => {
                            for p in s.split(&delim) {
                                parts.push(self.alloc_string(p.to_string()));
                            }
                        }
                    }
                    let arr_ptr = self.alloc_array(parts);
                    self.stack.push(arr_ptr);
                    self.ip += 1;
                }

                Instr::StrIncludes(n) => {
                    let start = if *n >= 1 { Some(self.pop_int()?) } else { None };
                    let needle = self.pop_string()?;
                    let haystack = self.pop_string()?;
                    let found = match start {
                        Some(s) if s >= 0 => {
                            let start = s as usize;
                            start <= haystack.len()
                                && haystack.is_char_boundary(start)
                                && haystack[start..].contains(&needle)
                        }
                        Some(_) => false,
                        None => haystack.contains(&needle),
                    };
                    self.stack.push(StackValue::Bool(found));
                    self.ip += 1;
                }

                Instr::StrStartsWith => {
                    let prefix = self.pop_string()?;
                    let s = self.pop_string()?;
                    self.stack.push(StackValue::Bool(s.starts_with(&prefix)));
                    self.ip += 1;
                }

                Instr::StrEndsWith => {
                    let suffix = self.pop_string()?;
                    let s = self.pop_string()?;
                    self.stack.push(StackValue::Bool(s.ends_with(&suffix)));
                    self.ip += 1;
                }

                Instr::StrIndexOf(n) => {
                    let start = if *n >= 1 { Some(self.pop_int()?) } else { None };
                    let needle = self.pop_string()?;
                    let haystack = self.pop_string()?;
                    let pos = match start {
                        Some(s) if s >= 0 => {
                            let start = s as usize;
                            if start <= haystack.len() && haystack.is_char_boundary(start) {
                                haystack[start..].find(&needle).map(|p| (p + start) as f64)
                            } else {
                                None
                            }
                        }
                        Some(_) => None,
                        None => haystack.find(&needle).map(|p| p as f64),
                    };
                    self.stack.push(StackValue::Number(pos.unwrap_or(-1.0)));
                    self.ip += 1;
                }

                Instr::StrLastIndexOf(n) => {
                    let start = if *n >= 1 { Some(self.pop_int()?) } else { None };
                    let needle = self.pop_string()?;
                    let haystack = self.pop_string()?;
                    let pos = match start {
                        Some(s) if s >= 0 => {
                            // Search the prefix up to `start + needle.len()`,
                            // clamped to a valid char boundary so slicing can't
                            // panic.
                            let mut end = haystack.len().min(s as usize + needle.len());
                            while end > 0 && !haystack.is_char_boundary(end) {
                                end -= 1;
                            }
                            haystack[..end].rfind(&needle).map(|p| p as f64)
                        }
                        Some(_) => None,
                        None => haystack.rfind(&needle).map(|p| p as f64),
                    };
                    self.stack.push(StackValue::Number(pos.unwrap_or(-1.0)));
                    self.ip += 1;
                }

                Instr::StrSlice => {
                    let end = self.pop_int()?;
                    let start = self.pop_int()?;
                    let s = self.pop_string()?;
                    if start < 0 || end < 0 || start > end {
                        return Err(VMError::ValueError);
                    }
                    let start = start as usize;
                    let end = end as usize;
                    if start > s.len()
                        || end > s.len()
                        || !s.is_char_boundary(start)
                        || !s.is_char_boundary(end)
                    {
                        return Err(VMError::ValueError);
                    }
                    let sliced = self.alloc_string(s[start..end].to_string());
                    self.stack.push(sliced);
                    self.ip += 1;
                }

                Instr::StrTrim => {
                    let s = self.pop_string()?;
                    let trimmed = self.alloc_string(s.trim().to_string());
                    self.stack.push(trimmed);
                    self.ip += 1;
                }

                Instr::StrToInt => {
                    // Parse losslessly into the canonical variant: non-negative
                    // (up to u64::MAX) -> PosInt, negative -> NegInt.
                    let s = self.pop_string()?;
                    let t = s.trim();
                    let val = if let Ok(u) = t.parse::<u64>() {
                        StackValue::PosInt(u)
                    } else if let Ok(i) = t.parse::<i64>() {
                        StackValue::NegInt(i)
                    } else {
                        return Err(VMError::ValueError);
                    };
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::StrToFloat => {
                    let s = self.pop_string()?;
                    let n: f64 = s.trim().parse().map_err(|_| VMError::ValueError)?;
                    self.stack.push(StackValue::Number(n));
                    self.ip += 1;
                }

                Instr::StrToJson => {
                    let s = self.pop_string()?;
                    let json: serde_json::Value =
                        serde_json::from_str(&s).map_err(|_| VMError::ValueError)?;
                    let converted = self.json_to_stack_value(&json, 0)?;
                    self.stack.push(converted);
                    self.ip += 1;
                }

                Instr::StrFromJson => {
                    let val = self.stack.pop().ok_or(VMError::StackUnderflow)?;
                    let json = self.stack_value_to_json(&val, 0)?;
                    let s = serde_json::to_string(&json).map_err(|_| VMError::ValueError)?;
                    let ptr = self.alloc_string(s);
                    self.stack.push(ptr);
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
                        condition: condition.clone(),
                    });
                }
            }
        }
    }

    /// Best-effort string representation of a value (for ArrJoin).
    fn stringify_value(&self, val: &StackValue) -> String {
        match val {
            StackValue::Null => "null".to_string(),
            StackValue::Bool(b) => b.to_string(),
            StackValue::PosInt(u) => u.to_string(),
            StackValue::NegInt(i) => i.to_string(),
            StackValue::Fn(addr) => format!("[function@{addr}]"),
            // Internal indirection; should not normally reach here.
            StackValue::Upval(c) => format!("[upval@{c}]"),
            StackValue::Number(n) => {
                if float_is_int(*n) {
                    format!("{}", *n as i64)
                } else {
                    format!("{}", n)
                }
            }
            StackValue::Ptr(p) => match self.heap.get(*p as usize) {
                Some(HeapValue::String(s)) => s.clone(),
                Some(HeapValue::Array(_)) => "[array]".to_string(),
                Some(HeapValue::Object(_)) => "[object]".to_string(),
                Some(HeapValue::Closure { addr, .. }) => format!("[closure@{addr}]"),
                None => "null".to_string(), // dangling pointer
            },
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
            vm.alloc_string(s.to_string());
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
    /// Heap pointer to string at the given index (pre-loaded via run_heap).
    fn s(addr: u32) -> StackValue {
        StackValue::Ptr(addr)
    }
    /// `n` plain (unboxed) local slots, for `Alloc`.
    fn plain(n: usize) -> Vec<SlotKind> {
        vec![SlotKind::Plain; n]
    }

    // ── stack manipulation ────────────────────────────────────────

    #[test]
    fn push_and_pop() {
        assert_eq!(run(vec![Push(n(1.0)), Push(n(2.0)), Pop(1)]), vec![n(1.0)]);
        assert_eq!(run(vec![Push(n(1.0)), Pop(1)]), vec![]);
        assert!(matches!(run_err(vec![Pop(1)]), VMError::StackUnderflow));
    }

    #[test]
    fn dup_swap_rot() {
        assert_eq!(run(vec![Push(n(1.0)), Dup]), vec![n(1.0), n(1.0)]);
        assert_eq!(
            run(vec![Push(n(1.0)), Push(n(2.0)), Swap]),
            vec![n(2.0), n(1.0)]
        );
        assert_eq!(
            run(vec![Push(n(1.0)), Push(n(2.0)), Push(n(3.0)), Rot]),
            vec![n(2.0), n(3.0), n(1.0)]
        );
        assert!(matches!(run_err(vec![Swap]), VMError::StackUnderflow));
        assert!(matches!(run_err(vec![Rot]), VMError::StackUnderflow));
    }

    // ── type predicates ───────────────────────────────────────────

    #[test]
    fn is_null() {
        assert_eq!(run(vec![Push(null()), IsNull]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(0.0)), IsNull]), vec![b(false)]);
    }

    #[test]
    fn is_bool() {
        assert_eq!(run(vec![Push(b(true)), IsBool]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(0.0)), IsBool]), vec![b(false)]);
    }

    #[test]
    fn is_int_float_num() {
        assert_eq!(run(vec![Push(n(3.0)), IsInt]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(3.14)), IsInt]), vec![b(false)]);
        // IsFloat is true only for numbers with a fractional part.
        assert_eq!(run(vec![Push(n(3.14)), IsFloat]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(3.0)), IsFloat]), vec![b(false)]);
        assert_eq!(run(vec![Push(null()), IsFloat]), vec![b(false)]);
        // IsNum is true for any number, integer-valued or not.
        assert_eq!(run(vec![Push(n(3.0)), IsNum]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(3.14)), IsNum]), vec![b(true)]);
        assert_eq!(run(vec![Push(null()), IsNum]), vec![b(false)]);
    }

    #[test]
    fn is_str_arr_obj() {
        // IsStr with a string
        assert_eq!(run_heap(vec![Push(s(0)), IsStr], &["hi"]), vec![b(true)]);
        // IsStr with a number
        assert_eq!(run(vec![Push(n(0.0)), IsStr]), vec![b(false)]);
        // IsArr
        let mut vm = VM::new(vec![IsArr]);
        vm.heap.push(HeapValue::Array(vec![]));
        vm.stack.push(s(0));
        match vm.step().unwrap() {
            StepResult::Done => {}
            _ => panic!(),
        }
        assert_eq!(vm.stack, vec![b(true)]);
        // IsObj
        let mut vm = VM::new(vec![IsObj]);
        vm.heap.push(HeapValue::Object(IndexMap::new()));
        vm.stack.push(s(0));
        match vm.step().unwrap() {
            StepResult::Done => {}
            _ => panic!(),
        }
        assert_eq!(vm.stack, vec![b(true)]);
    }

    // ── unary operators ───────────────────────────────────────────

    #[test]
    fn abs_neg() {
        assert_eq!(run(vec![Push(n(-3.0)), Abs]), vec![n(3.0)]);
        assert_eq!(run(vec![Push(n(3.0)), Neg]), vec![n(-3.0)]);
        assert!(matches!(
            run_err(vec![Push(null()), Abs]),
            VMError::TypeError
        ));
    }

    #[test]
    fn ceil_floor_round() {
        assert_eq!(run(vec![Push(n(3.14)), Ceil]), vec![n(4.0)]);
        assert_eq!(run(vec![Push(n(3.14)), Floor]), vec![n(3.0)]);
        assert_eq!(run(vec![Push(n(3.6)), Round]), vec![n(4.0)]);
    }

    #[test]
    fn sqrt_sign() {
        assert_eq!(run(vec![Push(n(9.0)), Sqrt]), vec![n(3.0)]);
        // f64::signum: 1.0 for positive/+0, -1.0 for negative/-0, self for NaN
        assert_eq!(run(vec![Push(n(5.0)), Sign]), vec![n(1.0)]);
        assert_eq!(run(vec![Push(n(-5.0)), Sign]), vec![n(-1.0)]);
        assert_eq!(run(vec![Push(n(0.0)), Sign]), vec![n(1.0)]);
        assert!(matches!(
            run_err(vec![Push(n(-1.0)), Sqrt]),
            VMError::ValueError
        ));
    }

    #[test]
    fn not_bitnot() {
        assert_eq!(run(vec![Push(b(false)), Not]), vec![b(true)]);
        assert_eq!(run(vec![Push(null()), Not]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(1.0)), Not]), vec![b(false)]);
        assert_eq!(run(vec![Push(n(5.0)), BitNot]), vec![n(-6.0)]);
        assert!(matches!(
            run_err(vec![Push(n(3.14)), BitNot]),
            VMError::TypeError
        ));
    }

    // ── binary operators ──────────────────────────────────────────

    #[test]
    fn add_sub_mul_div() {
        assert_eq!(run(vec![Push(n(2.0)), Push(n(3.0)), Add]), vec![n(5.0)]);
        assert_eq!(run(vec![Push(n(10.0)), Push(n(3.0)), Sub]), vec![n(7.0)]);
        assert_eq!(run(vec![Push(n(4.0)), Push(n(5.0)), Mul]), vec![n(20.0)]);
        assert_eq!(run(vec![Push(n(10.0)), Push(n(4.0)), Div]), vec![n(2.5)]);
        assert!(matches!(
            run_err(vec![Push(n(1.0)), Push(n(0.0)), Div]),
            VMError::ValueError
        ));
    }

    #[test]
    fn mod_op() {
        assert_eq!(run(vec![Push(n(10.0)), Push(n(3.0)), Mod]), vec![n(1.0)]);
        assert!(matches!(
            run_err(vec![Push(n(1.0)), Push(n(0.0)), Mod]),
            VMError::ValueError
        ));
    }

    #[test]
    fn min_max_pow() {
        assert_eq!(run(vec![Push(n(3.0)), Push(n(7.0)), Min]), vec![n(3.0)]);
        assert_eq!(run(vec![Push(n(3.0)), Push(n(7.0)), Max]), vec![n(7.0)]);
        assert_eq!(run(vec![Push(n(2.0)), Push(n(3.0)), Pow]), vec![n(8.0)]);
    }

    #[test]
    fn add_strings() {
        // heap[0]="hello ", heap[1]="world"
        assert_eq!(
            run_heap(vec![Push(s(0)), Push(s(1)), Add], &["hello ", "world"]),
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

    // ── comparisons ───────────────────────────────────────────────

    #[test]
    fn eq_neq() {
        assert_eq!(run(vec![Push(n(1.0)), Push(n(1.0)), Eq]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(1.0)), Push(n(2.0)), Eq]), vec![b(false)]);
        assert_eq!(run(vec![Push(n(1.0)), Push(n(2.0)), Neq]), vec![b(true)]);
        // NaN != NaN
        assert_eq!(
            run(vec![Push(n(f64::NAN)), Push(n(f64::NAN)), Eq]),
            vec![b(false)]
        );
        // different types are not equal
        assert_eq!(run(vec![Push(n(0.0)), Push(null()), Eq]), vec![b(false)]);
    }

    #[test]
    fn string_eq() {
        // heap[0]="abc", heap[1]="abc", heap[2]="xyz"
        let code = vec![Push(s(0)), Push(s(1)), Eq, Push(s(0)), Push(s(2)), Eq];
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
        assert_eq!(run(vec![Push(n(1.0)), Push(n(2.0)), Lt]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(2.0)), Push(n(1.0)), Gt]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(2.0)), Push(n(2.0)), LtEq]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(2.0)), Push(n(2.0)), GtEq]), vec![b(true)]);
        // Incomparable types → false
        assert_eq!(run(vec![Push(n(1.0)), Push(null()), Lt]), vec![b(false)]);
    }

    #[test]
    fn and_or() {
        // truthy && rhs → rhs
        assert_eq!(run(vec![Push(b(true)), Push(n(42.0)), And]), vec![n(42.0)]);
        // falsy && rhs → falsy
        assert_eq!(
            run(vec![Push(b(false)), Push(n(42.0)), And]),
            vec![b(false)]
        );
        // truthy || rhs → truthy
        assert_eq!(run(vec![Push(n(42.0)), Push(b(false)), Or]), vec![n(42.0)]);
        // falsy || rhs → rhs
        assert_eq!(run(vec![Push(null()), Push(n(99.0)), Or]), vec![n(99.0)]);
    }

    // ── bitwise ops ───────────────────────────────────────────────

    #[test]
    fn bitwise() {
        assert_eq!(
            run(vec![Push(n(10.0)), Push(n(12.0)), BitAnd]),
            vec![n(8.0)] // 0b1010 & 0b1100 = 0b1000
        );
        assert_eq!(
            run(vec![Push(n(10.0)), Push(n(12.0)), BitOr]),
            vec![n(14.0)] // 0b1010 | 0b1100 = 0b1110
        );
        assert_eq!(
            run(vec![Push(n(10.0)), Push(n(12.0)), BitXor]),
            vec![n(6.0)] // 0b1010 ^ 0b1100 = 0b0110
        );
        assert_eq!(run(vec![Push(n(1.0)), Push(n(3.0)), BitLhs]), vec![n(8.0)]);
        assert_eq!(run(vec![Push(n(8.0)), Push(n(2.0)), BitRhs]), vec![n(2.0)]);
    }

    // ── control flow ──────────────────────────────────────────────

    #[test]
    fn jump_and_jfalse() {
        // Jump over a Push: should only leave n(1.0) on stack
        assert_eq!(
            run(vec![Push(n(1.0)), Jump(3), Push(n(999.0))]),
            vec![n(1.0)]
        );
        // JFalse with false → jump over Push
        assert_eq!(run(vec![Push(b(false)), JFalse(3), Push(n(999.0))]), vec![]);
        // JFalse with true → don't jump, execute Push
        assert_eq!(
            run(vec![Push(b(true)), JFalse(3), Push(n(42.0))]),
            vec![n(42.0)]
        );
        // Null is falsy
        assert_eq!(run(vec![Push(null()), JFalse(3), Push(n(999.0))]), vec![]);
    }

    #[test]
    fn label_noop() {
        // Label should be a no-op at runtime
        assert_eq!(
            run(vec![Push(n(1.0)), Label(42), Push(n(2.0))]),
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
        // [4] Arg(0)     -- fn: push arg 0 (10)
        // [5] Arg(1)     -- fn: push arg 1 (20)
        // [6] Add        -- fn: 10 + 20 = 30
        // [7] Return(1)  -- fn: return 1 value
        assert_eq!(
            run(vec![
                Push(n(10.0)),
                Push(n(20.0)),
                Call(4, 2),
                Return(1),
                Arg(0),
                Arg(1),
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
                Push(n(10.0)),
                Push(n(3.0)),
                Call(4, 2),
                Return(1),
                Arg(0),
                Arg(1),
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
        // [5] Arg(0)         fn body
        // [6] Arg(1)
        // [7] Sub            10 - 3
        // [8] Return(1)
        assert_eq!(
            run(vec![
                Push(n(10.0)),
                Push(n(3.0)),
                Push(f(5)),
                CallDyn(2),
                Return(1),
                Arg(0),
                Arg(1),
                Sub,
                Return(1),
            ]),
            vec![n(7.0)]
        );
    }

    #[test]
    fn call_dyn_requires_fn() {
        // Top of stack must be a Fn, not some other value.
        let code = vec![Push(n(1.0)), Push(n(2.0)), CallDyn(1)];
        assert!(matches!(run_err(code), VMError::TypeError));
    }

    #[test]
    fn call_dyn_bad_addr() {
        let code = vec![Push(f(999)), CallDyn(0)];
        assert!(matches!(run_err(code), VMError::BadCall));
    }

    #[test]
    fn fn_value_equality_and_json() {
        // Same address -> equal; different -> not.
        assert_eq!(run(vec![Push(f(3)), Push(f(3)), Eq]), vec![b(true)]);
        assert_eq!(run(vec![Push(f(3)), Push(f(4)), Eq]), vec![b(false)]);
        // A Fn has no JSON representation.
        let mut vm = VM::new(vec![Push(f(0)), StrFromJson]);
        assert!(matches!(vm.step().unwrap_err(), VMError::ValueError));
    }

    #[test]
    fn map_as_bytecode_library_fn() {
        // `map(arr, fn)` written in the DSL itself: loop over the array, call
        // `fn` on each element via CallDyn, collect into a new array. Proves
        // first-class Fns work end-to-end with no special opcode and no
        // closures. Here map([1,2,3], double) -> [2, 4, 6].
        //
        // main:  build [1,2,3], push Fn(double), Call(map, 2), Return(1)
        // double(x):     Arg(0) * 2
        // map(arr, fn):  out=[]; i=0; while i<len: out.push(fn(arr[i])); i++
        let mut code: Vec<Instr> = Vec::new();

        // main
        code.push(Push(n(1.0)));
        code.push(Push(n(2.0)));
        code.push(Push(n(3.0)));
        code.push(ArrNew(3));
        let push_fn_idx = code.len();
        code.push(Push(f(0))); // patched -> double
        let call_map_idx = code.len();
        code.push(Call(0, 2)); // patched -> map
        code.push(Return(1));

        // double(x) = x * 2
        let double_addr = code.len() as u32;
        code.push(Arg(0));
        code.push(Push(n(2.0)));
        code.push(Mul);
        code.push(Return(1));

        // map(arr, fn)
        let map_addr = code.len() as u32;
        code.push(Alloc(plain(2))); // local 0 = out, local 1 = i
        code.push(ArrNew(0));
        code.push(SetLocal(0)); // out = []
        code.push(Push(n(0.0)));
        code.push(SetLocal(1)); // i = 0
        let loop_addr = code.len() as u32;
        code.push(Local(1));
        code.push(Arg(0));
        code.push(ArrLength);
        code.push(Lt); // i < len(arr)
        let jfalse_idx = code.len();
        code.push(JFalse(0)); // patched -> end
        code.push(Local(0)); // out (ArrPush receiver)
        code.push(Arg(0));
        code.push(Local(1));
        code.push(ArrGet); // arr[i]
        code.push(Arg(1));
        code.push(CallDyn(1)); // fn(arr[i])
        code.push(ArrPush); // out.push(...)
        code.push(Local(1));
        code.push(Push(n(1.0)));
        code.push(Add);
        code.push(SetLocal(1)); // i++
        code.push(Jump(loop_addr));
        let end_addr = code.len() as u32;
        code.push(Local(0));
        code.push(Return(1));

        code[push_fn_idx] = Push(f(double_addr));
        code[call_map_idx] = Call(map_addr, 2);
        code[jfalse_idx] = JFalse(end_addr);

        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
        assert_eq!(vm.stack.len(), 1);
        let StackValue::Ptr(p) = vm.stack[0] else {
            panic!("expected array pointer");
        };
        assert_eq!(
            vm.heap[p as usize],
            HeapValue::Array(vec![n(2.0), n(4.0), n(6.0)])
        );
    }

    // ── closures ──────────────────────────────────────────────────

    /// Append a `makeCounter` to `code`: a function that boxes a `count` local
    /// (slot 0), initializes it to 0, and returns a closure that increments and
    /// returns `count`. Returns makeCounter's code address.
    fn append_counter(code: &mut Vec<Instr>) -> u32 {
        let mc = code.len() as u32;
        code.push(Alloc(vec![SlotKind::Boxed])); // slot 0 = count (by-ref)
        code.push(Push(n(0.0)));
        code.push(SetLocal(0)); // count = 0 (writes through the cell)
        let mk = code.len();
        code.push(MakeClosure(0, vec![0])); // patched: capture count
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(Local(0)); // count  (slot 0 = captured upval)
        code.push(Push(n(1.0)));
        code.push(Add);
        code.push(SetLocal(0)); // count = count + 1 (through the shared cell)
        code.push(Local(0));
        code.push(Return(1)); // return count
        code[mk] = MakeClosure(inner, vec![0]);
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
        code.push(Alloc(plain(2))); // local 0 = c1, local 1 = c2
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
            HeapValue::Array(vec![n(1.0), n(2.0), n(1.0)])
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
        code.push(Alloc(plain(1))); // slot 0 = x (NOT boxed)
        code.push(Push(n(5.0)));
        code.push(SetLocal(0));
        let mk = code.len();
        code.push(MakeClosure(0, vec![0])); // snapshot x = 5
        code.push(Push(n(99.0)));
        code.push(SetLocal(0)); // x = 99 AFTER capture (must not be seen)
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(Local(0)); // return captured snapshot
        code.push(Return(1));
        code[call] = Call(maker, 0);
        code[mk] = MakeClosure(inner, vec![0]);
        assert_eq!(run(code), vec![n(5.0)]);
    }

    #[test]
    fn two_closures_share_one_cell() {
        // A getter and a setter closing over the same boxed `x` must see each
        // other's writes. setter(42) then getter() → 42.
        let mut code: Vec<Instr> = Vec::new();
        // main: arr = maker(); setter = arr[1]; setter(42); getter = arr[0]; getter()
        code.push(Alloc(plain(1))); // local 0 = [getter, setter]
        let call = code.len();
        code.push(Call(0, 0));
        code.push(SetLocal(0));
        code.push(Push(n(42.0))); // setter's arg
        code.push(Local(0));
        code.push(Push(n(1.0)));
        code.push(ArrGet); // setter
        code.push(CallDyn(1)); // setter(42) → (no result)
        code.push(Local(0));
        code.push(Push(n(0.0)));
        code.push(ArrGet); // getter
        code.push(CallDyn(0)); // getter() → 42
        code.push(Return(1));
        // maker
        let maker = code.len() as u32;
        code.push(Alloc(vec![SlotKind::Boxed])); // slot 0 = x (by-ref)
        code.push(Push(n(0.0)));
        code.push(SetLocal(0));
        let mk_get = code.len();
        code.push(MakeClosure(0, vec![0]));
        let mk_set = code.len();
        code.push(MakeClosure(0, vec![0]));
        code.push(ArrNew(2)); // [getter, setter]
        code.push(Return(1));
        let getter = code.len() as u32;
        code.push(Local(0));
        code.push(Return(1));
        let setter = code.len() as u32;
        code.push(Arg(0));
        code.push(SetLocal(0)); // x = arg (through the shared cell)
        code.push(Return(0));
        code[call] = Call(maker, 0);
        code[mk_get] = MakeClosure(getter, vec![0]);
        code[mk_set] = MakeClosure(setter, vec![0]);
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
        code.push(Alloc(vec![SlotKind::Boxed]));
        code.push(Push(n(7.0)));
        code.push(SetLocal(0));
        let mk_mid = code.len();
        code.push(MakeClosure(0, vec![0]));
        code.push(Return(1));
        let middle = code.len() as u32;
        // middle's slot 0 is x (installed upval); forward it to inner.
        let mk_in = code.len();
        code.push(MakeClosure(0, vec![0]));
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(Local(0));
        code.push(Return(1));
        code[call] = Call(outer, 0);
        code[mk_mid] = MakeClosure(middle, vec![0]);
        code[mk_in] = MakeClosure(inner, vec![0]);
        assert_eq!(run(code), vec![n(7.0)]);
    }

    #[test]
    fn closure_identity_equality() {
        // The same closure object equals itself (reference identity)…
        let same = vec![
            Alloc(vec![SlotKind::Boxed]),
            Push(n(1.0)),
            SetLocal(0),
            MakeClosure(6, vec![0]),
            Dup,
            Eq,
            Return(1), // addr 6: also a valid (never-called) closure target
        ];
        assert_eq!(run(same), vec![b(true)]);
        // …but two distinct closure objects do not (no content equality).
        let distinct = vec![
            Alloc(vec![SlotKind::Boxed]),
            Push(n(1.0)),
            SetLocal(0),
            MakeClosure(7, vec![0]),
            MakeClosure(7, vec![0]),
            Eq,
            Return(1),
            Return(1), // addr 7
        ];
        assert_eq!(run(distinct), vec![b(false)]);
    }

    #[test]
    fn closure_has_no_json_representation() {
        // Serializing a closure fails loudly, like a bare Fn.
        let code = vec![
            Alloc(vec![SlotKind::Boxed]),
            Push(n(1.0)),
            SetLocal(0),
            MakeClosure(5, vec![0]),
            StrFromJson,
            Return(1), // addr 5
        ];
        assert!(matches!(run_err(code), VMError::ValueError));
    }

    #[test]
    fn make_closure_rejects_out_of_range_capture() {
        // Capturing a slot the frame doesn't have is a compiler bug → BadLocal.
        let code = vec![
            Call(2, 0),
            Return(0),
            Alloc(plain(1)),
            MakeClosure(0, vec![5]), // only slot 0 exists
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
        // Function allocates a local, stores arg+arg in it, returns it.
        // [0] Push(7)
        // [1] Push(8)
        // [2] Call(4, 2)
        // [3] Return(1)
        // [4] Alloc(plain(1))
        // [5] Arg(0)
        // [6] Arg(1)
        // [7] Add
        // [8] SetLocal(0)
        // [9] Local(0)
        // [10] Return(1)
        assert_eq!(
            run(vec![
                Push(n(7.0)),
                Push(n(8.0)),
                Call(4, 2),
                Return(1),
                Alloc(plain(1)),
                Arg(0),
                Arg(1),
                Add,
                SetLocal(0),
                Local(0),
                Return(1),
            ]),
            vec![n(15.0)]
        );
    }

    #[test]
    fn alloc_bad_when_sp_not_fp() {
        // Alloc should fail when temporaries are on the stack (sp > fp)
        let code = vec![
            Push(n(1.0)), // caller pushes an arg
            Push(n(2.0)),
            Call(4, 2), // call fn
            Return(0),
            Push(n(99.0)), // fn pushes a temp FIRST (sp > fp)
            Alloc(plain(1)),      // should fail
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::BadAlloc));
    }

    // ── variables ─────────────────────────────────────────────────

    #[test]
    fn read_write_variable() {
        let mut vm = VM::new(vec![
            Push(n(42.0)),
            Write("x".into()),
            Read("x".into()),
            Push(n(1.0)),
            Add,
        ]);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(vm.stack, vec![n(43.0)]);
        assert_eq!(vm.variables.get("x"), Some(&n(42.0)));
    }

    #[test]
    fn read_unset_variable_is_null() {
        assert_eq!(run(vec![Read("nonexistent".into())]), vec![null()]);
    }

    // ── frame access validation ───────────────────────────────────

    #[test]
    fn arg_oob() {
        let code = vec![
            Push(n(1.0)),
            Call(3, 1),
            Return(0),
            Arg(5), // only 1 arg available
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::BadArg));
    }

    #[test]
    fn local_oob() {
        let code = vec![
            Push(n(1.0)),
            Call(3, 1),
            Return(0),
            Local(0), // no locals allocated
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
                Push(n(1.0)),
                Push(n(2.0)),
                Push(n(3.0)),
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
            Push(n(10.0)),
            Push(n(20.0)),
            Push(n(30.0)),
            ArrNew(3),
            Push(n(1.0)),  // index
            Push(n(99.0)), // value
            ArrSet,        // pops: value, index, arr_ptr (ptr consumed)
        ];
        // ArrSet consumes ptr, nothing left on stack.
        assert!(run(code).is_empty());
    }

    #[test]
    fn arr_get_set_with_dup() {
        // Keep ptr around with Dup before mutation.
        let code = vec![
            Push(n(10.0)),
            Push(n(20.0)),
            Push(n(30.0)),
            ArrNew(3),
            Dup,           // save ptr for later
            Push(n(1.0)),  // index
            Push(n(99.0)), // value
            ArrSet,        // pops value, index, ptr_copy → stack: [ptr]
            Push(n(1.0)),  // index
            ArrGet,        // pops index, ptr → pushes arr[1]
        ];
        assert_eq!(run(code), vec![n(99.0)]);
    }

    #[test]
    fn arr_push_pop() {
        // Create [10], push 20, pop back.
        let code = vec![
            Push(n(10.0)),
            ArrNew(1), // [10], stack: [ptr]
            Push(n(20.0)),
            ArrPush, // [10, 20], stack: []
        ];
        assert!(run(code).is_empty());
    }

    #[test]
    fn arr_pop_returns_last() {
        // Left-to-right: first pushed = arr[0]. Push 10, 20 → arr = [10, 20].
        let code = vec![
            Push(n(10.0)),
            Push(n(20.0)),
            ArrNew(2),
            ArrPop, // → 20 (last element)
        ];
        assert_eq!(run(code), vec![n(20.0)]);
    }

    #[test]
    fn arr_shift_unshift() {
        // JS semantics: unshift prepends, shift removes the front.
        // Push 30, Push 20, ArrNew(2) → arr = [30, 20] (left-to-right)
        // Dup ptr, Push 10, ArrUnshift → [10, 30, 20]
        // ArrShift → removes & returns front (10)
        let code = vec![
            Push(n(30.0)),
            Push(n(20.0)),
            ArrNew(2),
            Dup, // ptr for later
            Push(n(10.0)),
            ArrUnshift, // [10, 30, 20], ptr consumed
            ArrShift,   // → 10 (front of the Dup'd ptr)
        ];
        assert_eq!(run(code), vec![n(10.0)]);
    }

    #[test]
    fn arr_shift_empty_errors() {
        let code = vec![ArrNew(0), ArrShift];
        assert!(matches!(run_err(code), VMError::ValueError));
    }

    #[test]
    fn arr_join() {
        // Left-to-right: push 1, 2, 3 → arr = [1, 2, 3]
        let code = vec![
            Push(n(1.0)),
            Push(n(2.0)),
            Push(n(3.0)),
            ArrNew(3),
            Push(s(0)), // separator ", " at heap[0]
            ArrJoin,
        ];
        let strings = &[", "];
        let mut vm = VM::new(code);
        for s in strings {
            vm.alloc_string(s.to_string());
        }
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(
            vm.heap.last().unwrap(),
            &HeapValue::String("1, 2, 3".into())
        );
    }

    #[test]
    fn arr_get_oob() {
        let code = vec![
            Push(n(10.0)),
            ArrNew(1),
            Push(n(5.0)), // index 5, out of bounds
            ArrGet,       // returns Null
        ];
        assert_eq!(run(code), vec![null()]);
    }

    #[test]
    fn arr_set_oob() {
        let code = vec![
            Push(n(10.0)),
            ArrNew(1),
            Push(n(5.0)),  // index
            Push(n(99.0)), // value
            ArrSet,        // pops: value, index, arr_ptr
        ];
        assert!(matches!(run_err(code), VMError::ValueError));
    }

    // ── object operations ─────────────────────────────────────────

    #[test]
    fn obj_new_get_set() {
        // Left-to-right: fields ["a","b"], values pushed in field order.
        // Push a-val (20), push b-val (10) → a=20, b=10
        let mut vm = VM::new(vec![
            Push(n(20.0)), // "a" value (first field, pushed first)
            Push(n(10.0)), // "b" value (second field)
            ObjNew(vec!["a".into(), "b".into()]),
            Push(s(0)), // field "a" (heap[0]="a")
            ObjGetDyn,  // pops field_ptr, obj_ptr → pushes obj["a"]
        ]);
        vm.alloc_string("a".to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        // ObjGet consumes obj_ptr, so stack only has the retrieved value.
        assert_eq!(vm.stack, vec![n(20.0)]);
    }

    #[test]
    fn obj_get_set_dynamic() {
        // Test dynamic ObjGetDyn with a heap-allocated field name.
        let mut vm = VM::new(vec![
            Push(n(1.0)),
            Push(n(2.0)),
            ObjNew(vec!["x".into(), "y".into()]), // x=1, y=2
            Push(s(0)),                           // field "x" (heap[0]="x")
            ObjGetDyn,                            // → 1
        ]);
        vm.alloc_string("x".to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(vm.stack, vec![n(1.0)]);

        // Test dynamic ObjSetDyn: set a field, verify with ObjGet.
        let mut vm = VM::new(vec![
            Push(n(1.0)),
            Push(n(2.0)),
            ObjNew(vec!["x".into(), "y".into()]), // x=1, y=2
            Dup,                                  // keep ptr for verification
            Push(s(0)),                           // field "y" (heap[0]="y") — pushed before val
            Push(n(99.0)),                        // val — on top
            ObjSetDyn,                            // pops val, field_ptr, obj_ptr → obj.y = 99
            // Stack: [ptr]
            ObjGet("y".into()), // → 99
        ]);
        vm.alloc_string("y".to_string());
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
            Push(n(2.0)),                         // x value
            Push(n(1.0)),                         // y value
            ObjNew(vec!["x".into(), "y".into()]), // x=2, y=1
            Dup,                // keep ptr for verification after ObjSet consumes one
            Push(n(99.0)),      // value to set
            ObjSet("x".into()), // obj.x = 99, consumes one ptr
            ObjGet("x".into()), // → 99
        ];
        assert_eq!(run(code), vec![n(99.0)]);
    }

    #[test]
    fn obj_get_missing_key() {
        let code = vec![
            Push(n(1.0)),
            ObjNew(vec!["x".into()]),
            ObjGet("no_such_key".into()), // returns Null, ptr consumed
        ];
        assert_eq!(run(code), vec![null()]);
    }

    // ── string operations ─────────────────────────────────────────

    #[test]
    fn str_split() {
        // Verify split produces correct array contents.
        let mut vm = VM::new(vec![Push(s(0)), Push(s(1)), StrSplit(0)]);
        vm.alloc_string("a,b,c".to_string());
        vm.alloc_string(",".to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(vm.stack.len(), 1);
        if let StackValue::Ptr(p) = vm.stack[0] {
            match &vm.heap[p as usize] {
                HeapValue::Array(arr) => assert_eq!(arr.len(), 3),
                _ => panic!("expected array"),
            }
        } else {
            panic!("expected pointer");
        }
    }

    #[test]
    fn str_split_with_limit() {
        let code = vec![Push(s(0)), Push(s(1)), Push(n(2.0)), StrSplit(1)];
        let mut vm = VM::new(code);
        vm.alloc_string("a,b,c".to_string());
        vm.alloc_string(",".to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        // Should split into at most 2 parts
        if let StackValue::Ptr(p) = vm.stack[0] {
            if let HeapValue::Array(arr) = &vm.heap[p as usize] {
                assert_eq!(arr.len(), 2);
            } else {
                panic!("expected array");
            }
        } else {
            panic!("expected pointer");
        }
    }

    #[test]
    fn str_includes_starts_ends() {
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), StrIncludes(0)],
                &["hello world", "world"]
            ),
            vec![b(true)]
        );
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), StrStartsWith],
                &["hello world", "hello"]
            ),
            vec![b(true)]
        );
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), StrEndsWith],
                &["hello world", "world"]
            ),
            vec![b(true)]
        );
    }

    #[test]
    fn str_index_of() {
        // "hello hello" — first "hello" at 0
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), StrIndexOf(0)],
                &["hello hello", "hello"]
            ),
            vec![n(0.0)]
        );
        // "hello hello" with start=1 — second "hello" at 6
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), Push(n(1.0)), StrIndexOf(1)],
                &["hello hello", "hello"]
            ),
            vec![n(6.0)]
        );
        // Not found
        assert_eq!(
            run_heap(vec![Push(s(0)), Push(s(1)), StrIndexOf(0)], &["abc", "xyz"]),
            vec![n(-1.0)]
        );
    }

    #[test]
    fn str_last_index_of() {
        // "hello hello" — last "hello" at 6
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), StrLastIndexOf(0)],
                &["hello hello", "hello"]
            ),
            vec![n(6.0)]
        );
        // "hello hello" with start=5 — search backwards from index 5, finds at 0
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), Push(n(5.0)), StrLastIndexOf(1)],
                &["hello hello", "hello"]
            ),
            vec![n(0.0)]
        );
        // Not found
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), StrLastIndexOf(0)],
                &["abc", "xyz"]
            ),
            vec![n(-1.0)]
        );
    }

    #[test]
    fn str_slice() {
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(n(0.0)), Push(n(5.0)), StrSlice],
                &["hello world"]
            ),
            vec![s(1)] // "hello" at heap[1]
        );
        // Verify the sliced string
        let mut vm = VM::new(vec![StrSlice]);
        vm.heap.push(HeapValue::String("hello world".into()));
        vm.stack.push(s(0));
        vm.stack.push(n(0.0));
        vm.stack.push(n(5.0));
        match vm.step().unwrap() {
            StepResult::Done => {}
            _ => panic!(),
        }
        assert_eq!(vm.heap[1], HeapValue::String("hello".into()));
    }

    #[test]
    fn str_slice_bounds_error() {
        let mut vm = VM::new(vec![Push(s(0)), Push(n(0.0)), Push(n(999.0)), StrSlice]);
        vm.alloc_string("hi".to_string());
        assert!(matches!(vm.step().unwrap_err(), VMError::ValueError));
    }

    #[test]
    fn str_trim() {
        assert_eq!(run_heap(vec![Push(s(0)), StrTrim], &["  hi  "]), vec![s(1)]);
    }

    #[test]
    fn str_to_int_float() {
        // StrToInt yields a lossless Int; StrToFloat yields a Number.
        assert_eq!(run_heap(vec![Push(s(0)), StrToInt], &["42"]), vec![i(42)]);
        assert_eq!(
            run_heap(vec![Push(s(0)), StrToFloat], &["3.14"]),
            vec![n(3.14)]
        );
    }

    #[test]
    fn str_to_int_error() {
        let mut vm = VM::new(vec![Push(s(0)), StrToInt]);
        vm.alloc_string("abc".to_string());
        assert!(matches!(vm.step().unwrap_err(), VMError::ValueError));
    }

    #[test]
    fn str_to_json_from_json() {
        // Parse JSON string, then serialize back
        let code = vec![Push(s(0)), StrToJson, StrFromJson];
        let mut vm = VM::new(code);
        vm.alloc_string(r#"{"a":1,"b":[2,3]}"#.to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => {}
            }
        }
        assert_eq!(
            vm.heap.last().unwrap(),
            &HeapValue::String(r#"{"a":1,"b":[2,3]}"#.into())
        );
    }

    // ── effects ───────────────────────────────────────────────────

    #[test]
    fn invoke_yields() {
        match run_effect(vec![
            Push(n(1.0)),
            Push(n(2.0)),
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
            Push(n(10.0)),
            Push(n(3.0)),
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
            Push(n(1.0)), // a's arg 0
            Push(n(2.0)), // a's arg 1
            Push(n(3.0)), // b's arg 0
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
            Push(n(1.0)),
            Invoke("a".into(), 1),
            Push(n(2.0)),
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
        assert_eq!(run(vec![Push(n(-0.0)), Push(n(0.0)), Eq]), vec![b(true)]);
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
        let mut vm = VM::new(vec![Push(n(1.0)), Push(n(2.0)), Add]);
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
            run_err(vec![Push(s(99)), ArrLength]),
            VMError::ValueError
        ));
        // Type predicates stay total (false) on a dangling pointer.
        assert_eq!(run(vec![Push(s(99)), IsStr]), vec![b(false)]);
        assert_eq!(run(vec![Push(s(99)), IsArr]), vec![b(false)]);
        // Equality with a dangling pointer is simply not-equal, no panic.
        assert_eq!(run(vec![Push(s(99)), Push(s(99)), Eq]), vec![b(false)]);
    }

    #[test]
    fn str_slice_rejects_non_char_boundary() {
        // "é" is two UTF-8 bytes; slicing at byte 1 splits the codepoint.
        let mut vm = VM::new(vec![Push(s(0)), Push(n(0.0)), Push(n(1.0)), StrSlice]);
        vm.alloc_string("é".to_string());
        assert!(matches!(vm.step().unwrap_err(), VMError::ValueError));
    }

    #[test]
    fn str_includes_non_ascii_no_panic() {
        // start offset in the middle of a codepoint -> false, not a panic.
        assert_eq!(
            run_heap(
                vec![Push(s(0)), Push(s(1)), Push(n(1.0)), StrIncludes(1)],
                &["é", "x"]
            ),
            vec![b(false)]
        );
    }

    #[test]
    fn values_equal_is_deep() {
        // Two arrays built from independently-allocated strings must compare
        // equal by content (regression: derived PartialEq compared Ptrs).
        let code = vec![
            Push(s(0)),
            ArrNew(1),
            Push(s(1)),
            ArrNew(1),
            Eq, // ["abc"] == ["abc"] with distinct heap addresses
        ];
        assert_eq!(run_heap(code, &["abc", "abc"]), vec![b(true)]);
        // Differing content compares unequal.
        let code = vec![Push(s(0)), ArrNew(1), Push(s(1)), ArrNew(1), Eq];
        assert_eq!(run_heap(code, &["abc", "xyz"]), vec![b(false)]);
    }

    #[test]
    fn incremental_alloc_allowed() {
        // Two successive Allocs in a function should both succeed and yield
        // independent locals.
        let code = vec![
            Call(2, 0),
            Return(1), // propagate the function's result to the final stack
            Alloc(plain(1)),  // local 0
            Alloc(plain(1)),  // local 1 (was previously rejected)
            Push(n(7.0)),
            SetLocal(0),
            Push(n(8.0)),
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
            run_err(vec![Push(n(1.0)), Push(n(64.0)), BitLhs]),
            VMError::ValueError
        ));
        assert!(matches!(
            run_err(vec![Push(n(1.0)), Push(n(-1.0)), BitRhs]),
            VMError::ValueError
        ));
        // Valid shifts still work.
        assert_eq!(run(vec![Push(n(1.0)), Push(n(3.0)), BitLhs]), vec![n(8.0)]);
    }

    #[test]
    fn json_depth_is_bounded() {
        // Build JSON nested deeper than MAX_JSON_DEPTH; parsing must error
        // rather than overflow the native stack.
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        let mut vm = VM::new(vec![Push(s(0)), StrToJson]);
        vm.alloc_string(deep);
        assert!(matches!(vm.step().unwrap_err(), VMError::ValueError));
    }

    #[test]
    fn nan_serializes_as_null() {
        let mut vm = VM::new(vec![Push(n(f64::NAN)), StrFromJson]);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(vm.heap.last().unwrap(), &HeapValue::String("null".into()));
    }

    #[test]
    fn pop_respects_frame_floor() {
        // fn: 1 arg, 1 local, no temporaries. Pop must not steal a local.
        let code = vec![
            Push(n(1.0)),
            Call(3, 1),
            Return(0),
            Alloc(plain(1)), // local 0; sp == frame floor
            Pop(1),   // nothing above the floor -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn dup_cannot_duplicate_local() {
        let code = vec![
            Push(n(1.0)),
            Call(3, 1),
            Return(0),
            Alloc(plain(1)), // local 0; sp == floor
            Dup,      // nothing above the floor -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn swap_cannot_cross_frame_floor() {
        // One local + one temporary: Swap needs two temporaries above the
        // floor, but only one exists.
        let code = vec![
            Push(n(1.0)),
            Call(3, 1),
            Return(0),
            Alloc(plain(1)),     // local 0
            Push(n(9.0)), // single temporary
            Swap,         // would swap the temp with the local -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
    }

    #[test]
    fn rot_cannot_cross_frame_floor() {
        let code = vec![
            Push(n(1.0)),
            Call(3, 1),
            Return(0),
            Alloc(plain(1)),     // local 0
            Push(n(8.0)), // two temporaries (need three for Rot)
            Push(n(9.0)),
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
            Push(n(5.0)),
            Call(3, 1),
            Return(1),
            Alloc(plain(1)), // local 0
            Push(n(10.0)),
            SetLocal(0),  // local 0 = 10
            Push(n(1.0)), // temporaries: [1, 2]
            Push(n(2.0)),
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
    fn int_survives_json_roundtrip() {
        // A 2^60 id exceeds f64's 53-bit mantissa; it must round-trip exactly.
        let big = 1i64 << 60; // 1152921504606846976
        let json = format!(r#"{{"id":{big}}}"#);
        let mut vm = VM::new(vec![
            Push(s(0)),
            StrToJson,
            ObjGet("id".into()),
            // round-trip back out and confirm the textual form is preserved
            Dup,
            StrFromJson,
        ]);
        vm.alloc_string(json);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        // stack: [PosInt(big), Ptr(serialized)]
        assert_eq!(vm.stack[0], i(big));
        if let StackValue::Ptr(p) = vm.stack[1] {
            assert_eq!(vm.heap[p as usize], HeapValue::String(big.to_string()));
        } else {
            panic!("expected serialized string");
        }
    }

    #[test]
    fn u64_above_i64_max_survives_roundtrip() {
        // Values in the 2^63..2^64 band would corrupt as f64 or fail to fit
        // i64; PosInt carries them exactly. u64::MAX = 18446744073709551615.
        let big = u64::MAX;
        let json = format!(r#"{{"hash":{big}}}"#);
        let mut vm = VM::new(vec![
            Push(s(0)),
            StrToJson,
            ObjGet("hash".into()),
            Dup,
            StrFromJson,
        ]);
        vm.alloc_string(json);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(vm.stack[0], u(big));
        if let StackValue::Ptr(p) = vm.stack[1] {
            assert_eq!(vm.heap[p as usize], HeapValue::String(big.to_string()));
        } else {
            panic!("expected serialized string");
        }
    }

    #[test]
    fn negative_integers_are_negint_and_roundtrip() {
        // Negative JSON integers map to NegInt and serialize back exactly.
        let mut vm = VM::new(vec![Push(s(0)), StrToJson, ObjGet("x".into())]);
        vm.alloc_string(r#"{"x":-42}"#.to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(vm.stack, vec![i(-42)]);
        assert_eq!(vm.stack, vec![StackValue::NegInt(-42)]);
        // PosInt and NegInt never compare equal even at the boundary value 0
        // representations (different sign domains).
        assert_eq!(run(vec![Push(u(5)), Push(i(-5)), Eq]), vec![b(false)]);
        // Ordering across the sign boundary is structural.
        assert_eq!(run(vec![Push(i(-1)), Push(u(u64::MAX)), Lt]), vec![b(true)]);
    }

    #[test]
    fn posint_too_large_for_index_errors() {
        // A PosInt beyond i64::MAX can't be an array index -> error, no panic.
        let code = vec![Push(n(1.0)), ArrNew(1), Push(u(u64::MAX)), ArrGet];
        assert!(matches!(run_err(code), VMError::TypeError));
    }

    #[test]
    fn json_parses_integers_as_int_and_fractions_as_number() {
        let mut vm = VM::new(vec![Push(s(0)), StrToJson, ObjGet("a".into())]);
        vm.alloc_string(r#"{"a":7,"b":7.5}"#.to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(vm.stack, vec![i(7)]);

        let mut vm = VM::new(vec![Push(s(0)), StrToJson, ObjGet("b".into())]);
        vm.alloc_string(r#"{"a":7,"b":7.5}"#.to_string());
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(vm.stack, vec![n(7.5)]);
    }

    #[test]
    fn int_arithmetic_degrades_to_number() {
        // The transport guarantee is identity-preservation, NOT integer math:
        // any arithmetic promotes Int -> Number(f64).
        assert_eq!(run(vec![Push(i(2)), Push(i(3)), Add]), vec![n(5.0)]);
        assert_eq!(run(vec![Push(i(10)), Push(n(4.0)), Sub]), vec![n(6.0)]);
        assert_eq!(run(vec![Push(i(10)), Push(i(3)), Mod]), vec![n(1.0)]);
        assert_eq!(run(vec![Push(i(5)), Neg]), vec![n(-5.0)]);
    }

    #[test]
    fn int_number_cross_comparison() {
        // 1 == 1.0, ordering works across Int/Number.
        assert_eq!(run(vec![Push(i(1)), Push(n(1.0)), Eq]), vec![b(true)]);
        assert_eq!(run(vec![Push(i(2)), Push(n(2.5)), Lt]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(3.0)), Push(i(3)), GtEq]), vec![b(true)]);
        assert_eq!(run(vec![Push(i(2)), Push(i(2)), Eq]), vec![b(true)]);
    }

    #[test]
    fn int_type_predicates() {
        assert_eq!(run(vec![Push(i(5)), IsInt]), vec![b(true)]);
        assert_eq!(run(vec![Push(i(5)), IsNum]), vec![b(true)]);
        assert_eq!(run(vec![Push(i(5)), IsFloat]), vec![b(false)]);
        // integer-valued Number is still "int"; fractional Number is "float"
        assert_eq!(run(vec![Push(n(5.0)), IsInt]), vec![b(true)]);
        assert_eq!(run(vec![Push(n(5.5)), IsFloat]), vec![b(true)]);
    }

    #[test]
    fn int_indices_and_bitops() {
        // Int works directly as an array index (left-to-right: first = arr[0]).
        let code = vec![
            Push(n(10.0)),
            Push(n(20.0)),
            ArrNew(2), // [10, 20]
            Push(i(1)),
            ArrGet,
        ];
        assert_eq!(run(code), vec![n(20.0)]);
        // ...and as a bitwise operand.
        assert_eq!(run(vec![Push(i(10)), Push(i(12)), BitAnd]), vec![n(8.0)]);
    }

    #[test]
    fn str_split_negative_limit_errors() {
        let mut vm = VM::new(vec![Push(s(0)), Push(s(1)), Push(n(-1.0)), StrSplit(1)]);
        vm.alloc_string("a,b,c".to_string());
        vm.alloc_string(",".to_string());
        assert!(matches!(vm.step().unwrap_err(), VMError::ValueError));
    }
}

pub mod instr;
#[cfg(test)]
mod tests;
pub mod value;

// Re-exports so external paths (`crate::vm::Value` etc.) are unchanged.
pub use crate::rc_str::RcStr;
pub use instr::{
    ArrayPtr, CellIndex, ClosurePtr, CodeAddr, FieldName, Instr, LocalIndex, ObjectPtr, PromisePtr,
    SetMode, SlotKind, StackAddr, UpdateMode,
};
pub use value::Value;
pub(crate) use value::{float_is_int, js_number_to_string};

use std::sync::Arc;

use indexmap::IndexMap;
use smallvec::SmallVec;
use thin_vec::ThinVec;

use crate::compiler::Program;

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
    JS; "😀" is 4 here, 2 in JS). ASCII text is identical. Slicing at a
    mid-codepoint byte offset errors rather than coercing to a codepoint
    boundary.
  • Bitwise ops (`& | ^ << >> ~`) operate on full i64, not JS's 32-bit ToInt32
    semantics, and there is no unsigned right shift (`>>>`). Shift counts must be
    0..63 (JS masks to 0..31).
  • `s.split()`, `s.indexOf()`, etc. accept fewer arguments at runtime than the
    compiler's static arity check allows (JS-coerced defaults: absent needle →
    "undefined", absent start → 0). The compiler remains strict for the LLM's
    benefit; the runtime relaxes to match JS.
  • Number→string uses Rust's float formatting for the non-integer path, so very
    large/small magnitudes are not rendered in JS's exponential form (`1e21`).
  • No exceptions/try/catch/throw: the `Raise` condition mechanism is for host
    (LLM) intervention, not JS error handling.
  • Tool calls return *promises* (7_ASYNC Tier 1): `tools.f(args)` starts the
    call and pushes a promise; `await` is the only consumer. There is no
    `.then`/`.catch`/`.finally`, no `new Promise` (no executor pattern), and
    no `Promise.race`/`any`/`allSettled` — only `Promise.all`. `await x` on a
    non-promise passes it through, as in JS. Promises are transient values:
    identity-only `===`, "object" under `typeof`, no JSON form (reaching the
    persistence boundary errors with a missing-`await` hint, as does property
    access on a promise).
  • Tier 1 async limitation: an `async` function body runs synchronously on
    the caller's stack, so an `await` inside it blocks the whole program
    instead of suspending just that call (Tier 2 removes this). Consequently
    an async function returns its plain value, not a wrapped promise —
    observationally invisible, because `await` passes non-promises through
    and is the only promise consumer in this dialect.

*/

/// Default instruction budget for a freshly constructed VM. The host can
/// override `VM::fuel` before/after stepping. Chosen high enough that any
/// realistic orchestration program completes, low enough that a runaway
/// loop is caught in well under a second.
pub const DEFAULT_FUEL: u64 = 10_000_000;

/// Maximum nesting depth for JSON <-> value conversion. Bounds native
/// recursion so adversarial tool output cannot overflow the Rust stack.
const MAX_JSON_DEPTH: usize = 128;

pub struct VM {
    pub code: Vec<Instr>,
    pub arrays: Vec<ThinVec<Value>>,
    pub objects: Vec<IndexMap<FieldName, Value>>,
    pub closures: Vec<Closure>,
    /// Side table of captured bindings (cells). A `Boxed` local lives here so
    /// it has identity and outlives its frame; `Value::Upval` indexes it.
    /// Grows monotonically (no reclamation), like `heap`.
    pub cells: Vec<Value>,
    /// Promise heap, indexed by `Value::Promise(PromisePtr)`. Entries are
    /// allocated `Pending` by `Instr::Invoke` and transition exactly once to
    /// `Resolved`/`Rejected` via the host APIs `resolve_promise` /
    /// `reject_promise`. Grows monotonically, like the other heaps.
    pub promises: Vec<PromiseState>,
    /// Tool calls started (`Instr::Invoke`) but not yet handed to the host.
    /// Drained into `StepResult::Pending` when the program blocks on a
    /// pending promise, or into `StepResult::Done` (as `unstarted`) when the
    /// program finishes without awaiting them.
    outbox: Vec<InvokeCall>,
    pub stack: Vec<Value>, // sp == stack.len()
    pub callstack: Vec<CallFrame>,
    pub ip: CodeAddr,
    pub fp: StackAddr,
    /// Cache of the current (top) call frame's `local_count`, mirrored here so
    /// the hottest instructions (`Local`/`SetLocal`/`Pop`/`Pick`/`Dig`/… and
    /// `frame_floor`) read a plain field instead of chasing `callstack.last()`
    /// every time. Kept in sync wherever a frame's `local_count` is set:
    /// `Call`/`CallDyn` (→ nargs), `EnterFrame` (→ final count), and `Return`
    /// (→ the restored caller frame's count). `local_count` only changes at
    /// those four sites, so the mirror is always current.
    cur_local_count: u32,
    /// Remaining instruction budget. Decremented once per executed
    /// instruction across all `step()` calls; reaching zero yields
    /// `VMError::OutOfFuel`. The heap grows monotonically (no reclamation,
    /// by design — programs are expected to be short-lived), so this is the
    /// primary backstop against runaway execution.
    pub fuel: u64,
    /// Source byte offset per instruction (`spans[ip]`), populated by
    /// `for_program` from `Program::spans`. Empty when constructed via
    /// `VM::new` (hand-assembled instructions used by tests).
    pub spans: Vec<u32>,
    /// Source text the `spans` refer into, populated by `for_program` from
    /// `Program::source`. Empty for `VM::new` programs, where error
    /// rendering degrades gracefully to "at instruction N".
    pub source: Arc<str>,
    /// Size-capped ring buffer of console output lines (from `console.log`,
    /// `console.warn`, `console.error`, `console.info`). The host reads this
    /// for diagnostics; it is never a result channel. When the cap (256 lines)
    /// is exceeded, the oldest line is dropped and replaced by a
    /// `[... N lines dropped]` marker.
    pub console_lines: Vec<String>,
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
    pending_upvals: SmallVec<[Value; 8]>,
    /// Lazily-built, per-frame cache for the `arguments` array (its heap
    /// address). Built on the first `Instr::Arguments` in this frame and reused
    /// by later references, so repeated `arguments` uses don't re-materialize
    /// the array. `None` until first use (and for frames that never use it).
    arguments_cache: Option<ArrayPtr>,
}

/// A heap-allocated closure value: a code address plus its captured
/// environment. Built by `MakeClosure` and called through `CallDyn`.
#[derive(Clone, Debug, PartialEq)]
pub struct Closure {
    pub addr: CodeAddr,
    pub upvals: ThinVec<Value>,
}

/// State of one entry in the VM's `promises` heap. A promise is born
/// `Pending` (by `Instr::Invoke`) and transitions exactly once.
#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    /// Not yet settled. `waiters` is reserved for Tier 2 (suspended async
    /// continuations registered on this promise); unused in Tier 1, where the
    /// only waiter is the main strand re-executing its `Await`.
    Pending {
        waiters: Vec<u32>,
    },
    Resolved(Value),
    Rejected(Value),
}

#[derive(Debug)]
pub enum StepResult {
    /// Program completed (root frame returned). The `value` is the program's
    /// top-level return value, or `Undefined` when the program ends without a
    /// `return` statement. `unstarted` is the drained outbox: tool calls the
    /// program started but never awaited (fire-and-forget); the host decides
    /// whether to run or drop them — the program can no longer observe them.
    Done {
        value: Value,
        unstarted: Vec<InvokeCall>,
    },
    /// The program is blocked awaiting a still-pending promise. `calls` is
    /// the drained outbox: every tool call started since the last yield, each
    /// tagged with the promise it settles. The host performs calls (in any
    /// order / concurrently), settles at least one promise via
    /// `vm.resolve_promise(id, value)` / `vm.reject_promise(id, errval)`, and
    /// calls `step()` again; the blocking `Await` re-executes (`ip` is
    /// unchanged). `calls` can be empty when everything the program is
    /// waiting on was already handed over in an earlier `Pending`.
    Pending { calls: Vec<InvokeCall> },
    /// A condition was raised; host (LLM) decides how to proceed.
    /// The payload (if any) is the value passed to `raise("name", expr)`.
    /// ip has already advanced past the Raise instruction; the host may
    /// resume execution by pushing a replacement result value and calling
    /// `step()` again (or use `VM::resume_raise` for convenience).
    /// In-place code/ip patching is unsupported: live Fn/Closure values
    /// hold code addresses that a recompile invalidates. For complex
    /// restart scenarios, abandon this VM and run a rewritten program
    /// in a fresh VM (prior tool results stay available via the event log).
    Raise {
        condition: String,
        payload: Option<Value>,
    },
}

/// A single tool/function call requested by the program.
#[derive(Debug)]
pub struct InvokeCall {
    /// The promise this call settles: the host reports the call's outcome
    /// with `vm.resolve_promise(promise, value)` / `vm.reject_promise(...)`.
    pub promise: PromisePtr,
    pub name: String,
    /// Arguments in call order (`args[0]` is the first argument).
    pub args: Vec<Value>,
}

#[derive(Debug, PartialEq)]
pub enum ErrorKind {
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

/// Whether the host can resume from this error by feeding a value (see
/// `VM::resume_with`).
///
/// # Pop-first invariant
///
/// A `PushValueThenContinue` error has its instruction's operands
/// consumed before the error propagates out of `step()`, so
/// `resume_with` needs no per-instruction stack fixup — it just pushes
/// the replacement value and advances ip.
///
/// # Classification audit (instruction/site → mode → why)
///
/// | Site | Kind | Mode | Reason |
/// |------|------|------|--------|
/// | unary_num! / binary_num! / binary_int! / cmp_op! macros | TypeError | PushValueThenContinue | ops popped before coercion |
/// | Add (string concat path) | TypeError | PushValueThenContinue | both ops popped before to_number |
/// | BitNot, ToNum, TypeOf | TypeError/ValueError | PushValueThenContinue | operand popped first |
/// | CallDyn non-callable | TypeError | PushValueThenContinue | callable popped, then args dropped before failing (pop-first normalization) |
/// | CallSpread non-callable / non-array args | TypeError | PushValueThenContinue | callable+array popped first; dispatch same as CallDyn |
/// | IndexGet (non-container, bad index, mid-codepoint) | TypeError/ValueError | PushValueThenContinue | container+key popped first |
/// | IndexSet (non-container, negative/OOB index) | TypeError/ValueError | PushValueThenContinue | val+key+container popped first |
/// | ObjHas / ObjDelete (non-object) | TypeError | PushValueThenContinue | field+object popped first |
/// | Builtin handlers (args truncated by `Builtin::call` epilogue on the error path) | TypeError/ValueError | PushValueThenContinue | args truncated before error propagates |
/// | JSON depth / unsupported type (JSON.stringify, to_json) | ValueError | PushValueThenContinue | args popped by builtin before conversion |
/// | **ObjGet** (non-object peek) | TypeError | **NotResumable** | object peeked (not popped) before check |
/// | **ObjSet** (non-object peek) | TypeError | **NotResumable** | value popped, object peeked (not fully consumed) |
/// | **ObjExtend** (non-object src) | TypeError | PushValueThenContinue | both src+obj popped first |
/// | **ArrExtend** (non-array src) | TypeError | PushValueThenContinue | both src+arr popped first |
/// | **ArrPush** (non-array target) | TypeError | PushValueThenContinue | both val+arr popped first |
/// | Await (rejected promise) | ValueError | PushValueThenContinue | promise popped before failing; host may substitute a value for the rejection |
/// | Await (bad promise pointer) | ValueError | **NotResumable** | corrupt heap = invariant violation |
/// | **IncLocal** (non-numeric local) | TypeError | **NotResumable** | reads local by peek (no stack consumption) |
/// | bad heap/cell pointer (`get`/`get_mut` on arrays/objects/cells/closures) | TypeError/ValueError | **NotResumable** | corrupt heap = invariant violation; some sites also have no result slot (SetLocal) |
/// | Raise with argc > 1 | BadArg | NotResumable | instruction contract violated (compiler emits 0 or 1) |
/// | StackUnderflow, BadReturn, BadCall, BadAlloc, BadArg, BadLocal | — | NotResumable | invariant violation / compiler bug |
/// | OutOfFuel | — | RetrySameInstr | nothing consumed; refuel and retry |
///
/// All 9 `ErrorKind`s are covered. The bolded sites are the
/// TypeError/ValueError sites that error before full operand consumption
/// (or, for bad pointers, mid-mutation) and therefore must not be resumed.
#[derive(Debug)]
pub enum ResumeMode {
    /// Internal invariant broken (compiler bug / host misuse). Never resume.
    NotResumable,
    /// The failed instruction's operands were consumed; pushing a
    /// replacement result and advancing ip resumes as if it succeeded.
    PushValueThenContinue,
    /// Nothing was consumed; fix the budget/input and step() again
    /// (ip unchanged). Currently: OutOfFuel.
    RetrySameInstr,
}

#[derive(Debug)]
pub struct VMError {
    pub kind: ErrorKind,
    pub ip: CodeAddr,
    pub message: String,
    pub resume: ResumeMode,
}

impl VMError {
    /// Construct a `NotResumable` error at a given ip without borrowing the
    /// VM, for sites where a mutable borrow is already active (e.g. inside
    /// `ok_or_else` on a `get_mut`). Every such site is a bad heap/cell
    /// pointer — an invariant violation — so this is always `NotResumable`;
    /// resumable errors must go through `VM::fail`, which can see the
    /// stack state the pop-first invariant depends on.
    pub fn fail_at(ip: CodeAddr, kind: ErrorKind, msg: impl Into<String>) -> Self {
        VMError {
            kind,
            ip,
            message: msg.into(),
            resume: ResumeMode::NotResumable,
        }
    }
}

// ── submodules ──────────────────────────────────────────────

mod methods;
mod step;

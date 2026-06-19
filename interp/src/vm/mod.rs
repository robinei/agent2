pub mod instr;
#[cfg(test)]
mod tests;
pub mod value;

// Re-exports so external paths (`crate::vm::Value` etc.) are unchanged.
pub use crate::rc_str::RcStr;
pub use instr::{
    ArrayPtr, BufferPtr, CellIndex, ClosurePtr, CodeAddr, DataViewEntry, DataViewPtr, FieldName,
    GlobalId, Instr, LocalIndex, MapPtr, ObjectPtr, PromisePtr, SetMode, SetPtr, SlotKind,
    StackAddr, TypePrototype, TypeTag, TypedArrayKind, TypedArrayPtr, TypedArrayView, UpdateMode,
};
pub use value::Value;
pub(crate) use value::{MapKey, float_is_int, js_number_to_string};

use std::collections::VecDeque;
use std::sync::Arc;

use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};
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
  • Bitwise ops (`& | ^ << >> >>> ~`) operate on full i64, not JS's 32-bit
    ToInt32 semantics. Shift counts must be 0..63 (JS masks to 0..31).
    `>>>` is an unsigned (zero-fill) right shift on 64-bit values.
  • `s.split()`, `s.indexOf()`, etc. accept fewer arguments at runtime than the
    compiler's static arity check allows (JS-coerced defaults: absent needle →
    "undefined", absent start → 0). The compiler remains strict for the LLM's
    benefit; the runtime relaxes to match JS.
  • Number→string uses Rust's float formatting for the non-integer path, so very
    large/small magnitudes are not rendered in JS's exponential form (`1e21`).
  • A method call on a builtin-method *name* has its arity checked **only at
    runtime**, not at compile time — the receiver type isn't known when
    compiling (it may be an `Object` whose own/proto property shadows the builtin
    at a different arity). So unlike namespace/global builtins (`Math.max`…),
    which keep the strict compile-time arity lint, a method-name call always
    lowers to a builtin call: a matching structural receiver runs the builtin
    with its lenient, JS-faithful behavior (extra args ignored, missing optional
    args default — e.g. `'abc'.split()` is `['abc']`), and an `Object` receiver
    reroutes to the shadowing property.
  • `f.call`/`f.apply` are recognized as invocation forwarders (lowered to the
    `has_this` dispatch), so a plain object cannot shadow them with its own
    `call`/`apply` method — the receiver is always treated as the function being
    invoked. (`f.apply`'s args array may be nullish → no args, as in JS.)
  • A spread call over a nullish value — `f(...null)` / `f(...undefined)` — passes
    no args rather than throwing (JS: "not iterable"). Consistent with the VM's
    other nullish-spread leniencies (object rest, `.apply(t, null)`).
  • Exceptions: `throw`/`try`/`catch`/`finally` are supported with full JS
    completion-value semantics (6_LANGUAGE Part B + B2): `break`/
    `continue`/`return` crossing a `finally` boundary run the block on the
    way out, and a jump/return/throw from inside a `finally` overrides the
    pending completion, exactly as in JS (implemented by per-exit-path code
    duplication, not completion records). `raise()` is NOT catchable —
    conditions are addressed to the LLM — and neither are internal invariant
    errors, so a program cannot trap its own kill switch. (Fuel exhaustion
    is not even an error: `step(fuel)` running dry yields
    `StepResult::OutOfFuel` to the host, invisible to the program.)
    A caught runtime VM error materializes as a plain `{ name, message }`
    object (message = the rendered diagnostic with line/col + source line);
    `new Error(msg)` (and the standard subclass names) builds that same
    shape — no general `new` machinery is implied. `await` of a rejected
    promise inside `try` delivers the raw rejection value to `catch`, as in
    JS.
  • Tool calls return *promises* (7_ASYNC Tier 1): `tools.f(args)` starts the
    call and pushes a promise; `await` is the only consumer. There is no
    `.then`/`.catch`/`.finally`, no `new Promise` (no executor pattern), and
    no `Promise.race`/`any` — only `Promise.all` and `Promise.allSettled`
    (which never rejects and yields JS's `{ status, value/reason }` entries,
    with a non-promise element settling fulfilled). `await x` on a
    non-promise passes it through, as in JS. Promises are transient values:
    identity-only `===`, "object" under `typeof`, no JSON form (reaching the
    persistence boundary errors with a missing-`await` hint, as does property
    access on a promise).
  • Async functions are real (7_ASYNC Tier 2): a pending `await` inside an
    async function suspends just that call (frame snapshot, zero stack), the
    caller receives a promise and continues, and settled promises wake
    suspended calls through a deterministic FIFO ready queue drained at
    await points only. Two accepted divergences from JS: an async function
    that completes without ever suspending returns its plain value, not a
    wrapped promise (observationally invisible — `await` passes non-promises
    through and is the only promise consumer in this dialect); and a throw
    before the first suspension propagates synchronously to the caller
    instead of rejecting. After the first suspension, an uncaught
    throw/rejection rejects the call's promise, exactly as in JS. Circular
    awaits are detected and reported as a dedicated `Deadlock` error naming
    the await chain.
  • `instanceof` walks the `[[Prototype]]` chain for all RHS types (Step
    2b): `[] instanceof Array`, `m instanceof Map`, `f instanceof Function`,
    `x instanceof Object`, and `new F() instanceof F` all take one walk
    (no structural `TypeTag` fast path). Primitives are never `instanceof`
    anything (JS: "If Type(relObj) is not Object, return false").
    `instanceof Error` is unsupported — Error instances are plain `Object`s
    with no distinct tag or `Error.prototype` link.
  • `Function` is a constructor *value* for reflection (`typeof Function ===
    "function"`, `Map instanceof Function`,
    `Object.getPrototypeOf(Array) === Function.prototype`), but neither `new
    Function(body)` nor `Function(body)` is supported — both throw (a
    documented divergence; function expressions are the alternative).
  • `Object.getPrototypeOf` returns the real `[[Prototype]]` for any value
    (Step 2b): primitives return their wrapper type's prototype
    (`Object.getPrototypeOf(5) === Number.prototype`), structural types
    return their type prototype (`Object.getPrototypeOf([]) ===
    Array.prototype`), constructors return `Function.prototype`. Plain
    objects chain to `Object.prototype` (`Object.getPrototypeOf({}) ===
    Object.prototype`). `null`/`undefined` are a `TypeError` (no wrapper
    coercion). Prototype get/set is *only* via `Object.get/setPrototypeOf`
    — the legacy (Annex B) `__proto__` accessor is not modeled, so
    `__proto__` is an ordinary string-keyed data property.

*/

/// Maximum nesting depth for JSON <-> value conversion. Bounds native
/// recursion so adversarial tool output cannot overflow the Rust stack.
const MAX_JSON_DEPTH: usize = 128;

/// Compiled regular expression (JS-compatible via the `regress` crate).
/// Stored behind `Rc` and cloned via refcount bump. The pattern is
/// immutable; `last_index` is the one mutable bit — the `/g` iteration
/// cursor (a byte offset), held in a `Cell` so a shared handle can advance
/// it without the borrow overhead/panic risk of a `RefCell`. Each *literal
/// evaluation* allocates a fresh instance (via the `RegExp` constructor
/// builtin), so the cursor never leaks across unrelated uses — matching JS
/// object identity.
#[derive(Debug)]
pub struct RegExpData {
    pub pattern: RcStr,
    pub flags: RcStr,
    pub compiled: regress::Regex,
    /// `lastIndex`: where the next `/g` `exec`/`test` resumes (bytes).
    pub last_index: std::cell::Cell<usize>,
}

/// Reference-counted handle to a [`RegExpData`]. `Clone` is a refcount bump.
/// `PartialEq` uses pointer identity (`Rc::ptr_eq`) — matching JS semantics
/// where `/a/ === /a/` is `false`.
#[derive(Clone, Debug)]
pub struct RcRegExp(Rc<RegExpData>);

impl PartialEq for RcRegExp {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for RcRegExp {
    type Target = RegExpData;
    fn deref(&self) -> &RegExpData {
        &self.0
    }
}

impl RcRegExp {
    pub fn new(data: RegExpData) -> Self {
        RcRegExp(Rc::new(data))
    }
}

/// Whole-object integrity level (Step 2a). The same field dogfooded by
/// the builtin prototypes (constructed `Frozen`) is what the user-facing
/// `Object.freeze`/`seal`/`preventExtensions` write in Step 2d — one
/// mechanism, so the internal guarantee and the public surface are
/// provably the same code. A three-state enum (not a bool) because `seal`
/// sits between `preventExtensions` and `freeze`: `Sealed` forbids add +
/// delete but allows modifying existing keys, `Frozen` forbids all three.
/// Coarse / whole-object only — descriptor-accurate per-property
/// `writable`/`configurable` is the Step-4 tier.
///
/// The progression: `Extensible` → `NonExtensible` (cannot add) → `Sealed`
/// (cannot add or delete) → `Frozen` (cannot add, delete, or modify). Each
/// step is a strictly stronger restriction.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum IntegrityLevel {
    /// Default: add / modify / delete all permitted.
    #[default]
    Extensible,
    /// `Object.preventExtensions`: no new keys; modify + delete permitted.
    NonExtensible,
    /// `Object.seal`: no add, no delete; modify-existing permitted.
    Sealed,
    /// `Object.freeze` / builtin prototypes: no add, no delete, no modify.
    Frozen,
}

/// What kind of `Object` an `ObjData` is (Step 2a). Distinguishes the
/// reflective artifacts this phase introduces (which have **no JSON form**
/// per the invariant boundary) from ordinary user objects (which
/// serialize as data). `BuiltinNamespace` (Step 2a Part 2: `Math`, `JSON`)
/// is a frozen non-callable plain object carrying its statics/constants as
/// own properties; `BuiltinPrototype` is the frozen per-type prototype with
/// an empty map (methods are virtual rungs).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ObjKind {
    /// A plain user object — serializes to JSON.
    #[default]
    Ordinary,
    /// A frozen builtin prototype (`Array.prototype`, …) — no JSON form.
    BuiltinPrototype,
    /// A frozen builtin namespace (`Math`, `JSON`) — no JSON form. Carries
    /// its statics/constants as own properties (materialized, since the
    /// namespace is a plain object not a constructor — methods on it like
    /// `Math.max` are `Value::Builtin` entries).
    BuiltinNamespace,
}

/// An object stored in the `objects` heap. The `map` is the own-property
/// insertion-ordered store; `proto` is the optional prototype link (only
/// `Object` receivers carry a proto — primitives, arrays, maps, and sets
/// keep their structural builtin dispatch). `proto: None` is the common
/// case (plain object literals, `new F()` before `.prototype` is given a
/// proto, and all objects created by the existing VM code); only `proto:
/// Some(_)` triggers the chain walk in `ObjGet`/`ObjHas`. `integrity` is
/// the whole-object freeze/seal level (Step 2a); `kind` marks the
/// non-serializable reflective artifacts. Both default so `..Default::default()`
/// keeps existing construction sites untouched.
#[derive(Debug, Default)]
pub struct ObjData {
    pub proto: Option<ObjectPtr>,
    pub map: IndexMap<FieldName, Value>,
    pub integrity: IntegrityLevel,
    pub kind: ObjKind,
}

pub struct VM {
    pub code: Vec<Instr>,
    pub arrays: Vec<ThinVec<Value>>,
    pub objects: Vec<ObjData>,
    pub closures: Vec<Closure>,
    /// Map heap, indexed by `Value::Map(MapPtr)`. Each entry is an
    /// insertion-ordered map with SameValueZero key equality.
    pub maps: Vec<IndexMap<MapKey, Value>>,
    /// Set heap, indexed by `Value::Set(SetPtr)`. Each entry is an
    /// insertion-ordered set with SameValueZero equality.
    pub sets: Vec<IndexSet<MapKey>>,
    /// ArrayBuffer heap, indexed by `Value::ArrayBuffer(BufferPtr)`. Each entry
    /// is the owned byte buffer for typed arrays and DataViews to view into.
    pub buffers: Vec<Vec<u8>>,
    /// TypedArray heap, indexed by `Value::TypedArray(TypedArrayPtr)`. Each
    /// entry is a view over an ArrayBuffer at a specific offset and length.
    pub typed_arrays: Vec<TypedArrayView>,
    /// DataView heap, indexed by `Value::DataView(DataViewPtr)`. Each entry
    /// wraps an ArrayBuffer for byte-level get/set access.
    pub data_views: Vec<DataViewEntry>,
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
    /// Suspended async function calls (7_ASYNC Tier 2): each pending `await`
    /// inside an async frame copies that frame off the stack into a
    /// continuation record here. A slot is `None` once its continuation has
    /// been resumed (each suspension allocates a fresh slot); the heap grows
    /// monotonically like the others.
    continuations: Vec<Option<Continuation>>,
    /// Deterministic FIFO ready queue: continuations whose awaited promise
    /// has settled, paired with the settlement they receive on resume.
    /// Drained at await points only (no preemption), so the host's promise
    /// resolution order is the sole nondeterminism source (commitment 6).
    ready: VecDeque<(u32, ResumePayload)>,
    /// `ip` of the root strand's blocking `Await` while continuations run
    /// above the parked root region; the scheduler parks back here when the
    /// ready queue empties. Only meaningful while a strand is live — the
    /// root sets it immediately before launching one.
    root_ip: CodeAddr,
    /// Tool calls handed to the host (via `StepResult::Pending`) whose
    /// promise the host has not yet settled. When the root strand blocks
    /// with the ready queue, outbox, AND this all empty, no settlement can
    /// ever arrive: deadlock (only reachable via circular awaits).
    inflight: usize,
    /// Active `try` handlers, innermost last (see [`Instr::TryEnter`]). A
    /// throw — or a catchable runtime error — unwinds to the top entry;
    /// `TryExit` pops it on the normal path. The compiler guarantees entries
    /// never outlive their frame (it emits `TryExit`s on every jump out of a
    /// `try` block, including `return`).
    handlers: Vec<HandlerEntry>,
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
    /// Debug table (function names, source ranges, slot names — 9_TUI),
    /// populated by `for_program` from `Program::debug`. Empty (no
    /// entries) for `VM::new` programs; the introspection accessors
    /// degrade gracefully.
    pub debug: crate::debuginfo::DebugTable,
    /// Per-type frozen builtin prototype side table (Step 2a, 3), indexed by
    /// `TypeTag as usize`. Each entry holds the lazily-allocated prototype
    /// object and the `overridden` mutability-seam flag (Step 3). `None`
    /// prototype ptr until first reflective touch (lazy), so startup and the
    /// hot path pay nothing — `Vec::new()` at construction is zero-alloc and
    /// the table grows only when a prototype is actually consulted. The
    /// prototype itself is a frozen `ObjData` with `kind: BuiltinPrototype`
    /// (no JSON form) and an empty `map` (methods are virtual rungs resolved
    /// from the `builtins!` registry, not materialized — Step 2a Part 2's
    /// enumerability requirement). All prototypes chain to `Object.prototype`
    /// (which chains to `null`).
    pub prototypes: Vec<TypePrototype>,
    /// Per-namespace frozen object side table (Step 2a Part 2), indexed by
    /// `GlobalId as usize`. `None` until first reflective touch (lazy), so
    /// the bare identifier `Math` and the fast path `Math.max(…)` pay
    /// nothing unless the namespace object is actually read as a value. The
    /// object is a frozen `ObjData` with `kind: BuiltinNamespace` (no JSON
    /// form) carrying its statics/constants as own properties.
    pub namespaces: Vec<Option<ObjectPtr>>,
}

/// A read-only view of one live call frame, for the debugger
/// (`VM::frames`). Innermost-last, matching the callstack.
pub struct FrameView<'a> {
    /// The owning function's debug-table index and entry, when the program
    /// carries debug info.
    pub fn_index: Option<usize>,
    pub fn_debug: Option<&'a crate::debuginfo::FnDebug>,
    /// Base stack index of the frame (arg 0 / local 0).
    pub fp: usize,
    /// The frame's locals: `[params | upvals | own locals | self? |
    /// spill?]`. Boxed slots hold their `Upval` cell marker, not the
    /// value; render via `VM::cells` if needed.
    pub locals: &'a [Value],
    /// Expression temporaries above the locals (for the top frame, up to
    /// the stack top; below, up to the next frame's base).
    pub temps: &'a [Value],
}

impl FrameView<'_> {
    /// The owning function's name, or `"<unknown>"` without debug info.
    pub fn name(&self) -> &str {
        self.fn_debug.map_or("<unknown>", |f| f.name.as_str())
    }

    /// The declared name of local slot `i`, when known.
    pub fn local_name(&self, i: usize) -> Option<&str> {
        self.fn_debug?.slot_names.get(i)?.as_deref()
    }
}

pub struct CallFrame {
    arg_count: u32,
    local_count: u32,
    return_addr: CodeAddr,
    prev_fp: StackAddr,
    /// Extra call-group slots sitting *below* `fp` that belong to this call but
    /// are not args — the callee value and/or the receiver, left in place by the
    /// caller (read, not shifted away). `Return` reclaims them by truncating to
    /// `fp - reclaim_below` instead of `fp`. `0` for bare-address `Call` (no
    /// value on the stack); `1` for a plain `CallDyn`/reroute (callee or
    /// receiver); `2` for a method `CallDyn`/`CallSpread` (receiver + callee).
    reclaim_below: u32,
    /// A closure's captured environment, stashed by `CallDyn`/`Call` and
    /// installed as the callee's upval locals by the prologue `EnterFrame`
    /// (after the arg region is normalized to `nparams`). Points into
    /// `closures`; the upvals are read lazily and only when the callee's
    /// static upval count > 0. `u32::MAX` for bare-address calls and
    /// non-capturing canonical closures (their heap entry has empty upvals).
    pending_closure: ClosurePtr,
    /// Lazily-built, per-frame cache for the `arguments` array (its heap
    /// address). Built on the first `Instr::Arguments` in this frame and reused
    /// by later references, so repeated `arguments` uses don't re-materialize
    /// the array. `None` until first use (and for frames that never use it).
    arguments_cache: Option<ArrayPtr>,
    /// The `this` binding for this function call, stored outside the capturable
    /// local-slot space. Defaults to `Undefined`; set by method dispatch,
    /// `new`, `bind`, `.call`, and `.apply` (Phase 13 OO).
    pub(super) this_val: Value,
    /// The allocated instance object for a `new` call (Phase 13 OO). Set by
    /// `Instr::New` on the *caller* frame and consumed by `Instr::NewReturn`
    /// for the post-return fixup (if the constructor returns a non-object the
    /// instance is used instead). `None` for non-new calls.
    pub(super) new_obj: Option<ObjectPtr>,
    /// How this frame completes (7_ASYNC Tier 2). Direct calls are `Normal`;
    /// a scheduler-resumed async frame is `ResolvePromise` — it has no
    /// caller below it on the stack.
    completion: Completion,
}

/// How a frame's `Return` completes (7_ASYNC Tier 2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Completion {
    /// Ordinary call: the return value lands on the caller's stack at `fp`
    /// and execution continues at `return_addr`.
    Normal,
    /// Scheduler-resumed async frame: `Return` resolves this promise with
    /// the return value (waking waiters) and falls through to the scheduler;
    /// an uncaught throw rejects it instead.
    ResolvePromise(PromisePtr),
}

/// A suspended async function call (7_ASYNC Tier 2): everything needed to
/// re-create the frame at the stack top once the awaited promise settles.
/// The load-bearing invariant is that a suspended computation occupies zero
/// stack — established at runtime by copying the frame region out here
/// (frame snapshotting), not by a compile-time state-machine transform.
pub(super) struct Continuation {
    /// The instruction after the suspending `Await` (which consumed its
    /// operand at suspension; resume pushes the settled value in its place).
    resume_ip: CodeAddr,
    /// The frame's stack region `[fp, sp)` at suspension — args, locals,
    /// temps — minus the awaited promise. Everything in it is fp-relative
    /// (`Local`/`Pick`/`Dig`; heap values are pointers into the global
    /// heaps), so the region survives relocation to any new stack top.
    saved_stack: Vec<Value>,
    arg_count: u32,
    local_count: u32,
    arguments_cache: Option<ArrayPtr>,
    /// This frame's own active `try` handlers at suspension, outermost
    /// first, with `stack_len` stored fp-relative for re-basing on resume.
    /// Handlers of caller frames stay on the live handler stack (those
    /// frames keep executing); dropping an unresumed continuation drops its
    /// saved handlers with it — nothing leaks.
    saved_handlers: Vec<SavedHandler>,
    /// The promise this call settles when it completes — created at first
    /// suspension (the caller received it as the call's return value) and
    /// carried through re-suspensions via `Completion::ResolvePromise`.
    promise: PromisePtr,
    /// The promise this continuation waits on (await-chain rendering).
    awaiting: PromisePtr,
    /// Source span of the suspending `await` (await-chain rendering).
    await_span: u32,
}

/// One re-basable handler-stack entry inside a [`Continuation`].
pub(super) struct SavedHandler {
    catch_ip: CodeAddr,
    /// `HandlerEntry::stack_len - fp` at suspension.
    rel_stack_len: usize,
}

/// What a woken continuation receives from its settled promise.
#[derive(Debug, Clone)]
pub(super) enum ResumePayload {
    Resolved(Value),
    Rejected(Value),
}

/// One entry of the VM's handler stack: the `TryEnter` snapshot a throw
/// restores when it unwinds to this handler.
#[derive(Debug)]
pub(super) struct HandlerEntry {
    /// Where the catch block starts (the unwinder jumps here after pushing
    /// the thrown value).
    pub(super) catch_ip: CodeAddr,
    /// `stack.len()` at `TryEnter`: the unwinder truncates back to this.
    pub(super) stack_len: usize,
    /// `callstack.len()` at `TryEnter`: frames entered inside the `try` are
    /// discarded by truncating back to this.
    pub(super) callstack_len: usize,
    /// `fp` at `TryEnter`, restored on unwind.
    pub(super) fp: StackAddr,
}

/// Result of [`VM::throw_value`]: whether a `try` handler caught the value.
#[derive(Debug)]
pub enum ThrowOutcome {
    /// Unwound to a handler; resume execution with `step()`.
    Caught,
    /// No handler is active: the value is handed back so the host can
    /// escalate per its policy (e.g. surface the failure to the LLM).
    Uncaught(Value),
}

/// A heap-allocated closure value. `upvals` is the captured environment;
/// `prototype` is lazily allocated on first `F.prototype` access (Stage 4b),
/// `None` until then. `arity` is the JS `Function.prototype.length` (params
/// before the first default/rest) for `fn.length` (Step 6). The code address
/// (`addr`) lives inline in `Value::Closure` so dispatch can jump directly
/// without dereferencing this entry; this entry is read only by `EnterFrame`
/// to install upvals (and only when the callee's static upval count > 0) and
/// by `GetLength` for `fn.length`.
///
/// Step 2e: `props` is the inline own-property bag — `Option<Box<…>>` so a
/// function without user props (the common case) pays one `is_none()` branch
/// on a named-access miss only, never an allocation. `name`/`length`/
/// `prototype` are virtual rungs resolved by `get_property`; they materialize
/// into the bag only on reassign (`f.name = "x"`).
#[derive(Clone, Debug, PartialEq)]
pub struct Closure {
    pub upvals: ThinVec<Value>,
    pub prototype: Option<ObjectPtr>,
    pub arity: u16,
    pub props: Option<Box<IndexMap<RcStr, Value>>>,
}

/// A bound function value produced by `f.bind(thisArg, ...args)` — the only
/// receiver-carrying value in the VM. Immutable, refcounted via `Rc` (not an
/// arena entry): bound functions are transient, and the arena never reclaims.
/// The `Rc` graph is acyclic by construction — `BoundFn` has no interior
/// mutability and references heap aggregates only by arena index (a `u32`),
/// never by a strong `Rc`. The `bound_args` vector is fixed at bind time, and
/// any `Rc`-bearing element (`RcStr`/`RcRegExp`/another `Bound`) is itself
/// immutable and pre-existing, so no element can close a cycle back to this
/// `BoundFn`. (`ThinVec` keeps the common `f.bind(obj)` case — empty
/// `bound_args` — to a single pointer, no heap alloc.)
#[derive(Clone, Debug, PartialEq)]
pub struct BoundFn {
    pub this_val: Value,
    pub bound_args: ThinVec<Value>,
    pub callable: Value,
}

/// State of one entry in the VM's `promises` heap. A promise is born
/// `Pending` (by `Instr::Invoke`) and transitions exactly once.
#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    /// Not yet settled. `waiters` holds the ids of suspended continuations
    /// registered on this promise (Tier 2); settlement moves them onto the
    /// ready queue. The root strand never registers — its blocking `Await`
    /// simply re-executes.
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
    /// The `step(fuel)` instruction budget ran out before an effect,
    /// completion, or error. Nothing was consumed (`ip` is at the next
    /// unexecuted instruction); call `step` again to continue. This is the
    /// cooperative-scheduling yield (9_TUI): hosts run the VM in slices so
    /// a hot program can't starve the loop, debuggers single-step with
    /// `fuel = 1`. Never observable by the program — no `try` can trap it.
    OutOfFuel,
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
    /// An uncaught program-level `throw` (no active `try` handler). Distinct
    /// from the VM's own failures: the *program* produced this error value
    /// deliberately, and the host's policy for it differs (show the LLM the
    /// program's own error, not a VM diagnostic). The thrown value is
    /// preserved in [`VMError::payload`]; the message carries a rendering.
    UncaughtException,
    /// The root strand is blocked on a promise that can never settle: the
    /// ready queue, outbox, and in-flight host calls are all empty (7_ASYNC
    /// Tier 2). Only reachable via circular awaits among async calls — the
    /// message names the await chain. Never resumable.
    Deadlock,
    /// A bare-name resolution failure from [`Instr::PushName`] — the name
    /// is not in the builtin registry, not a hardcoded global, and not an
    /// error constructor. Maps to JS's `ReferenceError`.
    ReferenceError,
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
/// | ObjGet (non-object receiver) | TypeError | PushValueThenContinue | receiver popped in the error arm (pop-first normalized) |
/// | GetMethodOrProp (null/undefined receiver) | TypeError | PushValueThenContinue | receiver popped in the error arm (pop-first normalized, same as ObjGet) |
/// | ObjSet (non-object receiver) | TypeError | PushValueThenContinue | value popped, then receiver popped in the error arm (pop-first normalized) |
/// | **ObjExtend** (non-object src) | TypeError | PushValueThenContinue | both src+obj popped first |
/// | **ArrExtend** (non-array src) | TypeError | PushValueThenContinue | both src+arr popped first |
/// | **ArrPush** (non-array target) | TypeError | PushValueThenContinue | both val+arr popped first |
/// | Await (rejected promise, no handler, root strand) | ValueError | PushValueThenContinue | promise popped before failing; host may substitute a value for the rejection (with a reachable handler the rejection value unwinds to `catch`; inside a resumed strand it rejects the strand's promise — neither reaches the host) |
/// | Await (bad promise pointer) | ValueError | **NotResumable** | corrupt heap = invariant violation |
/// | **IncLocal** (non-numeric local) | TypeError | **NotResumable** | reads local by peek (no stack consumption) |
/// | bad heap/cell pointer (`get`/`get_mut` on arrays/objects/cells/closures) | TypeError/ValueError | **NotResumable** | corrupt heap = invariant violation; some sites also have no result slot (SetLocal) |
/// | Raise with argc > 1 | BadArg | NotResumable | instruction contract violated (compiler emits 0 or 1) |
/// | Throw (no handler) | UncaughtException | **NotResumable** | operand popped, but a `throw` has no result slot a substituted value could fill; the thrown value is preserved in `VMError::payload` |
/// | TryEnter (bad handler address) | BadCall | NotResumable | invariant violation / compiler bug |
/// | TryExit (empty handler stack) | BadArg | NotResumable | unmatched TryExit = compiler bug |
/// | StackUnderflow, BadReturn, BadCall, BadAlloc, BadArg, BadLocal | — | NotResumable | invariant violation / compiler bug |
/// | Deadlock | — | NotResumable | circular awaits: every strand is parked and no settlement can arrive; there is no execution state a value could resume |
///
/// All 10 `ErrorKind`s are covered. (Fuel exhaustion is not an error:
/// `step(fuel)` running dry yields `StepResult::OutOfFuel` — nothing
/// consumed, call `step` again to continue.) The bolded sites are the
/// TypeError/ValueError sites that error before full operand consumption
/// (or, for bad pointers, mid-mutation) and therefore must not be resumed.
#[derive(Debug)]
pub enum ResumeMode {
    /// Internal invariant broken (compiler bug / host misuse). Never resume.
    NotResumable,
    /// The failed instruction's operands were consumed; pushing a
    /// replacement result and advancing ip resumes as if it succeeded.
    PushValueThenContinue,
}

#[derive(Debug)]
pub struct VMError {
    pub kind: ErrorKind,
    pub ip: CodeAddr,
    pub message: String,
    pub resume: ResumeMode,
    /// The thrown program value, preserved for `UncaughtException` so the
    /// host can inspect it structurally (or carry it into a rewritten
    /// program) — the message only holds a rendering. `None` for every
    /// other kind.
    pub payload: Option<Value>,
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
            payload: None,
        }
    }
}

// ── submodules ──────────────────────────────────────────────

mod dispatch;
mod methods;

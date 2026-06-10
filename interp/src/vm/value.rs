use smallvec::SmallVec;
use thin_vec::ThinVec;

use crate::builtin::Builtin;
pub use crate::rc_str::RcStr;

use super::instr;
use super::instr::CodeAddr;

/// Object keys and string-valued instruction operands. A thin, refcounted,
/// immutable string: cloning a key (`ObjNew`/`ObjSet`) or pushing a literal
/// (`PushStr`) is a refcount bump, and identical interned names share one
/// allocation.
pub type FieldName = RcStr;

/// Convert a `SmallVec` to a `ThinVec`, copying from the stack allocation.
/// Used at boundaries where heap storage is required (alloc_array,
/// alloc_closure, etc.).
pub(crate) fn small_to_thin(sv: &SmallVec<[Value; 16]>) -> ThinVec<Value> {
    ThinVec::from(sv.as_slice())
}

/// Not `Copy`: the `String` variant owns an `RcStr` whose clone must bump a
/// refcount and whose drop must release one. Every other variant is a trivial
/// bit-copy, so `clone()` on a non-string value is as cheap as the old `Copy`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// JS `undefined`: the value of an absent thing, as distinct from `null`
    /// (a present, intentionally-empty value). Produced internally — never by
    /// JSON, which only yields `Null` — by a missing object property, an
    /// out-of-bounds array index, a read of an unset variable, and an
    /// uninitialized local. `null`/`undefined` thus mirror JS's data-vs-absence
    /// split. Falsy, has no JSON form of its own (see `stack_value_to_json`),
    /// and `=== undefined` only (strict): `undefined !== null`.
    Undefined,
    Null,
    Bool(bool),
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
    Float(f64),
    /// An immutable UTF-8 string, stored inline as a thin refcounted handle
    /// rather than behind a heap index. Cloning (stack dup, local read, pushing
    /// a literal) is a refcount bump; `===`/`<` compare by content (with an O(1)
    /// pointer-equality fast path for shared/interned strings). Unlike arrays
    /// and objects — which are `Ptr` into `heap` and compare by reference
    /// identity — strings are primitives and are reclaimed when the last
    /// reference drops (the heap itself never reclaims).
    String(RcStr),
    Array(instr::ArrayPtr),
    Object(instr::ObjectPtr),
    /// Internal indirection for a captured *by-reference* binding: indexes the
    /// VM's `cells` side table, which has identity and outlives stack frames.
    /// Only ever stored in a frame's local (or captured-arg) slots;
    /// `Local`/`SetLocal` dereference it transparently, so the marker never
    /// surfaces in expression temporaries, heap collections, or variables.
    Upval(instr::CellIndex),
    Closure(instr::ClosurePtr),
    /// A first-class function value: just a code address, with no captured
    /// environment. Covers non-capturing lambdas and named functions passed as
    /// values (dispatch tables, `map`/`filter` callbacks, etc.). Capturing
    /// lambdas instead become a `Value::Closure` (a code address plus a
    /// captured environment), built by `MakeClosure` and likewise called
    /// through `CallDyn`.
    Fn(CodeAddr),
    /// A builtin stdlib function as a first-class value (`Math.max`, `arr.push`
    /// passed as a callback). Like `Fn`, it is callable (via `CallDyn`), is a
    /// "function" under `typeof`, compares by identity, and has no JSON form.
    /// The compiler's common path uses the static `Instr::CallBuiltin` instead;
    /// this variant exists for the rarer higher-order/callback use.
    Builtin(Builtin),
}

/// Storage class for a local slot declared by `EnterFrame`. A `Plain` slot is an
/// ordinary stack local; a `Boxed` slot is captured by reference, so it is
/// backed by a `cells` entry and addressed through an `Upval` marker.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SlotKind {
    Plain,
    Boxed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Closure {
    pub addr: CodeAddr,
    pub upvals: ThinVec<Value>,
}

// ── free helper functions ─────────────────────────────────────────────

/// JS `Number.prototype.toString` for a finite-or-not f64. Integers print
/// without a decimal point; NaN/±Infinity get their JS spellings (Rust's
/// `Display` would otherwise emit "NaN"/"inf"). Diverges from JS only for the
/// very large/small magnitudes JS renders in exponential form (e.g. `1e+21`),
/// which don't arise from tool/JSON data here.
pub(crate) fn js_number_to_string(n: f64) -> String {
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
pub(crate) fn as_f64(val: &Value) -> Option<f64> {
    match val {
        Value::Float(n) => Some(*n),
        Value::PosInt(u) => Some(*u as f64),
        Value::NegInt(i) => Some(*i as f64),
        _ => None,
    }
}

/// Coerce a numeric value to i64 for integer-only ops (mod, bitwise, shifts,
/// indices). `NegInt` is taken directly; a `PosInt` must fit in i64; a
/// `Number` must be integer-valued. Returns None otherwise.
pub(crate) fn as_i64(val: &Value) -> Option<i64> {
    match val {
        Value::NegInt(i) => Some(*i),
        Value::PosInt(u) => i64::try_from(*u).ok(),
        Value::Float(n) if float_is_int(*n) => Some(*n as i64),
        _ => None,
    }
}

pub(crate) fn is_number(val: &Value) -> bool {
    matches!(val, Value::Float(_) | Value::PosInt(_) | Value::NegInt(_))
}

/// JS `ToNumber` applied to a string, as used when a loose `==` compares a
/// number to a string. Trims whitespace, treats the empty string as 0, and
/// otherwise parses as f64 — yielding NaN (which is never equal to anything)
/// when unparseable. Diverges from spec ToNumber on a few literal forms it
/// would accept (hex `0x…`, etc.), which don't arise from tool/JSON data here.
pub(crate) fn js_str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        0.0
    } else {
        t.parse::<f64>().unwrap_or(f64::NAN)
    }
}

/// Helper for `loose_equal`: a numeric value vs a string. Coerces the string
/// with `ToNumber` (`js_str_to_number`) and compares by f64. Non-numeric `num`
/// (already filtered by the caller's `is_number` guard) yields `false`.
pub(crate) fn num_loose_eq_str(num: &Value, s: &str) -> bool {
    match as_f64(num) {
        Some(a) => a == js_str_to_number(s),
        None => false,
    }
}

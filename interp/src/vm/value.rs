use crate::builtin::Builtin;
pub use crate::rc_str::RcStr;

use super::instr;
use super::instr::CodeAddr;

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

// ── Value methods ────────────────────────────────────────────
impl Value {
    /// JS truthiness. The falsy set is exactly `false`, `0`/`-0`, `NaN`, `""`,
    /// `null`, and `undefined`; everything else (incl. empty arrays/objects and
    /// the string "0") is truthy. Needs heap access to detect the empty string,
    /// hence a method.
    pub(crate) fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Null | Value::Undefined => false,
            Value::Float(n) => *n != 0.0 && !n.is_nan(),
            Value::PosInt(u) => *u != 0,
            // NegInt is always negative (i64::MIN..=-1), hence never zero.
            Value::NegInt(_) => true,
            // Empty string is falsy; any other string is truthy.
            Value::String(s) => !s.as_str().is_empty(),
            // All arrays/objects/closures/functions are truthy.
            Value::Array(_)
            | Value::Object(_)
            | Value::Closure(_)
            | Value::Fn(_)
            | Value::Builtin(_) => true,
            // Internal indirection; never a legitimate operand.
            Value::Upval(_) => false,
        }
    }

    /// JS `ToNumber` for the arithmetic operators. `null`→0, `undefined`→NaN,
    /// booleans→0/1, numbers pass through, strings parse (`ToNumber`, NaN when
    /// unparseable). Returns None for values JS would route through `ToPrimitive`
    /// first — arrays, objects, closures, functions — which this VM deliberately
    /// does not coerce (see the divergence note on `loose_equal`); arithmetic on
    /// those is a TypeError.
    pub(crate) fn to_number(&self) -> Option<f64> {
        match self {
            Value::Float(n) => Some(*n),
            Value::PosInt(u) => Some(*u as f64),
            Value::NegInt(i) => Some(*i as f64),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Null => Some(0.0),
            Value::Undefined => Some(f64::NAN),
            Value::String(s) => Some(js_str_to_number(s)),
            Value::Array(_)
            | Value::Object(_)
            | Value::Closure(_)
            | Value::Fn(_)
            | Value::Builtin(_)
            | Value::Upval(_) => None,
        }
    }

    /// Whether a value is a string (used to pick `+`'s concat vs add path).
    pub(crate) fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    /// Reference/value equality matching JS `===`. Primitives compare by value;
    /// strings, though heap-allocated here, are primitives and so compare by
    /// *content*. Arrays, objects, and closures compare by *reference identity*
    /// (same heap address) — `{a:1} === {a:1}` is false, as in JS.
    pub(crate) fn strict_equal(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            // Strict (===): undefined equals only itself; undefined !== null.
            // (Loose `null == undefined` would need a separate op; Eq is ===.)
            (Value::Undefined, Value::Undefined) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => {
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
            (Value::PosInt(a), Value::PosInt(b)) => a == b,
            (Value::NegInt(a), Value::NegInt(b)) => a == b,
            (Value::PosInt(_), Value::NegInt(_)) | (Value::NegInt(_), Value::PosInt(_)) => false,
            (Value::PosInt(a), Value::Float(b)) => !b.is_nan() && (*a as f64) == *b,
            (Value::Float(a), Value::PosInt(b)) => !a.is_nan() && *a == (*b as f64),
            (Value::NegInt(a), Value::Float(b)) => !b.is_nan() && (*a as f64) == *b,
            (Value::Float(a), Value::NegInt(b)) => !a.is_nan() && *a == (*b as f64),
            // Function values are equal iff they point at the same code address.
            (Value::Fn(a), Value::Fn(b)) => a == b,
            // Builtins compare by identity, like Fn.
            (Value::Builtin(a), Value::Builtin(b)) => a == b,
            // Strings are primitives: equal by *content*. `RcStr`'s `==` short-
            // circuits on pointer identity, so comparing shared/interned strings
            // (e.g. two clones of one literal) is O(1).
            (Value::String(a), Value::String(b)) => a == b,
            // Same heap address is the same object — JS reference identity, the
            // only equality arrays/objects/closures get (`{a:1} === {a:1}` is
            // false). A correct program never dangles (the heap only grows).
            (Value::Array(p), Value::Array(q)) => p == q,
            (Value::Object(p), Value::Object(q)) => p == q,
            (Value::Closure(p), Value::Closure(q)) => p == q,
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
    pub(crate) fn loose_equal(&self, other: &Value) -> bool {
        use Value::*;
        // null / undefined: loosely equal to each other, to nothing else.
        let l_nullish = matches!(self, Null | Undefined);
        let r_nullish = matches!(other, Null | Undefined);
        if l_nullish || r_nullish {
            return l_nullish && r_nullish;
        }
        match (self, other) {
            // Boolean → number, then re-run the comparison.
            (Bool(b), _) => Value::Float(if *b { 1.0 } else { 0.0 }).loose_equal(other),
            (_, Bool(b)) => Value::Float(if *b { 1.0 } else { 0.0 }).loose_equal(other),
            // Number vs string (either order): coerce the string with ToNumber.
            (l, String(s)) if l.is_number() => l.num_loose_eq_str(s),
            (String(s), r) if r.is_number() => r.num_loose_eq_str(s),
            // No further coercion: same-type primitives and heap-vs-heap defer
            // to the strict structural comparison.
            _ => self.strict_equal(other),
        }
    }

    /// Total ordering for comparable types. Returns None for incomparable types.
    pub(crate) fn compare(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::PosInt(a), Value::PosInt(b)) => Some(a.cmp(b)),
            (Value::NegInt(a), Value::NegInt(b)) => Some(a.cmp(b)),
            // Sign decides cross-variant ordering with no value juggling.
            (Value::PosInt(_), Value::NegInt(_)) => Some(std::cmp::Ordering::Greater),
            (Value::NegInt(_), Value::PosInt(_)) => Some(std::cmp::Ordering::Less),
            (Value::PosInt(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::PosInt(b)) => a.partial_cmp(&(*b as f64)),
            (Value::NegInt(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::NegInt(b)) => a.partial_cmp(&(*b as f64)),
            // Strings order lexicographically by bytes (UTF-8 byte order matches
            // code-point order). Arrays/objects/closures are incomparable.
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(n) => Some(*n),
            Value::PosInt(u) => Some(*u as f64),
            Value::NegInt(i) => Some(*i as f64),
            _ => None,
        }
    }
    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Value::NegInt(i) => Some(*i),
            Value::PosInt(u) => i64::try_from(*u).ok(),
            Value::Float(n) if float_is_int(*n) => Some(*n as i64),
            _ => None,
        }
    }

    pub(crate) fn is_number(&self) -> bool {
        matches!(self, Value::Float(_) | Value::PosInt(_) | Value::NegInt(_))
    }

    pub(crate) fn num_loose_eq_str(&self, s: &str) -> bool {
        match self.as_f64() {
            Some(a) => a == js_str_to_number(s),
            None => false,
        }
    }

    /// Byte length of a string value, if it is one.
    pub(crate) fn str_byte_len(&self) -> Option<usize> {
        match self {
            Value::String(s) => Some(s.len()),
            _ => None,
        }
    }
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

/// Coerce a numeric value to i64 for integer-only ops (mod, bitwise, shifts,
/// indices). `NegInt` is taken directly; a `PosInt` must fit in i64; a
/// `Number` must be integer-valued. Returns None otherwise.

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

//! Builtins — the JS standard-library surface (`arr.push`, `Math.max`,
//! `Object.keys`, `JSON.parse`, …) that is invoked with *call* syntax.
//!
//! The VM has no method/prototype objects, so these are recognized
//! structurally by the compiler and lowered to a call against a `Builtin` id
//! rather than to one dedicated instruction each. This keeps the instruction
//! set to true VM primitives and gives variadic/optional-argument builtins
//! (`Math.max`, `s.slice`) for free via a uniform calling convention.
//!
//! Calling convention (shared by `Instr::CallBuiltin` and a `Builtin` value
//! called through `CallDyn`): the `argc` arguments sit on the stack
//! left-to-right (arg 0 deepest, the last on top); for a method the receiver is
//! arg 0. The builtin pops exactly its `argc` arguments and pushes exactly one
//! result — assignment-style "leave a value" semantics, so every builtin call
//! is a well-formed expression.

use crate::vm::{CodeAddr, ErrorKind, RcStr, VM, VMError, Value};
use smallvec::SmallVec;
use thin_vec::ThinVec;

/// Construct a `NotResumable` error without borrowing `VM` (for use when a
/// mutable borrow is active). Every caller guards a heap-pointer lookup —
/// an invariant violation — so this delegates to `VMError::fail_at`.
fn fail_at(ip: CodeAddr, kind: ErrorKind, msg: &str) -> VMError {
    VMError::fail_at(ip, kind, msg)
}

// ── declarative builtin registry ─────────────────────────────────────────────

/// The kind of a builtin: either a method on a receiver value (string or array),
/// or a static function under a namespace (`Math.abs`, `JSON.parse`, …).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuiltinKind {
    Method,
    Namespace(&'static str),
}

/// Sentinel for variadic builtins: no upper bound on argument count.
const VARARG: u32 = u32::MAX;

/// The single-source-of-truth macro for every builtin. One row per builtin
/// declares its enum variant, kind, display name, argument bounds (counting the
/// receiver for methods), and handler function. The macro emits the enum,
/// `meta()`, `call()`, `for_method()`, and `for_namespace()` — no hand-written
/// dispatch duplication.
macro_rules! builtins {
    (
        $(
            $variant:ident, $kind:expr, $name:literal, $min:expr, $max:expr, $handler:ident;
        )*
    ) => {
        /// A builtin's identity. Used both as the static call target
        /// (`Instr::CallBuiltin(Builtin, argc)`, the compiler's fast path) and
        /// as a first-class value (`Value::Builtin(Builtin)`, for passing a
        /// builtin as a callback — invoked through `CallDyn`). The enum *is*
        /// the registry key: `Debug` prints the name and equality is trivial.
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        pub enum Builtin {
            $(
                $variant,
            )*
        }

        /// Compile-time facts about a builtin: its display name and accepted
        /// argument count (inclusive, **counting the receiver** for methods).
        /// One source of truth the compiler reads for arity checks and error
        /// messages.
        pub struct BuiltinMeta {
            pub name: &'static str,
            pub min_args: u32,
            pub max_args: u32,
            pub kind: BuiltinKind,
        }

        impl Builtin {
            pub const fn meta(self) -> BuiltinMeta {
                match self {
                    $(
                        Builtin::$variant => BuiltinMeta {
                            name: $name,
                            min_args: $min,
                            max_args: $max,
                            kind: $kind,
                        },
                    )*
                }
            }

            /// Dispatch: run the builtin against `vm`, consuming `argc` stack
            /// arguments and pushing one result.
            ///
            /// Arguments are read in-place via `Args` — never moved, cloned, or
            /// collected. The epilogue truncates the stack and pushes the result
            /// on both the `Ok` and `Err` paths, preserving the pop-first
            /// invariant.
            pub fn call(self, vm: &mut VM, argc: u32) -> Result<(), VMError> {
                let n = argc as usize;
                if vm.stack.len() < n {
                    return Err(vm.fail(ErrorKind::StackUnderflow, "stack underflow"));
                }
                let base = vm.stack.len() - n;
                let args = Args { base, argc: n };
                if argc < self.meta().min_args {
                    vm.stack.truncate(base);
                    return Err(vm.fail(
                        ErrorKind::BadArg,
                        format!(
                            "`{}` called with too few arguments ({argc})",
                            self.meta().name
                        ),
                    ));
                }
                let result = match self {
                    $(
                        Builtin::$variant => $handler(vm, args),
                    )*
                };
                // Epilogue: truncate args on both paths, push result on Ok.
                vm.stack.truncate(args.base);
                match result {
                    Ok(val) => {
                        vm.stack.push(val);
                        Ok(())
                    }
                    Err(mut e) => {
                        e.message = format!("in `{}`: {}", self.meta().name, e.message);
                        Err(e)
                    }
                }
            }

            /// Look up a method builtin by name (for `recv.push(…)` style calls).
            pub fn for_method(name: &str) -> Option<Builtin> {
                $(
                    if matches!($kind, BuiltinKind::Method) && $name == name {
                        return Some(Builtin::$variant);
                    }
                )*
                None
            }

            /// Look up a namespaced builtin by namespace + method name (for
            /// `Math.abs(…)` style calls and `Math.sqrt` as a value).
            pub fn for_namespace(ns: &str, name: &str) -> Option<Builtin> {
                $(
                    if let BuiltinKind::Namespace(ns_val) = $kind {
                        if ns_val == ns && $name == name {
                            return Some(Builtin::$variant);
                        }
                    }
                )*
                None
            }
        }
    };
}

builtins! {
    // ── array methods (Method, receiver + args) ──
    ArrayPush,    BuiltinKind::Method, "push",        1, VARARG, array_push;
    ArrayPop,     BuiltinKind::Method, "pop",         1, 1,      array_pop;
    ArrayShift,   BuiltinKind::Method, "shift",       1, 1,      array_shift;
    ArrayUnshift, BuiltinKind::Method, "unshift",     1, VARARG, array_unshift;
    ArrayJoin,    BuiltinKind::Method, "join",        1, 2,      array_join;
    // ── string methods (Method, receiver + args) ──
    StrSplit,       BuiltinKind::Method, "split",       2, 3, str_split;
    StrIncludes,    BuiltinKind::Method, "includes",    2, 3, str_includes;
    StrIndexOf,     BuiltinKind::Method, "indexOf",     2, 3, str_index_of;
    StrLastIndexOf, BuiltinKind::Method, "lastIndexOf", 2, 3, str_last_index_of;
    StrStartsWith,  BuiltinKind::Method, "startsWith",  2, 2, str_starts_with;
    StrEndsWith,    BuiltinKind::Method, "endsWith",    2, 2, str_ends_with;
    StrSlice,       BuiltinKind::Method, "slice",       2, 3, str_slice;
    StrTrim,        BuiltinKind::Method, "trim",        1, 1, str_trim;
    // ── object static ──
    ObjKeys,   BuiltinKind::Namespace("Object"), "keys",   1, 1, obj_keys;
    ObjValues, BuiltinKind::Namespace("Object"), "values", 1, 1, obj_values;
    // ── JSON static ──
    JSONParse,     BuiltinKind::Namespace("JSON"), "parse",     1, 1, json_parse;
    JSONStringify, BuiltinKind::Namespace("JSON"), "stringify", 1, 1, json_stringify;
    // ── Number static ──
    NumberIsInteger,  BuiltinKind::Namespace("Number"), "isInteger",  1, 1, number_is_integer;
    NumberParseInt,   BuiltinKind::Namespace("Number"), "parseInt",   1, 2, number_parse_int;
    NumberParseFloat, BuiltinKind::Namespace("Number"), "parseFloat", 1, 1, number_parse_float;
    // ── Array static ──
    ArrayIsArray, BuiltinKind::Namespace("Array"), "isArray", 1, 1, array_is_array;
    // ── Math ──
    MathAbs,   BuiltinKind::Namespace("Math"), "abs",   1, 1,      math_abs;
    MathSqrt,  BuiltinKind::Namespace("Math"), "sqrt",  1, 1,      math_sqrt;
    MathCeil,  BuiltinKind::Namespace("Math"), "ceil",  1, 1,      math_ceil;
    MathFloor, BuiltinKind::Namespace("Math"), "floor", 1, 1,      math_floor;
    MathRound, BuiltinKind::Namespace("Math"), "round", 1, 1,      math_round;
    MathSign,  BuiltinKind::Namespace("Math"), "sign",  1, 1,      math_sign;
    MathMin,   BuiltinKind::Namespace("Math"), "min",   0, VARARG, math_min;
    MathMax,   BuiltinKind::Namespace("Math"), "max",   0, VARARG, math_max;
    MathPow,   BuiltinKind::Namespace("Math"), "pow",   2, 2,      math_pow;
}

// ── argument accessor ────────────────────────────────────────────────────────

/// Zero-cost argument handle: a short-lived borrow token that reads arguments
/// in-place on the stack. `Copy` so handlers can pass it by value.
///
/// An absent argument (index ≥ argc) yields `&Value::Undefined`, matching JS
/// semantics. Handlers apply JS-level defaults for optional args (e.g. `join`
/// separator → `","`, `slice` end → length) themselves.
#[derive(Clone, Copy)]
struct Args {
    /// Index of arg 0 in `vm.stack` (the deepest).
    base: usize,
    /// Number of arguments present.
    argc: usize,
}

impl Args {
    /// Arg `i`, or `&Value::Undefined` if absent. Zero-cost — no clone.
    fn get<'a>(&self, vm: &'a VM, i: usize) -> &'a Value {
        if i < self.argc {
            &vm.stack[self.base + i]
        } else {
            &Value::Undefined
        }
    }
    /// All args (arg 0 = receiver, deepest) as a read-only slice.
    #[allow(dead_code)]
    fn slice<'a>(&self, vm: &'a VM) -> &'a [Value] {
        &vm.stack[self.base..self.base + self.argc]
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Clamp a byte offset into `[0, s.len()]` and round it up to the next UTF-8
/// char boundary, so it can always be used as a slice start. Used to apply JS's
/// "start position" arguments (which clamp rather than error) on our byte-string
/// representation.
fn clamp_start(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Clamp a byte offset into `[0, s.len()]` and round it down to the previous
/// UTF-8 char boundary, so it can safely be used as an end-of-slice boundary.
fn clamp_end(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Build a number `Value` from an integer-valued `f64`, mirroring the
/// `PosInt`/`NegInt`/`Number` split used by [`VM::json_to_stack_value`]:
/// non-negative integers that fit become `PosInt`, negative ones `NegInt`, and
/// anything else (fractions, out-of-range magnitudes, NaN/∞) stays `Number`.
fn int_value(n: f64) -> Value {
    if n.fract() == 0.0 {
        if (0.0..=u64::MAX as f64).contains(&n) {
            return Value::PosInt(n as u64);
        }
        if n < 0.0 && n >= i64::MIN as f64 {
            return Value::NegInt(n as i64);
        }
    }
    Value::Float(n)
}

/// JS `parseInt(string, radix)`: skip leading whitespace, an optional sign, an
/// optional `0x`/`0X` prefix for radix 16 (or auto-detected when radix is 0 or
/// omitted), then consume the longest run of digits valid in the radix. Returns
/// `NaN` when the radix is out of `[2, 36]` or no digits are found. Trailing
/// non-digit characters are ignored, exactly like the browser builtin.
fn js_parse_int(input: &str, mut radix: i64) -> f64 {
    let s = input.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut sign = 1.0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        if bytes[i] == b'-' {
            sign = -1.0;
        }
        i += 1;
    }
    let has_hex_prefix = i + 1 < bytes.len() && bytes[i] == b'0' && (bytes[i + 1] | 0x20) == b'x';
    if radix == 0 {
        if has_hex_prefix {
            radix = 16;
            i += 2;
        } else {
            radix = 10;
        }
    } else {
        if !(2..=36).contains(&radix) {
            return f64::NAN;
        }
        if radix == 16 && has_hex_prefix {
            i += 2;
        }
    }
    let start = i;
    let mut value = 0.0;
    while i < bytes.len() {
        let digit = match bytes[i] {
            c @ b'0'..=b'9' => (c - b'0') as i64,
            c @ b'a'..=b'z' => (c - b'a' + 10) as i64,
            c @ b'A'..=b'Z' => (c - b'A' + 10) as i64,
            _ => break,
        };
        if digit >= radix {
            break;
        }
        value = value * radix as f64 + digit as f64;
        i += 1;
    }
    if i == start {
        return f64::NAN;
    }
    sign * value
}

// ── array method implementations ─────────────────────────────────────────────

/// `arr.push(a, b, …)` → appends all arguments and returns the new length.
fn array_push(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    // Clone the values to push first (immutable borrow of vm.stack), then
    // mutate the array.
    let to_push: SmallVec<[Value; 8]> = args.slice(vm)[1..].iter().cloned().collect();
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    for v in to_push {
        arr.push(v);
    }
    Ok(int_value(arr.len() as f64))
}

/// `arr.pop()` → removes and returns the last element.
fn array_pop(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let val = arr
        .pop()
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(val)
}

/// `arr.shift()` → removes and returns the first element.
fn array_shift(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    if arr.is_empty() {
        return Err(vm.fail(ErrorKind::ValueError, "value error"));
    }
    Ok(arr.remove(0))
}

/// `arr.unshift(a, b, …)` → prepends all arguments (preserving order) and
/// returns the new length.
fn array_unshift(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    // Clone the values first (immutable borrow), then mutate.
    let to_insert: SmallVec<[Value; 8]> = args.slice(vm)[1..].iter().cloned().collect();
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    // Insert in reverse so order is preserved: unshift(1,2) → [1,2,...]
    for v in to_insert.into_iter().rev() {
        arr.insert(0, v);
    }
    Ok(int_value(arr.len() as f64))
}

/// `arr.join([sep])` → joins with sep (default ",").
fn array_join(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let sep = match args.get(vm, 1) {
        Value::Undefined => RcStr::from(","),
        v => vm.to_js_string(v, 0),
    };
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut joined = String::new();
    for (i, v) in arr.iter().enumerate() {
        if i > 0 {
            joined.push_str(sep.as_str());
        }
        // JS: null/undefined elements contribute the empty string.
        if !matches!(v, Value::Null | Value::Undefined) {
            joined.push_str(vm.to_js_string(v, 0).as_str());
        }
    }
    Ok(Value::String(RcStr::from(joined)))
}

// ── string method implementations ────────────────────────────────────────────

/// `s.split(delim[, limit])` → array of substrings.
fn str_split(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    // split() / split(undefined) → [s]
    let delim = args.get(vm, 1);
    if matches!(delim, Value::Undefined) {
        let parts: ThinVec<Value> = thin_vec::thin_vec![Value::String(s)];
        return Ok(vm.alloc_array(parts));
    }
    let delim_s = vm.string_from(delim)?;
    let limit = match args.get(vm, 2) {
        Value::Undefined => None,
        v => {
            let lim = v
                .as_i64()
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
            // JS: ToUint32 coercion — negative wraps huge → effectively no limit
            if lim < 0 { None } else { Some(lim as usize) }
        }
    };
    let parts: ThinVec<Value> = if delim_s.is_empty() {
        // split("") → array of characters (per UTF-8 char here — byte-string
        // divergence). No leading/trailing empty entries.
        let chars: ThinVec<Value> = s
            .chars()
            .map(|c| Value::String(RcStr::from(c.to_string())))
            .collect();
        match limit {
            Some(lim) => chars.into_iter().take(lim).collect(),
            None => chars,
        }
    } else {
        let splits: ThinVec<Value> = s
            .split(delim_s.as_str())
            .map(|p| Value::String(RcStr::from(p)))
            .collect();
        match limit {
            Some(lim) => splits.into_iter().take(lim).collect(),
            None => splits,
        }
    };
    Ok(vm.alloc_array(parts))
}

/// `s.includes(needle[, start])` → bool.
fn str_includes(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let needle = vm.str_from(args.get(vm, 1))?;
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let start = clamp_start(haystack, start.max(0) as usize);
    Ok(Value::Bool(haystack[start..].contains(needle)))
}

/// `s.indexOf(needle[, start])` → int (or -1).
fn str_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let needle = vm.str_from(args.get(vm, 1))?;
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let start = clamp_start(haystack, start.max(0) as usize);
    let pos = haystack[start..].find(needle).map(|p| (p + start) as f64);
    Ok(int_value(pos.unwrap_or(-1.0)))
}

/// `s.lastIndexOf(needle[, start])` → int (or -1).
fn str_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let needle = vm.str_from(args.get(vm, 1))?;
    let start = match args.get(vm, 2) {
        Value::Undefined => haystack.len() as i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let from = start.max(0) as usize;
    let end = clamp_end(haystack, from + needle.len());
    let pos = haystack[..end].rfind(needle).map(|p| p as f64);
    Ok(int_value(pos.unwrap_or(-1.0)))
}

/// `s.startsWith(prefix)` → bool.
fn str_starts_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let prefix = vm.str_from(args.get(vm, 1))?;
    Ok(Value::Bool(haystack.starts_with(prefix)))
}

/// `s.endsWith(suffix)` → bool.
fn str_ends_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let suffix = vm.str_from(args.get(vm, 1))?;
    Ok(Value::Bool(haystack.ends_with(suffix)))
}

/// `s.slice(start[, end])` → substring over a half-open byte range.
/// JS semantics: negative indices count from end, everything clamps,
/// `start ≥ end` → `""`. Only mid-codepoint is an error (byte-string divergence).
fn str_slice(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let len = s.len() as i64;

    let to_offset = |v: &Value, default: i64| -> Result<i64, VMError> {
        if matches!(v, Value::Undefined) {
            return Ok(default);
        }
        let n = v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if n < 0 {
            Ok((n + len).max(0))
        } else {
            Ok(n.min(len))
        }
    };

    let start = to_offset(args.get(vm, 1), 0)? as usize;
    let end = to_offset(args.get(vm, 2), len)?.max(0) as usize;

    // JS: start ≥ end → ""
    if start >= end {
        return Ok(Value::String(RcStr::from("")));
    }

    let end = end.min(s.len());
    let start_clamped = clamp_start(&s, start.min(s.len()));
    let end_clamped = clamp_end(&s, end);

    // Mid-codepoint error (only error case)
    if start_clamped < start || end_clamped > end {
        return Err(vm.fail(ErrorKind::ValueError, "value error"));
    }

    Ok(Value::String(RcStr::from(&s[start_clamped..end_clamped])))
}

/// `s.trim()` → trimmed string.
fn str_trim(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    Ok(Value::String(RcStr::from(s.trim())))
}

// ── object static implementations ────────────────────────────────────────────

/// `Object.keys(obj)` → array of strings.
fn obj_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let keys: ThinVec<RcStr> = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        .keys()
        .cloned()
        .collect();
    let strs: ThinVec<Value> = keys.into_iter().map(Value::String).collect();
    Ok(vm.alloc_array(strs))
}

/// `Object.values(obj)` → array of values.
fn obj_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let vals: ThinVec<Value> = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        .values()
        .cloned()
        .collect();
    Ok(vm.alloc_array(vals))
}

// ── JSON static implementations ──────────────────────────────────────────────

/// `JSON.parse(s)` → any.
fn json_parse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let json: serde_json::Value =
        serde_json::from_str(&s).map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
    vm.json_to_stack_value(&json, 0)
}

/// `JSON.stringify(x)` → str.
fn json_stringify(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let json = vm.stack_value_to_json(args.get(vm, 0), 0)?;
    let s =
        serde_json::to_string(&json).map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::String(RcStr::from(s)))
}

// ── Number static implementations ────────────────────────────────────────────

/// `Number.isInteger(x)` → bool.
fn number_is_integer(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let v = args.get(vm, 0);
    let is_int = matches!(v, Value::PosInt(_) | Value::NegInt(_))
        || matches!(v, Value::Float(n) if crate::vm::float_is_int(*n));
    Ok(Value::Bool(is_int))
}

/// `Number.parseInt(s[, radix])` → int (full JS semantics: optional sign,
/// `0x` prefix, any radix in `[2, 36]`, leading-digit parse with trailing
/// characters ignored). Unparseable input yields `NaN`, like the browser.
fn number_parse_int(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.str_from(args.get(vm, 0))?;
    let radix = match args.get(vm, 1) {
        Value::Undefined => 0,
        v => match v.to_number() {
            Some(n) if n.is_finite() => n as i64,
            _ => 0,
        },
    };
    Ok(int_value(js_parse_int(s, radix)))
}

/// `Number.parseFloat(s)` → float.
fn number_parse_float(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.str_from(args.get(vm, 0))?;
    let n: f64 = s
        .trim()
        .parse()
        .map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::Float(n))
}

// ── Array static implementations ─────────────────────────────────────────────

/// `Array.isArray(x)` → bool.
fn array_is_array(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    Ok(Value::Bool(matches!(args.get(vm, 0), Value::Array(_))))
}

// ── Math implementations ─────────────────────────────────────────────────────

/// Note: `math_abs` / `math_sqrt` etc. are individual functions so the macro
/// can map them. `math_unary` is used as a helper only.

fn math_abs(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.abs())
}
fn math_sqrt(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.sqrt())
}
fn math_ceil(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.ceil())
}
fn math_floor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.floor())
}
fn math_round(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.round())
}
fn math_sign(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.signum())
}

/// Math unary: read one arg, coerce ToNumber, apply f, return Number.
fn math_unary(vm: &mut VM, args: Args, f: fn(f64) -> f64) -> Result<Value, VMError> {
    let n = args
        .get(vm, 0)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    Ok(Value::Float(f(n)))
}

/// `Math.min(...nums)` → the smallest, ToNumber-coercing each. Zero args →
/// +Infinity. Follows `f64::min` (a NaN operand is ignored).
fn math_min(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut acc = f64::INFINITY;
    for i in 0..args.argc {
        let num = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        acc = acc.min(num);
    }
    Ok(Value::Float(acc))
}

/// `Math.max(...nums)` → the largest, ToNumber-coercing each. Zero args →
/// -Infinity. Follows `f64::max` (a NaN operand is ignored).
fn math_max(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut acc = f64::NEG_INFINITY;
    for i in 0..args.argc {
        let num = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        acc = acc.max(num);
    }
    Ok(Value::Float(acc))
}

/// `Math.pow(base, exp)` → base^exp.
fn math_pow(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let base = args
        .get(vm, 0)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let exp = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    Ok(Value::Float(base.powf(exp)))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use crate::vm::{Instr, StepResult};

    fn run(code: Vec<Instr>) -> Vec<Value> {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done { .. } => return vm.stack.clone(),
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    // ── ArrayPush ──────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_push_returns_length() {
        let out = run(vec![
            Instr::PushFloat(10.0),
            Instr::ArrNew(1),
            Instr::Pick(0),
            Instr::PushFloat(20.0),
            Instr::CallBuiltin(Builtin::ArrayPush, 2),
        ]);
        assert_eq!(out.last(), Some(&Value::PosInt(2)));
    }

    // ── ArrayPop ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_pop_returns_last() {
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert_eq!(out, vec![Value::Float(2.0)]);
    }

    #[test]
    fn call_builtin_array_pop_empty_errors() {
        let mut vm = VM::new(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert!(vm.step().unwrap_err().kind == ErrorKind::ValueError);
    }

    // ── ArrayShift ─────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_shift_returns_first() {
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert_eq!(out, vec![Value::Float(1.0)]);
    }

    #[test]
    fn call_builtin_array_shift_empty_errors() {
        let mut vm = VM::new(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert!(vm.step().unwrap_err().kind == ErrorKind::ValueError);
    }

    // ── ArrayUnshift ───────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_unshift_returns_length() {
        let out = run(vec![
            Instr::PushFloat(2.0),
            Instr::ArrNew(1),
            Instr::Pick(0),
            Instr::PushFloat(1.0),
            Instr::CallBuiltin(Builtin::ArrayUnshift, 2),
        ]);
        assert_eq!(out.last(), Some(&Value::PosInt(2)));
    }

    // ── ArrayJoin ──────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_join_default_sep() {
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayJoin, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "1,2"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_array_join_custom_sep() {
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::PushStr(" - ".into()),
            Instr::CallBuiltin(Builtin::ArrayJoin, 2),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "1 - 2"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── StrSplit ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_split() {
        let out = run(vec![
            Instr::PushStr("a,b,c".into()),
            Instr::PushStr(",".into()),
            Instr::CallBuiltin(Builtin::StrSplit, 2),
        ]);
        // result is an array
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_str_split_with_limit() {
        let out = run(vec![
            Instr::PushStr("a,b,c".into()),
            Instr::PushStr(",".into()),
            Instr::PushPosInt(2),
            Instr::CallBuiltin(Builtin::StrSplit, 3),
        ]);
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    // ── StrIncludes ────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_includes() {
        let out = run(vec![
            Instr::PushStr("hello world".into()),
            Instr::PushStr("world".into()),
            Instr::CallBuiltin(Builtin::StrIncludes, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_includes_not_found() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("x".into()),
            Instr::CallBuiltin(Builtin::StrIncludes, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(false)]);
    }

    // ── StrIndexOf ─────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_index_of() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("l".into()),
            Instr::CallBuiltin(Builtin::StrIndexOf, 2),
        ]);
        assert_eq!(out, vec![Value::PosInt(2)]);
    }

    #[test]
    fn call_builtin_str_index_of_not_found() {
        let out = run(vec![
            Instr::PushStr("abc".into()),
            Instr::PushStr("x".into()),
            Instr::CallBuiltin(Builtin::StrIndexOf, 2),
        ]);
        assert_eq!(out, vec![Value::NegInt(-1)]);
    }

    // ── StrLastIndexOf ─────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_last_index_of() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("l".into()),
            Instr::CallBuiltin(Builtin::StrLastIndexOf, 2),
        ]);
        assert_eq!(out, vec![Value::PosInt(3)]);
    }

    // ── negative `start` clamps to 0, matching JS (rather than failing) ─

    #[test]
    fn call_builtin_str_index_of_negative_start_clamps() {
        // "hello".indexOf("h", -5) === 0
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("h".into()),
            Instr::PushNegInt(-5),
            Instr::CallBuiltin(Builtin::StrIndexOf, 3),
        ]);
        assert_eq!(out, vec![Value::PosInt(0)]);
    }

    #[test]
    fn call_builtin_str_includes_negative_start_clamps() {
        // "hello".includes("h", -5) === true
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("h".into()),
            Instr::PushNegInt(-5),
            Instr::CallBuiltin(Builtin::StrIncludes, 3),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_last_index_of_negative_start_clamps() {
        // "hello".lastIndexOf("l", -3) === -1 (only an index-0 match qualifies)
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("l".into()),
            Instr::PushNegInt(-3),
            Instr::CallBuiltin(Builtin::StrLastIndexOf, 3),
        ]);
        assert_eq!(out, vec![Value::NegInt(-1)]);
    }

    // ── StrStartsWith / StrEndsWith ────────────────────────────────────

    #[test]
    fn call_builtin_str_starts_with() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("hel".into()),
            Instr::CallBuiltin(Builtin::StrStartsWith, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_ends_with() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("lo".into()),
            Instr::CallBuiltin(Builtin::StrEndsWith, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    // ── StrSlice ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_slice() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushPosInt(1),
            Instr::PushPosInt(4),
            Instr::CallBuiltin(Builtin::StrSlice, 3),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "ell"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_str_slice_single_arg() {
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::PushPosInt(2),
            Instr::CallBuiltin(Builtin::StrSlice, 2),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "llo"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── StrTrim ────────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_trim() {
        let out = run(vec![
            Instr::PushStr("  hi  ".into()),
            Instr::CallBuiltin(Builtin::StrTrim, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "hi"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── Object.keys / Object.values ────────────────────────────────────

    #[test]
    fn call_builtin_obj_keys() {
        // ObjNew with 2 field names pops 2 values. Push them first.
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ObjNew(vec!["a".into(), "b".into()].into()),
            Instr::CallBuiltin(Builtin::ObjKeys, 1),
        ]);
        // result is an array
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_obj_values() {
        let out = run(vec![
            Instr::PushFloat(5.0),
            Instr::ObjNew(vec!["x".into()].into()),
            Instr::CallBuiltin(Builtin::ObjValues, 1),
        ]);
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    // ── JSON.parse / JSON.stringify ────────────────────────────────────

    #[test]
    fn call_builtin_json_parse() {
        let out = run(vec![
            Instr::PushStr("42".into()),
            Instr::CallBuiltin(Builtin::JSONParse, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_json_stringify() {
        let out = run(vec![
            Instr::PushFloat(3.5),
            Instr::CallBuiltin(Builtin::JSONStringify, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "3.5"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── Number.isInteger / Number.parseInt / Number.parseFloat ─────────

    #[test]
    fn call_builtin_number_is_integer() {
        let out = run(vec![
            Instr::PushPosInt(5),
            Instr::CallBuiltin(Builtin::NumberIsInteger, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_number_parse_int() {
        let out = run(vec![
            Instr::PushStr("42".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_number_parse_int_ignores_trailing() {
        // JS parseInt("42px") === 42 — leading digits, trailing ignored.
        let out = run(vec![
            Instr::PushStr("42px".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_number_parse_int_radix() {
        // parseInt("ff", 16) === 255
        let out = run(vec![
            Instr::PushStr("ff".into()),
            Instr::PushPosInt(16),
            Instr::CallBuiltin(Builtin::NumberParseInt, 2),
        ]);
        assert_eq!(out, vec![Value::PosInt(255)]);
    }

    #[test]
    fn call_builtin_number_parse_int_hex_prefix() {
        // parseInt("0x1A") auto-detects base 16 === 26
        let out = run(vec![
            Instr::PushStr("0x1A".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(26)]);
    }

    #[test]
    fn call_builtin_number_parse_int_negative() {
        let out = run(vec![
            Instr::PushStr("  -17 ".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::NegInt(-17)]);
    }

    #[test]
    fn call_builtin_number_parse_int_nan() {
        // No leading digits → NaN (a Number, not an error).
        let out = run(vec![
            Instr::PushStr("nope".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert!(matches!(out.as_slice(), [Value::Float(n)] if n.is_nan()));
    }

    #[test]
    fn call_builtin_number_parse_int_zero_args_is_error_not_panic() {
        // Regression: a malformed CallBuiltin must not index an empty arg list.
        let mut vm = VM::new(vec![Instr::CallBuiltin(Builtin::NumberParseInt, 0)]);
        assert!(vm.step().unwrap_err().kind == ErrorKind::BadArg);
    }

    #[test]
    fn call_builtin_number_parse_float() {
        let out = run(vec![
            Instr::PushStr("3.14".into()),
            Instr::CallBuiltin(Builtin::NumberParseFloat, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.14)]);
    }

    // ── Array.isArray ──────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_is_array() {
        let out = run(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayIsArray, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_array_is_array_false() {
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::CallBuiltin(Builtin::ArrayIsArray, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(false)]);
    }

    // ── Math ─────────────────────────────────────────────────────────

    #[test]
    fn call_builtin_math_abs() {
        let out = run(vec![
            Instr::PushFloat(-5.0),
            Instr::CallBuiltin(Builtin::MathAbs, 1),
        ]);
        assert_eq!(out, vec![Value::Float(5.0)]);
    }

    #[test]
    fn call_builtin_math_sqrt() {
        let out = run(vec![
            Instr::PushFloat(9.0),
            Instr::CallBuiltin(Builtin::MathSqrt, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);
    }

    #[test]
    fn call_builtin_math_ceil_floor_round() {
        let out = run(vec![
            Instr::PushFloat(2.3),
            Instr::CallBuiltin(Builtin::MathCeil, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);

        let out = run(vec![
            Instr::PushFloat(2.7),
            Instr::CallBuiltin(Builtin::MathFloor, 1),
        ]);
        assert_eq!(out, vec![Value::Float(2.0)]);

        let out = run(vec![
            Instr::PushFloat(2.5),
            Instr::CallBuiltin(Builtin::MathRound, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);
    }

    #[test]
    fn call_builtin_math_sign() {
        let out = run(vec![
            Instr::PushFloat(-7.0),
            Instr::CallBuiltin(Builtin::MathSign, 1),
        ]);
        assert_eq!(out, vec![Value::Float(-1.0)]);
    }

    #[test]
    fn call_builtin_math_max_variadic() {
        let out = run(vec![
            Instr::PushFloat(3.0),
            Instr::PushFloat(9.0),
            Instr::PushFloat(5.0),
            Instr::CallBuiltin(Builtin::MathMax, 3),
        ]);
        assert_eq!(out, vec![Value::Float(9.0)]);
    }

    #[test]
    fn call_builtin_math_max_zero_args() {
        let out = run(vec![Instr::CallBuiltin(Builtin::MathMax, 0)]);
        assert!(matches!(out.as_slice(), [Value::Float(x)] if x.is_infinite() && *x < 0.0));
    }

    #[test]
    fn call_builtin_math_min_variadic() {
        let out = run(vec![
            Instr::PushFloat(3.0),
            Instr::PushFloat(-1.0),
            Instr::PushFloat(5.0),
            Instr::CallBuiltin(Builtin::MathMin, 3),
        ]);
        assert_eq!(out, vec![Value::Float(-1.0)]);
    }

    #[test]
    fn call_builtin_math_pow() {
        let out = run(vec![
            Instr::PushFloat(2.0),
            Instr::PushFloat(3.0),
            Instr::CallBuiltin(Builtin::MathPow, 2),
        ]);
        assert_eq!(out, vec![Value::Float(8.0)]);
    }

    // ── first-class value tests ────────────────────────────────────────

    #[test]
    fn builtin_as_first_class_value_via_calldyn() {
        let out = run(vec![
            Instr::PushFloat(2.0),
            Instr::PushFloat(7.0),
            Instr::PushBuiltin(Builtin::MathMax),
            Instr::CallDyn(2),
        ]);
        assert_eq!(out, vec![Value::Float(7.0)]);
    }

    #[test]
    fn builtin_value_shape() {
        let mut vm = VM::new(vec![Instr::PushBuiltin(Builtin::MathMax), Instr::TypeOf]);
        while !matches!(vm.step().unwrap(), StepResult::Done { .. }) {}
        match vm.stack.last() {
            Some(Value::String(s)) => assert_eq!(s.as_str(), "function"),
            other => panic!("{other:?}"),
        }
    }

    // ── flexible arity (meta-driven) ───────────────────────────────────

    #[test]
    fn builtin_ignores_surplus_args() {
        // A fixed-arity builtin (Math.sqrt, max 1) invoked with extra args
        // (as a callback would be: `(element, index, array)`) drops the surplus
        // and uses only the first argument.
        let out = run(vec![
            Instr::PushFloat(9.0), // the element
            Instr::PushFloat(1.0), // index — ignored
            Instr::PushFloat(7.0), // array stand-in — ignored
            Instr::PushBuiltin(Builtin::MathSqrt),
            Instr::CallDyn(3),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);
    }

    #[test]
    fn builtin_below_min_args_errors() {
        // Math.pow needs 2 args; calling it with 1 is a BadArg error.
        let mut vm = VM::new(vec![
            Instr::PushFloat(2.0),
            Instr::PushBuiltin(Builtin::MathPow),
            Instr::CallDyn(1),
        ]);
        let err = loop {
            match vm.step() {
                Err(e) => break e,
                Ok(StepResult::Done { .. }) => panic!("expected error"),
                Ok(_) => {}
            }
        };
        assert!(err.kind == ErrorKind::BadArg, "got {err:?}");
    }

    #[test]
    fn variadic_builtin_keeps_all_args() {
        // Math.max is variadic (max = u32::MAX): surplus is never trimmed.
        let out = run(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(9.0),
            Instr::PushFloat(4.0),
            Instr::PushBuiltin(Builtin::MathMax),
            Instr::CallDyn(3),
        ]);
        assert_eq!(out, vec![Value::Float(9.0)]);
    }

    // ── Step 1: new Args-based tests ───────────────────────────────────

    #[test]
    fn optional_arg_defaults_via_undefined_rule() {
        // [1,2,3].join() → "1,2,3"
        assert_eq!(
            testutil::run_ret("return [1,2,3].join();"),
            serde_json::json!("1,2,3")
        );
        // "a,b".split(",") → ["a","b"]
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',');"),
            serde_json::json!(["a", "b"])
        );
        // "abc".slice(1) → "bc"
        assert_eq!(
            testutil::run_ret("return 'abc'.slice(1);"),
            serde_json::json!("bc")
        );
    }

    #[test]
    fn resumability_builtin_failure_consumes_operands() {
        // [].pop() → ValueError. After failure, operands should be consumed.
        let mut vm = VM::for_program(
            testutil::compile_ok("return [].pop();"),
            serde_json::Value::Null,
        )
        .unwrap();
        let err = loop {
            match vm.step() {
                Err(e) => break e,
                Ok(StepResult::Done { .. }) => panic!("expected error"),
                Ok(_) => {}
            }
        };
        assert_eq!(err.kind, ErrorKind::ValueError);
        // Stack has operands consumed: only the error-predecessor state should remain.
        // We can resume_with a value.
        vm.resume_with(&err, Value::PosInt(99)).unwrap();
        loop {
            match vm.step().unwrap() {
                StepResult::Done { value } => {
                    assert_eq!(value, Value::PosInt(99));
                    break;
                }
                _ => {}
            }
        }
    }

    // ── Step 2: lookup tests ───────────────────────────────────────────

    #[test]
    fn for_method_lookup() {
        assert_eq!(Builtin::for_method("push"), Some(Builtin::ArrayPush));
        assert_eq!(Builtin::for_method("trim"), Some(Builtin::StrTrim));
        assert_eq!(Builtin::for_method("abs"), None); // namespace, not method
        assert_eq!(Builtin::for_method("nope"), None);
    }

    #[test]
    fn for_namespace_lookup() {
        assert_eq!(
            Builtin::for_namespace("Math", "abs"),
            Some(Builtin::MathAbs)
        );
        assert_eq!(
            Builtin::for_namespace("Object", "keys"),
            Some(Builtin::ObjKeys)
        );
        assert_eq!(Builtin::for_namespace("Math", "push"), None);
        assert_eq!(Builtin::for_namespace("Foo", "bar"), None);
    }
}

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
                // Runtime arity: only require the receiver to be present for
                // methods; compile-time arity is strict (compiler lint).
                let min_runtime = match self.meta().kind {
                    BuiltinKind::Method => 1,
                    BuiltinKind::Namespace(_) => 0,
                };
                if argc < min_runtime {
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
    ArrayReverse, BuiltinKind::Method, "reverse",     1, 1,      array_reverse;
    ArrayFlat,    BuiltinKind::Method, "flat",        1, 2,      array_flat;
    ArrayFill,    BuiltinKind::Method, "fill",        2, VARARG, array_fill;
    ArraySplice,  BuiltinKind::Method, "splice",      1, VARARG, array_splice;
    // ── string methods (Method, receiver + args) ──
    StrSplit,       BuiltinKind::Method, "split",       2, 3, str_split;
    StrIncludes,    BuiltinKind::Method, "includes",    2, 3, includes_poly;
    StrIndexOf,     BuiltinKind::Method, "indexOf",     2, 3, index_of_poly;
    StrLastIndexOf, BuiltinKind::Method, "lastIndexOf", 2, 3, last_index_of_poly;
    StrStartsWith,  BuiltinKind::Method, "startsWith",  2, 2, str_starts_with;
    StrEndsWith,    BuiltinKind::Method, "endsWith",    2, 2, str_ends_with;
    StrSlice,       BuiltinKind::Method, "slice",       2, 3, slice_poly;
    StrTrim,        BuiltinKind::Method, "trim",        1, 1, str_trim;
    StrReplace,      BuiltinKind::Method, "replace",      3, 3, str_replace;
    StrReplaceAll,   BuiltinKind::Method, "replaceAll",   3, 3, str_replace_all;
    StrToLowerCase,  BuiltinKind::Method, "toLowerCase",  1, 1, str_to_lower_case;
    StrToUpperCase,  BuiltinKind::Method, "toUpperCase",  1, 1, str_to_upper_case;
    StrPadStart,     BuiltinKind::Method, "padStart",     2, 3, str_pad_start;
    StrPadEnd,       BuiltinKind::Method, "padEnd",       2, 3, str_pad_end;
    StrRepeat,       BuiltinKind::Method, "repeat",       2, 2, str_repeat;
    StrTrimStart,    BuiltinKind::Method, "trimStart",    1, 1, str_trim_start;
    StrTrimEnd,      BuiltinKind::Method, "trimEnd",      1, 1, str_trim_end;
    StrCharAt,       BuiltinKind::Method, "charAt",       2, 2, str_char_at;
    StrAt,           BuiltinKind::Method, "at",           2, 2, at_poly;
    StrConcat,       BuiltinKind::Method, "concat",       1, VARARG, concat_poly;
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
    // ── console ──
    ConsoleLog,  BuiltinKind::Namespace("console"), "log",  0, VARARG, console_log;
    ConsoleWarn, BuiltinKind::Namespace("console"), "warn", 0, VARARG, console_warn;
    ConsoleError,BuiltinKind::Namespace("console"), "error",0, VARARG, console_error;
    ConsoleInfo, BuiltinKind::Namespace("console"), "info", 0, VARARG, console_info;
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

/// JS `parseFloat(string)`: skip leading whitespace, accept a sign, "Infinity"
/// (with optional sign), and the longest decimal/hexadecimal numeric prefix.
/// Returns `NaN` when no digits are found. Never errors.
fn js_parse_float(input: &str) -> f64 {
    let s = input.trim_start();
    // Infinity check: case-insensitive, matches "Infinity" or "+Infinity" or "-Infinity"
    // as the numeric prefix.
    let lower = s.to_ascii_lowercase();
    for prefix in ["infinity", "+infinity", "-infinity"] {
        if lower.starts_with(prefix) {
            if s.starts_with('-') {
                return f64::NEG_INFINITY;
            }
            return f64::INFINITY;
        }
    }
    // Longest-prefix parse: find the end of the numeric prefix.
    let bytes = s.as_bytes();
    let mut i = 0;
    // Optional sign
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    // Detect hex: 0x / 0X
    let is_hex = i + 1 < bytes.len() && bytes[i] == b'0' && (bytes[i + 1] | 0x20) == b'x';
    if is_hex {
        i += 2;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i == start {
            return f64::NAN;
        }
        // Parse hex prefix; fall through to standard parse for correctness.
        // The hex integer can be large — use standard parse handling on the
        // recognized prefix.
        return s[..i].parse::<f64>().unwrap_or(f64::NAN);
    }
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    // Check for exponent
    if i < bytes.len() && (bytes[i] | 0x20) == b'e' {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == start {
        return f64::NAN;
    }
    s[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `Math.round`: half toward +∞.
fn js_round(n: f64) -> f64 {
    // If already integral (or NaN/±∞), return unchanged.
    if n.fract() == 0.0 || n.is_nan() || n.is_infinite() {
        return n;
    }
    // For |n| ≥ 2^52, all representable values are integers.
    if n.abs() >= (1u64 << 52) as f64 {
        return n;
    }
    // Round half toward +∞: floor(x + 0.5), but -0.5 → -0.
    if n == -0.5 {
        return -0.0_f64;
    }
    (n + 0.5).floor()
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

/// `arr.pop()` → removes and returns the last element, or `undefined` if empty.
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
    // JS: empty array → undefined, not an error.
    Ok(arr.pop().unwrap_or(Value::Undefined))
}

/// `arr.shift()` → removes and returns the first element, or `undefined` if empty.
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
    // JS: empty array → undefined, not an error.
    if arr.is_empty() {
        return Ok(Value::Undefined);
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

/// `arr.reverse()` → reverses in-place, returns the receiver.
fn array_reverse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    arr.reverse();
    Ok(Value::Array(arr_ptr))
}

/// `arr.flat([depth])` → flattens nested arrays to the given depth (default 1).
fn array_flat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let depth = match args.get(vm, 1) {
        Value::Undefined => 1usize,
        v => v
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
            as usize,
    };
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let mut result: ThinVec<Value> = ThinVec::new();
    flatten_into(vm, arr, depth, &mut result)?;
    Ok(vm.alloc_array(result))
}

fn flatten_into(
    vm: &VM,
    src: &[Value],
    depth: usize,
    out: &mut ThinVec<Value>,
) -> Result<(), VMError> {
    for v in src {
        if depth > 0 {
            if let Value::Array(p) = v {
                let nested = vm
                    .arrays
                    .get(*p as usize)
                    .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad array pointer"))?;
                flatten_into(vm, nested, depth - 1, out)?;
                continue;
            }
        }
        out.push(v.clone());
    }
    Ok(())
}

/// `arr.fill(value[, start[, end]])` → fills in-place, returns the receiver.
fn array_fill(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let value = args.get(vm, 1).clone();
    // Extract start/end values before borrowing arr.
    let start_arg = args.get(vm, 2).clone();
    let end_arg = args.get(vm, 3).clone();
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let len = arr.len() as i64;
    let to_idx = |v: &Value, default: i64| -> i64 {
        if matches!(v, Value::Undefined) {
            return default;
        }
        v.to_number().map(|n| {
            let i = n as i64;
            if i < 0 { (i + len).max(0) } else { i.min(len) }
        }).unwrap_or(default)
    };
    let start = to_idx(&start_arg, 0).max(0) as usize;
    let end = to_idx(&end_arg, len).max(0) as usize;
    let end = end.min(arr.len());
    for i in start..end {
        arr[i] = value.clone();
    }
    Ok(Value::Array(arr_ptr))
}

/// `arr.splice(start[, deleteCount[, ...items]])` → mutates in-place, returns
/// the removed elements.
fn array_splice(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    // Extract all args before mutable borrow.
    let start_val = args.get(vm, 1).clone();
    let del_val = if args.argc >= 3 {
        Some(args.get(vm, 2).clone())
    } else {
        None
    };
    let to_insert: SmallVec<[Value; 8]> = (3..args.argc)
        .map(|i| args.get(vm, i).clone())
        .collect();
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let len = arr.len() as i64;
    let to_idx = |v: &Value| -> i64 {
        if matches!(v, Value::Undefined) {
            return 0;
        }
        v.to_number()
            .map(|n| {
                let i = n as i64;
                if i < 0 { (i + len).max(0) } else { i.min(len) }
            })
            .unwrap_or(0)
    };
    let start = to_idx(&start_val);
    let del_count = match &del_val {
        Some(v) if !matches!(v, Value::Undefined) => {
            let n = v.to_number().unwrap_or(0.0);
            (n as i64).max(0).min(len - start) as usize
        }
        _ => (len - start).max(0) as usize,
    };
    let start = start as usize;
    let removed: ThinVec<Value> = arr.drain(start..start + del_count).collect();
    drop(arr);
    // Insert new items at start position.
    if !to_insert.is_empty() {
        let arr = vm
            .arrays
            .get_mut(arr_ptr as usize)
            .ok_or_else(|| fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
        for v in to_insert.into_iter().rev() {
            arr.insert(start, v);
        }
    }
    Ok(vm.alloc_array(removed))
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
            // JS: ToUint32 coercion — truncate fractional, negative wraps.
            let lim = v
                .to_number()
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
            if lim.is_nan() || lim.is_infinite() || lim < 0.0 {
                None
            } else {
                Some((lim as u32) as usize)
            }
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
        // JS: split fully first, then truncate to limit (not splitn which
        // leaves remainder unsplit in the last entry).
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

/// `s.includes(needle[, start])` → bool. An absent needle is coerced to
/// the string `"undefined"` (matching JS).
fn str_includes(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let start = clamp_start(haystack, start.max(0) as usize);
    Ok(Value::Bool(haystack[start..].contains(needle.as_str())))
}

/// `s.indexOf(needle[, start])` → int (or -1). An absent needle is coerced to
/// the string `"undefined"` (matching JS).
fn str_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let start = clamp_start(haystack, start.max(0) as usize);
    let pos = haystack[start..].find(needle.as_str()).map(|p| (p + start) as f64);
    Ok(int_value(pos.unwrap_or(-1.0)))
}

/// `s.lastIndexOf(needle[, start])` → int (or -1). An absent needle is
/// coerced to the string `"undefined"` (matching JS).
fn str_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => haystack.len() as i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let from = start.max(0) as usize;
    let end = clamp_end(haystack, from + needle.len());
    let pos = haystack[..end].rfind(needle.as_str()).map(|p| p as f64);
    Ok(int_value(pos.unwrap_or(-1.0)))
}

/// `s.startsWith(prefix)` → bool. An absent prefix is coerced to the string
/// `"undefined"` (matching JS).
fn str_starts_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let prefix = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::Bool(haystack.starts_with(prefix.as_str())))
}

/// `s.endsWith(suffix)` → bool. An absent suffix is coerced to the string
/// `"undefined"` (matching JS).
fn str_ends_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = vm.str_from(args.get(vm, 0))?;
    let suffix = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::Bool(haystack.ends_with(suffix.as_str())))
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

/// `s.replace(pattern, replacement)` — string-only patterns.
fn str_replace(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    // JS: pattern must be a string; regex is unsupported.
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    let replacement = vm.to_js_string(args.get(vm, 2), 0);
    // Replace only the first occurrence.
    if let Some(idx) = s.find(pattern.as_str()) {
        let mut out = String::with_capacity(s.len() - pattern.len() + replacement.len());
        out.push_str(&s[..idx]);
        out.push_str(replacement.as_str());
        out.push_str(&s[idx + pattern.len()..]);
        Ok(Value::String(RcStr::from(out)))
    } else {
        Ok(Value::String(s))
    }
}

/// `s.replaceAll(pattern, replacement)` — string-only patterns.
fn str_replace_all(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    let replacement = vm.to_js_string(args.get(vm, 2), 0);
    Ok(Value::String(RcStr::from(s.replace(
        pattern.as_str(),
        replacement.as_str(),
    ))))
}

/// `s.toLowerCase()` → lowercase string.
fn str_to_lower_case(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    Ok(Value::String(RcStr::from(s.to_lowercase())))
}

/// `s.toUpperCase()` → uppercase string.
fn str_to_upper_case(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    Ok(Value::String(RcStr::from(s.to_uppercase())))
}

/// `s.padStart(targetLength[, padString])` → padded string.
fn str_pad_start(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let len = args.get(vm, 1);
    let target_len = len
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        as usize;
    let pad: RcStr = match args.get(vm, 2) {
        Value::Undefined => RcStr::from(" "),
        v => vm.to_js_string(v, 0),
    };
    if s.len() >= target_len || pad.is_empty() {
        return Ok(Value::String(s));
    }
    let needed = target_len - s.len();
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut out = String::with_capacity(target_len);
    for i in 0..needed {
        out.push(pad_chars[i % pad_chars.len()]);
    }
    out.push_str(&s);
    Ok(Value::String(RcStr::from(out)))
}

/// `s.padEnd(targetLength[, padString])` → padded string.
fn str_pad_end(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let len = args.get(vm, 1);
    let target_len = len
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        as usize;
    let pad: RcStr = match args.get(vm, 2) {
        Value::Undefined => RcStr::from(" "),
        v => vm.to_js_string(v, 0),
    };
    if s.len() >= target_len || pad.is_empty() {
        return Ok(Value::String(s));
    }
    let needed = target_len - s.len();
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut out = String::with_capacity(target_len);
    out.push_str(&s);
    for i in 0..needed {
        out.push(pad_chars[i % pad_chars.len()]);
    }
    Ok(Value::String(RcStr::from(out)))
}

/// `s.repeat(count)` → repeated string. Negative counts → ValueError.
fn str_repeat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let count = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    if count < 0.0 || count.is_infinite() {
        return Err(vm.fail(ErrorKind::ValueError, "value error"));
    }
    let n = (count as usize).min(10_000); // reasonable cap
    Ok(Value::String(RcStr::from(s.as_str().repeat(n))))
}

/// `s.trimStart()` → left-trimmed string.
fn str_trim_start(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    Ok(Value::String(RcStr::from(s.trim_start())))
}

/// `s.trimEnd()` → right-trimmed string.
fn str_trim_end(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    Ok(Value::String(RcStr::from(s.trim_end())))
}

/// `s.charAt(index)` → single character (UTF-8 byte range) or empty string.
fn str_char_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        as i64;
    if idx < 0 || idx as usize >= s.len() {
        return Ok(Value::String(RcStr::from("")));
    }
    let byte = s.as_bytes()[idx as usize];
    // Return the single byte as a char (charAt is per-byte in our string model)
    Ok(Value::String(RcStr::from(
        (byte as char).to_string(),
    )))
}

/// `s.at(index)` → character at index (negative counts from end), or undefined.
fn str_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let len = s.len() as i64;
    let i = if idx < 0.0 { idx as i64 + len } else { idx as i64 };
    if i < 0 || i as usize >= s.len() {
        return Ok(Value::Undefined);
    }
    let byte = s.as_bytes()[i as usize];
    Ok(Value::String(RcStr::from(
        (byte as char).to_string(),
    )))
}

/// `s.concat(str1, str2, …)` → concatenated string. Receiver must be a string.
fn str_concat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut out = vm.to_js_string(args.get(vm, 0), 0).to_string();
    for i in 1..args.argc {
        let piece = vm.to_js_string(args.get(vm, i), 0);
        out.push_str(piece.as_str());
    }
    Ok(Value::String(RcStr::from(out)))
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
    // Coerce to string (undefined → "undefined") matching JS.
    let s = vm.to_js_string(args.get(vm, 0), 0);
    let radix = match args.get(vm, 1) {
        Value::Undefined => 0,
        v => match v.to_number() {
            Some(n) if n.is_finite() => n as i64,
            _ => 0,
        },
    };
    Ok(int_value(js_parse_int(s.as_str(), radix)))
}

/// `Number.parseFloat(s)` → float. JS semantics: skip leading whitespace, take
/// the longest numeric prefix, return NaN on failure, accept `Infinity`.
fn number_parse_float(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.str_from(args.get(vm, 0))?;
    let n = js_parse_float(&s);
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
/// JS `Math.round`: rounds half toward +∞ (not away-from-zero like Rust).
fn math_round(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let n = args
        .get(vm, 0)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    Ok(Value::Float(js_round(n)))
}

/// JS `Math.sign`: returns the input unchanged when n == 0.0 or -0.0, else
/// the signum. (Rust `signum` returns ±1 for ±0; JS returns ±0.)
fn math_sign(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let n = args
        .get(vm, 0)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    if n == 0.0 {
        // Preserve sign: +0 → +0, -0 → -0
        Ok(Value::Float(n))
    } else {
        Ok(Value::Float(n.signum()))
    }
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
/// +Infinity. JS: NaN propagates (the first NaN encountered wins).
fn math_min(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut acc = f64::INFINITY;
    for i in 0..args.argc {
        let num = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if num.is_nan() {
            return Ok(Value::Float(f64::NAN));
        }
        acc = acc.min(num);
    }
    Ok(Value::Float(acc))
}

/// `Math.max(...nums)` → the largest, ToNumber-coercing each. Zero args →
/// -Infinity. JS: NaN propagates (the first NaN encountered wins).
fn math_max(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut acc = f64::NEG_INFINITY;
    for i in 0..args.argc {
        let num = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if num.is_nan() {
            return Ok(Value::Float(f64::NAN));
        }
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

// ── console implementations ──────────────────────────────────────────────────

/// Maximum number of lines in the console buffer.
const CONSOLE_CAP: usize = 256;
/// Maximum bytes per line; longer lines are truncated with a trailing `…`.
const CONSOLE_LINE_CAP: usize = 4096;

fn console_log(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "")
}
fn console_warn(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "[warn] ")
}
fn console_error(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "[error] ")
}
fn console_info(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "")
}

fn console_write(vm: &mut VM, args: Args, prefix: &str) -> Result<Value, VMError> {
    let mut line = String::new();
    for i in 0..args.argc {
        if i > 0 {
            line.push(' ');
        }
        let s = match args.get(vm, i) {
            Value::String(s) => s.as_str().to_owned(),
            other => {
                match vm.stack_value_to_json(other, 2) {
                    Ok(serde_json::Value::String(s)) => s,
                    Ok(j) => serde_json::to_string(&j).unwrap_or_default(),
                    Err(_) => "[unserializable]".to_string(),
                }
            }
        };
        line.push_str(&s);
    }
    // Truncate long lines.
    if line.len() > CONSOLE_LINE_CAP {
        line.truncate(CONSOLE_LINE_CAP - 3);
        line.push('…');
    }
    let full = if prefix.is_empty() {
        line
    } else {
        format!("{prefix}{line}")
    };
    // Ring-buffer logic.
    if vm.console_lines.len() >= CONSOLE_CAP {
        let dropped = vm.console_lines.len() - CONSOLE_CAP + 1;
        vm.console_lines.drain(0..dropped);
        vm.console_lines
            .push(format!("[… {dropped} lines dropped]"));
    }
    vm.console_lines.push(full);
    Ok(Value::Undefined)
}

// ── polymorphic handlers (dispatch on receiver: string vs array) ─────────────

fn slice_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_slice(vm, args),
        Value::Array(_) => array_slice_builtin(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

fn includes_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_includes(vm, args),
        Value::Array(_) => array_includes(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

fn index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_index_of(vm, args),
        Value::Array(_) => array_index_of(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

fn last_index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_last_index_of(vm, args),
        Value::Array(_) => array_last_index_of(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

fn at_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_at(vm, args),
        Value::Array(_) => array_at(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

fn concat_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_concat(vm, args),
        Value::Array(_) => array_concat(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

/// `arr.slice(start[, end])` → new array, subset of the original.
fn array_slice_builtin(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let len = arr.len() as i64;
    let to_idx = |v: &Value, default: i64| -> i64 {
        if matches!(v, Value::Undefined) {
            return default;
        }
        v.to_number()
            .map(|n| {
                let i = n as i64;
                if i < 0 { (i + len).max(0) } else { i.min(len) }
            })
            .unwrap_or(default)
    };
    let start = to_idx(args.get(vm, 1), 0).max(0) as usize;
    let end = to_idx(args.get(vm, 2), len).max(0) as usize;
    let end = end.min(arr.len());
    if start >= end {
        return Ok(vm.alloc_array(ThinVec::new()));
    }
    let subset: ThinVec<Value> = arr[start..end].iter().cloned().collect();
    Ok(vm.alloc_array(subset))
}

fn array_includes(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let needle = args.get(vm, 1);
    let start = args.get(vm, 2).to_number().unwrap_or(0.0) as usize;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    for v in &arr[start.min(arr.len())..] {
        if v == needle {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn array_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let needle = args.get(vm, 1);
    let start = args.get(vm, 2).to_number().unwrap_or(0.0) as i64;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let from = start.max(0) as usize;
    for (i, v) in arr.iter().enumerate().skip(from) {
        if v == needle {
            return Ok(int_value(i as f64));
        }
    }
    Ok(Value::NegInt(-1))
}

fn array_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let needle = args.get(vm, 1);
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    // Default start is last index + needle length (JS behavior)
    let start = match args.get(vm, 2) {
        Value::Undefined => arr.len() as i64,
        v => v.to_number().unwrap_or(arr.len() as f64) as i64,
    };
    let end = (start + 1).min(arr.len() as i64).max(0) as usize;
    for (i, v) in arr.iter().enumerate().take(end).rev() {
        if v == needle {
            return Ok(int_value(i as f64));
        }
    }
    Ok(Value::NegInt(-1))
}

fn array_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let idx = args.get(vm, 1).to_number().unwrap_or(0.0);
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let len = arr.len() as i64;
    let i = if idx < 0.0 { idx as i64 + len } else { idx as i64 };
    if i < 0 || i as usize >= arr.len() {
        return Ok(Value::Undefined);
    }
    Ok(arr[i as usize].clone())
}

fn array_concat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut result: ThinVec<Value> = arr.clone();
    for i in 1..args.argc {
        match args.get(vm, i) {
            Value::Array(p) => {
                let other = vm
                    .arrays
                    .get(*p as usize)
                    .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
                result.extend(other.iter().cloned());
            }
            v => result.push(v.clone()),
        }
    }
    Ok(vm.alloc_array(result))
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
    fn call_builtin_array_pop_empty_returns_undefined() {
        let out = run(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert_eq!(out, vec![Value::Undefined]);
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
    fn call_builtin_array_shift_empty_returns_undefined() {
        let out = run(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert_eq!(out, vec![Value::Undefined]);
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
    fn call_builtin_number_parse_int_zero_args_is_not_an_error() {
        // Runtime: namespace builtins accept >= 0 args; absent string → "undefined",
        // parseInt("undefined") → NaN.
        let out = run(vec![Instr::CallBuiltin(Builtin::NumberParseInt, 0)]);
        assert!(matches!(out.as_slice(), [Value::Float(n)] if n.is_nan()));
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
        // Math.pow with 1 arg at runtime: the compile-time lint catches this
        // statically; at runtime it now succeeds (NaN^undefined → NaN).
        // Instead, test with 0 args (bad anyway) — Math.pow needs at least 1.
        // Actually: min_args for namespaced builtins at runtime is 0, so
        // Math.pow with 0 args returns NaN (pow of no args → NaN).
        let out = run(vec![
            Instr::PushBuiltin(Builtin::MathPow),
            Instr::CallDyn(0),
        ]);
        assert!(matches!(out.as_slice(), [Value::Float(n)] if n.is_nan()));
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
        // A wrong-receiver call (number.pop()) → TypeError. After failure,
        // operands should be consumed.
        let mut vm = VM::for_program(
            testutil::compile_ok("return (42).pop();"),
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
        assert_eq!(err.kind, ErrorKind::TypeError);
        // Stack has operands consumed.
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

    // ── Step 3: JS contract fixes ─────────────────────────────────────

    #[test]
    fn js_pop_shift_empty_returns_undefined() {
        // JS: [].pop() === undefined, [].shift() === undefined
        assert_eq!(
            testutil::run_ret("return [[].pop(), [].shift()];"),
            serde_json::json!([null, null])
        );
    }

    #[test]
    fn js_split_limit_truncates_not_splitn() {
        // JS: "a,b,c".split(",", 2) → ["a", "b"]
        assert_eq!(
            testutil::run_ret("return 'a,b,c'.split(',', 2);"),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn js_split_empty_string_to_chars() {
        // JS: "abc".split("") → ["a", "b", "c"]
        assert_eq!(
            testutil::run_ret("return 'abc'.split('');"),
            serde_json::json!(["a", "b", "c"])
        );
    }

    #[test]
    fn js_split_limit_coercion() {
        // JS: limit 0 → []; negative → effectively no limit
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',', 0);"),
            serde_json::json!([])
        );
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',', -1);"),
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',', 2.9);"),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn js_parse_float_trailing_garbage_and_infinity() {
        // JS: Number.parseFloat("3.14abc") === 3.14
        assert_eq!(
            testutil::run_ret("return Number.parseFloat('3.14abc');"),
            serde_json::json!(3.14)
        );
        // JS: Number.parseFloat("abc") → NaN
        let v = testutil::run_val("return Number.parseFloat('abc');");
        assert!(matches!(v, Value::Float(n) if n.is_nan()));
        // JS: Number.parseFloat("  2.5") === 2.5
        assert_eq!(
            testutil::run_ret("return Number.parseFloat('  2.5');"),
            serde_json::json!(2.5)
        );
        // JS: Number.parseFloat("Infinity") === Infinity
        let v = testutil::run_val("return Number.parseFloat('Infinity');");
        assert!(matches!(v, Value::Float(n) if n.is_infinite() && n > 0.0));
    }

    #[test]
    fn js_math_round_half_toward_positive_infinity() {
        // JS: Math.round(2.5) === 3, Math.round(-2.5) === -2
        assert_eq!(testutil::eval("Math.round(2.5)"), Value::Float(3.0));
        assert_eq!(testutil::eval("Math.round(-2.5)"), Value::Float(-2.0));
        assert_eq!(testutil::eval("Math.round(3.4)"), Value::Float(3.0));
        // -0.5 → -0 in JS (sign preserved). Use direct VM to pass -0.5.
        let out = run(vec![
            Instr::PushFloat(-0.5),
            Instr::CallBuiltin(Builtin::MathRound, 1),
        ]);
        assert!(
            matches!(out.as_slice(), [Value::Float(f)] if *f == 0.0 && f.is_sign_negative()),
            "Math.round(-0.5) should be -0, got {out:?}"
        );
    }

    #[test]
    fn js_math_sign_returns_zero_not_one() {
        // Math.sign(0) → 0
        let v = testutil::eval("Math.sign(0)");
        assert!(matches!(v, Value::Float(f) if f == 0.0));
        // Math.sign(-0) → -0 (use direct VM to produce -0.0)
        let out = run(vec![
            Instr::PushFloat(-0.0_f64),
            Instr::CallBuiltin(Builtin::MathSign, 1),
        ]);
        assert!(matches!(out.as_slice(), [Value::Float(f)] if *f == 0.0 && f.is_sign_negative()));
    }

    #[test]
    fn js_math_min_max_nan_propagates() {
        // JS: Math.min(1, NaN) → NaN
        let v = testutil::eval("Math.min(1, NaN)");
        assert!(matches!(v, Value::Float(n) if n.is_nan()));
        // JS: Math.max(1, NaN) → NaN
        let v = testutil::eval("Math.max(1, NaN)");
        assert!(matches!(v, Value::Float(n) if n.is_nan()));
    }

    #[test]
    fn js_slice_negative_indexes_and_clamping() {
        // JS: "abcdef".slice(-3) → "def"
        assert_eq!(
            testutil::run_ret("return 'abcdef'.slice(-3);"),
            serde_json::json!("def")
        );
        // JS: "abc".slice(2, 1) → ""
        assert_eq!(
            testutil::run_ret("return 'abc'.slice(2, 1);"),
            serde_json::json!("")
        );
        // JS: "abc".slice(0, 99) → "abc"
        assert_eq!(
            testutil::run_ret("return 'abc'.slice(0, 99);"),
            serde_json::json!("abc")
        );
    }

    #[test]
    fn js_static_arity_is_strict_runtime_is_relaxed() {
        // Static: 'a,b'.split(',', 2, 3) is still a compile error (surplus args).
        let errs = testutil::compile_errs("'a,b'.split(',', 2, 3);");
        let msg = errs.join("\n");
        assert!(msg.contains("split"), "expected split arity error, got: {msg}");

        // Static: 'abc'.split() is still a compile error (too few args).
        // The static compiler requires recv + delim for split.
        let errs = testutil::compile_errs("'abc'.split();");
        let msg = errs.join("\n");
        assert!(msg.contains("split"), "expected split arity error, got: {msg}");

        // Runtime: calling split with only a receiver via direct VM call.
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::CallBuiltin(Builtin::StrSplit, 1),
        ]);
        // Should return ["hello"] (split with undefined delimiter → [self])
        match &out[0] {
            Value::Array(_) => {} // pass
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn js_includes_absent_needle_coerces_to_string_undefined() {
        // Runtime via direct VM: calling includes with only a receiver.
        let out = run(vec![
            Instr::PushStr("undefined!".into()),
            Instr::CallBuiltin(Builtin::StrIncludes, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn js_slice_no_args_returns_whole_string() {
        // Runtime via direct VM: slice with no args returns the whole string.
        let out = run(vec![
            Instr::PushStr("hello".into()),
            Instr::CallBuiltin(Builtin::StrSlice, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "hello"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── Step 4b: string method tests ──────────────────────────────────

    #[test]
    fn string_replace_and_replace_all() {
        // replace only the first occurrence.
        assert_eq!(
            testutil::run_ret("return 'aba'.replace('a', 'x');"),
            serde_json::json!("xba")
        );
        // replaceAll replaces all.
        assert_eq!(
            testutil::run_ret("return 'aba'.replaceAll('a', 'x');"),
            serde_json::json!("xbx")
        );
    }

    #[test]
    fn string_case_methods() {
        assert_eq!(
            testutil::run_ret("return 'Hello'.toLowerCase();"),
            serde_json::json!("hello")
        );
        assert_eq!(
            testutil::run_ret("return 'Hello'.toUpperCase();"),
            serde_json::json!("HELLO")
        );
    }

    #[test]
    fn string_pad() {
        assert_eq!(
            testutil::run_ret("return '5'.padStart(3, '0');"),
            serde_json::json!("005")
        );
        assert_eq!(
            testutil::run_ret("return '5'.padEnd(3, '0');"),
            serde_json::json!("500")
        );
    }

    #[test]
    fn string_repeat() {
        assert_eq!(
            testutil::run_ret("return 'ab'.repeat(0);"),
            serde_json::json!("")
        );
        assert_eq!(
            testutil::run_ret("return 'ab'.repeat(2);"),
            serde_json::json!("abab")
        );
        // Negative → ValueError (RangeError in JS).
        assert_eq!(
            testutil::run_err_kind("return 'ab'.repeat(-1);"),
            ErrorKind::ValueError
        );
    }

    #[test]
    fn string_trim_variants() {
        assert_eq!(
            testutil::run_ret("return '  a '.trimStart();"),
            serde_json::json!("a ")
        );
        assert_eq!(
            testutil::run_ret("return '  a '.trimEnd();"),
            serde_json::json!("  a")
        );
    }

    #[test]
    fn string_char_at_and_at() {
        // charAt returns empty string for OOB.
        assert_eq!(
            testutil::run_ret("return 'abc'.charAt(5);"),
            serde_json::json!("")
        );
        // at returns undefined for OOB.
        let v = testutil::run_val("return 'abc'.at(5);");
        assert_eq!(v, Value::Undefined);
        // at with negative index.
        assert_eq!(
            testutil::run_ret("return 'abc'.at(-1);"),
            serde_json::json!("c")
        );
    }

    #[test]
    fn string_concat() {
        assert_eq!(
            testutil::run_ret("return 'a'.concat('b', 'c');"),
            serde_json::json!("abc")
        );
    }

    // ── Step 4c: array method tests ───────────────────────────────────

    #[test]
    fn array_reverse() {
        assert_eq!(
            testutil::run_ret("return [1,2,3].reverse();"),
            serde_json::json!([3, 2, 1])
        );
    }

    #[test]
    fn array_flat() {
        // Default depth 1.
        assert_eq!(
            testutil::run_ret("return [1,[2,[3]]].flat();"),
            serde_json::json!([1, 2, [3]])
        );
        // Depth 2.
        assert_eq!(
            testutil::run_ret("return [1,[2,[3]]].flat(2);"),
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn array_fill() {
        assert_eq!(
            testutil::run_ret("return [1,2,3].fill(0, 1);"),
            serde_json::json!([1, 0, 0])
        );
    }

    #[test]
    fn array_splice() {
        // splice(1, 2, 9) — delete 2 at index 1, insert 9.
        assert_eq!(
            testutil::run_ret(
                "const a=[1,2,3,4]; const r=a.splice(1,2,9); return [a,r];",
            ),
            serde_json::json!([[1, 9, 4], [2, 3]])
        );
    }

    #[test]
    fn array_polymorphic_methods() {
        // slice
        assert_eq!(
            testutil::run_ret("return [1,2,3].slice(1);"),
            serde_json::json!([2, 3])
        );
        // indexOf
        assert_eq!(
            testutil::run_ret("return [1,2,3].indexOf(2);"),
            serde_json::json!(1)
        );
        // lastIndexOf
        assert_eq!(
            testutil::run_ret("return [1,2,1].lastIndexOf(1);"),
            serde_json::json!(2)
        );
        // includes
        assert_eq!(
            testutil::run_ret("return [1,2,3].includes(3);"),
            serde_json::json!(true)
        );
        // at
        assert_eq!(
            testutil::run_ret("return [1,2,3].at(-1);"),
            serde_json::json!(3)
        );
        // concat
        assert_eq!(
            testutil::run_ret("return [1,2].concat(3, [4]);"),
            serde_json::json!([1, 2, 3, 4])
        );
    }

    // ── Step 4a: console tests ────────────────────────────────────────

    #[test]
    fn console_log_formats_args() {
        let out = run(vec![
            Instr::PushStr("a".into()),
            Instr::PushPosInt(1),
            Instr::PushStr("[2]".into()),
            Instr::CallBuiltin(Builtin::ConsoleLog, 3),
        ]);
        // Returns undefined.
        assert_eq!(out, vec![Value::Undefined]);
    }

    #[test]
    fn console_buffer_readable_after_run() {
        let prog = testutil::compile_ok(
            "console.log('hello', 42); console.warn('oops'); return 1;",
        );
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        loop {
            match vm.step().unwrap() {
                crate::vm::StepResult::Done { .. } => break,
                _ => {}
            }
        }
        // console_lines should have two entries.
        let lines = &vm.console_lines;
        assert_eq!(lines.len(), 2, "got: {lines:?}");
        assert!(lines[0].contains("hello") && lines[0].contains("42"), "line 0: {}", lines[0]);
        assert!(lines[1].contains("[warn]") && lines[1].contains("oops"), "line 1: {}", lines[1]);
    }
}

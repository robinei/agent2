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

use crate::vm::{StackValue, VM, VMError, as_i64};

/// A builtin's identity. Used both as the static call target
/// (`Instr::CallBuiltin(Builtin, argc)`, the compiler's fast path) and as a
/// first-class value (`StackValue::Builtin(Builtin)`, for passing a builtin as
/// a callback — invoked through `CallDyn`). The enum *is* the registry key:
/// `Debug` prints the name and equality is trivial.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Builtin {
    // ── array methods ──
    ArrayPush,
    ArrayPop,
    ArrayShift,
    ArrayUnshift,
    ArrayJoin,
    // ── string methods ──
    StrSplit,
    StrIncludes,
    StrIndexOf,
    StrLastIndexOf,
    StrStartsWith,
    StrEndsWith,
    StrSlice,
    StrTrim,
    // ── object static ──
    ObjKeys,
    ObjValues,
    // ── JSON static ──
    JSONParse,
    JSONStringify,
    // ── Number static ──
    NumberIsInteger,
    NumberParseInt,
    NumberParseFloat,
    // ── Array static ──
    ArrayIsArray,
    // ── Math ──
    MathAbs,
    MathSqrt,
    MathCeil,
    MathFloor,
    MathRound,
    MathSign,
    MathMin,
    MathMax,
    MathPow,
}

/// Compile-time facts about a builtin: its display name and accepted argument
/// count (inclusive, **counting the receiver** for methods). One source of
/// truth the compiler reads for arity checks and error messages.
pub struct BuiltinMeta {
    pub name: &'static str,
    pub min_args: u32,
    pub max_args: u32,
}

impl Builtin {
    pub const fn meta(self) -> BuiltinMeta {
        match self {
            // ── array methods (receiver + args) ──
            Builtin::ArrayPush => BuiltinMeta {
                name: "push",
                min_args: 2, // recv + 1
                max_args: 2,
            },
            Builtin::ArrayPop => BuiltinMeta {
                name: "pop",
                min_args: 1, // recv
                max_args: 1,
            },
            Builtin::ArrayShift => BuiltinMeta {
                name: "shift",
                min_args: 1, // recv
                max_args: 1,
            },
            Builtin::ArrayUnshift => BuiltinMeta {
                name: "unshift",
                min_args: 2, // recv + 1
                max_args: 2,
            },
            Builtin::ArrayJoin => BuiltinMeta {
                name: "join",
                min_args: 1, // recv (separator defaults to "," when argc==1)
                max_args: 2,
            },
            // ── string methods (receiver + args) ──
            Builtin::StrSplit => BuiltinMeta {
                name: "split",
                min_args: 2, // recv + delim
                max_args: 3, // recv + delim + limit
            },
            Builtin::StrIncludes => BuiltinMeta {
                name: "includes",
                min_args: 2, // recv + needle
                max_args: 3, // recv + needle + start
            },
            Builtin::StrIndexOf => BuiltinMeta {
                name: "indexOf",
                min_args: 2, // recv + needle
                max_args: 3, // recv + needle + start
            },
            Builtin::StrLastIndexOf => BuiltinMeta {
                name: "lastIndexOf",
                min_args: 2, // recv + needle
                max_args: 3, // recv + needle + start
            },
            Builtin::StrStartsWith => BuiltinMeta {
                name: "startsWith",
                min_args: 2, // recv + prefix
                max_args: 2,
            },
            Builtin::StrEndsWith => BuiltinMeta {
                name: "endsWith",
                min_args: 2, // recv + suffix
                max_args: 2,
            },
            Builtin::StrSlice => BuiltinMeta {
                name: "slice",
                min_args: 2, // recv + start
                max_args: 3, // recv + start + end
            },
            Builtin::StrTrim => BuiltinMeta {
                name: "trim",
                min_args: 1, // recv
                max_args: 1,
            },
            // ── object static (no receiver) ──
            Builtin::ObjKeys => BuiltinMeta {
                name: "Object.keys",
                min_args: 1,
                max_args: 1,
            },
            Builtin::ObjValues => BuiltinMeta {
                name: "Object.values",
                min_args: 1,
                max_args: 1,
            },
            // ── JSON static (no receiver) ──
            Builtin::JSONParse => BuiltinMeta {
                name: "JSON.parse",
                min_args: 1,
                max_args: 1,
            },
            Builtin::JSONStringify => BuiltinMeta {
                name: "JSON.stringify",
                min_args: 1,
                max_args: 1,
            },
            // ── Number static (no receiver) ──
            Builtin::NumberIsInteger => BuiltinMeta {
                name: "Number.isInteger",
                min_args: 1,
                max_args: 1,
            },
            Builtin::NumberParseInt => BuiltinMeta {
                name: "Number.parseInt",
                min_args: 1,
                max_args: 2,
            },
            Builtin::NumberParseFloat => BuiltinMeta {
                name: "Number.parseFloat",
                min_args: 1,
                max_args: 1,
            },
            // ── Array static (no receiver) ──
            Builtin::ArrayIsArray => BuiltinMeta {
                name: "Array.isArray",
                min_args: 1,
                max_args: 1,
            },
            // ── Math (no receiver) ──
            Builtin::MathAbs => BuiltinMeta {
                name: "Math.abs",
                min_args: 1,
                max_args: 1,
            },
            Builtin::MathSqrt => BuiltinMeta {
                name: "Math.sqrt",
                min_args: 1,
                max_args: 1,
            },
            Builtin::MathCeil => BuiltinMeta {
                name: "Math.ceil",
                min_args: 1,
                max_args: 1,
            },
            Builtin::MathFloor => BuiltinMeta {
                name: "Math.floor",
                min_args: 1,
                max_args: 1,
            },
            Builtin::MathRound => BuiltinMeta {
                name: "Math.round",
                min_args: 1,
                max_args: 1,
            },
            Builtin::MathSign => BuiltinMeta {
                name: "Math.sign",
                min_args: 1,
                max_args: 1,
            },
            Builtin::MathMin => BuiltinMeta {
                name: "Math.min",
                min_args: 0,
                max_args: u32::MAX, // variadic
            },
            Builtin::MathMax => BuiltinMeta {
                name: "Math.max",
                min_args: 0,
                max_args: u32::MAX, // variadic
            },
            Builtin::MathPow => BuiltinMeta {
                name: "Math.pow",
                min_args: 2,
                max_args: 2,
            },
        }
    }

    /// Dispatch: run the builtin against `vm`, consuming `argc` stack arguments
    /// and pushing one result.
    pub fn call(self, vm: &mut VM, argc: u32) -> Result<(), VMError> {
        match self {
            // ── array methods ──
            Builtin::ArrayPush => array_push(vm, argc),
            Builtin::ArrayPop => array_pop(vm, argc),
            Builtin::ArrayShift => array_shift(vm, argc),
            Builtin::ArrayUnshift => array_unshift(vm, argc),
            Builtin::ArrayJoin => array_join(vm, argc),
            // ── string methods ──
            Builtin::StrSplit => str_split(vm, argc),
            Builtin::StrIncludes => str_includes(vm, argc),
            Builtin::StrIndexOf => str_index_of(vm, argc),
            Builtin::StrLastIndexOf => str_last_index_of(vm, argc),
            Builtin::StrStartsWith => str_starts_with(vm, argc),
            Builtin::StrEndsWith => str_ends_with(vm, argc),
            Builtin::StrSlice => str_slice(vm, argc),
            Builtin::StrTrim => str_trim(vm, argc),
            // ── object static ──
            Builtin::ObjKeys => obj_keys(vm, argc),
            Builtin::ObjValues => obj_values(vm, argc),
            // ── JSON static ──
            Builtin::JSONParse => json_parse(vm, argc),
            Builtin::JSONStringify => json_stringify(vm, argc),
            // ── Number static ──
            Builtin::NumberIsInteger => number_is_integer(vm, argc),
            Builtin::NumberParseInt => number_parse_int(vm, argc),
            Builtin::NumberParseFloat => number_parse_float(vm, argc),
            // ── Array static ──
            Builtin::ArrayIsArray => array_is_array(vm, argc),
            // ── Math ──
            Builtin::MathAbs => math_unary(vm, argc, |n| n.abs()),
            Builtin::MathSqrt => math_unary(vm, argc, |n| n.sqrt()),
            Builtin::MathCeil => math_unary(vm, argc, |n| n.ceil()),
            Builtin::MathFloor => math_unary(vm, argc, |n| n.floor()),
            Builtin::MathRound => math_unary(vm, argc, |n| n.round()),
            Builtin::MathSign => math_unary(vm, argc, |n| n.signum()),
            Builtin::MathMin => math_min(vm, argc),
            Builtin::MathMax => math_max(vm, argc),
            Builtin::MathPow => math_pow(vm, argc),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Pop `n` stack values into a Vec (arg 0 first, deepest).
fn pop_args(vm: &mut VM, n: u32) -> Result<Vec<StackValue>, VMError> {
    let n = n as usize;
    if vm.stack.len() < n {
        return Err(VMError::StackUnderflow);
    }
    let start = vm.stack.len() - n;
    Ok(vm.stack.drain(start..).collect())
}

/// Pop exactly `$want` arguments, erroring if `argc` doesn't match.
macro_rules! check_arity {
    ($vm:expr, $argc:expr, $want:expr) => {{
        if $argc as usize != $want {
            return Err(VMError::BadArg);
        }
        pop_args($vm, $argc)
    }};
}

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

/// Build a number `StackValue` from an integer-valued `f64`, mirroring the
/// `PosInt`/`NegInt`/`Number` split used by [`VM::json_to_stack_value`]:
/// non-negative integers that fit become `PosInt`, negative ones `NegInt`, and
/// anything else (fractions, out-of-range magnitudes, NaN/∞) stays `Number`.
fn int_value(n: f64) -> StackValue {
    if n.fract() == 0.0 {
        if (0.0..=u64::MAX as f64).contains(&n) {
            return StackValue::PosInt(n as u64);
        }
        if n < 0.0 && n >= i64::MIN as f64 {
            return StackValue::NegInt(n as i64);
        }
    }
    StackValue::Number(n)
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

/// `arr.push(x)` → appends `x` and returns the new length.
fn array_push(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 2)?;
    let arr_ptr = match &args[0] {
        StackValue::Ptr(p) => *p,
        _ => return Err(VMError::TypeError),
    };
    let val = args[1];
    let arr = vm.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
    arr.push(val);
    let len = arr.len();
    vm.stack.push(StackValue::Number(len as f64));
    Ok(())
}

/// `arr.pop()` → removes and returns the last element.
fn array_pop(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let arr_ptr = match &args[0] {
        StackValue::Ptr(p) => *p,
        _ => return Err(VMError::TypeError),
    };
    let arr = vm.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
    let val = arr.pop().ok_or(VMError::ValueError)?;
    vm.stack.push(val);
    Ok(())
}

/// `arr.shift()` → removes and returns the first element.
fn array_shift(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let arr_ptr = match &args[0] {
        StackValue::Ptr(p) => *p,
        _ => return Err(VMError::TypeError),
    };
    let arr = vm.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
    if arr.is_empty() {
        return Err(VMError::ValueError);
    }
    let val = arr.remove(0);
    vm.stack.push(val);
    Ok(())
}

/// `arr.unshift(x)` → prepends `x` and returns the new length.
fn array_unshift(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 2)?;
    let arr_ptr = match &args[0] {
        StackValue::Ptr(p) => *p,
        _ => return Err(VMError::TypeError),
    };
    let val = args[1];
    let arr = vm.heap_arr_mut(arr_ptr).ok_or(VMError::TypeError)?;
    arr.insert(0, val);
    let len = arr.len();
    vm.stack.push(StackValue::Number(len as f64));
    Ok(())
}

/// `arr.join([sep])` → joins with sep (default ",").
fn array_join(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let (arr_ptr, sep) = match args.len() {
        1 => match &args[0] {
            StackValue::Ptr(p) => (*p, ",".to_string()),
            _ => return Err(VMError::TypeError),
        },
        2 => {
            let p = match &args[0] {
                StackValue::Ptr(p) => *p,
                _ => return Err(VMError::TypeError),
            };
            let s = vm.to_js_string(&args[1], 0);
            (p, s)
        }
        _ => return Err(VMError::BadArg),
    };
    let arr = vm.heap_arr(arr_ptr).ok_or(VMError::TypeError)?;
    let parts: Vec<String> = arr
        .iter()
        .map(|v| match v {
            StackValue::Null | StackValue::Undefined => String::new(),
            _ => vm.to_js_string(v, 0),
        })
        .collect();
    let ptr = vm.alloc_string(parts.join(&sep));
    vm.stack.push(ptr);
    Ok(())
}

// ── string method implementations ────────────────────────────────────────────

/// `s.split(delim[, limit])` → array of substrings.
fn str_split(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let (s, delim, limit) = match args.len() {
        2 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            None,
        ),
        3 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            {
                let lim = as_i64(&args[2]).ok_or(VMError::TypeError)?;
                if lim < 0 {
                    return Err(VMError::ValueError);
                }
                Some(lim as usize)
            },
        ),
        _ => return Err(VMError::BadArg),
    };
    let mut parts = Vec::new();
    match limit {
        Some(lim) => {
            for p in s.splitn(lim, &delim) {
                parts.push(vm.alloc_string(p.to_string()));
            }
        }
        None => {
            for p in s.split(&delim) {
                parts.push(vm.alloc_string(p.to_string()));
            }
        }
    }
    let ptr = vm.alloc_array(parts);
    vm.stack.push(ptr);
    Ok(())
}

/// `s.includes(needle[, start])` → bool.
fn str_includes(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let (haystack, needle, start) = match args.len() {
        2 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            None,
        ),
        3 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            Some(as_i64(&args[2]).ok_or(VMError::TypeError)?),
        ),
        _ => return Err(VMError::BadArg),
    };
    let found = match start {
        // JS clamps the start position into range rather than erroring; a
        // negative value behaves like 0.
        Some(s) => {
            let start = clamp_start(&haystack, s.max(0) as usize);
            haystack[start..].contains(&needle)
        }
        None => haystack.contains(&needle),
    };
    vm.stack.push(StackValue::Bool(found));
    Ok(())
}

/// `s.indexOf(needle[, start])` → int (or -1).
fn str_index_of(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let (haystack, needle, start) = match args.len() {
        2 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            None,
        ),
        3 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            Some(as_i64(&args[2]).ok_or(VMError::TypeError)?),
        ),
        _ => return Err(VMError::BadArg),
    };
    let pos = match start {
        // JS clamps the start position into range; a negative value behaves
        // like 0, and a too-large one only matches the empty needle at len.
        Some(s) => {
            let start = clamp_start(&haystack, s.max(0) as usize);
            haystack[start..].find(&needle).map(|p| (p + start) as f64)
        }
        None => haystack.find(&needle).map(|p| p as f64),
    };
    vm.stack.push(StackValue::Number(pos.unwrap_or(-1.0)));
    Ok(())
}

/// `s.lastIndexOf(needle[, start])` → int (or -1).
fn str_last_index_of(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let (haystack, needle, start) = match args.len() {
        2 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            None,
        ),
        3 => (
            vm.pop_string_from(&args[0])?,
            vm.pop_string_from(&args[1])?,
            Some(as_i64(&args[2]).ok_or(VMError::TypeError)?),
        ),
        _ => return Err(VMError::BadArg),
    };
    let pos = match start {
        // JS searches backward for a match starting at index <= `start`; a
        // negative value behaves like 0 (only an index-0 match qualifies).
        Some(s) => {
            let from = s.max(0) as usize;
            let mut end = haystack.len().min(from + needle.len());
            while end > 0 && !haystack.is_char_boundary(end) {
                end -= 1;
            }
            haystack[..end].rfind(&needle).map(|p| p as f64)
        }
        None => haystack.rfind(&needle).map(|p| p as f64),
    };
    vm.stack.push(StackValue::Number(pos.unwrap_or(-1.0)));
    Ok(())
}

/// `s.startsWith(prefix)` → bool.
fn str_starts_with(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 2)?;
    let haystack = vm.pop_string_from(&args[0])?;
    let prefix = vm.pop_string_from(&args[1])?;
    vm.stack
        .push(StackValue::Bool(haystack.starts_with(&prefix)));
    Ok(())
}

/// `s.endsWith(suffix)` → bool.
fn str_ends_with(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 2)?;
    let haystack = vm.pop_string_from(&args[0])?;
    let suffix = vm.pop_string_from(&args[1])?;
    vm.stack.push(StackValue::Bool(haystack.ends_with(&suffix)));
    Ok(())
}

/// `s.slice(start[, end])` → substring over a half-open byte range. Unlike JS,
/// negative indices are rejected (`ValueError`) rather than counted from the
/// end; `start`/`end` must be in range and on char boundaries. Optional `end`
/// defaults to the string length.
fn str_slice(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let (s, start, end): (String, usize, usize) = match args.len() {
        2 => {
            let s = vm.pop_string_from(&args[0])?;
            let start = as_i64(&args[1]).ok_or(VMError::TypeError)?;
            if start < 0 {
                return Err(VMError::ValueError);
            }
            let start = start as usize;
            let end = s.len();
            (s, start, end)
        }
        3 => {
            let s = vm.pop_string_from(&args[0])?;
            let start = as_i64(&args[1]).ok_or(VMError::TypeError)?;
            let end = as_i64(&args[2]).ok_or(VMError::TypeError)?;
            if start < 0 || end < 0 || start > end {
                return Err(VMError::ValueError);
            }
            (s, start as usize, end as usize)
        }
        _ => return Err(VMError::BadArg),
    };
    if start > s.len() || end > s.len() || !s.is_char_boundary(start) || !s.is_char_boundary(end) {
        return Err(VMError::ValueError);
    }
    let ptr = vm.alloc_string(s[start..end].to_string());
    vm.stack.push(ptr);
    Ok(())
}

/// `s.trim()` → trimmed string.
fn str_trim(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let s = vm.pop_string_from(&args[0])?;
    let ptr = vm.alloc_string(s.trim().to_string());
    vm.stack.push(ptr);
    Ok(())
}

// ── object static implementations ────────────────────────────────────────────

/// `Object.keys(obj)` → array of strings.
fn obj_keys(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let obj_ptr = match &args[0] {
        StackValue::Ptr(p) => *p,
        _ => return Err(VMError::TypeError),
    };
    let keys: Vec<String> = vm
        .heap_obj(obj_ptr)
        .ok_or(VMError::TypeError)?
        .keys()
        .cloned()
        .collect();
    let strs: Vec<StackValue> = keys.into_iter().map(|k| vm.alloc_string(k)).collect();
    let ptr = vm.alloc_array(strs);
    vm.stack.push(ptr);
    Ok(())
}

/// `Object.values(obj)` → array of values.
fn obj_values(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let obj_ptr = match &args[0] {
        StackValue::Ptr(p) => *p,
        _ => return Err(VMError::TypeError),
    };
    let vals: Vec<StackValue> = vm
        .heap_obj(obj_ptr)
        .ok_or(VMError::TypeError)?
        .values()
        .copied()
        .collect();
    let ptr = vm.alloc_array(vals);
    vm.stack.push(ptr);
    Ok(())
}

// ── JSON static implementations ──────────────────────────────────────────────

/// `JSON.parse(s)` → any.
fn json_parse(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let s = vm.pop_string_from(&args[0])?;
    let json: serde_json::Value = serde_json::from_str(&s).map_err(|_| VMError::ValueError)?;
    let val = vm.json_to_stack_value(&json, 0)?;
    vm.stack.push(val);
    Ok(())
}

/// `JSON.stringify(x)` → str.
fn json_stringify(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let json = vm.stack_value_to_json(&args[0], 0)?;
    let s = serde_json::to_string(&json).map_err(|_| VMError::ValueError)?;
    let ptr = vm.alloc_string(s);
    vm.stack.push(ptr);
    Ok(())
}

// ── Number static implementations ────────────────────────────────────────────

/// `Number.isInteger(x)` → bool.
fn number_is_integer(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let is_int = matches!(&args[0], StackValue::PosInt(_) | StackValue::NegInt(_))
        || matches!(&args[0], StackValue::Number(n) if crate::vm::float_is_int(*n));
    vm.stack.push(StackValue::Bool(is_int));
    Ok(())
}

/// `Number.parseInt(s[, radix])` → int (full JS semantics: optional sign,
/// `0x` prefix, any radix in `[2, 36]`, leading-digit parse with trailing
/// characters ignored). Unparseable input yields `NaN`, like the browser.
fn number_parse_int(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    if args.is_empty() || args.len() > 2 {
        return Err(VMError::BadArg);
    }
    let s = vm.pop_string_from(&args[0])?;
    // JS coerces the radix via ToInt32; a missing/NaN radix means "auto" (0).
    let radix = match args.get(1) {
        Some(v) => match vm.to_number(v) {
            Some(n) if n.is_finite() => n as i64,
            _ => 0,
        },
        None => 0,
    };
    vm.stack.push(int_value(js_parse_int(&s, radix)));
    Ok(())
}

/// `Number.parseFloat(s)` → float.
fn number_parse_float(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let s = vm.pop_string_from(&args[0])?;
    let n: f64 = s.trim().parse().map_err(|_| VMError::ValueError)?;
    vm.stack.push(StackValue::Number(n));
    Ok(())
}

// ── Array static implementations ─────────────────────────────────────────────

/// `Array.isArray(x)` → bool.
fn array_is_array(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let is_arr = match &args[0] {
        StackValue::Ptr(p) => vm.heap_arr(*p).is_some(),
        _ => false,
    };
    vm.stack.push(StackValue::Bool(is_arr));
    Ok(())
}

// ── Math implementations ─────────────────────────────────────────────────────

/// Math unary: pop one arg, coerce ToNumber, apply f, push Number.
fn math_unary(vm: &mut VM, argc: u32, f: fn(f64) -> f64) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 1)?;
    let n = vm.to_number(&args[0]).ok_or(VMError::TypeError)?;
    vm.stack.push(StackValue::Number(f(n)));
    Ok(())
}

/// `Math.min(...nums)` → the smallest, ToNumber-coercing each. Zero args →
/// +Infinity. Follows `f64::min` (a NaN operand is ignored).
fn math_min(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let mut acc = f64::INFINITY;
    for v in &args {
        let num = vm.to_number(v).ok_or(VMError::TypeError)?;
        acc = acc.min(num);
    }
    vm.stack.push(StackValue::Number(acc));
    Ok(())
}

/// `Math.max(...nums)` → the largest, ToNumber-coercing each. Zero args →
/// -Infinity. Follows `f64::max` (a NaN operand is ignored).
fn math_max(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = pop_args(vm, argc)?;
    let mut acc = f64::NEG_INFINITY;
    for v in &args {
        let num = vm.to_number(v).ok_or(VMError::TypeError)?;
        acc = acc.max(num);
    }
    vm.stack.push(StackValue::Number(acc));
    Ok(())
}

/// `Math.pow(base, exp)` → base^exp.
fn math_pow(vm: &mut VM, argc: u32) -> Result<(), VMError> {
    let args = check_arity!(vm, argc, 2)?;
    let base = vm.to_number(&args[0]).ok_or(VMError::TypeError)?;
    let exp = vm.to_number(&args[1]).ok_or(VMError::TypeError)?;
    vm.stack.push(StackValue::Number(base.powf(exp)));
    Ok(())
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{HeapValue, Instr, StepResult};

    fn run(code: Vec<Instr>) -> Vec<StackValue> {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm.stack.clone(),
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    // ── ArrayPush ──────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_push_returns_length() {
        let out = run(vec![
            Instr::Push(StackValue::Number(10.0)),
            Instr::ArrNew(1),
            Instr::Dup,
            Instr::Push(StackValue::Number(20.0)),
            Instr::CallBuiltin(Builtin::ArrayPush, 2),
        ]);
        assert_eq!(out.last(), Some(&StackValue::Number(2.0)));
    }

    // ── ArrayPop ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_pop_returns_last() {
        let out = run(vec![
            Instr::Push(StackValue::Number(1.0)),
            Instr::Push(StackValue::Number(2.0)),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(2.0)]);
    }

    #[test]
    fn call_builtin_array_pop_empty_errors() {
        let mut vm = VM::new(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert!(matches!(vm.step(), Err(VMError::ValueError)));
    }

    // ── ArrayShift ─────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_shift_returns_first() {
        let out = run(vec![
            Instr::Push(StackValue::Number(1.0)),
            Instr::Push(StackValue::Number(2.0)),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(1.0)]);
    }

    #[test]
    fn call_builtin_array_shift_empty_errors() {
        let mut vm = VM::new(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert!(matches!(vm.step(), Err(VMError::ValueError)));
    }

    // ── ArrayUnshift ───────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_unshift_returns_length() {
        let out = run(vec![
            Instr::Push(StackValue::Number(2.0)),
            Instr::ArrNew(1),
            Instr::Dup,
            Instr::Push(StackValue::Number(1.0)),
            Instr::CallBuiltin(Builtin::ArrayUnshift, 2),
        ]);
        assert_eq!(out.last(), Some(&StackValue::Number(2.0)));
    }

    // ── ArrayJoin ──────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_join_default_sep() {
        let out = run(vec![
            Instr::Push(StackValue::Number(1.0)),
            Instr::Push(StackValue::Number(2.0)),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayJoin, 1),
        ]);
        // result is a Ptr to a heap string "1,2"
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_array_join_custom_sep() {
        let out = run(vec![
            Instr::Push(StackValue::Number(1.0)),
            Instr::Push(StackValue::Number(2.0)),
            Instr::ArrNew(2),
            Instr::PushStr(" - ".to_string()),
            Instr::CallBuiltin(Builtin::ArrayJoin, 2),
        ]);
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    // ── StrSplit ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_split() {
        let out = run(vec![
            Instr::PushStr("a,b,c".to_string()),
            Instr::PushStr(",".to_string()),
            Instr::CallBuiltin(Builtin::StrSplit, 2),
        ]);
        // result is an array Ptr
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_str_split_with_limit() {
        let out = run(vec![
            Instr::PushStr("a,b,c".to_string()),
            Instr::PushStr(",".to_string()),
            Instr::Push(StackValue::PosInt(2)),
            Instr::CallBuiltin(Builtin::StrSplit, 3),
        ]);
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    // ── StrIncludes ────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_includes() {
        let out = run(vec![
            Instr::PushStr("hello world".to_string()),
            Instr::PushStr("world".to_string()),
            Instr::CallBuiltin(Builtin::StrIncludes, 2),
        ]);
        assert_eq!(out, vec![StackValue::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_includes_not_found() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("x".to_string()),
            Instr::CallBuiltin(Builtin::StrIncludes, 2),
        ]);
        assert_eq!(out, vec![StackValue::Bool(false)]);
    }

    // ── StrIndexOf ─────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_index_of() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("l".to_string()),
            Instr::CallBuiltin(Builtin::StrIndexOf, 2),
        ]);
        assert_eq!(out, vec![StackValue::Number(2.0)]);
    }

    #[test]
    fn call_builtin_str_index_of_not_found() {
        let out = run(vec![
            Instr::PushStr("abc".to_string()),
            Instr::PushStr("x".to_string()),
            Instr::CallBuiltin(Builtin::StrIndexOf, 2),
        ]);
        assert_eq!(out, vec![StackValue::Number(-1.0)]);
    }

    // ── StrLastIndexOf ─────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_last_index_of() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("l".to_string()),
            Instr::CallBuiltin(Builtin::StrLastIndexOf, 2),
        ]);
        assert_eq!(out, vec![StackValue::Number(3.0)]);
    }

    // ── negative `start` clamps to 0, matching JS (rather than failing) ─

    #[test]
    fn call_builtin_str_index_of_negative_start_clamps() {
        // "hello".indexOf("h", -5) === 0
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("h".to_string()),
            Instr::Push(StackValue::NegInt(-5)),
            Instr::CallBuiltin(Builtin::StrIndexOf, 3),
        ]);
        assert_eq!(out, vec![StackValue::Number(0.0)]);
    }

    #[test]
    fn call_builtin_str_includes_negative_start_clamps() {
        // "hello".includes("h", -5) === true
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("h".to_string()),
            Instr::Push(StackValue::NegInt(-5)),
            Instr::CallBuiltin(Builtin::StrIncludes, 3),
        ]);
        assert_eq!(out, vec![StackValue::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_last_index_of_negative_start_clamps() {
        // "hello".lastIndexOf("l", -3) === -1 (only an index-0 match qualifies)
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("l".to_string()),
            Instr::Push(StackValue::NegInt(-3)),
            Instr::CallBuiltin(Builtin::StrLastIndexOf, 3),
        ]);
        assert_eq!(out, vec![StackValue::Number(-1.0)]);
    }

    // ── StrStartsWith / StrEndsWith ────────────────────────────────────

    #[test]
    fn call_builtin_str_starts_with() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("hel".to_string()),
            Instr::CallBuiltin(Builtin::StrStartsWith, 2),
        ]);
        assert_eq!(out, vec![StackValue::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_ends_with() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::PushStr("lo".to_string()),
            Instr::CallBuiltin(Builtin::StrEndsWith, 2),
        ]);
        assert_eq!(out, vec![StackValue::Bool(true)]);
    }

    // ── StrSlice ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_slice() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::Push(StackValue::PosInt(1)),
            Instr::Push(StackValue::PosInt(4)),
            Instr::CallBuiltin(Builtin::StrSlice, 3),
        ]);
        // result is a Ptr to "ell"
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_str_slice_single_arg() {
        let out = run(vec![
            Instr::PushStr("hello".to_string()),
            Instr::Push(StackValue::PosInt(2)),
            Instr::CallBuiltin(Builtin::StrSlice, 2),
        ]);
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    // ── StrTrim ────────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_trim() {
        let out = run(vec![
            Instr::PushStr("  hi  ".to_string()),
            Instr::CallBuiltin(Builtin::StrTrim, 1),
        ]);
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    // ── Object.keys / Object.values ────────────────────────────────────

    #[test]
    fn call_builtin_obj_keys() {
        // ObjNew with 2 field names pops 2 values. Push them first.
        let out = run(vec![
            Instr::Push(StackValue::Number(1.0)),
            Instr::Push(StackValue::Number(2.0)),
            Instr::ObjNew(vec!["a".to_string(), "b".to_string()]),
            Instr::CallBuiltin(Builtin::ObjKeys, 1),
        ]);
        // result is an array Ptr
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_obj_values() {
        let out = run(vec![
            Instr::Push(StackValue::Number(5.0)),
            Instr::ObjNew(vec!["x".to_string()]),
            Instr::CallBuiltin(Builtin::ObjValues, 1),
        ]);
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    // ── JSON.parse / JSON.stringify ────────────────────────────────────

    #[test]
    fn call_builtin_json_parse() {
        let out = run(vec![
            Instr::PushStr("42".to_string()),
            Instr::CallBuiltin(Builtin::JSONParse, 1),
        ]);
        assert_eq!(out, vec![StackValue::PosInt(42)]);
    }

    #[test]
    fn call_builtin_json_stringify() {
        let out = run(vec![
            Instr::Push(StackValue::Number(3.5)),
            Instr::CallBuiltin(Builtin::JSONStringify, 1),
        ]);
        match &out[0] {
            StackValue::Ptr(_) => {}
            other => panic!("expected Ptr, got {other:?}"),
        }
    }

    // ── Number.isInteger / Number.parseInt / Number.parseFloat ─────────

    #[test]
    fn call_builtin_number_is_integer() {
        let out = run(vec![
            Instr::Push(StackValue::PosInt(5)),
            Instr::CallBuiltin(Builtin::NumberIsInteger, 1),
        ]);
        assert_eq!(out, vec![StackValue::Bool(true)]);
    }

    #[test]
    fn call_builtin_number_parse_int() {
        let out = run(vec![
            Instr::PushStr("42".to_string()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![StackValue::PosInt(42)]);
    }

    #[test]
    fn call_builtin_number_parse_int_ignores_trailing() {
        // JS parseInt("42px") === 42 — leading digits, trailing ignored.
        let out = run(vec![
            Instr::PushStr("42px".to_string()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![StackValue::PosInt(42)]);
    }

    #[test]
    fn call_builtin_number_parse_int_radix() {
        // parseInt("ff", 16) === 255
        let out = run(vec![
            Instr::PushStr("ff".to_string()),
            Instr::Push(StackValue::PosInt(16)),
            Instr::CallBuiltin(Builtin::NumberParseInt, 2),
        ]);
        assert_eq!(out, vec![StackValue::PosInt(255)]);
    }

    #[test]
    fn call_builtin_number_parse_int_hex_prefix() {
        // parseInt("0x1A") auto-detects base 16 === 26
        let out = run(vec![
            Instr::PushStr("0x1A".to_string()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![StackValue::PosInt(26)]);
    }

    #[test]
    fn call_builtin_number_parse_int_negative() {
        let out = run(vec![
            Instr::PushStr("  -17 ".to_string()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![StackValue::NegInt(-17)]);
    }

    #[test]
    fn call_builtin_number_parse_int_nan() {
        // No leading digits → NaN (a Number, not an error).
        let out = run(vec![
            Instr::PushStr("nope".to_string()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert!(matches!(out.as_slice(), [StackValue::Number(n)] if n.is_nan()));
    }

    #[test]
    fn call_builtin_number_parse_int_zero_args_is_error_not_panic() {
        // Regression: a malformed CallBuiltin must not index an empty arg list.
        let mut vm = VM::new(vec![Instr::CallBuiltin(Builtin::NumberParseInt, 0)]);
        assert!(matches!(vm.step(), Err(VMError::BadArg)));
    }

    #[test]
    fn call_builtin_number_parse_float() {
        let out = run(vec![
            Instr::PushStr("3.14".to_string()),
            Instr::CallBuiltin(Builtin::NumberParseFloat, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(3.14)]);
    }

    // ── Array.isArray ──────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_is_array() {
        let out = run(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayIsArray, 1),
        ]);
        assert_eq!(out, vec![StackValue::Bool(true)]);
    }

    #[test]
    fn call_builtin_array_is_array_false() {
        let out = run(vec![
            Instr::Push(StackValue::Number(1.0)),
            Instr::CallBuiltin(Builtin::ArrayIsArray, 1),
        ]);
        assert_eq!(out, vec![StackValue::Bool(false)]);
    }

    // ── Math ─────────────────────────────────────────────────────────

    #[test]
    fn call_builtin_math_abs() {
        let out = run(vec![
            Instr::Push(StackValue::Number(-5.0)),
            Instr::CallBuiltin(Builtin::MathAbs, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(5.0)]);
    }

    #[test]
    fn call_builtin_math_sqrt() {
        let out = run(vec![
            Instr::Push(StackValue::Number(9.0)),
            Instr::CallBuiltin(Builtin::MathSqrt, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(3.0)]);
    }

    #[test]
    fn call_builtin_math_ceil_floor_round() {
        let out = run(vec![
            Instr::Push(StackValue::Number(2.3)),
            Instr::CallBuiltin(Builtin::MathCeil, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(3.0)]);

        let out = run(vec![
            Instr::Push(StackValue::Number(2.7)),
            Instr::CallBuiltin(Builtin::MathFloor, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(2.0)]);

        let out = run(vec![
            Instr::Push(StackValue::Number(2.5)),
            Instr::CallBuiltin(Builtin::MathRound, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(3.0)]);
    }

    #[test]
    fn call_builtin_math_sign() {
        let out = run(vec![
            Instr::Push(StackValue::Number(-7.0)),
            Instr::CallBuiltin(Builtin::MathSign, 1),
        ]);
        assert_eq!(out, vec![StackValue::Number(-1.0)]);
    }

    #[test]
    fn call_builtin_math_max_variadic() {
        let out = run(vec![
            Instr::Push(StackValue::Number(3.0)),
            Instr::Push(StackValue::Number(9.0)),
            Instr::Push(StackValue::Number(5.0)),
            Instr::CallBuiltin(Builtin::MathMax, 3),
        ]);
        assert_eq!(out, vec![StackValue::Number(9.0)]);
    }

    #[test]
    fn call_builtin_math_max_zero_args() {
        let out = run(vec![Instr::CallBuiltin(Builtin::MathMax, 0)]);
        assert!(matches!(out.as_slice(), [StackValue::Number(x)] if x.is_infinite() && *x < 0.0));
    }

    #[test]
    fn call_builtin_math_min_variadic() {
        let out = run(vec![
            Instr::Push(StackValue::Number(3.0)),
            Instr::Push(StackValue::Number(-1.0)),
            Instr::Push(StackValue::Number(5.0)),
            Instr::CallBuiltin(Builtin::MathMin, 3),
        ]);
        assert_eq!(out, vec![StackValue::Number(-1.0)]);
    }

    #[test]
    fn call_builtin_math_pow() {
        let out = run(vec![
            Instr::Push(StackValue::Number(2.0)),
            Instr::Push(StackValue::Number(3.0)),
            Instr::CallBuiltin(Builtin::MathPow, 2),
        ]);
        assert_eq!(out, vec![StackValue::Number(8.0)]);
    }

    // ── first-class value tests ────────────────────────────────────────

    #[test]
    fn builtin_as_first_class_value_via_calldyn() {
        let out = run(vec![
            Instr::Push(StackValue::Number(2.0)),
            Instr::Push(StackValue::Number(7.0)),
            Instr::Push(StackValue::Builtin(Builtin::MathMax)),
            Instr::CallDyn(2),
        ]);
        assert_eq!(out, vec![StackValue::Number(7.0)]);
    }

    #[test]
    fn builtin_value_shape() {
        let mut vm = VM::new(vec![
            Instr::Push(StackValue::Builtin(Builtin::MathMax)),
            Instr::TypeOf,
        ]);
        while !matches!(vm.step().unwrap(), StepResult::Done) {}
        match vm.heap.last() {
            Some(HeapValue::String(s)) => assert_eq!(s, "function"),
            other => panic!("{other:?}"),
        }
    }
}

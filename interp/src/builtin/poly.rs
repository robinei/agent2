use crate::builtin::Args;
use crate::builtin::array::{
    array_at, array_concat, array_includes, array_index_of, array_last_index_of,
    array_slice_builtin,
};
use crate::builtin::string::{
    str_at, str_concat, str_includes, str_index_of, str_last_index_of, str_slice,
};
use crate::vm::{ErrorKind, VM, VMError, Value};

// ── polymorphic handlers (dispatch on receiver: string vs array) ─────────────

/// `x.toString([radix])` for any receiver — the general ToString method.
/// Descendents to the same `to_js_string` coercion used by `String(x)` and
/// template interpolation (one renderer behind both). An object's own
/// `toString` shadows the default (the `CallBuiltin`/`dispatch_call` sites
/// intercept Object receivers before calling this handler); a plain object
/// otherwise
/// yields `"[object Object]"`. Nullish receivers never reach here:
/// `null.toString()` errors at the member access.
///
/// Step 2b: for a **number** receiver with a `radix` argument (2..=36),
/// converts to the given base (`(255).toString(16)` → `"ff"`) matching JS
/// `Number.prototype.toString(radix)`. Other receiver types ignore the
/// radix.
pub fn value_to_string(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0);
    // Number with radix: JS `Number.prototype.toString(radix)`.
    if recv.is_number() && args.argc >= 2 {
        let radix = args.get(vm, 1);
        let r = radix
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "toString: radix must be a number"))?;
        if !r.is_finite() || !(2.0..=36.0).contains(&r) {
            return Err(vm.fail(ErrorKind::ValueError, "toString: radix must be in [2, 36]"));
        }
        let radix = r as u32;
        let n = recv.as_f64().unwrap_or(0.0);
        return Ok(Value::String(crate::vm::RcStr::from(
            number_to_radix_string(n, radix),
        )));
    }
    let s = vm.to_js_string(recv, 0);
    Ok(Value::String(s))
}

/// Render a finite number in the given radix (2..=36), matching JS
/// `Number.prototype.toString(radix)`. Handles integers exactly; fractions
/// use a best-effort representation (JS uses an exact round-trip algorithm;
/// here we use Rust's formatting which is close enough for the common
/// cases). NaN → "NaN", ±Infinity → "Infinity"/"-Infinity".
fn number_to_radix_string(n: f64, radix: u32) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let neg = n < 0.0;
    let n = n.abs();
    let int_part = n.trunc() as u64;
    let frac_part = n - int_part as f64;
    let mut int_str = if int_part == 0 {
        "0".to_string()
    } else {
        let mut s = String::new();
        let mut v = int_part;
        while v > 0 {
            let d = (v % radix as u64) as usize;
            s.insert(0, digits[d] as char);
            v /= radix as u64;
        }
        s
    };
    // Fractional part: up to ~20 digits in the target radix.
    if frac_part > 0.0 {
        int_str.push('.');
        let mut f = frac_part;
        for _ in 0..20 {
            f *= radix as f64;
            let d = f.trunc() as u32 as usize;
            if d >= digits.len() {
                break;
            }
            int_str.push(digits[d] as char);
            f -= d as f64;
            if f < 1e-10 {
                break;
            }
        }
    }
    if neg { format!("-{int_str}") } else { int_str }
}

pub fn slice_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_slice(vm, args),
        Value::Array(_) => array_slice_builtin(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn includes_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_includes(vm, args),
        Value::Array(_) => array_includes(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_index_of(vm, args),
        Value::Array(_) => array_index_of(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn last_index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_last_index_of(vm, args),
        Value::Array(_) => array_last_index_of(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn at_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_at(vm, args),
        Value::Array(_) => array_at(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn concat_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_concat(vm, args),
        Value::Array(_) => array_concat(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

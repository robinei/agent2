use crate::builtin::Args;
use crate::vm::{VM, VMError, Value};

// ── Number static implementations ────────────────────────────────────────────

/// `Number.isInteger(x)` → bool.
pub fn number_is_integer(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let v = args.get(vm, 0);
    let is_int = matches!(v, Value::PosInt(_) | Value::NegInt(_))
        || matches!(v, Value::Float(n) if crate::vm::float_is_int(*n));
    Ok(Value::Bool(is_int))
}

/// `Number.parseInt(s[, radix])` → int (full JS semantics: optional sign,
/// `0x` prefix, any radix in `[2, 36]`, leading-digit parse with trailing
/// characters ignored). Unparseable input yields `NaN`, like the browser.
pub fn number_parse_int(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    // Coerce to string (undefined → "undefined") matching JS.
    let s = vm.to_js_string(args.get(vm, 0), 0);
    let radix = match args.get(vm, 1) {
        Value::Undefined => 0,
        v => match v.to_number() {
            Some(n) if n.is_finite() => n as i64,
            _ => 0,
        },
    };
    Ok(Value::int_from_f64(js_parse_int(s.as_str(), radix)))
}

/// `Number.parseFloat(s)` → float. JS semantics: skip leading whitespace, take
/// the longest numeric prefix, return NaN on failure, accept `Infinity`.
pub fn number_parse_float(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.str_from(args.get(vm, 0))?;
    let n = js_parse_float(&s);
    Ok(Value::Float(n))
}

// ── helpers ─────────────────────────────────────────────────────

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

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{
        Value,
        builtin::Builtin,
        testutil::{self, run_instrs},
        vm::Instr,
    };

    // ── Number.isInteger / Number.parseInt / Number.parseFloat ─────────

    #[test]
    fn call_builtin_number_is_integer() {
        let out = run_instrs(vec![
            Instr::PushPosInt(5),
            Instr::CallBuiltin(Builtin::NumberIsInteger, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_number_parse_int() {
        let out = run_instrs(vec![
            Instr::PushStr("42".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_number_parse_int_ignores_trailing() {
        // JS parseInt("42px") === 42 — leading digits, trailing ignored.
        let out = run_instrs(vec![
            Instr::PushStr("42px".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_number_parse_int_radix() {
        // parseInt("ff", 16) === 255
        let out = run_instrs(vec![
            Instr::PushStr("ff".into()),
            Instr::PushPosInt(16),
            Instr::CallBuiltin(Builtin::NumberParseInt, 2),
        ]);
        assert_eq!(out, vec![Value::PosInt(255)]);
    }

    #[test]
    fn call_builtin_number_parse_int_hex_prefix() {
        // parseInt("0x1A") auto-detects base 16 === 26
        let out = run_instrs(vec![
            Instr::PushStr("0x1A".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(26)]);
    }

    #[test]
    fn call_builtin_number_parse_int_negative() {
        let out = run_instrs(vec![
            Instr::PushStr("  -17 ".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert_eq!(out, vec![Value::NegInt(-17)]);
    }

    #[test]
    fn call_builtin_number_parse_int_nan() {
        // No leading digits → NaN (a Number, not an error).
        let out = run_instrs(vec![
            Instr::PushStr("nope".into()),
            Instr::CallBuiltin(Builtin::NumberParseInt, 1),
        ]);
        assert!(matches!(out.as_slice(), [Value::Float(n)] if n.is_nan()));
    }

    #[test]
    fn call_builtin_number_parse_int_zero_args_is_not_an_error() {
        // Runtime: namespace builtins accept >= 0 args; absent string → "undefined",
        // parseInt("undefined") → NaN.
        let out = run_instrs(vec![Instr::CallBuiltin(Builtin::NumberParseInt, 0)]);
        assert!(matches!(out.as_slice(), [Value::Float(n)] if n.is_nan()));
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn call_builtin_number_parse_float() {
        let out = run_instrs(vec![
            Instr::PushStr("3.14".into()),
            Instr::CallBuiltin(Builtin::NumberParseFloat, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.14)]);
    }

    #[test]
    #[allow(clippy::approx_constant)]
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
}

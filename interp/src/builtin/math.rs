use crate::builtin::Args;
use crate::vm::{ErrorKind, VM, VMError, Value};

// ── Math implementations ─────────────────────────────────────────────────────

/// Math unary: read one arg, coerce ToNumber, apply f, return Number.
pub fn math_unary(vm: &mut VM, args: Args, f: impl Fn(f64) -> f64) -> Result<Value, VMError> {
    let n = args
        .get(vm, 0)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    Ok(Value::Float(f(n)))
}

pub fn math_abs(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.abs())
}
pub fn math_sqrt(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.sqrt())
}
pub fn math_ceil(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.ceil())
}
pub fn math_floor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.floor())
}

/// JS `Math.round`: rounds half toward +∞ (not away-from-zero like Rust).
pub fn math_round(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, js_round)
}

/// JS `Math.sign`: returns the input unchanged when n == 0.0 or -0.0, else
/// the signum. (Rust `signum` returns ±1 for ±0; JS returns ±0.)
pub fn math_sign(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| {
        if n == 0.0 {
            // Preserve sign: +0 → +0, -0 → -0
            n
        } else {
            n.signum()
        }
    })
}

/// `Math.min(...nums)` → the smallest, ToNumber-coercing each. Zero args →
/// +Infinity. JS: NaN propagates (the first NaN encountered wins).
pub fn math_min(vm: &mut VM, args: Args) -> Result<Value, VMError> {
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
pub fn math_max(vm: &mut VM, args: Args) -> Result<Value, VMError> {
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

pub fn math_trunc(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.trunc())
}
pub fn math_cbrt(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.cbrt())
}
pub fn math_exp(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.exp())
}
pub fn math_log(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.ln())
}
pub fn math_log2(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.log2())
}
pub fn math_log10(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.log10())
}
pub fn math_sin(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.sin())
}
pub fn math_cos(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.cos())
}
pub fn math_tan(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.tan())
}
pub fn math_asin(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.asin())
}
pub fn math_acos(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.acos())
}
pub fn math_atan(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    math_unary(vm, args, |n| n.atan())
}

/// `Math.atan2(y, x)` → angle of (x, y).
pub fn math_atan2(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let y = args
        .get(vm, 0)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let x = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    Ok(Value::Float(y.atan2(x)))
}

/// `Math.hypot(...values)` → sqrt of sum of squares. Returns +0 if no args.
pub fn math_hypot(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut sum = 0.0_f64;
    for i in 0..args.argc {
        let num = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if num.is_infinite() {
            return Ok(Value::Float(f64::INFINITY));
        }
        sum += num * num;
    }
    Ok(Value::Float(sum.sqrt()))
}

/// `Math.pow(base, exp)` → base^exp.
pub fn math_pow(vm: &mut VM, args: Args) -> Result<Value, VMError> {
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

// ── helpers ─────────────────────────────────────────────────────

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

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{
        Value,
        builtin::Builtin,
        testutil::{self, run_instrs},
        vm::Instr,
    };

    // ── Math ─────────────────────────────────────────────────────────

    #[test]
    fn call_builtin_math_abs() {
        let out = run_instrs(vec![
            Instr::PushFloat(-5.0),
            Instr::CallBuiltin(Builtin::MathAbs, 1),
        ]);
        assert_eq!(out, vec![Value::Float(5.0)]);
    }

    #[test]
    fn call_builtin_math_sqrt() {
        let out = run_instrs(vec![
            Instr::PushFloat(9.0),
            Instr::CallBuiltin(Builtin::MathSqrt, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);
    }

    #[test]
    fn call_builtin_math_ceil_floor_round() {
        let out = run_instrs(vec![
            Instr::PushFloat(2.3),
            Instr::CallBuiltin(Builtin::MathCeil, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);

        let out = run_instrs(vec![
            Instr::PushFloat(2.7),
            Instr::CallBuiltin(Builtin::MathFloor, 1),
        ]);
        assert_eq!(out, vec![Value::Float(2.0)]);

        let out = run_instrs(vec![
            Instr::PushFloat(2.5),
            Instr::CallBuiltin(Builtin::MathRound, 1),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);
    }

    #[test]
    fn call_builtin_math_sign() {
        let out = run_instrs(vec![
            Instr::PushFloat(-7.0),
            Instr::CallBuiltin(Builtin::MathSign, 1),
        ]);
        assert_eq!(out, vec![Value::Float(-1.0)]);
    }

    #[test]
    fn call_builtin_math_max_variadic() {
        let out = run_instrs(vec![
            Instr::PushFloat(3.0),
            Instr::PushFloat(9.0),
            Instr::PushFloat(5.0),
            Instr::CallBuiltin(Builtin::MathMax, 3),
        ]);
        assert_eq!(out, vec![Value::Float(9.0)]);
    }

    #[test]
    fn call_builtin_math_max_zero_args() {
        let out = run_instrs(vec![Instr::CallBuiltin(Builtin::MathMax, 0)]);
        assert!(matches!(out.as_slice(), [Value::Float(x)] if x.is_infinite() && *x < 0.0));
    }

    #[test]
    fn call_builtin_math_min_variadic() {
        let out = run_instrs(vec![
            Instr::PushFloat(3.0),
            Instr::PushFloat(-1.0),
            Instr::PushFloat(5.0),
            Instr::CallBuiltin(Builtin::MathMin, 3),
        ]);
        assert_eq!(out, vec![Value::Float(-1.0)]);
    }

    #[test]
    fn call_builtin_math_pow() {
        let out = run_instrs(vec![
            Instr::PushFloat(2.0),
            Instr::PushFloat(3.0),
            Instr::CallBuiltin(Builtin::MathPow, 2),
        ]);
        assert_eq!(out, vec![Value::Float(8.0)]);
    }

    #[test]
    fn js_math_round_half_toward_positive_infinity() {
        // JS: Math.round(2.5) === 3, Math.round(-2.5) === -2
        assert_eq!(testutil::eval("Math.round(2.5)"), Value::Float(3.0));
        assert_eq!(testutil::eval("Math.round(-2.5)"), Value::Float(-2.0));
        assert_eq!(testutil::eval("Math.round(3.4)"), Value::Float(3.0));
        // -0.5 → -0 in JS (sign preserved). Use direct VM to pass -0.5.
        let out = run_instrs(vec![
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
        let out = run_instrs(vec![
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

    // ── flexible arity (meta-driven) ───────────────────────────────────

    #[test]
    fn builtin_math_sqrt_ignores_surplus_args() {
        // A fixed-arity builtin (Math.sqrt, max 1) invoked with extra args
        // (as a callback would be: `(element, index, array)`) drops the surplus
        // and uses only the first argument.
        let out = run_instrs(vec![
            Instr::PushBuiltin(Builtin::MathSqrt),
            Instr::PushFloat(9.0), // the element
            Instr::PushFloat(1.0), // index — ignored
            Instr::PushFloat(7.0), // array stand-in — ignored
            Instr::CallDyn(3, false),
        ]);
        assert_eq!(out, vec![Value::Float(3.0)]);
    }

    #[test]
    fn builtin_math_sqrt_pow_no_args_gives_nan() {
        let out = run_instrs(vec![
            Instr::PushBuiltin(Builtin::MathPow),
            Instr::CallDyn(0, false),
        ]);
        assert!(matches!(out.as_slice(), [Value::Float(n)] if n.is_nan()));
    }

    // ── Step 4e: new math functions ──────────────────────────────────

    #[test]
    fn js_math_trunc() {
        let v = testutil::eval("Math.trunc(-1.9)");
        assert_eq!(v, Value::Float(-1.0));
    }

    #[test]
    fn js_math_log_e() {
        assert_eq!(testutil::eval("Math.log(Math.E)"), Value::Float(1.0));
    }

    #[test]
    fn js_math_hypot() {
        assert_eq!(testutil::eval("Math.hypot(3,4)"), Value::Float(5.0));
    }

    #[test]
    fn js_math_atan2() {
        let v = testutil::eval("Math.atan2(1,1)");
        // π/4
        assert!((v.as_f64().unwrap() - std::f64::consts::FRAC_PI_4).abs() < 1e-15);
    }

    #[test]
    fn js_math_cbrt() {
        assert_eq!(testutil::eval("Math.cbrt(27)"), Value::Float(3.0));
    }

    #[test]
    fn js_math_exp() {
        assert_eq!(
            testutil::eval("Math.exp(1)"),
            Value::Float(std::f64::consts::E)
        );
    }

    #[test]
    fn js_math_log2() {
        assert_eq!(testutil::eval("Math.log2(8)"), Value::Float(3.0));
    }

    #[test]
    fn js_math_log10() {
        assert_eq!(testutil::eval("Math.log10(100)"), Value::Float(2.0));
    }

    #[test]
    fn js_math_sin_cos_tan() {
        assert_eq!(testutil::eval("Math.sin(0)"), Value::Float(0.0));
        assert_eq!(testutil::eval("Math.cos(0)"), Value::Float(1.0));
        // tan(π/4) ≈ 0.9999999999999999 in JS due to floating point
        let v = testutil::eval("Math.tan(Math.PI/4)");
        assert!((v.as_f64().unwrap() - 1.0).abs() < 1e-15);
    }

    #[test]
    fn js_math_asin_acos_atan() {
        assert_eq!(
            testutil::eval("Math.asin(1)"),
            Value::Float(std::f64::consts::FRAC_PI_2)
        );
        assert_eq!(
            testutil::eval("Math.acos(0)"),
            Value::Float(std::f64::consts::FRAC_PI_2)
        );
        assert_eq!(
            testutil::eval("Math.atan(1)"),
            Value::Float(std::f64::consts::FRAC_PI_4)
        );
    }

    #[test]
    fn js_math_hypot_infinity() {
        // hypot with Infinity → Infinity
        let v = testutil::eval("Math.hypot(Infinity, 1)");
        assert!(v.as_f64().unwrap().is_infinite());
    }

    #[test]
    fn js_math_hypot_zero_args() {
        let v = testutil::eval("Math.hypot()");
        assert_eq!(v, Value::Float(0.0));
    }

    #[test]
    fn variadic_builtin_math_max_keeps_all_args() {
        // Math.max is variadic (max = u32::MAX): surplus is never trimmed.
        let out = run_instrs(vec![
            Instr::PushBuiltin(Builtin::MathMax),
            Instr::PushFloat(1.0),
            Instr::PushFloat(9.0),
            Instr::PushFloat(4.0),
            Instr::CallDyn(3, false),
        ]);
        assert_eq!(out, vec![Value::Float(9.0)]);
    }
}

use crate::builtin::Builtin;
use crate::vm::instr::Instr::*;
use crate::vm::*;

// ── harness ──────────────────────────────────────────────────

/// Run code in a fresh VM (no initial heap) to completion; return final stack.
fn run(code: Vec<Instr>) -> Vec<Value> {
    let mut vm = VM::new(code);
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => vm.stack.clone(),
        other => panic!("unexpected effect: {other:?}"),
    }
}

/// Build a `PushStr` for a string literal — terse sugar for the many tests
/// that push string operands inline.
fn ps(val: &str) -> Instr {
    Instr::PushStr(RcStr::from(val))
}

/// Run code to the first effect (Pending/Raise), returning the StepResult.
fn run_effect(code: Vec<Instr>) -> StepResult {
    let mut vm = VM::new(code);
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => panic!("unexpected completion"),
        effect => effect,
    }
}

/// Run code that is expected to error; return the error.
fn run_err(code: Vec<Instr>) -> VMError {
    let mut vm = VM::new(code);
    match vm.step(u64::MAX) {
        Err(e) => e,
        Ok(StepResult::Done { .. }) => panic!("unexpected completion"),
        Ok(_) => panic!("unexpected effect"),
    }
}

// ── helpers ───────────────────────────────────────────────────

fn n(v: f64) -> Value {
    Value::Float(v)
}
/// Canonical integer value: non-negative -> PosInt, negative -> NegInt.
fn i(v: i64) -> Value {
    if v < 0 {
        Value::NegInt(v)
    } else {
        Value::PosInt(v as u64)
    }
}
/// A PosInt directly (for values above i64::MAX).
#[allow(dead_code)]
fn u(v: u64) -> Value {
    Value::PosInt(v)
}
/// A function value pointing at a code address.
fn f(addr: u32) -> Value {
    Value::Closure { addr, ptr: addr }
}
fn b(v: bool) -> Value {
    Value::Bool(v)
}
fn null() -> Value {
    Value::Null
}
fn undef() -> Value {
    Value::Undefined
}
/// Object value at the given address.
#[allow(dead_code)]
fn s(addr: u32) -> Value {
    Value::Object(addr)
}
/// A string value (strings are inline now, not heap pointers).
fn str_v(val: &str) -> Value {
    Value::String(RcStr::from(val))
}
/// `n` plain (unboxed) local slots, for `EnterFrame`.
fn plain(n: usize) -> Vec<SlotKind> {
    vec![SlotKind::Plain; n]
}
/// `n` boxed (captured-by-ref) local slots, for `EnterFrame`.
#[allow(dead_code)]
fn boxed(n: usize) -> Vec<SlotKind> {
    vec![SlotKind::Boxed; n]
}

// ── stack manipulation ────────────────────────────────────────

#[test]
fn push_and_pop() {
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Pop(1)]),
        vec![n(1.0)]
    );
    assert_eq!(run(vec![PushFloat(1.0), Pop(1)]), vec![]);
    assert!(matches!(
        run_err(vec![Pop(1)]).kind,
        ErrorKind::StackUnderflow
    ));
}

#[test]
fn dup_swap_rot() {
    // Pick(0) = former Dup
    assert_eq!(run(vec![PushFloat(1.0), Pick(0)]), vec![n(1.0), n(1.0)]);
    // Dig(1) = former Swap
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Dig(1)]),
        vec![n(2.0), n(1.0)]
    );
    // Dig(2) = former Rot
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Dig(2)]),
        vec![n(2.0), n(3.0), n(1.0)]
    );
    assert!(matches!(
        run_err(vec![Dig(1)]).kind,
        ErrorKind::StackUnderflow
    ));
    assert!(matches!(
        run_err(vec![Dig(2)]).kind,
        ErrorKind::StackUnderflow
    ));
}

#[test]
fn pick() {
    // Pick(0) was formerly Dup; Pick(n) copies the n-th-from-top value to the top.
    assert_eq!(run(vec![PushFloat(1.0), Pick(0)]), vec![n(1.0), n(1.0)]);
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Pick(1)]),
        vec![n(1.0), n(2.0), n(1.0)]
    );
    assert_eq!(
        run(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            PushFloat(3.0),
            Pick(2)
        ]),
        vec![n(1.0), n(2.0), n(3.0), n(1.0)]
    );
    // Cannot reach below the frame's temporaries.
    assert!(matches!(
        run_err(vec![PushFloat(1.0), Pick(1)]).kind,
        ErrorKind::StackUnderflow
    ));
}

#[test]
fn dig() {
    // Dig(0) no-op, Dig(1) was formerly Swap, Dig(2) was formerly Rot.
    assert_eq!(run(vec![PushFloat(1.0), Dig(0)]), vec![n(1.0)]);
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Dig(1)]),
        vec![n(2.0), n(1.0)]
    );
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Dig(2)]),
        vec![n(2.0), n(3.0), n(1.0)]
    );
    assert!(matches!(
        run_err(vec![PushFloat(1.0), Dig(1)]).kind,
        ErrorKind::StackUnderflow
    ));
}

#[test]
fn nip() {
    // Nip(1) drops the value below top, leaving top in place.
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Nip(1)]),
        vec![n(2.0)]
    );
    // Nip(2) drops two values below top.
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), PushFloat(3.0), Nip(2)]),
        vec![n(3.0)]
    );
    // Nip rejects reaching below frame_floor.
    assert!(matches!(
        run_err(vec![PushFloat(1.0), Nip(1)]).kind,
        ErrorKind::StackUnderflow
    ));
}

#[test]
fn tee_local() {
    // TeeLocal writes top to a local without popping.
    // EnterFrame allocates Undefined as local 0; Push pushes 5.0 on top;
    // TeeLocal writes 5.0 into local 0 (replacing Undefined) and
    // leaves it on the stack. Result: [5.0, 5.0].
    assert_eq!(
        run(vec![
            EnterFrame(0, false, vec![SlotKind::Plain].into()),
            PushFloat(5.0),
            TeeLocal(0),
        ]),
        vec![n(5.0), n(5.0)]
    );
    // Verify the local was actually written.
    assert_eq!(
        run(vec![
            EnterFrame(0, false, vec![SlotKind::Plain].into()),
            PushFloat(7.0),
            TeeLocal(0),
            Pop(1),
            GetLocal(0),
        ]),
        vec![n(7.0), n(7.0)]
    );
    // TeeLocal on out-of-range slot errors.
    assert!(matches!(
        run_err(vec![PushFloat(1.0), TeeLocal(0)]).kind,
        ErrorKind::BadLocal
    ));
}

#[test]
fn inc_local() {
    use crate::vm::UpdateMode;
    // Prefix ++ in place: new value on stack AND in local.
    assert_eq!(
        run(vec![
            EnterFrame(0, false, vec![SlotKind::Plain].into()),
            PushFloat(5.0),
            SetLocal(0),
            IncLocal(0, -1.0, UpdateMode::Prefix),
        ]),
        vec![n(6.0), n(6.0)]
    );
    // Postfix ++: old value on stack, local updated to new.
    assert_eq!(
        run(vec![
            EnterFrame(0, false, vec![SlotKind::Plain].into()),
            PushFloat(5.0),
            SetLocal(0),
            IncLocal(0, -1.0, UpdateMode::Postfix),
        ]),
        vec![n(6.0), n(5.0)]
    );
    // Postfix --: old value pushed, local decremented.
    assert_eq!(
        run(vec![
            EnterFrame(0, false, vec![SlotKind::Plain].into()),
            PushFloat(5.0),
            SetLocal(0),
            IncLocal(0, 1.0, UpdateMode::Postfix),
            Pop(1), // drop old value
            GetLocal(0),
        ]),
        vec![n(4.0), n(4.0)]
    );
    // IncLocal on out-of-range slot errors.
    assert!(matches!(
        run_err(vec![IncLocal(0, -1.0, UpdateMode::Prefix)]).kind,
        ErrorKind::BadLocal
    ));
}

#[test]
fn jtrue() {
    // Truthy takes the jump (skipping the Push); falsy falls through.
    assert_eq!(run(vec![PushBool(true), JTrue(3), PushFloat(9.0)]), vec![]);
    assert_eq!(
        run(vec![PushBool(false), JTrue(3), PushFloat(9.0)]),
        vec![n(9.0)]
    );
}

#[test]
fn jnotnullish() {
    // Not nullish: takes the jump and LEAVES the value.
    assert_eq!(
        run(vec![PushFloat(5.0), JNotNullish(3), PushFloat(9.0)]),
        vec![n(5.0)]
    );
    // null / undefined: the value is POPPED on fall-through (the emitter
    // short-circuits past it).
    assert_eq!(
        run(vec![PushNull, JNotNullish(3), PushFloat(9.0)]),
        vec![n(9.0)]
    );
    assert_eq!(
        run(vec![PushUndefined, JNotNullish(3), PushFloat(9.0)]),
        vec![n(9.0)]
    );
}

// ── type predicates ───────────────────────────────────────────

#[test]
fn is_null() {
    assert_eq!(run(vec![PushNull, IsNull]), vec![b(true)]);
    assert_eq!(run(vec![PushFloat(0.0), IsNull]), vec![b(false)]);
}

#[test]
fn is_bool() {
    assert_eq!(run(vec![PushBool(true), IsBool]), vec![b(true)]);
    assert_eq!(run(vec![PushFloat(0.0), IsBool]), vec![b(false)]);
}

// ── unary operators ───────────────────────────────────────────

#[test]
#[allow(clippy::approx_constant)]
fn not_bitnot() {
    assert_eq!(run(vec![PushBool(false), Not]), vec![b(true)]);
    assert_eq!(run(vec![PushNull, Not]), vec![b(true)]);
    assert_eq!(run(vec![PushFloat(1.0), Not]), vec![b(false)]);
    assert_eq!(run(vec![PushFloat(5.0), BitNot]), vec![n(-6.0)]);
    assert!(matches!(
        run_err(vec![PushFloat(3.14), BitNot]).kind,
        ErrorKind::TypeError
    ));
}

// ── binary operators ──────────────────────────────────────────

#[test]
fn add_sub_mul_div() {
    assert_eq!(run(vec![PushFloat(2.0), PushFloat(3.0), Add]), vec![n(5.0)]);
    assert_eq!(
        run(vec![PushFloat(10.0), PushFloat(3.0), Sub]),
        vec![n(7.0)]
    );
    assert_eq!(
        run(vec![PushFloat(4.0), PushFloat(5.0), Mul]),
        vec![n(20.0)]
    );
    assert_eq!(
        run(vec![PushFloat(10.0), PushFloat(4.0), Div]),
        vec![n(2.5)]
    );
    // JS: x/0 -> ±Infinity, 0/0 -> NaN (never an error).
    assert!(matches!(
        run(vec![PushFloat(1.0), PushFloat(0.0), Div]).as_slice(),
        [Value::Float(x)] if x.is_infinite() && *x > 0.0
    ));
    assert!(matches!(
        run(vec![PushFloat(0.0), PushFloat(0.0), Div]).as_slice(),
        [Value::Float(x)] if x.is_nan()
    ));
}

#[test]
fn mod_op() {
    assert_eq!(
        run(vec![PushFloat(10.0), PushFloat(3.0), Mod]),
        vec![n(1.0)]
    );
    // JS %: float remainder (5.5 % 2 == 1.5), dividend's sign (-5 % 3 == -2).
    assert_eq!(run(vec![PushFloat(5.5), PushFloat(2.0), Mod]), vec![n(1.5)]);
    assert_eq!(
        run(vec![PushFloat(-5.0), PushFloat(3.0), Mod]),
        vec![n(-2.0)]
    );
    // x % 0 -> NaN, not an error.
    assert!(matches!(
        run(vec![PushFloat(1.0), PushFloat(0.0), Mod]).as_slice(),
        [Value::Float(x)] if x.is_nan()
    ));
}

#[test]
fn add_strings() {
    // Concatenation yields an inline string value on the stack.
    assert_eq!(
        run(vec![ps("hello "), ps("world"), Add]),
        vec![str_v("hello world")]
    );
}

/// Run `code` and return the top of the stack as a String, panicking if it
/// isn't one. Handy for ops that produce a result string (Add concat, ToStr,
/// ArrJoin).
fn run_last_str(code: Vec<Instr>) -> String {
    match run(code).last() {
        Some(Value::String(s)) => s.as_str().to_owned(),
        other => panic!("expected a string result, got {other:?}"),
    }
}

#[test]
fn add_concat_coerces() {
    // `+` concatenates when either side is a string, coercing the other.
    assert_eq!(run_last_str(vec![ps("x="), PushFloat(5.0), Add]), "x=5");
    assert_eq!(run_last_str(vec![PushFloat(5.0), ps("!"), Add]), "5!");
    assert_eq!(run_last_str(vec![ps("v="), PushNull, Add]), "v=null");
    assert_eq!(run_last_str(vec![ps("b="), PushBool(true), Add]), "b=true");
    // An array operand stringifies like join(",") on the concat path.
    assert_eq!(
        run_last_str(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            ArrNew(2),
            ps("!"),
            Add
        ]),
        "1,2!"
    );
}

#[test]
fn arithmetic_coerces() {
    // ToNumber coercion on -, *, /, % (strings, bools, null).
    assert_eq!(run(vec![ps("6"), PushFloat(1.0), Sub]), vec![n(5.0)]);
    assert_eq!(run(vec![PushBool(true), PushFloat(2.0), Mul]), vec![n(2.0)]);
    assert_eq!(run(vec![PushNull, PushFloat(1.0), Add]), vec![n(1.0)]);
    assert_eq!(run(vec![ps("6"), ps("2"), Mul]), vec![n(12.0)]);
    // undefined -> NaN propagates.
    assert!(matches!(
        run(vec![PushUndefined, PushFloat(1.0), Sub]).as_slice(),
        [Value::Float(x)] if x.is_nan()
    ));
    // An unparseable string -> NaN.
    assert!(matches!(
        run(vec![ps("abc"), PushFloat(1.0), Mul]).as_slice(),
        [Value::Float(x)] if x.is_nan()
    ));
}

#[test]
fn truthiness_matches_js() {
    // Falsy: false, 0, NaN, "", null, undefined.
    for code in [
        vec![PushBool(false), Not],
        vec![PushFloat(0.0), Not],
        vec![PushPosInt(0), Not],
        vec![PushFloat(f64::NAN), Not],
        vec![PushNull, Not],
        vec![PushUndefined, Not],
    ] {
        assert_eq!(run(code), vec![b(true)], "expected falsy");
    }
    assert_eq!(run(vec![ps(""), Not]), vec![b(true)]); // "" falsy
    // Truthy: nonzero, "0", non-empty string, [], {}.
    assert_eq!(run(vec![PushFloat(1.0), Not]), vec![b(false)]);
    assert_eq!(run(vec![ps("0"), Not]), vec![b(false)]); // "0" truthy
    assert_eq!(run(vec![ArrNew(0), Not]), vec![b(false)]); // [] truthy
    assert_eq!(run(vec![ObjNew(vec![].into()), Not]), vec![b(false)]); // {} truthy
    // And JFalse on 0 takes the branch (0 is falsy).
    assert_eq!(run(vec![PushFloat(0.0), JFalse(3), PushFloat(9.0)]), vec![]);
    // || picks the second operand when the first is 0 (falsy).
    assert_eq!(run(vec![PushFloat(0.0), PushFloat(7.0), Or]), vec![n(7.0)]);
}

#[test]
fn to_num_instruction() {
    // JS ToNumber: strings parse, bools→0/1, null→0, undefined/garbage→NaN.
    assert_eq!(run(vec![ps("42"), ToNum]), vec![n(42.0)]);
    assert_eq!(run(vec![PushBool(true), ToNum]), vec![n(1.0)]);
    assert_eq!(run(vec![PushNull, ToNum]), vec![n(0.0)]);
    assert!(matches!(
        run(vec![PushUndefined, ToNum]).as_slice(),
        [Value::Float(x)] if x.is_nan()
    ));
    // An array/object has no numeric form.
    assert!(matches!(
        run_err(vec![ArrNew(0), ToNum]).kind,
        ErrorKind::TypeError
    ));
}

#[test]
fn to_bool_instruction() {
    assert_eq!(run(vec![PushFloat(0.0), ToBool]), vec![b(false)]);
    assert_eq!(run(vec![PushFloat(1.0), ToBool]), vec![b(true)]);
    assert_eq!(run(vec![PushNull, ToBool]), vec![b(false)]);
    assert_eq!(run(vec![ps(""), ToBool]), vec![b(false)]);
    assert_eq!(run(vec![ArrNew(0), ToBool]), vec![b(true)]); // [] is truthy
}

#[test]
fn to_str_instruction() {
    assert_eq!(run_last_str(vec![PushFloat(5.0), ToStr]), "5");
    assert_eq!(run_last_str(vec![PushFloat(1.5), ToStr]), "1.5");
    assert_eq!(run_last_str(vec![PushNegInt(-3), ToStr]), "-3");
    assert_eq!(run_last_str(vec![PushNull, ToStr]), "null");
    assert_eq!(run_last_str(vec![PushUndefined, ToStr]), "undefined");
    assert_eq!(run_last_str(vec![PushBool(true), ToStr]), "true");
    // NaN / Infinity get JS spellings.
    assert_eq!(run_last_str(vec![PushFloat(f64::NAN), ToStr]), "NaN");
    assert_eq!(
        run_last_str(vec![PushFloat(f64::INFINITY), ToStr]),
        "Infinity"
    );
    // Array -> join(","), object -> "[object Object]".
    assert_eq!(
        run_last_str(vec![PushFloat(1.0), PushFloat(2.0), ArrNew(2), ToStr]),
        "1,2"
    );
    assert_eq!(
        run_last_str(vec![PushFloat(1.0), ObjNew(vec!["a".into()].into()), ToStr]),
        "[object Object]"
    );
}

// ── comparisons ───────────────────────────────────────────────

#[test]
fn eq_neq() {
    assert_eq!(run(vec![PushFloat(1.0), PushFloat(1.0), Eq]), vec![b(true)]);
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Eq]),
        vec![b(false)]
    );
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(2.0), Neq]),
        vec![b(true)]
    );
    // NaN != NaN
    assert_eq!(
        run(vec![PushFloat(f64::NAN), PushFloat(f64::NAN), Eq]),
        vec![b(false)]
    );
    // different types are not equal
    assert_eq!(run(vec![PushFloat(0.0), PushNull, Eq]), vec![b(false)]);
}

#[test]
fn loose_eq_nullish() {
    // null == undefined (and reflexively), but neither == anything else.
    assert_eq!(run(vec![PushNull, PushUndefined, LooseEq]), vec![b(true)]);
    assert_eq!(run(vec![PushUndefined, PushNull, LooseEq]), vec![b(true)]);
    assert_eq!(run(vec![PushNull, PushNull, LooseEq]), vec![b(true)]);
    assert_eq!(run(vec![PushNull, PushFloat(0.0), LooseEq]), vec![b(false)]);
    assert_eq!(
        run(vec![PushUndefined, PushBool(false), LooseEq]),
        vec![b(false)]
    );
    // strict still distinguishes them
    assert_eq!(run(vec![PushNull, PushUndefined, Eq]), vec![b(false)]);
    // LooseNeq is the negation
    assert_eq!(run(vec![PushNull, PushUndefined, LooseNeq]), vec![b(false)]);
    assert_eq!(run(vec![PushNull, PushFloat(0.0), LooseNeq]), vec![b(true)]);
}

#[test]
fn loose_eq_boolean_coercion() {
    // booleans coerce to 0/1.
    assert_eq!(
        run(vec![PushBool(true), PushFloat(1.0), LooseEq]),
        vec![b(true)]
    );
    assert_eq!(
        run(vec![PushBool(false), PushFloat(0.0), LooseEq]),
        vec![b(true)]
    );
    assert_eq!(
        run(vec![PushBool(true), PushFloat(2.0), LooseEq]),
        vec![b(false)]
    );
}

#[test]
fn loose_eq_number_string_coercion() {
    // 1 == "1"
    assert_eq!(run(vec![PushFloat(1.0), ps("1"), LooseEq]), vec![b(true)]);
    // "1" == 1 (other order)
    assert_eq!(run(vec![ps("1"), PushFloat(1.0), LooseEq]), vec![b(true)]);
    // 0 == "" (empty string ToNumber is 0)
    assert_eq!(run(vec![PushFloat(0.0), ps(""), LooseEq]), vec![b(true)]);
    // false == "" via double coercion
    assert_eq!(run(vec![PushBool(false), ps(""), LooseEq]), vec![b(true)]);
    // 1 == "abc" -> NaN -> false
    assert_eq!(
        run(vec![PushFloat(1.0), ps("abc"), LooseEq]),
        vec![b(false)]
    );
    // 1.5 == "1.5"
    assert_eq!(run(vec![PushFloat(1.5), ps("1.5"), LooseEq]), vec![b(true)]);
}

#[test]
fn loose_eq_strings_not_coerced_to_each_other() {
    // Two strings still compare as strings (no numeric coercion): "1" vs "1.0".
    assert_eq!(run(vec![ps("1"), ps("1.0"), LooseEq]), vec![b(false)]);
}

#[test]
fn loose_eq_object_vs_primitive_not_coerced() {
    // Documented divergence: [5] == 5 is false here (true in JS).
    assert_eq!(
        run(vec![PushFloat(5.0), ArrNew(1), PushFloat(5.0), LooseEq]),
        vec![b(false)]
    );
}

#[test]
fn string_eq() {
    // Strings compare by content: "abc"=="abc" is true, "abc"=="xyz" false.
    // (The two "abc" operands are distinct allocations, exercising the
    // value-compare path, not just the pointer fast path.)
    let out = run(vec![ps("abc"), ps("abc"), Eq, ps("abc"), ps("xyz"), Eq]);
    assert_eq!(out, vec![b(true), b(false)]);
}

#[test]
fn ordering() {
    // Numbers
    assert_eq!(run(vec![PushFloat(1.0), PushFloat(2.0), Lt]), vec![b(true)]);
    assert_eq!(run(vec![PushFloat(2.0), PushFloat(1.0), Gt]), vec![b(true)]);
    assert_eq!(
        run(vec![PushFloat(2.0), PushFloat(2.0), LtEq]),
        vec![b(true)]
    );
    assert_eq!(
        run(vec![PushFloat(2.0), PushFloat(2.0), GtEq]),
        vec![b(true)]
    );
    // Incomparable types → false
    assert_eq!(run(vec![PushFloat(1.0), PushNull, Lt]), vec![b(false)]);
}

#[test]
fn and_or() {
    // truthy && rhs → rhs
    assert_eq!(
        run(vec![PushBool(true), PushFloat(42.0), And]),
        vec![n(42.0)]
    );
    // falsy && rhs → falsy
    assert_eq!(
        run(vec![PushBool(false), PushFloat(42.0), And]),
        vec![b(false)]
    );
    // truthy || rhs → truthy
    assert_eq!(
        run(vec![PushFloat(42.0), PushBool(false), Or]),
        vec![n(42.0)]
    );
    // falsy || rhs → rhs
    assert_eq!(run(vec![PushNull, PushFloat(99.0), Or]), vec![n(99.0)]);
}

// ── bitwise ops ───────────────────────────────────────────────

#[test]
fn bitwise() {
    assert_eq!(
        run(vec![PushFloat(10.0), PushFloat(12.0), BitAnd]),
        vec![n(8.0)] // 0b1010 & 0b1100 = 0b1000
    );
    assert_eq!(
        run(vec![PushFloat(10.0), PushFloat(12.0), BitOr]),
        vec![n(14.0)] // 0b1010 | 0b1100 = 0b1110
    );
    assert_eq!(
        run(vec![PushFloat(10.0), PushFloat(12.0), BitXor]),
        vec![n(6.0)] // 0b1010 ^ 0b1100 = 0b0110
    );
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(3.0), BitLhs]),
        vec![n(8.0)]
    );
    assert_eq!(
        run(vec![PushFloat(8.0), PushFloat(2.0), BitRhs]),
        vec![n(2.0)]
    );
}

// ── control flow ──────────────────────────────────────────────

#[test]
fn jump_and_jfalse() {
    // Jump over a Push: should only leave n(1.0) on stack
    assert_eq!(
        run(vec![PushFloat(1.0), Jump(3), PushFloat(999.0)]),
        vec![n(1.0)]
    );
    // JFalse with false → jump over Push
    assert_eq!(
        run(vec![PushBool(false), JFalse(3), PushFloat(999.0)]),
        vec![]
    );
    // JFalse with true → don't jump, execute Push
    assert_eq!(
        run(vec![PushBool(true), JFalse(3), PushFloat(42.0)]),
        vec![n(42.0)]
    );
    // Null is falsy
    assert_eq!(run(vec![PushNull, JFalse(3), PushFloat(999.0)]), vec![]);
}

#[test]
fn label_noop() {
    // Label should be a no-op at runtime
    assert_eq!(
        run(vec![PushFloat(1.0), Label(42), PushFloat(2.0)]),
        vec![n(1.0), n(2.0)]
    );
}

#[test]
fn call_and_return() {
    // Main calls a function at index 4 that adds its two args and
    // returns the sum.  Main then returns that value.
    // Left-to-right: first pushed is arg 0.
    //
    // [0] Push(10)   -- arg 0
    // [1] Push(20)   -- arg 1
    // [2] Call(4, 2) -- call fn at 4 with 2 args
    // [3] Return(1)  -- main returns 1 value
    // [4] Local(0)   -- fn: args arrive in place as locals (10)
    // [5] Local(1)   -- fn: local 1 (20)
    // [6] Add        -- fn: 10 + 20 = 30
    // [7] Return(1)  -- fn: return 1 value
    assert_eq!(
        run(vec![
            PushFloat(10.0),
            PushFloat(20.0),
            Call(4, 2),
            Return(1),
            GetLocal(0),
            GetLocal(1),
            Add,
            Return(1),
        ]),
        vec![n(30.0)]
    );
}

#[test]
fn arg_order_is_left_to_right() {
    // Non-commutative op pins the convention: fn computes arg0 - arg1.
    // Push 10 then 3 -> arg0=10, arg1=3 -> 10 - 3 = 7.
    assert_eq!(
        run(vec![
            PushFloat(10.0),
            PushFloat(3.0),
            Call(4, 2),
            Return(1),
            GetLocal(0),
            GetLocal(1),
            Sub,
            Return(1),
        ]),
        vec![n(7.0)]
    );
}

#[test]
fn call_dyn_indirect() {
    // Indirect call through a Fn value. Function at [4] computes arg0 - arg1.
    // Layout: push callable below the args.
    // [0] Push(Fn(5))    callable below
    // [1] Push(10)       arg 0
    // [2] Push(3)        arg 1
    // [3] CallDyn(2, false)
    // [4] Return(1)      main returns the result
    // [5] Local(0)       fn body: args arrive in place as locals
    // [6] Local(1)
    // [7] Sub            10 - 3
    // [8] Return(1)
    assert_eq!(
        run(vec![
            PushFn(5, u32::MAX, 0),
            PushFloat(10.0),
            PushFloat(3.0),
            CallDyn(2, false),
            Return(1),
            GetLocal(0),
            GetLocal(1),
            Sub,
            Return(1),
        ]),
        vec![n(7.0)]
    );
}

#[test]
fn call_dyn_requires_fn() {
    // Callee (below args) must be a Fn, not some other value.
    let code = vec![PushFloat(2.0), PushFloat(1.0), CallDyn(1, false)];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn call_spread_with_builtin() {
    // Math.max(...[3, 7]) — stack: [callable, args_arr] (callable below)
    let code = vec![
        PushBuiltin(Builtin::MathMax), // callable below
        PushFloat(3.0),
        PushFloat(7.0),
        ArrNew(2), // args array
        CallSpread(false),
    ];
    assert_eq!(run(code), vec![n(7.0)]);
}

#[test]
fn call_spread_with_empty_array() {
    // NumberParseInt(...[]) → NaN
    let code = vec![
        PushBuiltin(Builtin::NumberParseInt),
        ArrNew(0), // empty args
        CallSpread(false),
    ];
    let out = run(code);
    assert!(matches!(out[0], Value::Float(f) if f.is_nan()));
}

#[test]
fn call_spread_with_fn() {
    // fn(a, b) = a - b, called as fn(...[10, 3])
    let code = vec![
        PushFn(6, u32::MAX, 0), // callable below args array (addr of fn body)
        PushFloat(10.0),
        PushFloat(3.0),
        ArrNew(2), // args array
        CallSpread(false),
        Return(1),
        GetLocal(0),
        GetLocal(1),
        Sub,
        Return(1),
    ];
    assert_eq!(run(code), vec![n(7.0)]);
}

#[test]
fn call_spread_non_array_error() {
    // CallSpread(false) with a non-array args value → TypeError
    let code = vec![
        PushBuiltin(Builtin::MathMax),
        PushFloat(42.0), // not an array
        CallSpread(false),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn call_spread_non_callable_error() {
    // CallSpread(false) with a non-callable → TypeError
    let code = vec![
        PushFloat(42.0), // not callable
        ArrNew(0),
        CallSpread(false),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn arguments_builds_array_of_frame_args() {
    // Call a fn with 3 args; its body builds `arguments` and returns it.
    // [0] callable, [1..3] args, [4] CallDyn(3, false), [5] Return(1)
    // [6] Arguments (fn body), [7] Return(1)
    let mut vm = VM::new(vec![
        PushFn(6, u32::MAX, 0),
        PushFloat(10.0),
        PushFloat(20.0),
        PushFloat(30.0),
        CallDyn(3, false),
        Return(1),
        Arguments,
        Return(1),
    ]);
    while !matches!(vm.step(u64::MAX).unwrap(), StepResult::Done { .. }) {}
    match vm.stack.as_slice() {
        [Value::Array(p)] => {
            let a = &vm.arrays[*p as usize];
            assert_eq!(a, &vec![n(10.0), n(20.0), n(30.0)]);
        }
        other => panic!("expected one Array, got {other:?}"),
    }
}

#[test]
fn arguments_is_cached_within_a_frame() {
    // Two `Arguments` in the same frame yield the SAME heap pointer (the
    // per-frame cache), so `Eq` (reference equality for arrays) is true.
    let out = run(vec![
        PushFn(4, u32::MAX, 0),
        PushFloat(1.0),
        CallDyn(1, false),
        Return(1),
        Arguments, // fn body: build (and cache)
        Arguments, // reuse the cached array
        Eq,        // same Ptr → true
        Return(1),
    ]);
    assert_eq!(out, vec![b(true)]);
}

#[test]
fn call_dyn_bad_addr() {
    let code = vec![PushFn(999, u32::MAX, 0), CallDyn(0, false)];
    assert!(matches!(run_err(code).kind, ErrorKind::BadCall));
}

#[test]
fn fn_value_equality_and_json() {
    // Same address -> equal; different -> not.
    assert_eq!(
        run(vec![PushFn(3, u32::MAX, 0), PushFn(3, u32::MAX, 0), Eq]),
        vec![b(true)]
    );
    assert_eq!(
        run(vec![PushFn(3, u32::MAX, 0), PushFn(4, u32::MAX, 0), Eq]),
        vec![b(false)]
    );
}

// ── closures ──────────────────────────────────────────────────

/// Append a `makeCounter` to `code`: a function that boxes a `count` local
/// (slot 0), initializes it to 0, and returns a closure that increments and
/// returns `count`. Returns makeCounter's code address.
fn append_counter(code: &mut Vec<Instr>) -> u32 {
    let mc = code.len() as u32;
    code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into())); // slot 0 = count (by-ref)
    code.push(PushFloat(0.0));
    code.push(SetLocal(0)); // count = 0 (writes through the cell)
    let mk = code.len();
    code.push(ClosureNew(0, 0, vec![0].into())); // patched: capture count
    code.push(Return(1));
    let inner = code.len() as u32;
    // 0 params, 1 upval → EnterFrame installs the captured cell at slot 0.
    code.push(EnterFrame(0, false, vec![].into()));
    code.push(GetLocal(0)); // count  (slot 0 = captured upval)
    code.push(PushFloat(1.0));
    code.push(Add);
    code.push(SetLocal(0)); // count = count + 1 (through the shared cell)
    code.push(GetLocal(0));
    code.push(Return(1)); // return count
    code[mk] = ClosureNew(inner, 0, vec![0].into());
    mc
}

/// Run to completion and return the finished VM (to inspect heap/cells).
fn run_vm(code: Vec<Instr>) -> VM {
    let mut vm = VM::new(code);
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => vm,
        other => panic!("unexpected effect: {other:?}"),
    }
}

#[test]
fn closure_captures_by_reference_across_calls() {
    // c = makeCounter(); c() + c()  →  1 + 2 = 3. The captured cell
    // persists between calls (and outlives makeCounter's frame), so the
    // count is not reset — that's capture by reference.
    let mut code: Vec<Instr> = Vec::new();
    let call_mc = code.len();
    code.push(Call(0, 0)); // patched → makeCounter; leaves a closure
    code.push(Pick(0));
    code.push(CallDyn(0, false)); // first call → 1
    code.push(Dig(1));
    code.push(CallDyn(0, false)); // second call → 2
    code.push(Add);
    code.push(Return(1));
    let mc = append_counter(&mut code);
    code[call_mc] = Call(mc, 0);
    assert_eq!(run(code), vec![n(3.0)]);
}

#[test]
fn closures_have_independent_cells() {
    // Two makeCounter() results must not share state: c1(), c1(), c2()
    // → [1, 2, 1].
    let mut code: Vec<Instr> = Vec::new();
    code.push(EnterFrame(0, false, plain(2).into())); // local 0 = c1, local 1 = c2
    let call1 = code.len();
    code.push(Call(0, 0));
    code.push(SetLocal(0));
    let call2 = code.len();
    code.push(Call(0, 0));
    code.push(SetLocal(1));
    code.push(GetLocal(0));
    code.push(CallDyn(0, false)); // c1() → 1
    code.push(GetLocal(0));
    code.push(CallDyn(0, false)); // c1() → 2
    code.push(GetLocal(1));
    code.push(CallDyn(0, false)); // c2() → 1
    code.push(ArrNew(3));
    code.push(Return(1));
    let mc = append_counter(&mut code);
    code[call1] = Call(mc, 0);
    code[call2] = Call(mc, 0);

    let vm = run_vm(code);
    let Value::Array(p) = vm.stack[0] else {
        panic!("expected array pointer");
    };
    assert_eq!(vm.arrays[p as usize].as_slice(), &[n(1.0), n(2.0), n(1.0)]);
}

#[test]
fn closure_captures_plain_slot_by_value() {
    // A Plain (unboxed) slot is captured by *value*: the closure snapshots
    // the value at capture time, so mutating the local afterward is not
    // observed. maker(): x=5; f=closure-over-x; x=99; return f. f() → 5.
    let mut code: Vec<Instr> = Vec::new();
    let call = code.len();
    code.push(Call(0, 0)); // patched → maker; leaves a closure
    code.push(CallDyn(0, false));
    code.push(Return(1));
    // maker
    let maker = code.len() as u32;
    code.push(EnterFrame(0, false, plain(1).into())); // slot 0 = x (NOT boxed)
    code.push(PushFloat(5.0));
    code.push(SetLocal(0));
    let mk = code.len();
    code.push(ClosureNew(0, 0, vec![0].into())); // snapshot x = 5
    code.push(PushFloat(99.0));
    code.push(SetLocal(0)); // x = 99 AFTER capture (must not be seen)
    code.push(Return(1));
    let inner = code.len() as u32;
    code.push(EnterFrame(0, false, vec![].into())); // install the by-value upval at slot 0
    code.push(GetLocal(0)); // return captured snapshot
    code.push(Return(1));
    code[call] = Call(maker, 0);
    code[mk] = ClosureNew(inner, 0, vec![0].into());
    assert_eq!(run(code), vec![n(5.0)]);
}

#[test]
fn two_closures_share_one_cell() {
    // A getter and a setter closing over the same boxed `x` must see each
    // other's writes. setter(42) then getter() → 42.
    let mut code: Vec<Instr> = Vec::new();
    // main: arr = maker(); setter = arr[1]; setter(42); getter = arr[0]; getter()
    code.push(EnterFrame(0, false, plain(1).into())); // local 0 = [getter, setter]
    let call = code.len();
    code.push(Call(0, 0));
    code.push(SetLocal(0));
    code.push(GetLocal(0));
    code.push(PushFloat(1.0));
    code.push(IndexGet); // setter
    code.push(PushFloat(42.0)); // setter's arg — goes above callee
    code.push(CallDyn(1, false)); // setter(42) → (no result)
    code.push(GetLocal(0));
    code.push(PushFloat(0.0));
    code.push(IndexGet); // getter
    code.push(CallDyn(0, false)); // getter() → 42
    code.push(Return(1));
    // maker
    let maker = code.len() as u32;
    code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into())); // slot 0 = x (by-ref)
    code.push(PushFloat(0.0));
    code.push(SetLocal(0));
    let mk_get = code.len();
    code.push(ClosureNew(0, 0, vec![0].into()));
    let mk_set = code.len();
    code.push(ClosureNew(0, 0, vec![0].into()));
    code.push(ArrNew(2)); // [getter, setter]
    code.push(Return(1));
    let getter = code.len() as u32;
    code.push(EnterFrame(0, false, vec![].into())); // upval x at slot 0
    code.push(GetLocal(0));
    code.push(Return(1));
    let setter = code.len() as u32;
    // 1 param (slot 0) + 1 upval x (slot 1): write the param into x's cell.
    code.push(EnterFrame(1, false, vec![].into()));
    code.push(GetLocal(0)); // the arg
    code.push(SetLocal(1)); // x = arg (through the shared cell)
    code.push(Return(0));
    code[call] = Call(maker, 0);
    code[mk_get] = ClosureNew(getter, 0, vec![0].into());
    code[mk_set] = ClosureNew(setter, 0, vec![0].into());
    assert_eq!(run(code), vec![n(42.0)]);
}

#[test]
fn nested_capture_forwards_same_cell() {
    // outer boxes x=7 and returns `middle`; middle returns `inner`; inner
    // reads x. The cell threads through both closure levels unchanged.
    // outer()()() → 7.
    let mut code: Vec<Instr> = Vec::new();
    let call = code.len();
    code.push(Call(0, 0)); // → middle closure
    code.push(CallDyn(0, false)); // → inner closure
    code.push(CallDyn(0, false)); // → 7
    code.push(Return(1));
    let outer = code.len() as u32;
    code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into()));
    code.push(PushFloat(7.0));
    code.push(SetLocal(0));
    let mk_mid = code.len();
    code.push(ClosureNew(0, 0, vec![0].into()));
    code.push(Return(1));
    let middle = code.len() as u32;
    // middle's slot 0 is x (installed upval); forward it to inner.
    code.push(EnterFrame(0, false, vec![].into()));
    let mk_in = code.len();
    code.push(ClosureNew(0, 0, vec![0].into()));
    code.push(Return(1));
    let inner = code.len() as u32;
    code.push(EnterFrame(0, false, vec![].into()));
    code.push(GetLocal(0));
    code.push(Return(1));
    code[call] = Call(outer, 0);
    code[mk_mid] = ClosureNew(middle, 0, vec![0].into());
    code[mk_in] = ClosureNew(inner, 0, vec![0].into());
    assert_eq!(run(code), vec![n(7.0)]);
}

#[test]
fn closure_identity_equality() {
    // The same closure object equals itself (reference identity)…
    let same = vec![
        EnterFrame(0, false, vec![SlotKind::Boxed].into()),
        PushFloat(1.0),
        SetLocal(0),
        ClosureNew(6, 0, vec![0].into()),
        Pick(0),
        Eq,
        Return(1), // addr 6: also a valid (never-called) closure target
    ];
    assert_eq!(run(same), vec![b(true)]);
    // …but two distinct closure objects do not (no content equality).
    let distinct = vec![
        EnterFrame(0, false, vec![SlotKind::Boxed].into()),
        PushFloat(1.0),
        SetLocal(0),
        ClosureNew(7, 0, vec![0].into()),
        ClosureNew(7, 0, vec![0].into()),
        Eq,
        Return(1),
        Return(1), // addr 7
    ];
    assert_eq!(run(distinct), vec![b(false)]);
}

#[test]
fn make_closure_rejects_out_of_range_capture() {
    // Capturing a slot the frame doesn't have is a compiler bug → BadLocal.
    let code = vec![
        Call(2, 0),
        Return(0),
        EnterFrame(0, false, plain(1).into()),
        ClosureNew(0, 0, vec![5].into()), // only slot 0 exists
        Return(1),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::BadLocal));
}

#[test]
fn call_dyn_rejects_non_closure_pointer() {
    // A Ptr to a non-closure heap value (here an array) is not callable.
    let code = vec![ArrNew(0), CallDyn(0, false)];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn call_with_locals() {
    // Function allocates a local, stores arg+arg in it, returns it. Args
    // arrive in place as locals 0,1; the declared local is allocated at
    // slot 2 (after the two params).
    // [4] EnterFrame(2, false, plain(1)) -- 2 params + 1 local at slot 2
    // [5] Local(0)                        -- arg 0 (7)
    // [6] Local(1)                        -- arg 1 (8)
    // [7] Add
    // [8] SetLocal(2)
    // [9] Local(2)
    // [10] Return(1)
    assert_eq!(
        run(vec![
            PushFloat(7.0),
            PushFloat(8.0),
            Call(4, 2),
            Return(1),
            EnterFrame(2, false, plain(1).into()),
            GetLocal(0),
            GetLocal(1),
            Add,
            SetLocal(2),
            GetLocal(2),
            Return(1),
        ]),
        vec![n(15.0)]
    );
}

// ── frame access validation ───────────────────────────────────

#[test]
fn local_oob() {
    // Called with one arg (→ local 0); reading local 1 is out of range.
    let code = vec![
        PushFloat(1.0),
        Call(3, 1),
        Return(0),
        GetLocal(1), // only local 0 (the arg) exists
        Return(0),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::BadLocal));
}

#[test]
fn call_bad_addr() {
    assert!(matches!(
        run_err(vec![Call(999, 0)]).kind,
        ErrorKind::BadCall
    ));
}

// ── array operations ──────────────────────────────────────────

#[test]
fn arr_new_and_length() {
    assert_eq!(
        run(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            PushFloat(3.0),
            ArrNew(3),
            GetLength
        ]),
        vec![n(3.0)]
    );
}

#[test]
fn arr_get_set() {
    // Create [10, 20, 30], set arr[1] = 99, read it back.
    let code = vec![
        PushFloat(10.0),
        PushFloat(20.0),
        PushFloat(30.0),
        ArrNew(3),
        PushFloat(1.0),         // index
        PushFloat(99.0),        // value
        IndexSet(SetMode::New), // pops value, index, arr_ptr; leaves the value
    ];
    // IndexSet leaves the assigned value (assignment is an expression).
    assert_eq!(run(code), vec![n(99.0)]);
}

#[test]
fn arr_get_set_with_dup() {
    // Keep ptr around with Pick(0) (formerly Dup) before mutation.
    let code = vec![
        PushFloat(10.0),
        PushFloat(20.0),
        PushFloat(30.0),
        ArrNew(3),
        Pick(0),                // save ptr for later
        PushFloat(1.0),         // index
        PushFloat(99.0),        // value
        IndexSet(SetMode::New), // pops value, index, ptr_copy; leaves value → [ptr, 99]
        Pop(1),                 // drop the assigned-value result → [ptr]
        PushFloat(1.0),         // index
        IndexGet,               // pops index, ptr → pushes arr[1]
    ];
    assert_eq!(run(code), vec![n(99.0)]);
}

#[test]
fn arr_get_oob() {
    let code = vec![
        PushFloat(10.0),
        ArrNew(1),
        PushFloat(5.0), // index 5, out of bounds
        IndexGet,       // JS: out-of-bounds reads as undefined
    ];
    assert_eq!(run(code), vec![undef()]);
}

#[test]
fn arr_set_oob() {
    let code = vec![
        PushFloat(10.0),
        ArrNew(1),
        PushFloat(5.0),         // index
        PushFloat(99.0),        // value
        IndexSet(SetMode::New), // pops: value, index, arr_ptr
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::ValueError));
}

#[test]
fn arr_extend_and_push() {
    // Build [1, 2, 3] via ArrNew + ArrExtend + ArrPush, verify length.
    let code = vec![
        PushFloat(1.0), // leading element
        ArrNew(1),      // [1]
        PushFloat(10.0),
        PushFloat(20.0),
        ArrNew(2),       // [10, 20] — the extend source
        ArrExtend,       // [1, 10, 20]
        PushFloat(99.0), // trailing element
        ArrPush,         // [1, 10, 20, 99]
        GetLength,
    ];
    assert_eq!(run(code), vec![n(4.0)]);
}

#[test]
fn arr_extend_empty_leading() {
    // Start with empty array then extend.
    let code = vec![
        ArrNew(0), // []
        PushFloat(3.0),
        PushFloat(4.0),
        ArrNew(2), // [3, 4]
        ArrExtend, // [3, 4]
        GetLength,
    ];
    assert_eq!(run(code), vec![n(2.0)]);
}

#[test]
fn arr_extend_non_array_error() {
    // ArrExtend on a non-array source is a TypeError.
    let code = vec![
        ArrNew(0),
        PushFloat(42.0), // not an array
        ArrExtend,
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn arr_push_twice() {
    // Multiple ArrPush calls.
    let code = vec![
        ArrNew(0),
        PushFloat(1.0),
        ArrPush,
        PushFloat(2.0),
        ArrPush,
        PushFloat(3.0),
        ArrPush,
        GetLength,
    ];
    assert_eq!(run(code), vec![n(3.0)]);
}

// ── object operations ─────────────────────────────────────────

#[test]
fn obj_new_get_set() {
    // Left-to-right: fields ["a","b"], values pushed in field order.
    // Push a-val (20), push b-val (10) → a=20, b=10
    let out = run(vec![
        PushFloat(20.0), // "a" value (first field, pushed first)
        PushFloat(10.0), // "b" value (second field)
        ObjNew(vec!["a".into(), "b".into()].into()),
        ps("a"),  // field "a" (the string key)
        IndexGet, // pops key, obj_ptr → pushes obj["a"]
    ]);
    // IndexGet consumes obj_ptr, so stack only has the retrieved value.
    assert_eq!(out, vec![n(20.0)]);
}

#[test]
fn obj_get_set_dynamic() {
    // Computed get via IndexGet with a string key.
    let out = run(vec![
        PushFloat(1.0),
        PushFloat(2.0),
        ObjNew(vec!["x".into(), "y".into()].into()), // x=1, y=2
        ps("x"),                                     // field "x"
        IndexGet,                                    // → 1
    ]);
    assert_eq!(out, vec![n(1.0)]);

    // Computed set via IndexSet: set a field, verify with ObjGet.
    let out = run(vec![
        PushFloat(1.0),
        PushFloat(2.0),
        ObjNew(vec!["x".into(), "y".into()].into()), // x=1, y=2
        Pick(0),                                     // keep ptr for verification
        ps("y"),                                     // field "y" — pushed before val
        PushFloat(99.0),                             // val — on top
        IndexSet(SetMode::New),                      // obj.y = 99; leaves val → [ptr, 99]
        Pop(1),                                      // drop the result → [ptr]
        ObjGet("y".into()),                          // → 99
    ]);
    assert_eq!(out, vec![n(99.0)]);
}

#[test]
fn obj_get_known_set_known() {
    // Test ObjSet (static set) and ObjGet (static get).
    // Create {x:2, y:1}, modify x=99 with ObjSet, verify with ObjGet.
    let code = vec![
        PushFloat(2.0),                              // x value
        PushFloat(1.0),                              // y value
        ObjNew(vec!["x".into(), "y".into()].into()), // x=2, y=1
        Pick(0),         // keep ptr for verification after ObjSet consumes one
        PushFloat(99.0), // value to set
        ObjSet("x".into(), SetMode::New), // obj.x = 99; leaves value → [ptr, 99]
        Pop(1),          // drop the result → [ptr]
        ObjGet("x".into()), // → 99
    ];
    assert_eq!(run(code), vec![n(99.0)]);
}

#[test]
fn obj_get_missing_key() {
    let code = vec![
        PushFloat(1.0),
        ObjNew(vec!["x".into()].into()),
        ObjGet("no_such_key".into()), // JS: missing key reads as undefined
    ];
    assert_eq!(run(code), vec![undef()]);
}

#[test]
fn obj_extend_merges_fields() {
    // Build {a:1} then extend with {b:2}, then {c:3} via ObjSet.
    let code = vec![
        PushFloat(1.0),                  // a value
        ObjNew(vec!["a".into()].into()), // {a:1}
        PushFloat(2.0),
        ObjNew(vec!["b".into()].into()),  // {b:2}
        ObjExtend,                        // {a:1, b:2}
        Pick(0),                          // dup for next read
        PushFloat(3.0),                   // c value
        ObjSet("c".into(), SetMode::New), // {a:1, b:2, c:3}; leaves val
        Pop(1),                           // drop val, keep obj
        ObjGet("a".into()),               // → 1
    ];
    assert_eq!(run(code), vec![n(1.0)]);
}

#[test]
fn obj_extend_null_undefined_noop() {
    // null/undefined source is a no-op.
    let code = vec![
        PushFloat(1.0),
        ObjNew(vec!["x".into()].into()), // {x:1}
        PushNull,                        // src = null (no-op)
        ObjExtend,                       // {x:1}
        ObjGet("x".into()),
    ];
    assert_eq!(run(code), vec![n(1.0)]);

    let code2 = vec![
        PushFloat(2.0),
        ObjNew(vec!["y".into()].into()), // {y:2}
        PushUndefined,                   // src = undefined (no-op)
        ObjExtend,                       // {y:2}
        ObjGet("y".into()),
    ];
    assert_eq!(run(code2), vec![n(2.0)]);
}

#[test]
fn obj_extend_non_object_error() {
    // Non-object, non-null/undefined src is TypeError.
    let code = vec![
        ObjNew(vec![].into()), // {}
        PushFloat(42.0),       // not an object
        ObjExtend,
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn obj_extend_later_wins() {
    // Later field wins on conflict (IndexMap insertion-order semantics).
    let code = vec![
        PushFloat(1.0),
        ObjNew(vec!["x".into()].into()), // {x:1}
        PushFloat(2.0),
        ObjNew(vec!["x".into()].into()), // {x:2}
        ObjExtend,                       // {x:2} (later wins)
        ObjGet("x".into()),
    ];
    assert_eq!(run(code), vec![n(2.0)]);
}

// ── undefined & typeof ────────────────────────────────────────

#[test]
fn undefined_is_falsy() {
    assert_eq!(run(vec![PushUndefined, Not]), vec![b(true)]);
    // Branches like null: JFalse on undefined takes the jump.
    assert_eq!(run(vec![PushUndefined, JFalse(3), PushFloat(9.0)]), vec![]);
}

#[test]
fn undefined_strict_equality() {
    // undefined === undefined, but undefined !== null (Eq is strict ===).
    assert_eq!(run(vec![PushUndefined, PushUndefined, Eq]), vec![b(true)]);
    assert_eq!(run(vec![PushUndefined, PushNull, Eq]), vec![b(false)]);
    assert_eq!(run(vec![PushNull, PushUndefined, Neq]), vec![b(true)]);
}

#[test]
fn undefined_is_not_comparable() {
    // Relational ops on undefined are all false (compare() yields None),
    // matching JS `undefined < 1 === false`, `undefined >= undefined === false`.
    assert_eq!(run(vec![PushUndefined, PushFloat(1.0), Lt]), vec![b(false)]);
    assert_eq!(
        run(vec![PushUndefined, PushUndefined, GtEq]),
        vec![b(false)]
    );
}

#[test]
fn uninitialized_local_is_undefined() {
    // `let x;` then read x -> undefined.
    let code = vec![
        EnterFrame(0, false, vec![SlotKind::Plain].into()),
        GetLocal(0),
        Return(1),
    ];
    assert_eq!(run(code), vec![undef()]);
}

#[test]
fn typeof_tags() {
    // typeof returns JS strings; check each via a heap-string comparison.
    let cases: &[(Value, &str)] = &[
        (undef(), "undefined"),
        (null(), "object"),
        (b(true), "boolean"),
        (n(3.5), "number"),
        (i(7), "number"),
        (f(0), "function"),
    ];
    for (val, tag) in cases {
        let instr = match val {
            Value::Undefined => PushUndefined,
            Value::Null => PushNull,
            Value::Bool(b) => PushBool(*b),
            Value::Float(f) => PushFloat(*f),
            Value::PosInt(u) => PushPosInt(*u),
            Value::NegInt(i) => PushNegInt(*i),
            Value::Closure { addr, .. } => PushFn(*addr, u32::MAX, 0),
            _ => panic!("unexpected stack value"),
        };
        let out = run(vec![instr, TypeOf, ps(tag), Eq]);
        assert_eq!(out, vec![b(true)], "typeof {val:?} should be {tag:?}");
    }
}

#[test]
fn typeof_heap_values() {
    // string -> "string", array/object -> "object", closure -> "function".
    let str_tag = run(vec![ps("hi"), TypeOf, ps("string"), Eq]);
    assert_eq!(str_tag, vec![b(true)]);
    let arr_tag = run(vec![PushFloat(1.0), ArrNew(1), TypeOf, ps("object"), Eq]);
    assert_eq!(arr_tag, vec![b(true)]);
    let obj_tag = run(vec![
        PushFloat(1.0),
        ObjNew(vec!["a".into()].into()),
        TypeOf,
        ps("object"),
        Eq,
    ]);
    assert_eq!(obj_tag, vec![b(true)]);
    // typeof a missing property is "undefined".
    let miss_tag = run(vec![
        PushFloat(1.0),
        ObjNew(vec!["a".into()].into()),
        ObjGet("b".into()),
        TypeOf,
        ps("undefined"),
        Eq,
    ]);
    assert_eq!(miss_tag, vec![b(true)]);
}

// ── effects ───────────────────────────────────────────────────

#[test]
fn invoke_pushes_promise_and_continues() {
    // Invoke no longer yields: it allocates a Pending promise, records the
    // call in the outbox, and execution continues. A program that never
    // awaits runs to Done, which reports the unstarted call.
    let mut vm = VM::new(vec![
        PushFloat(1.0),
        PushFloat(2.0),
        Invoke("my_tool".into(), 2),
    ]);
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { unstarted, .. } => {
            assert_eq!(unstarted.len(), 1);
            assert_eq!(unstarted[0].name, "my_tool");
            // push order = arg order: Push(1), Push(2) -> args [1, 2]
            assert_eq!(unstarted[0].args, vec![n(1.0), n(2.0)]);
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(vm.stack, vec![Value::Promise(0)]);
    assert!(matches!(vm.promises[0], PromiseState::Pending { .. }));
}

#[test]
fn await_non_promise_passes_through() {
    // `await 42` is the identity.
    assert_eq!(run(vec![PushFloat(42.0), Await]), vec![n(42.0)]);
}

#[test]
fn await_rejected_escalates_resumably() {
    // A rejected promise escalates through the Phase 3 path: the promise is
    // consumed and the error is PushValueThenContinue-resumable, so the host
    // may substitute a value for the rejection.
    let mut vm = VM::new(vec![Invoke("f".into(), 0), Await, Return(1)]);
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls[0].promise,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.reject_promise(id, Value::String("boom".into())).unwrap();
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(err.message.contains("rejected"), "got: {}", err.message);
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    vm.resume_with(&err, n(7.0)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, n(7.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn raise_yields() {
    match run_effect(vec![Raise("something_broke".into(), 0)]) {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "something_broke");
            assert!(payload.is_none());
        }
        other => panic!("expected Raise, got {other:?}"),
    }
}

#[test]
fn await_pending_yields_and_resumes() {
    let mut vm = VM::new(vec![
        PushFloat(10.0),
        PushFloat(3.0),
        Invoke("add".into(), 2),
        Await,
        Return(1), // return the result
    ]);
    // First step runs to the Await, which blocks and delivers the call.
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "add");
            assert_eq!(calls[0].args, vec![n(10.0), n(3.0)]);
            calls[0].promise
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(id, n(13.0)).unwrap();
    // Resume — the Await re-executes and completes with the result.
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert_eq!(value, n(13.0));
            assert!(unstarted.is_empty());
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(vm.stack, vec![n(13.0)]);
}

#[test]
fn outbox_accumulates_across_other_ops() {
    // Two Invokes separated by other instructions still land in ONE Pending
    // yield: fan-out no longer depends on instruction adjacency.
    let mut vm = VM::new(vec![
        PushFloat(1.0),
        PushFloat(2.0),
        Invoke("a".into(), 2),
        PushFloat(3.0),
        Invoke("b".into(), 1),
        Await,  // b's promise (top of stack)
        Dig(1), // bring a's promise to the top
        Await,
    ]);
    let (pa, pb) = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].name, "a");
            assert_eq!(calls[0].args, vec![n(1.0), n(2.0)]);
            assert_eq!(calls[1].name, "b");
            assert_eq!(calls[1].args, vec![n(3.0)]);
            (calls[0].promise, calls[1].promise)
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(pa, n(100.0)).unwrap();
    vm.resolve_promise(pb, n(200.0)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(vm.stack, vec![n(200.0), n(100.0)]);
}

#[test]
fn out_of_order_resolution() {
    // The program awaits `a` first, but the host resolves `b` first: the
    // re-executed Await yields a second Pending (with an empty calls list —
    // everything was already delivered) until `a` is resolved.
    let mut vm = VM::new(vec![
        PushFloat(1.0),
        Invoke("a".into(), 1),
        PushFloat(2.0),
        Invoke("b".into(), 1),
        Dig(1), // a's promise on top
        Await,
        Dig(1), // b's promise on top
        Await,
    ]);
    let (pa, pb) = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 2);
            (calls[0].promise, calls[1].promise)
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(pb, n(22.0)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => assert!(calls.is_empty()),
        other => panic!("expected Pending, got {other:?}"),
    }
    vm.resolve_promise(pa, n(11.0)).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Done { .. }
    ));
    assert_eq!(vm.stack, vec![n(11.0), n(22.0)]);
}

#[test]
fn await_already_resolved_does_not_yield() {
    // Awaiting an already-settled promise proceeds without a host round-trip,
    // and a second await of the same promise sees the same value.
    let mut vm = VM::new(vec![
        Invoke("f".into(), 0),
        Pick(0), // duplicate the promise
        Await,
        Pop(1), // discard the first await's value
        Await,  // promise underneath: still resolved
    ]);
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls[0].promise,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(id, n(5.0)).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Done { .. }
    ));
    assert_eq!(vm.stack, vec![n(5.0)]);
}

#[test]
fn settle_promise_misuse_errors() {
    let mut vm = VM::new(vec![Invoke("f".into(), 0), Await]);
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls[0].promise,
        other => panic!("expected Pending, got {other:?}"),
    };
    // Bad id.
    assert!(vm.resolve_promise(id + 1, Value::Null).is_err());
    // Double settle.
    vm.resolve_promise(id, n(1.0)).unwrap();
    assert!(vm.resolve_promise(id, n(2.0)).is_err());
    assert!(vm.reject_promise(id, Value::Null).is_err());
}

// ── edge cases ────────────────────────────────────────────────

#[test]
fn empty_program_done() {
    let mut vm = VM::new(vec![]);
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Done { .. }
    ));
}

#[test]
fn number_signed_zero() {
    // -0.0 and +0.0 should be equal for Eq
    assert_eq!(
        run(vec![PushFloat(-0.0), PushFloat(0.0), Eq]),
        vec![b(true)]
    );
}

// ── robustness / regression ───────────────────────────────────

#[test]
fn fuel_stops_infinite_loop() {
    // [Jump(0)] loops forever; the fuel slice must break it.
    let mut vm = VM::new(vec![Jump(0)]);
    assert!(matches!(vm.step(1000).unwrap(), StepResult::OutOfFuel));
}

#[test]
fn fuel_is_consumed_per_instruction() {
    // Three instructions: a 2-fuel slice runs dry, one more unit finishes.
    let mut vm = VM::new(vec![PushFloat(1.0), PushFloat(2.0), Add]);
    assert!(matches!(vm.step(2).unwrap(), StepResult::OutOfFuel));
    assert!(matches!(vm.step(1).unwrap(), StepResult::Done { .. }));
}

#[test]
fn dangling_pointer_does_not_panic() {
    // A PushPtr with no backing heap cell must error, not panic.
    assert!(matches!(
        run_err(vec![PushObject(99), GetLength]).kind,
        ErrorKind::ValueError
    ));
    // Type predicates stay total (false) on a dangling pointer.
    assert_eq!(run(vec![PushObject(99), IsStr]), vec![b(false)]);
    // `===` on pointers is pure reference identity (no heap lookup), so the
    // same address compares equal — even when dangling — without panicking.
    assert_eq!(run(vec![PushObject(99), PushObject(99), Eq]), vec![b(true)]);
}

#[test]
fn object_array_equality_is_by_reference() {
    // JS ===: two distinct arrays/objects are never equal, even with
    // identical content.
    let code = vec![ps("abc"), ArrNew(1), ps("abc"), ArrNew(1), Eq];
    assert_eq!(run(code), vec![b(false)]);
    // But the SAME array (one allocation, duplicated handle) is equal.
    let code = vec![ps("abc"), ArrNew(1), Pick(0), Eq];
    assert_eq!(run(code), vec![b(true)]);
    // Strings remain primitives: distinct allocations compare by content.
    assert_eq!(run(vec![ps("abc"), ps("abc"), Eq]), vec![b(true)]);
}

#[test]
fn frame_allocates_multiple_locals() {
    // A frame allocates all its locals at once (EnterFrame), yielding
    // independent slots.
    let code = vec![
        Call(2, 0),
        Return(1), // propagate the function's result to the final stack
        EnterFrame(0, false, plain(2).into()),
        PushFloat(7.0),
        SetLocal(0),
        PushFloat(8.0),
        SetLocal(1),
        GetLocal(0),
        GetLocal(1),
        Add,
        Return(1),
    ];
    assert_eq!(run(code), vec![n(15.0)]);
}

#[test]
fn bit_shift_rejects_bad_count() {
    assert!(matches!(
        run_err(vec![PushFloat(1.0), PushFloat(64.0), BitLhs]).kind,
        ErrorKind::ValueError
    ));
    assert!(matches!(
        run_err(vec![PushFloat(1.0), PushFloat(-1.0), BitRhs]).kind,
        ErrorKind::ValueError
    ));
    // Valid shifts still work.
    assert_eq!(
        run(vec![PushFloat(1.0), PushFloat(3.0), BitLhs]),
        vec![n(8.0)]
    );
}

#[test]
fn pop_respects_frame_floor() {
    // fn: 1 arg, 1 local, no temporaries. Pop must not steal a local.
    let code = vec![
        PushFloat(1.0),
        Call(3, 1),
        Return(0),
        EnterFrame(0, false, plain(1).into()), // local 0; sp == frame floor
        Pop(1),                                // nothing above the floor -> underflow
        Return(0),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::StackUnderflow));
}

#[test]
fn dup_cannot_duplicate_local() {
    let code = vec![
        PushFloat(1.0),
        Call(3, 1),
        Return(0),
        EnterFrame(0, false, plain(1).into()), // local 0; sp == floor
        Pick(0),                               // nothing above the floor -> underflow
        Return(0),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::StackUnderflow));
}

#[test]
fn swap_cannot_cross_frame_floor() {
    // One local + one temporary: Dig(1) (formerly Swap) needs two temporaries
    // above the floor, but only one exists.
    let code = vec![
        PushFloat(1.0),
        Call(3, 1),
        Return(0),
        EnterFrame(0, false, plain(1).into()), // local 0
        PushFloat(9.0),                        // single temporary
        Dig(1),                                // would swap the temp with the local -> underflow
        Return(0),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::StackUnderflow));
}

#[test]
fn rot_cannot_cross_frame_floor() {
    let code = vec![
        PushFloat(1.0),
        Call(3, 1),
        Return(0),
        EnterFrame(0, false, plain(1).into()), // local 0
        PushFloat(8.0),                        // two temporaries (need three for Dig(2))
        PushFloat(9.0),
        Dig(2),
        Return(0),
    ];
    assert!(matches!(run_err(code).kind, ErrorKind::StackUnderflow));
}

#[test]
fn stack_ops_work_within_frame() {
    // Sanity: with enough temporaries above the floor, the ops succeed
    // and leave locals/args untouched.
    let code = vec![
        PushFloat(5.0),
        Call(3, 1),
        Return(1),
        EnterFrame(0, false, plain(1).into()), // local 0
        PushFloat(10.0),
        SetLocal(0),    // local 0 = 10
        PushFloat(1.0), // temporaries: [1, 2]
        PushFloat(2.0),
        Dig(1),      // -> [2, 1]
        Pop(1),      // -> [2]
        GetLocal(0), // -> [2, 10]
        Add,         // -> [12]
        Return(1),
    ];
    assert_eq!(run(code), vec![n(12.0)]);
}

// ── Int transport type ────────────────────────────────────────

#[test]
fn negative_integers_are_negint_and_roundtrip() {
    // PosInt and NegInt never compare equal even at the boundary value 0
    // representations (different sign domains).
    assert_eq!(run(vec![PushPosInt(5), PushNegInt(-5), Eq]), vec![b(false)]);
    // Ordering across the sign boundary is structural.
    assert_eq!(
        run(vec![PushNegInt(-1), PushPosInt(u64::MAX), Lt]),
        vec![b(true)]
    );
}

#[test]
fn posint_too_large_for_index_errors() {
    // A PosInt beyond i64::MAX can't be an array index -> error, no panic.
    let code = vec![PushFloat(1.0), ArrNew(1), PushPosInt(u64::MAX), IndexGet];
    assert!(matches!(run_err(code).kind, ErrorKind::TypeError));
}

#[test]
fn int_arithmetic_degrades_to_number() {
    // The transport guarantee is identity-preservation, NOT integer math:
    // any arithmetic promotes Int -> Number(f64).
    assert_eq!(run(vec![PushPosInt(2), PushPosInt(3), Add]), vec![n(5.0)]);
    assert_eq!(run(vec![PushPosInt(10), PushFloat(4.0), Sub]), vec![n(6.0)]);
    assert_eq!(run(vec![PushPosInt(10), PushPosInt(3), Mod]), vec![n(1.0)]);
    assert_eq!(run(vec![PushPosInt(5), Neg]), vec![n(-5.0)]);
}

#[test]
fn int_number_cross_comparison() {
    // 1 == 1.0, ordering works across Int/Number.
    assert_eq!(run(vec![PushPosInt(1), PushFloat(1.0), Eq]), vec![b(true)]);
    assert_eq!(run(vec![PushPosInt(2), PushFloat(2.5), Lt]), vec![b(true)]);
    assert_eq!(
        run(vec![PushFloat(3.0), PushPosInt(3), GtEq]),
        vec![b(true)]
    );
    assert_eq!(run(vec![PushPosInt(2), PushPosInt(2), Eq]), vec![b(true)]);
}

#[test]
fn int_indices_and_bitops() {
    // Int works directly as an array index (left-to-right: first = arr[0]).
    let code = vec![
        PushFloat(10.0),
        PushFloat(20.0),
        ArrNew(2), // [10, 20]
        PushPosInt(1),
        IndexGet,
    ];
    assert_eq!(run(code), vec![n(20.0)]);
    // ...and as a bitwise operand.
    assert_eq!(
        run(vec![PushPosInt(10), PushPosInt(12), BitAnd]),
        vec![n(8.0)]
    );
}

// ── Phase 0: allocation baseline ────────────────────────────

/// Run a representative hot-loop workload and record the allocation count.
/// Each iteration does Math.abs + a string concat `s += "x"`. String
/// literals ride inline in `PushStr` as `RcStr` (each push is a refcount
/// bump, zero per-iteration literal allocation); only the concat allocates.
#[test]
fn alloc_baseline_hot_loop() {
    use crate::alloc_counter;

    // Build a loop that does builtin calls + string concat (the hot paths).
    let mut code = Vec::new();
    code.push(ps("hello")); // s = "hello"
    for _ in 0..100 {
        // Math.abs(-42) → drop result (just measuring the call overhead)
        code.push(PushFloat(-42.0));
        code.push(CallBuiltin(Builtin::MathAbs, 1));
        code.push(Pop(1));
        // s += "x" — the literal is a refcount bump; the Add allocates.
        code.push(ps("x"));
        code.push(Add);
    }
    code.push(Pop(1)); // drop s

    // Reset after building `code` so the literals' construction isn't counted.
    let mut vm = VM::new(code);
    alloc_counter::reset();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("unexpected effect: {other:?}"),
    }
    let allocs = alloc_counter::count();
    eprintln!("BASELINE hot_loop_100_iter: {allocs} allocs");
    assert!(allocs > 0, "should have some allocations");
}

/// Breakdown: measure each allocation source in isolation.
#[test]
fn alloc_breakdown() {
    use crate::alloc_counter;

    // 1. How many allocs to materialize one string value? One: the single
    //    `RcStr` block (header + bytes).
    alloc_counter::reset();
    let _s = Value::String(RcStr::from("x"));
    let per_string = alloc_counter::count();
    eprintln!("  RcStr::from: {per_string}");

    // 2. How many allocs for to_js_string on a string value? Zero — the fast
    //    path clones the existing `RcStr` (a refcount bump).
    let v = Value::String(RcStr::from("hello"));
    let vm = VM::new(vec![]);
    alloc_counter::reset();
    let _ = vm.to_js_string(&v, 0);
    let per_to_js_string = alloc_counter::count();
    eprintln!("  to_js_string on string: {per_to_js_string}");

    // 3. How many allocs for a single Add (string + string)? The pushes are
    //    refcount bumps; only the result string allocates.
    let code = vec![ps("hello"), ps("x"), Add];
    let mut vm = VM::new(code);
    alloc_counter::reset();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("unexpected effect: {other:?}"),
    }
    let per_add = alloc_counter::count();
    eprintln!("  Add (str+str): {per_add}");

    // 4. How many allocs for Math.abs call?
    alloc_counter::reset();
    {
        let mut vm = VM::new(vec![PushFloat(-42.0), CallBuiltin(Builtin::MathAbs, 1)]);
        match vm.step(u64::MAX).unwrap() {
            StepResult::Done { .. } => {}
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    let per_math_abs = alloc_counter::count();
    eprintln!("  Math.abs: {per_math_abs}");

    // 5. VM construction (Vec::new for heap/stack/cells/callstack)
    alloc_counter::reset();
    {
        let _vm = VM::new(vec![]);
    }
    let vm_new = alloc_counter::count();
    eprintln!("  VM::new: {vm_new}");

    // 6. Stack Vec growth during execution
    alloc_counter::reset();
    {
        let mut vm = VM::new(vec![]);
        for _ in 0..10 {
            vm.stack.push(Value::Null);
        }
    }
    let stack_growth = alloc_counter::count();
    eprintln!("  stack push x10: {stack_growth}");

    // 7. Per-instruction: CallDyn on a bare Fn (no closure, no upvals).
    alloc_counter::reset();
    {
        // Program: push Fn(3), CallDyn(0, false), Return(0) | PushPosInt(42), Return(1)
        let mut vm = VM::new(vec![
            PushFn(3, u32::MAX, 0),
            CallDyn(0, false),
            Return(0),
            PushPosInt(42),
            Return(1),
        ]);
        match vm.step(u64::MAX).unwrap() {
            StepResult::Done { .. } => {}
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    let call_dyn_ret = alloc_counter::count();
    eprintln!("  CallDyn + Return (bare Fn): {call_dyn_ret}");

    // 8. Two CallDyn calls.
    alloc_counter::reset();
    {
        let mut vm = VM::new(vec![
            PushFn(5, u32::MAX, 0),
            CallDyn(0, false),
            PushFn(5, u32::MAX, 0),
            CallDyn(0, false),
            Return(0),
            PushPosInt(42),
            Return(1),
        ]);
        match vm.step(u64::MAX).unwrap() {
            StepResult::Done { .. } => {}
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    let call_dyn_x2 = alloc_counter::count();
    eprintln!("  CallDyn x2: {call_dyn_x2}");
    eprintln!(
        "  -> marginal per extra CallDyn: {}",
        call_dyn_x2.saturating_sub(call_dyn_ret)
    );
}

// ── string edge cases ───────────────────────────────────────────

#[test]
fn string_mid_codepoint_index_errors() {
    // Indexing into the middle of a multi-byte UTF-8 codepoint is a
    // ValueError (not a panic).
    let mut vm = VM::new(vec![
        PushStr("é".into()), // 2-byte UTF-8
        PushPosInt(1),       // middle of codepoint
        IndexGet,
    ]);
    match vm.step(u64::MAX) {
        Err(e) if e.kind == ErrorKind::ValueError => {} // expected
        other => panic!("expected ValueError, got {other:?}"),
    }
}

#[test]
fn string_empty_needle_index_of() {
    // Empty-string needle: `indexOf` returns 0 (matching JS).
    let mut vm = VM::new(vec![
        PushStr("hello".into()),
        PushStr("".into()),
        CallBuiltin(Builtin::StrIndexOf, 2),
    ]);
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("unexpected effect: {other:?}"),
    }
    assert_eq!(vm.stack.last(), Some(&Value::PosInt(0)));
}

// ── resource guards ─────────────────────────────────────────────

#[test]
fn out_of_fuel_stops_infinite_loop() {
    let mut vm = VM::new(vec![
        Jump(0), // infinite loop
    ]);
    // A tiny slice yields OutOfFuel every time; the loop never escapes.
    assert!(matches!(vm.step(5).unwrap(), StepResult::OutOfFuel));
    assert!(matches!(vm.step(5).unwrap(), StepResult::OutOfFuel));
}

// ── relational non-coercion (JS divergence) ─────────────────────

#[test]
fn relational_non_coercion() {
    // Cross-type comparisons return false without coercion.
    // String vs number: "2" > 1 → false.
    assert_eq!(
        run(vec![ps("2"), PushFloat(1.0), Gt]),
        vec![Value::Bool(false)]
    );
    // Number vs string: 1 < "2" → false.
    assert_eq!(
        run(vec![PushFloat(1.0), ps("2"), Lt]),
        vec![Value::Bool(false)]
    );
}

// ── negative-index array errors (JS divergence) ─────────────────

#[test]
fn arr_index_negative_errors() {
    // Negative IndexGet on an array is a ValueError.
    let mut vm = VM::new(vec![
        PushPosInt(0),  // dummy array element
        ArrNew(1),      // create array (pops 1 value)
        PushNegInt(-1), // index -1
        IndexGet,       // should error
    ]);
    match vm.step(u64::MAX) {
        Err(e) if e.kind == ErrorKind::ValueError => {} // expected
        other => panic!("expected ValueError, got {other:?}"),
    }
    // Negative IndexSet on an array is also a ValueError.
    let mut vm = VM::new(vec![
        PushPosInt(0),  // dummy array element
        ArrNew(1),      // create array (pops 1 value)
        PushNegInt(-1), // index -1
        PushPosInt(99), // value to set
        IndexSet(SetMode::New),
    ]);
    match vm.step(u64::MAX) {
        Err(e) if e.kind == ErrorKind::ValueError => {} // expected
        other => panic!("expected ValueError, got {other:?}"),
    }
}

// ── UTF-8 string length (JS divergence) ─────────────────────────

#[test]
fn string_utf8_length() {
    // String `.length` returns byte count, not char count.
    // "é" is 2 bytes in UTF-8.
    assert_eq!(run(vec![ps("é"), GetLength]), vec![Value::Float(2.0)]);
    // "😀" is 4 bytes.
    assert_eq!(run(vec![ps("😀"), GetLength]), vec![Value::Float(4.0)]);
}

// ── NaN in Math.min/max (JS divergence) ─────────────────────────

#[test]
fn math_min_max_nan_propagates() {
    // JS: Math.min with NaN propagates NaN.
    let out = run(vec![
        PushFloat(f64::NAN),
        PushFloat(5.0),
        CallBuiltin(Builtin::MathMin, 2),
    ]);
    assert!(out[0].as_f64().is_some_and(|n| n.is_nan()), "got {out:?}");
    // JS: Math.max with NaN propagates NaN.
    let out = run(vec![
        PushFloat(f64::NAN),
        PushFloat(3.0),
        CallBuiltin(Builtin::MathMax, 2),
    ]);
    assert!(out[0].as_f64().is_some_and(|n| n.is_nan()), "got {out:?}");
}

// ── Invoke interleaved with Raise ───────────────────────────────

#[test]
fn invoke_interleaved_with_raise() {
    // Invoke never yields; Raise still does. Calls started before the Raise
    // are delivered (in start order) by the first Await after it.
    let mut vm = VM::new(vec![
        PushPosInt(1),  // A arg0
        PushPosInt(10), // A arg1
        Invoke("A".into(), 2),
        Raise("err".into(), 0),
        Pop(1),         // discard the raise's resumed value
        PushPosInt(2),  // B arg0
        PushPosInt(20), // B arg1
        Invoke("B".into(), 2),
        Await,  // B's promise (top)
        Dig(1), // A's promise
        Await,
    ]);
    // First yield is the Raise — the started call A stays in the outbox.
    match vm.step(u64::MAX).unwrap() {
        StepResult::Raise { condition, .. } => assert_eq!(condition, "err"),
        other => panic!("expected Raise, got {other:?}"),
    }
    vm.resume_raise(Value::PosInt(300));
    // The Await delivers both A (pre-Raise) and B (post-Raise) together.
    let (pa, pb) = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].name, "A");
            assert_eq!(calls[1].name, "B");
            (calls[0].promise, calls[1].promise)
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(pa, Value::PosInt(100)).unwrap();
    vm.resolve_promise(pb, Value::PosInt(200)).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Done { .. }
    ));
    assert_eq!(vm.stack, vec![Value::PosInt(200), Value::PosInt(100)]);
}

// ── fuel charged per invoke ─────────────────────────────────────

#[test]
fn fuel_charged_per_invoke() {
    // Each Invoke is one instruction = one fuel unit; starting many calls
    // cannot bypass the budget.
    let code = || {
        vec![
            PushPosInt(1),
            Invoke("X".into(), 1),
            PushPosInt(2),
            Invoke("Y".into(), 1),
        ]
    };
    let mut vm = VM::new(code());
    // Exactly one fuel unit per instruction.
    match vm.step(4).unwrap() {
        StepResult::Done { unstarted, .. } => assert_eq!(unstarted.len(), 2),
        other => panic!("expected Done, got {other:?}"),
    }
    // One unit less runs out before the second Invoke.
    let mut vm = VM::new(code());
    match vm.step(3).unwrap() {
        StepResult::OutOfFuel => {}
        other => panic!("expected OutOfFuel, got {other:?}"),
    }
}

// ── empty-string needle for includes / startsWith ───────────────

#[test]
fn string_empty_needle_includes_starts_with() {
    // Empty-string `includes` returns true.
    assert_eq!(
        run(vec![
            ps("hello"),
            ps(""),
            CallBuiltin(Builtin::StrIncludes, 2)
        ]),
        vec![Value::Bool(true)]
    );
    // Empty-string `startsWith` returns true.
    assert_eq!(
        run(vec![
            ps("hello"),
            ps(""),
            CallBuiltin(Builtin::StrStartsWith, 2)
        ]),
        vec![Value::Bool(true)]
    );
}

// ── JSON depth limits ───────────────────────────────────────────

#[test]
fn stack_value_to_json_depth_limit() {
    // A deeply nested value (depth > MAX_JSON_DEPTH=128) must error,
    // not hang or panic.
    let mut vm = VM::new(vec![]);
    let mut innermost = Value::Null;
    for _ in 0..130 {
        let obj = vm.objects.len() as u32;
        vm.objects.push(ObjData {
            proto: None,
            map: [(
                RcStr::from("x"),
                std::mem::replace(&mut innermost, Value::Null),
            )]
            .into_iter()
            .collect(),
        });
        innermost = Value::Object(obj);
    }
    // `stack_value_to_json` on the deeply nested value should error,
    // and the message should say so (Step 4: JSON depth message quality).
    let result = vm.stack_value_to_json(&innermost, 0);
    assert!(
        matches!(result, Err(ref e) if e.kind == ErrorKind::ValueError),
        "expected ValueError for depth > 128, got {result:?}"
    );
    let msg = result.unwrap_err().message;
    assert!(msg.contains("depth"), "got: {msg}");
}

#[test]
fn json_to_value_depth_limit() {
    // A deeply nested JSON seed must error, not hang or panic.
    let mut val = serde_json::Value::Null;
    for _ in 0..130 {
        val = serde_json::json!({"x": val});
    }
    // `json_to_stack_value` on the deeply nested JSON should error,
    // and the message should say so.
    let mut vm = VM::new(vec![]);
    let result = vm.json_to_stack_value(&val, 0);
    assert!(
        matches!(result, Err(ref e) if e.kind == ErrorKind::ValueError),
        "expected ValueError for depth > 128, got {result:?}"
    );
    let msg = result.unwrap_err().message;
    assert!(msg.contains("depth"), "got: {msg}");
}

// ── Step 1: VM retains spans and source ──────────────────────────

#[test]
fn for_program_populates_spans_and_source() {
    // After `for_program`, the VM's spans table has one entry per instruction
    // and the source is non-empty.
    let prog = crate::testutil::compile_ok("return 1 + 2;");
    let vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    assert_eq!(
        vm.spans.len(),
        vm.code.len(),
        "spans must have one entry per instruction"
    );
    assert!(!vm.source.is_empty(), "source must be non-empty");
}

#[test]
fn vm_new_leaves_spans_and_source_empty() {
    let vm = VM::new(vec![Instr::PushPosInt(0)]);
    assert!(vm.spans.is_empty());
    assert!(vm.source.is_empty());
}

// ── render_error tests ─────────────────────────────────────────

#[test]
fn render_error_with_source_and_spans() {
    // A compiled program's error should render with the source line + caret.
    let prog = crate::testutil::compile_ok("return [] - 1;");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = loop {
        match vm.step(u64::MAX) {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("expected error"),
            Ok(_) => {}
        }
    };
    let rendered = vm.render_error(&err);
    assert!(
        rendered.contains("return [] - 1;"),
        "should contain source line, got: {rendered}"
    );
    assert!(
        rendered.contains("^"),
        "should contain caret, got: {rendered}"
    );
    assert!(
        rendered.contains("cannot coerce array") && rendered.contains("to number"),
        "should contain coercion message with type, got: {rendered}"
    );
}

#[test]
fn render_error_without_source_falls_back() {
    // A VM::new error (no spans/source) should render as "at instruction N".
    let mut vm = VM::new(vec![Instr::Pop(1)]); // will StackUnderflow
    let err = vm.step(u64::MAX).unwrap_err();
    let rendered = vm.render_error(&err);
    assert!(
        rendered.contains("at instruction 0"),
        "should show instruction ip, got: {rendered}"
    );
    assert!(
        rendered.contains("stack underflow"),
        "should show the message, got: {rendered}"
    );
}

// ── resume_with tests (Step 3) ──────────────────────────────────

#[test]
fn resume_with_push_value_then_continue() {
    // `return [] - 1;` → TypeError in Sub (binary_num! pops first).
    // resume_with should feed a replacement value and the program completes.
    let prog = crate::testutil::compile_ok("return [] - 1;");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = loop {
        match vm.step(u64::MAX) {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("expected error"),
            Ok(_) => {}
        }
    };
    assert!(matches!(err.kind, ErrorKind::TypeError));
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    // Feed 0.0 as the subtraction result; program should complete with 0.
    vm.resume_with(&err, Value::Float(0.0)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::Float(0.0));
        }
        other => panic!("expected Done after resume, got {other:?}"),
    }
}

#[test]
fn out_of_fuel_slices_resume() {
    // OutOfFuel consumes nothing: a zero-fuel slice yields immediately,
    // a later slice continues where it left off, and driving the whole
    // program with `step(1)` completes like one big slice.
    let mut vm = VM::new(vec![PushPosInt(1), PushPosInt(2), Add]);
    assert!(matches!(vm.step(0).unwrap(), StepResult::OutOfFuel));
    match vm.step(10).unwrap() {
        StepResult::Done { .. } => {} // fine
        other => panic!("expected Done, got {other:?}"),
    }
    let mut vm = VM::new(vec![PushPosInt(1), PushPosInt(2), Add]);
    loop {
        match vm.step(1).unwrap() {
            StepResult::OutOfFuel => continue,
            StepResult::Done { .. } => break,
            other => panic!("expected Done, got {other:?}"),
        }
    }
    assert_eq!(vm.stack, vec![Value::Float(3.0)]);
}

#[test]
fn resume_with_not_resumable_errors() {
    // A bad local index is an invariant violation: NotResumable. resume_with
    // must fail.
    let mut vm = VM::new(vec![GetLocal(3)]);
    let err = vm.step(u64::MAX).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::BadLocal));
    assert!(matches!(err.resume, ResumeMode::NotResumable));
    let result = vm.resume_with(&err, Value::Null);
    assert!(result.is_err(), "resume_with on NotResumable should error");
    assert!(matches!(result.unwrap_err().kind, ErrorKind::BadArg));
}

#[test]
fn try_exit_without_handler_is_bad_arg() {
    let err = run_err(vec![TryExit]);
    assert!(matches!(err.kind, ErrorKind::BadArg));
    assert!(matches!(err.resume, ResumeMode::NotResumable));
}

#[test]
fn try_enter_throw_unwinds_to_handler() {
    // TryEnter(3) guards Throw; the handler at ip 3 receives the value.
    // 0: TryEnter(3)  1: PushStr("x")  2: Throw  3: (handler; "x" on stack)
    let stack = run(vec![TryEnter(3), ps("x"), Throw]);
    assert_eq!(stack, vec![Value::String("x".into())]);
}

#[test]
fn throw_without_handler_is_uncaught_exception() {
    let err = run_err(vec![ps("boom"), Throw]);
    assert!(matches!(err.kind, ErrorKind::UncaughtException));
    assert!(matches!(err.resume, ResumeMode::NotResumable));
    assert!(
        err.message.contains("uncaught exception"),
        "got: {}",
        err.message
    );
    // The thrown value is preserved structurally.
    assert_eq!(err.payload, Some(Value::String("boom".into())));
}

#[test]
fn try_exit_after_clean_body_pops_handler() {
    // A throw after TryExit must NOT be caught by the exited handler.
    let err = run_err(vec![TryEnter(4), TryExit, ps("late"), Throw]);
    assert!(matches!(err.kind, ErrorKind::UncaughtException));
    assert!(err.message.contains("uncaught"), "got: {}", err.message);
}

#[test]
fn objget_non_object_pops_receiver_and_resumes() {
    // Pop-first normalization: ObjGet on a non-object consumes the receiver,
    // so the error is PushValueThenContinue and resume_with works unchanged.
    let mut vm = VM::new(vec![PushPosInt(1), ObjGet("foo".into())]);
    let err = vm.step(u64::MAX).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::TypeError));
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    assert_eq!(vm.stack.len(), 0, "receiver consumed before the error");
    vm.resume_with(&err, Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(vm.stack, vec![Value::Null]);
}

#[test]
fn objset_non_object_pops_operands_and_resumes() {
    // ObjSet pops the value, then (in the error arm) the receiver: both
    // operands consumed → PushValueThenContinue.
    let mut vm = VM::new(vec![
        PushPosInt(1),
        PushPosInt(2),
        ObjSet("foo".into(), SetMode::New),
    ]);
    let err = vm.step(u64::MAX).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::TypeError));
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    assert_eq!(vm.stack.len(), 0, "value and receiver both consumed");
    vm.resume_with(&err, Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn push_value_then_continue_pop_first_invariant() {
    // Verify the invariant: after a PushValueThenContinue error, the stack
    // has the instruction's operands consumed, so resume_with just pushes
    // and continues without additional fixup.
    let mut vm = VM::new(vec![
        PushPosInt(42),            // some unrelated value
        PushStr("notanum".into()), // operand that will fail to_number()
        BitNot,                    // unary op: pops operand, fails to_number
    ]);
    // Stack: [42, "notanum"]
    let err = loop {
        match vm.step(u64::MAX) {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("expected error"),
            Ok(_) => {}
        }
    };
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    // After the error, the failed operand was consumed — only 42 remains.
    assert_eq!(vm.stack.len(), 1);
    // resume_with pushes a replacement and advances ip past BitNot.
    vm.resume_with(&err, Value::Float(99.0)).unwrap();
    assert_eq!(vm.stack.len(), 2);
    // Continue stepping — should reach Done.
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── Step 4: message quality tests ──────────────────────────────

#[test]
fn type_name_covers_all_variants() {
    use crate::vm::value::Value;
    assert_eq!(Value::Undefined.type_name(), "undefined");
    assert_eq!(Value::Null.type_name(), "null");
    assert_eq!(Value::Bool(true).type_name(), "boolean");
    assert_eq!(Value::Float(1.0).type_name(), "number");
    assert_eq!(Value::PosInt(1).type_name(), "number");
    assert_eq!(Value::NegInt(-1).type_name(), "number");
    assert_eq!(Value::String("hi".into()).type_name(), "string");
    assert_eq!(Value::Array(0).type_name(), "array");
    assert_eq!(Value::Object(0).type_name(), "object");
    assert_eq!(Value::Closure { addr: 0, ptr: 0 }.type_name(), "function");
}

#[test]
fn preview_string_truncation() {
    let vm = VM::new(vec![]);
    // Short string: quoted as-is.
    let short = Value::String("hello".into());
    assert!(
        vm.preview(&short).contains("hello"),
        "got: {}",
        vm.preview(&short)
    );
    // Long string: truncated with …
    let long = Value::String("a".repeat(50).into());
    let prev = vm.preview(&long);
    assert!(prev.starts_with('\"') && prev.contains('…'), "got: {prev}");
    assert!(prev.len() <= 50, "too long: {prev}");
    // Multi-byte codepoint straddling the truncation point: must floor to a
    // char boundary, not panic (39 ASCII bytes + 4-byte emoji spans byte 40).
    let tricky = format!("{}🎉🎉", "a".repeat(39));
    let prev = vm.preview(&Value::String(tricky.into()));
    assert!(prev.contains('…'), "got: {prev}");
}

#[test]
fn preview_array_summary() {
    let mut vm = VM::new(vec![]);
    let arr = vm.alloc_array(vec![Value::PosInt(1), Value::PosInt(2)].into());
    assert_eq!(vm.preview(&arr), "[array of 2]");
}

#[test]
fn preview_object_summary() {
    let mut vm = VM::new(vec![]);
    let obj = vm.alloc_object(
        [
            ("a".into(), Value::PosInt(1)),
            ("b".into(), Value::PosInt(2)),
        ]
        .into_iter()
        .collect(),
    );
    let prev = vm.preview(&obj);
    assert!(prev.starts_with("{object with keys"), "got: {prev}");
    assert!(prev.contains("a") && prev.contains("b"), "got: {prev}");
}

#[test]
fn message_coercion_includes_type_name() {
    // binary_num! TypeError: `[] + 1` → array + number
    let prog = crate::testutil::compile_ok("return [] + 1;");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = loop {
        match vm.step(u64::MAX) {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("expected error"),
            Ok(_) => {}
        }
    };
    let msg = &err.message;
    assert!(msg.contains("array"), "should mention array type: {msg}");
    assert!(msg.contains("number"), "should mention number type: {msg}");
    assert!(
        msg.contains("[array of 0]"),
        "should include preview: {msg}"
    );
}

#[test]
fn message_indexget_includes_container_type() {
    // IndexGet on non-container: container first (deep), key second (top).
    let err = run_err(vec![
        PushPosInt(42), // container (deep, popped second)
        PushPosInt(0),  // key (top, popped first)
        IndexGet,
    ]);
    assert!(
        err.message.contains("number"),
        "should mention type: {}",
        err.message
    );
    assert!(
        err.message.contains("cannot index into"),
        "got: {}",
        err.message
    );
}

#[test]
fn message_objget_non_object_includes_type() {
    // ObjGet peeks stack.last() — if it's not an object, message names the type.
    let err = run_err(vec![PushPosInt(42), ObjGet("key".into())]);
    assert!(
        err.message.contains("number"),
        "should mention type: {}",
        err.message
    );
}

#[test]
fn message_negative_index_includes_value() {
    // IndexGet with negative index: container first (deep), key second (top).
    let mut vm = VM::new(vec![
        PushArray(0),   // container (deep, popped second)
        PushNegInt(-1), // key (top, popped first)
        IndexGet,
    ]);
    vm.arrays.push(vec![].into());
    let err = vm.step(u64::MAX).unwrap_err();
    assert!(
        err.kind == ErrorKind::ValueError,
        "expected ValueError, got {:?}: {}",
        err.kind,
        err.message
    );
}

#[test]
fn message_calldyn_non_callable_and_resume() {
    // CallDyn on a non-callable: message names the type, the args are
    // dropped (pop-first normalization), and resume_with substitutes the
    // call result so the program completes with a balanced stack.
    // (A function-typed variable reassigned to a number forces the dynamic
    // call path with a number callable — `let n = 5; n()` would resolve as
    // an unknown global instead.)
    let prog = crate::testutil::compile_ok("let f = () => 1; f = 5; return f(1, 2);");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = loop {
        match vm.step(u64::MAX) {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("expected error"),
            Ok(_) => {}
        }
    };
    assert!(
        err.message.contains("cannot call a number"),
        "got: {}",
        err.message
    );
    assert!(
        matches!(err.resume, ResumeMode::PushValueThenContinue),
        "got {:?}",
        err.resume
    );
    vm.resume_with(&err, Value::PosInt(7)).unwrap();
    let value = match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => value,
        other => panic!("unexpected effect: {other:?}"),
    };
    assert_eq!(value, Value::PosInt(7));
}

#[test]
fn message_builtin_error_includes_builtin_name() {
    // Builtin failures identify themselves via BuiltinMeta::name.
    // Wrong receiver: number has no pop method → TypeError.
    let err = crate::testutil::run_runtime_err("return (42).pop();");
    assert!(err.message.contains("`pop`"), "got: {}", err.message);
}

#[test]
fn bad_object_pointer_is_not_resumable() {
    // A dangling heap pointer is an invariant violation: NotResumable even
    // though the kind is TypeError.
    let err = run_err(vec![
        PushObject(99), // no such object
        ps("k"),
        ObjHas,
    ]);
    assert_eq!(err.kind, ErrorKind::TypeError);
    assert!(
        matches!(err.resume, ResumeMode::NotResumable),
        "bad pointer must not be resumable, got {:?}",
        err.resume
    );
}

#[test]
fn raise_with_two_payloads_is_bad_arg() {
    // Instruction contract: Raise argc is 0 or 1. Hand-assembled argc=2
    // errors instead of silently leaving a stray stack value.
    let mut vm = VM::new(vec![PushPosInt(1), PushPosInt(2), Raise("err".into(), 2)]);
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::BadArg);
    assert!(
        matches!(err.resume, ResumeMode::NotResumable),
        "got {:?}",
        err.resume
    );
}

#[test]
fn cyclic_value_serialization_errors() {
    // A self-referential object must error, not hang, on serialization.
    // (Today the depth guard is what trips; this pins the no-hang contract
    // independently of how cycles are detected.)
    let mut vm = VM::new(vec![]);
    vm.objects.push(ObjData {
        proto: None,
        map: [(RcStr::from("me"), Value::Object(0))]
            .into_iter()
            .collect(),
    });
    let result = vm.stack_value_to_json(&Value::Object(0), 0);
    assert!(
        matches!(result, Err(ref e) if e.kind == ErrorKind::ValueError),
        "expected ValueError for cyclic value, got {result:?}"
    );
}

// ── Step 2: prototype chain ───────────────────────────────────────

#[test]
fn proto_chain_own_hit() {
    // An own property is resolved immediately, without walking the chain.
    let mut vm = VM::new(vec![]);
    let mut map = IndexMap::new();
    map.insert(RcStr::from("x"), Value::PosInt(42));
    vm.objects.push(ObjData { proto: None, map });
    let val = vm.resolve_proto_chain(0, "x").unwrap();
    assert_eq!(val, Value::PosInt(42));
}

#[test]
fn proto_chain_proto_hit() {
    // A property not on own map is found by walking the proto chain.
    let mut vm = VM::new(vec![]);
    // Parent (proto): has "x"
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("x"), Value::PosInt(99));
    let parent_ptr = 0u32;
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    });
    // Child: has no "x", but proto links to parent
    let mut child = IndexMap::new();
    child.insert(RcStr::from("y"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: Some(parent_ptr),
        map: child,
    });
    let val = vm.resolve_proto_chain(1, "x").unwrap();
    assert_eq!(val, Value::PosInt(99));
}

#[test]
fn proto_chain_miss() {
    // A property not found anywhere on the chain returns Undefined.
    let mut vm = VM::new(vec![]);
    vm.objects.push(ObjData {
        proto: None,
        map: IndexMap::new(),
    });
    let val = vm.resolve_proto_chain(0, "nope").unwrap();
    assert_eq!(val, Value::Undefined);
}

#[test]
fn proto_chain_none_short_circuit() {
    // An object with proto: None returns Undefined on a miss with one
    // branch (no loop entry). Sanity: the call doesn't hang.
    let mut vm = VM::new(vec![]);
    vm.objects.push(ObjData {
        proto: None,
        map: IndexMap::new(),
    });
    let val = vm.resolve_proto_chain(0, "missing").unwrap();
    assert_eq!(val, Value::Undefined);
}

#[test]
fn proto_chain_own_shadows_proto() {
    // An own property takes precedence over a same-named proto property.
    let mut vm = VM::new(vec![]);
    // Parent: x = 1
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("x"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    });
    // Child: x = 2, proto = parent
    let mut child = IndexMap::new();
    child.insert(RcStr::from("x"), Value::PosInt(2));
    vm.objects.push(ObjData {
        proto: Some(0),
        map: child,
    });
    let val = vm.resolve_proto_chain(1, "x").unwrap();
    assert_eq!(val, Value::PosInt(2), "own must shadow proto");
}

#[test]
fn proto_chain_self_referential_no_hang() {
    // A self-referential proto chain terminates via the depth cap; the
    // VM must not hang or overflow.
    let mut vm = VM::new(vec![]);
    let mut map = IndexMap::new();
    map.insert(RcStr::from("self"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: Some(0), // points to itself
        map,
    });
    let val = vm.resolve_proto_chain(0, "nope").unwrap();
    assert_eq!(val, Value::Undefined);
}

#[test]
fn obj_has_walks_proto_chain() {
    // The `in` operator (ObjHas instruction) walks the proto chain.
    let mut vm = VM::new(vec![]);
    // Parent: has "a"
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("a"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    });
    // Child: no "a", proto = parent
    vm.objects.push(ObjData {
        proto: Some(0),
        map: IndexMap::new(),
    });
    let val = vm.resolve_proto_chain(1, "a").unwrap();
    assert_eq!(val, Value::PosInt(1), "proto-chain hit via resolve");
}

#[test]
fn obj_has_own_vs_proto() {
    // `in` walks the chain, yielding true for a proto property.
    // Manual instruction test using ObjHas with a proto-linked object.
    let mut vm = VM::new(vec![
        PushObject(1), // child with proto
        ps("a"),
        ObjHas,
    ]);
    // Build heap by hand
    vm.objects.clear();
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("a"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    }); // 0: parent
    vm.objects.push(ObjData {
        proto: Some(0),
        map: IndexMap::new(),
    }); // 1: child
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {
            assert_eq!(
                vm.stack,
                vec![Value::Bool(true)],
                "a in child -> true via proto"
            );
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

#[test]
fn obj_set_only_affects_own() {
    // Assignment touches only the own map; it does not write through
    // to the prototype.
    let mut vm = VM::new(vec![
        PushObject(1), // child
        PushPosInt(99),
        ObjSet(RcStr::from("a"), SetMode::New),
        // Now read child's "a" — should be 99 (own)
        PushObject(1),
        ObjGet(RcStr::from("a")),
        // Next, read parent's "a" — should still be 1 (unchanged)
        PushObject(0),
        ObjGet(RcStr::from("a")),
    ]);
    vm.objects.clear();
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("a"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    }); // 0: parent
    vm.objects.push(ObjData {
        proto: Some(0),
        map: IndexMap::new(),
    }); // 1: child
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {
            assert_eq!(vm.stack.len(), 3);
            assert_eq!(vm.stack[0], Value::PosInt(99), "ObjSet result");
            assert_eq!(vm.stack[1], Value::PosInt(99), "child.a after set");
            assert_eq!(vm.stack[2], Value::PosInt(1), "parent.a unchanged");
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

#[test]
fn obj_delete_only_affects_own() {
    // `delete` only touches own properties, not proto ones.
    let mut vm = VM::new(vec![
        PushObject(1), // child
        ps("a"),
        ObjDelete, // delete child.a (should be false — not own)
        // Then demonstrate: child still inherits "a" from parent
        PushObject(1),
        ps("a"),
        ObjHas,
    ]);
    vm.objects.clear();
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("a"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    }); // 0: parent
    vm.objects.push(ObjData {
        proto: Some(0),
        map: IndexMap::new(),
    }); // 1: child
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {
            assert_eq!(vm.stack.len(), 2);
            assert_eq!(
                vm.stack[0],
                Value::Bool(false),
                "delete non-own yields false"
            );
            assert_eq!(
                vm.stack[1],
                Value::Bool(true),
                "a in child walk-proto -> true"
            );
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

#[test]
fn obj_extend_reads_proto() {
    // Object spread copies *own* properties from the source only; proto
    // properties are not enumerable and are not included.
    let mut vm = VM::new(vec![
        PushObject(0), // empty target
        PushObject(2), // child with proto parent
        ObjExtend,     // → pops src and target, extends, pushes target back
        // Target is now on stack; read its "own_only" property
        ObjGet(RcStr::from("own_only")),
    ]);
    vm.objects.clear();
    // Target
    vm.objects.push(ObjData {
        proto: None,
        map: IndexMap::new(),
    }); // 0
    // Parent
    let mut parent = IndexMap::new();
    parent.insert(RcStr::from("proto_only"), Value::PosInt(1));
    vm.objects.push(ObjData {
        proto: None,
        map: parent,
    }); // 1: parent
    // Child: own "own_only", proto = parent
    let mut child = IndexMap::new();
    child.insert(RcStr::from("own_only"), Value::PosInt(2));
    vm.objects.push(ObjData {
        proto: Some(1),
        map: child,
    }); // 2: child
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {
            assert_eq!(vm.stack, vec![Value::PosInt(2)], "own_only was copied");
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

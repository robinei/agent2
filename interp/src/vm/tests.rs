#[cfg(test)]
mod tests {
    use crate::builtin::Builtin;
    use crate::vm::instr::Instr::*;
    use crate::vm::*;

    // ── harness ──────────────────────────────────────────────────

    /// Run code in a fresh VM (no initial heap) to completion; return final stack.
    fn run(code: Vec<Instr>) -> Vec<Value> {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm.stack.clone(),
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    /// Build a `PushStr` for a string literal — terse sugar for the many tests
    /// that push string operands inline.
    fn ps(val: &str) -> Instr {
        Instr::PushStr(RcStr::from(val))
    }

    /// Run code to the first effect (Invoke/Raise), returning the StepResult.
    fn run_effect(code: Vec<Instr>) -> StepResult {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => panic!("unexpected completion"),
                effect => return effect,
            }
        }
    }

    /// Run code that is expected to error; return the error.
    fn run_err(code: Vec<Instr>) -> VMError {
        let mut vm = VM::new(code);
        loop {
            match vm.step() {
                Err(e) => return e,
                Ok(StepResult::Done) => panic!("unexpected completion"),
                Ok(_) => panic!("unexpected effect"),
            }
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
    fn u(v: u64) -> Value {
        Value::PosInt(v)
    }
    /// A function value pointing at a code address.
    fn f(addr: u32) -> Value {
        Value::Fn(addr)
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
        assert!(matches!(run_err(vec![Pop(1)]), VMError::StackUnderflow));
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
        assert!(matches!(run_err(vec![Dig(1)]), VMError::StackUnderflow));
        assert!(matches!(run_err(vec![Dig(2)]), VMError::StackUnderflow));
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
            run_err(vec![PushFloat(1.0), Pick(1)]),
            VMError::StackUnderflow
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
            run_err(vec![PushFloat(1.0), Dig(1)]),
            VMError::StackUnderflow
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
            run_err(vec![PushFloat(1.0), Nip(1)]),
            VMError::StackUnderflow
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
                Local(0),
            ]),
            vec![n(7.0), n(7.0)]
        );
        // TeeLocal on out-of-range slot errors.
        assert!(matches!(
            run_err(vec![PushFloat(1.0), TeeLocal(0)]),
            VMError::BadLocal
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
                Local(0),
            ]),
            vec![n(4.0), n(4.0)]
        );
        // IncLocal on out-of-range slot errors.
        assert!(matches!(
            run_err(vec![IncLocal(0, -1.0, UpdateMode::Prefix)]),
            VMError::BadLocal
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
        // Not nullish: takes the jump and LEAVES the value (peek, no pop).
        assert_eq!(
            run(vec![PushFloat(5.0), JNotNullish(3), PushFloat(9.0)]),
            vec![n(5.0)]
        );
        // null / undefined: fall through; the value stays for the short-circuit.
        assert_eq!(
            run(vec![PushNull, JNotNullish(3), PushFloat(9.0)]),
            vec![null(), n(9.0)]
        );
        assert_eq!(
            run(vec![PushUndefined, JNotNullish(3), PushFloat(9.0)]),
            vec![undef(), n(9.0)]
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
    fn not_bitnot() {
        assert_eq!(run(vec![PushBool(false), Not]), vec![b(true)]);
        assert_eq!(run(vec![PushNull, Not]), vec![b(true)]);
        assert_eq!(run(vec![PushFloat(1.0), Not]), vec![b(false)]);
        assert_eq!(run(vec![PushFloat(5.0), BitNot]), vec![n(-6.0)]);
        assert!(matches!(
            run_err(vec![PushFloat(3.14), BitNot]),
            VMError::TypeError
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
            run_err(vec![ArrNew(0), ToNum]),
            VMError::TypeError
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
                Local(0),
                Local(1),
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
                Local(0),
                Local(1),
                Sub,
                Return(1),
            ]),
            vec![n(7.0)]
        );
    }

    #[test]
    fn call_dyn_indirect() {
        // Indirect call through a Fn value. Function at [4] computes arg0 - arg1.
        // Layout: push args left-to-right, then the callable on top.
        // [0] Push(10)       arg 0
        // [1] Push(3)        arg 1
        // [2] Push(Fn(5))    callable on top
        // [3] CallDyn(2)
        // [4] Return(1)      main returns the result
        // [5] Local(0)       fn body: args arrive in place as locals
        // [6] Local(1)
        // [7] Sub            10 - 3
        // [8] Return(1)
        assert_eq!(
            run(vec![
                PushFloat(10.0),
                PushFloat(3.0),
                PushFn(5),
                CallDyn(2),
                Return(1),
                Local(0),
                Local(1),
                Sub,
                Return(1),
            ]),
            vec![n(7.0)]
        );
    }

    #[test]
    fn call_dyn_requires_fn() {
        // Top of stack must be a Fn, not some other value.
        let code = vec![PushFloat(1.0), PushFloat(2.0), CallDyn(1)];
        assert!(matches!(run_err(code), VMError::TypeError));
    }

    #[test]
    fn arguments_builds_array_of_frame_args() {
        // Call a fn with 3 args; its body builds `arguments` and returns it.
        // [0..2] push args, [3] callable, [4] CallDyn(3), [5] Return(1)
        // [6] Arguments (fn body), [7] Return(1)
        let mut vm = VM::new(vec![
            PushFloat(10.0),
            PushFloat(20.0),
            PushFloat(30.0),
            PushFn(6),
            CallDyn(3),
            Return(1),
            Arguments,
            Return(1),
        ]);
        while !matches!(vm.step().unwrap(), StepResult::Done) {}
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
            PushFloat(1.0),
            PushFn(4),
            CallDyn(1),
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
        let code = vec![PushFn(999), CallDyn(0)];
        assert!(matches!(run_err(code), VMError::BadCall));
    }

    #[test]
    fn fn_value_equality_and_json() {
        // Same address -> equal; different -> not.
        assert_eq!(run(vec![PushFn(3), PushFn(3), Eq]), vec![b(true)]);
        assert_eq!(run(vec![PushFn(3), PushFn(4), Eq]), vec![b(false)]);
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
        code.push(MakeClosure(0, vec![0].into())); // patched: capture count
        code.push(Return(1));
        let inner = code.len() as u32;
        // 0 params, 1 upval → EnterFrame installs the captured cell at slot 0.
        code.push(EnterFrame(0, false, vec![].into()));
        code.push(Local(0)); // count  (slot 0 = captured upval)
        code.push(PushFloat(1.0));
        code.push(Add);
        code.push(SetLocal(0)); // count = count + 1 (through the shared cell)
        code.push(Local(0));
        code.push(Return(1)); // return count
        code[mk] = MakeClosure(inner, vec![0].into());
        mc
    }

    /// Run to completion and return the finished VM (to inspect heap/cells).
    fn run_vm(code: Vec<Instr>) -> VM {
        let mut vm = VM::new(code);
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm,
                other => panic!("unexpected effect: {other:?}"),
            }
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
        code.push(CallDyn(0)); // first call → 1
        code.push(Dig(1));
        code.push(CallDyn(0)); // second call → 2
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
        code.push(Local(0));
        code.push(CallDyn(0)); // c1() → 1
        code.push(Local(0));
        code.push(CallDyn(0)); // c1() → 2
        code.push(Local(1));
        code.push(CallDyn(0)); // c2() → 1
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
        code.push(CallDyn(0));
        code.push(Return(1));
        // maker
        let maker = code.len() as u32;
        code.push(EnterFrame(0, false, plain(1).into())); // slot 0 = x (NOT boxed)
        code.push(PushFloat(5.0));
        code.push(SetLocal(0));
        let mk = code.len();
        code.push(MakeClosure(0, vec![0].into())); // snapshot x = 5
        code.push(PushFloat(99.0));
        code.push(SetLocal(0)); // x = 99 AFTER capture (must not be seen)
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(EnterFrame(0, false, vec![].into())); // install the by-value upval at slot 0
        code.push(Local(0)); // return captured snapshot
        code.push(Return(1));
        code[call] = Call(maker, 0);
        code[mk] = MakeClosure(inner, vec![0].into());
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
        code.push(PushFloat(42.0)); // setter's arg
        code.push(Local(0));
        code.push(PushFloat(1.0));
        code.push(IndexGet); // setter
        code.push(CallDyn(1)); // setter(42) → (no result)
        code.push(Local(0));
        code.push(PushFloat(0.0));
        code.push(IndexGet); // getter
        code.push(CallDyn(0)); // getter() → 42
        code.push(Return(1));
        // maker
        let maker = code.len() as u32;
        code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into())); // slot 0 = x (by-ref)
        code.push(PushFloat(0.0));
        code.push(SetLocal(0));
        let mk_get = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        let mk_set = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        code.push(ArrNew(2)); // [getter, setter]
        code.push(Return(1));
        let getter = code.len() as u32;
        code.push(EnterFrame(0, false, vec![].into())); // upval x at slot 0
        code.push(Local(0));
        code.push(Return(1));
        let setter = code.len() as u32;
        // 1 param (slot 0) + 1 upval x (slot 1): write the param into x's cell.
        code.push(EnterFrame(1, false, vec![].into()));
        code.push(Local(0)); // the arg
        code.push(SetLocal(1)); // x = arg (through the shared cell)
        code.push(Return(0));
        code[call] = Call(maker, 0);
        code[mk_get] = MakeClosure(getter, vec![0].into());
        code[mk_set] = MakeClosure(setter, vec![0].into());
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
        code.push(CallDyn(0)); // → inner closure
        code.push(CallDyn(0)); // → 7
        code.push(Return(1));
        let outer = code.len() as u32;
        code.push(EnterFrame(0, false, vec![SlotKind::Boxed].into()));
        code.push(PushFloat(7.0));
        code.push(SetLocal(0));
        let mk_mid = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        code.push(Return(1));
        let middle = code.len() as u32;
        // middle's slot 0 is x (installed upval); forward it to inner.
        code.push(EnterFrame(0, false, vec![].into()));
        let mk_in = code.len();
        code.push(MakeClosure(0, vec![0].into()));
        code.push(Return(1));
        let inner = code.len() as u32;
        code.push(EnterFrame(0, false, vec![].into()));
        code.push(Local(0));
        code.push(Return(1));
        code[call] = Call(outer, 0);
        code[mk_mid] = MakeClosure(middle, vec![0].into());
        code[mk_in] = MakeClosure(inner, vec![0].into());
        assert_eq!(run(code), vec![n(7.0)]);
    }

    #[test]
    fn closure_identity_equality() {
        // The same closure object equals itself (reference identity)…
        let same = vec![
            EnterFrame(0, false, vec![SlotKind::Boxed].into()),
            PushFloat(1.0),
            SetLocal(0),
            MakeClosure(6, vec![0].into()),
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
            MakeClosure(7, vec![0].into()),
            MakeClosure(7, vec![0].into()),
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
            MakeClosure(0, vec![5].into()), // only slot 0 exists
            Return(1),
        ];
        assert!(matches!(run_err(code), VMError::BadLocal));
    }

    #[test]
    fn call_dyn_rejects_non_closure_pointer() {
        // A Ptr to a non-closure heap value (here an array) is not callable.
        let code = vec![ArrNew(0), CallDyn(0)];
        assert!(matches!(run_err(code), VMError::TypeError));
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
                Local(0),
                Local(1),
                Add,
                SetLocal(2),
                Local(2),
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
            Local(1), // only local 0 (the arg) exists
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::BadLocal));
    }

    #[test]
    fn call_bad_addr() {
        assert!(matches!(run_err(vec![Call(999, 0)]), VMError::BadCall));
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
                ArrLength
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
        assert!(matches!(run_err(code), VMError::ValueError));
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
            Local(0),
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
                Value::Fn(a) => PushFn(*a),
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
    fn invoke_yields() {
        match run_effect(vec![
            PushFloat(1.0),
            PushFloat(2.0),
            Invoke("my_tool".into(), 2),
        ]) {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "my_tool");
                // push order = arg order: Push(1), Push(2) -> args [1, 2]
                assert_eq!(calls[0].args, vec![n(1.0), n(2.0)]);
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    #[test]
    fn raise_yields() {
        match run_effect(vec![Raise("something_broke".into())]) {
            StepResult::Raise { condition } => {
                assert_eq!(condition, "something_broke");
            }
            other => panic!("expected Raise, got {other:?}"),
        }
    }

    #[test]
    fn resume_after_invoke() {
        let mut vm = VM::new(vec![
            PushFloat(10.0),
            PushFloat(3.0),
            Invoke("add".into(), 2),
            Return(1), // return the result
        ]);
        // First step should yield Invoke
        match vm.step().unwrap() {
            StepResult::Invoke { .. } => {}
            other => panic!("expected Invoke, got {other:?}"),
        }
        // Host pushes result
        vm.stack.push(n(13.0));
        // Resume — should complete with the result on stack
        match vm.step().unwrap() {
            StepResult::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(vm.stack, vec![n(13.0)]);
    }

    #[test]
    fn invoke_batches_consecutive() {
        // Two consecutive Invokes fan out in one step. Left-to-right codegen:
        // evaluate/push all calls' args in order — call 0 (a) deepest, and
        // within a multi-arg call, arg 0 deepest. Here: a(1, 2), b(3).
        let mut vm = VM::new(vec![
            PushFloat(1.0), // a's arg 0
            PushFloat(2.0), // a's arg 1
            PushFloat(3.0), // b's arg 0
            Invoke("a".into(), 2),
            Invoke("b".into(), 1),
        ]);
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "a");
                assert_eq!(calls[0].args, vec![n(1.0), n(2.0)]);
                assert_eq!(calls[1].name, "b");
                assert_eq!(calls[1].args, vec![n(3.0)]);
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
        // Host pushes one result per call, in call order.
        vm.stack.push(n(100.0)); // a's result
        vm.stack.push(n(200.0)); // b's result
        match vm.step().unwrap() {
            StepResult::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(vm.stack, vec![n(100.0), n(200.0)]);
    }

    #[test]
    fn invoke_does_not_batch_across_other_ops() {
        // A non-Invoke instruction between two Invokes breaks the batch.
        let mut vm = VM::new(vec![
            PushFloat(1.0),
            Invoke("a".into(), 1),
            PushFloat(2.0),
            Invoke("b".into(), 1),
        ]);
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "a");
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
        vm.stack.push(n(11.0)); // a's result
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "b");
                assert_eq!(calls[0].args, vec![n(2.0)]);
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    // ── edge cases ────────────────────────────────────────────────

    #[test]
    fn empty_program_done() {
        let mut vm = VM::new(vec![]);
        assert!(matches!(vm.step().unwrap(), StepResult::Done));
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
        // [Jump(0)] loops forever; the fuel budget must break it.
        let mut vm = VM::new(vec![Jump(0)]);
        vm.fuel = 1000;
        assert!(matches!(vm.step().unwrap_err(), VMError::OutOfFuel));
        assert_eq!(vm.fuel, 0);
    }

    #[test]
    fn fuel_is_consumed_per_instruction() {
        let mut vm = VM::new(vec![PushFloat(1.0), PushFloat(2.0), Add]);
        let before = vm.fuel;
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                _ => panic!(),
            }
        }
        assert_eq!(before - vm.fuel, 3); // three instructions executed
    }

    #[test]
    fn dangling_pointer_does_not_panic() {
        // A PushPtr with no backing heap cell must error, not panic.
        assert!(matches!(
            run_err(vec![PushObject(99), ArrLength]),
            VMError::ValueError
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
            Local(0),
            Local(1),
            Add,
            Return(1),
        ];
        assert_eq!(run(code), vec![n(15.0)]);
    }

    #[test]
    fn bit_shift_rejects_bad_count() {
        assert!(matches!(
            run_err(vec![PushFloat(1.0), PushFloat(64.0), BitLhs]),
            VMError::ValueError
        ));
        assert!(matches!(
            run_err(vec![PushFloat(1.0), PushFloat(-1.0), BitRhs]),
            VMError::ValueError
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
        assert!(matches!(run_err(code), VMError::StackUnderflow));
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
        assert!(matches!(run_err(code), VMError::StackUnderflow));
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
            Dig(1), // would swap the temp with the local -> underflow
            Return(0),
        ];
        assert!(matches!(run_err(code), VMError::StackUnderflow));
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
        assert!(matches!(run_err(code), VMError::StackUnderflow));
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
            Dig(1),   // -> [2, 1]
            Pop(1),   // -> [2]
            Local(0), // -> [2, 10]
            Add,      // -> [12]
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
        assert!(matches!(run_err(code), VMError::TypeError));
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
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                other => panic!("unexpected effect: {other:?}"),
            }
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
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
        let per_add = alloc_counter::count();
        eprintln!("  Add (str+str): {per_add}");

        // 4. How many allocs for Math.abs call?
        alloc_counter::reset();
        {
            let mut vm = VM::new(vec![PushFloat(-42.0), CallBuiltin(Builtin::MathAbs, 1)]);
            loop {
                match vm.step().unwrap() {
                    StepResult::Done => break,
                    other => panic!("unexpected effect: {other:?}"),
                }
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
            // Program: push Fn(3), CallDyn(0), Return(0) | PushPosInt(42), Return(1)
            let mut vm = VM::new(vec![
                PushFn(3),
                CallDyn(0),
                Return(0),
                PushPosInt(42),
                Return(1),
            ]);
            loop {
                match vm.step().unwrap() {
                    StepResult::Done => break,
                    other => panic!("unexpected effect: {other:?}"),
                }
            }
        }
        let call_dyn_ret = alloc_counter::count();
        eprintln!("  CallDyn + Return (bare Fn): {call_dyn_ret}");

        // 8. Two CallDyn calls.
        alloc_counter::reset();
        {
            let mut vm = VM::new(vec![
                PushFn(5),
                CallDyn(0),
                PushFn(5),
                CallDyn(0),
                Return(0),
                PushPosInt(42),
                Return(1),
            ]);
            loop {
                match vm.step().unwrap() {
                    StepResult::Done => break,
                    other => panic!("unexpected effect: {other:?}"),
                }
            }
        }
        let call_dyn_x2 = alloc_counter::count();
        eprintln!("  CallDyn x2: {call_dyn_x2}");
        eprintln!(
            "  -> marginal per extra CallDyn: {}",
            call_dyn_x2.saturating_sub(call_dyn_ret)
        );
    }
}

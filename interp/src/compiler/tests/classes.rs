//! Phase 13 Step 7a — plain `class` (constructor + methods + fields, no
//! `extends`). Behavioral parity with the hand-written
//! `function C(){…}; C.prototype.m = …` form, field-init ordering, and the
//! alternative-naming diagnostics for rejected sugar.

use crate::compiler::compile;
use crate::testutil;
use crate::vm::Value;

// ── constructor + methods ────────────────────────────────────────────

#[test]
fn class_constructor_and_method() {
    assert_eq!(
        testutil::run_val(
            "class Point {
                 constructor(x, y) { this.x = x; this.y = y; }
                 sum() { return this.x + this.y; }
             }
             return new Point(3, 4).sum();",
        ),
        testutil::num(7.0)
    );
}

/// A `class` behaves identically to its hand-written function+prototype form.
#[test]
fn class_matches_handwritten_form() {
    let class_src = "class C {
                         constructor(n) { this.n = n; }
                         doubled() { return this.n * 2; }
                     }
                     return new C(21).doubled();";
    let hand_src = "function C(n) { this.n = n; }
                    C.prototype.doubled = function() { return this.n * 2; };
                    return new C(21).doubled();";
    assert_eq!(testutil::run_val(class_src), testutil::num(42.0));
    assert_eq!(testutil::run_val(hand_src), testutil::num(42.0));
}

// ── fields ───────────────────────────────────────────────────────────

#[test]
fn class_fields_initialize_on_construction() {
    assert_eq!(
        testutil::run_val(
            "class C { x = 10; y = 20; }
             const c = new C();
             return c.x + c.y;",
        ),
        testutil::num(30.0)
    );
}

/// Fields initialize before the constructor body runs, in declaration order.
#[test]
fn class_fields_precede_constructor_body() {
    assert_eq!(
        testutil::run_val(
            "class C {
                 x = 1;
                 constructor() { this.x = this.x + 100; }
             }
             return new C().x;",
        ),
        testutil::num(101.0)
    );
}

/// A field with no initializer is `undefined`.
#[test]
fn class_field_without_initializer_is_undefined() {
    assert_eq!(
        testutil::run_val("class C { x; } return new C().x;"),
        Value::Undefined
    );
}

/// A default constructor (no explicit `constructor`) still runs field inits.
#[test]
fn class_default_constructor_runs_fields() {
    assert_eq!(
        testutil::run_val("class C { v = 5; } return new C().v;"),
        Value::PosInt(5)
    );
}

/// A field initializer can reference an earlier-evaluated field via `this`.
#[test]
fn class_field_reads_earlier_field() {
    assert_eq!(
        testutil::run_val(
            "class C { a = 2; b = this.a + 3; }
             return new C().b;",
        ),
        testutil::num(5.0)
    );
}

// ── `this` and method dispatch ───────────────────────────────────────

#[test]
fn class_method_binds_this() {
    assert_eq!(
        testutil::run_val(
            "class Counter {
                 constructor() { this.count = 0; }
                 inc() { this.count = this.count + 1; return this.count; }
             }
             const c = new Counter();
             c.inc();
             c.inc();
             return c.inc();",
        ),
        testutil::num(3.0)
    );
}

/// A method read off the instance and called bare detaches `this`.
#[test]
fn class_detached_method_loses_this() {
    let err = testutil::run_runtime_err(
        "class C { constructor() { this.x = 1; } get() { return this.x; } }
         const c = new C();
         const f = c.get;
         return f();",
    );
    // `this` is undefined when detached → reading `.x` on undefined errors.
    assert!(
        err.message.contains("type error") || err.message.contains("undefined"),
        "expected a this-detachment error, got: {}",
        err.message
    );
}

/// `new C() instanceof C` (Step 8 already landed).
#[test]
fn class_instance_is_instanceof_class() {
    assert_eq!(
        testutil::run_val("class C {} return new C() instanceof C;"),
        Value::Bool(true)
    );
}

/// A method can capture a closure variable from the enclosing scope.
#[test]
fn class_method_captures_outer() {
    assert_eq!(
        testutil::run_val(
            "const base = 100;
             class C { add(n) { return base + n; } }
             return new C().add(5);",
        ),
        testutil::num(105.0)
    );
}

/// A field initializer arrow captures the constructor's lexical `this`.
#[test]
fn class_field_arrow_captures_this() {
    assert_eq!(
        testutil::run_val(
            "class C {
                 constructor() { this.x = 9; }
                 getter = () => this.x;
             }
             const c = new C();
             const g = c.getter;
             return g();",
        ),
        Value::PosInt(9)
    );
}

// ── class expression ─────────────────────────────────────────────────

#[test]
fn class_expression_binds_to_const() {
    assert_eq!(
        testutil::run_val(
            "const C = class {
                 constructor(v) { this.v = v; }
                 get() { return this.v; }
             };
             return new C(8).get();",
        ),
        Value::PosInt(8)
    );
}

// ── rejected sugar (alternative-naming diagnostics) ──────────────────

// ── `extends` / `super` (Step 7b) ────────────────────────────────────

/// A derived class inherits the parent's prototype methods, and `super(x)`
/// threads the instance as `this` into the parent constructor.
#[test]
fn class_extends_inherits_and_super_threads_this() {
    assert_eq!(
        testutil::run_val(
            "class P { constructor(x) { this.x = x; } get() { return this.x; } }
             class C extends P { constructor(x) { super(x); } }
             return new C(7).get();",
        ),
        Value::PosInt(7)
    );
}

/// `super.m()` calls the parent's `m` even when `C` overrides it.
#[test]
fn class_super_method_skips_override() {
    assert_eq!(
        testutil::run_ret(
            "class P { constructor() {} label() { return \"P\"; } }
             class C extends P {
                 constructor() { super(); }
                 label() { return \"C-\" + super.label(); }
             }
             return new C().label();",
        ),
        serde_json::json!("C-P")
    );
}

/// A derived class's field initializes *after* `super()` returns, so it can
/// observe a value the parent constructor set.
#[test]
fn class_derived_field_initializes_after_super() {
    assert_eq!(
        testutil::run_val(
            "class P { constructor() { this.base = 40; } }
             class C extends P {
                 derived = this.base + 2;
                 constructor() { super(); }
             }
             return new C().derived;",
        ),
        testutil::num(42.0)
    );
}

/// `new C()` is an instance of both `C` and its parent `P` (proto chain link).
#[test]
fn class_derived_instanceof_both() {
    assert_eq!(
        testutil::run_val(
            "class P { constructor() {} }
             class C extends P { constructor() { super(); } }
             const c = new C();
             return (c instanceof C) && (c instanceof P);",
        ),
        Value::Bool(true)
    );
}

/// A method inherited from a *grandparent* resolves through the chain.
#[test]
fn class_extends_grandparent_method() {
    assert_eq!(
        testutil::run_val(
            "class A { constructor() {} hi() { return 1; } }
             class B extends A { constructor() { super(); } }
             class C extends B { constructor() { super(); } }
             return new C().hi();",
        ),
        Value::PosInt(1)
    );
}

/// `super(...args)` forwards spread args to the parent constructor.
#[test]
fn class_super_spread_args() {
    assert_eq!(
        testutil::run_val(
            "class P { constructor(a, b) { this.sum = a + b; } }
             class C extends P {
                 constructor(args) { super(...args); }
             }
             return new C([3, 4]).sum;",
        ),
        testutil::num(7.0)
    );
}

/// A derived class without an explicit constructor is rejected (the MVP does
/// not synthesize a forwarding default constructor).
#[test]
fn class_derived_without_constructor_is_rejected() {
    let errs = compile("class B { constructor() {} } class C extends B {}")
        .expect_err("derived default ctor rejected");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("constructor") && e.message.contains("super")),
        "expected a missing-constructor diagnostic, got: {errs:?}"
    );
}

/// An expression superclass (`extends mixin(B)`) is rejected — `extends`
/// requires a plain identifier in the MVP.
#[test]
fn class_extends_expression_is_rejected() {
    let errs = compile("function mix(b) { return b; } class C extends mix(Object) {}")
        .expect_err("expression superclass rejected");
    assert!(
        errs.iter().any(|e| e.message.contains("extends")),
        "expected an extends diagnostic, got: {errs:?}"
    );
}

/// A derived class behaves identically to its hand-written prototype-link form.
#[test]
fn class_extends_matches_handwritten_form() {
    let class_src = "class P { constructor(n) { this.n = n; } val() { return this.n; } }
                     class C extends P { constructor(n) { super(n * 2); } }
                     return new C(10).val();";
    let hand_src = "function P(n) { this.n = n; }
                    P.prototype.val = function() { return this.n; };
                    function C(n) { P.call(this, n * 2); }
                    Object.setPrototypeOf(C.prototype, P.prototype);
                    return new C(10).val();";
    assert_eq!(testutil::run_val(class_src), testutil::num(20.0));
    assert_eq!(testutil::run_val(hand_src), testutil::num(20.0));
}

#[test]
fn class_static_member_is_rejected() {
    assert!(compile("class C { static m() {} }").is_err());
    assert!(compile("class C { static x = 1; }").is_err());
}

#[test]
fn class_accessors_are_rejected() {
    assert!(compile("class C { get x() { return 1; } }").is_err());
    assert!(compile("class C { set x(v) {} }").is_err());
}

#[test]
fn class_computed_and_private_members_are_rejected() {
    assert!(compile("class C { [\"m\"]() {} }").is_err());
    assert!(compile("class C { #x = 1; }").is_err());
}

/// Rejected sugar produces a diagnostic, not a panic, carrying a source span.
#[test]
fn class_rejection_has_span() {
    let errs = compile("class C { static m() {} }").expect_err("should reject");
    let rendered = errs[0].render("class C { static m() {} }");
    assert!(rendered.starts_with("1:"), "got: {rendered}");
}

//! Higher-order array method tests — prelude lowering + runtime behavior.
//! Exercises `map`, `filter`, `reduce`, `forEach`, `find`, `some`, `every`.

use crate::compiler::compile;
use crate::testutil;
use crate::vm::Value;

// ── Phase 4.0: higher-order array methods (prelude) ─────────────────

#[test]
fn hof_map_filter() {
    // map applies the callback to each element.
    assert_eq!(
        testutil::run_ret("return [1, 2, 3].map(x => x * 2);"),
        serde_json::json!([2, 4, 6])
    );
    // map result summed back via reduce.
    assert_eq!(
        testutil::run_val("let a = [1, 2, 3].map(x => x * 2); return a[0] + a[1] + a[2];"),
        testutil::num(12.0)
    );
    // filter keeps matching elements.
    assert_eq!(
        testutil::run_val("let a = [1, 2, 3, 4].filter(x => x % 2 === 0); return a.length;"),
        testutil::num(2.0)
    );
}

#[test]
fn hof_reduce_both_forms() {
    // reduce with an initial value.
    assert_eq!(
        testutil::run_val("return [1, 2, 3, 4].reduce((s, x) => s + x, 0);"),
        testutil::num(10.0)
    );
    // reduce without an initial value (seeds from element 0).
    assert_eq!(
        testutil::run_val("return [1, 2, 3, 4].reduce((s, x) => s + x);"),
        testutil::num(10.0)
    );
}

#[test]
fn hof_search_methods() {
    assert_eq!(
        testutil::run_val("return [1, 2, 3].some(x => x === 2);"),
        Value::Bool(true)
    );
    assert_eq!(
        testutil::run_val("return [1, 2, 3].every(x => x > 0);"),
        Value::Bool(true)
    );
    assert_eq!(
        testutil::run_val("return [1, 2, 3].every(x => x > 1);"),
        Value::Bool(false)
    );
    assert_eq!(
        testutil::run_val("return [5, 6, 7].find(x => x > 5);"),
        Value::PosInt(6)
    );
    assert_eq!(
        testutil::run_val("return [5, 6, 7].findIndex(x => x === 7);"),
        testutil::num(2.0)
    );
    assert_eq!(
        testutil::run_val("return [1, 2].find(x => x > 9);"),
        Value::Undefined
    );
    assert_eq!(
        testutil::run_val("return [1, 2].findIndex(x => x > 9);"),
        Value::NegInt(-1)
    );
}

#[test]
fn hof_foreach_side_effects() {
    assert_eq!(
        testutil::run_ret("let sum = 0; [1, 2, 3].forEach(x => { sum += x; }); return sum;"),
        serde_json::json!(6)
    );
}

#[test]
fn hof_callback_index_and_array_args() {
    assert_eq!(
        testutil::run_val("return [10, 20, 30].map((x, i) => x + i).reduce((s, x) => s + x, 0);"),
        testutil::num(63.0) // (10+0)+(20+1)+(30+2) = 63
    );
}

#[test]
fn hof_closure_callback_captures() {
    assert_eq!(
        testutil::run_val(
            "let k = 10; return [1, 2, 3].map(x => x + k).reduce((s, x) => s + x, 0);"
        ),
        testutil::num(36.0) // (1+10)+(2+10)+(3+10) = 36
    );
}

#[test]
fn hof_chained_and_nested() {
    assert_eq!(
        testutil::run_val(
            "return [1, 2, 3, 4, 5].filter(x => x % 2 === 1).map(x => x * x).reduce((s, x) => s + x, 0);"
        ),
        testutil::num(35.0) // 1 + 9 + 25
    );
}

#[test]
fn hof_inside_user_function() {
    assert_eq!(
        testutil::run_ret(
            "function total(a) { return a.map(x => x + 1).reduce((s, x) => s + x, 0); } return total([1, 2, 3]);"
        ),
        serde_json::json!(9) // 2 + 3 + 4
    );
}

#[test]
fn hof_arity_errors() {
    assert!(compile("[1].map();").is_err()); // needs a callback
    assert!(compile("[1].reduce();").is_err()); // needs 1 or 2 args
}

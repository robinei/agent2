use crate::builtin::Args;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};

// ── JSON static implementations ──────────────────────────────────────────────

/// `JSON.parse(s)` → any.
pub fn json_parse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    // **serde already said what was wrong; this used to throw it away.**
    // `in `parse`: value error` names neither the reason nor the place,
    // and both are in hand: serde's message carries "expected value at
    // line 1 column 1" or "trailing characters", which is the whole
    // diagnosis. Seen live on the 2026-09-20 suite.
    //
    // The head of the input goes with it, because the commonest cause
    // is parsing something that was never JSON — a command's output, a
    // file, an object that was already a value — and one look at the
    // first characters settles which.
    let json: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
        let head: String = s.chars().take(60).collect();
        let more = if s.chars().nth(60).is_some() { "…" } else { "" };
        vm.fail(
            ErrorKind::ValueError,
            &format!("JSON.parse: {e} — the text begins {head:?}{more}"),
        )
    })?;
    vm.json_to_stack_value(&json, 0)
}

/// `JSON.stringify(value[, replacer[, space]])` → str, or the value
/// `undefined` when `value` itself has no JSON form (JS: `SerializeJSONProperty`
/// on the root returns `undefined` in that case, and `JSON.stringify` passes
/// that straight through — it does not throw). `undefined` nested in an
/// array (→ `null`) or an object (key omitted) is already handled inside
/// [`VM::stack_value_to_json`]; this is only the root, which previously fell
/// through to that same function and failed with a bare "value error" —
/// hit by the 2026-09-17 `sweep-200` eval on `JSON.stringify(outline.items,
/// null, 2)` where `items` was `undefined`. Matching JS here (rather than
/// just improving the error text) is deliberate: this dialect's contract is
/// to diverge from JS in exactly three named, documented ways and otherwise
/// behave like it — an `undefined` root was never one of the three, so the
/// throw was a gap, not a divergence worth keeping. Other root values with
/// no JSON form (a function, a promise, a `RegExp`, …) still throw via
/// `stack_value_to_json` below, unchanged.
pub fn json_stringify(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    if matches!(args.get(vm, 0), Value::Undefined) {
        return Ok(Value::Undefined);
    }
    let json = vm.stack_value_to_json(args.get(vm, 0), 0)?;
    // Check replacer: only null/undefined are accepted.
    if args.argc >= 2 {
        match args.get(vm, 1) {
            Value::Null | Value::Undefined => {}
            _ => {
                return Err(vm.fail(ErrorKind::TypeError, "replacer is not supported"));
            }
        }
    }
    // Determine indent string.
    let indent = if args.argc >= 3 {
        let space = args.get(vm, 2);
        match space {
            Value::Undefined | Value::Null => String::new(),
            Value::PosInt(n) => " ".repeat((*n).min(10) as usize),
            Value::NegInt(_) => String::new(),
            Value::Float(n) => {
                let n = n.trunc().clamp(0.0, 10.0) as usize;
                " ".repeat(n)
            }
            Value::String(s) => s.chars().take(10).collect(),
            _ => {
                return Err(vm.fail(ErrorKind::TypeError, "type error"));
            }
        }
    } else {
        String::new()
    };
    if indent.is_empty() {
        let s = serde_json::to_string(&json)
            .map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
        Ok(Value::String(RcStr::from(s)))
    } else {
        let s = pretty_print_json(&json, &indent);
        Ok(Value::String(RcStr::from(s)))
    }
}

/// Simple JSON pretty-printer with custom indent.
fn pretty_print_json(value: &serde_json::Value, indent: &str) -> String {
    let mut out = String::new();
    pretty_print_value(value, indent, 0, &mut out);
    out
}

fn pretty_print_value(value: &serde_json::Value, indent: &str, depth: usize, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let pad = indent.repeat(depth + 1);
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad);
                out.push('"');
                out.push_str(k);
                out.push_str("\": ");
                pretty_print_value(v, indent, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&indent.repeat(depth));
            out.push('}');
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            let pad = indent.repeat(depth + 1);
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad);
                pretty_print_value(v, indent, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&indent.repeat(depth));
            out.push(']');
        }
        _ => out.push_str(&value.to_string()),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// **serde knew; the message did not pass it on.** A live run on
    /// the 2026-09-20 suite was told `in `parse`: value error`, which
    /// names neither the reason nor the place, while both were in the
    /// error being discarded. The commonest cause is parsing something
    /// that was never JSON, so the head of the input goes too.
    #[test]
    fn json_parse_says_why_and_shows_what_it_was_given() {
        let err = crate::testutil::run_runtime_err("JSON.parse('not json at all');");
        assert_eq!(err.kind, crate::ErrorKind::ValueError);
        assert!(err.message.contains("line 1"), "the place: {}", err.message);
        assert!(
            err.message.contains("not json at all"),
            "and what it was handed: {}",
            err.message
        );
        // A long input is clipped rather than quoted whole.
        let err = crate::testutil::run_runtime_err(
            "JSON.parse('x'.repeat(5000));",
        );
        assert!(err.message.len() < 220, "{} bytes", err.message.len());
        assert!(err.message.contains('…'), "{}", err.message);
    }

    use crate::{
        Value,
        builtin::Builtin,
        testutil::{self, run_instrs},
        vm::{ErrorKind, Instr},
    };

    // ── JSON.parse / JSON.stringify ────────────────────────────────────

    #[test]
    fn call_builtin_json_parse() {
        let out = run_instrs(vec![
            Instr::PushStr("42".into()),
            Instr::CallBuiltin(Builtin::JSONParse, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_json_stringify() {
        let out = run_instrs(vec![
            Instr::PushFloat(3.5),
            Instr::CallBuiltin(Builtin::JSONStringify, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "3.5"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── Step 4i: JSON.stringify with space ────────────────────────────

    #[test]
    fn json_stringify_with_space() {
        // Two-space indent matches node's pretty output.
        let v = testutil::run_val("return JSON.stringify({a:1}, null, 2);");
        match v {
            Value::String(s) => {
                assert_eq!(s.as_str(), "{\n  \"a\": 1\n}");
            }
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn json_stringify_compact_no_space() {
        // No space → compact, unchanged from today.
        let v = testutil::run_val("return JSON.stringify({a:1});");
        match v {
            Value::String(s) => assert_eq!(s.as_str(), "{\"a\":1}"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn json_stringify_replacer_type_error() {
        // Non-null/undefined replacer → TypeError.
        use crate::testutil::run_err_kind;
        assert_eq!(
            run_err_kind("return JSON.stringify({a:1}, x => x, 2);"),
            ErrorKind::TypeError
        );
    }

    // ── JSON.stringify(undefined) — sweep-200 2026-09-17: threw ────────

    #[test]
    fn json_stringify_root_undefined_returns_undefined_value() {
        // Real JS: `JSON.stringify(undefined)` is the *value* `undefined`,
        // not a string and not a throw. This dialect used to throw a bare
        // "value error" here (`outline.items` being `undefined` in
        // `JSON.stringify(outline.items, null, 2)` was the eval hit).
        let v = testutil::run_val("return JSON.stringify(undefined);");
        assert_eq!(v, Value::Undefined);
    }

    #[test]
    fn json_stringify_root_undefined_with_space_still_returns_undefined() {
        // The `space` argument only shapes a produced string; there is
        // none here, so it must not change the outcome.
        let v = testutil::run_val("return JSON.stringify(undefined, null, 2);");
        assert_eq!(v, Value::Undefined);
    }

    #[test]
    fn json_stringify_undefined_in_array_is_null() {
        // Already correct going in (stack_value_to_json's array arm), but
        // pinned here alongside the root-undefined fix so the three cases
        // the eval's outline.items call cares about are covered together.
        let v = testutil::run_val("return JSON.stringify([1, undefined, 3]);");
        match v {
            Value::String(s) => assert_eq!(s.as_str(), "[1,null,3]"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn json_stringify_undefined_in_object_omits_key() {
        // Same: already correct (stack_value_to_json's object arm), pinned
        // here for the same reason.
        let v = testutil::run_val("return JSON.stringify({a: 1, b: undefined, c: 3});");
        match v {
            Value::String(s) => assert_eq!(s.as_str(), "{\"a\":1,\"c\":3}"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn json_stringify_non_serializable_root_still_throws() {
        // A function still has no JSON form at the root — only `undefined`
        // gets the JS "return undefined, don't throw" treatment here.
        use crate::testutil::run_err_kind;
        assert_eq!(
            run_err_kind("return JSON.stringify(function() {});"),
            ErrorKind::ValueError
        );
    }
}

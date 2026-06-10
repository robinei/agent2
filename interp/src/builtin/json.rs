use crate::builtin::Args;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};

// ── JSON static implementations ──────────────────────────────────────────────

/// `JSON.parse(s)` → any.
pub fn json_parse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let json: serde_json::Value =
        serde_json::from_str(&s).map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
    vm.json_to_stack_value(&json, 0)
}

/// `JSON.stringify(value[, replacer[, space]])` → str.
pub fn json_stringify(vm: &mut VM, args: Args) -> Result<Value, VMError> {
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
                let n = n.trunc().max(0.0).min(10.0) as usize;
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
}

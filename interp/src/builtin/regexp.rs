use crate::builtin::Args;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};
use indexmap::IndexMap;

/// Build the result object for `exec()` / non-global `match()`:
/// `{ "0": full, "1": cap1, ..., "index": start, "input": input }`.
pub(crate) fn build_exec_result(
    m: &regress::Match,
    input: RcStr,
) -> IndexMap<RcStr, Value> {
    let text = input.as_str();
    let n_captures = m.captures.len();
    let mut obj = IndexMap::with_capacity(4 + n_captures);
    obj.insert(RcStr::from("0"), Value::String(RcStr::from(&text[m.range.clone()])));
    for (i, cap) in m.captures.iter().enumerate() {
        let key = RcStr::from((i + 1).to_string());
        let val = match cap {
            Some(range) => Value::String(RcStr::from(&text[range.clone()])),
            None => Value::Undefined,
        };
        obj.insert(key, val);
    }
    obj.insert(RcStr::from("index"), Value::PosInt(m.range.start as u64));
    obj.insert(RcStr::from("input"), Value::String(input));
    obj.insert(RcStr::from("length"), Value::PosInt((1 + n_captures) as u64));
    obj
}

/// `regexp.test(str)` — returns `true` if the pattern matches anywhere in `str`.
pub fn regexp_test(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let rx = regexp_receiver(vm, args.get(vm, 0))?;
    let input = vm.string_from(args.get(vm, 1))?;
    let found = rx.compiled.find(input.as_str()).is_some();
    Ok(Value::Bool(found))
}

/// `regexp.exec(str)` — returns an object `{ "0": full, "1": cap1, ...,
/// index: start, input: str }`, or `null` when there is no match.
/// Indexed access (`result[0]`, `result[1]`) works because `IndexGet`
/// toStrings the key and looks it up in the object.
pub fn regexp_exec(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let rx = regexp_receiver(vm, args.get(vm, 0))?;
    let input = vm.string_from(args.get(vm, 1))?;
    let text = input.as_str();
    let m = match rx.compiled.find(text) {
        Some(m) => m,
        None => return Ok(Value::Null),
    };
    let obj = build_exec_result(&m, input);
    Ok(vm.alloc_object(obj))
}

/// `regexp.toString()` — returns `"/pattern/flags"`.
pub fn regexp_to_string(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let rx = regexp_receiver(vm, args.get(vm, 0))?;
    let mut s = String::from("/");
    s.push_str(rx.pattern.as_str());
    s.push('/');
    if !rx.flags.as_str().is_empty() {
        s.push_str(rx.flags.as_str());
    }
    Ok(Value::String(RcStr::from(s)))
}

/// Extract a RegExp from a value, failing with a TypeError on mismatch.
pub(crate) fn try_reg_exp<'a>(_vm: &VM, val: &'a Value) -> Option<&'a crate::vm::RcRegExp> {
    match val {
        Value::RegExp(rx) => Some(rx),
        _ => None,
    }
}

/// Extract the RegExp receiver from arg 0, failing with a TypeError otherwise.
fn regexp_receiver<'a>(vm: &VM, val: &'a Value) -> Result<&'a crate::vm::RcRegExp, VMError> {
    match val {
        Value::RegExp(rx) => Ok(rx),
        _ => Err(vm.fail(
            ErrorKind::TypeError,
            "RegExp.prototype method called on incompatible receiver",
        )),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self};
    use crate::vm::ErrorKind;

    // ── regexp smoke tests ───────────────────────────────────────────

    #[test]
    fn regexp_test_method_returns_bool() {
        let val = testutil::run_val("let r = /hello/; return r.test('hello world');");
        assert_eq!(val, Value::Bool(true));

        let val = testutil::run_val("let r = /hello/; return r.test('goodbye');");
        assert_eq!(val, Value::Bool(false));
    }

    #[test]
    fn regexp_exec_returns_match() {
        let val =
            testutil::run_val("let r = /(\\d+)/; let m = r.exec('abc 123 def'); return m[0];");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "123"),
            _ => panic!("expected '123', got {val:?}"),
        }
        // Capture group via m[1].
        let val =
            testutil::run_val("let r = /(\\d+)/; let m = r.exec('abc 123 def'); return m[1];");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "123"),
            _ => panic!("expected '123', got {val:?}"),
        }
    }

    #[test]
    fn regexp_exec_result_has_index_and_input() {
        let val = testutil::run_val("let m = /(\\d+)/.exec('abc 123 def'); return m.index;");
        assert_eq!(val, Value::PosInt(4));
        let val = testutil::run_val("let m = /(\\d+)/.exec('abc 123 def'); return m.input;");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "abc 123 def"),
            _ => panic!("expected 'abc 123 def', got {val:?}"),
        }
    }

    #[test]
    fn regexp_exec_returns_null_on_no_match() {
        let val = testutil::run_val("let r = /xyz/; return r.exec('abc');");
        assert_eq!(val, Value::Null);
    }

    #[test]
    fn regexp_source_and_flags_properties() {
        let val = testutil::run_val("let r = /hello/gi; return r.source;");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "hello"),
            _ => panic!("expected 'hello', got {val:?}"),
        }

        let val = testutil::run_val("let r = /hello/gi; return r.flags;");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "gi"),
            _ => panic!("expected 'gi', got {val:?}"),
        }
    }

    #[test]
    fn regexp_global_ignorecase_properties() {
        let val = testutil::run_val("return /test/g.global;");
        assert_eq!(val, Value::Bool(true));
        let val = testutil::run_val("return /test/.global;");
        assert_eq!(val, Value::Bool(false));
        let val = testutil::run_val("return /test/i.ignoreCase;");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn new_regexp_constructor_works() {
        let val = testutil::run_val("let r = new RegExp('hello', 'i'); return r.test('HELLO');");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn typeof_regexp_is_object() {
        let val = testutil::run_val("return typeof /test/;");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "object"),
            _ => panic!("expected 'object', got {val:?}"),
        }
    }

    #[test]
    fn regexp_negated_character_class() {
        let val = testutil::run_val("let r = /[^a]+/; return r.test('123');");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn regexp_to_string_method() {
        let val = testutil::run_val("return /hello/gi.toString();");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "/hello/gi"),
            _ => panic!("expected '/hello/gi', got {val:?}"),
        }
        // No flags case.
        let val = testutil::run_val("return /x/.toString();");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "/x/"),
            _ => panic!("expected '/x/', got {val:?}"),
        }
    }

    #[test]
    fn string_match_with_regexp() {
        // Non-global: returns exec-like object.
        let val = testutil::run_val("let m = 'abc 123 def'.match(/(\\d+)/); return m[0];");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "123"),
            _ => panic!("expected '123', got {val:?}"),
        }
        // Non-global: has index/input.
        let val = testutil::run_val("let m = 'abc 123 def'.match(/(\\d+)/); return m.index;");
        assert_eq!(val, Value::PosInt(4));
        // Global: returns array of full matches.
        let val = testutil::run_val("return JSON.stringify('a1 b2 c3'.match(/\\d/g));");
        match val {
            Value::String(s) => {
                let parsed: serde_json::Value = serde_json::from_str(s.as_str()).unwrap();
                assert_eq!(parsed, serde_json::json!(["1", "2", "3"]));
            }
            _ => panic!("expected JSON array, got {val:?}"),
        }
        // No match.
        let val = testutil::run_val("return 'abc'.match(/xyz/);");
        assert_eq!(val, Value::Null);
        // String pattern.
        let val = testutil::run_val("let m = 'hello'.match('ll'); return m[0];");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "ll"),
            _ => panic!("expected 'll', got {val:?}"),
        }
    }

    #[test]
    fn string_search_with_regexp() {
        let val = testutil::run_val("return 'hello world'.search(/world/);");
        assert_eq!(val, Value::PosInt(6));
        let val = testutil::run_val("return 'hello world'.search(/xyz/);");
        assert_eq!(val, Value::NegInt(-1));
        // String pattern.
        let val = testutil::run_val("return 'hello world'.search('world');");
        assert_eq!(val, Value::PosInt(6));
    }

    #[test]
    fn string_replace_with_regexp() {
        // Non-global: replaces first only.
        let val = testutil::run_val("return 'a a a'.replace(/a/, 'b');");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "b a a"),
            _ => panic!("expected 'b a a', got {val:?}"),
        }
        // Global: replaces all.
        let val = testutil::run_val("return 'a a a'.replace(/a/g, 'b');");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "b b b"),
            _ => panic!("expected 'b b b', got {val:?}"),
        }
        // Case-insensitive.
        let val = testutil::run_val("return 'Hello'.replace(/h/i, 'J');");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "Jello"),
            _ => panic!("expected 'Jello', got {val:?}"),
        }
    }

    #[test]
    fn string_replace_all_with_regexp() {
        let val = testutil::run_val("return 'a a a'.replaceAll(/a/g, 'b');");
        match val {
            Value::String(s) => assert_eq!(s.as_str(), "b b b"),
            _ => panic!("expected 'b b b', got {val:?}"),
        }
        // Non-global RegExp should error.
        let err = testutil::run_err_kind("return 'a'.replaceAll(/a/, 'b');");
        assert_eq!(err, ErrorKind::TypeError);
    }

    #[test]
    fn string_split_with_regexp() {
        let val = testutil::run_val("return JSON.stringify('a,b,c'.split(/,/));");
        match val {
            Value::String(s) => {
                let parsed: serde_json::Value = serde_json::from_str(s.as_str()).unwrap();
                assert_eq!(parsed, serde_json::json!(["a", "b", "c"]));
            }
            _ => panic!("expected JSON array, got {val:?}"),
        }
        // With limit.
        let val = testutil::run_val("return JSON.stringify('a,b,c'.split(/,/, 2));");
        match val {
            Value::String(s) => {
                let parsed: serde_json::Value = serde_json::from_str(s.as_str()).unwrap();
                assert_eq!(parsed, serde_json::json!(["a", "b"]));
            }
            _ => panic!("expected JSON array, got {val:?}"),
        }
    }
}

use crate::builtin::Args;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};
use indexmap::IndexMap;

/// Build the result object for `exec()` / non-global `match()`:
/// `{ "0": full, "1": cap1, ..., "index": start, "input": input }`, plus a
/// `groups` object when the pattern has named captures (`(?<name>…)`) —
/// `undefined` otherwise, matching JS. Takes `&mut VM` to allocate the
/// nested `groups` object.
pub(crate) fn build_exec_result(
    vm: &mut VM,
    m: &regress::Match,
    input: RcStr,
) -> IndexMap<RcStr, Value> {
    let text = input.as_str();
    let n_captures = m.captures.len();
    let mut obj = IndexMap::with_capacity(5 + n_captures);
    obj.insert(
        RcStr::from("0"),
        Value::String(RcStr::from(&text[m.range.clone()])),
    );
    for (i, cap) in m.captures.iter().enumerate() {
        let key = RcStr::from((i + 1).to_string());
        let val = match cap {
            Some(range) => Value::String(RcStr::from(&text[range.clone()])),
            None => Value::Undefined,
        };
        obj.insert(key, val);
    }
    // Named groups → `groups` (only when present; else `.groups` is undefined).
    let mut named: IndexMap<RcStr, Value> = IndexMap::new();
    for (name, range) in m.named_groups() {
        let val = match range {
            Some(r) => Value::String(RcStr::from(&text[r])),
            None => Value::Undefined,
        };
        named.insert(RcStr::from(name), val);
    }
    obj.insert(RcStr::from("index"), Value::PosInt(m.range.start as u64));
    obj.insert(RcStr::from("input"), Value::String(input));
    obj.insert(
        RcStr::from("length"),
        Value::PosInt((1 + n_captures) as u64),
    );
    if !named.is_empty() {
        let groups = vm.alloc_object(named);
        obj.insert(RcStr::from("groups"), groups);
    }
    obj
}

/// Find the next match honoring `/g` statefulness: a global regex resumes
/// at its `lastIndex`, advances it past the match, and resets it to `0` on
/// no match (so the standard `while ((m = re.exec(s)) !== null)` loop
/// terminates). A zero-width match steps forward one char to guarantee
/// progress. A non-global regex always scans from `0` and never touches
/// `lastIndex`.
fn next_match(rx: &crate::vm::RcRegExp, text: &str) -> Option<regress::Match> {
    let global = rx.flags.contains('g');
    let sticky = rx.flags.contains('y');
    // Both `/g` and `/y` resume from `lastIndex`; `/y` additionally requires
    // the match to begin *exactly* there (anchored), not merely after it.
    let stateful = global || sticky;
    let start = if stateful { rx.last_index.get() } else { 0 };
    if start > text.len() {
        if stateful {
            rx.last_index.set(0);
        }
        return None;
    }
    match rx.compiled.find_from(text, start).next() {
        Some(m) if !sticky || m.range.start == start => {
            if stateful {
                let end = if m.range.end == m.range.start {
                    // zero-width: advance one full char so we don't re-match
                    m.range.end + text[m.range.end..].chars().next().map_or(1, char::len_utf8)
                } else {
                    m.range.end
                };
                rx.last_index.set(end);
            }
            Some(m)
        }
        // No match, or a sticky regex matched past `lastIndex` (not anchored).
        _ => {
            if stateful {
                rx.last_index.set(0);
            }
            None
        }
    }
}

/// `regexp.test(str)` — returns `true` if the pattern matches. A `/g`
/// regex tests from `lastIndex` and advances it, like `exec`.
pub fn regexp_test(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let rx = regexp_receiver(vm, args.get(vm, 0))?.clone();
    let input = vm.string_from(args.get(vm, 1))?;
    Ok(Value::Bool(next_match(&rx, input.as_str()).is_some()))
}

/// `regexp.exec(str)` — returns an object `{ "0": full, "1": cap1, ...,
/// index: start, input: str }`, or `null` when there is no match.
/// Indexed access (`result[0]`, `result[1]`) works because `IndexGet`
/// toStrings the key and looks it up in the object. For a `/g` regex the
/// search resumes at `lastIndex` and advances it (`next_match`).
pub fn regexp_exec(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let rx = regexp_receiver(vm, args.get(vm, 0))?.clone();
    let input = vm.string_from(args.get(vm, 1))?;
    match next_match(&rx, input.as_str()) {
        Some(m) => {
            let obj = build_exec_result(vm, &m, input);
            Ok(vm.alloc_object(obj))
        }
        None => Ok(Value::Null),
    }
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

    // ── stateful /g exec / lastIndex ─────────────────────────────────

    #[test]
    fn global_exec_loop_terminates_with_captures() {
        // The canonical "iterate all matches" idiom — must terminate and
        // yield each capture group, not spin forever (the bug this fixes).
        // The exact script-src scenario from the field report.
        let out = testutil::run_ret(
            r#"const re = /<script[^>]*src="([^"]+)"/g;
               const html = '<script src="a.js"></script><script defer src="b.js">';
               const out = []; let m;
               while ((m = re.exec(html)) !== null) { out.push(m[1]); }
               return out;"#,
        );
        assert_eq!(out, serde_json::json!(["a.js", "b.js"]));
    }

    #[test]
    fn global_exec_advances_and_resets_lastindex() {
        // lastIndex advances per match, then resets to 0 at exhaustion so a
        // second pass starts over.
        let out = testutil::run_ret(
            "const re = /\\d/g; const s = 'a1b2'; \
             const a = []; \
             re.exec(s); a.push(re.lastIndex); \
             re.exec(s); a.push(re.lastIndex); \
             re.exec(s); a.push(re.lastIndex); \
             re.exec(s); a.push(re.lastIndex); return a;",
        );
        // matches at idx1 (lastIndex 2), idx3 (lastIndex 4), then null -> 0,
        // then matches idx1 again (lastIndex 2).
        assert_eq!(out, serde_json::json!([2, 4, 0, 2]));
    }

    #[test]
    fn lastindex_is_writable() {
        let out = testutil::run_ret(
            "const re = /\\d/g; re.lastIndex = 2; const m = re.exec('1x3'); return m[0];",
        );
        assert_eq!(out, serde_json::json!("3"));
    }

    #[test]
    fn non_global_exec_is_stateless() {
        // Without /g, exec always matches from 0 and never advances.
        let out = testutil::run_ret(
            "const re = /\\d/; const s = '1a2'; \
             return [re.exec(s)[0], re.lastIndex, re.exec(s)[0]];",
        );
        assert_eq!(out, serde_json::json!(["1", 0, "1"]));
    }

    #[test]
    fn global_exec_zero_width_match_progresses() {
        // A zero-width global match must step forward, not loop forever.
        let out = testutil::run_ret(
            "const re = /x*/g; const out = []; let m; let n = 0; \
             while ((m = re.exec('axbx')) !== null && n < 20) { out.push(m.index); n++; } \
             return out;",
        );
        // matches: '' @0, 'x' @1, '' @2, 'x' @3, '' @4 → indices distinct, terminates.
        assert_eq!(out, serde_json::json!([0, 1, 2, 3, 4]));
    }

    #[test]
    fn match_all_returns_all_with_captures() {
        let out = testutil::run_ret(
            "const re = /(\\w)(\\d)/g; const ms = 'a1b2'.matchAll(re); \
             return ms.map(m => [m[0], m[1], m[2]]);",
        );
        assert_eq!(out, serde_json::json!([["a1", "a", "1"], ["b2", "b", "2"]]));
    }

    #[test]
    fn match_all_requires_global_flag() {
        assert_eq!(
            testutil::run_err_kind("return 'ab'.matchAll(/a/);"),
            ErrorKind::TypeError
        );
    }

    #[test]
    fn match_length_is_capture_count_not_entry_count() {
        // `.length` on a match object reads its stored `length` property
        // (1 + captures), not the object's entry count — so idiomatic
        // `for (i < m.length)` iteration works.
        assert_eq!(
            testutil::run_ret("return /(\\d)(\\w)/.exec('1a').length;"),
            serde_json::json!(3) // full match + 2 groups
        );
        assert_eq!(
            testutil::run_ret("return /\\d/.exec('a1').length;"),
            serde_json::json!(1) // full match, no groups
        );
    }

    #[test]
    fn length_on_plain_object_is_property_or_undefined() {
        // No entry-count: a plain object's `.length` is the property (or
        // undefined), matching JS.
        assert_eq!(
            testutil::run_ret("return ({ a: 1, b: 2 }).length === undefined;"),
            serde_json::json!(true)
        );
        assert_eq!(
            testutil::run_ret("return ({ length: 5, a: 1 }).length;"),
            serde_json::json!(5)
        );
    }

    #[test]
    fn size_is_distinct_from_length() {
        // `.size` reads map/set entry count or an object's `size` property
        // (undefined if absent) — never conflated with `.length`.
        assert_eq!(
            testutil::run_ret("return new Set([1, 2, 3]).size;"),
            serde_json::json!(3)
        );
        assert_eq!(
            testutil::run_ret("return ({ size: 7, a: 1 }).size;"),
            serde_json::json!(7)
        );
        assert_eq!(
            testutil::run_ret("return ({ a: 1 }).size === undefined;"),
            serde_json::json!(true)
        );
        // Cross-type access is a loud TypeError, not a wrong number.
        assert_eq!(
            testutil::run_err_kind("return [1, 2].size;"),
            ErrorKind::TypeError
        );
        assert_eq!(
            testutil::run_err_kind("return new Map().length;"),
            ErrorKind::TypeError
        );
    }

    #[test]
    fn exec_exposes_named_groups() {
        let out = testutil::run_ret(
            r"const m = /(?<y>\d+)-(?<mo>\d+)/.exec('2026-06'); return [m.groups.y, m.groups.mo];",
        );
        assert_eq!(out, serde_json::json!(["2026", "06"]));
    }

    #[test]
    fn groups_is_absent_without_named_captures() {
        // No named groups → `.groups` is undefined (JS parity).
        let out = testutil::run_ret("const m = /(\\d)/.exec('a1'); return m.groups === undefined;");
        assert_eq!(out, serde_json::json!(true));
    }

    #[test]
    fn sticky_anchors_at_lastindex() {
        // `/y` matches only exactly at lastIndex, advancing it on success.
        let out = testutil::run_ret(
            "const re = /\\d/y; const s = 'a1'; \
             const r0 = re.exec(s); \
             re.lastIndex = 1; const r1 = re.exec(s); \
             return [r0 === null, r1[0], re.lastIndex];",
        );
        // At index 0 'a' isn't a digit → null; anchored at 1 → '1', lastIndex 2.
        assert_eq!(out, serde_json::json!([true, "1", 2]));
    }

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

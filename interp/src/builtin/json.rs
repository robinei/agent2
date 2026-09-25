use crate::builtin::Args;
use crate::vm::{ErrorKind, JsString, VM, VMError, Value};

// ── JSON static implementations ──────────────────────────────────────────────

/// `JSON.parse(s)` → any.
pub fn json_parse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    // **A non-string is nearly always a field that is not there.**
    // `JSON.parse(r.body)` on a result with no `body` said `in
    // \`parse\`: type error`, naming neither the value nor that a
    // string was wanted.
    let arg = args.get(vm, 0).clone();
    if !matches!(arg, Value::String(_)) {
        let what = vm.describe_operand(&arg);
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!("JSON.parse needs a string; got {what}").as_str(),
        ));
    }
    let s = vm.string_from(&arg)?;
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
    let s = s.to_utf8_lossy();
    let json: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
        let head: String = s.chars().take(60).collect();
        let more = if s.chars().nth(60).is_some() {
            "…"
        } else {
            ""
        };
        vm.fail(
            ErrorKind::SyntaxError,
            format!("JSON.parse: {e} — the text begins {head:?}{more}").as_str(),
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
    // Asked first, and then thrown away: the walker below emits the text, but
    // this is what decides that a promise or a closure has no JSON form at
    // all, and the two must not disagree about that.
    vm.stack_value_to_json(args.get(vm, 0), 0)?;
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
            Value::String(s) => s.to_utf8_lossy().chars().take(10).collect(),
            other => {
                let what = vm.describe_operand(&other.clone());
                return Err(vm.fail(
                    ErrorKind::TypeError,
                    format!(
                        "JSON.stringify's third argument is the indent — a number of \
                         spaces, or the string to indent with. Got {what}."
                    )
                    .as_str(),
                ));
            }
        }
    } else {
        String::new()
    };
    let mut out = String::new();
    write_json_value(vm, args.get(vm, 0), 0, &indent, 0, &mut out)?;
    Ok(Value::String(JsString::from(out)))
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

// ── the string boundary JSON cannot cross on its own ─────────────────────────

/// Append `units` to `out` as a JSON string literal, emitting `\udXXX` for an
/// unpaired surrogate.
///
/// **`serde_json` structurally cannot do this.** Its `Value::String` holds a
/// Rust `String`, so an unpaired surrogate has already become U+FFFD before
/// the serializer ever sees it, and it has no way to emit a lone escape
/// anyway. `built-ins/JSON/stringify/value-string-escape-unicode.js` requires
/// `JSON.stringify("\uD834")` to be `"\ud834"` — and requires the *paired*
/// case next to it to come out as the character, which is why this walks code
/// points rather than escaping every surrogate it sees.
///
/// This is the one place in the crate where routing through `serde_json`
/// stopped being free. See `docs/30_STRINGS.md`.
fn write_json_string(units: &[u16], out: &mut String) {
    out.push('"');
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        match u {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            _ if u < 0x20 => out.push_str(&format!("\\u{u:04x}")),
            _ => {
                let (cp, n) = crate::units::code_point_at(units, i).expect("i is in range");
                match char::from_u32(cp) {
                    // A real code point, a paired surrogate included: the
                    // character itself, as serde would have written it.
                    Some(c) => {
                        out.push(c);
                        i += n;
                        continue;
                    }
                    // Only an unpaired surrogate reaches here.
                    None => out.push_str(&format!("\\u{u:04x}")),
                }
            }
        }
        i += 1;
    }
    out.push('"');
}

/// Write `v` as JSON text.
///
/// Strings, arrays and objects are walked here so that every string — a
/// value or a key — goes through [`write_json_string`]. Everything else
/// delegates to `stack_value_to_json`, which owns the rules this must not
/// drift from: what has no JSON form and what that costs, `undefined`
/// dropped in an object and `null`ed in an array, the depth cap, the refusal
/// to serialize a builtin prototype.
///
/// The seam is honest but not free: a `Map` or `Set` nested inside the value
/// is converted by that function, so a lone surrogate inside *one of those*
/// is still U+FFFD. `JSON.stringify` of a `Map` is already a divergence
/// (JS gives `{}`), so this is a corner of a corner, and naming it is
/// cheaper than duplicating the collection rules to cover it.
fn write_json_value(
    vm: &VM,
    v: &Value,
    depth: usize,
    indent: &str,
    level: usize,
    out: &mut String,
) -> Result<(), VMError> {
    match v {
        Value::String(s) => {
            write_json_string(s.as_units(), out);
            Ok(())
        }
        Value::Array(p) => {
            let arr = vm
                .arrays
                .get(*p as usize)
                .ok_or_else(|| vm.fail_invariant(ErrorKind::BadPointer, "bad array pointer"))?
                .clone();
            if arr.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            let (open, sep, close) = brackets("[", "]", indent, level);
            out.push_str(&open);
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    out.push_str(&sep);
                }
                match elem {
                    // JS: `undefined` array slots stringify to `null`.
                    Value::Undefined => out.push_str("null"),
                    _ => write_json_value(vm, elem, depth + 1, indent, level + 1, out)?,
                }
            }
            out.push_str(&close);
            Ok(())
        }
        Value::Object(p) => {
            // `stack_value_to_json` owns the refusals; ask it about this
            // object before walking it, so the two cannot disagree about
            // which objects have a JSON form.
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail_invariant(ErrorKind::BadPointer, "bad object pointer"))?;
            if !matches!(obj.kind, crate::vm::ObjKind::Ordinary) {
                vm.stack_value_to_json(v, depth)?;
            }
            let entries: Vec<(JsString, Value)> = obj
                .map
                .iter()
                // JS: properties whose value is `undefined` are omitted.
                .filter(|(_, val)| !matches!(val, Value::Undefined))
                .map(|(k, val)| (k.clone(), val.clone()))
                .collect();
            if entries.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            let (open, sep, close) = brackets("{", "}", indent, level);
            out.push_str(&open);
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(&sep);
                }
                write_json_string(k.as_units(), out);
                out.push_str(if indent.is_empty() { ":" } else { ": " });
                write_json_value(vm, val, depth + 1, indent, level + 1, out)?;
            }
            out.push_str(&close);
            Ok(())
        }
        other => {
            let j = vm.stack_value_to_json(other, depth)?;
            if indent.is_empty() {
                out.push_str(&j.to_string());
            } else {
                pretty_print_value(&j, indent, level, out);
            }
            Ok(())
        }
    }
}

/// The three pieces of punctuation a container needs, compact or indented.
fn brackets(open: &str, close: &str, indent: &str, level: usize) -> (String, String, String) {
    if indent.is_empty() {
        (open.to_string(), ",".to_string(), close.to_string())
    } else {
        let pad = indent.repeat(level + 1);
        (
            format!("{open}\n{pad}"),
            format!(",\n{pad}"),
            format!("\n{}{close}", indent.repeat(level)),
        )
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod surrogate_tests {
    use crate::testutil;

    /// **The one place routing through `serde_json` stopped being free.**
    ///
    /// `serde_json::Value::String` holds a Rust `String`, so an unpaired
    /// surrogate is U+FFFD before the serializer sees it and there is no way
    /// to ask for a lone `\udXXX` anyway. The paired case next to it has to
    /// keep coming out as the character, which is why the writer walks code
    /// points rather than escaping every surrogate it meets.
    #[test]
    fn a_lone_surrogate_survives_stringify_as_an_escape() {
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("\uD834")"#),
            r#""\ud834""#
        );
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("\uDF06")"#),
            r#""\udf06""#
        );
        // A pair is the character, not two escapes.
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("\uD834\uDF06")"#),
            "\"\u{1D306}\""
        );
        // Mixed: lone, pair, lone — the case that catches a writer which
        // escapes on sight or decodes greedily.
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("\uD834\uD834\uDF06\uD834")"#),
            "\"\\ud834\u{1D306}\\ud834\""
        );
        // Inside a container, and as a key.
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify(["\uD834"])"#),
            r#"["\ud834"]"#
        );
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify({ "\uD834": 1 })"#),
            r#"{"\ud834":1}"#
        );
    }

    /// The ordinary output has to be byte-for-byte what it was, because every
    /// tool result and log line in the harness crosses here.
    #[test]
    fn ordinary_values_are_written_exactly_as_before() {
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("a\"b\\c")"#),
            r#""a\"b\\c""#
        );
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("\n\t\r")"#),
            r#""\n\t\r""#
        );
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify("\u0000")"#),
            r#""\u0000""#
        );
        // Non-ASCII is written as itself, as serde does — not \u-escaped.
        assert_eq!(testutil::eval_str(r#"JSON.stringify("é→😀")"#), "\"é→😀\"");
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify({a:1,b:[1,2],c:null,d:undefined,e:""})"#),
            r#"{"a":1,"b":[1,2],"c":null,"e":""}"#
        );
        assert_eq!(testutil::eval_str("JSON.stringify([])"), "[]");
        assert_eq!(testutil::eval_str("JSON.stringify({})"), "{}");
        assert_eq!(
            testutil::eval_str("JSON.stringify([1,undefined,2])"),
            "[1,null,2]"
        );
        // Indented output, including a nested empty container.
        assert_eq!(
            testutil::eval_str("JSON.stringify({a:[1,{b:2}],c:{},d:[]}, null, 2)"),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": 2\n    }\n  ],\n  \"c\": {},\n  \"d\": []\n}"
        );
        // A round trip through parse still holds for everything JSON can say.
        assert_eq!(
            testutil::eval_str(r#"JSON.stringify(JSON.parse('{"x":[1,"é",true,null]}'))"#),
            r#"{"x":[1,"é",true,null]}"#
        );
    }

    /// **What is still broken, pinned so it is not mistaken for working.**
    /// `serde_json` *rejects* a lone-surrogate escape on parse, so the
    /// round trip is one-way: a program can write `"\ud834"` out and cannot
    /// read it back. Fixing that means our own JSON reader, which is a
    /// separate change with a separate justification — none of the 142
    /// failing `built-ins/JSON` tests turns on it (they are error-type and
    /// `json-parse-with-source` failures).
    #[test]
    fn parse_still_refuses_a_lone_surrogate_escape() {
        let m = testutil::run_ret(
            r#"try { JSON.parse('"\\ud834"'); return "parsed"; } catch (e) { return "threw"; }"#,
        );
        assert_eq!(m, "threw");
        // A *paired* escape reads back fine, which is the common case.
        assert_eq!(
            testutil::eval_str(r#"JSON.parse('"\\ud834\\udf06"')"#),
            "\u{1D306}"
        );
    }
}

#[cfg(test)]
mod indent_argument_tests {
    use crate::testutil;

    /// **The third argument is the indent, and the refusal says so.**
    /// It said "type error", which names neither the argument nor what
    /// it should have been — and this is the argument a reader is most
    /// likely to guess at, because in JavaScript it quietly accepts a
    /// number *or* a string and ignores everything else.
    #[test]
    fn a_bad_indent_names_the_argument_and_the_value() {
        for (src, what) in [
            ("JSON.stringify({a:1}, null, {})", "an object"),
            ("JSON.stringify({a:1}, null, [1])", "an array"),
            ("JSON.stringify({a:1}, null, true)", "a boolean"),
        ] {
            let m = testutil::run_ret(&format!(
                r#"try {{ return {src}; }} catch (e) {{ return e.message; }}"#
            ));
            let m = m.as_str().unwrap_or_default();
            assert!(m.contains("third argument is the indent"), "{src}: {m}");
            assert!(m.contains(what), "{src} names the value: {m}");
        }

        // Both forms JavaScript accepts still work.
        let out = testutil::run_ret(r#"return JSON.stringify({a:1}, null, 2);"#);
        assert!(out.as_str().unwrap().contains("\n  \"a\""), "{out}");
        let out = testutil::run_ret(r#"return JSON.stringify({a:1}, null, "\t");"#);
        assert!(out.as_str().unwrap().contains("\t"), "{out}");
    }
}

#[cfg(test)]
mod tests {
    /// **serde knew; the message did not pass it on.** A live run on
    /// the 2026-09-20 suite was told `in `parse`: value error`, which
    /// names neither the reason nor the place, while both were in the
    /// error being discarded. The commonest cause is parsing something
    /// that was never JSON, so the head of the input goes too.
    /// A non-string is nearly always a field that is not there, and
    /// saying which is the difference between one turn and two.
    #[test]
    fn json_parse_names_a_non_string_argument() {
        let err = crate::testutil::run_runtime_err("JSON.parse(undefined);");
        assert!(err.message.contains("needs a string"), "{}", err.message);
        assert!(err.message.contains("undefined"), "{}", err.message);
    }

    #[test]
    fn json_parse_says_why_and_shows_what_it_was_given() {
        let err = crate::testutil::run_runtime_err("JSON.parse('not json at all');");
        // Not a judgement call: the spec names a `JSON.parse` failure a
        // `SyntaxError`, so a program catching one by the book used to
        // catch nothing.
        assert_eq!(err.kind, crate::ErrorKind::SyntaxError);
        assert!(err.message.contains("line 1"), "the place: {}", err.message);
        assert!(
            err.message.contains("not json at all"),
            "and what it was handed: {}",
            err.message
        );
        // A long input is clipped rather than quoted whole.
        let err = crate::testutil::run_runtime_err("JSON.parse('x'.repeat(5000));");
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
            Value::String(s) => assert!(s.eq_str("3.5")),
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
                assert!(s.eq_str("{\n  \"a\": 1\n}"));
            }
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn json_stringify_compact_no_space() {
        // No space → compact, unchanged from today.
        let v = testutil::run_val("return JSON.stringify({a:1});");
        match v {
            Value::String(s) => assert!(s.eq_str("{\"a\":1}")),
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
            Value::String(s) => assert!(s.eq_str("[1,null,3]")),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn json_stringify_undefined_in_object_omits_key() {
        // Same: already correct (stack_value_to_json's object arm), pinned
        // here for the same reason.
        let v = testutil::run_val("return JSON.stringify({a: 1, b: undefined, c: 3});");
        match v {
            Value::String(s) => assert!(s.eq_str("{\"a\":1,\"c\":3}")),
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

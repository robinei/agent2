//! **One string, one answer to "what is the position here".**
//!
//! Strings in this dialect are UTF-16 code units: `.length` counts units and
//! every index is a unit offset, which is what JS means by both. This file is
//! where that contract is recorded, and it has been rewritten once — it used
//! to record the opposite.
//!
//! # What it recorded before (2026-09-24, morning)
//!
//! Strings were UTF-8 byte sequences, and the *spellings* of "read a
//! character" disagreed with each other:
//!
//! ```text
//!                          "a—b"                     "—b"
//!   s.length                 5  (bytes)
//!   [...s].length            3  (characters)
//!   s.split("").length       3  (characters)
//!   for (const ch of s)      TRAPS at byte 2
//!   s[0]                                              "—"
//!   s.charAt(0)                                       "â"   ← not in the string
//!   s.at(0)                                           "â"   ← not in the string
//! ```
//!
//! Found by reading a live session, not by a test: a model brace-matching a
//! Rust function body in `report.rs` was cut off mid-character. Those three
//! were fixed and this file was written to hold them together, with a heading
//! that said *"Nothing here changes the byte model"*.
//!
//! # Why that was not enough (2026-09-24, afternoon)
//!
//! Seven more disagreements turned up the same day by asking seven questions,
//! none of them loud, none of them caught by a test: `"aéb".slice(0, 2)` was
//! `"a"`, `"a".padStart(3, "💩")` had `.length` 9, `"😀".padStart(4, "-")`
//! padded nothing, `String.fromCharCode(0xD83D, 0xDE00)` was `""`,
//! `"\uD800".length` was 7, `"a😀b".indexOf("b")` was 5, and `charCodeAt` did
//! not exist at all.
//!
//! **They were not ten bugs. They were one bug ten times**: every operation
//! decided locally what a position was, nothing checked the answer, and the
//! unit is invisible in a `usize`. Making the byte offsets consistent was
//! possible; keeping them consistent was not, because the surrounding
//! language — every JS a maintainer or a model has ever read — uses a
//! different unit. See `docs/30_STRINGS.md`.
//!
//! So the model moved. `.length` counts code units, `s[i]` reads one code
//! unit and can no longer refuse, and the readings still agree.

fn val(src: &str) -> serde_json::Value {
    let program = interp::compile(src).unwrap_or_else(|e| {
        panic!(
            "{src}\n{:?}",
            e.iter().map(|d| &d.message).collect::<Vec<_>>()
        )
    });
    let mut vm = interp::VM::for_program(program, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        interp::StepResult::Done { value, .. } => {
            vm.stack_value_to_json(&value, 0).expect("value to JSON")
        }
        other => panic!("{src} did not finish: {other:?}"),
    }
}

/// The idiom a program reaches for, on text that is not pure ASCII.
#[test]
fn for_of_walks_characters_not_bytes() {
    assert_eq!(
        val(r#"let n = 0; for (const ch of "a—b") n++; return n;"#),
        3
    );
    assert_eq!(
        val(r#"let o = ""; for (const ch of "a—b") o += "|" + ch; return o;"#),
        "|a|—|b"
    );
    // The brace-matching scan that started this: it must reach the end.
    assert_eq!(
        val(r#"let d = 0, seen = 0;
               for (const ch of "fn f() { /* — */ { } }") {
                 if (ch === "{") { d++; seen++; }
                 if (ch === "}") d--;
               }
               return [d, seen];"#),
        serde_json::json!([0, 2])
    );
    // An empty string is no iterations, not one.
    assert_eq!(val(r#"let n = 0; for (const ch of "") n++; return n;"#), 0);
}

/// `for…of` and the spread walk **code points**; `split("")` walks **code
/// units**. That is not a disagreement, it is what JS specifies, and the
/// difference only shows above the BMP.
#[test]
fn the_three_ways_to_split_a_string_agree_where_js_says_they_do() {
    // No astral characters: all three agree, as they always did.
    assert_eq!(
        val(r#"const s = "a—bc";
               let f = []; for (const ch of s) f.push(ch);
               return [f.length, [...s].length, s.split("").length];"#),
        serde_json::json!([4, 4, 4])
    );
    // With one: `split("")` cuts the surrogate pair in half, because the
    // empty separator splits between units. V8 answers 5/5/6 here too.
    assert_eq!(
        val(r#"const s = "a—b😀c";
               let f = []; for (const ch of s) f.push(ch);
               return [f.length, [...s].length, s.split("").length];"#),
        serde_json::json!([5, 5, 6])
    );
    // All three still reassemble the original.
    assert_eq!(
        val(
            r#"const s = "a—b😀c"; let f = []; for (const ch of s) f.push(ch);
               return f.join("") === s && [...s].join("") === s
                      && s.split("").join("") === s;"#
        ),
        true
    );
}

/// **`.length` and every index count UTF-16 code units**, which is the answer
/// JS gives. Each line here was a different number before.
#[test]
fn length_and_indexing_count_code_units() {
    assert_eq!(val(r#"return "a—b".length;"#), 3); // was 5
    assert_eq!(val(r#"return "aéb".length;"#), 3); // was 4
    assert_eq!(val(r#"return "😀".length;"#), 2); // was 4
    assert_eq!(val(r#"return "—b"[0];"#), "—");
    assert_eq!(val(r#"return "—b"[1];"#), "b"); // was index 3
    // **There is no position left to refuse.** `"a—b"[2]` raised
    // `cannot index string at byte offset 2` with a paragraph of advice; it
    // is the third character now.
    assert_eq!(val(r#"return "a—b"[2];"#), "b");
    // Past the end is `undefined`, not an error.
    assert_eq!(val(r#"return "ab"[9] === undefined;"#), true);
}

/// **`s[i]`, `s.charAt(i)` and `s.at(i)` are one question** and give one
/// answer: the code unit at `i`.
#[test]
fn char_at_and_at_read_the_same_unit_offset_as_indexing() {
    assert_eq!(
        val(r#"const s = "—b"; return [s[0], s.charAt(0), s.at(0)];"#),
        serde_json::json!(["—", "—", "—"])
    );
    assert_eq!(
        val(r#"const s = "a—b😀";
               return [0, 1, 2, 3, 4].map((i) => s[i] === s.charAt(i));"#),
        serde_json::json!([true, true, true, true, true])
    );
    // Inside an astral character — the one place an index can still land
    // between the halves of something — all three read the same unit rather
    // than one refusing and two inventing. The halves reassemble.
    assert_eq!(
        val(r#"const s = "😀"; return s[0] + s[1] === s
                      && s.charAt(0) === s[0] && s.at(1) === s[1];"#),
        true
    );
    // Out of range keeps its JS answer: "" for charAt, undefined for at.
    assert_eq!(val(r#"return "ab".charAt(9);"#), "");
    assert_eq!(val(r#"return "ab".at(9) === undefined;"#), true);
    // A negative `at` counts back in units, because `length` does.
    assert_eq!(val(r#"return "a—b".at(-1);"#), "b");
}

/// The seven silent wrong answers of 2026-09-24, each with the answer JS
/// gives. Every one of these was measured on the byte model first.
#[test]
fn the_silent_divergences_are_gone() {
    assert_eq!(val(r#"return "aéb".slice(0, 2);"#), "aé"); // was "a"
    assert_eq!(val(r#"return "aéb".substring(0, 2);"#), "aé");
    assert_eq!(val(r#"return "a".padStart(3, "💩").length;"#), 3); // was 9
    assert_eq!(val(r#"return "😀".padStart(4, "-");"#), "--😀"); // was "😀"
    assert_eq!(val(r#"return "a😀b".indexOf("b");"#), 3); // was 5
    assert_eq!(val(r#"return "a😀b".lastIndexOf("b");"#), 3);
    assert_eq!(val(r#"return "a😀b".match(/b/).index;"#), 3); // was 5
    assert_eq!(val(r#"return /./u.exec("😀")[0].length;"#), 2); // was 4
    assert_eq!(val(r#"return "😀".charCodeAt(0);"#), 55357); // was a TypeError
    assert_eq!(val(r#"return "😀".codePointAt(0);"#), 128512);
    // A surrogate pair is the only way `fromCharCode` can say "astral", and
    // it used to drop both halves and return "".
    assert_eq!(val(r#"return String.fromCharCode(0xD83D, 0xDE00);"#), "😀");
    assert_eq!(
        val(r#"return String.fromCharCode(0xD83D, 0xDE00) === "😀";"#),
        true
    );
}

/// **A lone surrogate can be written, and this is the point of the change.**
///
/// It is the one capability UTF-16 has that "UTF-8 with an `is_ascii` bit"
/// does not, and without it the whole representation swap reduces to
/// code-unit indexing — which the cheap option would have delivered for a
/// fifth of the work. `docs/30_STRINGS.md` calls that the most important
/// sentence in the plan. So this test is the answer to "was it worth doing".
///
/// `"\uD800"` compiled to *seven bytes* before: U+FFFD followed by the
/// literal characters `d800`, because oxc's cooked value cannot hold a
/// surrogate and encodes one in band. See `compiler/cook.rs`.
#[test]
fn a_source_literal_can_hold_a_lone_surrogate() {
    assert_eq!(val(r#"return "\uD800".length;"#), 1); // was 7
    assert_eq!(val(r#"return "\uD800".charCodeAt(0);"#), 55296);
    assert_eq!(val(r#"return "\uDFFF".charCodeAt(0);"#), 57343);
    // An explicit pair is one character, and two units.
    assert_eq!(val(r#"return "\uD83D\uDE00".length;"#), 2);
    assert_eq!(val(r#"return "\uD83D\uDE00" === "😀";"#), true);
    // Split: it used to give ["\uFFFD","d","8","0","0"].
    assert_eq!(val(r#"return "\uD800".split("").length;"#), 1);
    // A lone surrogate and a genuine U+FFFD in the same literal — oxc encodes
    // the second one as an escaped escape, so this is where a decoder that
    // collapsed the two cases would be caught.
    assert_eq!(val(r#"return "\uD800\uFFFD".length;"#), 2);
    assert_eq!(val(r#"return "\uD800\uFFFD".charCodeAt(0);"#), 55296);
    assert_eq!(val(r#"return "\uD800\uFFFD".charCodeAt(1);"#), 65533);
    // Template literals cook through the same path.
    assert_eq!(val(r#"return `a\uD800b`.length;"#), 3);
    assert_eq!(val(r#"return `a\uD800b`.charCodeAt(1);"#), 55296);
    assert_eq!(val(r#"return `x${1}\uD800`.length;"#), 3);
    // And a program can tell: the mitigation for the U+FFFD policy at the
    // JSON boundary is that it can ask before the crossing loses anything.
    assert_eq!(val(r#"return "\uD800".isWellFormed();"#), false);
    assert_eq!(val(r#"return "\uD83D\uDE00".isWellFormed();"#), true);
    assert_eq!(
        val(r#"return "\uD800".toWellFormed().charCodeAt(0);"#),
        65533
    );
    // `fromCharCode` and a literal agree, which they could not before.
    assert_eq!(
        val(r#"return String.fromCharCode(0xD800) === "\uD800";"#),
        true
    );
    // An object key can be one too — the key path cooks through `cook.rs` as
    // well, and a key that decoded differently from the literal used to read
    // it would be a hole nothing else would find.
    assert_eq!(val(r#"const o = { "\uD800": 7 }; return o["\uD800"];"#), 7);
    // **A `const` must propagate the literal, not oxc'''s encoding of it.**
    // Constant propagation held the initializer as a `String`, which cannot
    // carry a surrogate, so this answered 5 while the same literal written
    // inline answered 1 — two spellings of one literal disagreeing, which is
    // the shape this whole phase is about. Found by `unicode-back-reference`,
    // a test262 file that had been green on mangled input.
    assert_eq!(val(r#"const s = "\uD800"; return s.length;"#), 1);
    assert_eq!(val(r#"const s = "\uD800"; return s === "\uD800";"#), true);
    assert_eq!(
        val(r#"const s = "foo\uD834bar"; return s.charCodeAt(3);"#),
        55348
    );
}

/// **Code-unit `Ord` is a fix, not a side effect.** JS defines `<` on strings
/// as code-unit comparison; `str::cmp` compared code points, and the two
/// disagree for exactly this pair.
#[test]
fn relational_operators_compare_code_units() {
    assert_eq!(val(r#"return "\u{10000}" < "Ｚ";"#), true);
    assert_eq!(val(r#"return "\u{10000}".localeCompare("Ｚ");"#), -1);
    // The comparator and the operator agree, which is the actual contract.
    assert_eq!(
        val(r#"const xs = ["Ｚ", "\u{10000}"];
               return xs.slice().sort((a, b) => a.localeCompare(b))[0]
                      === (xs[0] < xs[1] ? xs[0] : xs[1]);"#),
        true
    );
}

/// `trim` is defined against JS's whitespace set, not Rust's. They disagree
/// on U+FEFF in one direction and U+200B in the other.
#[test]
fn trim_uses_the_js_whitespace_set() {
    assert_eq!(val(r#"return "﻿ hi  ".trim();"#), "hi");
    assert_eq!(val(r#"return " x".trimStart();"#), "x");
    // A zero-width space is *not* whitespace in JS and must survive.
    assert_eq!(val(r#"return "​x".trim().length;"#), 2);
}

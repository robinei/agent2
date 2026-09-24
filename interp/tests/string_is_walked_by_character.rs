//! **One string, one answer to "what is the character here".**
//!
//! Strings in this dialect are UTF-8 byte sequences: `.length` counts
//! bytes and every index is a byte offset (see the module doc on
//! `vm::mod`). That is a deliberate choice and it stays. What was not
//! deliberate is that the spellings of "read a character" disagreed
//! with each other, and that the one a program reaches for could not
//! be used at all:
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
//! `for…of` lowered to an index loop — `c[i]` while `i < c.length` —
//! which for a string steps one *byte* at a time and hits the trap on
//! the second byte of the first non-ASCII character. `charAt`/`at`
//! returned `byte as char`, the raw byte reinterpreted as a codepoint,
//! so every byte over 0x7F was wrong and `s[0] === s.charAt(0)` was
//! false at a perfectly valid boundary.
//!
//! Found by reading a live session of 2026-09-24, not by a test: a
//! model brace-matching a Rust function body in `report.rs` was cut
//! off mid-character. This repository's own sources are full of em
//! dashes, and `for (const ch of s)` is exactly what a program reaches
//! for once told that `.length` counts bytes — so the advice and the
//! implementation pointed at the same wall.
//!
//! Nothing here changes the byte model. `.length` still counts bytes,
//! `s[i]` still takes a byte offset and still refuses to answer from
//! inside a character. The three readings just agree now.

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

fn err(src: &str) -> String {
    let program = interp::compile(src).expect("compiles");
    let mut vm = interp::VM::for_program(program, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX) {
        Err(e) => e.message,
        Ok(other) => panic!("{src} was expected to fail, got {other:?}"),
    }
}

/// The idiom a program reaches for, on text that is not pure ASCII.
#[test]
fn for_of_walks_characters_not_bytes() {
    assert_eq!(val(r#"let n = 0; for (const ch of "a—b") n++; return n;"#), 3);
    assert_eq!(
        val(r#"let o = ""; for (const ch of "a—b") o += "|" + ch; return o;"#),
        "|a|—|b"
    );
    // The brace-matching scan that started this: it must reach the end.
    assert_eq!(
        val(
            r#"let d = 0, seen = 0;
               for (const ch of "fn f() { /* — */ { } }") {
                 if (ch === "{") { d++; seen++; }
                 if (ch === "}") d--;
               }
               return [d, seen];"#
        ),
        serde_json::json!([0, 2])
    );
    // An empty string is no iterations, not one.
    assert_eq!(val(r#"let n = 0; for (const ch of "") n++; return n;"#), 0);
}

/// `for…of` is the third spelling of this, and it agrees with the two
/// that were already right.
#[test]
fn the_three_ways_to_split_a_string_into_characters_agree() {
    assert_eq!(
        val(r#"const s = "a—b😀c";
               let f = []; for (const ch of s) f.push(ch);
               return [f.length, [...s].length, s.split("").length];"#),
        serde_json::json!([5, 5, 5])
    );
    assert_eq!(
        val(r#"const s = "a—b😀c"; let f = []; for (const ch of s) f.push(ch);
               return f.join("") === s && [...s].join("") === s;"#),
        true
    );
}

/// **Byte offsets are unchanged.** The point of the fix is that the
/// readings agree, not that the model moved.
#[test]
fn length_and_indexing_still_count_bytes() {
    assert_eq!(val(r#"return "a—b".length;"#), 5);
    assert_eq!(val(r#"return "aéb".length;"#), 4);
    assert_eq!(val(r#"return "—b"[0];"#), "—");
    assert_eq!(val(r#"return "—b"[3];"#), "b");
    assert!(err(r#"return "a—b"[2];"#).starts_with("cannot index string at byte offset 2"));
}

/// **`s[i]`, `s.charAt(i)` and `s.at(i)` are one question.** They
/// answered it three ways: two of them returned a character that was
/// not in the string.
#[test]
fn char_at_and_at_read_the_same_byte_offset_as_indexing() {
    assert_eq!(
        val(r#"const s = "—b"; return [s[0], s.charAt(0), s.at(0)];"#),
        serde_json::json!(["—", "—", "—"])
    );
    assert_eq!(
        val(r#"const s = "a—b😀"; return [0, 1, 4].map((i) => s[i] === s.charAt(i));"#),
        serde_json::json!([true, true, true])
    );
    // Inside a character, all three refuse — rather than one refusing
    // and two inventing.
    for src in [
        r#"return "a—b"[2];"#,
        r#"return "a—b".charAt(2);"#,
        r#"return "a—b".at(2);"#,
    ] {
        assert!(
            err(src).contains("falls inside a multi-byte UTF-8 character"),
            "{src} answered from inside a character"
        );
    }
    // Out of range keeps its JS answer: "" for charAt, undefined for at.
    assert_eq!(val(r#"return "ab".charAt(9);"#), "");
    assert_eq!(val(r#"return "ab".at(9) === undefined;"#), true);
    // A negative `at` counts back in bytes, because `length` does.
    assert_eq!(val(r#"return "a—b".at(-1);"#), "b");
}

/// **The refusal says what to do instead.** There is one way to arrive
/// there — stepping an index along a string — and the program that did
/// it cannot be expected to know the remedy.
#[test]
fn the_refusal_names_the_way_out() {
    let m = err(r#"return "a—b"[2];"#);
    assert!(m.contains("for (const ch of s)"), "{m}");
    assert!(m.contains("indexOf"), "{m}");
}

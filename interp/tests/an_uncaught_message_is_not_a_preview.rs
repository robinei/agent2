//! **An uncaught throw is the last thing a program says, so it is said
//! in full.**
//!
//! The top-level render of an uncaught value used to go through
//! `VM::preview`, which cuts a string at 40 bytes. That is right for
//! naming a value *inside* some other error — `cannot index into
//! "abcdefgh…"` — and fatally wrong for the thrown value itself.
//!
//! The harness refuses a bad call by throwing its explanation as a
//! string (`Runner::settle_err` in the `agent` crate), so every one of
//! those refusals reached the model as its first 40 characters. Live on
//! 2026-09-24, a program that passed an array to `history.keep` was
//! handed
//!
//! ```text
//! uncaught exception: "keep_history(result) needs a tool result…"
//! ```
//!
//! and none of the sentence explaining that it should call `keep` on
//! each row instead. That refusal had been written the same morning so
//! that it would teach; 40 characters of it taught nothing, and the
//! truncation was invisible because every test asserted on a prefix.

fn uncaught(src: &str) -> String {
    let program = interp::compile(src).expect("compiles");
    let mut vm = interp::VM::for_program(program, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX) {
        Err(e) => e.message,
        Ok(other) => panic!("expected a throw, got {other:?}"),
    }
}

/// A refusal is a paragraph; it arrives as one.
#[test]
fn a_thrown_string_arrives_whole() {
    let long = "history.keep(result) takes one row: a tool result, or the id of one. \
                For several, one call each: reads.map((r) => history.keep(r)). One row \
                shows one value; there is no combined view.";
    assert!(long.len() > 40, "the case only exists past the old cut");
    let m = uncaught(&format!("throw {:?};", long));
    assert!(m.contains(long), "the whole message survives: {m}");
    assert!(!m.contains('…'), "and nothing was cut: {m}");
}

/// `new Error(...)` was already whole — a thrown string now matches it,
/// rather than being the one shape that got clipped.
#[test]
fn a_thrown_string_and_a_thrown_error_say_the_same_amount() {
    let msg = "the parser drops the last field when the line ends in a comma, which is \
               why the count is one short of what the header promises";
    let s = uncaught(&format!("throw {:?};", msg));
    let e = uncaught(&format!("throw new Error({:?});", msg));
    assert!(s.contains(msg) && e.contains(msg), "{s}\n{e}");
}

/// **Bounded, though.** A program may throw something bulky, and the
/// bound is where a paragraph fits rather than where a phrase does.
#[test]
fn something_bulky_is_still_bounded_and_says_so() {
    let huge = "x".repeat(50_000);
    let m = uncaught(&format!("throw {:?};", huge));
    assert!(m.len() < 4_000, "bounded: {} bytes", m.len());
    assert!(m.contains("50000 bytes total"), "and says what it cut: {m}");
}

/// **Only a thrown string changed.** Everything else still falls
/// through to `preview`, which is right for it: a thrown array or
/// object is a value being identified, not a message being read, and
/// its short summary is the whole point.
#[test]
fn a_thrown_value_that_is_not_a_string_still_gets_its_summary() {
    let m = uncaught("throw [1, 2, 3];");
    assert!(m.contains("[array of 3]"), "{m}");

    let m = uncaught("throw { a: 1, b: 2 };");
    assert!(m.contains("object"), "{m}");
}

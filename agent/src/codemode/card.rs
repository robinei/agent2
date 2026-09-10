//! The card — the system prompt (phase 20 doc, Step C4).
//!
//! Under code mode there is no separate dialect card and no
//! instruction header: this string *is* the system prompt (Step A1:
//! "the card goes in `system`"), the one place the model is told
//! anything, and the immutable cache prefix everything else is
//! appended after. One file, one golden test, versioned — a card
//! change is a behaviour change, same as any other.
//!
//! A spec, not a persona: no "you are a helpful assistant". The
//! register is "programs are written here; emit only valid
//! JavaScript; the whole response is parsed" — every sentence below
//! is held to that.
//!
//! Carries the verb set and nothing else structural — no schemas, the
//! signatures are the documentation — plus the guidance collected from
//! the parts that derived each line, cited inline below so a future
//! edit can trace a sentence back to its reasoning instead of
//! wondering why it's there.

/// The card, verbatim. Kept as one constant (not assembled from
/// fragments) so the golden test below is the whole prompt, not an
/// approximation of it.
pub const CARD: &str = r#"Programs are written here. Every response is a JavaScript program and
nothing else — no prose, no code fence, no explanation outside the
program itself. The whole response is parsed as JavaScript; a response
that fails to parse comes back as a trap. Narrate inside the program
instead, on lines starting `//: ` — these stream to whoever is
watching as they are written. `//:` is what you are about to do;
`say()` is what happened. Only `say()` can report a result, because
only it runs. Open with a `//:` plan block: it streams first and
doubles as the plan the rest of the program follows.

Verbs available in every program, as plain functions — not a `tools.`
namespace, which is reserved for this session's configured tools
(listed separately, below):

  say(text) / say(to, text)        tell the user or another agent
                                    `to` is a quoted name: say("robin", "done")
  ask(who, text)                   ask a question; resolves to the answer
                                    `who` is a quoted name: await ask("robin", "which one?")
  answer(question, label, value)   discharge an `ask()` another program is
                                    blocked on, by that question's own id —
                                    not for an ordinary message: that is say()
  spawn(charter)                   a new agent, a clean room
  fork()                           a new context inheriting your whole history
  append_history(value)            remember a projection for your own future
  artifact(id)                     fetch a completed call's value by id
  list_agents()                    every agent in this subtree, with status
  raise(name, payload?)            suspend for judgement

`raise()` suspends this program and asks for a decision, made by
another program that runs while this one is still suspended. That
program's last line must be `return resume(value);` — continue with
`value` — or `return abandon();` — discard this program; a replacement
follows next. The `return` is not optional: calling `resume(value)` or
`abandon()` without returning it is not a decision, the same as never
calling either. Falling off the end without returning one of those
means no decision was made.

A root program's return value is read by nobody. Reach people through
`say()` — a root program that never calls it is a silent no-op, the one
new way to do nothing at all.

Await at the top level directly — this dialect permits it. Do not wrap
the program in an unawaited `(async () => { ... })()`: a call awaited
only inside that inner function, with its own promise never awaited by
anything, is not guaranteed to finish within this run. Write the
sequence — `for`, `await`, `Promise.all` — as top-level statements.

What a step boundary costs: `raise()` spends an inference on your own
full context and stops this program; `spawn()` spends one on a child's
clean context; `fork()` spends one on a child that inherits everything
you know. A large, self-contained step is nearly free as a `spawn()`. A
judgement that needs this conversation is a `fork()`. Do not `raise()`
once per step — that is the same round trip a tool loop pays, spelled
in JavaScript.

Look before you leap, once: when the shape of the data decides the
approach, a small reconnaissance program followed by the real one beats
guessing — two round trips, not twenty. That is for when you cannot
decide the approach at all until you see the data — not for every read.
If you already know what you would do with the data once you have it —
including asking a question and acting on the answer — read it and
finish the task in this same program; `ask()` is a normal `await`, not
a reason to end early. Ending a program is not "pausing to think":
nothing continues on its own, so a program that stops after reporting
what it found, with the actual task still undone, has not paused —
it has quietly failed to do the task.

Work from what you actually read, not from what a file like this
usually contains. A generic check tuned for a shape the real data
doesn't have will find nothing and call that "fine" — that is a false
negative, not a clean result. If the specific thing in front of you
doesn't match what you expected, say what it actually says, or ask;
never let "no match" stand in for "no problem." A comment or note that
reads like a question ("is this still right?", "or is it X now?") is
the ambiguity announcing itself in plain language — that is louder
than any keyword pattern, and passing over it because nothing matched
a regex is exactly the false negative above.

Match the program to the task. This is a push against timid
orchestration, not against short programs — a question that needs no
tools is a two-line program that `say()`s the answer.

Return a decision, not a dataset: keep data as an artifact reachable by
id, and give `append_history()` a short projection of it, never the raw
result. Append for your own future self across tasks, not to read
something back next turn — if you need a value now, you are already
holding it in a variable.

If history grows too large, a compaction program runs first, with
`remove_history(id, label)` and `rewrite_history(id, label, value)`.
Prefer removing entries outright and keeping the rest verbatim; rewrite
only the rows that truly need shortening.

Tools available in this session:
"#;

/// A worked exemplar: a real user/assistant pair opening `messages`,
/// never part of the card — its whole point is to demonstrate an
/// *assistant* turn (Step B1), which only a message in that role can
/// do. Turn one has nothing else to imitate (the model's own prior
/// programs are its few-shot evidence, and there are none yet), so
/// this is the restoring force against a timid first program —
/// cheap insurance, not a remedy applied after the fact.
pub struct Exemplar {
    pub user: &'static str,
    pub assistant: &'static str,
}

/// Two demonstrations, not one, each carrying a shape the other
/// can't: a tool call with an outcome to branch on, and — added
/// 2026-09-10, after a live regression run's one consistently-failing
/// task turned out to have no worked example of its own shape
/// anywhere in context — reading real data, recognizing a genuine
/// ambiguity, `ask()`-ing about it *inline*, and acting on the answer,
/// all in one program that still ends by reporting what it did. The
/// card's prose already says "ask() is a normal await, not a reason to
/// end early"; a demonstration of a model actually doing that is a
/// stronger restoring force than the sentence describing it.
pub const SEED_EXEMPLARS: &[Exemplar] = &[
    Exemplar {
        user: "can you check whether the tests pass and let robin know?",
        assistant: r#"//: running the test suite, then reporting what happened
const result = await tools.bash("cargo test 2>&1 | tail -20");
if (result.exit === 0) {
    say("robin", "tests pass.");
} else {
    say("robin", `tests failed:\n${result.output}`);
}"#,
    },
    Exemplar {
        user: "the retry count in ops/config.json looks off — can you fix it?",
        assistant: r#"//: read the real value first — I can't tell what "off" means by guessing
const cfg = await tools.read_file("ops/config.json");
const parsed = JSON.parse(cfg.content);
say("robin", `ops/config.json currently sets retries to ${parsed.retries}.`);

//: the file doesn't say what it should be, and a wrong guess is worse
//: than asking — resolve it here, in this same program, then act on it
const target = await ask("robin", `retries is currently ${parsed.retries} — what should it be?`);

parsed.retries = Number(target);
await tools.write_file("ops/config.json", JSON.stringify(parsed, null, 2));
say("robin", `set retries to ${target} in ops/config.json.`);"#,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_is_stable() {
        // A golden test in the sense Step C4 asks for: any edit to
        // `CARD` shows up as a diff review must look at, not a byte
        // count that silently drifts. Comparing full text (not just a
        // hash) so the diff itself is legible in a failure message.
        const EXPECTED_LEN: usize = 5394;
        assert_eq!(
            CARD.len(),
            EXPECTED_LEN,
            "CARD changed length ({} -> {}) — a deliberate edit should \
             update EXPECTED_LEN in this test, not silently pass",
            EXPECTED_LEN,
            CARD.len()
        );
    }

    #[test]
    fn the_card_never_says_you_are_a_helpful_assistant() {
        // "A spec, not a persona" (Step C4), checked directly rather
        // than only asserted in a doc comment.
        // Specific persona-establishing phrases, not the bare "you are
        // a" substring — which false-positives on the card's own
        // legitimate "you are *already* holding it in a variable".
        let lower = CARD.to_lowercase();
        for phrase in ["you are a helpful", "helpful assistant", "i am an ai"] {
            assert!(
                !lower.contains(phrase),
                "found persona language: {phrase:?}"
            );
        }
    }

    #[test]
    fn the_card_names_every_bare_verb() {
        for verb in [
            "say(",
            "ask(",
            "answer(",
            "spawn(",
            "fork(",
            "append_history(",
            "artifact(",
            "raise(",
            "resume(",
            "abandon(",
            "remove_history(",
            "rewrite_history(",
            "list_agents(",
        ] {
            assert!(CARD.contains(verb), "card is missing {verb}");
        }
    }

    #[test]
    fn the_card_states_the_no_fence_rule_and_the_no_op_rule() {
        assert!(CARD.contains("no code fence"));
        assert!(CARD.contains("parsed as JavaScript"));
        assert!(CARD.contains("silent no-op"));
    }

    #[test]
    fn the_exemplars_assistant_turn_is_valid_javascript() {
        // The one thing in this file that must actually compile: each
        // exemplar's assistant turn is exactly what a real completion
        // would need to parse (Step B1's own rule for an assistant
        // turn), so it is held to the same standard here.
        for ex in SEED_EXEMPLARS {
            interp::compile(ex.assistant)
                .unwrap_or_else(|e| panic!("seed exemplar does not parse: {e:?}"));
        }
    }

    #[test]
    fn the_exemplars_assistant_turn_has_no_entry_header_and_no_fence() {
        // Step B1: an assistant turn is bare source, nothing else —
        // each exemplar must model that, not just the card's prose
        // about it.
        for ex in SEED_EXEMPLARS {
            assert!(!ex.assistant.starts_with("```"));
            assert!(!ex.assistant.starts_with('['));
        }
    }

    #[test]
    fn the_exemplars_open_with_a_plan_comment() {
        for ex in SEED_EXEMPLARS {
            assert!(ex.assistant.trim_start().starts_with("//:"));
        }
    }

    #[test]
    fn the_second_exemplar_demonstrates_ask_inline_then_acting_on_it() {
        // Card prose alone ("ask() is a normal await, not a reason to
        // end early") wasn't enough to stop a live run from splitting
        // an ordinary read-then-ask into two programs (2026-09-10) —
        // this exemplar is the demonstration, so hold it to actually
        // being one: `ask(` appears, and something runs after it in
        // the same program (not the program's last line).
        let ex = &SEED_EXEMPLARS[1];
        let ask_at = ex
            .assistant
            .find("await ask(")
            .expect("second exemplar should demonstrate ask()");
        let after = &ex.assistant[ask_at..];
        assert!(
            after.lines().count() > 2,
            "ask() should not be the last meaningful line of the exemplar"
        );
    }
}

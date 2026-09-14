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
watching as they are written, which means they all arrive before any
of the program has run: nothing here executes until the whole response
is generated. So `//:` belongs at the very top, as one plan block that
streams first and doubles as the plan the rest of the program follows
— a `//:` line anywhere later doesn't narrate something happening now,
since nothing has run yet by the time it arrives either; it only makes
the reader wait through more code to reach it. `say()` is what
happened, and it is the only thing that can report a result, because
only it runs. Document the body itself with plain `//` comments, the
ordinary kind — they read alongside the code they're next to, not as
a standalone message, so write them assuming the reader can already
see the lines around them; `//:` narration has to stand on its own
with no code in view.

Verbs available in every program, as plain functions — not a `tools.`
namespace, which is reserved for this session's configured tools
(listed separately, below):

  say(text) / say(to, text)        tell the user or another agent
                                    quote the address, always: say("user", "done")
                                    "user" is the human; there is only one
  ask(who, text)                   ask a question; resolves to the answer
                                    await ask("user", "which one?")
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
guessing — two round trips, not twenty, and the second one has to be a
real `fork()`, not an implied continuation. Writing `//: next, I'll...`
and then ending the program is not a plan: nothing reads that comment
and nothing runs on its own, so whatever you meant to do next simply
does not happen. That is for when you cannot decide the approach at all
until you see the data — not for every read. If you already know what
you would do with the data once you have it — including asking a
question and acting on the answer, or characterizing and comparing what
you just read — read it and finish the task in this same program;
`ask()` is a normal `await`, not a reason to end early. Ending a
program is not "pausing to think": nothing continues on its own, so a
program that stops after reporting what it found, with the actual task
still undone, has not paused — it has quietly failed to do the task.

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

/// Four demonstrations, each carrying a shape the others can't: a
/// tool call with an outcome to branch on; reading real data,
/// recognizing a genuine ambiguity, `ask()`-ing about it *inline*, and
/// acting on the answer, all in one program that still ends by
/// reporting what it did (added 2026-09-10, after a live regression
/// run's one consistently-failing task turned out to have no worked
/// example of its own shape anywhere in context); gathering several
/// pieces of data and finishing the *judgment* about them in the same
/// program, with no deferred step (added 2026-09-14, after the same
/// thing happened again to a different rule); and — added the same
/// day, immediately after: recognizing a judgment that genuinely
/// **cannot** be finished in the same program, because the data itself
/// doesn't decide it, and calling `raise()` instead of stopping having
/// merely noticed the problem. The card's prose already said "ask() is
/// a normal await, not a reason to end early," "ending a program...
/// has quietly failed to do the task," and described `raise()` three
/// times before each of these landed; in every case the sentence alone
/// did not hold against a live run, and a demonstration of the model
/// actually doing the right thing was the stronger restoring force.
///
/// Live evidence for the third exemplar: two independent tasks
/// (`fan-out`, `judgment-in-the-middle`) each produced a genuinely
/// well-reasoned recon-only program whose own closing `//:` comment
/// planned a "next program" — nothing ever ran it, because nothing in
/// the architecture continues a finished program on its own. Live
/// evidence for the fourth: fixing that exposed a *second*,
/// narrower case of the identical failure the third exemplar's fix
/// didn't reach — a program facing two conflicting numbers with no
/// data-side way to break the tie read both, said so, and stopped,
/// having recognized exactly the moment `raise()` exists for and not
/// called it. `raise()` had prose in three places and no worked
/// example anywhere — the one major verb that had never gotten this
/// treatment until this exemplar.
pub const SEED_EXEMPLARS: &[Exemplar] = &[
    Exemplar {
        user: "can you check whether the tests pass and let me know?",
        assistant: r#"//: running the test suite, then reporting what happened
const result = await tools.bash("cargo test 2>&1 | tail -20");
if (result.exit === 0) {
    say("tests pass.");
} else {
    say(`tests failed:\n${result.output}`);
}"#,
    },
    Exemplar {
        user: "the retry count in ops/config.json looks off — can you fix it?",
        assistant: r#"//: read the real value first — I can't tell what "off" means by guessing.
//: The file won't say what it should be either, and a wrong guess is
//: worse than asking — so resolve that here, in this same program,
//: rather than stopping once I've reported what's there now.
const cfg = await tools.read_file("ops/config.json");
const parsed = JSON.parse(cfg.content);
say(`ops/config.json currently sets retries to ${parsed.retries}.`);

const target = await ask("user", `retries is currently ${parsed.retries} — what should it be?`);

parsed.retries = Number(target);
await tools.write_file("ops/config.json", JSON.stringify(parsed, null, 2));
say(`set retries to ${target} in ops/config.json.`);"#,
    },
    Exemplar {
        user: "look at server-a.log and server-b.log and tell me which one is actually failing",
        assistant: r#"//: read both — "failing" is a judgement on the real text, and once I
//: have it there is no separate step to defer to: nothing runs after
//: this program unless I make it, so the comparison happens right here
const [a, b] = await Promise.all([
    tools.read_file("server-a.log"),
    tools.read_file("server-b.log"),
]);

const aErrors = (a.content.match(/ERROR/g) || []).length;
const bErrors = (b.content.match(/ERROR/g) || []).length;
say(aErrors > bErrors
    ? `server-a is failing: ${aErrors} ERROR lines vs server-b's ${bErrors}.`
    : `server-b is failing: ${bErrors} ERROR lines vs server-a's ${aErrors}.`);"#,
    },
    Exemplar {
        user: "two synced backups of the same file disagree — which one should I keep?",
        assistant: r#"//: read both — if they genuinely differ, that's not a fact more reading
//: resolves; it's a judgement call, and I can't invent a reason to
//: prefer one over the other from the files alone. If so: raise it —
//: not guess, and not stop here having only noticed the problem.
const [a, b] = await Promise.all([
    tools.read_file("backup-1.txt"),
    tools.read_file("backup-2.txt"),
]);

if (a.content === b.content) {
    say("identical — no real conflict, either is fine.");
} else {
    const keep = await raise("conflicting_backups", { a: a.content, b: b.content });
    say(`keeping ${keep}.`);
}"#,
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
        const EXPECTED_LEN: usize = 6361;
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
    fn no_exemplar_scatters_narration_mid_program() {
        // `//:` streams to a live watcher purely in generation order —
        // nothing executes until the whole completion is done, so a
        // `//:` line past the opening block doesn't narrate anything
        // happening "now" (found live 2026-09-14, on re-reading the
        // card's own exemplars against docs/20_CODE_MODE.md Step
        // G1b's disclaimer: "a debug surface, not a progress
        // indicator"). It only makes a live watcher wait through more
        // code to reach a line that was never going to correspond to
        // execution timing anyway. `//:` belongs in one contiguous
        // block at the top; anything documenting the body uses plain
        // `//`, which stays inside the collapsed source (Step G3)
        // instead of being pulled into the chat pane as a standalone,
        // possibly context-free message (Step G1b).
        for ex in SEED_EXEMPLARS {
            let mut past_the_opening_block = false;
            for line in ex.assistant.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//:") {
                    assert!(
                        !past_the_opening_block,
                        "`//:` line found after the opening block in: {:?}",
                        ex.assistant
                    );
                } else if !trimmed.is_empty() {
                    past_the_opening_block = true;
                }
            }
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

    #[test]
    fn the_third_exemplar_finishes_the_judgment_in_the_same_program() {
        // Card prose alone ("ending a program... has quietly failed to
        // do the task") wasn't enough either — two independent live
        // runs (2026-09-14) each wrote a genuinely well-reasoned
        // recon-only program whose own closing comment planned a "next
        // program," which nothing then ran. This exemplar is the
        // demonstration: both reads happen, then the judgment (a
        // say() call comparing what was actually read) happens in the
        // same program, not a planned-but-absent one.
        let ex = &SEED_EXEMPLARS[2];
        let last_read = ex
            .assistant
            .rfind("tools.read_file(")
            .expect("third exemplar should read more than one thing");
        let after = &ex.assistant[last_read..];
        assert!(
            after.contains("say("),
            "the judgement must be reported in the same program as the reads, \
             not deferred to an implied next one"
        );
        // The specific failure this exemplar answers: a program ending
        // on a forward-looking comment instead of doing the work.
        assert!(!ex.assistant.to_lowercase().contains("next program"));
    }

    #[test]
    fn the_fourth_exemplar_actually_calls_raise() {
        // raise() had prose in three places and no worked example
        // anywhere — live 2026-09-14 found a program that read two
        // genuinely conflicting, data-side-irresolvable numbers,
        // correctly recognized it couldn't decide, and then just
        // stopped instead of calling raise() — recognizing the
        // moment isn't the same as acting on it, the same gap a
        // worked example (not another sentence) closed for ask() and
        // for finishing a judgement inline.
        let ex = &SEED_EXEMPLARS[3];
        assert!(
            ex.assistant.contains("await raise("),
            "the fourth exemplar exists specifically to demonstrate raise() \
             actually being called, not just described"
        );
        // And it must not be the program's last line — same
        // "reported, not just decided" standard the ask()-inline
        // exemplar is held to.
        let raise_at = ex.assistant.find("await raise(").unwrap();
        assert!(ex.assistant[raise_at..].lines().count() > 1);
    }
}

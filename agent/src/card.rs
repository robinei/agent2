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
that fails to parse comes back as a trap. Say things to people with
`tell()` — it is the only way anything reaches a reader, and a comment
never does. Open with a `tell()` saying what you are about to do, in
one or two sentences, before the work starts: someone is waiting, and
that line is what they read while the rest of this program is still
being written. Then `tell()` again as things actually happen, carrying
what you found — a count, a name, the thing that was surprising — which
is the part a plan written up front cannot contain.

Comments are for the code, not for the reader of the conversation.
Write plain `//` comments where a line needs explaining, assuming
whoever reads them can see the lines around them.

Verbs available in every program, as plain functions — not a `tools.`
namespace, which is reserved for this session's configured tools
(listed separately, below):

  tell(text) / tell(to, text)      message someone. Bare, it reaches
                                    whoever is waiting on you, or the
                                    user when no one is
                                    "user" is the human; there is only one
  ask(who, text)                   ask a question; resolves to the answer
                                    await ask("user", "which one?")
  answer(question, label, value)   discharge an `ask()` another program is
                                    blocked on, by that question's own id —
                                    not for an ordinary message: that is tell()
  spawn(charter)                   a new agent, a clean room. Creating is
                                    not messaging: it comes back idle, and
                                    does nothing until you tell/ask it
  fork()                           a new context inheriting your whole
                                    history. Also idle until messaged
  append_history(value)            remember a projection for your own future
  artifact(id)                     fetch a completed call's value by id
  list_agents()                    every agent in this subtree, with status
  raise(name, payload?)            suspend for judgement

**When to ask, and who to ask.** The moment the next step turns on a
judgement the data cannot settle, stop guessing and get the judgement.
Which verb depends only on who can give it. A person has to decide
(which of these did you mean, is this the one to delete, do you want
this at all) — `await ask("user", …)`, and act on the answer in this
same program. A judgement that needs everything you have read and
worked out, but no new information from outside — `raise()`, and the
decision comes back into the middle of this program with every variable
still alive. Neither is a last resort or an admission: guessing at a
question that has a real answer is the failure, and an irreversible
step taken on a guess is the expensive one.

`raise()` suspends this program and asks for a decision, made by
another program that runs while this one is still suspended. That
program's last line must be `return resume(value);` — continue with
`value` — or `return abandon();` — discard this program; a replacement
follows next. The `return` is not optional: calling `resume(value)` or
`abandon()` without returning it is not a decision, the same as never
calling either. Falling off the end without returning one of those
means no decision was made.

A root program's return value is read by nobody. Reach people through
`tell()` — a root program that never calls it is a silent no-op, the one
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

**Finish the task in this program.** Reading, deciding and acting all
belong in one program: `ask()` and `raise()` are ordinary `await`s that
hand you an answer mid-program, not reasons to stop. Ending a program is
not pausing to think — nothing continues on its own, so a program that
reports what it found and stops, with the task still undone, has
quietly failed it, and `tell("next, I'll…")` before ending promises work
that will never happen.

Only one thing justifies a short first program: you cannot know what to
do until you see the data, in a way you cannot express as code. Then
read, and end with a `fork()` you message — the continuation has to be
real, not implied.

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
tools is a two-line program that `tell()`s the answer.

Nobody reads your return value, so nothing you want seen belongs in it.
Keep data as an artifact reachable by id, and give `append_history()` a
short projection of it, never the raw result. Append for your own future self across tasks, not to read
something back next turn — if you need a value now, you are already
holding it in a variable.

If history grows too large, a compaction program runs first, with
`remove_history(id, label)` and `rewrite_history(id, label, value)`.
Prefer removing entries outright and keeping the rest verbatim; rewrite
only the rows that truly need shortening.

Tools available in this session:
"#;

/// Per-tool clip for the rendered input schema, carried over unchanged
/// from the deleted `host/dialect.rs` (22_ONE_VOCABULARY's licensed
/// step 1: this ~80-line renderer was the one thing `dialect.rs` did
/// that `card.rs` did not, so it moved here rather than being
/// reinvented). A schema can run long (nested objects, enums); this
/// keeps one misbehaving tool from dominating the cache-immutable
/// prefix the rest of the card sits in front of.
const SCHEMA_MAX_BYTES: usize = 200;

/// The tool manifest appended after [`CARD`]: one line per registered
/// tool, sorted by name, each with its description and a clipped
/// preview of its positional-argument schema.
///
/// This is the one piece of the system prompt that is a property of
/// *this session's* registry rather than static text — everything above
/// it in [`CARD`] is the same for every agent everywhere. Kept as a
/// separate function (not folded into `CARD` itself) so the immutable
/// prefix — the part every request shares byte-for-byte, where cache
/// hits actually pay off — stops at the end of `CARD`, and only the
/// tail varies per session/allowlist.
pub fn tool_manifest(registry: &crate::host::ToolRegistry) -> String {
    let mut manifest = String::new();
    let mut tools: Vec<_> = registry.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    for def in tools {
        manifest.push_str(&format!(
            "\n- tools.{} — {} args schema: {}",
            def.name,
            def.description,
            crate::report::clip(&def.input_schema.to_string(), SCHEMA_MAX_BYTES),
        ));
    }
    manifest
}

/// The full system prompt for one agent: [`CARD`] plus [`tool_manifest`]
/// for its (possibly allowlist-narrowed) registry. Callers snapshot this
/// once, at the agent's root (`Agent.system` — see `types::EventPayload`),
/// never recompute it mid-conversation: the system prompt is the
/// immutable cache prefix, and a later card edit or registry change must
/// not alter an existing conversation's prompt out from under it.
pub fn full_card(registry: &crate::host::ToolRegistry) -> String {
    format!("{CARD}{}", tool_manifest(registry))
}

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

/// Five demonstrations, each carrying a shape the others can't: a
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
/// well-reasoned recon-only program whose own closing comment planned
/// a "next program" — nothing ever ran it, because nothing in
/// the architecture continues a finished program on its own. Live
/// evidence for the fourth: fixing that exposed a *second*,
/// narrower case of the identical failure the third exemplar's fix
/// didn't reach — a program facing two conflicting numbers with no
/// data-side way to break the tie read both, said so, and stopped,
/// having recognized exactly the moment `raise()` exists for and not
/// called it. `raise()` had prose in three places and no worked
/// example anywhere — the one major verb that had never gotten this
/// treatment until this exemplar.
///
/// The fifth is different in kind from the other four: it isn't
/// fixing an observed live failure, because `append_history`'s
/// payoff isn't wired to anywhere yet in this standalone harness — no
/// multi-turn session, no compaction, and (checked directly,
/// 2026-09-14) not even to a same-run handler; a raised program's own
/// prior `append_history` calls never reach the completion that
/// decides its fate. So there was nothing to observe failing. This
/// exemplar exists to test the other half of that gap on purpose: does
/// the model reach for `append_history` appropriately — voluntarily,
/// for a genuinely recurring task, with a short projection rather than
/// the raw data, and *not* used to read something back the same
/// turn — once it's shown how, even with the payoff still unbuilt?
/// `runner::RunOutcome::appended` is the instrument; nothing gates a
/// task's pass/fail on it, the same observational discipline the
/// ask/raise and resume/abandon experiments already use.
#[allow(dead_code)] // see `Exemplar` -- restored in Pass C.
pub const SEED_EXEMPLARS: &[Exemplar] = &[
    Exemplar {
        user: "can you check whether the tests pass and let me know?",
        assistant: r#"tell("running the test suite, then I'll let you know what happened.");
const result = await tools.bash("cargo test 2>&1 | tail -20");
if (result.exit === 0) {
    tell("tests pass.");
} else {
    tell(`tests failed:\n${result.output}`);
}"#,
    },
    Exemplar {
        user: "the retry count in ops/config.json looks off — can you fix it?",
        assistant: r#"tell("reading the current retry setting before touching it.");
// I can't tell what "off" means by guessing, and the file won't say
// what it should be either — a wrong guess is worse than asking, so
// resolve that here, in this same program, rather than stopping once
// I've reported what's there now.
const cfg = await tools.read_file("ops/config.json");
const parsed = JSON.parse(cfg.content);
tell(`ops/config.json currently sets retries to ${parsed.retries}.`);

const target = await ask("user", `retries is currently ${parsed.retries} — what should it be?`);

parsed.retries = Number(target);
await tools.write_file("ops/config.json", JSON.stringify(parsed, null, 2));
tell(`set retries to ${target} in ops/config.json.`);"#,
    },
    Exemplar {
        user: "look at server-a.log and server-b.log and tell me which one is actually failing",
        assistant: r#"tell("reading both logs to see which one is actually failing.");
// "failing" is a judgement on the real text, and once I have it there
// is no separate step to defer to: nothing runs after this program
// unless I make it, so the comparison happens right here.
const [a, b] = await Promise.all([
    tools.read_file("server-a.log"),
    tools.read_file("server-b.log"),
]);

const aErrors = (a.content.match(/ERROR/g) || []).length;
const bErrors = (b.content.match(/ERROR/g) || []).length;
tell(aErrors > bErrors
    ? `server-a is failing: ${aErrors} ERROR lines vs server-b's ${bErrors}.`
    : `server-b is failing: ${bErrors} ERROR lines vs server-a's ${aErrors}.`);"#,
    },
    Exemplar {
        user: "two synced backups of the same file disagree — which one should I keep?",
        assistant: r#"tell("comparing the two backups now.");
// If they genuinely differ, that's not a fact more reading resolves;
// it's a judgement call, and I can't invent a reason to prefer one
// over the other from the files alone. If so: raise it — not guess,
// and not stop here having only noticed the problem.
const [a, b] = await Promise.all([
    tools.read_file("backup-1.txt"),
    tools.read_file("backup-2.txt"),
]);

if (a.content === b.content) {
    tell("identical — no real conflict, either is fine.");
} else {
    const keep = await raise("conflicting_backups", { a: a.content, b: b.content });
    tell(`keeping ${keep}.`);
}"#,
    },
    Exemplar {
        user: r#"[the program you were running has suspended]

  4 | if (a.content === b.content) {
  5 |     tell("identical — no real conflict, either is fine.");
  6 | } else {
  7 |     const keep = await raise("conflicting_backups", { a: a.content, b: b.content });
    |                              ^ condition `conflicting_backups` raised

payload: {"a":"retries=3\nbackoff=200ms","b":"retries=3\nbackoff=500ms"}

Write a handler program; its `return` value is the restart. `resume(value)` continues past the raise with `value` becoming the result of the `raise(...)` expression; `abandon()` gives up on it. Or do neither and write a program that handles this some other way."#,
        assistant: r#"// The two differ only in backoff, and the larger one is the safer
// default under load — that is a judgement I can make from what is
// already here, so it does not need to go to a person.
tell("backup-2 has the longer backoff (500ms vs 200ms) — safer under load, so I'm keeping that one.");
return resume("backup-2.txt");"#,
    },
    Exemplar {
        user: "summarise what each of the twelve files in reports/ is about",
        assistant: r#"tell("reading reports/ — twelve files, so I'll hand the summarising to a helper rather than do it in one long pass here.");
const names = (await tools.bash("ls reports/")).output.trim().split("\n");

// Twelve summaries is a lot of judgement and none of it needs my
// history, so it goes to a clean room. spawn() only creates the agent —
// it sits idle until asked, so the ask is what actually sets it working,
// and awaiting it is what brings the answer back here.
const helper = spawn("You summarise files. One sentence each, concrete, no preamble.");

const summaries = [];
for (const name of names) {
    const file = await tools.read_file(`reports/${name}`);
    summaries.push(await ask(helper, `One sentence on what this is about:\n\n${file.content}`));
}

tell(names.map((n, i) => `${n}: ${summaries[i]}`).join("\n"));"#,
    },
    Exemplar {
        user: "clean up temp files older than a day in /tmp/build-cache — this cleanup job runs nightly",
        assistant: r#"tell("clearing anything in /tmp/build-cache older than a day, and I'll flag it if tonight's count looks unusual.");
const found = await tools.bash("find /tmp/build-cache -type f -mtime +1 -printf '.' | wc -c");
const count = Number(found.output.trim()) || 0;

if (count === 0) {
    tell("nothing older than a day — /tmp/build-cache is already clean.");
} else {
    await tools.bash("find /tmp/build-cache -type f -mtime +1 -delete");
    tell(`removed ${count} stale file(s).`);
    if (count > 200) {
        // a short projection for whoever runs this next — not read
        // back by me, I'm done; this run's own count is already in
        // the variable I just used
        append_history(`/tmp/build-cache had ${count} stale files tonight — well above the usual handful; worth checking what's writing there if it keeps climbing.`);
    }
}"#,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{ToolDef, ToolRegistry};
    use serde_json::json;

    fn registry_with_tools() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "fetch_page".into(),
            description: "Fetch a URL and return its body text.".into(),
            input_schema: json!({ "type": "array", "items": [{ "type": "string" }] }),
            handler: Box::new(|_| Ok(json!(null))),
        });
        registry
    }

    /// `dialect.rs`'s own test, ported: the manifest is generated from
    /// the registry's schemas, not hand-maintained prose.
    #[test]
    fn tool_manifest_is_generated_from_schemas() {
        let manifest = tool_manifest(&registry_with_tools());
        let line = manifest
            .lines()
            .find(|l| l.starts_with("- tools.fetch_page"))
            .expect("a fetch_page line");
        assert!(line.contains("Fetch a URL and return its body text."));
        assert!(line.contains(r#"{"type":"array","items":[{"type":"string"}]}"#));
    }

    #[test]
    fn full_card_appends_the_manifest_after_the_card() {
        let full = full_card(&registry_with_tools());
        assert!(full.starts_with(CARD));
        assert!(full.contains("- tools.fetch_page"));
    }

    #[test]
    fn the_card_is_stable() {
        // A golden test in the sense Step C4 asks for: any edit to
        // `CARD` shows up as a diff review must look at, not a byte
        // count that silently drifts. Comparing full text (not just a
        // hash) so the diff itself is legible in a failure message.
        const EXPECTED_LEN: usize = 6787;
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
            "tell(",
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
    fn the_exemplars_open_with_a_tell() {
        // Narration used to be a special leading-comment convention
        // that nothing ever streamed to anyone (there was no
        // extraction code anywhere in the harness); it's replaced by
        // an ordinary `tell()` call, which actually reaches the user.
        // `tell()` has no placement rule the old convention claimed to
        // (a live watcher never existed to have one for) — it can
        // appear anywhere in the program — but each exemplar still
        // opens by saying what it's about to do, which is the
        // property this checks. Leading `//` comments are skipped:
        // a comment is not a statement, and the handler exemplar
        // opens by explaining the judgement it is about to make
        // before announcing it.
        for ex in SEED_EXEMPLARS {
            let first_statement = ex
                .assistant
                .lines()
                .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
                .unwrap_or("");
            assert!(
                first_statement.trim_start().starts_with("tell("),
                "exemplar for {:?} opens with {first_statement:?}, not a tell()",
                ex.user
            );
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
        // tell() call comparing what was actually read) happens in the
        // same program, not a planned-but-absent one.
        let ex = &SEED_EXEMPLARS[2];
        let last_read = ex
            .assistant
            .rfind("tools.read_file(")
            .expect("third exemplar should read more than one thing");
        let after = &ex.assistant[last_read..];
        assert!(
            after.contains("tell("),
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

    #[test]
    fn the_fifth_exemplar_uses_append_history_as_a_short_projection() {
        // append_history's payoff isn't wired anywhere yet (see the
        // doc comment on SEED_EXEMPLARS) — this exemplar tests only
        // whether the model reaches for the verb appropriately once
        // shown how, not whether anything downstream uses it.
        // Found by what it demonstrates, not by position — an exemplar
        // added ahead of it must not silently retarget this test at a
        // different one.
        let ex = SEED_EXEMPLARS
            .iter()
            .find(|e| e.assistant.contains("append_history("))
            .expect("an exemplar demonstrating append_history");
        assert!(
            ex.assistant.contains("append_history("),
            "the fifth exemplar exists to demonstrate append_history actually \
             being called, not just described"
        );
        // The card's own rule: "never the raw result." The appended
        // string must be short — a projection, not a dump of the
        // count() call's own output.
        let call_start = ex.assistant.find("append_history(").unwrap();
        let call_end = ex.assistant[call_start..].find(");").unwrap() + call_start;
        let payload = &ex.assistant[call_start..call_end];
        assert!(
            payload.len() < 200,
            "append_history's payload should be a short projection, not a \
             dump: {} bytes",
            payload.len()
        );
        // And the card's other rule: not to read something back the
        // same turn. There is no matching artifact()/read of this
        // call's own value anywhere in the exemplar.
        assert!(!ex.assistant.contains("artifact("));
    }
}

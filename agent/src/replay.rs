//! Feed a log's own inputs back through the harness and see what comes
//! out.
//!
//! **A log is already a complete script.** Everything that went *in* is
//! recorded: the user's posts, every tool result, the answer to every
//! question — and the reply text itself, which reconstructs byte for
//! byte from its parts because [`crate::testkit::Invariant::PartsConcatenate`]
//! says it must. So a run can be replayed with nothing captured that a
//! log does not already hold, and 369 of the 373 kept runs on this
//! machine are single-branch, which is 1,368 real replies available as
//! test input for free.
//!
//! That matters because real replies reach shapes no fixture does: a
//! seven-cell reply, a provider leaking `</think>`, a completion cut off
//! mid-token, a model writing `<tool_call>` at a harness that has no such
//! channel. The bug this file was written a day after — a cell ending in
//! a fire-and-forget call ending the whole reply — needs exactly one of
//! those shapes, and it is in **57 of the corpus's 323 multi-cell
//! replies**.
//!
//! **The log is the snapshot; nothing is blessed.** A stored expectation
//! would be invalidated wholesale by any deliberate change — the day
//! `finish` started halting, every one of them would have gone red at
//! once, which is how a snapshot suite stops being read. Instead both
//! sides are projected through [`Said`] and the *projections* compared:
//! what it said, what it appended, what it called, how it ended. Those
//! move when behaviour moves and stay still when an id shifts, and they
//! need no file to keep up to date — record a new run and the corpus
//! grows itself.
//!
//! **What it cannot tell you.** It compares the harness against its own
//! past, so a divergence means something *moved*, never that the move
//! was wrong. After a deliberate change the right reading is a report —
//! "107 logs called `done()` with no argument, as intended" — which is
//! why the corpus sweep prints a tally rather than asserting. The
//! invariants are the half that says a direction is wrong.
//!
//! **First sweep, 2026-09-20, 369 logs in 14 seconds:**
//!
//! | | |
//! |---|---|
//! | identical | 97 |
//! | name event ids, and their ids did not line up ([`Script::names_ids`]) | 147 |
//! | stopped following the run ([`Replayed::drift`]) | 2 |
//! | moved | 123 |
//!
//! Every one of the moved traced to a change that was made on purpose
//! or to a shape the log format has since left behind: `done()` with no
//! argument, now a compile error; the console recorded once per
//! `console.log` rather than once per line; the `↓ history[N]`
//! annotation that used to reach the person; a row stored as a JSON
//! string rather than a value; and one reply from before the notebook
//! splitter worked, logged whole as prose with its fence inside it.
//! **No unexplained regression** — which is the result to want from a
//! first run, and the baseline the next one is read against.
//!
//! **Feeding the reasoning back is what makes the ids line up.** A real
//! completion carries the model's thinking, and the harness logs it as
//! a part of the reply — so a replay that left it out logged one event
//! fewer per reply and everything after shifted. Programs name ids
//! constantly (`history.fetch(9)` is the vocabulary working: the model
//! reads a row's id out of its document and writes it back), so that
//! one missing part put 160 of 368 runs out of reach. Feeding it
//! brought 63 of them back, and the rest are logs whose ids genuinely
//! do not line up — a run that drifted for its own reasons, or a shape
//! this adapter does not reproduce.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::testkit::{Conversation, Ending, Said};
use crate::types::{Address, Author, Call, EventId, EventPayload, Outcome, Tree};

/// One call the original made, and what it was told.
#[derive(Debug, Clone)]
pub struct Called {
    pub name: String,
    pub args: serde_json::Value,
    pub outcome: Option<Result<serde_json::Value, String>>,
}

/// One thing that went into a run, in the order it went in.
#[derive(Debug, Clone)]
pub enum Step {
    /// The person said something.
    User(String),
    /// The model replied — the whole completion, prose and fences,
    /// with whatever the provider said it was thinking. The reasoning
    /// is not an input to the *program*, but it is logged as a part of
    /// the reply, so a replay that drops it logs one event fewer and
    /// every id after it shifts.
    Reply {
        text: String,
        thinking: Option<String>,
    },
    /// A question to the person was answered.
    Answer(serde_json::Value),
}

/// A log read back as its own inputs.
pub struct Script {
    pub charter: String,
    pub steps: Vec<Step>,
    /// Every tool call the run made, in dispatch order, with what it
    /// was answered — `None` for one that never settled, which is how
    /// a run that ended mid-call is recorded.
    ///
    /// **In order, not by name.** Keying by name and matching on
    /// arguments looks more forgiving and is worse: when the replay
    /// makes a call the original did not, a by-name queue quietly hands
    /// it the *next* result and every call after that is answered with
    /// its neighbour's value. The whole run then diverges, and the
    /// report blames the last thing to look wrong. In order, with the
    /// name and arguments checked, the divergence is caught at the call
    /// that caused it.
    pub calls: Vec<Called>,
    /// Whether any cell names an event id as a literal —
    /// `history.fetch(20)`, `remove_history(7)`, `answer(3, …)`.
    ///
    /// **Such a run cannot be replayed faithfully, and that is a fact
    /// about the vocabulary rather than a shortcoming here.** Ids are
    /// the harness's index: the model reads one out of its document and
    /// writes it back, which is exactly what `history` is for. A replay
    /// logs its own events, so #20 in the replay is whatever the replay
    /// put there — and the settlement batching alone is enough to shift
    /// it. The fetch then lands on a different row and returns
    /// something with none of the fields the program expects.
    ///
    /// Reported as its own verdict rather than counted as a
    /// divergence: it says nothing about whether behaviour moved.
    pub names_ids: bool,
}

/// Read a branch of a log back as the inputs that produced it.
pub fn script_of(tree: &Tree, leaf: EventId) -> Result<Script, String> {
    let path = tree.path_events(leaf);
    let charter = path
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::Agent { charter, .. } => Some(charter.clone()),
            _ => None,
        })
        .ok_or("the branch has no Agent root")?;

    // The reply text, rebuilt from its parts — the one input that is
    // not recorded as itself — and the reasoning beside it.
    let mut replies: HashMap<EventId, String> = HashMap::new();
    let mut thinking: HashMap<EventId, String> = HashMap::new();
    for event in &path {
        if let EventPayload::Part { reply, part } = &event.payload {
            match part {
                crate::types::Part::Prose(t) | crate::types::Part::Cell(t) => {
                    replies.entry(*reply).or_default().push_str(t)
                }
                crate::types::Part::Thinking(t) => thinking.entry(*reply).or_default().push_str(t),
            }
        }
    }

    // Every settlement, by the call it answers — so an `Invoke` can be
    // paired with its outcome without a second walk.
    let mut settled: HashMap<EventId, Result<serde_json::Value, String>> = HashMap::new();
    for event in &path {
        if let EventPayload::Result { call, outcome } = &event.payload {
            settled.insert(*call, outcome_to_result(outcome));
        }
    }

    let mut steps = Vec::new();
    let mut calls = Vec::new();
    for event in &path {
        match &event.payload {
            EventPayload::Post {
                from: Author::User,
                origin,
            } => {
                if let Some((text, _, _)) = tree.resolve(origin).direct() {
                    steps.push(Step::User(text.to_owned()));
                }
            }
            // **A reply with no parts is an input, not an absence.**
            // The provider returned nothing — it spent the whole
            // completion on reasoning, or errored after the first
            // token — and the harness has behaviour for that: an empty
            // program, a terminal, and a notice saying the reply said
            // nothing. Skipping it here made the replay stop a step
            // short and report the ending as moved, which is the
            // adapter's fault and not the harness's.
            EventPayload::Reply | EventPayload::Restart => {
                steps.push(Step::Reply {
                    text: replies.get(&event.id).cloned().unwrap_or_default(),
                    thinking: thinking.get(&event.id).cloned(),
                });
            }
            EventPayload::Call(Call::Invoke { name, args, .. }) => calls.push(Called {
                name: name.clone(),
                args: args.clone(),
                outcome: settled.get(&event.id).cloned(),
            }),
            // The person answering a question is an input; a `tell`
            // being delivered is the harness settling its own send.
            EventPayload::Call(Call::Send {
                to: Address::User,
                expects_reply: true,
                ..
            }) => {
                if let Some(Ok(v)) = settled.get(&event.id) {
                    steps.push(Step::Answer(v.clone()));
                }
            }
            _ => {}
        }
    }
    let names_ids = replies.values().any(|text| names_an_id(text));
    Ok(Script {
        charter,
        steps,
        calls,
        names_ids,
    })
}

/// Whether a reply writes an event id as a literal argument to one of
/// the verbs that takes one.
///
/// **Both spellings of each**, because a program may write either: the
/// compiler lowers `history.fetch` to the settle verb `fetch_history`,
/// and a hand-written program can name the settle verb directly.
///
/// `keep` and `peek` were missing until 2026-09-24, and they take an id
/// like the rest — a live run that day wrote
/// `history.peek(6, (r) => …)` off the menu. The cost of the omission
/// is not cosmetic: this flag decides whether a divergence is charged
/// to the harness (`Drifted`) or excused as the run having named an id
/// (`NamesIds`), so a run that kept or peeked by number and then
/// diverged was scored as a regression it did not cause.
fn names_an_id(reply: &str) -> bool {
    const VERBS: [&str; 10] = [
        "history.fetch(",
        "fetch_history(",
        "history.keep(",
        "keep_history(",
        "history.peek(",
        "peek_history(",
        "history.remove(",
        "remove_history(",
        "history.replace(",
        "replace_history(",
    ];
    VERBS.iter().any(|verb| {
        reply.match_indices(verb).any(|(at, _)| {
            reply[at + verb.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    })
}

fn outcome_to_result(outcome: &Outcome) -> Result<serde_json::Value, String> {
    match outcome {
        Outcome::Delivered(v) => Ok(v.clone()),
        Outcome::Failed(why) => Err(why.clone()),
    }
}

/// Run a script through a fresh harness and project what it did.
///
/// The invariants run on every reply, as they do for any test — so a
/// replay that breaks one fails here rather than quietly diverging.
pub fn run(script: &Script) -> Replayed {
    let mut c = Conversation::with_charter(&script.charter);
    // A run that ended mid-call left a call unsettled on purpose; the
    // replay leaves it unsettled too, so it parks in the same place.
    // Everything else is answered in order, with the name and arguments
    // checked — see `Script::calls`.
    let queue: std::rc::Rc<RefCell<std::collections::VecDeque<Called>>> =
        std::rc::Rc::new(RefCell::new(script.calls.iter().cloned().collect()));
    let drifted: std::rc::Rc<RefCell<Option<String>>> = std::rc::Rc::new(RefCell::new(None));
    for name in script
        .calls
        .iter()
        .map(|c| c.name.clone())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let queue = std::rc::Rc::clone(&queue);
        let drifted = std::rc::Rc::clone(&drifted);
        let expected_name = name.clone();
        c.tool(&name, move |args| {
            let next = queue.borrow_mut().pop_front();
            if std::env::var("AGENT_REPLAY_TRACE").is_ok() {
                eprintln!(
                    "TRACE call {expected_name}({}) -> {:?}",
                    brief(args),
                    next.as_ref()
                        .map(|c| (c.name.clone(), brief(&c.args), c.outcome.is_some()))
                );
            }
            match next {
                Some(called) if called.name == expected_name && &called.args == args => {
                    called.outcome.unwrap_or_else(|| {
                        Err("the run this replays never settled this call".to_owned())
                    })
                }
                Some(called) => {
                    let why = format!(
                        "expected `{}({})`, got `{expected_name}({})`",
                        called.name,
                        brief(&called.args),
                        brief(args)
                    );
                    drifted.borrow_mut().get_or_insert(why.clone());
                    Err(why)
                }
                None => {
                    let why = format!("`{expected_name}` called after the original ran out");
                    drifted.borrow_mut().get_or_insert(why.clone());
                    Err(why)
                }
            }
        });
    }
    for step in &script.steps {
        match step {
            Step::User(text) => {
                c.user(text);
            }
            Step::Reply { text, thinking } => {
                if let Some(thinking) = thinking {
                    c.thinking(thinking);
                }
                c.reply(text);
            }
            Step::Answer(value) => {
                // The ask this answers is whichever one is open; a
                // branch holds at most one at a time.
                let Some(ask) = c.open_ask() else { continue };
                c.answer(ask, value.clone());
            }
        }
    }
    let said = Said::of_branch(c.tree(), c.runner().spine.leaf_id);
    let drift = drifted.borrow().clone();
    Replayed { said, drift }
}

/// What a replay produced, and whether it stopped following the run it
/// was replaying.
///
/// **A drift makes every later field meaningless.** Once the replay has
/// made a different call, the results it is handed are its neighbours'
/// and everything downstream cascades — so a report that lists the
/// eight fields that then differ is naming symptoms. `drift` is the
/// cause, and where it is `Some` nothing else is worth reading.
pub struct Replayed {
    pub said: Said,
    pub drift: Option<String>,
}

/// A field's value, short enough that a report of thirty of them still
/// reads as a report.
fn brief_line(s: &str) -> String {
    let flat = s.replace('\n', " ⏎ ");
    if flat.chars().count() <= 220 {
        return flat;
    }
    format!(
        "{}… ({} chars)",
        flat.chars().take(220).collect::<String>(),
        flat.chars().count()
    )
}

fn brief(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.chars().count() <= 60 {
        return s;
    }
    format!("{}…", s.chars().take(60).collect::<String>())
}

/// One field of the projection that came out different.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub field: &'static str,
    pub was: String,
    pub now: String,
}

/// What moved between the run and its replay.
///
/// Deliberately not the event sequence: ids are positional, so one
/// extra event shifts every id after it and a one-line change reads as
/// a whole-log diff. These are the facts a reader cares about, and they
/// hold still through a refactor.
pub fn compare(was: &Said, now: &Said) -> Vec<Divergence> {
    let mut out = Vec::new();
    let mut note = |field, a: String, b: String| {
        if a != b {
            out.push(Divergence {
                field,
                was: a,
                now: b,
            });
        }
    };
    note(
        "prose",
        format!("{:?}", was.prose),
        format!("{:?}", now.prose),
    );
    note(
        "tells",
        format!("{:?}", was.tells),
        format!("{:?}", now.tells),
    );
    note(
        "rows",
        format!("{:?}", was.values()),
        format!("{:?}", now.values()),
    );
    note(
        "calls",
        format!("{:?}", call_names(was)),
        format!("{:?}", call_names(now)),
    );
    note(
        "printed",
        format!("{:?}", was.printed),
        format!("{:?}", now.printed),
    );
    note(
        "asked",
        format!("{:?}", asks_of(was)),
        format!("{:?}", asks_of(now)),
    );
    note(
        "ended",
        format!("{:?}", ending_shape(&was.ended)),
        format!("{:?}", ending_shape(&now.ended)),
    );
    out
}

fn call_names(s: &Said) -> Vec<&str> {
    s.calls.iter().map(|(n, _)| n.as_str()).collect()
}

fn asks_of(s: &Said) -> Vec<&str> {
    s.asks.iter().map(|a| a.text.as_str()).collect()
}

/// An ending by kind, not by payload. A trap's message can carry a
/// path or a byte offset that says nothing about whether behaviour
/// moved; that it trapped at all does.
fn ending_shape(e: &Ending) -> &'static str {
    match e {
        Ending::Finished(_) => "finished",
        Ending::Completed(_) => "handed on",
        Ending::Raised { .. } => "raised",
        Ending::Trapped { .. } => "trapped",
        Ending::CellFailed(_) => "would not compile",
        Ending::Abandoned => "abandoned",
        Ending::Posted => "posted",
        Ending::Running => "still running",
    }
}

/// What one log's replay says: it stopped following the run, or these
/// projection fields moved.
pub enum Verdict {
    Identical,
    /// The run names event ids literally, so it cannot be replayed into
    /// a log whose ids are its own. See [`Script::names_ids`].
    NamesIds,
    /// The replay made a different call, so it stopped being a replay.
    /// Everything downstream is its neighbour's value cascading.
    Drifted(String),
    Moved(Vec<Divergence>),
}

/// Replay one log file and say what moved.
pub fn replay_file(path: &std::path::Path) -> Result<Verdict, String> {
    let tree = crate::open_tree_read_only(path.to_str().ok_or("not a utf-8 path")?)?;
    let leaf = tree
        .list_leaves()
        .first()
        .map(|(l, _)| *l)
        .ok_or("the log has no branches")?;
    let script = script_of(&tree, leaf)?;
    let was = Said::of_branch(&tree, leaf);
    let now = run(&script);
    if let Some(why) = now.drift {
        // A run that named an id may have drifted *because* of it: the
        // fetch returned a neighbour's row, the program branched on
        // something else, and the next call is not the one recorded.
        // Attributing that to the harness would be wrong.
        return Ok(if script.names_ids {
            Verdict::NamesIds
        } else {
            Verdict::Drifted(why)
        });
    }
    let moved = compare(&was, &now.said);
    Ok(if moved.is_empty() {
        Verdict::Identical
        // **Tried first, explained second.** Naming an id is only a
        // problem if the ids actually failed to line up, and once the
        // reasoning is fed back they usually do. Short-circuiting on
        // the name alone gave up on 160 logs that mostly replay fine.
    } else if script.names_ids {
        Verdict::NamesIds
    } else {
        Verdict::Moved(moved)
    })
}

#[cfg(test)]
mod tests {
    /// **Every verb that takes an id is watched, in both spellings.**
    /// `keep` and `peek` were not, and this flag is what keeps a
    /// divergence from being charged to the harness — see
    /// `names_an_id`.
    #[test]
    fn a_literal_id_is_noticed_whichever_verb_and_spelling_names_it() {
        for verb in [
            "history.fetch",
            "fetch_history",
            "history.keep",
            "keep_history",
            "history.peek",
            "peek_history",
            "history.remove",
            "remove_history",
            "history.replace",
            "replace_history",
        ] {
            assert!(
                super::names_an_id(&format!("```js\n{verb}(6);\n```")),
                "{verb} takes an id and naming one literally must be noticed"
            );
        }
        // A value in hand is not a literal id, and must not be read as
        // one — that is the ordinary case and excusing it would hide
        // every real divergence behind it.
        for ok in [
            "history.keep(f)",
            "history.peek(f.id)",
            "history.fetch(row)",
        ] {
            assert!(!super::names_an_id(&format!("```js\n{ok};\n```")), "{ok}");
        }
    }

    use super::*;

    /// **A run replays to itself.** Build one through the harness, read
    /// its log back as inputs, feed those through a fresh harness, and
    /// the two projections agree — which is the whole premise: a log
    /// holds everything that went in.
    #[test]
    fn a_run_replays_to_the_same_conversation() {
        let mut c = Conversation::new();
        c.answers(
            "read_file",
            serde_json::json!({ "content": "NEW", "version": 1 }),
        );
        c.answers("bash", serde_json::json!({ "status": 0, "stdout": "ok\n" }));
        c.user("is PATH right, and does the check pass?");
        c.reply(
            "Reading it first.\n\n```js\nconst f = await tools.read_file(\"PATH\");\n\
             history.note({ says: f.content });\n```\n",
        );
        c.reply(
            "Now the check.\n\n```js\nconst r = await tools.bash(\"make check\");\n\
             console.log(r.stdout);\ntell(`PATH says NEW and the check ${r.status === 0 ? \"passes\" : \"fails\"}.`); finish();\n```\n",
        );

        let leaf = c.runner().spine.leaf_id;
        let script = script_of(c.tree(), leaf).expect("the log reads back as a script");
        assert_eq!(script.steps.len(), 3, "one post and two replies");

        let was = Said::of_branch(c.tree(), leaf);
        let now = run(&script);
        assert_eq!(now.drift, None, "the replay followed the run");
        assert_eq!(
            compare(&was, &now.said).len(),
            0,
            "{:#?}",
            compare(&was, &now.said)
        );
        assert_eq!(now.said.tells, ["PATH says NEW and the check passes."]);
        assert_eq!(now.said.ended, Ending::Finished(None));
    }

    /// A question to the person is an input like any other, and the
    /// answer replays into the expression that asked.
    #[test]
    fn an_answered_question_replays_as_the_answer() {
        let mut c = Conversation::new();
        c.user("A or B?");
        let r = c.reply(
            "```js\nconst pick = await choose(\"user\", \"which?\", [\"A\", \"B\"]);\n\
             tell(`picked ${pick}.`); finish();\n```\n",
        );
        c.answer(r.ask().call, serde_json::json!("B"));

        let leaf = c.runner().spine.leaf_id;
        let script = script_of(c.tree(), leaf).expect("a script");
        assert!(
            matches!(script.steps.last(), Some(Step::Answer(v)) if v == &serde_json::json!("B")),
            "{:?}",
            script.steps
        );
        let was = Said::of_branch(c.tree(), leaf);
        let now = run(&script);
        assert_eq!(now.drift, None, "the replay followed the run");
        assert_eq!(
            compare(&was, &now.said).len(),
            0,
            "{:#?}",
            compare(&was, &now.said)
        );
        assert_eq!(now.said.tells, ["picked B."]);
    }

    /// **The comparison has to be able to fail.** A projection that
    /// cannot report a difference is not evaluating anything, so this
    /// makes one on purpose.
    #[test]
    fn a_difference_is_reported_with_both_sides() {
        let mut a = Conversation::new();
        a.user("go");
        a.reply("```js\ntell(\"one\"); finish();\n```\n");
        let mut b = Conversation::new();
        b.user("go");
        b.reply("```js\ntell(\"two\"); finish();\n```\n");

        let was = Said::of_branch(a.tree(), a.runner().spine.leaf_id);
        let now = Said::of_branch(b.tree(), b.runner().spine.leaf_id);
        let moved = compare(&was, &now);
        assert_eq!(moved.len(), 1, "{moved:#?}");
        assert_eq!(moved[0].field, "tells");
        assert!(moved[0].was.contains("one") && moved[0].now.contains("two"));
    }

    /// **One log, in full.** The sweep says which field moved; this
    /// says what it moved to, for the one log worth looking at.
    ///
    ///     AGENT_REPLAY_LOG=path/to/run.jsonl \
    ///       cargo test -p agent replay_one_log -- --ignored --nocapture
    #[test]
    #[ignore = "needs AGENT_REPLAY_LOG"]
    fn replay_one_log() {
        let Ok(path) = std::env::var("AGENT_REPLAY_LOG") else {
            eprintln!("set AGENT_REPLAY_LOG to a log file");
            return;
        };
        let path = std::path::PathBuf::from(path);
        let tree = crate::open_tree_read_only(path.to_str().unwrap()).expect("a readable log");
        let leaf = tree
            .list_leaves()
            .first()
            .map(|(l, _)| *l)
            .expect("a branch");
        let script = script_of(&tree, leaf).expect("a script");
        println!("{} steps, {} calls", script.steps.len(), script.calls.len());
        if script.names_ids {
            println!(
                "NOTE: this run names event ids literally, so a replay's own ids do not \n                       line up and any divergence below may be that rather than behaviour."
            );
        }
        // Where the two id sequences part company — the fact that
        // decides whether naming an id could ever replay faithfully.
        let replayed_kinds = {
            let now = run(&script);
            now.said.kinds.clone()
        };
        let was_kinds = Said::of_branch(&tree, leaf).kinds.clone();
        if let Some(at) = was_kinds
            .iter()
            .zip(&replayed_kinds)
            .position(|(a, b)| a != b)
        {
            println!(
                "event sequences part at {at}: was {:?}, now {:?}",
                &was_kinds[at.saturating_sub(2)..(at + 3).min(was_kinds.len())],
                &replayed_kinds[at.saturating_sub(2)..(at + 3).min(replayed_kinds.len())]
            );
        } else {
            println!(
                "event sequences agree for {} events (was {}, now {})",
                was_kinds.len().min(replayed_kinds.len()),
                was_kinds.len(),
                replayed_kinds.len()
            );
        }
        let was = Said::of_branch(&tree, leaf);
        let now = run(&script);
        if let Some(why) = &now.drift {
            println!("\nDRIFTED: {why}");
        }
        for d in compare(&was, &now.said) {
            println!("\n── {} ──\n  was {}\n  now {}", d.field, d.was, d.now);
        }
        println!(
            "\nwas ending {:?}\nnow ending {:?}",
            was.ended, now.said.ended
        );
    }

    /// **The corpus sweep.** Ignored by default because it needs logs
    /// this repository does not carry, and because after a deliberate
    /// change its output is a *report* rather than a verdict — "47 logs
    /// now drop the cells after `finish`" is the thing to read, not 369
    /// red tests.
    ///
    ///     AGENT_REPLAY_DIR=~/.claude/jobs/*/tmp \
    ///       cargo test -p agent replay_the_corpus -- --ignored --nocapture
    #[test]
    #[ignore = "needs AGENT_REPLAY_DIR; prints a report rather than asserting"]
    fn replay_the_corpus() {
        let Ok(dir) = std::env::var("AGENT_REPLAY_DIR") else {
            eprintln!("set AGENT_REPLAY_DIR to a directory of logs");
            return;
        };
        let verbose = std::env::var("AGENT_REPLAY_VERBOSE").is_ok();
        let mut read = 0usize;
        let mut unreadable = 0usize;
        let mut moved: std::collections::BTreeMap<&'static str, usize> = Default::default();
        let mut examples: std::collections::BTreeMap<&'static str, String> = Default::default();
        let mut clean = 0usize;

        let mut stack = vec![std::path::PathBuf::from(dir)];
        let mut logs = Vec::new();
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    logs.push(path);
                }
            }
        }
        logs.sort();

        let mut drifted = 0usize;
        let mut id_bound = 0usize;
        for path in &logs {
            match std::panic::catch_unwind(|| replay_file(path)) {
                Ok(Ok(Verdict::Identical)) => {
                    read += 1;
                    clean += 1;
                }
                Ok(Ok(Verdict::NamesIds)) => {
                    read += 1;
                    id_bound += 1;
                }
                // A drift is one fact, not the eight fields that then
                // cascade off it. Counted on its own and never folded
                // in with them.
                Ok(Ok(Verdict::Drifted(why))) => {
                    read += 1;
                    drifted += 1;
                    if verbose {
                        println!("drift {:24} {}  {why}", "", path.display());
                    }
                    examples
                        .entry("drifted")
                        .or_insert_with(|| format!("{}\n           {why}", path.display()));
                }
                Ok(Ok(Verdict::Moved(diffs))) => {
                    read += 1;
                    if verbose {
                        let fields: Vec<&str> = diffs.iter().map(|d| d.field).collect();
                        println!("moved {:24} {}", fields.join(","), path.display());
                    }
                    for d in diffs {
                        *moved.entry(d.field).or_default() += 1;
                        examples.entry(d.field).or_insert_with(|| {
                            format!(
                                "{}\n           was {}\n           now {}",
                                path.display(),
                                brief_line(&d.was),
                                brief_line(&d.now)
                            )
                        });
                    }
                }
                Ok(Err(_)) => unreadable += 1,
                Err(_) => {
                    read += 1;
                    *moved.entry("panicked").or_default() += 1;
                    examples
                        .entry("panicked")
                        .or_insert_with(|| path.display().to_string());
                }
            }
        }
        println!(
            "\n{read} logs replayed, {unreadable} unreadable — {clean} identical, \
             {drifted} stopped following the run, {id_bound} name event ids"
        );
        if drifted > 0 {
            println!(
                "\n{drifted:6}  drifted\n        e.g. {}",
                examples["drifted"]
            );
        }
        for (field, n) in &moved {
            println!("\n{n:6}  {field} moved");
            println!("        e.g. {}", examples[field]);
        }
    }
}

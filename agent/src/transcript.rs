//! What happened, for the person who was not watching.
//!
//! `agent session --headless` prints events as they arrive, which is
//! the right thing for a machine reading a pipe and the wrong thing for
//! someone opening a finished log: every part, every settlement, every
//! console line, in the order the loop happened to produce them.
//! `agent document` prints the other extreme — the exact bytes the
//! model is about to read, which answers "what is it looking at" and
//! not "what did it do".
//!
//! This is the middle one, and it exists for **driving a session from
//! outside**. A person running the harness unattended needs three
//! things after each exchange: what the agent said to them, what it
//! actually did, and whether it is waiting on them. The last line here
//! is the one that matters — a branch waiting on an `ask()` and a
//! branch that has rested look identical in an event dump, and only one
//! of them wants you to type something.

use crate::types::{
    Address, Author, Call, Event, EventId, EventPayload, Handback, Origin, Outcome, Tree,
};

/// Longest a quoted line runs before it is cut. Generous — this is for
/// reading, not for a budget — but a `tell` carrying a pasted file
/// should not be the whole transcript.
const LINE_MAX: usize = 400;

/// Console lines shown per block, and how many of them come from the
/// end rather than the start.
const CONSOLE_ROWS: usize = 12;
const CONSOLE_TAIL_ROWS: usize = 3;

/// Render the branch that `leaf` sits on, oldest event first.
pub fn render(tree: &Tree, leaf: EventId) -> String {
    let path = tree.path_events(leaf);
    let mut out = String::new();
    for event in &path {
        if let Some(line) = line_for(tree, &path, event) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.push('\n');
    out.push_str(&waiting_on(&path));
    out.push('\n');
    out
}

/// One line — or a small block — per event, or `None` for the ones that
/// are bookkeeping rather than conversation.
///
/// **What is left out is the point.** `Part`s are the reply broken into
/// pieces and the pieces are already shown as what they *did*; a
/// `ReplyEnd` is a cost, which `agent score` is for; a `Result` is
/// folded into the call it settles, because a call and its answer are
/// one fact to a reader and two rows to the log.
fn line_for(tree: &Tree, path: &[&Event], event: &Event) -> Option<String> {
    let id = event.id.as_u64();
    Some(match &event.payload {
        EventPayload::Agent { charter, .. } => {
            format!("#{id} agent  «{}»", clip(charter.trim()))
        }
        EventPayload::Fork { name } => match name {
            Some(n) => format!("#{id} fork   «{n}»"),
            None => format!("#{id} fork"),
        },
        EventPayload::Post { from, origin } => {
            let resolved = tree.resolve(origin);
            let text = resolved
                .direct()
                .map(|(t, _, _)| t.to_owned())
                .unwrap_or_else(|| "(message body unavailable)".into());
            let who = match from {
                Author::User => "you",
                Author::Harness => "harness",
                Author::Agent(_) => "agent",
            };
            // A post carrying a `Send` is that send arriving; the send
            // itself already printed on the other branch.
            let arrow = if matches!(origin, Origin::Sent(_)) {
                "»"
            } else {
                "→"
            };
            format!("#{id} {who:<7}{arrow} {}", clip(text.trim()))
        }
        // **The harness asking for room, not the person asking for
        // work.** A compaction inserts a reply that is about the
        // document rather than about the task, and leaving it out made
        // that reply look like the agent wandering off — the one thing
        // a driver reading this most needs not to misread.
        EventPayload::Compaction {
            measured,
            limit,
            unit,
        } => format!(
            "#{id} harness→ the conversation is {measured} {unit:?} against {limit}; \
             make room",
        ),
        EventPayload::Reply => format!("#{id} reply"),
        EventPayload::Restart => format!("#{id} restart"),
        EventPayload::Call(call) => {
            let settled = settlement(path, event.id);
            match call {
                // Prose is the agent talking; it reads as speech, not
                // as a call, whatever the log calls it.
                Call::Send {
                    prose: true, text, ..
                } => format!("       ┆ {}", clip(text.trim())),
                Call::Send {
                    to,
                    text,
                    expects_reply,
                    ..
                } => {
                    let mark = match (expects_reply, to) {
                        (true, _) => "?",
                        (false, Address::User) => "!",
                        (false, _) => "»",
                    };
                    format!("#{id}    {mark} {}", clip(text.trim()))
                }
                Call::Invoke { name, args, .. } => {
                    format!(
                        "#{id}    → {name}({}) {}",
                        clip_args(args),
                        outcome_tag(settled)
                    )
                }
                Call::Spawn { charter, .. } => {
                    format!("#{id}    ✳ spawn «{}»", clip(charter.trim()))
                }
                Call::Fork { .. } => format!("#{id}    ✳ fork"),
            }
        }
        EventPayload::Note { value, .. } => {
            format!("#{id}    ▸ {}", clip(&crate::machine::note_text(value)))
        }
        EventPayload::Console { lines } if !lines.is_empty() => console_block(lines),
        EventPayload::Answer { question, value } => {
            format!(
                "#{id}    ✓ answered #{}: {}",
                question.as_u64(),
                clip(&value.to_string())
            )
        }
        EventPayload::Compacted { of, text, window } => {
            let how = match (text, window) {
                (Some(_), _) => "replaced",
                (None, Some(w)) => {
                    return Some(format!(
                        "#{id}    ✂ #{} → window {}..{}",
                        of.as_u64(),
                        w.from,
                        w.to
                    ));
                }
                (None, None) => "removed",
            };
            format!("#{id}    ✂ #{} {how}", of.as_u64())
        }
        // **Finishing and handing on are one ending with a flag on
        // it** — `finish()` says the task is done, and says nothing
        // about how the program ended. What it returned, if anything,
        // is the other half and reads on the same line.
        EventPayload::Handback { how, .. } => match how {
            Handback::Completed { value, rested } => {
                let mark = if *rested {
                    "✓ finished"
                } else {
                    "⏎ handed on"
                };
                match value {
                    Some(v) => format!("       {mark} → {}", clip_args(v)),
                    None => format!("       {mark}"),
                }
            }
            Handback::Raised { name, .. } => format!("#{id}    ⏸ raised «{name}»"),
            Handback::Trapped { kind, message, .. } => {
                format!("#{id}    ✗ trapped {kind}: {}", clip(message))
            }
            Handback::CellFailed { message } => {
                format!("#{id}    ✗ would not compile: {}", clip(message))
            }
            Handback::Abandoned => format!("#{id}    ⏏ abandoned"),
            Handback::Superseded => format!("#{id}    ⏏ superseded"),
            Handback::Posted { .. } => format!("#{id}    ⏸ a message arrived"),
            // **The process died and took the VM with it.** A program
            // parked on an `ask()` or mid-call does not survive the
            // process that holds it; the next one to open the log
            // reconciles it into this. Worth its own line, because it
            // is the one ending that means "nobody decided anything" —
            // and because a driver that sees it knows the answer it is
            // about to give will arrive as a notice rather than into
            // the expression that asked.
            Handback::Interrupted => {
                format!("#{id}    ⏻ interrupted — the run went with the process")
            }
        },
        _ => return None,
    })
}

/// What a program printed, bounded by **rows** as well as by width.
///
/// `cap_console` already bounds what the *model* is shown, and the log
/// holds that bounded version — but "bounded" there is two hundred
/// lines, which is the right budget for a model reading a report and
/// the wrong one for a person scanning a conversation. A run that
/// cats a file puts the whole file here, and a transcript that scrolls
/// for a screen and a half has stopped being the readable view it
/// exists to be.
///
/// Head and tail, because both ends carry: the first lines say what
/// the program set out to print and the last ones usually say how it
/// went.
fn console_block(lines: &[String]) -> String {
    let render = |l: &String| format!("       · {}", clip(l));
    if lines.len() <= CONSOLE_ROWS {
        return lines.iter().map(render).collect::<Vec<_>>().join("\n");
    }
    let head = CONSOLE_ROWS - CONSOLE_TAIL_ROWS;
    let mut out: Vec<String> = lines[..head].iter().map(render).collect();
    out.push(format!(
        "       ·   … {} more lines",
        lines.len() - CONSOLE_ROWS
    ));
    out.extend(lines[lines.len() - CONSOLE_TAIL_ROWS..].iter().map(render));
    out.join("\n")
}

/// The closing line, and the only one a driver has to read: whether
/// this branch wants something from the person.
///
/// Three states, and they are not three shades of the same one. A
/// branch waiting on an `ask()` cannot move until someone answers; a
/// branch that rested is finished with the task; a branch that merely
/// ran out of turn will carry on by itself when it is next prompted.
fn waiting_on(path: &[&Event]) -> String {
    if let Some(ask) = open_ask(path) {
        let EventPayload::Call(Call::Send { text, options, .. }) = &ask.payload else {
            unreachable!("open_ask returns a Send")
        };
        let choices = if options.is_empty() {
            String::new()
        } else {
            format!("  [{}]", options.join(", "))
        };
        return format!(
            "waiting on you: #{}  {}{choices}\n  answer it: agent session --headless --real \
             --turn '…' <log>",
            ask.id.as_u64(),
            clip(text.trim())
        );
    }
    match path.iter().rev().find_map(|e| match &e.payload {
        EventPayload::Handback { how, .. } => Some(how),
        _ => None,
    }) {
        Some(Handback::Raised { name, .. }) => {
            format!("waiting on: a handler for «{name}»")
        }
        // **A parked program is not a finished one.** A message landing
        // mid-run suspends it (rule B) exactly as a raise does, and it
        // sits there until a reply decides whether to carry on or write
        // something else. Reading `nothing` here is how a driver
        // concludes a run is over when it is holding a program open.
        Some(Handback::Posted { .. }) => {
            "waiting on: the next reply, to resume the parked program or replace it".to_owned()
        }
        // **Every non-terminal handback is holding a program open**, and
        // the `Posted` note above is the general case: a trap or a cell
        // that would not compile leaves the branch suspended with a
        // repair owed, and read `nothing` here. Asking `is_terminal`
        // rather than naming three more variants is also what keeps a
        // variant added later from silently defaulting to "nothing".
        Some(h) if !h.is_terminal() => {
            "waiting on: the next reply, to repair the stopped program or replace it".to_owned()
        }
        Some(Handback::Interrupted) => {
            "waiting on: nothing — the last run went with its process, so say it again".to_owned()
        }
        // **A post with no reply under it is not a rest.** The fold
        // above reads only the newest `Handback`, so a message that
        // landed *after* the last program ended left this line saying
        // `nothing` while the branch owed a turn — which is the exact
        // way the `Posted` arm above says a driver comes to conclude a
        // run is over. Seen in `lab/live1` on 2026-09-21: the person
        // said "ok fine, go ahead and do the other three after all",
        // and the only line a driver is told to read said the branch
        // wanted nothing.
        _ => match unanswered_post(path) {
            Some(id) => format!(
                "waiting on: nothing — #{} landed after the last reply and has not been \
                 taken up yet",
                id.as_u64()
            ),
            None => "waiting on: nothing".to_owned(),
        },
    }
}

/// The newest `Post` on this path with no `Reply` logged after it — the
/// branch has been spoken to and has not started answering.
fn unanswered_post(path: &[&Event]) -> Option<EventId> {
    let last_reply = path
        .iter()
        .rev()
        .find(|e| matches!(e.payload, EventPayload::Reply))
        .map(|e| e.id);
    path.iter()
        .rev()
        .find(|e| matches!(e.payload, EventPayload::Post { .. }))
        .filter(|post| last_reply.is_none_or(|r| post.id > r))
        .map(|e| e.id)
}

/// The `Send { to: user, expects_reply }` on this path with no `Result`
/// — the same fold the session's own inbox uses, over a log rather than
/// over live state, so a finished log answers the question too.
fn open_ask<'a>(path: &[&'a Event]) -> Option<&'a Event> {
    path.iter().rev().copied().find(|e| {
        matches!(
            &e.payload,
            EventPayload::Call(Call::Send {
                to: Address::User,
                expects_reply: true,
                ..
            })
        ) && settlement(path, e.id).is_none()
    })
}

fn settlement<'a>(path: &[&'a Event], call: EventId) -> Option<&'a Outcome> {
    path.iter().rev().find_map(|e| match &e.payload {
        EventPayload::Result { call: c, outcome } if *c == call => Some(outcome),
        _ => None,
    })
}

/// How a call turned out, in the smallest thing that is still an
/// answer. A `status` is what a `bash` result is read for and nothing
/// else in the value usually matters; anything else gets `ok`, because
/// the value itself is what `history.fetch` is for.
fn outcome_tag(outcome: Option<&Outcome>) -> String {
    match outcome {
        None => "…".to_owned(),
        Some(Outcome::Failed(why)) => format!("✗ {}", clip(why)),
        Some(Outcome::Delivered(v)) => match v.get("status").and_then(|s| s.as_i64()) {
            Some(0) => "ok".to_owned(),
            Some(n) => format!("status {n}"),
            None => "ok".to_owned(),
        },
    }
}

fn clip_args(args: &serde_json::Value) -> String {
    let rendered = match args.as_array() {
        Some(items) => items
            .iter()
            .map(|a| match a.as_str() {
                Some(s) => format!("{s:?}"),
                None => a.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => args.to_string(),
    };
    clip(&rendered)
}

/// One line, bounded. Newlines become `⏎` so a multi-line value stays
/// one row of the transcript — the shape is what is being read here,
/// and a value that needs its own screen has an id to fetch it by.
fn clip(text: &str) -> String {
    let flat = text.replace('\n', " ⏎ ");
    if flat.chars().count() <= LINE_MAX {
        return flat;
    }
    let head: String = flat.chars().take(LINE_MAX).collect();
    format!("{head}… ({} chars)", flat.chars().count())
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    let Some(path) = args.first() else {
        return Err("usage: agent transcript <log.jsonl> [branch-id]".into());
    };
    let tree = crate::open_tree_read_only(path)?;
    let leaf = match args.get(1) {
        Some(raw) => {
            let n: u64 = raw
                .trim_start_matches('#')
                .parse()
                .map_err(|_| format!("not an event id: {raw}"))?;
            let id = EventId::checked(n).ok_or("event ids start at 1")?;
            // Any id on the branch will do — what is rendered is the
            // path down to that branch's newest leaf.
            tree.list_leaves()
                .into_iter()
                .map(|(l, _)| l)
                .find(|l| tree.path_events(*l).iter().any(|e| e.id == id))
                .ok_or_else(|| format!("#{n} is not on any branch of this log"))?
        }
        None => tree
            .list_leaves()
            .first()
            .map(|(l, _)| *l)
            .ok_or("the log has no branches to render")?,
    };
    print!("{}", render(&tree, leaf));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Conversation;

    /// **A message with no reply under it is not a rest.** The fold
    /// reads only the newest `Handback`, so a post that landed after
    /// the last program ended used to leave this line saying `nothing`
    /// while the branch owed a turn — the exact misreading the
    /// `Posted` arm exists to prevent one level up. Seen in
    /// `lab/live1` on 2026-09-21.
    #[test]
    fn a_post_with_no_reply_under_it_is_named() {
        let mut c = Conversation::new();
        c.user("is it green?");
        c.reply("Yes.\n\n```js\ntell(\"green.\"); finish();\n```\n");
        let settled = render(c.tree(), c.runner().spine.leaf_id);
        assert!(settled.ends_with("waiting on: nothing\n"), "{settled}");

        c.user("ok, now do the other three");
        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(
            out.contains("has not been taken up yet"),
            "a post nobody has replied to reads as a rest: {out}"
        );
    }

    /// A trap leaves the branch suspended with a repair owed, exactly as
    /// a rule-B pause does — and used to read `nothing` because the fold
    /// named `Raised` and `Posted` and defaulted the rest.
    #[test]
    fn a_trapped_program_is_still_holding_the_branch() {
        let mut c = Conversation::new();
        c.user("write it");
        c.reply("```js\nnope.missing();\n```\n");
        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(
            out.contains("to repair the stopped program or replace it"),
            "{out}"
        );
    }

    /// The shape of it, over a run that reads, says something and
    /// finishes: what it said, what it ran, how it ended, and that
    /// nobody is being waited on.
    #[test]
    fn a_finished_run_reads_as_what_it_did() {
        let mut c = Conversation::new();
        c.answers("bash", serde_json::json!({ "status": 0, "stdout": "ok\n" }));
        c.user("is it green?");
        c.reply(
            "Running the check.\n\n```js\nconst r = await tools.bash(\"make check\");\n\
             tell(r.status === 0 ? \"green.\" : \"not green.\"); finish();\n```\n",
        );

        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(out.contains("you    → is it green?"), "{out}");
        assert!(out.contains("┆ Running the check."), "{out}");
        assert!(out.contains("→ bash(\"make check\") ok"), "{out}");
        assert!(out.contains("! green."), "{out}");
        assert!(
            out.contains("✓ finished"),
            "finishing and handing on read differently: {out}"
        );
        assert!(out.ends_with("waiting on: nothing\n"), "{out}");
    }

    /// **The line a driver actually reads.** A branch parked on an
    /// `ask()` says so, names the id to answer, and shows the closed
    /// set of answers when there is one — the difference between a
    /// session that is finished and one that is waiting for you is
    /// invisible in an event dump, and it is the whole reason to look.
    #[test]
    fn a_branch_waiting_on_a_person_says_so_and_names_the_id() {
        let mut c = Conversation::new();
        c.user("A or B?");
        let r = c.reply(
            "```js\nconst pick = await choose(\"user\", \"which one?\", [\"A\", \"B\"]);\n\
             tell(`picked ${pick}.`); finish();\n```\n",
        );

        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(
            out.contains(&format!("waiting on you: #{}", r.ask().call.as_u64())),
            "{out}"
        );
        assert!(out.contains("which one?  [A, B]"), "{out}");

        // And once it is answered, it is not waiting any more.
        c.answer(r.ask().call, serde_json::json!("B"));
        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(out.contains("waiting on: nothing"), "{out}");
        assert!(out.contains("! picked B."), "{out}");
    }

    /// **A parked program is not a finished one.** A message arriving
    /// mid-run suspends the program (rule B); the branch then owes a
    /// reply that either resumes it or writes something else, and a
    /// closing line reading `nothing` is how a driver concludes the run
    /// is over while it is holding a program open.
    ///
    /// Seen live on 2026-09-20: a `bash` call still in flight, the
    /// program suspended on an arriving message, and the transcript
    /// said nothing was waiting.
    #[test]
    fn a_program_parked_by_a_message_says_what_it_owes() {
        let mut c = Conversation::new();
        c.user("go");
        // A cell that blocks on a tool, so a post can land mid-run.
        c.never_answers("scan");
        c.allow(crate::testkit::Invariant::CallsSettle);
        c.chunk("```js\nconst r = await tools.scan();\ntell(r.out); finish();\n```\n");
        c.harness("something arrived");
        c.end_reply();

        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(out.contains("⏸ a message arrived"), "{out}");
        assert!(
            out.contains("waiting on: the next reply"),
            "the branch owes a decision, and says so: {out}"
        );
    }

    /// **A program that ends itself reads as a decision, not a fault**,
    /// and what it returned is on the same line — which is the whole of
    /// what it said to the next reply. A trap renders as `✗`; this is
    /// the model noticing its own check fail and saying so.
    #[test]
    fn a_return_reads_as_a_decision() {
        let mut c = Conversation::new();
        c.answers(
            "bash",
            serde_json::json!({ "status": 1, "stdout": "2 failed\n" }),
        );
        c.user("is it green?");
        c.reply(
            "```js\nconst r = await tools.bash(\"make check\");\n\
             if (r.status !== 0) return `CHECK fails: ${r.stdout}`;\ntell(\"green.\"); finish();\n```\n",
        );

        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(out.contains("→ bash(\"make check\") status 1"), "{out}");
        assert!(out.contains("⏎ handed on → \"CHECK fails:"), "{out}");
        assert!(!out.contains("✗"), "a decision is not a fault: {out}");
    }

    /// **A program that cats a file does not take the transcript with
    /// it.** The log holds what the model was shown, which is two
    /// hundred lines — the right budget for a model reading a report
    /// and the wrong one for a person scanning a conversation.
    #[test]
    fn a_long_print_is_bounded_at_both_ends() {
        let mut c = Conversation::new();
        c.user("go");
        c.reply("```js\nfor (let i = 1; i <= 60; i++) console.log(`line ${i}`);\n```\n");

        let out = render(c.tree(), c.runner().spine.leaf_id);
        let printed: Vec<&str> = out.lines().filter(|l| l.contains(" · ")).collect();
        assert_eq!(printed.len(), CONSOLE_ROWS + 1, "{printed:#?}");
        assert!(
            printed[0].contains("line 1"),
            "the start is kept: {}",
            printed[0]
        );
        assert!(
            printed[CONSOLE_ROWS - CONSOLE_TAIL_ROWS].contains("… 48 more lines"),
            "and it says how much is missing: {}",
            printed[CONSOLE_ROWS - CONSOLE_TAIL_ROWS]
        );
        assert!(
            printed.last().is_some_and(|l| l.contains("line 60")),
            "and the end, which is usually how it went: {:?}",
            printed.last()
        );
    }

    /// **A compaction is something that happened.** It inserts a reply
    /// that is about the document rather than about the task, and
    /// leaving it out of the transcript made that reply look like the
    /// agent wandering off — which is the one thing a driver reading
    /// this most needs not to misread.
    #[test]
    fn a_compaction_says_why_the_next_reply_is_not_about_the_task() {
        let mut c = Conversation::new();
        c.user("go");
        c.reply("```js\nhistory.append(\"something\");\n```\n");
        c.log(EventPayload::Compaction {
            measured: 40_000,
            limit: 32_000,
            unit: crate::types::Measure::Bytes,
        });

        let out = render(c.tree(), c.runner().spine.leaf_id);
        assert!(
            out.contains("40000 Bytes against 32000"),
            "the numbers it fired on: {out}"
        );
        assert!(out.contains("make room"), "and what was asked for: {out}");
    }

    /// A value that would take the screen is one line with an id beside
    /// it: the transcript says the shape, and `history.fetch` says the
    /// rest.
    #[test]
    fn a_long_value_is_one_line_with_its_size() {
        let mut c = Conversation::new();
        c.user("go");
        c.reply("```js\nhistory.append(\"x\".repeat(5000));\n```\n");

        let out = render(c.tree(), c.runner().spine.leaf_id);
        let row = out
            .lines()
            .find(|l| l.contains(" ▸ "))
            .expect("the row is in the transcript");
        assert!(
            row.len() < LINE_MAX + 60,
            "one line, bounded: {} chars",
            row.len()
        );
        assert!(
            row.contains("chars)"),
            "and it says how much there is: {row}"
        );
    }
}

# Phase 27 — one history, four verbs, and a menu that is an index

Everything here comes out of one observation, measured on 2026-09-17:
**the largest thing in a document is the thing nobody chose.**

A completion report ends with an artifact menu — every call the program
made, its arguments clipped, *and its result clipped*. On a real
`dead-code-sweep` run that menu was **9,901 bytes of a 36,821-byte
document: 27%**, and the biggest item after the card. It did not matter
much while a completed program ended the conversation, because nobody
read it. Under automatic continuation (27.1) it lands in front of every
program that follows, and compounds.

Three facts about it, each discovered by asking what a program actually
sees:

- **A return value does not replace it.** `CompletionReport::render`
  emits the return preview, then the console, then the menu, and the
  menu is built without consulting the value. A program that carefully
  distils its findings hands the next writer the distillation *and* the
  firehose, with the firehose winning on volume. `return` is advisory;
  the replay is mandatory.
- **Compaction cannot touch it.** `document::label_of` names `turn`,
  `post`, `note`, `fork`, `return`, `condition` — and falls through to
  the literal `"event"` for everything else, which includes every `Call`
  and every `Result`. Phase 26's `NotARow` guard refuses those by name.
  So the one thing a compaction program cannot remove is the largest
  thing in the document.
- **Its content is unrecoverable once compacted — for the rows that are
  not calls.** `artifact(id)` serves a `Result`, a settled `Call`, a
  `Return` and a `Console`, and refuses the rest. `compaction.rs`'s
  header promises it "never drops an id — only content", and the
  compaction request tells the model "nothing is deleted … its
  artifacts stay fetchable by it". For a compacted `Post` there is no
  artifact. We have been telling the model something false.

## The shape

**One history.** Not a transcript plus an artifact store: a single list
of rows, each with an id and a label, some of which happen to be calls.
The menu is not a separate compartment; it is those rows, rendered.

**Four verbs over it**, one job each:

    append_history(value)              add a row — for your own later turns
    fetch_history(id)                  read one back, whole — logs nothing
    rewrite_history(id, label, value)  shorten one
    remove_history(id, label)          drop one's content, keep its id

**Three channels out of a program**, one audience each:

    tell(text)        the person. Rare, and only what a person must read.
    append_history()  your own later turns. Where piecemeal emission goes.
    return value      the next program. Work product and continuation, one act.
    done()            the task is over.

`tell` has been the dumping ground — `tell(f.content)` appears in live
runs — because it was the only channel that obviously *went* somewhere.
Giving the other impulses their own destination is the point.

**Reading is free; only deliberate acts add.** `fetch_history` is
answered from the log and logs nothing, so a value reaches the
*program* without entering the *document*. That is what lets the menu
be an index: you see enough to decide, you fetch what you want, and
nothing you fetched is inflicted on the program after you.

It is also why the index must carry a headline and not just an id. If
fetching is free but blind, the model must fetch to find out whether it
wanted the thing — paying in round trips what it saved in tokens.
`[#19] bash("cargo check 2>&1")` decides without fetching;
`[#19] call` does not.

## Steps

**27.1 — a program continues by default; `done()` ends the task.**
*(in progress)* `finish_program` stops advancing `shown` past the
`Return`, so completing prompts again; a new bare verb `done()` is the
only thing that rests the branch. Rationale: the dominant observed
failure across every card variant and both models is a program that
does one step and stops, abandoning the task. With termination as an
omission — falling off the end — the easiest accident causes the most
expensive failure. Making it an act inverts that: an accidental
continuation costs one visible, self-correcting turn.
Gate: `a_completed_program_continues_by_default`,
`done_ends_the_conversation`, and Program-mode behaviour unchanged
where `done()` is called.

**27.2 — the menu becomes an index.** `ArtifactState::Delivered`
renders without the value, and the label's argument preview is clipped
hard, because `replace_file(["src/lib.rs", "<the whole file>"])` is a
result in all but name. `Failed` keeps its message: a failure is
precisely what was not chosen and must still be seen.
Gate: a report for a run with N calls is under a fixed small bound
regardless of how large those calls' results were; a failed call's
error text still appears; `artifact(id)` still fetches the full value.

**27.3 — the completion report is a row.** *(done, and not what this
step said.)* The plan here was to give `label_of` a `call` and a
`result` so a menu row could be compacted under its own id. Reading the
renderer says that is the wrong shape: a `Call` has no line anywhere.
It is rendered *inside* the completion report that `derive_report`
builds around the `Return` closing its program, and that report is one
row, already named `return`.

What was actually broken was worse, and one arm away. `render_with_
lookup` consulted the compaction lookup for `Turn`s and for everything
`pending_line` handles — but **not** for `Return` or `Condition`, whose
arms called `derive_report` unconditionally. So the checksum accepted
`remove_history(id, "return")`, the dry run re-rendered the report in
full, the batch came back the same size, and it was refused for freeing
nothing. Two live compaction programs on 2026-09-16 did correct work
and were told "compact more of it and return again"; the same shape as
the label-mismatch bug, and found the same way.

Both arms now go through one `report_line`, which checks the shadow
first exactly as `pending_line` does. `Call` and `Result` keep the
`"event"` fallback and stay `NotARow` — the honest answer once the row
that contains them is reachable.
Gate: `compacting_a_return_removes_the_report_rendered_around_it` (the
freed bytes are the report's, not "some"), and
`a_call_is_not_a_row_because_the_report_around_it_is`.

**27.4 — `artifact` becomes `fetch_history`, and reaches every row.**
The rename is only honest with the extension: fetching a compacted
`Post` must return its original content, or the promise in 27's opening
stays false. Wide but mechanical — the verb appears in the card, the
exemplars, every report header, `machine.rs`'s dispatch, and a number
of tests. Its own commit.
Gate: `fetch_history` returns a `Post`'s text; returns a compacted
row's *original* content; still logs nothing (assert no new events).

**27.5 — the card states the four channels**, and says plainly that
`tell` reaches a person and nothing else does.

**27.6 — compaction prefers by lifetime.** *(done, with 27.3.)* Folded
in there because once the report became reachable the old sentence —
"the largest tool results you have already acted on are the first
targets" — was pointing at things that are not rows. The order the
request now states: `return` first (a whole program's report, whose
calls stay fetchable by their own ids), then `turn`, then `note` and
`post`, and never the task. A sort key in the compaction request, not
a mechanism.

## What this is measured against

`short` on the four-task set at n=7 — 19/28, 51KB reasoning, 20.8k
input tokens — and pi on the same set, 21/28 (21/21 on the three tasks
it can attempt; it cannot ask, so `ambiguous-config` is beyond it),
14KB reasoning, 75.7k input tokens.

The prediction 27.1 makes: `short`'s `ambiguous-config` failures are
all premature stops, so automatic continuation should move it toward
`mid`'s 7/7 without giving back throughput. The prediction 27.2 makes:
input tokens fall further, because 27% of every document stops being
replay.

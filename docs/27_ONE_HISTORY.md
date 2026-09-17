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
  not calls.** *(fixed in 27.4.)* `artifact(id)` served a `Result`, a
  settled `Call`, a
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
*(done.)* The rename is only honest with the extension: fetching a
compacted `Post` must return its original content, or the promise in
27's opening stays false.

The extension needed no compaction-aware code, which is the part worth
keeping. Compaction never touches its target — it appends a
`Compacted` event that only the *renderer* consults. The fetch reads
the log, so it reads the original, for free. The document shrinks; the
history does not.

The menu's own title went with it: `## new artifacts` named a
compartment that no longer exists, and it is now `## new rows — fetch
any of them with fetch_history(id)`, in the vocabulary the compaction
request already used ("each row above carries its `[id]` and its
label").
Gate: `fetch_history_reads_a_compacted_post_back_whole` (the original
text, and the program's three events with nothing added for the fetch),
`fetch_history_reads_a_note_and_a_program_back`.

**27.5 — the card states the four channels.** *(done.)* This was
listed as a wording change and is not one: after 27.1 the shipped card
taught the opposite of what the code does. It said "a bare `return` —
or simply running off the end — ends the *conversation*", listed
`next_program` as the one correct ending, and told the model "a root
program's return value is read by nobody". Every one of those is now
false, and the last two are false in the direction that causes the
failure 27.1 exists to prevent.

The card now opens on the four channels, and the sentence before them
is the one that matters: **a program finishing is not the task
ending.** `next_program` is gone from every card — `return value` does
exactly what it did, and two verbs for one act is the thing to remove,
not to document. It stays in the interpreter, unmentioned; nothing
reaches for a verb the card does not name.

`next_program` is gone from the interpreter too, now that a
measurement has run without it: zero uses across 82 live runs once no
card named it. With it went its reserved condition name, its `Handover`
special case in `machine.rs`, its own branch of the completion report,
and `score`'s `handovers` field — which counted exactly that verb and
would otherwise have gone on reporting 0 forever. A handover is a
`Return` the next program reads, and `programs` counts those.

`tell` is stated as the person's channel and nothing else's, with the
cadence that follows from "rare": the first program says what is about
to happen, the finishing one says what the answer was, the ones between
usually say nothing. The old card asked for a `tell` "as things
actually happen", which is where `tell(f.content)` came from.

Every exemplar was rewritten to end on purpose — `done()` where the
task finishes, `return` where it hands on — in the shipped card and in
all five eval variants, since a variant that teaches falling off the
end now measures the accident rather than the card.
Gate: `every_exemplar_ends_on_purpose`, which reads the ending off a
real VM run rather than grepping for the word, and asserts *not both*
(a value returned beside `done()` is read by nobody);
`every_eval_card_variant_ends_on_purpose`, textual, because `sketch`
exists to test exemplars-as-shape and its programs deliberately do not
run.

**27.6 — compaction prefers by lifetime.** *(done, with 27.3.)* Folded
in there because once the report became reachable the old sentence —
"the largest tool results you have already acted on are the first
targets" — was pointing at things that are not rows. The order the
request now states: `return` first (a whole program's report, whose
calls stay fetchable by their own ids), then `turn`, then `note` and
`post`, and never the task. A sort key in the compaction request, not
a mechanism.

**27.7 — the return value arrives whole.** *(done. Found by running
27.1–27.5 and reading the programs, not by planning.)*
`CompletionReport::render` gave the return value the same 256-byte
preview a menu row gets, on the rule that nothing enters a context
unchosen. That was sound while a `return` was read by nobody — it was
then just another artifact. 27.1 made it the channel the whole design
runs on, and nothing re-read the rendering.

What it looked like in a live `ambiguous-config` run: program 1 reads
`deploy.yaml` and returns `{question, content: <the file>}`. Program 2
sees 256 bytes of it, says *"Reading the whole file — the last look was
cut off"*, and reads the file again. So does program 3. Program 4
finally acts — by then over the task's 3-program cap, and it guessed
rather than asking. Four programs, three of them re-fetching what the
first had already handed them, each behaving reasonably given what it
could see.

The "unchosen" rule is not violated by rendering it whole and never
was: the author of the value and the reader of the report are the same
mind one turn apart, and the author picked it deliberately over
everything else it was holding. That is what chosen means. The document
now has **exactly one generous channel and it is the chosen one** — the
menu is an index, a call's arguments are clipped, a result is a size,
and the thing a program deliberately addressed to its successor arrives
intact. `RETURN_MAX_BYTES` (8 KB) bounds the pathological case and
`clip_answer` names the id, so the remainder is a `fetch_history` away
rather than lost.

The other half of that run was the card's, not the renderer's: the
model returned *the question* instead of answering it, because the card
described the next program as somebody else ("the next writer", "the
program after this one"). It is not somebody else. Every card now says
so: **you are the next writer**, what you return comes back to you
read, so a question you return is one you will be answering.
Gate: `a_return_value_reaches_the_next_program_whole`,
`an_absurd_return_value_is_bounded_and_says_where_the_rest_is`.

## What running it found

Three of the four things in this phase that mattered were not in the
plan above. They came out of running 27.1–27.5 and reading the
programs, and they have a shape in common: **each was a rule the card
asked the model to remember, standing in for a property the harness
could have had.**

**A truncated channel** (27.7, above). The card could not have fixed
this one — the model was reading 256 bytes and saying so.

**A pipeline's status was its last stage's.** The card carried a
paragraph asking every program to write `set -o pipefail`. A
`skipped-tests` run forgot, ran `python3 -m unittest … | tail -5`
against a file its own edit had left syntactically broken, read
`tail`'s 0, kept the change, reverted nothing, and reported a clean
sweep. Every verdict in that loop was `tail` succeeding. `bash` runs
with `pipefail` now and the paragraph is gone, along with the ritual
prefix in eight exemplars. An instruction the model must remember at
every call site is worse than a fact about the tool it must know once
— and this is *our* bash, with no compatibility contract to keep.

Turning it on without checking what the statuses actually are would
have introduced a worse lie: `grep … | head -40` that truncates leaves
`grep` killed by SIGPIPE, so one of the commonest idioms a program
writes would have reported failure on success. 141 normalises to 0,
and nothing is hidden by it, because `pipefail` takes the *rightmost*
non-zero stage.

**"It ran and said no" and "it never ran" shared a channel.** Asked
whether `bash` should reject on a non-zero status: no — non-zero is the
*information* in almost everything we run, and making it an exception
turns every probe loop into a try/catch around expected control flow.
But 126/127 are bash saying it could not execute the command at all,
which is the event "could not spawn bash" is, one level down, and
`run_bash` already returned `Err` for that. Resolved, they were
`{status: 127, stdout: ""}` — and a program reading stdout saw nothing
and concluded there was nothing to find.

### And two about the apparatus, not the agent

`short`'s exemplars 11 and 12 **were** the `skipped-tests` task —
exemplar 11's user turn read "some tests here are skipped — which ones
pass now?" against the eval's "Some tests here are skipped. Work out
which ones actually pass now." Both are rewritten onto a neutral
surface. `short` scored 3/7 on that task with the answer sitting in its
own card, which is its own kind of finding.

A suite reads the binary and the card off disk at spawn time, so a
`cargo build` or a card edit fourteen minutes into an n=7 run silently
splits it in two and the aggregate averages two systems. That happened
here and the run was discarded. Runs go from a `git worktree` pinned to
the commit under test now, so editing cannot reach them; and the driver
hashes the binary and the card before and after and says so loudly if
they moved, because a worktree prevents the mistake only while someone
remembers to use one.

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

## What it measured — and the correction that came with it

**n=7 per task does not resolve what we argue from.** Two suites of the
identical configuration — same commit, same card, provenance hashes
matching — came back **26/28 and 21/28**. A five-point swing from
nothing but sampling, and larger than most of the changes this
apparatus has been used to justify. The first of those was reported as
"the number" before the second existed. `drive.py --pool` exists now:
it sums suites of one configuration, prints a Wilson interval, and
refuses to pool suites whose stamps disagree.

Pooled over two suites at the same commit:

| | all four | the three pi can attempt |
|---|---|---|
| `short` + 27 | **47/56, 84%** (72–91%) | 35/42, 83% (69–92%) |
| pi | 21/28, 75% (57–87%) | **21/21, 100%** (85–100%) |
| `short`, pre-27 | 19/28, 68% (49–82%) | 16/21, 76% (55–89%) |

Per task, ours against pi: `ambiguous-config` **12/14 vs 0/7**,
`plain-question` 14/14 vs 7/7, `dead-code-sweep` 11/14 vs 7/7,
`skipped-tests` 10/14 vs 7/7.

**So the whole-set lead is entirely the task pi cannot attempt.** On
the work both can do, the tool loop is still ahead, and saying "26/28
beats 21/28" hides that. What 27 actually bought is `ambiguous-config`
3/7 → 12/14 and `skipped-tests` 3/7 → 10/14; what it has not bought is
parity on per-item probe loops, where pi's many small checked steps
still beat our few large ones.

Cost, per run, ours against pi: 61s against 19s of provider time, 13.0k
input tokens against 18.9k, 4.2k output against 1.8k, 14.7KB of
reasoning against 3.5KB. We are cheaper to feed and three times slower
to finish.

The failures that remain are one shape — a probe loop whose check could
not say no — and `short` turned out not to state that rule at all. Same
shape as the ask gap: a general rule missing from the variant, losing a
task, and mistaken for a fact about the harness.

## Card arms measured after the phase, one variable at a time

All at n=14 per task, same binary within each comparison, the card the
only thing that moved. Pass rate, then per-item credit once the graded
checkers existed.

| arm | all | ambiguous | dead-code | plain | skipped |
|---|---|---|---|---|---|
| baseline (27 complete) | 47/56 | 12/14 | 11/14 | 14/14 | 10/14 |
| + "the check must be able to say no" | 46/56 | 14/14 | 8/14 | 14/14 | 10/14 |
| + placeholder exemplars, sentence trimmed | 46/54 | 14/14 | 10/13 | 14/14 | 8/13 |
| + **"ask once for everything the check will answer at once"** | **49/54** | 13/14 | 11/12 | 14/14 | 11/14 |

**Two of the three did nothing, and both were mine.** The say-no rule
was a correct-sounding paragraph reasoned from the failures; it bought
`ambiguous-config` and cost `dead-code-sweep`, net zero. The exemplar
rewrite was a fix for a problem I had created that morning by making
three exemplars concrete enough to copy — one run reproduced exemplar
12 verbatim, invented names and the `// [worked example]` marker and
all, into a Rust repo that had none of them.

**The one that moved came from reading pi's transcript**, not from
having an opinion about prose. Its whole strategy is one line of its
own thinking: *"remove all of them, then run `cargo check` to see which
warnings appear."* One compile, eight answers. The card had always
taught the opposite. Calls per program halved on both target tasks (7.6
→ 4.1, 4.2 → 2.3) and programs per run fell 5 → 3 and 4 → 3, so the
mechanism is visibly the intended one.

**Half the prediction failed, which is the useful half.** I predicted
`dead-code-sweep`'s provider time would roughly halve; it went 193.7s →
**241.5s**. `skipped-tests` did drop (142.2 → 81.5s), so the effect is
real but not universal: on the larger task the cost is *reasoning*, not
round trips — 45.7KB of it, essentially unchanged by removing half the
calls. **The wall-clock gap to pi is a thinking-budget problem, not a
round-trip problem**, and the whole phase had assumed the opposite.

**And the graded signal earned its keep immediately.** Rescoring both
arms with the same checkers: `dead-code-sweep` credit 88% → 86%,
`skipped-tests` 88% → 90%, while the pass rate jumped on both. So the
rule did not make per-item judgements better — it made failures
*concentrate*, and runs stopped losing on one stray item. That is a
different and more honest description of the win than "it got better",
and one bit per run could never have told us.

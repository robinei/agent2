# Live-session findings, 2026-09-21 (Qwen3.8-27B on the LAN box)

Scenarios: lab/live1 (interrupt a running program), lab/live2 (interrupt
a program blocked on a 25s bash call), lab/live3 (ambiguous task that
should provoke a question). Plus re-reads of lab/conv, lab/intr3.

## Interruption: works, in both hard cases

- live1: post arrived while the program was mid-loop over four files.
  Rule B parked it at the next fuel slice, after alpha.py was written
  and before beta/gamma/delta. `handback at 924: Posted { ids: [20] }`.
- live2: post arrived 6s into a 25s `bash("./slow.sh")`. The program
  suspended at once — `handback at 33: Posted` — without waiting for
  the call. This is the case the design calls out ("a parked program
  has no next slice of its own, so ask for one") and it holds.
- A post arriving during *generation* is queued, not acted on. That is
  by design (`Interrupt` is the separate gesture) but `--stdin` has no
  way to send an Interrupt, so the lab can only test rule B.

## What the models did with a parked frame

- live1 wrote `history.append(abandon());`, then verified the three
  files it had been told to leave alone were untouched and said so
  precisely. Correct, and it found `abandon()` although the report
  offers only `resume()`.
- live2 acknowledged in prose and wrote a cell containing `finish();`.
  The parked program was `Superseded`. Also correct.

So the model handles the interruption well. What does not is the
labelling around it (both now fixed, see commits below).

## Fixed this session

1. `0ca21e1` snake_case survives the chat pane. `_` and `*` were the
   same delimiter, so `invoice_total(x) then line_total(y)` reached the
   person as `invoicetotal(x) then linetotal(y)`, and an unclosed run
   recovered as `*` — `invoice*total`. Three spellings of one
   identifier in one pane, none of them the model's. CommonMark's
   intraword rule; plus `Frame` now remembers the character that
   opened it so recovery never invents one.
2. `85412a2` a reopened session is not still running, and a stop is not
   a failure. `program_status` is filled only from the live
   `SessionEvent::ProgramStatus`, so a reopened log fell through
   `unwrap_or("running")` and titled *every* block of a finished
   session `program: running`. And `ProgramStatus::Failed` — whose own
   doc says it means abandoned — rendered as `failed`, so a program the
   person had just asked to stop was marked as having gone wrong.

Both were found by capturing real TUI frames under `script`, not by
reading the code. The same capture confirmed that reopening a finished
log still appends nothing to it.

## Open, not acted on

- **The parked-frame report names only `resume()`.** "It is paused…
  `resume()` continues it from where it stopped" is the whole of the
  guidance, and the person who just interrupted almost always wants the
  opposite. `WORK_UNDER_WAY` ("a reply with no ```js block ends here")
  is suppressed in exactly this case, being gated on
  `replies_since_spoken_to` being `Some` — which it is not right after
  someone speaks. Both models found their way regardless.
- **"Stop" does not stop the tool call.** live2's `./slow.sh` ran its
  full 25s after the program had parked at 6s. Deliberate ("calls it
  had already issued still settle"), but for a person whose ten-minute
  test run is the expensive thing, stop does not stop it.
- **An orphaned settlement costs a completion on a rested branch.**
  intr3: user says "never mind, forget it" → branch rests → the
  cancelled bash times out → the harness posts a notice whose own words
  are "Nothing is owed in reply" → the branch wakes and spends a
  completion saying "Understood, I've discarded the result." The notice
  says nothing is owed; the trigger rule does not read it.
- **"Stop" is the most expensive prompt in the system.** Across 15 lab
  sessions the median thinking block is 758 bytes. The three largest
  after a user post are all a stop: 28,171 / 18,771 / 12,754 bytes.
  One of them blew the old 8,000-token cap and stranded the branch.

## Measured, and it does not support the change

The pending question of inlining small tool results. Median delivered
result across the lab corpus is 164 bytes and 61% are ≤256 — but the
placeholder line that withholds one is only ~70 chars, so inlining
would cost *more* in 81% of rows. The "the description is longer than
the value" intuition holds for 19% of them, not for the general case.

## The ask() round trip (live3)

Ambiguous task ("point at the new host", nothing names it). The model
read both files, then wrote one block that asks *and* does the work
after the answer lands:

    const newHost = (await ask("user", "What's the new hostname? …")).trim();
    ... replace both files, grep for leftovers, check the JSON parses

`agent transcript` said `waiting on you: #22` with the text and the
command to answer it. Answering from stdin resumed the parked block
with every variable alive, and config.json was written correctly.

Then it trapped: `Edit.replaceOnce(...).result` is `undefined`.

## `Edit.replaceCount` is the odd one out, and it costs traps

Across 420 logs with cells:

    replaceOnce   248     -> returns the text
    applyEdits    153     -> returns the text
    replaceLines   48     -> returns the text
    replaceCount   42     -> returns { result, count }

**6 traps** in 5 distinct runs come from mixing the two shapes
(`live3`, `skipped-tests` x2, `dead-code-sweep`, `delegate-notes`).
Five are the other direction — `.result` read off a function that
returns a string, then `undefined` fed into the next call, where the
diagnostic points at the *downstream* call rather than the mistake.

`Edit.count(text, needle)` already exists and is used 23 times, so the
count `replaceCount` bundles is separately available. Making all nine
return the text would remove the only asymmetry in the family and cost
nothing that is not already reachable. **Not done — this is a
model-facing dialect change, and it is your call.**

## Imitation beats instruction, measured

The card says "Every `↓` and `←` was added by the harness, never by
you. Do not write them yourself." Across 453 logs, **358 of 2,357 prose
parts (15.2%)** contain a bare `↓ history[N]` line the model wrote. It
is harmless — `strip_imitated_markers` takes it off both the delivery
and the document render — but one prose part in seven carries it, which
is what an instruction competing with a format the model sees on every
one of its own blocks is worth.

## The one that matters: a second message destroyed the parked program

`lab/live2`, double interrupt. `bash` in flight → "actually stop" →
parked at the next fuel slice → "and just tell me what phase it got
to" arrives before the command finishes → the result lands as **"A
call you issued has settled with no program awaiting it"**.

Cause: `d38c416` taught `needs_prompt` to answer for a `Suspended`
branch with an unseen post (the deaf-session fix). `prompt_if_needed`
then ran, and its last line was an unconditional
`self.phase = Phase::AwaitingLlm` — safe only while it was reachable
from `Idle` alone. Over `Phase::Suspended(run, _)` it dropped the
parked `Run` and its VM.

The orphaned result is the visible half. The invisible half: the
`Posted` handback on the log still said the program was parked, so the
report, the transcript and the model were all still offering a
`resume()` of a frame that no longer existed.

Fixed in `4898a80`, with a deterministic reproduction
(`a_second_post_leaves_the_parked_program_where_it_was`).

## Verified live on the fixed build (lab/live4)

Same shape as live2, new binary:

    [#21] invoke bash("./slow.sh")
    [#23] post: actually stop
    [#24] handback at 174: Posted { ids: [23] }   <- parked at once
    [#26] post: and just tell me what phase it reached
    [#27] result of #21: {"status":0,"stdout":"phase 1..5 ..."}

No orphan notice. The result is delivered into the parked program,
which is still there.

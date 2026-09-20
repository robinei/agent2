# 29 — What snags a model

A night spent reading 96 kept eval runs for places the harness trips
the model it is trying to help. Not a design phase: an audit, and the
fixes that fell out of it. The method is worth more than any one
finding, so it is first.

## What wants a decision

Everything below is done and measured. Four things are not, and each
is a judgement rather than a repair:

1. **Whether the document should shrink a duplicated `history.append`.**
   72% of appended bytes copy a result. The report now names it, which
   costs nothing and acts a turn late; rendering it as a pointer would
   save the bytes now and needs the card's "what you get is what its
   row shows" promise re-read first. I did not decide this at one in
   the morning.
2. **The console duplicates rows at exactly the same rate.** 72% of
   the 2.0 MB of console output in the corpus is verbatim a result
   already held in a row — the same figure as `history.append`, from
   the same habit. Console bodies are 9.3% of all document bytes, so
   about 6.7% of every prompt is text that is one `fetch` away.

   I did not act on it, and the reason is worth writing down rather
   than rediscovering. Unlike the `append` case this breaks no card
   promise — the console section is openly a clipped tail. It fails on
   something else: the model prints a file it already has *in order to
   have it in front of it next turn*. Replacing those bytes with a
   pointer would defeat the only purpose the call had. Whether paying
   4 KB of prompt to avoid a `fetch` is a good trade is a question
   about how the model works, not about how the report renders, and
   the lever for it is the card. It is the small lever either way:
   6.7% of bytes, against round trips that cost whole completions.

3. **A twentieth of the card is unmeasured**, though one probe now
   says the mechanism works. `spawn`, `fork`,
   `list_agents`, `answer`, the `Agent` type and the two-argument
   `tell` come to 923 bytes, 4.9% of the card, on every turn of every
   run — and not one program in 295 kept runs calls any of them. The
   suite has no multi-agent task, so this is not evidence that the
   vocabulary is unused in general; it is evidence that **we know
   nothing about it**. Two honest options: add a task that needs a
   second agent, or accept that a twentieth of the prompt is carried
   on faith. Deleting it is a third, and the wrong one — it would
   remove a capability rather than test it.

   Two live probes, one completion each, settled the functional half
   and found a snag in passing. Told to spawn a helper and ask it
   `17*23`, the model did exactly that and the answer came back — the
   vocabulary works end to end against a real provider. But the helper
   then spent **two more completions and 691 output tokens** doing
   nothing: `answer(11, "391")` discharges the question and the block
   carrying it is still a block, so the reply earned another turn. It
   wrote prose, got the "your last reply ran nothing" nudge — correct
   advice, wrong situation — and only then said `done()`. Three
   completions for a one-line answer.

   The second probe confirmed the fix before it was written: a helper
   whose charter told it to `done()` after answering used one
   completion per question, and a later `ask` woke it with everything
   it knew still in front of it. `answer`'s entry in the card says so
   now. It is the kind of thing that only shows up when the path is
   actually run, which is the argument for the eval task.

4. **The local model.** `Qwen3.8-27B` works and is far too slow for the
   suite — 450 seconds a program, a `skipped-tests` run capped out
   after two. Useful for watching behaviour, not for measuring it.

## What it came to

The whole suite, `deepseek-v4-flash`, `--repeat 3` both sides — the
same shape as the 2026-09-19 baseline. Each arm from a worktree pinned
to one commit, so no run straddles a build.

Both sides measured at scale, from worktrees pinned to one commit each,
so no run straddles a build.

| | baseline (37 runs) | HEAD (63 runs) |
|---|---|---|
| passed | 36/37 (97%) | 56/63 (89%) |
| prompt bytes | 1,074 KB | **684 KB (−36%)** |
| programs per run | 5.0 | **3.3 (−33%)** |

**The round-trip figure reconfirmed on fresh arms**, measured a
different way — replies rather than programs, and the mean of task
means rather than a pooled average, so the task mix cannot carry it:

| task | baseline | HEAD |
|---|---|---|
| ambiguous-config | 2.2 | 3.0 |
| dead-code-sweep | 6.7 | 3.0 |
| plain-question | 1.0 | 1.0 |
| skipped-tests | 5.8 | 2.0 |
| sweep-200 | 10.5 | 9.5 |
| sweep-40 | 5.3 | 4.0 |
| sweep-8 | 6.0 | 5.0 |
| **mean of task means** | **5.3** | **3.9 (−27%)** |

One task moved the wrong way, and the run that did it is the async
IIFE below — five programs to ask one question, now a compile error.

**The cost is a third lower and the pass rate is a question, not a
win.** Fisher's exact on 36/37 against 56/63 gives p = 0.25: the drop
is not distinguishable from chance, and it is not evidence of safety
either.

**Two more arms did not settle it.** 27 further runs at later HEADs
came back 23/27, which pools to **79/90 (88%) against the baseline's
36/37**, p = 0.18. Three sets of arms now, the same direction each
time, and never significant — which is what a small real effect and a
small sample look like from the outside, and also what noise looks
like. Worth saying plainly: the two arms were built before two of
their own four failures had their causes fixed (a no-op write nobody
could see, and the async IIFE), so they are not a clean read on the
current tree either.

What can be said about the original drop:

- It concentrates in the two **large** sweeps. `sweep-40` 6/6 → 6/9 and
  `sweep-200` 4/4 → 7/9, while `sweep-8` — the same task, smaller —
  went 5/6 → 9/9, and `dead-code-sweep`, `plain-question`,
  `ambiguous-config` and `skipped-tests` all held or improved.
- Every failure is the same task judgement: a definition deleted that
  something still reached without naming it. None is a harness fault.
- **It is gone.** Ten fresh runs of these two tasks, after `outline`
  was made to say what it does not know, came back 10/10 — the
  baseline's own number, at half the programs. A second ten-run arm
  came back 9/10, and its one loss was not the model: `exit=101`, the
  agent panicking in its own renderer (below). **Twenty runs, twenty
  correct answers, one harness crash.** Against the earlier HEAD arms
  (13/18) that is p = 0.017 by Fisher's exact test. Everything below
  was measured before both arms.
- Fewer *programs* is not the mechanism: within HEAD the failing runs
  average 5.6 programs against 5.4 for the passing ones.
- **Bigger programs may be.** On the sweep tasks, HEAD's cells average
  726 bytes against the baseline's 472, and the median is 473 against
  296 — half again as much logic per block, which is the batching the
  opening listing was meant to buy. The failure read is a plain logic
  bug of the kind a long block invites: the run built a set of
  *suffixes* (`"004"`) from `helper_(\w+)` and then asked it about
  full names (`helper_004`), so every helper looked unused. Three
  small blocks have fewer places to disagree with themselves than one
  large one.

  That is a trade, not a defect: fewer round trips against more logic
  per round trip, and it is the hardest tasks that pay. Worth watching
  rather than reversing — the same batching is where a third of the
  cost went.

`outline` now says it lists what a file defines and never what uses it,
and names the mechanisms — `getattr`, string tables, registries — to
look for instead of the names. It landed after every run in the table
above, and the twenty runs that followed went 20/20 on the answer.

**And the behaviour it names moved, which is better evidence than the
outcome.** Across those runs against the nine before it:

| | before | with the guidance |
|---|---|---|
| a program mentions `getattr` | 2/9 | **6/10** |
| a program reads the `IDS` table | 4/9 | **8/10** |
| a program asserts before writing | 3/9 | 6/10 |

An outcome can move for any reason; a guideline naming `getattr` and
then tripling how often programs mention `getattr` is the guideline
doing the thing it was written to do.

**The same is true of the assertion paragraph**, and this one is
measured on matched task mix rather than on one pair of tasks. Does a
run's program ever `throw` or `console.assert` on something it worked
out?

| task | baseline | HEAD |
|---|---|---|
| ambiguous-config | 0/5 | 1/4 |
| dead-code-sweep | 0/6 | 2/4 |
| skipped-tests | 1/4 | 3/4 |
| sweep-200 | 3/4 | 2/2 |
| sweep-40 | 2/6 | 2/2 |
| sweep-8 | 2/6 | 2/2 |
| **total** | **8/31** | **12/19** |

p = 0.016, and not one task moves the other way. It shows up in the
run summaries too, as traps that are the model's own assertions
firing: "dynamic access mechanisms present — reviewing before
deleting", "double blank at output line 603", "name refs changed:
app.py:helper_004". Each of those is a reply that stopped instead of
writing a wrong file.

**The flip side, which I could not measure.** One `sweep-40` run threw
five of its own assertions in a row — two of them wrong, comparing
`helpers.helper_000` against `helper_000`, and comparing a fresh grep
against a stale `history.fetch` — and never reached a write at all.
Each throw is a round trip, and the card tells the model to assert
without saying what to do when the assertion is the thing that is
wrong. Across the six arms only two runs threw three or more, and one
of those passed: n=2, so this is a thing to watch rather than a
finding, and not grounds for touching a paragraph that is otherwise
doing its job.

Per task: `sweep-200` 11 → 5 programs and 395 → 146 KB, `sweep-8` 4 → 2
and 107 → 53 KB, `skipped-tests` 5.5 → 3 and 146 → 82 KB,
`dead-code-sweep` 7 → 6, `ambiguous-config` 2.5 → 2, `sweep-40` and
`plain-question` unchanged.

The denominators are the other result. A run the provider never
answered is excluded rather than failed, and the baseline lost three
(A run the provider never answered is excluded by the driver rather
than failed. The baseline lost three that way in one batch; the HEAD
suites lost none, because the retry budget now outlasts the 530 that
took them.)

A third off the cost, and rather more than a third on the tasks with
room to move: `skipped-tests` and `sweep-200` each lost roughly two
thirds of their prompt.

**Which change bought it is not attributable from these runs.** The
arms in between are 5 and 6 runs each on differing task mixes, which
resolves nothing; only the 37-vs-63 endpoints are solid. One mechanism
*is* directly measured, though: **runs that spend their whole first
program on `ls`, `find` or `pwd` went from 6 of 15 to 0 of 6 and 1 of
5** once the opening context said what was in the directory. The shape
of the win is a model that can see the tree planning the whole job in
one program instead of discovering it three files at a time — and the
cell sizes bear that out, 726 bytes against 472 on the sweeps.

One change moved a pass rate outright: refusing an edit that would
double a line's indentation took `skipped-tests` from 2/4 to 6/6. The
guard fired once in those six runs, and the reply after it opened

> The `old` strings were missing the leading indentation.

then rewrote the edit with the indentation included and read the diff
the write handed back — a message taken verbatim and a result field
that was undocumented six hours earlier, in the same turn. That is what
a report earning its place looks like.

### Where it did not help

The full suite at the end was 9/11 against a 14/15 baseline, on 42%
fewer prompt bytes. Two runs account for the difference and neither is
a regression in the harness:

- `sweep-200` fetched a row it had appended as `{ app: … }` and called
  `matchAll` on it. That is the copy-a-result habit biting a turn
  later, as a shape confusion rather than as bytes — a good argument
  for the advisory, and the message now names the key.
- `ambiguous-config` read the file holding the ambiguity, then read the
  README, then ran `git log` and `grep` against a fixture with no git
  repo, then asked its question. Four programs where the checker allows
  fewer. Exploring before asking is not wrong, and a card sentence
  pushing models to ask sooner would buy this back by making them ask
  when they should look.

  I suspected the opening listing of inviting the extra reading, and
  checked: the reply the question lands on is 2, 2, 4 at HEAD against
  3, 2 at baseline. Two of three ask *sooner* than before. The four is
  variance, not a cost of showing the directory.

## The method

**Compare the model-facing surface against what the code does.** Every
claim the harness makes — a tool's `returns`, an `@example`, a card
sentence, an error message — is a promise a model will act on. Four
declarations turned out to describe something other than what the tool
did, and the measurements say those cost more than every dialect gap
put together.

**Probe, don't grep.** 105 sites in `interp` say "type error" or "value
error"; seven are reachable by a program. 81 dialect constructs were
tried to find 14 gaps. 38 results were compared against JS to find the
silent ones. Reading the code tells you what exists; running it tells
you what a model can hit.

**A message that names the value finds the bug it was hiding.** Twice
in one night, and both times the bug was older than the message.

**Read the runs, not the aggregates.** Every real finding below came
from opening a log. The one thing built from an aggregate alone was
withdrawn a commit later, because reading the runs it fired on showed
it was wrong five times out of six.

**Where an example and a rule disagree, the example wins.** Twice, with
receipts.

**Render the prompt at the moment you are asking about.** `agent
document <log>` could only show the newest leaf, which is the least
interesting point in a run: what a reply was answering is the spine
*before* it, and a run that went wrong went wrong in the middle. It
takes an event id now. The first mid-run document it printed had a
report reading "The last 0 of 3 lines" over an empty fence, and the
first log rendered end to end crashed the binary.

## Three ways the harness broke itself

These are a different class from everything else here. A snag costs a
round trip; these cost the run. Three of 294 kept runs exited 101 —
the agent dead, the task half done, the work unreachable.

### A console line bigger than the budget showed none of it

A `sweep-40` reply printed `outline("helpers.py")`, 4,531 bytes, the
one thing it had gone to fetch. What came back:

    ### it printed
    The last 0 of 3 lines; `history.fetch(19)` for all of them.
    ```text

    ```

Its next two replies were spent re-fetching that row and re-reading
the file, and the run scored 40%.

`render_console` walks the tail newest-first and stops at the first
line that does not fit, so a line past the *whole* budget stopped it on
the first step and `start` ran off the end. Not a corner case: the
interpreter caps a console line at 4,096 bytes, the report's section
budget **is** 4,096, and the walk charges a line its length plus its
newline — so every capped line overshot by exactly one. Any
`console.log` of a file, an outline or a grep dump over 4KB hit it.

One line now always survives and is clipped to the budget. An interp
test pins cap == budget so the two cannot drift apart in silence.

This is the third time the same shape has appeared: a channel exists,
is too narrow for what programs actually reach for it to do, and the
model pays for it somewhere else. The first two were a 200-byte
per-line clip and a 20-line cap on this same section.

### Call offsets do not survive compaction

`told_literal_cuts` concatenates every `Prose`/`Cell` part to get the
string a call's `site` indexes. The render loop concatenates the same
parts — except that a compacted one contributes a one-line `↓
history[N] … summary` marker instead of its bytes. From the first
shadow onward the two strings disagree, and every later offset points
somewhere else in a string that is now shorter.

A live `sweep-200` compacted 54,651 bytes mid-run and panicked with
`start=5596 end=5620` against a 369-byte reply. The fix carries the
cuts across with the parts that survived; a cut inside a shadowed part
is dropped, because the call it annotates is no longer in the text.
With nothing compacted it is the identity, which is nearly every
render.

Worth noticing: compaction is new, and this is the failure mode a new
mechanism has — not wrong in itself, but invalidating an assumption
something older was built on.

### `history.fetch(0)`

`EventId` is a `NonZeroU64`. Every id-taking entry point filters `> 0`
before converting — except the re-attach check, which runs first and
built one straight from the program's argument. Two runs reached 0
through their own arithmetic and died on `expected non-zero EventId!`.

The fix is a constructor that cannot panic rather than a filter
somebody has to remember. And since a panic is the one outcome no
program can recover from, there is now a standing sweep: forty calls
with ids that are not ids, arguments of the wrong shape, and the values
JS arithmetic reaches when something upstream went wrong. The bar is
only "no panic" — refusing is a fine answer, and so is doing the thing.
Each case has to compile and run too, or a sweep like this quietly
stops testing what it names; that assertion immediately caught two
cases that were exercising the parser and nothing else.

## The declarations are clean, checked against the runs

The class this audit found most expensive was a tool whose `returns`
described something other than what it handed back — four of them.
Worth re-checking against data rather than against the source, so:
every `Delivered` result in the 295 kept runs, bucketed by its actual
key set.

| tool | shapes returned | n |
|---|---|---|
| `bash` | `{status, stdout, stderr}` | 1,423 |
| `read_file` | `{content, version}` | 1,453 |
| `replace_file` | `{version, diff}` / `{version}` | 327 / 5 |
| `outline` | `{items}` | 95 |
| `parse_errors` | `{ok, errors}` | 70 |
| `create_file` | `{version}` | 15 |

Nothing else, and nothing that disagrees with the manifest. The 31
`outline` results shaped as a bare array are from before it was
fixed. `read_file` never returned `truncated` in 1,453 calls, which is
what the declaration now says; `bash` still declares it and still
means it, though nothing in this corpus reached the cap.

Every non-object result is a proper `Failed` with a usable message —
a version mismatch that names the current version and the last-write
route, a missing path, a `create_file` on an existing file that names
`replace_file` instead. No silent nulls.

## The one that destroyed a file

`outline`'s `kind` was the tree-sitter node kind. The same concept has
four names across the four languages it parses — `function_item`,
`function_declaration`, `function_definition` — and the word every
model reaches for matches none of them. Across the kept corpus:

| programs compare `kind` against | uses | runs |
|---|---|---|
| `"function"` | 26 | 17 |
| `"function_definition"` | 17 | 15 |
| `"def"`, `"class"` | 2 | 2 |

**The wrong guess was the commoner one.** And it fails in the worst
available way: `items.filter(i => i.kind === "function")` is an empty
array, not an error. A run gets no signal that it asked the wrong
question — it gets an answer meaning "none of them", which on this
task shape reads as "nothing here is live".

One `sweep-8` run followed that all the way:

    const newContent = '"""Assorted helpers."""\n';
    await tools.replace_file("helpers.py", newContent, version);

The whole file replaced by its docstring, every live function with it.
The verdict was "the tests no longer pass — something still in use was
removed".

`kind` is now a word a reader would guess, the same word in every
language, with the grammars' real distinctions kept — a Rust struct
and an enum do not both become "type". The declaration lists the
fourteen values instead of saying `string`, so there is nothing left
to guess.

This is the fourth declaration in this document that described
something other than what the tool did, and the only one that cost a
file. The lesson is narrower than "check your declarations": an enum
rendered as `string` is an invitation to guess, and a filter is the
one place where a wrong guess returns a plausible answer instead of an
error.

## The edit that eats an indent, one branch further on

Ten kept runs hit an `IndentationError`; six of them failed for it.
Three wrote the same call:

    Edit.replaceCount(text, '@unittest.skip("rates were in flux")\n', "")

against `    @unittest.skip(…)\n    def test_base_rate(self):`. The
four spaces before the `@` survive, the `def` keeps its own, and the
method ends up defined eight columns in with a body no longer indented
relative to it.

Two things had to be wrong at once for that to land. `replaceCount`
had **no indentation check at all** — and it is the verb a program
reaches for to strip a decorator from several methods at once, which
is this edit once per method. And the check its siblings do have
required `new` to *begin* with the same indentation, the case where
the two collide; an empty `new` orphans the indentation instead. Same
corruption, one branch further on, and the guard written for the
family missed the member that was actually being written.

The remaining seven have other causes — line splicing through bash,
heredocs — and are not this.

Worth noticing about the shape: the first version of this guard was
written from two live incidents and generalised from exactly those
two. The third incident was the same mistake with one detail changed.
A guard written from examples covers the examples.

## Rows that said the same thing whichever way it went

The narrow-channel shape has a mirror image, and it took a second pass
to see it. A channel can be wide enough and still tell the reader
nothing, because it says the same words whatever happened. Two of
these were in the menu — the part of the document a model reads on
every single turn.

**A failed command looked exactly like a successful one.** Every
`bash` row read `→ ok, {status, stdout, stderr}, N bytes`: the field
names, which the card declares anyway, in place of the one number that
varies. The card opens `bash`'s description with "Read `status` before
`stdout`. A command that ran and failed writes nothing, and nothing
reads as 'found no problems'" — and then the menu hid the status. 105
of 1,178 bash calls in the corpus exited non-zero and not one row
mentioned it; the only way to find out was to spend a `fetch` on a row
indistinguishable from the 1,073 that had nothing to report. Now:

    - `[54]` `bash("python3 test.py 2>&1")` → status 1, {status, …}, 414 bytes

Silent at zero, like every other count in that file.

**A write that changed nothing was reported by omitting a field.** The
weakest signal there is. A `sweep-40` run computed a cleaned
`helpers.py`, wrote back bytes identical to what was on disk, read no
`diff`, and told the person it had deleted the dead helpers — all 24
were still there. Its row said `→ ok, {version}, 17 bytes`, exactly
like a write that had done something. Five writes in 323 changed
nothing and two of those runs failed. The row now says `no change`.

**The commonest trap pointed a caret at the model's own prose.**

    1:1: cannot read .length of undefined
    Dead-code hunting in `helpers.py` — first, what's in the repo and
    ^

    ### where it stopped
    in <unknown> → <unknown>

Nothing in that report is true except the words "of undefined". The
higher-order methods lower to real JS helpers compiled ahead of the
user's program, so `xs.map(f)` on an undefined `xs` fails at
`a.length` inside `__map`; that instruction's span is in the prelude
region, rebasing subtracts the prelude's length and saturates to zero,
and zero renders as line 1, column 1. The span cannot be recovered —
there is genuinely no user source there — but the name can, and it is
the half the reader needs: the trap now reads "`map` was called on
undefined". And a site of zero renders no location at all, which is
already the convention elsewhere in that file. 11 of 121 diagnostics
in the corpus, 9%, pointed at prose this way.

**A reply's own marker, doubled.**

    ↓ history[77]
    Let me get the full source with line numbers.

    ↓ history[78]
    ↓ history[78]
    ```js

The model reads `↓ history[N]` above every block of its own and writes
them back. The replacement pass only looked *forward* from a block's
start, which catches a marker at the head of a paragraph and misses
one at the tail of the paragraph before — the same line, one byte
either side of a boundary the writer cannot see. 17 across 291 runs,
0 after. It matters more than 6% of runs suggests, for the reason the
sibling fix on `/* ← history[N] */` already records: the doubled form
is what gets imitated next turn.

## Two things the model is spending turns on

Neither is a defect, and one may not be fixable, but both are large
enough to name.

**A quarter of every reply that runs anything does nothing but load.**
26% of the 1,072 replies with a cell make no write, no `tell`, no
`append` and no `done` — they read, print, and stop. It is flat across
every arm measured tonight, baseline and HEAD alike, 20% to 34%.
Much of that is the documented rhythm: end when what to do next
genuinely depends on what came back.

The sharp subset is not. 86 of 818 replies with calls (10.5%) spend
the *whole* reply re-reading files the run had already read, with no
write in between — a turn that cannot be waiting on new information,
because there is none. 341 of the 352 files read twice were re-read
that way. The card said results do not cross replies and that `fetch`
reads them back, but never that a fetch hands the value straight to
the block that asked, so "get it into view" reads like a step you take
before you can work with something. `fetch`'s entry now says it is
not. Untested against a live arm; the number to watch is that 10.5%.

**A check that ran and was never read.** 125 of 216 closing blocks
(57%) call a tool and assert nothing on the result. The pass rates
are 81% with the shape and 88% without, n=25 on the smaller arm —
noise, and I will not claim otherwise. But the transport fact behind
it is not statistical: the report carrying that block's console is the
one that is never sent, so a verification written as a `console.log`
in the block that calls `done()` has told nobody anything. A run read
in full did exactly that, printed a grep showing all forty helpers
still present, and told the person "Removed 0 dead helpers: , , , …".

## What was actually wrong

### `outline` returned something other than what it declared

Its `returns` said `{ items: Array<{ name, kind, line }> }` and its
`@example` said `const { items } = await tools.outline(path)`. It
returned a bare array, whose entries carry `start_line`/`end_line` and
not `line`.

| | |
|---|---|
| `outline` calls across 96 runs | 21 |
| traps caused | **11** |
| share of all traps recorded | 11 of 30 |
| share of all `.length of undefined` traps | all of them |

It was also the only tool in the set returning a bare array; the other
six return objects, 858 calls between them. The declaration was right
about what the model wanted, so the value moved.

### `replace_file` returned a diff nobody was told about

Declared `{ version }`, returns `{ version, diff }` on 84 of 86
observed calls, and its guideline read *"You will not see the result,
so verify in this same program: read it back."* Which is false, and
paid for: **39% of writes were followed by a `read_file` of the same
path**, and not one cell in 96 runs mentioned `.diff`. The menu row had
been printing `→ ok, {version, diff}` the whole time, so the
declaration was contradicting the document around it.

### `read_file` declared a field it cannot produce

`{ content, version, truncated? }`, against a handler that returns
`{ content, version }` on every path. `bash` does have a `truncated`,
fired by its 4 MB stream cap — and it has never appeared in a result
across 154 runs, because nothing has produced 4 MB. A field a program
can branch on and never see is a dead branch in every program that
checks it.

That is four declarations in one night describing something other than
what the tool does. **All seven tools and all four `history` verbs have
since been checked against their handlers**, and a test holds the
manifest's claims now rather than a reading of it.

### `bash`'s description was cut mid-word

`DESCRIPTION_MAX_BYTES` is 400 and the doc beside it claimed "every
shipped tool fits comfortably inside this". `bash` was 408. What fell
off the end was `"s timeout, 4MB per stream"` — so the model was never
told a command has 30 seconds or that output stops at 4MB. The limits
now lead the sentence, because a clip takes the tail, and a test holds
the claim.

### The worked examples broke the card's own rules

`history.append`'s doc says in bold **"Not the bytes of something you
read"**. The fifth exemplar did exactly that, with prose defending it.
Measured over 42 baseline runs, by exact string match against what a
result delivered: **72% of all appended bytes are a copy** — 77 KB of
107 KB. The advisory's 400-byte floor sees 52 of those 72 points, which
is the part worth a sentence in a report; the rest is small strings
that are not worth naming.

The fourth wrote every file in a loop and then told the person how
many, with nothing run in between — the exact shape
`wrote_without_verifying` nudges about, demonstrated in the card the
nudge quotes.

### A reply cannot read what it printed

`sweep-8`, 2026-09-19. The run deleted six helpers, ran `test.py`,
printed the output, and in the next block of the same reply told the
person *"Syntax check and tests pass"* while the console held
`AttributeError` and `TEST_EXIT:1`. Its prose between the blocks read
*"Check what came back, then report."*

There is no way to do that. `console.log` reaches the model in the
*next* reply, and the card said output lands there "too: the cheap one,
for looking rather than keeping, and for findings **as you go**" —
where "as you go" is the false promise.

### "Your program did not run", over six rows that had

The blocks of one reply are one program that pauses between them, so
when the third block fails to compile the first two have already run.
The report for that case was the heading `## YOUR PROGRAM DID NOT RUN`
and the diagnostic — no rows, no console, 474 bytes.

| | |
|---|---|
| compile failures across 96 runs | 24 |
| where earlier blocks had already added rows | **24 — all of them** |
| rows hidden from those reports | 199 |

On `dead-code-sweep`, 2026-09-20, the hidden work was both source files
read, two rows appended holding them, and a `cargo check`. The next
reply started the task from the beginning and got it wrong.

The reasoning in the comment was half right: a cell that would not
compile built no VM, so *that cell* has no console, no rows and no
stack. The reply is not that cell.

I looked for a behavioural signal afterwards — whether the next reply
re-reads a file the failed one had already read — and found none: 0 of
6 on both sides. So this one rests on the report being true rather than
on a measured change, which is a weaker footing than the rest of the
night and worth saying.

### What one turn looks like now

A real report from the closing runs, and four of the night's fixes are
in it:

```text
## YOUR PROGRAM RAN, THEN A BLOCK DID NOT COMPILE

288:7: `test` is already declared — this code shares one scope with
what ran before it, so that name is taken. Use a different one, or
assign to it without `const`/`let`.
const test = await tools.read_file("test_shipping.py");
      ^

### rows it added
- `[7]`  `read_file("shipping.py")`      → ok, {content, version}, 433 bytes
- `[10]` `read_file("test_shipping.py")` → ok, {content, version}, 996 bytes
- `[15]` `bash("… pytest --collect-only…")` → ok, {status, stdout, stderr}, 77 bytes
- `[21]` `bash("… pytest -v …")`          → ok, {status, stdout, stderr}, 77 bytes

### it printed
=== shipping.py ===
"""Shipping cost rules."""
…both files, whole…
```

The heading is true, the diagnostic names the scope, the four rows
exist, and the files arrived whole. The same turn before tonight was
the heading `## YOUR PROGRAM DID NOT RUN`, the bare words `test is
already declared`, and nothing else.

## The dialect

81 constructs a model might write, probed against what `interp`
implements. Two silent wrong answers, which are worse than gaps because
the program carries on:

- `Array.from({length: 3}, (_, i) => i)` — the standard range idiom —
  returned `[null, null, null]`. The builtin cannot invoke a closure, so
  the mapper was accepted and dropped.
- `[...Array(3).keys()]`, the other range idiom, threw. Both ways to
  write a range failed, one loudly and one silently.

And `Array.from(new Set(xs))` threw while `[...new Set(xs)]` worked —
two spellings of one operation disagreeing, which a reader can only
find by falling into it.

**And the worst category, checked separately.** A silent wrong answer
never shows up as a trap, so 38 cases were compared against what JS
would return. Seven differ:

| | here | JS |
|---|---|---|
| `[NaN].includes(NaN)` | `false` | `true` |
| `String(1e21)` | `1000000000000000000000` | `1e+21` |
| `'ab'.replace('a','[$&]')` | `[$&]b` | `[a]b` |
| `Object.keys({b:1,'2':1,a:1,'1':1})` | insertion order | integer keys first |
| `Object.is`, `[] + []`, `a.length = 1` | throw | a value |

The last three are loud and therefore fine. The other four are quiet —
and none occurs in 1,076 cells of real model code: no `NaN` at all, no
`Object.is`, no `.length` assignment, and all three uses of `$&` are
the regex-escape idiom `name.replace(/[.*+?…]/g, "\\$&")`, where `$&`
works. `$1`, `$&` against a regex, and the replacement-function form
all behave.

So the dialect is sound where models actually operate, and these stay
as they are rather than being fixed on spec — recorded here so the next
person knows they were looked at.

**A message that names the value finds the bug it was hiding.** Twice
in one night. `Array.from`'s "type error" hid the fact that it took no
Set; when it started naming what it got, `Object.fromEntries needs an
object; got a map` turned up in a live run within the hour — and a
`Map` is a list of entries, so that refusal was a gap too. Fourteen
further collection-interop cases were then probed and all passed, so
that family is done.

**What was left undone, and why.** Object getters, labelled
break/continue, generators, `String.raw`, `structuredClone`, `BigInt`,
`Object.groupBy`: **zero uses across 96 runs**. Closing them would be
speculation.

### The forgery guard checked a shape nothing emits

`escape_untrusted` exists so untrusted content cannot render looking
like a row the harness wrote. It matched `^\[\d+\]`; rows have been
`` `[2]` user told you: … `` since ids were backticked. It escaped the
dead shape and passed the live one, and had no test in either
direction.

A fifth: the retry budget. `backoff`'s doc says a cold model load
"needs ten to twenty" seconds; five attempts of doubling from 400ms
totals six. The test guarding it asserted the total was in
`6_000..20_000` and passed at exactly 6,000 — the bottom of a range
whose *upper* number is the case it is named for. Three eval runs were
lost to an upstream 530 that outlasted it.

That is the same failure as `RETURN_MAX_BYTES` and the console line
cap, in a place where being out of date is a hole rather than a
wart: **a constant or a pattern that encodes another part of the
system's format has to be checked against that format, because nothing
else will notice when it moves.**

## Errors that sent the reader to the wrong place

| was | is |
|---|---|
| `` `f` is already declared `` | …and this code shares one scope with what ran before it. The check only fires across fragments, so the other declaration is never in the fragment being read. Six runs died here |
| `replaceOnce expected 1 match, found 4 of X — widen it` | …*at lines 3, 11, 19, 27*, or name the line with `Edit.replaceLines`. The offsets were in hand at the moment of the error |
| `f is not defined` | …and `f` was bound in the reply at `[12]`; nothing crosses between replies. Said only when the declaration is findable, so there are no false positives |
| `assignment to undeclared variable` | …there is no implicit global here; declare it with `let`. Accurate before, and silent about both things a reader needs |
| `in \`replaceOnce\`: type error` | …`text` is the `{ result, count }` object `replaceCount` returns. The mirror mistake had a message; this one did not, and it cost a run its task |
| `history.append` returned `null` | …returns the row's id. Two live programs invented an identifier for it rather than do without, and died on it |
| *(nothing)* | …an edit that would leave a line indented twice over is refused, naming the column and the fix. Two runs wrote a broken file and reported success |

Of 50 traps and compile failures across the kept runs, four were
repeated immediately — the same message on the next handback. All four
are in this table. **That ratio is the most direct score there is for
whether a message works**, and it is worth watching.

## What now holds itself

Four of the night's findings were a claim in the prompt that nothing
checked. So the checks exist now, and each was verified by breaking the
thing it guards:

| guard | catches |
|---|---|
| `every_tool_description_fits_the_clip` | a description cut where the model reads it |
| `the_shipped_manifest_tells_the_truth_about_itself` | a `returns` that names a field the handler cannot produce |
| `every_worked_example_compiles` | an exemplar teaching a construct this dialect does not have |
| `every_tool_example_compiles` | a typo in a one-line `@example`, copied verbatim |
| `the_cards_declarations_are_valid_typescript` | a stray brace in half the card |
| `the_card_teaches_the_markers_the_document_uses` | a glyph renamed in the renderer and not in the card |

Everything the model is given to imitate — the declarations, the worked
examples, the one-line `@example`s — is now checked mechanically rather
than by reading. That is the part of tonight most likely to still be
paying next month, because the failures it prevents are the ones
nobody notices: a claim in a prompt has no compiler, no reviewer and no
user to complain, and the only reader it misleads writes code for a
living.

## What was rejected

**A nudge for "spoke to the person after a failing command".** Built
from an aggregate, withdrawn after reading the runs. It fired on 9 of
96; 46 of its 58 firings were `grep` exiting 1 with no matches, which
is an answer. Teaching it that exit 1 with nothing written is a search
that found nothing cut it to 7 — and those were mostly a model running
an un-skipped trial suite *expecting* failures and correctly saying so.
It also never caught the case that motivated it, because that run wrote
`; echo TEST_EXIT:$?` and exited 0.

A report section that is wrong most of the time it appears is how a
reader learns to skip that section.

## The one that had a cause underneath it

`history.append` carrying bytes already on the log is 72% of everything
appended — and reading the run that did it showed the harness had left
no other door.

A `read_file` result renders as a menu row: an id and a size, not the
content. To *see* a file a program prints it, and the printed section
was capped at 20 lines. So a model that needed a 51-line file in front
of it printed it, got "the last 20 of 51 lines", printed it again, and
then appended it — which is the one thing that renders a value whole.

The file was **1,299 bytes against a 4 KB budget**. The line cap was
binding and the byte budget was not, and the doc on the byte budget
describes this exact failure one constant over: a per-line clip that
"made the channel useless for the thing programs actually reach for it
to do".

Worth holding on to: **a rule the model keeps breaking is worth reading
as a route around something.** The card forbade the copy, the worked
example was fixed, the report now names it — and none of that would
have helped while looking at a file whole was impossible any other way.

### The waste it removed, measured

The same file read twice inside one reply — the thing the
redeclaration diagnostic was catching all along:

| | read_file calls | same path twice in one reply | *any* call repeated identically |
|---|---|---|---|
| baseline | 95 | 7 (7%) | 11 of 205 (5.4%) |
| HEAD, suite 1 | 91 | 0 | 1 of 166 (0.6%) |
| HEAD, suite 2 | 120 | 1 (1%) | 3 of 211 (1.4%) |

The survivors are defensible — a test re-run after a change is not a
repeat of the same question.

**What is not demonstrated:** the copy advisory. Appended bytes per run
are 1,538 at baseline against 981 and 2,803 across the two HEAD suites,
and the share of them that duplicates a result is 64% against 60% and
16%. The direction is right and the variance is larger than the effect.
The model in that `sweep-8` run appended all four files *in the same
program that read them*, before any report could have reached it — so
the advisory can only work on the turn after, and one turn after is
where the evidence would have to come from. Worth re-measuring with
more runs before believing it.

Not from the message, which changed after both suites: from the report
that now shows what an earlier block already did, and from the console
that now returns a file whole rather than its last twenty lines. A
model that can see what it has does not fetch it again.

## What is left, and the one decision I did not make

Traps after the night, over 58 runs: twelve, all singletons, and not
one `.length of undefined` where there had been eleven. The largest
remaining class is the one the notebook model creates:

    `r` is already declared — this code shares one scope with what ran
    before it, so that name is taken.

Four of the twelve tonight; six of thirty in the baseline. Consistently
a fifth to a quarter of everything that traps, and it is not a mistake
in the ordinary sense — a model writing `const r = await tools.bash(…)`
in its second block is treating a block as a step, which is what a
block looks like.

The message now explains the scope. The card explains it in the
preamble. Neither stops it, because both are read before the moment and
the moment does not feel like one that needs them.

**The option I nearly took, and should not have.** Let a later block's
`const` rebind the name, since the dialect is ours and "informed purely
by what we find the model to act best on". Then I put the failing line
next to the binding it clashed with, in four runs at HEAD:

```text
first:  const f = await tools.read_file("helpers.py");
again:  const f = await tools.read_file("helpers.py");
```

Four of four were the same statement written twice. It is not a naming
collision — the model is redoing work, and the value it wants is still
bound. Allowing the rebind would have made a wasted call silent, and
the message's old advice ("use a different one") would have kept the
second read while removing the complaint.

So the error stays and the remedy it leads with changed: **use what you
already have.** The card says a `const` in the first block is still
bound in the second; the diagnostic is where that gets read.

## The next round trip, untried

If the lever is round trips, the list of them in a run is short:
orientate, read, edit, verify, recover, compact, answer. Tonight
removed or bounded four — orientation, the re-read, the compaction that
could not help, and a share of the traps. What is left is *read*, and
the opening context is where it would go: a fixture whose four files
are two kilobytes could be handed over whole and the first read turn
would vanish.

Not done, and not obviously right. The bound is the problem — 50
directory entries is a fixed cost and file contents are not — and the
card's whole account of `read_file`, `history.fetch` and what a row
holds assumes the model fetches what it needs. Pre-loading changes that
relationship for every session to save one turn in small ones.

Recording it because it is the obvious next question, not because it is
the obvious next change.

## Compaction, forced

None of the suite's documents get near a 64 KB budget any more, so
compaction had to be provoked: `sweep-200` at 34,000 bytes, against a
card-and-examples floor of 25,792 that no handler can touch.

It fired **thirteen times in one run**, removed 49 rows, and the
document was never once below the floor. `COMPACTION_ATTEMPTS` is
supposed to bound exactly this and cannot — it counts fires since the
last *success*, and at the floor every round succeeds at removing rows
while shrinking nothing. Block compaction, added the same night, made
it worse by giving the model more rows it could always find something
among.

The guard that does not depend on counting is the floor itself: render
the document with every nameable row shadowed at once, and if that is
still over the threshold, no handler can get under it. Asking is a
completion spent on nothing.

Same scenario with the guard: **0 fires in both runs**, the pass rate
unchanged at 1/2, and the cost of that arm halved — 9.0 programs to
4.5, 236 KB to 155 KB. Pure waste removed, nothing else moved.

Worth keeping: **a bound on attempts is not a bound on a loop whose
every round reports success.** The question a guard has to answer is
whether the work can help, not how many times it has been tried.

## The local model

`Qwen3.8-27B` over the LAN, 64k context, as a second opinion on
everything above. It works with the harness and it is far too slow for
the suite: roughly 450 seconds a program, and a `skipped-tests` run hit
a 3,600-second cap after two.

It is still the best evidence for block compaction, because it reached
for the feature on its first exposure to it. Its compaction program:

```js
history.remove(4);
history.remove(5);
history.remove(9, 12);
history.append("Findings so far: …");
```

`4` and `5` are the prose and the cell of its own previous reply, named
off the `↓ history[N]` markers that had existed for an hour. A second
run compacted four rows and every one was a `Part` — its own blocks
again.

It also produced the loop that led to the floor guard, and the
repeat-yourself behaviour that led to a spent compaction program
removing itself. A model slow enough to watch is worth having.

## Where the bytes are, and where they went

A rendered document, by role, over twelve of each:

| | baseline | HEAD |
|---|---|---|
| card + worked examples | 72.7% | 73.9% |
| assistant turns | 15.9% | 15.0% |
| user turns | 11.4% | 11.1% |

Within a user turn at baseline, `### it printed` and `### rows it
added` are about 7% and 6% of the whole document; everything else is
3%.

**The composition barely moved, and that is the finding.** A document
is about the same size as it was; there are fewer of them. Prompt bytes
fell 36% and programs per run fell 33%, which is the same number twice:
the savings are round trips, not smaller prompts.

Two consequences worth carrying:

- **Compaction operates on the quarter of a document that is not the
  card**, and the card is a floor no handler can reach. That is what
  the floor guard is about, and why a byte budget near 26 KB is
  meaningless.
- **Shaving the conversation is the small lever; not needing the turn
  is the large one.** Everything measured tonight that mattered
  removed a round trip — the orientation program, the re-read, the
  compaction that could not help, the reply spent on a trap. Nothing
  that shaved bytes off a turn showed up in the totals at all.

  The opening listing is the cleanest case, measured on the call
  rather than on the outcome. Does the first program run `ls` or
  `find`?

  | | first program orients |
  |---|---|
  | baseline (`baseline`, `base2`) | 26/31 |
  | with the listing (`sweeps`, `sweeps2`) | **2/20** |

  p = 2×10⁻⁷. Four lines of directory in the system message, and 84%
  of runs stop spending a completion to find out what is in front of
  them.

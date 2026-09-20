# 29 — What snags a model

A night spent reading 96 kept eval runs for places the harness trips
the model it is trying to help. Not a design phase: an audit, and the
fixes that fell out of it. The method is worth more than any one
finding, so it is first.

## What it came to

The whole suite, `deepseek-v4-flash`, `--repeat 3` both sides — the
same shape as the 2026-09-19 baseline. Each arm from a worktree pinned
to one commit, so no run straddles a build.

| | baseline | after |
|---|---|---|
| passed | 17/18 (94%) | 20/21 (95%), **38/42 (90%) pooled** |
| prompt bytes | 1,052 KB | **640 KB (−39%)** |
| programs per run | 4.8 | **3.3 (−31%)** |

Two suites were run at HEAD. Pooled they are 38/42 against 17/18, which
at these sizes says the pass rate did not move; the cost did, by about
two fifths. All four failures are task judgement — a test un-skipped
that still fails, dead helpers left behind, a helper removed that was
still reached through `getattr`, and one run that took four programs to
ask its question. None is a harness fault.

Per task: `sweep-200` 11 → 5 programs and 395 → 146 KB, `sweep-8` 4 → 2
and 107 → 53 KB, `skipped-tests` 5.5 → 3 and 146 → 82 KB,
`dead-code-sweep` 7 → 6, `ambiguous-config` 2.5 → 2, `sweep-40` and
`plain-question` unchanged.

The denominators are the other result. A run the provider never
answered is excluded rather than failed, and the baseline lost three
that way; this one lost none, because the retry budget now outlasts the
530 that took them.

A third off both, with the pass rate holding — and rather more than a
third on the tasks with room to move: `skipped-tests` and `sweep-200`
each lost roughly two thirds of their prompt. Most of the cost came off
one change — telling the model what is in
the working directory, so it stops spending its first program finding
out — and the shape of the win is that a model which can see the tree
plans the whole job in one program instead of discovering it three
files at a time.

The pass rate moved on one change too: refusing an edit that would
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
sentence, an error message — is a promise a model will act on. Two of
them were false, and the measurements say those two cost more than
every dialect gap put together.

**Read the runs, not the aggregates.** Every real finding below came
from opening a log. The one thing built from an aggregate alone was
withdrawn a commit later, because reading the runs it fired on showed
it was wrong five times out of six.

**Where an example and a rule disagree, the example wins.** Twice, with
receipts.

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
Measured: **72% of all appended bytes echo a result already on the
log** — 79 KB carried twice across 19 runs.

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

## Where the bytes are

Measured over 20 rendered documents:

| | |
|---|---|
| card + worked examples | **66.8%** |
| assistant turns | 16.8% |
| `### it printed` | 6.9% |
| `### rows it added` | 6.1% |
| everything else in a user turn | 3.5% |

Compaction operates on the third of the document that is not the card.
Worth knowing before optimising any of it.

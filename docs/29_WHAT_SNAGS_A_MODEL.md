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
| passed | 17/18 (94%) | **17/17 (100%)** |
| prompt bytes | 1,052 KB | **699 KB (−34%)** |
| programs per run | 4.8 | **3.0 (−37%)** |

Per task, programs and prompt both fell on every one of the seven:
`sweep-200` 11 → 6, `skipped-tests` 5.5 → 2, `dead-code-sweep` 7 → 5.5,
`sweep-40` 4 → 3, `sweep-8` 4 → 3.5, `ambiguous-config` 2.5 → 2,
`plain-question` 1 → 1.

(A run the provider never answered is excluded by the driver rather
than failed, which is why the denominators differ. Three such were lost
to an upstream 530 in one batch, and the retry budget has since been
raised to outlast it.)

Half the completions and half the bytes, with the pass rate holding.
Most of the cost came off one change — telling the model what is in
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
  when they should look. Left alone, and worth watching: it is the one
  task where showing the directory may invite reading more of it.

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

**The option I did not take:** let a later block's `const` rebind the
name, since the dialect is ours and "informed purely by what we find
the model to act best on". The analyzer already shadows by default —
the check exists specifically to stop it, because a closure made in an
earlier block would go on pointing at the stranded slot. That trades a
loud error for a silent one, which is the trade this codebase keeps
refusing, and it is a language decision rather than a repair. It wants
a person awake.

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

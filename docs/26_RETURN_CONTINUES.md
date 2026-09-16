# Phase 26 — the short program was right all along

A sketch, not a plan: it is written against a measurement whose second
half (the card ablation) is still running, and the decision it argues
for should not be taken until that lands. What follows is the shape the
evidence points at, and the reasons it would be a subtraction.

## What was measured

`dead-code-sweep`, n=3 a side, same model, same fixture, same
confinement, same checker (25.6b):

  median      trips  9 -> 2        in 33,347 -> 20,081 tokens
              out 2,516 -> 60,059  56.5s -> 349.6s     3/3 -> 0/2

Round trips fall 4.5x and input tokens fall 40% — both real, both
smaller than the round-trip ratio implies, because 86% of what a tool
loop re-sends is a cache hit. Output tokens rise 24x, and nearly all of
that is reasoning: 206KB of thinking against 10-27KB of program,
against pi's 5.8KB.

## What that says

The cost is not "programs". It is **anticipation**. A tool loop decides
the next move holding the last result; a program that must handle a
whole task decides every branch in advance, including the ones it never
reaches, and reasoning is what that costs. Phase 23 spent itself
fighting the model's instinct to write short programs. The instinct was
right, and the card grew 36% in a day defending the wrong side of it.

## The shape

Keep `run_program` as the *only* path to tools — that is the half that
pays, and it pays independently of round trips. A tool loop pastes every
result into context verbatim, which is most of pi's 25-47k input
tokens; a program reads three files, keeps the two lines that matter,
and returns those. "Nothing enters a context unchosen" survives intact
and is strengthened: the program chooses.

Then stop fighting the short program. The common path becomes a couple
of tool calls, a little glue, and a value returned to context. A
batching loop stays available for a uniform sweep, where it genuinely
pays, but nothing compels one.

**The vocabulary change that makes it work is one rule:**

    return <value>   continue, with that value in the document
    return / falling off the end   the task is finished

Today a root program's return value is read by nobody and
`next_program(payload)` is the only continuation. Under the rule above
they are the same act, and `next_program` disappears into `return`.

## What it deletes

- `next_program` — the verb, its reserved condition name, the
  `Disposition::Handover` special case in `suspend()`, and the
  `prompt_if_needed` call that had to be bolted onto it.
- From the card: "There are two ways to stop, and only one of them
  continues"; "Blocked is not done"; "twice on the same obstacle is a
  treadmill"; "One program is not your whole budget"; the
  loop-versus-handover discriminator and its "if you can write down what
  the next program should do, you can write the program"; "hand over the
  smallest thing that lets the next writer choose", which becomes
  ordinary advice about a return value.
- Every card paragraph added on 2026-09-16 that exists to make one
  program carry a whole task.

What stays is what the model cannot guess: the response format, the
verb list, `Edit.*`, the three silent dialect divergences, and the fact
that a program's return value is what continues.

## The ablation result — over half the cost was ours

`minimal` (5.6KB of card against 17.3KB, every line of engineering
guidance removed) against the current card, same tasks, n=3:

  dead-code-sweep      current  ->  minimal
    passed             0/2      ->  1/2
    reasoning          170.1KB  ->  77.3KB     (-55%)
    output tokens      51,140   ->  22,214     (-57%)
    prompt tokens      20,116   ->   9,258     (-54%)
    calls per program  24.7     ->  20.0

  whole suite          8/9      ->  8/10

**More than half the reasoning blowup was self-inflicted.** Cutting
two thirds of the card cut the cost roughly in half, improved the pass
rate on the hardest task, and — the part that matters most — *kept the
batching*. Calls per program barely moved, so the model still writes
the loop; it simply stops reasoning at length about controls, stale
offsets and broken baselines before doing so. Every one of those
paragraphs was added on 2026-09-16 in response to a single failing run,
and together they were costing more than they bought.

So the answer to "intrinsic or card-induced" is *both, and mostly
card*. That moderates this sketch rather than confirming it: the
current architecture is substantially cheaper than it looked an hour
ago, and the honest next step is a card cut, not a rewrite.

What survives the moderation is the direction. 77KB of reasoning is
still thirteen times pi's 5.8KB, and pi was 3/3 at a 56-second median
against our 1/2 at seven minutes. Halving a 24x gap leaves a 13x gap.
The short-program instinct still looks right; it is just no longer
obvious that `return`-continues is the only way to get there, when
deleting prose got half of it for free.

## What would refute it

**The ablation, still running.** Two explanations predict opposite
things. If `minimal` (5.6KB against 17.3KB) collapses the reasoning,
much of the 206KB was self-inflicted by our own prose and the current
architecture may be fine as it stands. If `minimal` reasons just as
hard, the cost is intrinsic to anticipating a whole task and this phase
is the answer.

**A second task.** `skipped-tests` is the same shape on another
surface, and both agents can run it. One task cannot separate "this
design" from "this fixture".

**The risk in the middle ground.** If the model, freed from the
pressure to batch, never writes a loop even where one plainly pays,
then the input saving shrinks toward a tool loop's and the whole
exercise is a slower tool loop. The measurement for that is calls per
program on `dead-code-sweep`: a sweep of eight candidates should still
produce one loop, not eight programs.

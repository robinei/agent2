# agent2 against pi, one task, 2026-09-21

`sweep-8` (dead-code analysis over 4 files), `deepseek-v4-flash` via
`opencode.ai/zen/go/v1`, same model and same confinement both sides,
`thinking: high` on both — pi's session records
`thinkingLevel: "high"`, so the reasoning gap is not a configuration
difference.

**n=1 per arm.** Both passed, so nothing here speaks to reliability;
the repo's own `--pool` help notes that n=7 per task does not resolve a
five-point difference. What this measures is shape and cost.

|                    | agent2 (code) | pi (tool loop) |
|--------------------|---------------|----------------|
| verdict            | pass, 100%    | pass, 100%     |
| round trips        | **3**         | 6              |
| calls per trip     | 3.3           | 1.3            |
| wall clock         | 137.2s        | **32.1s**      |
| tokens in          | 25,539        | **16,319**     |
| uncached in        | 9,667         | **3,775**      |
| tokens out         | 7,380         | **1,273**      |
| reasoning          | 24,191 B      | **1,796 B**    |
| recorded cost      | ~4x (est.)    | **$0.00137**   |

## The reasoning is one turn, and most of it is not the task

Per reply: 298 B, 659 B, **23,234 B**. pi: 81, 991, 695, 29 B.

agent2's first ~1.5 KB reaches exactly the conclusion pi reaches in its
991 B block — that `helper_006`/`007` are live through
`getattr(helpers, "helper_" + i)` and only `000`–`003` are dead. Both
correct, both for the right reason.

The other 21.8 KB, by line (257 lines total):

- **85 lines** on the task itself (pi: 12 of 17).
- **28 lines** of harness bookkeeping — which row holds what, whether
  to `history.remove(25)`, where to put it relative to `tell`/`append`/
  `finish`. **pi: zero.** It has no rows to manage.
- the remainder: **six fenced `js` drafts of the program**, rehearsed in
  the thinking before one was emitted, plus meta-reasoning about what
  the *grader* wants ("maybe the harness runs lint.py and expects it to
  report nothing?").
- 23 x "wait", 15 x "actually", 3 x "hmm".

**The card forbids the largest component.** "Thinking is not writing,
and only writing survives... So do not rehearse a block — write it."
It rehearsed six times. Same family as the 15.2% of prose parts that
copy back a `↓ history[N]` the card also forbids: an instruction
losing to the shape of the work.

## What this does and does not show

It does **not** show "code mode costs more reasoning because a program
is riskier and the model insures with foresight". That was the first
reading and the text does not support it: the five guards in the final
program are cheap, a few lines. The premium is rehearsal and
bookkeeping, and both are addressable — one by the card, one by the
design.

It does show the batching thesis working: 3 round trips against 6, and
one of agent2's three was wasted (an `outline` whose information the
next turn's full `read_file` superseded — the re-read pattern, caught
in the act). The achievable count was 2 against 6.

**Where the crossover sits is the question worth measuring.** A round
trip on flash is ~5s of overhead, so halving them saves little and the
reasoning premium dominates. On the LAN box at 4-9 minutes a turn, 2
trips against 6 is eight minutes against thirty and the premium is
irrelevant. "Which is better" is the wrong question; "at what
round-trip cost does code mode win" is the one this can answer.

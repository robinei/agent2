# Does supervision actually work?

Live two-child probe, `gpt-5.6-sol`, before designing a task around it.
Everything measured, nothing assumed.

## What works

- **Children run concurrently.** Both helpers issued their `bash` call
  at +38.8 s, from one `tell` each.
- **`tell("parent", …)` is reachable and was used unprompted.** The
  orchestrator wrote it into its children's instructions within an hour
  of the card first mentioning it. Their reports arrived on the
  parent's branch.
- **The parent polls while they work.** `list_agents` at +85.5, +91.4,
  +94.4 s with `wait_until` between — a real loop, and the rows carried
  `parent`, `status` and `open`.

## What does not

**`status` is too coarse to supervise on.** Every poll returned
`idle`, and it was right each time: a child between steps is
indistinguishable from one with nothing to do. The model said so
itself —

> The first poll happened before either helper had begun processing its
> instruction, so that timeline was premature.

`branch_status` reports what the *runner* is doing this instant
(`running`, `thinking`, `suspended`, `idle`, `dormant`), and a child
spends most of its life between those. Polling it yields a timeline
that mostly says nothing.

**So a supervision task must turn on `open`, not on `status`.** A child
blocked on a question is unambiguously stuck, unambiguously its
parent's problem, and the count is exact. That is the field the card
did not declare until today, and the reason it now says: *a supervisor
that polls `status` alone sees a stalled child as merely quiet.*

## What the task therefore needs

Children that **get stuck and ask**, not children that merely take a
long time. Long work alone is answered by `Promise.all`
(`supervise-builds` measured exactly that). The shape that needs a
supervisor is one where a child cannot finish without a decision only
the parent can make — and then the loop, `open`, and
`answer(question, value)` are all doing work that nothing else can do.

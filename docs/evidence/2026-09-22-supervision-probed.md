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

## What does not — retracted

The first version of this file said `status` was too coarse to
supervise on, because every poll in the first probe returned `idle`.
That was a non-observation generalised into a finding: the parent's
loop had a 47-second gap and **no poll ever overlapped the work**, so
`idle` was correct each time and nothing was ever learned about a busy
child.

Polled tightly, `status` is exact:

    +12.6s  idle       told, not yet started
    +15.1s  thinking   awaiting a completion
    +17.6s  running    and stays running for the whole 25s job
    +40.2s  running

It reports `idle` only when a child is genuinely idle.

## The five statuses, and the one the card invented

| status | `Runner::status` | means |
|---|---|---|
| `running` | `Phase::Running` | a program of its own is executing |
| `suspended` | parked frames | a program stopped part-way, waiting for an answer |
| `thinking` | `Phase::AwaitingLlm` | waiting on a completion |
| `idle` | `Phase::Idle` | a live runner with nothing to do |
| `dormant` | no runner at all | not loaded this session; wakes when spoken to |

`idle` and `dormant` differ in whether the branch is *loaded*, not in
whether it has work. A child that finished and reported is `idle`; one
never yet spoken to, or from a session rebuilt off a log, is
`dormant`, and nothing is lost either way.

The card briefly listed a sixth, `"returned above"` — which is the
panic message in `Runner::status`'s `unreachable!()` arm, scraped out
of the function as though it were a value. A test now refuses it.

## What this means for the task

**`suspended` and `open` are the supervision signals**, and they agree:
a child parked on `ask("parent", …)` holds a frame *and* carries an
open question. `running` says it is fine; `suspended` with `open > 0`
says it is stuck and the parent is the one holding it up.

Which still points the same way the first version did, for a better
reason: the task wants children that **get stuck and ask**, not
children that merely take a long time. Long work alone is answered by
`Promise.all`, as `supervise-builds` measured.
## What the task therefore needs

Children that **get stuck and ask**, not children that merely take a
long time. Long work alone is answered by `Promise.all`
(`supervise-builds` measured exactly that). The shape that needs a
supervisor is one where a child cannot finish without a decision only
the parent can make — and then the loop, `open`, and
`answer(question, value)` are all doing work that nothing else can do.

## Measured: what a child blocked on its parent actually reports

A child was spawned, told to `ask("parent", …)`, and deliberately left
unanswered while the parent polled ten times.

    +11.6s  child: idle,    open 0
    +17.2s  child SENDS  → {"Branch": 1}  expects_reply: true   (post #31)
    +30.7s  child: running, open 0
    …
    +45.7s  child: running, open 0

**`ask("parent", …)` works.** The question lands on the parent's
branch as an ordinary post that expects a reply. The child even
escalated on its own: *"Please answer open question #30 with the units
for widget_count; I will not guess."*

**But the roster shows nothing.** A child waiting on its parent reads
`running`, not `suspended` — an unanswered `ask` is a promise its
program has not settled, not a parked frame — and `open` stays `0`.

`open` counts what that agent **owes**: questions put *to it* that it
has not answered. A child waiting on you is your debt, not its own.
The card said the opposite until this probe, which is the third thing
that sentence got wrong after inventing a status and omitting three
fields.

**And nothing is missing.** The signal a supervisor needs is not in
the roster and does not need to be: the child's question arrives on
the parent's own branch with an `[id]`, in its `# NEW EVENTS` and its
tail, and `answer(id, value)` discharges it — the same path a question
from the user takes. `list_agents` is for *who exists and what they
are doing*; the parent's own open list is for *who is waiting on me*.

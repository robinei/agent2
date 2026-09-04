# Phase 18 — Answering is a declaration

The experience this phase is for: **an orchestrator that can talk without
accidentally answering.** A branch may owe an answer to a suspended
subagent while a human types an unrelated line into the same branch. The
agent replies conversationally, as models do, and today that reply is
silently delivered to the subagent as the return value of its `ask`. The
subagent resumes on prose that was addressed to someone else, and nothing
anywhere records that this happened.

The fix is a removal, not a mechanism. **A bare turn stops meaning "here
is a value."** It goes back to meaning exactly one thing: here is text, I
am done. Every `Answer` in the log then exists because the model called
`answer(question, value)` and said so.

## The problem, precisely

`Runner::apply_turn` (`machine.rs`) routes a turn with zero tool calls
into `Runner::answer_open`, which binds the reply to
`open.iter().find(|id| id.as_u64() <= self.shown)` — the oldest open
post, unconditionally. `open` holds posts from two unrelated kinds of
asker:

- **A program.** A subagent's `tools.ask` leaves the caller's tool call
  pending in `deliver_send` (`host/mod.rs`) until an `Answer` lands, and
  that value becomes the return value of the `ask` expression inside the
  running program. Getting this wrong is a **silent, irreversible**
  corruption of machine control flow.
- **A human.** Nothing is suspended. The human reads the turn's text
  regardless of what the binding does, because `StepOutput::Answered`
  carries the text to the client either way. Getting this wrong costs a
  confusing line the human can respond to.

One binding rule serves both, and it cannot tell them apart. "Oldest" is
fair bookkeeping; it is not a statement of intent.

**Worse, the log cannot distinguish a targeted answer from an oblivious
one.** A correct binding and a lucky arrival order produce identical
bytes. That is the same defect 17_BRANCHES Step C3 named when it refused
to let an agent silently decline — "an absence cannot be told apart from
a crash, an interruption, or a model that lost track" — applied one level
up. C3 fixed the decline case with an explicit event and left the answer
case implicit.

## The rule that settles it

> **A bare turn answers nothing. `open` is discharged only by
> `answer(question, value)`. A branch may be idle and still owe.**

Three consequences, each of which is a deletion:

1. `answer_open` stops appending an `Answer`. It keeps only what a bare
   turn genuinely does: abandon a suspended program, go idle, hand the
   text to the client. Its name goes with its old job.
2. `needs_prompt`'s standing-state clause goes. Waking a branch because
   something is *still* open re-prompts for a cause already shown, which
   contradicts that function's own stated rule ("never twice for the same
   thing: `shown` advances at each render"). C3 added the clause as a
   patch for stranding; with nothing auto-closing, the patch would be an
   unbounded prompt loop. Deleting it restores the stated rule.
3. The TUI's Enter stops creating obligations. A human's ordinary line
   becomes a `tell`, which is what every other harness does and what the
   muscle memory expects. Alt+Enter becomes the deliberate ask.

Consequence 3 is what makes 1 and 2 comfortable rather than noisy. With
Enter asking, every casual line would sit in `open` forever waiting for
an `answer` call that is pure formality. With Enter telling, `open` comes
to mean almost exactly "a program is suspended waiting on this value" —
a small, rare, genuinely load-bearing set.

## What replaces the liveness C3 was buying

C3's objection to a branch that goes idle owing an answer: "an
agent-authored post stranded that way is a `Send` that never settles — a
program parked forever, which is the deadlock rule B claims to have ruled
out." That objection buys liveness with correctness, and this phase
reverses the trade deliberately. A parked program is **visible**; a
program resumed with the wrong string is not.

What keeps a parked ask findable, all of it already built:

- `Unmatched::OwedAnswer` (`tree.rs`) and `Unmatched::PendingAsk` name
  both halves at reconciliation.
- `Runner::unrendered_cause` returns `open().first()`, and reconciliation
  calls `Runner::owe_prompt` to lower `shown` so a re-opened log renders
  the owed post again. **Reopening is a new fact, so this is a legal wake
  and it stays.**
- `BranchInfo.open` (`host/protocol.rs`) drives the navigator's
  `· N open` badge, so a branch owing something says so on screen.
- The request tail lists every open post on every request the branch
  does render.

The disagreement C2 found was between two meanings of "live":
reconciliation meant *has unfinished business you can see*, the session
meant *will be prompted again*. C3 settled it by making the session agree
with reconciliation. This phase settles it the other way, by letting the
two mean different things — which is what they always did.

## Alternatives, closed

Recorded so this is not re-litigated. These were worked through in full
before this plan; do not reopen them mid-implementation.

- **Bind to newest instead of oldest.** Still a guess, and it reopens the
  starvation C3 measured.
- **Guess the target from the reply's content.** The prose-parsing
  pattern 8_HARNESS decision 1 rejects.
- **Refuse a bare reply when several posts are open.** The refused turn
  produces nothing, so the model retries into the same state. Note the
  contrast with this phase: here a bare reply is perfectly *legal*, it
  simply does not answer. That is why one deadlocks and the other does
  not.
- **A mandatory `[@4]` targeting prefix on every plain reply.** Sound in
  principle, and two of the usual objections to it are weak: a fixed
  prefix at position zero is a wire format rather than inference, and
  models emit it reliably. It loses on scope. It pays a new failure mode
  on every turn to buy correctness on the rare ambiguous one, and it puts
  structured intent in the text lane when `answer(question, value)` is a
  validated typed lane already sitting there.
- **A per-turn "answering nothing" declaration.** Unnecessary once a bare
  turn answers nothing by definition. That *is* the null declaration.

## The removal ledger

- `Runner::answer_open`'s `Answer` append, its `open`/`shown` lookup, and
  the name `answer_open`.
- `needs_prompt`'s `if !self.open().is_empty() { return true; }` clause
  and its comment block.
- `request_tail`'s index-zero special case marking a privileged first
  entry, and the `enumerate` it needs.
- The card's "a plain reply with no tool call always answers the
  **oldest** open post automatically" sentence and its test needle.
- `docs/ANSWER_TARGETING_EVAL.md`, superseded by this file.

## What this deliberately does not add

- **No targeting prefix, no tag format, no prose parsing.** See above.
- **No decline event.** A bare turn is the null declaration; a separate
  one would be a second way to say the same thing.
- **No timeout or auto-escalation on a parked ask.** It is visible in
  four places already. Add one only if live use shows the visibility is
  insufficient, and add it as its own phase.
- **No change to `answer`'s eligibility, batching, or refusal
  behavior.** `Runner::eligible`'s `Restart::Answer` arm is correct and
  stays untouched.

---

## Ground rules

- Commit per step, `targeting:` prefix.
- Gate after every step: `cargo fmt && cargo clippy --workspace
  --all-targets && cargo test` — all three green, zero warnings.
- **Finish a step before starting the next.** Do not batch steps into one
  commit, and do not start a step whose predecessor's gate is not green.
- **Step B1 is indivisible.** Its two halves are mutually load-bearing:
  the binding change alone loops forever, and the wake-clause deletion
  alone strands posts. Neither half has a green gate on its own. Land
  them in one commit.
- No network in any test; `ScriptedLlm` drives everything new. Live
  verification is the user's to run and is called out where it matters.
- `14_CLEANUP.md`'s lint rules still apply.
- When a comment explains *why* a line exists and the line changes, the
  comment changes with it. A stale rationale is worse than none here,
  because the next reader will trust it.

---

## Part A — The default: a human message tells

### Step A0 — DESIGN.md amendment

- [x] DESIGN.md's "Exchanges" section (line ~113) gains the rule: a bare
      turn answers nothing, `open` is discharged only by `answer`, and a
      branch may be idle while owing. State the reason in one sentence —
      an implicit answer cannot be told apart from an oblivious one — and
      name it as the same reasoning C3 used to refuse a silent decline.
- [x] The "user is an author, not an agent" subsection (line ~187) gains
      one sentence: the user's default send is a **tell**, and an ask is
      a deliberate gesture, because a human has recourse to ask again and
      a suspended program does not.
- [x] The roadmap range on line 6 becomes `0_…` – `18_…`.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step A1 — Enter tells, Alt+Enter asks (`debug/attach.rs`, `main.rs`)

Independent of Part B and landed first, so B's tests run against the
traffic shape the design assumes.

- [x] `attach.rs` `on_input_key`: the `KeyCode::Enter` arm's
      `expects_reply: !key.modifiers.contains(KeyModifiers::ALT)` loses
      the `!`. Update the adjacent comment — it currently calls ALT "the
      tell modifier" and cites 17_BRANCHES.
- [x] `main.rs`'s non-TUI kickoff (`SessionCommand::UserTurn` with
      `expects_reply: true`, ~line 232) becomes `false`, so the CLI is
      not the surprising one. *(Deliberate: a kickoff line is a task
      instruction, not a question, and the agent's reply reaches the
      client either way.)*
- [x] The footer hint (~line 1153) becomes
      `enter send (alt+enter ask)`.
- [x] **Do not touch `resolve_submit`.** Its `asking_user` short-circuit
      to `SessionCommand::Reply` is a different axis — the human
      answering the agent, not asking it — and it correctly wins over
      both defaults. `reply_mode_wins_over_ask_or_tell_when_a_branch_is_waiting_on_you`
      must still pass unmodified except for wording in its doc comment.
- [x] `alt_enter_is_the_tell_modifier` is renamed and inverted:
      `alt_enter_is_the_ask_modifier`, asserting plain Enter yields
      `expects_reply: false` and Alt+Enter yields `true`.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

**Superseded (post-phase revision).** Alt+Enter turned out not to be a
reliable gesture in practice: many terminals and window managers claim
it for their own fullscreen toggle before it ever reaches the app, and
modifier+Enter chords are ambiguous in legacy terminal mode generally
(Enter's own control code already occupies the byte a modifier would
need to alter). The ask gesture is now a dedicated key (`a`,
`AttachedApp::arm_ask`) that arms the input line the same way the
rename/resume/rewrite/spawn keys already did, rather than a modifier
read off the Enter keypress itself. Enter still tells by default.

---

## Part B — Answering is a declaration

### Step B1 — The bare turn stops answering; the wake rule loses its standing clause (`machine.rs`, `host/dialect.rs`)

**One commit. Read the ground rule above before starting.** Expect a red
tree in the middle of this step; that is the step, not a mistake.

- [x] `Runner::answer_open` is renamed `Runner::go_idle` and keeps only:
      abandon a `Phase::Suspended` program (`note_status(...,
      ProgramStatus::Failed)`, stash `last_vm`), bump `generation`, set
      `Phase::Idle`, emit
      `StepOutput::Answered { question: None, value: text_value(&text) }`,
      then `prompt_if_needed`. The `tree.append(EventPayload::Answer)`
      call, the `open`/`shown` lookup, and the two-branch split all go.
      *(The surviving body is exactly today's "nothing was open" arm.)*
- [x] `StepOutput::Answered.question` **stays** — `machine.rs:~1176`, the
      `answer` restart path, still produces `Some(_)`. Only the bare-turn
      producer stops.
- [x] `Runner::needs_prompt` loses `if !self.open().is_empty() { return
      true; }` and the comment block above it. Its doc comment's trigger
      rule is restated without the open-post clause, and the "never twice
      for the same thing" sentence is now literally true again.
- [x] **`unrendered_cause` and `owe_prompt` stay exactly as they are.**
      Reopening a log is a new fact, so re-rendering an owed post once
      per session open is a legal wake, and it is what keeps a parked ask
      from disappearing. Do not "simplify" `open().first()` out of
      `unrendered_cause`.
- [x] `dialect.rs`'s `## eligibility` section: the sentence beginning "A
      plain reply with no tool call always answers the **oldest** open
      post automatically…" is replaced by the new rule — a plain reply is
      text and answers nothing; a post stays open until `answer(question,
      value)` names it; several answer calls may ride one turn. Keep the
      existing `[#id]` sentence and the batching sentence.
- [x] The card test's needle list (`dialect.rs`, ~line 461) drops
      `"always answers the **oldest** open post"` and gains a needle from
      the new sentence.
- [x] `request_tail`: the index-zero special case and its `enumerate` are
      deleted, so the ids render as a plain list. The sentence says the
      posts stay open until `answer` names them. *(Enrichment is B2; this
      bullet is only about not lying.)*
- [x] `a_fan_in_of_asks_is_answered_to_the_last_one` is rewritten and
      renamed — four posts are now discharged by four `answer` calls, not
      by four bare turns — and it keeps its tail assertion that the
      branch owes nothing at the end.
- [x] New: `a_bare_turn_leaves_every_open_post_open` — one open post, a
      bare turn, assert `open()` is unchanged, no `Answer` event was
      appended, and `StepOutput::Answered { question: None }` still
      carried the text.
- [x] New: `a_branch_that_owes_an_answer_goes_idle` — the anti-loop test.
      After the bare turn above, assert `is_idle()` **and**
      `!needs_prompt(&tree)` with a post still open. This is the test
      that would have caught the unbounded prompt loop.
- [x] New: `a_reopened_log_re_renders_an_owed_post` — or extend the
      existing reconciliation coverage — pinning that `owe_prompt` still
      gives an owed post one render per session open now that nothing
      else wakes it.
- [x] `explicit_answer_binds_the_named_post` and
      `ineligible_restart_reports_and_recovers` must pass **unmodified**.
      If either needs changing, the change went further than this step.
- [x] `many_open_posts_are_noted_in_the_request_tail` and the
      `deepseek.rs` request-body fixture both embed the old tail string
      and must be updated to the new wording here.
- [x] Grep for stale prose: `grep -rn "oldest" agent/src` should return
      no claim that a bare reply binds to the oldest open post.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step B2 — The tail names who is waiting (`machine.rs`, `report.rs`)

Additive, and only worth doing after B1 makes `open` rare: a line that
fires every turn gets tuned out, a line that fires only when something is
genuinely parked gets read.

- [x] `request_tail`'s open-post line names each post's **asker** beside
      its id, resolved the way `render_post` resolves it (`Author::User`
      / `Author::Harness` / `Author::Agent(id)`). Today the tail is bare
      ids and the author appears only in the body prefix, so the model
      must scan upthread to tell a parked program from a person.
- [x] The wording carries the consequence, not the policy: what is
      waiting and what frees it. A post from an agent means that agent's
      program is suspended on this value and stays suspended until
      `answer` names it. **The rule itself stays in the card** — the
      cache-discipline split from C3 puts rules there and now-facts in
      the tail, and restating the policy every turn spends tokens on
      something that never changes.
- [x] `OPEN_NOTE_MAX_IDS` and the `, and N more` overflow still bound the
      line; asker names must not let it grow unbounded.
- [x] Presence still goes last. Extend
      `many_open_posts_are_noted_in_the_request_tail` rather than adding
      a parallel test, and keep its `ends_with(ABSENT)` assertion.
- [x] New: a two-asker case — one open post from the user, one from an
      agent — asserting both are attributed in the tail.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step B3 — `Context::input` binds the oldest *open* post (`types.rs`)

Adjacent, pre-existing, and cheap. Its doc comment already claims the
behavior this step implements.

- [x] `Context::input` currently scans `messages` for the first
      `Post` whose `origin.direct()` reports `expects_reply`, regardless
      of whether it is still open. After the first answer it therefore
      keeps binding to the answered post. Rewrite it to resolve
      `open.first()` and return that post's `input`, `Value::Null` when
      nothing is open.
- [x] New test: two open posts, answer the first explicitly, assert
      `input()` moves to the second; answer that one, assert `Null`.
- [x] *(If this turns out entangled with something not named here, stop
      and report rather than widening the step. It is a coherence fix,
      not a prerequisite for anything else in this phase.)*
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

---

## Part C — Docs sweep

### Step C1 — Record the change where the old rule is stated

- [x] `docs/17_BRANCHES.md` Step C3 gains a short **superseded** note
      under its existing text: the auto-close was the termination
      argument for the open-post wake clause, 18_TARGETING removes both,
      and the two meanings of "live" are now allowed to differ. **Do not
      edit C3's checkboxes or rewrite its history** — this file records
      what was built and when.
- [x] The same file's "Answers" section gains a one-line pointer to
      18_TARGETING for the current binding rule.
- [x] `docs/ANSWER_TARGETING_EVAL.md` is deleted. Its question is
      decided, and leaving a file marked "unresolved, not scheduled"
      invites someone to build the tag prefix later. The alternatives it
      weighed are preserved in this file's "Alternatives, closed".
- [x] `grep -rn "a plain reply answers\|answers the oldest" docs/` returns
      only historical Step C3 text, now marked superseded.
- [x] Gate: `cargo fmt && cargo clippy --workspace --all-targets &&
      cargo test` green.

### Step C2 — Live verification (user-driven, DeepSeek)

*(Left unchecked: it needs the network and is the user's to run.)*

- [ ] Drive `cargo run -p agent -- session --real`. Spawn a worker that
      asks the orchestrator something. Before answering, send an
      unrelated line on the same branch with plain Enter. Confirm: the
      line lands as a tell and opens nothing; the orchestrator's
      conversational reply closes nothing; the worker's post is still
      listed in the next request's tail with the worker named; an
      `answer` call resolves it and the worker proceeds. Record the
      transcript note here.
- [ ] Confirm the negative case too: the orchestrator goes idle owing the
      worker's post, the navigator shows `· 1 open` on that branch, and
      the session does not spin re-prompting it.

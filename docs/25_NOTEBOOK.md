# Phase 25 — the response is a notebook

The model's reply stops being a bare JavaScript program and becomes
**markdown containing executable code blocks**. Prose is prose, reaching
the person as it streams; the code blocks are one *compilation that
pauses at each block*, sharing a frame and a scope. The reply is logged
piece by piece as it arrives (D15), and prompts exactly one next
completion however many blocks it held.

## Why

Three separate problems collapse into one answer.

1. **Streaming.** A person asking for a report waits for the whole
   completion, because the report is a string literal inside a program
   that has not run. Markdown prose renders line by line as it arrives,
   with no extraction, no marker convention, and no scanner.
2. **Escaping.** A markdown report inside a JS string literal is
   `\n`-soup at best, and *broken by its own code fences* at worst.
   Outside a literal, it is just text.
3. **The prior.** `document.rs`'s `extract_program` already concedes
   this fight: its doc comment says the card states absolutely that a
   completion is only ever valid JavaScript, "[b]ut a model habitually
   wraps its answer in a ```javascript fence anyway", so the harness
   silently strips one. The format the model reaches for is markdown
   with fenced code. Today's transport spends effort fighting that; this
   one spends it agreeing.

A fourth thing falls out. `//: ` narration (phase 23, deleted) and
phase 24's prefix dispatch of leading `tell()` calls both exist to get
*some* text to the person before the program ends. Neither is needed if
the text is not inside the program.

## The shape

````markdown
Both files claim to own the retry policy, so I will read them together.

```js
const a = await tools.read_file("client.rs");
const b = await tools.read_file("retry.rs");
history.append(`client.rs@${a.version}, retry.rs@${b.version}`);
```

`retry.rs` is the newer of the two, so it wins.

```js
await tools.replace_file("client.rs", Edit.replaceOnce(a.content, OLD, NEW), a.version);
tell("client.rs now defers to retry.rs.");
done();
```
````

One completion, logged as four events in this order: a `Send` carrying
the opening prose, a `Turn` holding cell 0, a `Send` for the middle
prose, a `Turn` holding cell 1 (D1, D15). Two cells sharing one frame —
`a` is visible in the second because the analyzer and compiler never
stopped between them (D12).

Note what cell 0 does *not* do: end with `return`. That is the habit
exemplar 02 teaches, and there is no `return` here at all (D5) — a cell
keeps what is worth keeping with `history.append`. The run ends by
running out of cells — `done()` does not end it (D8), it only decides
that the branch rests once it has.

## Decisions

### D1 — A reply is a run of events, in source order

**One completion is not one `Turn`** (see D15, which decided this). A
reply decomposes, as it streams, into its pieces in source order: each
prose segment is a `Call::Send { to: User }` — prose *is* a message to
the person — and each cell is a `Turn` whose `source` is that cell's
JavaScript.

`Call::site` keeps its meaning exactly — an offset into the owning
`Turn`'s `source` — because the cell's offset is **subtracted when the
call is logged**. D2's absolute spans exist so the *analyzer's* in-memory
tables do not collide across cells; nothing requires them to survive
into the log, and the reply is not stored whole for them to index. So
`Message::Turn` gains no field and the log schema does not change.

An earlier draft had each cell-`Turn` record its offset within the
reply. Unnecessary: the only consumers of a site — `report.rs`'s
per-call-site annotation, the caret diagnostic, the TUI's program pane —
all want a position within the program they are showing, which is the
cell.

What is given up: the reply is recoverable in content and order, but not
byte-for-byte. The fences and the whitespace between pieces are not
stored anywhere. Nothing downstream needs them — sites resolve per cell,
and the document renders the pieces in order — but a log no longer
reproduces the exact bytes the provider returned.

### D2 — One coordinate system: the markdown's own offsets

There is a single buffer, byte-for-byte as long as the reply and with
the same newlines, in which **only the cell being compiled is live**.
Every other byte is a space, except `\n`, which is kept. The parser is
handed that buffer, so the spans it emits are already offsets into the
markdown.

`oxc_parser` has no offset option — `Parser::new` takes only the source
text, and `ParseOptions` carries nothing for it (checked against
0.134.0) — so spans are relative to whatever `&str` it is given. The
only alternatives are to line the bytes up, or to walk the AST
afterwards adding an offset to every node's span, which costs a
`VisitMut` over every node type and a full tree walk to recover what
lining up gives for free.

**Newlines are preserved in the fill, not just byte count.** Byte-exact
alone would satisfy every consumer that goes through a span, but line
and column are computed from a source, and this way the parse buffer
answers those identically to the markdown too. Filling is byte-wise, so
a multi-byte character in the prose becomes that many spaces and the
length is exact; the fill is never read as text, only skipped.

It is one buffer, not one per cell. After a cell compiles, its bytes are
blanked by the same rule, and the next cell's text is written at its own
offset when its fence closes. One allocation amortized, and O(cell) of
filling per cell.

The cost is that the parser re-lexes the whitespace prefix each time, so
lexing is quadratic over the reply. This is the trade phase 24 already
accepted in writing — "re-parsing the accumulated prefix from scratch on
each chunk is quadratic over a few KB against a parser that runs at
MB/s, which is not measurable."

Why this matters beyond tidiness: both the analysis and the debug table
are *span-keyed*. `analyzer/mod.rs` resolves bindings "keyed by source
span", and `debuginfo.rs` identifies a function as "the innermost
function whose *source* span contains the instruction's span". Parse two
cells from their own substrings and both start at zero, so their
bindings and their function extents collide. Absolute spans are what let
the analyzer simply *accumulate* across cells (D12) rather than being
rebuilt and handed a carried table.

Note that these absolute spans are a *compile-time* device. They keep
the analyzer's tables from colliding; they are not what gets logged.
`Call::site` and `Condition::site` are written cell-local, by
subtracting the cell's offset at log time, so both keep the meaning they
have today — an offset into the owning `Turn`'s `source` (D1).

Rejected along the way, recorded because the middle one was wrong in an
instructive way:

- **Blank the whole markdown except this cell**, allocating a fresh
  full-length copy per cell. The right idea with two wasteful details,
  both fixed above: reuse the buffer, and keep the newlines.
- **Compile the bare substring and rebase spans afterwards** — add
  `cell.start` across `Program::spans` and the debug table. Looks
  cheaper and is not: a pass per cell, a special case for the appended
  prelude (whose spans sit past the user's text and would otherwise land
  in the following prose), and it does nothing about the span collision
  above, so the analyzer still has to be seeded by hand.
- **Cell-local spans**, as a notebook's line numbers are. Rejected
  because `site` is not only for error text: it is logged on
  `Call::Send` and `Condition` and read back to annotate a program per
  call site. Going relative would put a cell index on every
  span-carrying event in the log and teach every consumer to resolve it.

### D3 — ```js executes; quoting is the marked case

The alternative was to mark execution (```run) and leave ```js inert.
Rejected, on which case deserves the tax:

- Executing is what every turn does. Marking it taxes every turn, and a
  forgotten marker means a turn that says it acted and did nothing.
- Quoting code back to the person is rare — and *currently impossible*,
  since the whole reply is a program. It is a new capability, and new
  capabilities can carry the marker.

So ```js / ```javascript execute, in order, and a block the model wants
the person to *read* rather than run is tagged as anything else
(```text, ```rust, or a 4-backtick fence around it — the notebook prior
already handles this: quoted code lives in the markdown, not in a code
cell).

A **bare** ` ``` ` fence with no info string does *not* execute. That is
narrower than today's `extract_program`, which strips a bare fence along
with ```js and ```javascript, so it gives up some leniency against a
habit the model demonstrably has. It is the right trade here because a
bare fence is also how prose quotes anything at all, and the failure is
caught rather than silent: a reply whose only code sits in a bare fence
has no executable cell, so the turn simply rests (D4) and the person's
next message gets it moving again.

This is the decision most likely to be wrong, and it is cheap to
reverse: it is one predicate over the fence's info string.

### D4 — A reply with no cells rests the branch, and needs no rule

It is a complete turn: the model spoke and stopped. Nothing is refused,
nothing is re-asked.

**This is not a rule being added — it is what the harness already does.**
`finish_program` shows that resting is not a state:

```rust
if self.done {
    self.shown = self.spine.leaf_id.as_u64();
    self.done = false;
}
self.phase = Phase::Idle;
out.extend(self.prompt_if_needed(tree)?);
```

`done()` works *entirely* by advancing `shown` so `needs_prompt`'s
outcome clause goes false. Resting **is** "shown has caught up".

A cell-less reply reaches the same place by a different road. It logs
`Send`s and no `Turn`, so `unseen_posts` is empty — sends are not posts
— and `last_turn_outcome > shown` is false, because the newest `Turn` is
the previous reply's and was shown already. `needs_prompt` is false on
both clauses and the branch sits `Idle`. That is bit-for-bit the state
`done()` produces. **A reply with no cells is an implicit `done()`,
without anything being implemented.**

Two earlier drafts of this decision were wrong, in opposite directions:

- **"It goes to the repair loop like any other unusable completion."**
  Fighting the harness to refuse a turn it handles correctly — and, once
  D15 lands, re-asking after the prose has already been delivered, so
  the person may hear it twice.
- **"Not re-asking is worse: the branch goes quiet."** Quiet is what a
  finished turn looks like. Silence, the failure this project has
  measured, is a branch that produces *nothing*; a cell-less reply
  speaks and then rests, which is `tell(); done();` with the ceremony
  removed.

**And the original rationale overclaimed.** It cited the chat-drift
numbers (programs/run 8.9 → 3.5, read-tell-stop 60% → 0%) as though
requiring a cell prevented drift. It never could: a drifting model would
write `done();` in a cell and drift identically. Forcing a cell only
ever prevented the *silent* variant, and there is no silent variant.

What remains is a **metric, not a gate**. The share of replies with no
cells is exactly the chat-mode-drift measurement 25.8 wants, and it is
free to count. The actual guards against drift are the card and the
exemplars, which is where they always were.

### D5 — There is no `return`

A top-level `return` is a compile error in every cell, naming what to
use instead. This is the Python notebook rule exactly — `'return'
outside function` is a `SyntaxError` there, and a cell cannot return by
construction.

It is not a restriction so much as a verb losing its job. Under this
transport `return` did two things, and something else already does each:

- **End the program.** Falling off the last cell ends it; `done()` rests
  the branch.
- **Leave a row the next turn reads.** That is `history.append`, which
  is finer-grained anyway (a cell can append twice) and is what exemplar
  05 already teaches.

An earlier draft kept `return` legal in the final cell, so that a
one-cell notebook would behave byte-identically to today's program. That
bought migration convenience at the price of a rule with an exception,
and an exception the model would have to locate a cell boundary to
apply. Cut.

The diagnostic matters more than the rule, because ending a program with
`return {…}` is the *trained* habit — exemplar 02 teaches it. It must
say what to write instead, not merely that this is disallowed.

### D6 — Granularity comes from `history.append`

Cells add a *place* to put things, not a new way to record them.
`history.append` writes one row per call — finer than per-cell — and
exemplar 05 exists to teach exactly that, for exactly this reason (two
rows compact independently; one fat value does not).

Between cells the data channel is the shared scope. What belongs in the
log goes there by the verb that means it.

There is consequently no `EventPayload::Cell`. **A cell runs purely for
side effect.** It has no value — D5 removed the only statement that
could produce one — so there is nothing about "a cell ran" worth a row
that its own actions do not already say. Everything that matters is
logged as it happens, by the cell's own doing: a `Call` at dispatch, its
`Result`, a `Note` for `history.append`, a `Console` for `console.log`.
A cell is a region of instructions and nothing more.

A cell's `Return`/`Condition` (D7) is not an exception to this. It
records no value either — it records *how that run ended*, which is what
`Cause::Abandoned`'s invariant is about: a branch that suspended and a
branch that finished must be distinguishable on reload. Bookkeeping, not
data. `Return { value: null }` is now the only shape it takes.

### D7 — One terminal per cell, one report per reply

`Cause::Abandoned`'s doc states the invariant: *a run must have exactly
one log-visible terminal, or nothing downstream can be derived from the
log alone* — and `Return`'s states the completing half of it, that a
program ending without a `return` still logs `Return { value: null }`.
Cells must not break either.

Under D15 a cell *is* a run, so each cell logs exactly one `Return` or
`Condition` and the invariant holds per cell. Cutting the `return`
*statement* (D5) does not cut the `Return` *event*: its own doc already
says a program that ends without a `return` still logs
`Return { value: null }`, which is now simply the only case.

**What must not multiply is the prompting, not the events.** A reply
with three cells is still **one report and one next completion**, and
two things keep it that way:

- `needs_prompt` already returns false unless the branch is
  `Phase::Idle` — "Awaiting an LLM: a request is already out, and
  everything logged since will ride the next one." Outcomes logged while
  the reply is still streaming therefore cannot trigger anything.
- The gap is the *unconditional* path: `finish_program` and `suspend`
  render "bypassing this rule entirely", on the premise that a program
  running out is a turn running out. Under a notebook those come apart,
  and that path has to learn the difference between a cell ending — walk
  to the next one — and the reply ending.

That second point is owed by 25.4 whichever way D15 had gone; it is a
cost of notebooks, not of logging as you stream.

### D8 — `done()` does not stop anything, here or today

An earlier draft of this decision said `done()` ends the notebook and
later cells never run. That is not what `done()` does. `machine.rs`'s
`TOOL_DONE` arm is explicit: the flag is "recorded on the `Runner`
rather than answered-and-forgotten because the decision it feeds (rest
instead of continue) isn't made until the program's return value is
known, in `finish_program` — a program can call `done()` and then keep
running (more `tell`s, more calls) before actually returning".

So `done()` keeps its meaning exactly: a flag consulted when the run
ends, deciding rest rather than continue. **Cells after it still run**,
the same way statements after it still run today. Nothing changes and
nothing needs to.

This is also why cells need no terminating `Return` (D12): there is no
case where falling from one cell into the next is wrong. Normal
sequence *is* that fall-through — cell 1 is appended at exactly the ip
cell 0 stopped at. `resume(v)` after a condition falls out of cell *k*
into *k+1*, which is what D9 specifies. A closure from cell 0 called in
cell 2 enters the function's own body, not cell 0's top level. And
nothing spans cells, so no loop or `try` can jump backwards into an
earlier one.

The card must say what `done()` does *not* do, because the natural
reading is wrong and an exemplar written during this design got it
wrong: `if (bail) { tell("left it alone"); done(); }` followed by the
edit it meant to skip. Guard with `else`, not with `done()`.

### D9 — A raise or a trap suspends the notebook mid-cell

Also unchanged. `raise(...)` suspends with a handler frame; a trap
suspends resumably or not. Remaining cells do not run, because the VM is
parked inside cell *k*.

`resume(v)` then continues **from that instruction** and falls out of
cell *k*. The VM half of that is exactly what `resume` already does and
needs nothing new; what *is* new is that falling out of a cell returns
to the **cell driver** rather than ending the run, so the driver must
resume its walk at cell *k+1* rather than treating the handback as a
completed program. That is the same integration point D7 names, and it
is where the `finish_program`-renders-unconditionally behaviour has to
learn the difference between a cell ending and a notebook ending.

The semantics match the notebook prior: an erroring cell stops a Run
All.

### D10 — The kernel lives for one turn

Cells within a notebook share scope. Nothing survives to the next
notebook.

A persistent cross-turn kernel is the one notebook affordance this
design refuses, and the reason is the thesis: *nothing the model ever
saw depends on state outside the log* (`EventPayload::Condition`'s own
doc). A live kernel is exactly such state. Turn-scoped is also what the
model already lives with today.

### D11 — A cell executes the moment its fence closes

Not when the completion ends. A closing fence is decidable at the line
level with no parsing — three-or-more backticks at line start — which is
the property phase 24 could never get from a JS expression, where
`tell("a")` might still become `tell("a").then(...)`.

Cells run strictly in sequence: cell *k* finishes, awaits included,
before cell *k+1* starts, because cell *k+1* may read what it computed.
Generation continues meanwhile. On the measured workload that is the
common case by a wide margin — execution is 0.0–0.1s against 8.8–28.2s
waiting on the provider (three tasks, 2026-09-18).

Three consequences, and the second is the reason to do it:

- **Live output.** Prose, then a block, then its effects beneath it,
  then the next prose. The Jupyter reading experience, and the phase 24
  requirement — *a user must never have to read the generated JavaScript
  to know what is happening* — met by the format.
- **A suspension cancels the completion.** A trap or a `raise` in cell
  0 parks the VM, so no later cell can run until a handler resumes it —
  generating them is waste, and the harness can cancel the generation
  still in flight.

  **Not `done()`**, though an earlier draft said so. `done()` does not
  stop execution (D8), so later cells still run; cancelling on it would
  cut off the reply mid-sentence, and `tell` *then* `done()` is the
  taught shape — the prose the model is still writing is the answer the
  person asked for. The machinery exists:
  `llm_epoch` and `cancels` already do this for `Interrupt`, `LlmDone`
  already drops a response whose epoch has moved, and `deepseek.rs`'s
  `parse_sse` checks the token **between SSE lines** — "so an
  interrupted generation stops streaming within a chunk rather than at
  the end of a completion that may run for minutes."

  What that buys for certain is that the harness stops reading and
  closes the connection. Whether the *provider* then halts generation
  and stops billing is provider behaviour on disconnect, not something
  this repo can assert — usually yes for streaming, but **25.5 should
  measure it rather than claim it**. Either way concatenation cannot
  capture it at all.
- **Truncation becomes partial progress.** Today a truncated completion
  is never compiled and the whole thing is re-asked. Here the cells that
  ran stand, and the next completion continues with their results in
  hand. This retires `Cause::Truncated`'s current guarantee, and the
  card must stop implying it.

Two costs to accept honestly.

**Cells written on a false premise.** While cell 0 runs and fails, the
model is still writing cells 1–3 assuming it succeeded. Today the whole
program is written blind too, so this is not a regression — but the
failure is now visible mid-stream, which is new, and cancellation bounds
the waste rather than removing it.

**A call would outrun its own `Turn`.** `Call`'s doc pins its
placement: "Parent: the owning agent's spine, **between the program's
`Turn` and its eventual `Return`/`Condition`**." A cell dispatches calls
before the completion ends, so a single `Turn` holding the whole reply
could not be logged before them. **Decided in D15: the reply is logged
as it arrives**, so each cell's `Turn` lands before that cell runs and
the placement holds.

### D12 — One compilation that pauses

The `Analyzer`, its growing `ProgramAnalysis`, the `Compiler` and the VM
all stay alive for the whole reply and are fed each cell in turn through
the shared buffer (D2). **Nothing is carried between cells, because
nothing is rebuilt.** A cell boundary is not a compilation boundary — it
is a point at which the instructions emitted so far happen to be run.

That works because spans arrive absolute. The analysis table is
span-keyed, so it accumulates without collision; the compiler appends to
its own `code`/`spans`, so label ids backpatch against the same vector
they were emitted into and addresses come out absolute by construction.

One pass must be scoped to the newly appended range: **backpatch**,
which runs from the cell's first instruction onward. Cell 0's jumps hold
resolved addresses by now, not label ids, and a pass that cannot tell
the two apart would corrupt them.

**A cell must not end the way a program does.** `compile_program` emits
a trailing `Instr::Return(0)` — "Root frame ends with `Return(0)` →
`StepResult::Done`" (`compiler/stmt.rs`). Sent down that path, cell 0
would unwind the root frame on its way out and take every top-level
local with it, so cell 1 would find no `a`. That is the shared scope
this whole decision rests on, destroyed by the first thing anyone would
write.

So a cell's compilation omits the trailing `Return(0)` and ends with
`Instr::Pause` instead, which stops the VM **without unwinding** and
reports `StepResult::Paused`.

**Nothing sets `ip`.** A cell stops by running to the end of what
existed, so `ip` already points at the append position; appending the
next cell puts its first instruction exactly where the VM is standing.
The code vector grows in front of a VM that is already there. That is
what "one compilation that pauses" means mechanically, and it is why
the driver never needs to know an instruction offset at all.

An instruction rather than a stop-at-ip bound on `step`, for a reason
particular to this VM: **every semantic effect here is already an
instruction** — `Settle`, `Raise`, `FreshCell`, `EnterFrame`. The one
non-instruction pause is `OutOfFuel`, and that is a scheduling artifact,
not a semantic one. A fragment boundary is semantic, it is where the
compiler decided one ends, and the prologue is already in the stream
(`ExtendFrame`): a prologue of instructions with an epilogue living in
driver state would be asymmetric, and a bug in the driver's bound would
be invisible in a disassembly.

`OutOfFuel` is the precedent for the shape — "Nothing was consumed
(`ip` is at the next unexecuted instruction); call `step` again to
continue." `Paused` is that, minus the budget.

It also covers resume-after-condition with no special case: a trap in
cell 2 resumes, runs out the rest of cell 2, hits its `Pause`, and the
driver logs cell 2's terminal without reconstructing which cell it was
in.

Named `Pause`, not `Yield` (which reads as generator semantics to anyone
who knows JS, even though this VM has no generators) and not `Suspend`
(taken by conditions, `Phase::Suspended`). Per D16 it is named for
incremental evaluation, not for cells.

The *event* terminal (D7) is written by the driver on seeing `Paused`,
not by an instruction. Only the reply ending unwinds the frame — and
not `done()`, which stops nothing (D8).

#### Capture across cells, and the one thing that must move

Slot *indices* are stable — allocated in declaration order, so cell 1
resolves `a` to the slot cell 0 gave it and allocates its own names
above. Slot *representation* is not. `analyzer/captures.rs` assigns each
own-local a `SlotKind` from capture analysis: a slot captured by a
descendant closure is `Boxed`, one eager cell shared by reference, and
everything else is `Plain`, a raw value in the frame.

```js
// cell 0
let x = 5;
console.log(x);          // nothing captures x → Plain, a raw value
```
```js
// cell 1
const f = () => x + 1;   // x is captured now → this code expects a Box
```

Cell 0 wrote a raw value into a slot cell 1's closure expects to be a
cell. Boxing is a property of the binding decided by uses that may not
have been written yet — the one thing a widening window cannot settle
after the fact.

**The primitive for moving it already exists**: `Instr::FreshCell(slot)`.
Its doc comment describes re-boxing a captured loop local per iteration,
but the implementation is general — it reads the slot, dereferencing an
`Upval` if there is one and otherwise taking the raw value, allocates a
cell seeded with it, and stores `Value::Upval(idx)` back. On a `Plain`
slot that is exactly a value-preserving `Plain → Boxed` promotion.

**And the promotion set is a diff, not a rule.** `finalize_tables`
recomputes `slot_kinds` over the accumulated scopes, so re-finalizing
after each cell and comparing against the previous result yields exactly
the slots that flipped. Those get a `FreshCell` at this cell's start.
There is no "is this the first capture of an earlier name" logic to
write.

**Why promotion is sound.** A flip can only be caused by a *new* closure
in the cell being compiled: if any closure in an earlier cell had
referenced the name, that reference was itself a capture and the slot
was `Boxed` from the start. So every instruction compiled against the
`Plain` representation is straight-line code in a cell that has already
run to completion, and nothing that could observe the old
representation survives the boundary.

The ordering invariant it rests on: promotion is emitted at cell
*start*, and cells run strictly sequentially (D11). A fence can close
while the previous cell is still suspended on an await, so a cell may
**compile** early — but its first instruction does not **execute** until
the previous cell has finished, so that cell's post-await tail never
reads a slot promoted underneath it.

#### Drafts that were wrong, and why

Three, each inventing machinery to fix a problem the previous one had
created. Recorded because the errors are the useful part.

1. **A run-lived scope map with runtime name resolution**, on the
   grounds that whole-program analysis could change codegen for code
   that already ran. That objection applies to *recompiling the prefix*,
   which nothing here does. It would have cost a hash lookup per access
   and compile-time name resolution to solve a problem that was absent.
2. **Boxing every top-level slot unconditionally**, to pin the
   representation before any cell ran. The right problem, but
   `FreshCell` already moves a slot, so the common case need not pay.
3. **Compiling each cell standalone and appending its `Program`**, which
   made jump targets cell-relative and needed a base address threaded
   through label resolution — and left the span collision untouched, so
   the analyzer still had to be seeded by hand. Emitting into the shared
   buffers in the first place means neither problem arises.

Worth checking during 25.2 rather than assuming: `NameRes::Const` — a
const binding folded at compile time that "never reaches the frame" —
should accumulate safely, since the analysis table simply keeps it and
later cells fold it identically. Loop-declared `FreshCell` slots do not
arise at cell top level.

### D13 — The TUI collapses cells

A code block renders semi-collapsed by default — a few lines and a count
— and expands on a keystroke.

After a cell has run, what matters is its *effects*: the calls it made,
what they returned, what it logged. Those are rows the TUI already
renders. The source is how it got there, and it is the least interesting
thing on screen for the person who asked a question. Collapsing is what
makes "never have to read the generated JavaScript" true in practice
rather than only in principle.

### D14 — `Message::Turn`'s doc comment becomes false

The type is well named, and D15 keeps it that way. `Message::{Post,
Turn}` splits incoming from "this context's own output (assistant
role)", and under D15 a cell-`Turn`'s `source` holds JavaScript exactly
as it does today — so the rename this decision originally called for
(`source` → `text`, because it would have held mostly prose) is **not
needed**. Logging the reply as it arrives keeps the field honest.

Three clauses of its doc comment still go from explanatory to false, and
must be rewritten:

- "there is no separate prose channel and no tool-call wrapper around
  it" — there is now: the prose segments of the reply, logged as
  `Call::Send { to: User }`.
- "A program that wants to speak calls `tell()`/`ask()` from inside
  itself" — no longer the only way, and no longer the usual way.
- "it never returns prose alongside a list of calls, because there is no
  second channel for the prose to live in" — that second channel is
  precisely what this phase adds.

**And a deletion falls out.** The same comment ends: "A compacted
program still lands here as a comment-only `source`, which is what keeps
role alternation intact under compaction with no special case", and
`document.rs`'s `compacted_program_comment` wraps the replacement text
as `//: [17] … text`. That wrapper exists **only** because the assistant
slot had to hold valid JavaScript. Under D15 a compacted *prose* segment
is just prose in a `Send`, and a compacted *cell* keeps the comment form
it already has. The `//:` marker survives for cells and disappears for
prose, which is the first time in this design it has had a coherent
scope.

A caveat this decision used to carry has dissolved: while D4 made a
cell-less reply a compile failure, a compacted turn — which is exactly
such a reply — needed the check confined to the completion path. D4 no
longer rejects anything, so there is nothing to confine.

### D15 — The reply is logged as it arrives

The fork D11 opened, decided. **A reply is logged piece by piece as it
streams**, not as one `Turn` at the end: each prose segment a
`Call::Send { to: User }`, each cell a `Turn` logged before that cell
runs. "One completion, one `Turn`" is given up.

The alternative was to **buffer a cell's events and append them after a
single `Turn`** at the end of the completion. Rejected, and not on
balance — it gives back a property the design explicitly bought.
`Call`'s doc: logging at dispatch rather than at resolution is what
distinguishes a call that "definitively did not work" from one that was
"in flight when the process died — a `send_email` issued a millisecond
before `kill -9` used to be invisible in the log." Buffering reopens
exactly that window, for the 10–25s of generation still to come, over
file edits already made and messages already on a person's screen.

What made the choice cheaper than it looked:

- **The trigger rule mostly already handles it.** `needs_prompt`
  returns false unless the branch is `Phase::Idle` — "Awaiting an LLM: a
  request is already out, and everything logged since will ride the next
  one." Outcomes logged mid-stream cannot prompt.
- **The part that does need work is owed anyway.** `finish_program`
  renders unconditionally, and teaching it that a cell ending is not a
  reply ending is a cost of notebooks under either option (D7, 25.4).

What it gains beyond correctness:

- **Compile failure becomes partial progress**, the way D11 already made
  truncation partial progress. A cell 2 that does not compile leaves
  cells 0 and 1 standing instead of discarding the reply.
- **`Call::site` resolves into its own cell's `Turn`** — local, and
  never pointing into something not yet written.
- **`Turn.source` stays honest.** With prose in its own events, a
  cell-`Turn` holds JavaScript, so the rename D14 called for is not
  needed.

Prose as `Call::Send { to: User }` is not a workaround. A prose segment
*is* a message to the person, it renders in history exactly as a `tell`
does, and the model therefore re-reads its own reply in a shape it
already knows. The one adjustment: these sends are logged at generation
time rather than execution time, which is the distinction the card
already has to draw between prose and `tell()`.

**A prose `Send` is a new shape, and owes four things.** `Call::Send` is
documented as "**This branch's program** messaged an agent or the user",
and `Call` as "one per call **a program issues**, logged at dispatch.
Parent: the owning agent's spine, **between the program's `Turn` and its
eventual `Return`/`Condition`**." A prose segment satisfies none of
that: no program issued it, and the reply's *opening* prose is logged
before any `Turn` exists at all. Using `Call::Send` anyway is still the
right call — it renders in history exactly as a `tell` does, so the
model re-reads its own reply in a shape it already knows, and a new
payload would need its own rendering, compaction and menu handling — but
the relaxations must be deliberate, not discovered:

1. **It settles immediately, and the `Result` is written directly.**
   "Every call gets exactly one `Result`" is `Result`'s own invariant.
   A prose send has no VM promise behind it, so it cannot settle through
   `ToolDone`/`on_tool_results` the way `deliver_send` settles a `tell`
   — the host writes the `Result` itself at delivery.
2. **`site`/`site_end` are synthetic.** No instruction issued it. Zero
   width, the convention `span.rs` already names for "an instruction
   with no source expression of its own".
3. **`Call::Send`'s and `Call`'s doc comments must say so.** Both assert
   a program as the issuer; after this, prose is the exception and the
   comments name it.
4. **Its position is before the first `Turn`, not between.** `Call`'s
   placement rule holds for program-issued calls and no longer describes
   every `Call` on the spine.

Worth checking during 25.5: whether a multi-paragraph report reads
correctly through the `you told user:` row rendering, which was built
for one-line tells and escapes untrusted text. A report is the case this
whole phase exists for, so it is the case that must render well.

**The cost, stated plainly.** The reply is recoverable in content and
order but not byte-for-byte — fences and the whitespace between pieces
are stored nowhere. Nothing downstream needs them, but a log no longer
reproduces the exact bytes the provider returned.

### D16 — What `interp` gains is a REPL, not a notebook

The changes this phase asks of `interp` are all one capability:
**incremental evaluation** — feed successive fragments into a live VM
that shares a frame and a scope. That is what a REPL is, and it is what
an interactive shell, an eval loop, or a debugger evaluating an
expression against a live frame would each want. Today's
`compile(src) -> Program` plus a fresh VM becomes the degenerate case:
one fragment, then done. Nothing is removed and the one-shot path is
untouched.

So the direction is generalization. Three rules keep it that way, and
they are easy to violate by accident:

**Instruction names carry no notebook concepts.** Two are added, and
both are named for incremental evaluation: `ExtendFrame(local_kinds)`,
which pairs with `EnterFrame`, and `Pause`, which stops without
unwinding and pairs with `OutOfFuel`. An `Instr::CellBoundary` would
have put a markdown word into the instruction set of a JavaScript VM;
neither of these does, and both are what any REPL needs.

**The top-level-`return` rule is a flag, not a policy.** D5 is the one
genuinely notebook-shaped thing heading for the compiler, and its
message names `history.append` and `done()` — harness verbs. `interp`
already has `allow_return_outside_function`; this is enforcing its
inverse and belongs in the same place, with the harness-vocabulary
message supplied at the boundary rather than hardcoded.

The caveat, stated so nobody argues it later: `interp` is *already*
dialect-aware — `HARNESS_VERBS` in `interp/src/lib.rs` names `tell`,
`ask`, `done`, `history`. This is not new contamination in kind. But
there is a difference between `interp` knowing the dialect's *names* and
`interp` knowing the *notebook's rules*, and the flag is where that line
sits.

**Markdown never enters `interp`.** Fence recognition, the shared
buffer, cell spans: all `agent/src/notebook.rs`. `interp` receives a
`&str` and knows nothing about fences. 25.1 is already scoped this way.

#### The frame must grow, and a cell boundary is the only place it can

`EnterFrame(nparams, build_args, local_kinds)` bakes the frame's local
count *and* every slot's kind into one instruction, executed once at
frame entry. Cell 0's has already run by the time cell 1 declares a
local, and patching the emitted instruction does not grow a live frame.

The stack layout makes this look worse than it is:

```
    │  expr temporaries    │  <- sp
    ├──────────────────────┤
    │  declared locals     │  fp + nparams + K ..
    │  upvals (K)          │
    │  params (= args)     │
```

Locals sit at the *bottom* of the frame with temporaries above them, so
extending the locals region would normally mean shifting everything
above it. **At a cell boundary there is nothing above it.** A cell is a
run of complete statements, so the operand stack is balanced between
them and `sp` is exactly the top of the locals. Extending is a push, not
an insert:

1. Assert `sp == fp + nparams + K + cur_local_count` — the frame's
   temporaries are empty.
2. For each new slot, what `EnterFrame` already does: `Plain` pushes
   `Undefined`, `Boxed` allocates a `cells` entry and pushes
   `Upval(idx)`. This is `EnterFrame`'s own allocation loop applied to a
   suffix, so factor it out rather than writing it twice.
3. Bump `cur_local_count` **and** `callstack.last_mut().local_count`.
   The cached copy is re-derived from the frame (`methods.rs:222`), so
   bumping only the cache is undone by the next frame change.

**That invariant is why the prologue is the only place this can happen**
— mid-cell, temporaries are live and extending would mean shifting them
— and it is cheap to assert rather than assume.

It also puts weight on D11's sequential rule for a reason beyond data
dependencies: continuations snapshot `local_count` (`methods.rs:911`),
so a cell suspended on a top-level await must finish before the next
cell extends the frame, or a parked continuation resumes into a frame
that grew underneath it.

Three alternatives, all worse. **Over-allocating** at `EnterFrame`
cannot work: the reply is still streaming when cell 0's frame is built,
so any reserve is a guess that can be exceeded, and a generous one
wastes stack on every frame. **Re-running a patched `EnterFrame`** would
re-initialize cell 0's locals. **A heap scope object** is the scope map
D12 already rejected.

#### Yes, it is an instruction: `ExtendFrame(local_kinds)`

Mirroring `EnterFrame(nparams, build_args, local_kinds)`. The first
fragment emits `EnterFrame` as today; every later one emits
`ExtendFrame` carrying only its own new slots.

An instruction rather than a call the driver makes between `step`s,
because the slot kinds are *compiler* knowledge — the analyzer computed
them — and routing them through the driver means carrying compiler
output to the VM by hand, which the instruction stream already does. It
is also the same shape as `FreshCell`: an instruction that adjusts frame
state at a point the compiler chose. Doing one half of the prologue
through a side channel and the other half through the stream would be
two mechanisms for one operation.

It stays general (D16): `ExtendFrame` pairs with `EnterFrame`, is what
any incremental evaluator needs, and carries no notebook vocabulary.

**It does not subsume `FreshCell`; they touch disjoint slots.** A *new*
slot that a closure in this same cell captures is allocated `Boxed` by
`ExtendFrame` directly. `FreshCell` is only for *pre-existing* `Plain`
slots being promoted (D12's diff). Order between them is therefore
irrelevant.

**The emit-then-patch idiom already exists.** `ReturnSpill`'s doc
describes materializing a slot "by patching the already-emitted
`EnterFrame`" — the compiler emits the prologue before compiling the
body and patches the final `local_kinds` in afterwards. `ExtendFrame`
follows exactly that: emitted at the cell's start, patched when the cell
finishes compiling.

Two edge cases, both from the same doc comment:

- "`Err(i)`: no `EnterFrame` was emitted (a root frame with no locals)"
  — so a first cell that declares nothing leaves no prologue at all, and
  a later `ExtendFrame` must extend from zero with no `EnterFrame`
  before it.
- A cell that declares no new locals elides `ExtendFrame` rather than
  emitting an empty one.

So the cell prologue is two instructions at the one point where the
frame is quiescent: **`ExtendFrame` for the new slots, then `FreshCell`
for the ones a new closure just captured** (D12). With the stop
mechanism, that is the entire VM-side surface of this phase.

## What this deletes

- `//: ` narration, and the question of which marker it should use.
- Phase 24 in its entirety — the prefix re-parse, the span blanking for
  dispatch, the "settle the extent before dispatching" rule that existed
  because `tell("a")` might still become `tell("a").then(...)`. A fence
  terminator is unambiguous at the line level; a JS expression's is not.
- `tell()` as the prose channel.
- `extract_program`'s fence-stripping special case, which inverts into
  being the rule.

## What `tell()` is for now

It keeps three jobs markdown prose structurally cannot do: interpolate a
computed value, address an agent, and speak **after** the work.

> **Prose says what you already know. `tell()` says what you just found
> out.**

That sentence belongs on the card, because it also names the new hazard.
Prose is emitted while the completion streams — *before any instruction
runs*. A report turn is safe, since it reports what earlier turns
established and that is already in context. But findings written in
prose above the code that checks them are assertions made before their
evidence exists, and that is the failure mode to watch for.

## Steps

**25.1 — The split and the buffer.** `notebook.rs`: markdown in,
`Vec<CellSpan>` out, plus the shared parse buffer of D2 — same length,
same newlines, one cell live at a time. Pure, no IO, no JS parsing.

Recognition is **strict, and deliberately so**: a fence is three or more
backticks at column 0, the info string is exactly `js` or `javascript`
in lower case, and the closing fence is at least as long as the opening
one. No tildes, no indented fences, nothing inside a list item or a
block quote. CommonMark permits all of those; a strict subset is safe
here because a missed cell is not silent: the reply still speaks, the
turn rests (D4), and the person's next message gets it moving again — so
a missed cell costs one exchange and nothing else. A *wrongly*
recognised cell runs code the model did not mean to run, and has no such
backstop.
Gate: the buffer's length and every newline position match the markdown
for each cell in turn, and `cargo test -p agent notebook` covers
```js, ```javascript, a non-executable tag, an unterminated final fence,
a 4-backtick fence wrapping a 3-backtick one, and zero cells; asserts
every span slices the markdown back to exactly the cell's text.

**25.2 — One paused compilation (D12, D16).** Analyzer,
`ProgramAnalysis`, `Compiler` and VM all live across fragments and are
fed each in turn; backpatch scoped to the appended range; a cell
prologue of `ExtendFrame(local_kinds)` plus `FreshCell` for the
`Plain → Boxed` diff; an epilogue of `Instr::Pause` reporting `StepResult::Paused`. `interp`-level only —
no transport, no events, no markdown, fragments driven by a test
harness. **Name everything for incremental evaluation, not for cells
(D16)**: what is being built here is a REPL, and the notebook is one
caller of it.

Gate: `cargo test -p interp` green, plus tests that a `const` in cell 0
is readable in cell 1; that an undeclared name is a *compile* error
naming it; that a redeclaration across cells is caught; that a function
declared in cell 0 and called in cell 1 resolves a top-level name; that
a call in cell 2 logs a `site` slicing `Turn.source` to that call's own
text; and — the case that forced promotion — that a closure in cell 1
capturing a variable cell 0 declared and already wrote sees the current
value through the promoted cell, not a stale copy.

Plus the two `ExtendFrame` edge cases: a first fragment declaring no
locals (no `EnterFrame` is emitted at all, so a later `ExtendFrame`
extends from zero), and a fragment declaring no new locals (no
`ExtendFrame` emitted). And a debug assertion that the frame is
quiescent — `sp == fp + nparams + K + cur_local_count` — at every
prologue, since that invariant is what makes extending a push instead of
a shift.

**25.3 — The `return` diagnostic (D5).** A top-level `return` in any
cell is a compile error naming `history.append` and `done()`. Gate: a
test asserting the message names both, and that a `return` inside a
function *in* a cell is left alone.

**25.4 — `Transport::Notebook`, batch first, and the cell driver.**

Wire the split into the compile path beside `extract_program`, executing
cells in sequence *after* the completion ends. No streaming yet — this
isolates the transport from the scheduling change.

**This step owns the seam D7 and D9 both point at, and it is the
riskiest thing in the phase.** `finish_program` renders
unconditionally today, on the premise that a program running out is a
turn running out. Under a notebook those come apart: a cell ending means
*walk to the next cell*, and only the notebook ending means *one report,
one next completion*. The same fork governs a handback — `resume(v)`
falls out of cell *k* into the driver, which must continue at *k+1*
rather than treat the run as complete. Every other step here is local;
this one changes a control-flow premise the harness has held since the
loop was written, and it is the only part of this design not traced
against the code.

Gate: a scripted session test asserting `Turn.source` is byte-identical
to the completion, exactly one `Return`/`Condition` per turn, one report
and one next completion for a three-cell reply (**not three**), that a
`raise` in cell 0 resumed by a handler runs cells 1 and 2 afterwards,
and every `Call::site` resolving to the right span in the markdown.

**25.5 — Execute as the fences close (D11, D15).** Log each piece as it
arrives — prose as a `Call::Send { to: User }`, settled immediately by
the host with a synthetic site, each cell as a `Turn` before it runs —
then dispatch on fence close, and cancel the in-flight completion on
`done()`, trap or raise. Update `Call`'s and `Call::Send`'s doc comments
for the prose case (D15). Gate: a prose send has exactly one `Result`
and no dangling promise; a multi-paragraph report renders legibly
through the `you told user:` row; and tests
that a two-cell notebook runs cell 0 before cell 1's fence arrives; that
a **trap** in cell 0 cancels the completion (epoch moved, `LlmDone`
dropped) while a `done()` in cell 0 does **not** — later cells still run
and the reply finishes (D8, D11); and that a mid-stream truncation
leaves cell 0's effects standing with the turn reported as partial.

**25.6 — TUI (D13).** Semi-collapsed cells, expand on a keystroke,
effects rendered beneath each cell as they land. Gate: manual, plus the
existing `chat.rs` render tests still green.

**25.7 — Card, exemplars, and the doc corrections (D14).** The
prose/`tell()` split as the sentence above; cells are one scope; no
`return`; ```js runs; and — the line that must be there because the
natural reading is wrong and an exemplar written during this design got
it wrong — that `done()` does not stop anything, so a branch you mean to
skip is guarded with `else`, not with `done()` (D8). Plus the three false
clauses of `Message::Turn`'s doc rewritten, and
`compacted_program_comment` narrowed to cells — a compacted *prose*
segment is a `Send` and needs no comment wrapper. No field rename: D15
leaves `Turn.source` holding JavaScript. Gate: every exemplar parses and
runs against stub tools; a compacted prose segment renders as plain
prose with no marker, a compacted cell keeps its comment form, and role
alternation holds in both.

**25.8 — Measure before adopting.** Two arms on the existing tasks,
differing only by `AGENT2_TRANSPORT`. The plumbing is already there:
`Transport` is read from that variable in `document.rs`, and `drive.py`
already lists it among the provenance knobs, so the two arms stamp
distinguishably rather than producing two JSONs that cannot be told
apart.

Gate: report **programs/run** and **output tokens** explicitly — those
are where chat-mode drift would show, and they are the reason to keep
this behind a transport rather than switching to it. Also report whether
cancelling an in-flight completion actually reduced output tokens
(D11), which is asserted nowhere and assumed in one place.

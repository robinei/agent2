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
keeps what is worth keeping with `history.append`, and the run ends by
running out of cells or by `done()`.

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
has no executable cell, which D4 makes a compile failure and the repair
loop re-asks.

This is the decision most likely to be wrong, and it is cheap to
reverse: it is one predicate over the fence's info string.

### D4 — A reply with no executable cell is a compile failure

It goes to the repair loop like any other unusable completion.

Without this rule the format makes "reply with prose and do nothing"
expressible, which is chat mode — and chat mode is what the code-mode
thesis measured its way out of (programs/run 8.9 → 3.5, read-tell-stop
60% → 0%). A turn that only wants to speak still writes one cell:
`done();`.

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

### D8 — `done()` ends the notebook

Unchanged in meaning: it is the only thing that rests a branch. It
therefore also stops the notebook — the cell driver stops, and later
cells are never compiled or run. The card says so once; cells after
`done()` are dead code and read as such.

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
- **Early stop cancels the completion.** A `done()`, a trap or a
  `raise` in cell 0 makes every later cell moot, and the harness can
  cancel the generation still in flight. The machinery exists:
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

One thing to be careful of while doing it: D4 makes a reply with no
executable cell a *compile failure*, and a compacted turn is exactly
such a reply. D4 governs an arriving completion, never a stored one —
the check belongs on the completion path, not on anything that renders
history.

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
Gate: the buffer's length and every newline position match the markdown
for each cell in turn, and `cargo test -p agent notebook` covers
```js, ```javascript, a non-executable tag, an unterminated final fence,
a 4-backtick fence wrapping a 3-backtick one, and zero cells; asserts
every span slices the markdown back to exactly the cell's text.

**25.2 — One paused compilation (D12).** Analyzer, `ProgramAnalysis`,
`Compiler` and VM all live for the whole reply and are fed each cell in
turn through the shared buffer (D2); backpatch scoped to the appended
range; growable frame locals; `FreshCell` emitted for the
`Plain → Boxed` diff across re-finalization. `interp`-level only — no
transport, no events, cells driven by a test harness.

Gate: `cargo test -p interp` green, plus tests that a `const` in cell 0
is readable in cell 1; that an undeclared name is a *compile* error
naming it; that a redeclaration across cells is caught; that a function
declared in cell 0 and called in cell 1 resolves a top-level name; that
a call in cell 2 logs a `site` slicing `Turn.source` to that call's own
text; and — the case that forced promotion — that a closure in cell 1
capturing a variable cell 0 declared and already wrote sees the current
value through the promoted cell, not a stale copy.

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
that a two-cell notebook runs cell 0 before cell 1's fence arrives, that
`done()` in cell 0 cancels the completion (epoch moved, `LlmDone`
dropped), and that a mid-stream truncation leaves cell 0's effects
standing with the turn reported as partial.

**25.6 — TUI (D13).** Semi-collapsed cells, expand on a keystroke,
effects rendered beneath each cell as they land. Gate: manual, plus the
existing `chat.rs` render tests still green.

**25.7 — Card, exemplars, and the doc corrections (D14).** The
prose/`tell()` split as the sentence above; cells are one scope; no
`return`; `done()` ends the notebook; ```js runs. Plus the three false
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

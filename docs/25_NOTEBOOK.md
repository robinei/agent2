# Phase 25 — the response is a notebook

The model's reply stops being a bare JavaScript program and becomes
**markdown containing executable code blocks**. Prose is prose, reaching
the person as it streams; the code blocks are one *run* — separate
compilations sharing one frame, one scope and one outcome.

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

One completion. One `Turn` whose `source` is the whole markdown, byte
for byte. Two cells sharing one frame — `a` is visible in the second
cell because the compiler carries cell 0's scope table into cell 1 and
both run against the same locals (D12).

Note what cell 0 does *not* do: end with `return`. That is the habit
exemplar 02 teaches, and there is no `return` here at all (D5) — a cell
keeps what is worth keeping with `history.append`, and the run ends by
running out of cells or by `done()`.

## Decisions

### D1 — A cell is a span, not an event

One completion is one `Turn`, and `Turn.source` is the entire markdown
reply. Cells are byte spans within it, numbered from zero.

This keeps the property phase 24 was careful about: `Call::site` is an
offset into the stored source, so a report can annotate a program per
call site from the log alone. Cells are *compiled* from their own text
(D2), but they are never *stored* that way — the log holds the markdown
and offsets into it, so `Turn.source` stays what the model wrote and no
consumer has to know cells exist.

### D2 — Compile the cell's own substring, then rebase its spans

A cell is compiled from its own text. Afterwards, `cell.start` is added
to every span the compilation produced — `Program::spans` (one `Span`
per instruction, a flat array), each debug function's
`span_start`/`span_end`, and any diagnostic's span.

The result is what matters downstream: **every offset is absolute into
`Turn.source`**, which is the whole markdown, byte for byte. So
`Call::site`, `Condition::site`, `report.rs`'s per-call-site annotation
and its line-and-caret diagnostic all keep working untouched, and a
caret lands in the model's own reply with its prose around it.

Two rejected alternatives:

- **Blanking** — overwrite every byte outside the cell with spaces and
  compile that, so offsets come out absolute with no rebase. This is
  what phase 24 did for a leading `tell()`, and an earlier draft of this
  doc adopted it. With per-cell compilation it is strictly worse: it
  allocates and parses a full-length copy of the markdown once per cell
  to save two loops over arrays that already exist.
- **Cell-local spans** — accept that an offset means "byte 40 of cell 2",
  as a notebook's line numbers do. Rejected because `site` is not only
  for error text: it is logged on `Call::Send` and `Condition`, and
  read back to annotate a program per call site. Going relative would
  put a cell index on every span-carrying event in the log and teach
  every consumer to resolve it — schema churn to avoid an addition.

**The prelude is the trap here.** `compile_with` *appends* the
higher-order-method helpers to the source ("appended, so user spans are
unchanged"), so prelude instructions carry spans past the end of the
user's text. Harmless today; after rebasing, cell 0's prelude spans
would land inside the prose that follows it, and a report could annotate
a paragraph with a call result. A span starting at or beyond the cell's
own length is therefore made zero-width synthetic rather than shifted.

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

There is consequently no `EventPayload::Cell`. Cells leave no events of
their own — they are spans in a `Turn`, and everything a cell does is
logged by the calls it makes.

### D7 — Exactly one terminal per notebook

`Cause::Abandoned`'s doc states the invariant: *a run must have exactly
one log-visible terminal, or nothing downstream can be derived from the
log alone* — and `Return`'s states the completing half of it, that a
program ending without a `return` still logs `Return { value: null }`.
Cells must not break either.

The notebook — not the cell — logs exactly one `Return` or `Condition`.
Cutting the `return` *statement* (D5) does not cut the `Return`
*event*: its own doc already says a program that ends without a `return`
still logs `Return { value: null }`, which is now simply the only case.

Had cells logged outcomes of their own, the trigger rule ("the newest
`Turn`'s run has an outcome that has not been shown yet") would fire per
cell and prompt a fresh completion after each one — the thesis exactly
inverted.

**One notebook, one report, one next completion.**

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

The cost to accept honestly: while cell 0 runs and fails, the model is
still writing cells 1–3 on the assumption that it succeeded. Today the
whole program is written blind too, so this is not a regression — but
the failure is now visible mid-stream, which is new, and cancellation
bounds the waste rather than removing it.

### D12 — Cells compile incrementally; capture promotes with `FreshCell`

Each cell compiles once, with the compiler re-entered carrying the prior
scope table. Cell 0's instructions are never regenerated, so nothing a
later cell does can change them. Slot indices are allocated in
declaration order and stay put: cell 1 resolves `a` to the slot cell 0
gave it, and allocates its own names above.

**An earlier draft of this doc got the obstacle wrong.** It argued that
appending is unsafe because the whole-program analysis could change
codegen for code that already ran, and proposed moving top-level names
into a run-lived scope map with runtime name resolution. That objection
applies to *recompiling the prefix*, which nothing here does. Incremental
compilation has no such problem, and the scope map would have bought
nothing for a hash lookup per access and a lost compile-time resolution.

The real obstacle is narrower, and it is not the slot index but the slot
*representation*. `analyzer/captures.rs` assigns each own-local a
`SlotKind` from whole-unit capture analysis: a slot captured by a
descendant closure is `Boxed` — one eager cell shared by reference —
and everything else is `Plain`, a raw value in the frame. So:

```js
// cell 0
let x = 5;
console.log(x);          // nothing captures x → Plain, a raw value
```
```js
// cell 1
const f = () => x + 1;   // x is captured now → this code expects a Box
```

Cell 0 has run and written a raw value into the slot that cell 1's
closure expects to be a cell. The index agreed; the representation did
not. Boxing is a property of the binding decided by uses that may not
have been written yet, which is the one thing a widening window cannot
settle after the fact.

**The promotion primitive already exists**: `Instr::FreshCell(slot)`.
Its doc comment describes it as re-boxing a captured loop local per
iteration, but the implementation is general — it reads the slot,
dereferencing an `Upval` if there is one and otherwise taking the raw
value, allocates a cell seeded with it, and stores `Value::Upval(idx)`
back. On a `Plain` slot that is exactly a value-preserving
`Plain → Boxed` promotion.

So slots stay `Plain` by default. When cell *k*'s analysis finds it
captures a name an earlier cell declared `Plain`, the compiler emits
`FreshCell(slot)` at the top of cell *k* and flips that name to `Boxed`
in the carried scope table. Nothing pays an indirection unless a later
cell actually closes over it, and then it costs one instruction, once.

An earlier draft boxed every top-level slot unconditionally to pin the
representation before any cell ran. Unnecessary, given the above.

**How a later cell's code gets into the running VM.** The same
`Compiler` instance keeps emitting. Its buffers are its own fields —
`code: Vec<Instr>`, `spans: Vec<Span>`, `next_label` — so cell 1's
instructions are appended to the buffer cell 0 filled, and the backpatch
pass resolves label ids to indices into that same vector. **Addresses
come out absolute by construction; there is nothing to rebase.**

An earlier draft of this doc had each cell compiled standalone and its
`Program` appended afterwards, which created a jump-rebasing problem and
a `base_addr` parameter to solve it. Both were self-inflicted: compiling
into the shared buffer in the first place means the problem never
arises.

What the appending *does* require is that each pass run over the **newly
appended range**, not the whole buffer:

- **Backpatch** from the cell's first instruction onward. Cell 0's jumps
  hold resolved addresses by now, not label ids, and a pass that cannot
  tell the two apart would corrupt them.
- **Span rebasing** over the same range. A cell is parsed from its own
  substring (D2), so its spans arrive cell-relative and `cell.start` is
  added as they are appended.

**The real plumbing is in the analyzer, not the compiler.** Scope and
capture analysis is a separate pass over the cell's AST, and it
allocates slots from zero for each unit it sees. It has to be seeded
with the prior scope table and the frame's current local count, or cell
1 gives `let y` slot 0 and stomps cell 0's `x`. That seeding is the one
genuinely new piece; everything else on this list is free once the
compiler instance simply stays alive.

One detail for 25.1b: `Program::source` is what diagnostics render
against, and under this transport it should be the markdown, not the
cell — that is what makes a rebased span resolve. While the reply is
still streaming the markdown is a prefix, which is harmless (it always
contains every cell that has run) but means the VM's `source` grows
between cells rather than being set once.

**Why promotion is sound.** If an earlier cell's analysis said `Plain`,
then no closure in that cell referenced the name — a reference from a
nested function *is* a capture, which would have forced `Boxed` there.
So the only code compiled against the `Plain` representation is
straight-line code in cells that have already run to completion, and
nothing that could observe the old representation survives the boundary.

The ordering invariant this rests on: promotion is emitted at cell
*start*, and cells run strictly sequentially (D11). A cell's fence can
close while the previous cell is still suspended on an await, so cell
*k+1* may **compile** early — but its first instruction does not
**execute** until cell *k* has finished, so cell *k*'s post-await tail
never reads a slot promoted underneath it.

Worth checking during 25.2 rather than assuming: `NameRes::Const` —
a const binding folded at compile time that "never reaches the frame" —
appears to carry over safely, since a carried scope table folds it
identically in later cells. Loop-declared `FreshCell` slots do not arise
at cell top level.

### D13 — The TUI collapses cells

A code block renders semi-collapsed by default — a few lines and a count
— and expands on a keystroke.

After a cell has run, what matters is its *effects*: the calls it made,
what they returned, what it logged. Those are rows the TUI already
renders. The source is how it got there, and it is the least interesting
thing on screen for the person who asked a question. Collapsing is what
makes "never have to read the generated JavaScript" true in practice
rather than only in principle.

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

**25.1 — The split.** `notebook.rs`: markdown in, `Vec<CellSpan>` out.
Pure, no IO, no JS parsing. Gate: `cargo test -p agent notebook` covers
```js, ```javascript, a non-executable tag, an unterminated final fence,
a 4-backtick fence wrapping a 3-backtick one, and zero cells; asserts
every span slices the markdown back to exactly the cell's text.

**25.1b — Span rebasing.** Compile a cell's substring, add `cell.start`
across `Program::spans` and the debug function table, zero-width any
span at or beyond the cell's length (the appended prelude). Gate: a test
that a call in cell 2 logs a `site` which slices `Turn.source` to that
call's own text, and that no prelude instruction carries a span landing
in prose.

**25.2 — Incremental cell compilation (D12).** One `Compiler` instance
emitting every cell into its own buffers, so addresses stay absolute
with no rebasing; backpatch and span-rebase scoped to each cell's
appended range; the analyzer seeded with the prior scope table (name →
slot *and* `SlotKind`) and the frame's current local count; growable
frame locals; `FreshCell` emitted at a cell's start for each earlier-declared
name it is the first to capture. Gate: `cargo test -p interp` green, plus tests that a `const`
in cell 0 is readable in cell 1, that an undeclared name is a *compile*
error naming it, that a redeclaration across cells is caught, that a
function declared in cell 0 and called in cell 1 resolves a top-level
name, and — the case that forced promotion — that a closure in cell 1
capturing a variable cell 0 declared and already wrote sees the current
value through the promoted cell, not a stale copy.

**25.3 — The `return` diagnostic (D5).** A top-level `return` in any
cell is a compile error naming `history.append` and `done()`. Gate: a
test asserting the message names both, and that a `return` inside a
function *in* a cell is left alone.

**25.4 — `Transport::Notebook`, batch first.** Wire the split into the
compile path beside `extract_program`, executing cells in sequence
*after* the completion ends. No streaming yet — this isolates the
transport from the scheduling change. Gate: a scripted session test
asserting `Turn.source` is byte-identical to the completion, exactly one
`Return`/`Condition` per turn, one report, one next completion, and
every `Call::site` resolving to the right span in the markdown.

**25.5 — Execute as the fences close (D11).** Dispatch on fence close;
cancel the in-flight completion on `done()`, trap or raise. Gate: tests
that a two-cell notebook runs cell 0 before cell 1's fence arrives, that
`done()` in cell 0 cancels the completion (epoch moved, `LlmDone`
dropped), and that a mid-stream truncation leaves cell 0's effects
standing with the turn reported as partial.

**25.6 — TUI (D13).** Semi-collapsed cells, expand on a keystroke,
effects rendered beneath each cell as they land. Gate: manual, plus the
existing `chat.rs` render tests still green.

**25.7 — Card and exemplars.** The prose/`tell()` split as the sentence
above; cells are one scope; no `return`; `done()` ends the notebook;
```js runs. Gate: every exemplar parses and runs against stub tools.

**25.8 — Measure before adopting.** `drive.py --card` with a notebook
arm against the current one, on the existing tasks. Gate: report
**programs/run** and **output tokens** explicitly — those are where
chat-mode drift would show, and they are the reason to keep this behind
a transport rather than switching to it.

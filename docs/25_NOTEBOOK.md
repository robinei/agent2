# Phase 25 — the response is a notebook

The model's reply stops being a bare JavaScript program and becomes
**markdown containing executable code blocks**. Prose is prose, reaching
the person as it streams; the code blocks are one program.

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
for byte. Two cells sharing one scope — `a` is visible in the second
cell because **the cells are one program**.

Note what cell 0 does *not* do: end with `return`. That is the habit
exemplar 02 teaches, and under this transport it would end the notebook
before cell 1 ran — see D5, which makes it a compile error rather than a
silent one. A cell keeps what is worth keeping with `history.append`;
the sole `return` belongs in the last cell, if anywhere.

## Decisions

### D1 — A cell is a span, not an event

One completion is one `Turn`, and `Turn.source` is the entire markdown
reply. Cells are byte spans within it, numbered from zero.

This keeps the property phase 24 was careful about: `Call::site` is an
offset into the stored source, so a report can annotate a program per
call site from the log alone. If cells were sliced out and stored
separately, every offset would need rebasing, and `Turn.source` would
stop being what the model actually wrote.

### D2 — Blank outside the cell, compile the cell alone

Each cell compiles on its own, from a source built by **overwriting
every byte outside that cell with spaces**. Blanking preserves length,
so every `Call::site` still points into the markdown the model wrote,
and `Turn.source` stays byte-exact. That is phase 24's trick, per cell.

An earlier draft blanked everything outside *all* fences and compiled
one program, which gave shared scope for free because the cells were
literally one source. D11 gives up that freebie deliberately: cells must
execute as they arrive, and a cell cannot be part of a program whose
later half has not been written yet.

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

`Return`'s doc states the invariant: *a run must have exactly one
log-visible terminal, or nothing downstream can be derived from the log
alone.* Cells must not break it.

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
therefore also stops the notebook — remaining cells do not run, the same
way code after `return` in a function does not. The card says so once;
cells after `done()` are dead code and read as such.

### D9 — A raise or a trap suspends the notebook mid-cell

Also unchanged. `raise(...)` suspends with a handler frame; a trap
suspends resumably or not. Remaining cells do not run, because the VM is
parked inside cell *k*.

`resume(v)` then continues **from that instruction**, falls out of cell
*k*, and runs cells *k+1…* normally. This needs no new machinery — it is
exactly what `resume` already does, and it matches the notebook prior
(an erroring cell stops a Run All).

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
  cancel the generation still in flight. `llm_epoch` and `cancels`
  already do this for `Interrupt`, and `LlmDone` already drops a
  response whose epoch has moved. **This saves output tokens, not just
  latency**, and concatenation cannot capture it at all.
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

### D12 — Top-level names live in a scope map, not in slots

This is what D11 costs, and it is the only real work in the phase.

Top-level variables lower to `GetLocal(LocalIndex)`/`SetLocal(LocalIndex)`,
resolved by a **whole-program** analysis (`analysis.scopes`,
`const_fn_scopes`). So "append the next cell's instructions to the
running program" is not safe: recompiling a longer prefix can change
codegen for code that already ran — a name const-folded across cells 0–1
stops being foldable when cell 2 reassigns it, and the live frame no
longer matches its own instructions. Appending would need an incremental
analyzer with a slot-stability guarantee.

Instead, **in notebook mode only**, a top-level declaration reads and
writes a notebook scope map that lives for the run. The compiler carries
the set of declared names across cells, so an undeclared reference is
still a compile-time error and a redeclaration is still caught. Each
cell is then an ordinary independent compilation unit: no appending, no
frame growth, no analyzer change, no slot-stability invariant.

`Instr::PushName(RcStr)` is already this shape — a name resolved at
runtime against a registry, `ReferenceError` when unknown — so the
mechanism is an extension of something present, not a new one.

The cost is a hash lookup instead of an array index for top-level names.
For programs of a few dozen statements dominated by I/O, it is not
measurable. Function scopes are untouched and keep their slots.

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

**25.1 — The split.** `notebook.rs`: markdown in, `Vec<CellSpan>` out,
plus a blanking helper producing one compilable source per cell. Pure,
no IO, no JS parsing. Gate: `cargo test -p agent notebook` covers ```js,
```javascript, a non-executable tag, an unterminated final fence, a
4-backtick fence wrapping a 3-backtick one, and zero cells; asserts the
blanked source is the same length as the markdown in every case.

**25.2 — The notebook scope (D12).** Top-level declarations compile
against a run-lived scope map; the compiler carries the declared-name
set between cells. Gate: `cargo test -p interp` green, plus tests that a
`const` in cell 0 is readable in cell 1, that an undeclared name is a
*compile* error naming it, that a redeclaration across cells is caught,
and that a function declared in cell 0 and called in cell 1 resolves a
top-level name correctly.

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

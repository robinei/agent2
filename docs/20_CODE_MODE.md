# Phase 20 — Code mode: the LLM writes programs, and nothing else

Replaces `20_PROGRAM_FORKS.md` (deleted, never committed — its argument
and the reason it was abandoned are recorded at the end of this file).

Resolves the open question `8_HARNESS.md` deferred — "Context growth…
the real mechanism should be designed from observed transcripts, not
speculation" — but not in the shape that file anticipated. The mechanism
is not summarization and not sub-frame hand-off. It is removing the
channel by which context grows without anyone choosing it.

Read `DESIGN.md` first. **This file contradicts it in one load-bearing
place, deliberately** — see "What this changes in DESIGN.md". Per that
file's own rule, that is a design change, not a detail.

## The problem

Two symptoms, one cause.

**The model writes small programs.** Observed repeatedly: it prefers a
short program that gets one result into context, then a new short
program responding to what it saw. Every card revision pushing for
larger orchestration has lost to this. It is not a prompting failure.
The chat/tool-call surface *is* the affordance for act-observe-comment,
and the model is completing from a distribution of assistant turns where
that rhythm is what assistant turns look like. We are arguing against
the weights with prose.

**Context grows without anyone choosing it.** `Runner::render_messages`
walks the spine and renders every `Message::Turn` as a permanent
Assistant+Tool pair, including every raise-and-restart round-trip (the
`resume` *tool* of today's design, unrelated to the `resume()`
decision value below), on every future request for that branch,
forever. The artifact menu accumulates alongside it (`8_HARNESS` named
this too). Nothing folds, because nothing was ever asked whether it
should be there.

The second symptom is downstream of the first: a model that writes ten
small programs pays ten round-trips of permanent transcript, where one
large program would have paid one. Fixing the growth without fixing the
rhythm treats the symptom.

## The thesis

**The LLM's only task, ever, is to complete a program.**

Every assistant turn the model produces is a program and nothing else —
no prose, no tool calls, no schemas. What it reads is a card that never
changes and a record of what has happened; what it writes is code. The
program does the work, including deciding what, if anything, is worth
remembering.

The request travels as an ordinary chat exchange (Part A), but nothing
chat-shaped reaches the model *inside* the turns: user turns are the
harness and the human reporting, assistant turns are pure code, and each
one is a demonstration of what the next should be (Step B1).

### The bet

No part of this is novel. Code mode is widely adopted; condition systems
are Lisp's; document-framing is old base-model practice. The bet is that
the *combination* is, and the reason to think so is that each part fixes
what makes the next one unusable:

- **Code mode alone is brittle.** A long program that meets the
  unexpected fails wholesale, so timidity is the *rational* strategy and
  the leverage goes untaken.
- **Conditions fix that.** The program stops, the mind decides with the
  heap intact, execution continues. Long programs become recoverable, so
  writing them becomes rational.
- **A handler that is a chat turn collapses back into the tool loop** on
  every failure. Handler-as-program gives the recovery path the same
  one-inference-to-N-operations property as the happy path.
- **Raising would still tax the future** if every round-trip were
  permanent transcript — which pushes back toward not raising, and back
  toward timid programs. Curated context closes the loop: twenty raises
  cost nothing permanent, so judgment stays cheap.

Conditions make big programs safe; handler-as-program keeps failure
cheap; curated context keeps asking cheap; and cheap asking is what lets
programs be big. Remove any one and the other three stop paying. That
mutual dependence is the whole claim, and it is what this phase is for.

### Why the bet gets better with time

Decode speed is bounded by memory bandwidth and has stayed roughly flat
while capability has moved a lot — and reasoning models made wall-clock
per task *worse*, not better. Intelligence per inference call is rising
fast; calls per second is not. Code mode converts one call into N
operations, so **its leverage scales with model capability**, while a
tool loop's does not: a smarter model still pays one call per step.

This also means the design is early. Today's models sit near the edge of
writing good long programs, so the honest expectation is a mechanism
that works and quality that lags — which is why the harness (Part H)
reads the mechanism rather than scoring absolute quality.

The *register* half of the bet — not the economic half below — gets
stronger the closer the model is to raw completion. Chat-tuned weights
are what that half is routing around, so a base model, or a fine-tune on
this shape, is where it pays most; that quietly favours open weights and
self-hosting. Worth knowing, but not a dependency: the mechanism this
phase actually rests on works on a chat endpoint unchanged.

### Why the model would write longer programs

**One mechanism carries this phase: the model cannot summon itself
except through `raise()`.**

Today the short-program strategy is *correct*, not a habit. The model
emits `run_program`, the turn ends, and the tool result comes back
carrying the data it wanted. Writing a small program is simply the
efficient way to see something. No instruction outweighs that, which is
why every card revision has lost.

Here, ending a program returns no results **and no control**. Nothing
triggers a completion except an inbound event (Step B1), so a program
that ends has ended; there is no "and then I will think about it". The
re-entry paths are exactly three — an inbound event, a `raise()` from a
running program, and `abandon()` from a handler — and only one of those
is the mind's to reach for. So the short-program strategy does not
merely stop paying: it stops being *available*. Iteration must go
through a call that is visible in the source, priced in the card, and
countable.

And when the mind does re-enter, what the following turn carries is
status, an effects line, and whatever the program *chose* to append —
facts, not content. A program that reads a file and raises gets back
"read 1 file", not the file. Both halves hold regardless of transport,
layout or register.

Everything else supports that: no tool affordance inviting a stop;
`raise()` as the only mid-program route back to the mind, explicit in
the source and priced as expensive in the card; the card's promise that
the mind will not be asked again once the program ends; and a root
return that means nothing, so ending early signals nothing.

**Register is a supporting mechanism, not the thesis.** Prior assistant
turns are demonstrations of what an assistant turn is here — pure code,
no preamble — at exactly the position where the next one is generated
(Step B1), and long programs beget long programs. This was the leading
argument in an earlier draft, when the plan was one continuous file with
no chat structure at all. Role-delimited turns and the card in `system`
both spend some of it, deliberately and for good reasons, which is why
it is second. If programs do not get longer, the first thing to check is
not the register — it is whether the model found a way to get results
back anyway (Part H).

### Pull, not push

One principle, applied five times. It is the through-line of this whole
phase, and each application is cheap only because the others hold:

| what used to be pushed into context | how it is pulled |
|---|---|
| every tool result | `append_history()` — the mind appends what its future self needs |
| the condition report's clipped payload | frame introspection — the handler queries the frozen VM |
| the artifact menu | history entries are ids; the log is the store |
| the whole conversation, re-read forever | compaction by reference, triggered as a condition |
| a program's raise/resume interior | never entered history in the first place |

The economic argument is the same every time: **the model emits
operations, not data.** Selection is cheap to express; content is
expensive to decode. Compaction makes this impossible to miss — dropping
120K tokens of history costs a few hundred output tokens as
`remove_history(…)` calls, versus tens of thousands to re-emit a summary
of what is kept.

## What this changes in DESIGN.md

Surfaced explicitly, per DESIGN.md's own instruction.

- **"The one exception: the answer crosses into context" is retired.**
  There is no answer that crosses. A root program returns nothing to
  anyone — it reaches the user through `say()`. A handler's value goes
  to the raising *program*, as a value, not into a context. What enters
  history is exactly (a) what nobody was waiting for, and (b) what a
  mind explicitly appended. The "one exception" becomes the only rule,
  with no exception: **nothing enters a context that a mind did not
  choose to put there.** The answer-budget machinery
  (`DEFAULT_ANSWER_BUDGET`, the clipping in `report.rs`) has no subject
  under this design.
- **The suspension table gains a row.** Context-budget overflow joins
  `OutOfFuel` as a condition with a handler (Part E). `8_HARNESS`
  already listed a memory budget as a row; this is that row, and it is
  the one resource limit currently handled by nobody.
- **The `tools.` namespace narrows; it does not go.** Revised after
  this file first shipped: `tools.*` stays as the surface for a
  specific agent's *configured capabilities* — `read_file`, `bash`,
  whatever the registry holds, which the compiler has no static view
  of and which `17_BRANCHES`'s naming constraint still applies to.
  What changes is the **closed, harness-defined vocabulary** —
  `say`/`ask`/`answer`/`spawn`/`fork`/`append_history`/`artifact`,
  plus the pure decision constructors `resume`/`abandon` — which
  becomes bare-global, joining `raise`, already unnamespaced today.
  The line is language versus library: a fixed set every agent has,
  known to the compiler exactly the way `raise` already is, versus a
  set that varies per agent and the compiler cannot see (Step C1).
- **"LLM as restart handler" becomes literal.** The handler is not an
  LLM turn choosing a restart from a menu — it is a program the LLM
  writes, running while the signaling frame is still live. That is
  Lisp's actual semantics, and it is what forces Part D.
- **Unchanged, and checked at every step:** the load-bearing suspension
  property. No VM is ever on another VM's stack; effects remain
  `StepResult` returns; the handler stack is a host-side structure of
  independently-stepped VMs. Also unchanged: recovery is re-execution,
  the log is append-only, determinism stays a non-goal.

## Ground rules

- Commit per step, `codemode:` prefix.
- Gate after every step: `cargo fmt && cargo clippy --workspace
  --all-targets && cargo test` — all three green, zero warnings.
- **Finish a step before starting the next; finish a part before
  starting the next part.** Part A is first because everything else
  produces the bytes it sends. Measurement is Part H, after there is
  something to measure.
- No network in any test; `ScriptedLlm` drives everything new. **Part
  G2's harness is not a test** — it necessarily talks to a live model,
  since no scripted LLM can tell you what a real one writes. It is a
  separate opt-in binary, never part of `cargo test`.
- `14_CLEANUP.md`'s lint rules still apply.
- The existing chat path stays working and untouched until Part I. This
  phase runs beside it, not through it.

---

## Part A — The request path

The transport and the renderer: the bytes that go out and how they
travel. Everything else in this file produces those bytes, so this is
first — but only this. Measurement moves to Part H, after there is
something to measure.

### The transport

- [x] **One transport: the document as a role-delimited chat exchange
      (Part B), thinking on, no prefill.** The model answers with the
      program itself, in format (the no-fence rule below). That is the
      only variant every provider supports, so it is the only one
      built. (This bullet originally read "a single user message" —
      written before the role-delimited flip later in this doc's own
      design discussion; Part B's layout is what was actually agreed
      and built, and this line is corrected to match it rather than
      left contradicting Part B.)
- [ ] **Why not prefill**, despite it being available on this project's
      provider: it works nowhere else (see the table), it is beta-gated
      where it does work, and the no-fence rule already took away most
      of what it bought — with no fence to seed, the difference
      collapses to the bare `<|assistant|>` register effect. A variant
      that cannot be relied on is test surface without a payoff.
- [ ] **Why not raw completion**, which fails the same test harder: no
      reasoning channel at all, unavailable on two of three providers,
      and the only variant with a stop-marker problem once the fence is
      gone. Recorded as a *future* option rather than deleted — the
      thesis says this bet pays most on a base model or a fine-tune,
      and that is the shape those want. Keeping the seam costs nothing;
      building the variant now does.
- [x] **The seam stays: the document is the interface.** One function
      renders the bytes; how they reach the model is a thin layer
      behind it. Adding a transport later must not restructure
      anything, which is the whole reason the seam exists with a single
      implementation behind it.
- [ ] **Caching is over the request prefix, and a request never
      contains reasoning.** Turn N+1's request is a strict extension of
      turn N's — identical through the last user turn, then it appends
      the program and the new turn — so thinking cannot break the
      match. DeepSeek requires `reasoning_content` to be excluded from
      next-turn context for exactly this reason, and `request_body`
      already keeps the ephemeral tail last for the same one (`prefix
      is immutable`). A program is processed as input once, on the
      first request it appears in, which is how every chat API works
      and what today's tool loop already pays. Not a cost of this
      design. (Some serving stacks additionally persist the KV of
      *generated* tokens so the next turn hits them free; thinking
      prevents that, since the generated sequence carries reasoning the
      next request omits. An optimization that may not exist, on top of
      a universal and small cost — do not plan around it either way.)
- [ ] **DeepSeek only; no other provider is implemented in this
      phase.** The tables and notes below are recorded so the choices
      above are legible and so a future port starts informed — not as
      work. Anything they imply about OpenAI or Anthropic is
      documentation, not a checkbox.
- [ ] Provider support, checked 2026-09 — kept because it is what makes
      the choice above obvious, and because it will drift:

      | | raw completion | prefill | thinking channel |
      |---|---|---|---|
      | DeepSeek | yes (`/beta`, FIM) | yes (`prefix: true`, `/beta`) | yes (`reasoning_content`) |
      | self-hosted / open weights | yes | yes | model-dependent |
      | Anthropic | no (legacy API retired) | no — removed in Claude 4.6, returns 400; also unsupported with extended thinking before that | yes |
      | OpenAI | effectively no — legacy `/v1/completions` is instruct-class only, and the `gpt-3.5-turbo-completions` alias shuts down 2026-10-23 | no — a trailing assistant message starts a *new* turn; occasional continuation is happenstance, not contract | not exposed as content |

- [x] **Thinking is the one non-portable thing kept**, because writing
      one long correct orchestration program is the most
      reasoning-heavy thing this system asks for. On DeepSeek:
      `thinking: {type: "enabled", reasoning_effort: "max"}`, enabled
      by default; the client already sends `{"type": "disabled"}` only
      when switched off, so this exists. Model ids are
      `deepseek-v4-flash` / `deepseek-v4-pro`; there is no separate
      reasoner model.
- [ ] **Thinking never re-enters a request; it is still logged.** It
      streams to a TUI pane and never appears in the rendered document,
      which is the pull-not-push rule applied to the one channel that
      would otherwise sneak back in — but "not rendered" is not "not
      stored" (next bullet). That it can be dropped this cleanly is **a
      property of DeepSeek, not of the design**, and an earlier draft
      justified it wrongly (claiming every request is a fresh
      single-turn document with no replay to manage — true before the
      role-delimited flip, false after it). Checked 2026-09, the three
      providers disagree completely:
      - **DeepSeek forbids replay.** `reasoning_content` must be
        excluded from next-turn context, so reasoning is genuinely
        transient and never enters a prefix.
      - **OpenAI (not implemented) requires it for caching.**
        Reasoning items must be passed back for a full cache hit —
        documented as the top cause of cache misses — with
        `reasoning.encrypted_content` provided so stateless clients can
        do it.
      - **Anthropic (not implemented) changed mid-line.** Sonnet and
        Haiku 4.5 and earlier stripped prior thinking; Opus 4.5 and
        4.6+ **keep it in context and bill it as input**. Changing
        thinking parameters also invalidates cached message prefixes.
- [ ] **Thinking is an artifact: stored, not rendered, retrievable.**
      `Message::Turn` already carries it (`types.rs`), so "never enters
      history" was always a statement about the rendered document, not
      about the log — the log keeps everything, as it does for every
      other artifact. Replay is therefore a **renderer decision the log
      already supports**: exclude on DeepSeek because it is required —
      which is all this phase does. A future port to OpenAI or
      Anthropic 4.6+ would include it, because caching needs it, and
      would need no new plumbing to do so.
      - Which turns the Anthropic behaviour from a leak into a
        **trade**: replay reasoning for the cache hit and accept that it
        accumulates in the prefix outside the mind's control, or omit it
        and pay worse cache. The renderer chooses; nothing arrives
        through an unwatched door.
      - Keep storing it regardless of provider. Part H's job is working
        out *why* the model behaves as it does, and the reasoning trace
        is the best evidence available — live in a pane, reviewable in
        the log afterwards.
- [ ] **What actually reaches the model, so this choice rests on the
      right grounds.** The API's structure is a serialization
      convention: the server applies a chat template that flattens
      roles into one token sequence with marker tokens, and that flat
      sequence is all the model sees. Chat framing costs ten to twenty
      tokens — the question was never *how much* structure arrives, but
      that `<|assistant|>` is the highest-signal token in the prompt,
      being the one after which a trained assistant turn follows. That
      single marker is the entire cost of not having prefill, and the
      no-fence rule plus the seed exemplar are what pay it down.
- [x] **The card goes in `system`.** An earlier draft kept it inside
      the document, arguing that the system region is conditioned for
      "instructions to an assistant" and that a uniform file register
      was worth protecting. That argument does not survive the
      role-delimited layout (Step B1): the register is set by the
      assistant-turn demonstrations, not by where the instructions sit.
      Three reasons to use `system` instead:
      - It is the **only structural trust boundary** the template
        offers. An instruction there is marked as an instruction by the
        protocol, not by a convention an untrusted post could imitate.
        Everything in a user turn is, structurally, content.
      - **System instructions are followed more reliably and take
        precedence over user content** — which is what a hard
        constraint like "emit only valid JavaScript" needs, once user
        turns may carry text from other agents that contradicts it.
      - It matches this file's own framing: the card is fixed forever
        and is the cache prefix, and `system` is the region for exactly
        that.
      The seed exemplar (Step C4) cannot live there — its point is to
      demonstrate an *assistant* turn — so it opens `messages` as a
      real user/assistant pair.
- [ ] **No fence: the response is only ever valid JavaScript.** The
      card says so absolutely — "emit only valid JavaScript; the
      whole response is parsed" — and it is a cheap constraint to obey
      because **the preamble has a legal form**. The model is not asked
      to suppress the urge to explain, only to spell it `//: like
      this`, which is the narration convention anyway (Step G1b). A
      "Sure! Here's a program that…" opening becomes the plan block.
      Redirection is far easier compliance than suppression.
- [ ] **And it is self-enforcing**: a completion that does not parse is
      already a condition with a handler, so non-compliance is told to
      the model and rewritten, not silently accepted. A checked
      constraint beats a requested one.
- [x] **Tolerate a stray fence silently.** The likeliest residual
      failure is habit — wrapping in ```javascript. Strip a leading or
      trailing fence before parsing so it costs nothing, and reserve
      the parse-failure condition for genuine syntax errors. Do not
      advertise the leniency in the card: a card that is approximate
      teaches that the card is approximate.
- [ ] **Register is the thing at risk without prefill**, and three
      things blunt it: thinking gives deliberation somewhere else to
      go; the model's own prior entries are bare code, so the penalty
      *shrinks* as a session grows; and the seed exemplar (Step C4)
      covers turn one, the weak point where history is empty and there
      is nothing to imitate. That is a second, independent reason for
      the exemplar — insurance for the single transport, not only
      against timidity.
- [ ] **A truncated completion is a condition, not a crash — and
      thinking makes it likelier.** Reasoning tokens are output tokens,
      billed as such and counted against `max_tokens`, so the ceiling
      must cover reasoning *plus* a long program. A truncated result
      does not parse, which is already a trap with a handler (Part C),
      but the report must distinguish **ran out while thinking** from
      **ran out while writing**: the first is fixed by lowering
      `reasoning_effort`, the second by writing a shorter program, and
      neither is fixed by hunting a missing brace. Set the ceiling
      generously and say which budget was exhausted.
- [ ] **A max-tokens ceiling is still required.** End-of-turn handles
      stopping on the one transport built, so no stop sequence is
      needed — but a runaway completion must be truncated rather than
      parsed as program source, and truncation is a condition (above),
      not a crash. If the raw variant is ever built it needs a sentinel
      line, since with no fence it has no stop marker at all.
- [x] The document is assembled by one function with a golden-render
      test, the way the condition report is (`report.rs` precedent).
      The renderer emits the card, the grouped turns, and nothing else
      — so a future transport changes only how those bytes travel.

---

## Part B — The document

The request is a chat exchange whose **content** is a code document:
user turns report, assistant turns are programs and nothing else.

```
system       ── the card ── (fixed forever; the cache prefix)
             Programs are written here. Emit only valid JavaScript;
             the whole response is parsed. …

user         [1] robin: can you fix the ledger parser?

assistant    const rows = await read_file("ledger.csv");
             //: partitioning the malformed rows before touching the parser
             const bad = rows.filter(r => !r.id);
             append_history(`${bad.length} malformed rows, all missing id`);

user         [2] the program above completed
             [3] effects: wrote src/parse.rs; ran `cargo test` (exit 0, #51);
                 read 43 files
             [4] note: 4 malformed rows, all missing id
             [5] robin: good — now handle the quoted-comma case

assistant    ← the model writes here
```

### Step B1 — Layout: role-delimited

- [x] **An assistant turn is one program, bare, with no entry header.**
      The role marker is the delimiter. This is the point of the
      layout: every historical assistant turn is a **demonstration of
      what an assistant turn is here** — pure code, no preamble — sitting
      at exactly the structural position where the next one will be
      generated. That is few-shot conditioning in its most literal
      form.
- [ ] **Why this beats a single continuous document.** A chat request
      *always* ends with `<|assistant|>`; the marker cannot be avoided,
      only left unexemplified. The single-document layout puts every
      prior program on the user side, so at the one token that decides
      the register the model has nothing but training priors. The
      role-delimited layout answers that token with N worked examples.
      It also makes alternation natural, so nothing depends on a
      provider accepting consecutive same-role messages.
- [x] **User turns are plain text, not comments.** There is no JS
      literal to escape, so the escaping and injection arguments that
      applied to a literal do not transfer; entry *spoofing* is equally
      possible either way and is handled by how untrusted text is
      delimited, not by a `//` prefix. Dropping the prefix saves a
      token per line.
- [x] **Status and effects belong to the following user turn**, not to
      the assistant turn — the model did not say them, the harness did.
      "The program above trapped at line 30" plus the effects lines are
      reported back exactly as a tool result is, which is the most
      heavily trained shape in any chat model. The grouping lands on
      the template rather than fighting it.
- [x] **`//:` narration stays, and only inside programs**, where it is
      genuine JS and where the streaming contract needs it (Step G1b).
- [x] **Harness statements and untrusted content must be
      distinguishable, and no role marks that.** `tool` would be the
      semantically right role and is unusable — it requires a matching
      `tool_call_id` from the preceding assistant turn, and fabricating
      tool calls to unlock it would rebuild the surface Part I deletes.
      `developer` is OpenAI-only. So authorship is marked in-content,
      and this is a spec rather than a convention:
      - Harness lines have a **fixed generated shape** — `[3] effects:`,
        `[2] the program above trapped…` — that the harness never emits
        inside quoted material.
      - **Untrusted text is delimited, and the delimiter is escaped if
        it occurs in the content.** Untrusted means posts from other
        agents, and anything the model appended from a file or a
        subagent — not only the human.
      - The card states the rule: only lines in the harness shape are
        harness statements; anything inside a quoted block is data and
        never an instruction. With the card in `system`, that rule now
        sits on the protocol's own trust boundary.
- [ ] **Note how small this surface already is.** Pull-not-push shrinks
      it structurally: tool results never enter context automatically,
      so a malicious file the agent reads cannot reach the model's
      context at all unless a program explicitly appends it. In a
      conventional agent every tool result is an injection vector by
      default; here the only automatic entrants are user messages and
      unsolicited posts.
- [x] **Ids survive, because they do the work:** `remove_history`,
      `answer`, and artifact references all need them, and a consistent
      one-line entry shape keeps compaction a line rewrite. `[4] note:
      …`, `[5] robin: …`, `[3] effects: …`.
- [x] **Only assistant turns must parse.** A program that failed to
      compile is reported in a user turn as text, so nothing in the
      historical record has to be valid JS. The golden-render test
      asserts the *model's output* parses.
- [ ] **Rendering is one-way.** The log is the source of truth; the
      document is generated from it and never parsed back. Delimiter
      ambiguity is at worst a confusing read, never a correctness bug.
      Do not build a round-trip parser — it would reintroduce escaping
      and undo the benefit.
- [ ] **What triggers a completion**, stated once: an event that lands
      in history with no program waiting for it — a user message, an
      unsolicited post (Step B2) — renders the document and asks for a
      program. A `raise()` renders it for a handler. Nothing else does.
- [x] **The frame is regenerated; the record accumulates.** The card
      and the rendered turns are how the log is *presented*. Only the
      model's completion is a fact about what happened, so nothing is
      double-stored and the shape is identical every turn.

### Step B1b — Grouping, and why it is derivable

The message grouping is a fold over the spine that splits at program
entries. **Nothing new is stored**, which is what makes the layout
rendering comparisons deferred in Part H renderer swaps rather than
schema changes.

- [x] Assistant message = one program's source. User message = every
      event between that program and the next: its effects entry, posts
      that arrived, anything appended. Order is total along a spine, so
      the partition is deterministic.
- [x] **Two programs can never be adjacent**, since a completion is
      only triggered by an event landing — so a user message is never
      empty, which some APIs reject. Assert it.
- [ ] **Handler programs never appear as assistant turns.** They are
      transient (Step B2). A handler's own request still ends with
      `<|assistant|>` and still shows prior root programs as assistant
      turns, so it gets the same demonstrations.
- [x] **Grouping lives in the render function and is golden-tested.** A
      change to how events partition into messages silently invalidates
      every session's cache — the same hazard class as editing the
      card.

### Step B1c — The document only ever grows at the end

Prefix caching is a byte-level property, so the append-only log is not
enough on its own: the *rendering* must be append-only too.

- [ ] **DeepSeek and OpenAI cache automatically on the token prefix**;
      **Anthropic uses explicit breakpoints**, marked on the last
      stable turn. Role-delimited turns make that natural — there is a
      real message boundary to mark. Verify breakpoint count, the
      minimum cacheable length (below it nothing caches), and TTL.
- [x] **Nothing in flight is rendered.** The trap is status: rendering
      a running program and later rewriting its outcome mutates bytes
      inside the cached prefix. A program becomes part of the record
      only when finished; while it runs it lives in the transient tail
      with the condition report.
- [x] **The tail: what it is and where it goes.** Ephemeral content
      for one request only — the condition report when prompting a
      handler (Step D1), the `vm` pointer once Part F lands, anything
      the mind should see now and never again.
      - **It is appended to the final user turn's content**, not sent
        as its own trailing message. `request_body` currently does the
        latter, which was right when a request was one growing user
        message but would produce two consecutive user turns under the
        role-delimited layout.
      - **It costs nothing.** Prefix caching keys on tokens, not
        message boundaries, so everything before the tail still
        matches; divergence begins at the tail position, which is
        exactly where new content begins on the next request anyway.
      - **It is never logged**, so it cannot leak into the record — the
        same rule the existing comment states (`prefix is immutable`),
        and the reason the record stays append-only in bytes.
- [ ] **Card edits invalidate every session.** Tuning it (Part H) is
      a restart-shaped operation, not something to do mid-conversation.

### Step B2 — What enters history automatically

- [x] **Exactly DESIGN.md's rule C, and nothing else: data enters
      context precisely when no program is waiting to receive it.**
      - a user message, or an unsolicited post from another agent →
        history, automatically;
      - an `ask()` answer → a value in the awaiting program, not
        history;
      - a handler's returned value → a value in the raising program,
        not history;
      - a root program's completion → nothing; it reaches people
        through `say()`.
- [x] **An effects row per program, automatic.** A program's source is
      *intent*, not outcome — a loop over a computed file list does not
      say which files were written. So each program entry is accompanied
      by a compact record of what it did to the world:

          // [4] effects of [3]: wrote src/parse.rs, src/lex.rs;
          //   ran `cargo test` (exit 0, #51); read 43 files;
          //   spawned reviewer

      The line this draws: **automatic for facts about the world,
      opt-in for interpretations.** A file written is not the mind's
      private business — the user needs it, the future self needs it,
      and nobody was waiting for it, which makes it rule-C-shaped. What
      any of it *meant* is `append_history`, because only the mind can
      author that.
- [x] **Facts and identities, never content — not even truncated.**
      `#51` is the call id, not the output. Truncation is too
      little to work from and too much to be free, and it is exactly
      the machinery Part I deletes (`DEFAULT_ANSWER_BUDGET`, report
      clipping); reintroducing it here would rebuild the channel this
      phase exists to close. Everything is one fetch away by id.
- [ ] **The human's view is not the model's context.** The temptation
      to dump content is almost always "so the user can see it" — and
      the user has a better channel: the TUI renders reads and diffs
      live from `Call`/`Result` events (Step G3b). Once those are
      separate, the case for content in history disappears.
- [x] **Reads aggregate, writes itemize.** A read changed nothing and
      its only content is "I looked"; a program reading 500 files in a
      loop must not produce 500 entries, and its source already says it
      read every `.rs` file. Writes, commands and spawns are durable
      changes someone may need to review or undo, so they are named. A
      very long write list caps with a count and an id — capping a list
      of *names* is not truncating content.
- [x] **One row per program, never per call.** This is the property
      that makes the row safe: effects grow with the number of programs
      written, never with the amount of work done. A program doing 500
      operations costs the same as one doing three.
- [ ] **`say()` is append-plus-deliver.** The user's own conversation
      cannot be opt-in, or the model repeats itself or loses track of
      what it already said. This makes divergence between history and
      what the user believes impossible by construction.
- [ ] **Root programs are rendered; handler programs are not — but
      everything is logged.** The rendered record shows what the mind
      did at the top level; the stack is transient in *context* (Part
      D), not in the log. A handler's effect on the rendered record is
      the value it returned or something it explicitly appended. This
      one line is what the deleted forks phase spent five parts trying
      to buy.
- [ ] **Handler programs must be logged**, same as any other
      completion: they are real programs with real effects, and Part
      H's raise-placement diagnostic — the one that matters — cannot
      read raise sites that were never written down. Not rendered, not
      lost: the artifact rule again.
- [x] Test: a root program that raises forty times and completes adds
      exactly two entries to the rendered record — its own
      source-with-status, plus whatever it appended — and no entry per
      raise, while the log holds all forty handlers.

**Gate before Part C**: render a document from a fixture log with every
entry kind — including a program that failed to compile — and assert
that the *completion region* parses, that ids and labels round-trip,
that a forty-raise program contributes no interior, and that appending
an entry leaves every preceding byte unchanged.
`cargo test --workspace document`.

---

## Part C — The program surface

The verbs available to a completing model. Composition stays JS
(`Promise.all`, loops) — `17_BRANCHES` already fixed that as the rule.

### Step C1 — The harness verbs

- [ ] `say(text)` / `say(to, text)` — reach the user or an agent.
      Appends and delivers.
- [ ] `ask(who, text)` → Promise. The awaited answer is a value.
- [ ] `answer(question, value)` — discharge an inbound question,
      resolving the asker's promise. **Not `say()`**: a `say` to the
      asker is a *tell*, which delivers text and resolves nothing
      (`DESIGN.md`'s exchange table, with the `Answer` column blanked).
      `18_TARGETING`'s rule is why it must be explicit — an implicit
      answer and an oblivious one produce identical bytes. Inbound
      questions are history entries, so the question is addressable — and
      carries the same label checksum every id-bearing call takes
      (Part B), for the same reason: `answer(7, "ask from planner", {…})`.
      Answering the wrong question is as silent as compacting the wrong
      row.
- [ ] `spawn(charter)` → an agent handle (clean room); `fork()` → a
      handle to a context inheriting this one's history.
      `list_agents()` for the subtree with status. **Branches exist for
      minds, not for programs.** Synchrony is `await`, not a parameter
      — see Step C3.
- [ ] `artifact(id)` — fetch a completed call's value from the log by
      id, the mechanism behind every "one fetch away" in this file
      (DESIGN.md's `tools.tool_result`, renamed with the namespace).
      **Not `result(id)`**: `result` is one of the most common local
      names in JS and would be shadowed constantly. `artifact` is the
      noun this project already uses for the thing being fetched.
- [ ] `append_history(value)` — the mind's voluntary channel into its
      own future. The card teaches: append a *projection*, not the raw
      result.
- [ ] Ordinary tool calls are plain async functions returning values.
      Nothing about calling one puts anything in context.
- [x] **The harness vocabulary is bare-global; `tools.*` stays for
      configured capabilities.** Revised from an earlier, broader "no
      `tools.` namespace at all" — that version required either
      teaching the compiler a runtime tool registry or changing the
      undeclared-global fallback for *every* bare call (JS-conformance
      relevant, touches `interp`'s core call resolution for a set the
      compiler cannot see statically). Splitting instead:
      - **Bare**: `say`, `ask`, `answer`, `spawn`, `fork`,
        `append_history`, `artifact`, plus `resume`/`abandon` (Step
        D2's decision constructors — not tool calls at all, so
        `tools.resume(...)` would be actively misleading). All fixed,
        closed, identical for every agent — the compiler can know the
        whole list statically, exactly as it already knows `raise`
        (precedent already in `interp/src/compiler/call.rs`).
      - **Namespaced**: `tools.read_file`, `tools.bash`, and whatever
        else an agent's registry holds — per-agent, dynamic, no static
        list the compiler could ever see. Step C2 keeps this surface.
      - `read_file(path)` is wrong; `tools.read_file(path)` is right.
        `say(text)` is right; `tools.say(text)` is wrong (nothing is
        being invoked as a *tool* — a bare call is what ordinary JS
        looks like for a language-level verb, and it is a token saved
        on exactly the calls that appear most often in any program).
      - Compiler change: a small, closed set of new match arms in
        `compile_global_call`'s existing name-based dispatch (where
        `raise` already lives) — additive, does not touch the
        undeclared-identifier fallback, so every other bare call keeps
        today's `ReferenceError` semantics unchanged. Shadowing is
        unaffected: `compile_user_call` resolves a local binding before
        ever reaching this dispatch, so a program that declares its own
        `function ask(){}` calls that, not the harness verb.
- [x] **Parsing lands** (`agent/src/codemode/verbs.rs`):
      `parse_effect(vm, call)` turns one `Invoke` from any of the
      seven into a typed `HarnessEffect`, arity- and shape-checked —
      not wired to a host dispatcher or the event log yet (no
      `machine.rs`/`tree.rs` change), so nothing actually delivers a
      `say`, spawns an agent, or discharges a question. That is the
      next layer, and it is squarely the tree/log integration this
      phase has deliberately held off on.
- [ ] Errors take the same path as `raise()` — DESIGN.md's table
      already says a trapped error and a raise are rows of one shape;
      here they are literally one mechanism.

### Step C2 — The tool surface

The question this step answers: **does the interaction pattern a coding
agent's users expect survive?** Read/edit/run tooling, seeing what the
agent is doing, interrupting mid-flight. It survives, and two parts of
it get better.

- [ ] The tool surface stays under `tools.*` (Step C1's revision) —
      `tools.read_file`, `tools.write_file`, `tools.edit_file`,
      `tools.bash`, `tools.grep`, `tools.fetch`. No schemas, no
      registry rendered into the document beyond the card's list. The
      names and signatures are card surface.
- [ ] **Containment is the sandbox, not a dialog.** The program runs
      freely inside whatever boundary the process is given; there are
      no per-call approval prompts. Deliberate: forty prompts is worse
      than forty tool calls, and it would spend exactly the round trips
      this phase is buying. (Recorded because the mechanism is
      *available* for free if it is ever wanted — a gated call is just
      a condition, and the user is already DESIGN.md's outermost
      handler, able to supply a value without spending an LLM turn.
      Not building it.)
- [ ] **Interruption already works, and works better here.** A user
      message during a running program is a condition
      (`17_BRANCHES`); the mind writes a handler that can
      `return abandon()` and rewrite, keeping every completed call by
      id. A chat agent
      usually has to abort and start over.
- [ ] **The program is the plan.** Plan-then-approve does not need a
      plan *mode*: the program is written before it runs, so the TUI
      can show it, let the user edit it, and then run it. `19_UX`'s `e`
      gesture is already "replace this program with typed JS". That is
      strictly better than approving prose, because the artifact
      approved is the artifact executed.
### Step C3 — Plans, and the shape of the verb set

- [ ] **Conversation still plans, and needs nothing.** A chat turn is a
      two-line program that calls `say()`. The exchange lands in history
      as rows, so when the user says "ok go" the informal plan is
      *addressable data* — the program can reference `history[7].text`
      while being written against it.
- [ ] **A plan encoded as program structure is the todo list.** Chat
      agents bolt on a todo tool because prose plans drift and step 4
      gets forgotten. Here the plan is executed rather than remembered:
      step 4 is a statement, the program counter is the progress
      indicator, and re-planning is `return abandon()` plus a rewrite
      that keeps completed steps by artifact id. No todo mechanism is
      needed, and none should be added.
- [ ] **Card — what a step boundary costs.** `raise()` spends an
      inference on the *parent's full context*; `spawn()` spends one on
      a *child's clean context*. So a large self-contained step is
      nearly free to the orchestrator and should be a spawn; a step
      needing the parent's own judgment is a raise; everything else is
      plain code. A `raise()` per step is a tool loop in JS syntax and
      costs more than the loop it imitates — this is the
      raise-placement failure (Part H) written as guidance.
- [ ] **Keep encoded plans coarse.** Steps 5–10 are written blind, so
      phases survive rewriting and micro-steps do not.

- [ ] **`raise()` is not a cell in any table.** Its suspension is not a
      synchrony choice — it is what a condition *is*: the handler needs
      the frame alive to decide about it, needs the caller's history,
      and holds authority over the caller (`abandon()`). One of a kind,
      factored out before the delegation space is drawn.
- [ ] **The delegation space is [wait, detach] × [inherit, blank], and
      only one axis needs a verb.**
      - **Synchrony is `await`**, not a flag. `await spawn(c).ask(q)`
        is synchronous; `const a = spawn(c); …; await a.ask(q)` is
        detached. Composition is JS (`17_BRANCHES`).
      - **Inherit vs. blank slate is the real bit, and it is already
        the tree's own vocabulary**: `spawn(charter)` creates an
        `Agent` — clean room — and `fork()` creates a `Fork` —
        inherits. `context()` resets at one and carries through the
        other (`17_BRANCHES`), so these are the two branch-root
        payloads surfaced to a program, not new machinery.
- [ ] **`fork()` fills a cell the design could not previously express.**
      A context that knows everything I know, returns a value, and has
      *no authority over my execution* — "given this whole
      conversation, which of these three files is right?" wants the
      context, not a handler.
- [ ] **The concrete payoff: parallel judgment.** You cannot
      `Promise.all` over raises — each suspends the whole frame, so
      judgment calls are strictly serial. Over forks they are ordinary
      promises: `await Promise.all([fork().ask(a), fork().ask(b)])` is
      three full-history judgments at once.
- [ ] **The detached-inherit cell fills itself:** `const m = fork();`
      without awaiting leaves a copy of this mind running alongside —
      the monitoring case the deleted forks phase needed an `inline:
      true` flag for.
- [ ] **Card — what each costs.** A fork carries the whole history, so
      it is a full-context inference; a spawn is clean-room and cheap;
      a raise spends the *parent's* context and stops the program. Fork
      when the judgment needs the conversation, spawn when it needs a
      charter, raise when the frame itself is the subject.
- [ ] Explicitly rejected: a generalized `run({inherit, detach})`. One
      of those bits is `await` and the other has two well-named verbs;
      a boolean-flag API would erase both facts and is the shape a
      model miscalls.

- [ ] Note for `15_COMPAT.md`: every gap between this VM and real JS
      shows up here as a trap → handler → rewrite cycle, which spends
      exactly the round-trips this phase is buying. Compat is
      load-bearing for the agent bet, not a parallel hobby.

### Step C4 — The card is the system prompt

Under code mode there is no separate dialect card and no instruction
header: the system prompt is the only place the model is told anything,
and it is also the immutable cache prefix. `dialect.rs`'s card and the
system prompt become **one artifact** — which is a deletion, and the
reason this step exists rather than leaving guidance scattered through
Parts C and E.

It is the only steering wheel this design has. Treat
it the way `DESIGN.md` treats the condition report: a prompt-engineering
artifact with golden tests, iterated against live transcripts, not a
string constant someone edits in passing.

- [ ] **A spec, not a persona.** `system` makes the temptation
      stronger, so state the rule: "You are a helpful assistant that…"
      is the failure mode; "programs are written here; emit only valid
      JavaScript; the whole response is parsed" is the register.
- [ ] **One file, one golden test, versioned.** A card change is a
      behaviour change; the tuning log (Part H) records which change
      moved which number.
- [ ] **It carries the verb set and nothing else structural** — the
      tools in scope, `say`/`ask`/`answer`, `spawn`/`fork`/`raise`,
      `append_history`, and the decision values. No schemas: the
      signatures are the documentation.
- [ ] **Seed with one worked exemplar, as a real user/assistant pair
      opening `messages`.** It cannot live in the card — its point is to
      demonstrate an *assistant* turn (Step B1). The model's own prior
      programs are its few-shot evidence (the thesis), so turn one has
      nothing to imitate and a timid first program compounds. The
      exemplar is the restoring force, and it is cheap insurance rather
      than a remedy applied later.
- [ ] The guidance it carries, collected from the parts that derived
      each line:
      - **A root program that never calls `say()` is a silent no-op.**
        With no return value, that is the one new way to do nothing.
      - **Open with a `//:` plan block.** Those lines stream first
        (Step G1b) and double as the plan the program follows.
      - **`//:` is what I am about to do; `say()` is what happened.**
        Only `say()` can report a result, because only it runs.
      - **Look before you leap, once.** When the data's shape decides
        the approach, a small reconnaissance program then the real one
        beats guessing — two round trips, not twenty. It restores the
        observation-conditioned adaptation a one-shot program gives up.
      - **Match the program to the task.** The push is against timid
        *orchestration*, not against short programs; a question needing
        no tools is a two-line program that says the answer.
      - **What a step boundary costs** (Step C3): fork when the
        judgment needs the conversation, spawn when it needs a charter,
        raise when the frame itself is the subject.
      - **Compaction prefers removal and verbatim retention**
        (Part E); rewrite only rows that genuinely need it.
      - **Return a decision, not a dataset.** Data stays an artifact
        reachable by id; `append_history` takes a projection.
      - **Append for your future self across tasks, not to read
        something back next turn.** If you need the data now you are
        already holding it in a variable; appending it and ending the
        program is a message to yourself the long way round.

---


## Part D — `raise()`, the handler stack, and the decision value

`raise()` suspends the running program and asks the mind for a decision.
The mind answers by **writing another program**. That program runs while
the raising frame is still live — Lisp's actual semantics — so a branch
needs a **stack of VMs**.

### Step D1 — The stack

- [ ] Host-side `Vec<Vm>`, innermost scheduled, outer frames frozen at
      safe points. **No VM is ever on another VM's stack.** The
      load-bearing property is unchanged and a test asserts it.
- [ ] **Outer frames are frozen for execution only — their in-flight
      calls keep landing.** A program that raises may have outstanding
      async calls; those keep resolving into the log while the handler
      deliberates (`17_BRANCHES` already requires this for interrupts),
      so promises may already be settled when the frame resumes. Work
      continues during deliberation.
- [ ] **The stack is session state. A crash collapses it, never
      rebuilds it.** VMs are never persisted and recovery is
      re-execution; with determinism a non-goal, re-running an outer
      program may not raise in the same place at all, orphaning inner
      frames. On restart the branch comes back with its history — which
      is durable and now explicit — and the mind writes one new
      program.
- [ ] **A frame blocked on `await` is the easy case.** It is not
      executing, so it is definitionally between instructions — no
      fuel-slice boundary is needed to reach a safe point. A user
      question during `await spawn(c).ask(q)` suspends immediately, and
      the spawned agent **keeps working** while parent and user talk
      (outer frames are frozen for execution only). The interruption
      costs no progress; a chat agent interrupting a tool call has to
      abort it.
- [ ] **`resume()` takes no value for a posted condition.** A `raise()`
      has an expression waiting for one; a user interrupt does not, so
      `return resume()` means "continue, nothing changed" and the frame
      resumes still awaiting its promise. The canonical interrupt
      handler is `say(…)` then `return resume()`; `return abandon()` is
      what you write when the remark means the plan should change.
- [ ] Depth is bounded by host policy, and hitting the bound is itself
      a condition. Depth is expensive in a way stack depth normally is
      not: every frame is a full-context inference.
- [ ] The handler's prompt is the ordinary document plus a **transient
      condition report** for the frame it is deciding about. It vanishes
      as the stack unwinds and never becomes history. Part F is what
      later shrinks this to a pointer, once `vm` makes it queryable.

### Step D2 — The decision is the handler's return value

- [x] **`resume(value)` and `abandon()` are pure constructors.** They
      build a tagged decision and do nothing else. A handler expresses
      its choice by returning one: `return resume({rows: 4})` or
      `return abandon()`.
- [x] **Only a decision counts.** A handler that returns nothing,
      returns a non-decision, or falls off the end has decided nothing,
      and the condition re-fires with a report saying so. Neither
      default is safe — implicit `resume(undefined)` feeds garbage into
      a live program, implicit `abandon()` silently discards work —
      and this is `18_TARGETING`'s rule one layer down: an implicit
      answer and an oblivious one produce identical bytes, so nothing
      short of the explicit value can be trusted. The detection side
      (`agent/src/codemode/decision.rs`'s `read`, returning `None`
      uniformly for all three non-decision shapes) is built and
      tested; re-firing the condition on `None` is host-loop wiring,
      not built.
- [x] **Misuse is inert, not wrong.** `resume(v)` on its own line has
      no effect, so the failure mode is a loud re-fire rather than a
      silent resume with the wrong value. That is why these are
      constructors and not methods that record state on an object.
- [ ] **Top-level `return` comes back, and the earlier ban was the
      right instinct diagnosed slightly wrong.** The danger was never
      the keyword — it was `return <anything>` being read as a value,
      so a model writing an idiomatic early exit would accidentally
      resume with `undefined`. Tagging fixes that at the type: an
      untagged return is not a decision.
- [ ] **A root program's return means nothing, and the emptiness is
      load-bearing.** `return;` at its top level is an ordinary early
      exit and simply ends the program — ignored, never rejected. But
      it carries no meaning, deliberately: the card promises the mind
      it will not be asked again once this program ends, and if a root
      return could say "here is a summary" or "I am not finished", that
      promise becomes negotiable and ending early gets cheap. Two
      tempting readings, both rejected: *return as append* is a second
      spelling of `append_history` with worse timing (append when you
      learn, not at the end); *return as a completeness signal* is the
      multi-program-per-turn loop coming back through a side door. The
      legitimate case behind the second already has a verb — a program
      that genuinely cannot finish calls `raise()`, which keeps the
      frame alive instead of throwing it away.
- [ ] Returning a *decision* from a root program **is** an error: there
      is no caller to decide about, so it means the model confused the
      two program kinds.
- [x] **Nothing here is control flow.** No unwinding, no bypassing user
      `try`/`catch`/`finally`, no JS semantics bent. `raise()` still
      bypasses in-program handlers (DESIGN.md) — that is suspension,
      which is a different thing from choosing.
- [ ] **A handler is therefore the same shape as a program**:
      `(history, vm) → decision`, matching DESIGN.md's `(input, tools,
      artifacts) → returned JSON + effects`. Not a second concept.
- [ ] **`abandon()` sequencing:** the raising program is discarded, and
      once the handler returns, the mind is prompted for a replacement
      program at that stack level. The handler does not write the
      replacement inline; it is the next completion.
- [ ] **`abandon()` replaces the caller frame; it does not unwind the
      stack.** From frame 3, discarding frame 2 while frame 1 waits on
      a raise nobody will answer is incoherent. The replacement becomes
      the new frame 2 and frame 1 continues as if nothing happened.
      Abandon is "rewrite my caller", safe at any depth.
- [x] **Naming: `resume`, not `answer`.** `answer(question, value)` is
      already taken and means something else — discharging an inbound
      question, the only way to close an open post (`18_TARGETING`,
      `machine.rs`). `resume`/`abandon` is also the better pair: both
      name the *caller's* fate, which is the only thing a handler
      decides.
- [ ] **`vm` is postponed to Part F.** With decisions as values, the
      handler needs no handle to act through — only to *look* through,
      which is introspection and lands later. Until then the handler's
      prompt carries a rendered condition report, as today.

---

## Part E — Compaction as a condition

- [x] **Fires at a headroom threshold, not at overflow.** The handler
      is itself an inference whose prompt contains the history that
      already does not fit; there must be room for the handler's prompt
      and its program. Triggering at the failing `append_history()` is
      too late by construction.
- [ ] The handler is an ordinary program with `remove_history(id,
      label)` and `rewrite_history(id, label, value)`. It ends with
      `return resume()`, and the append that triggered it proceeds.
- [ ] **Compaction appends a projection event; it never mutates the
      log.** History is a projection; a compaction changes how the log
      projects from that point forward. The edit is auditable,
      replayable, and a branch forked before it still sees the
      original. Append-only survives.
- [x] **Never drop an id — only content.** A compacted entry leaves a
      stub: id, label, one line — a line rewrite, which is why the
      one-line entry shape (Step B1) suits compaction better than a
      literal would. Lossy but recoverable, because the
      result is still in the log and still fetchable. This is the same
      move the artifact model already makes, and it bounds the damage
      when the mind compacts wrong, which it will.
- [x] **The program is checked before it commits**: every id still
      present as at least a stub, size actually below the threshold, no
      row silently emptied. On failure the condition re-fires. This is
      the real advantage over regenerative summarization, above speed —
      a summary blob is opaque and cannot be asserted on.
- [ ] Header line (Step C4): **prefer removal and verbatim retention;
      rewrite only rows that genuinely need it.** The saving is
      proportional to what is *not* re-emitted. A compactor that
      restates everything more concisely buys nothing but syntax
      overhead.
- [ ] **Cost to state plainly and not over-claim:** compaction
      invalidates the prefix cache no matter how it was produced, so
      the next request re-reads the whole new prefix at full input
      price. Program-based compaction saves the decode, not the
      re-read — and the re-read is the half the user sits through.
      Therefore: compact **rarely and hard**, one handler run
      reclaiming a lot, never trimming at the margin.
- [ ] If the LLM compactor cannot free enough, host policy truncates
      bluntly and the user is the final authority — DESIGN.md's
      existing handler hierarchy, no new mechanism.

---

## Part F — Frame introspection

The load-bearing property is stated as "stop between two instructions
with the heap, log, await-chain, and console buffer inspectable." Today
all of that is spent rendering a *text report* the harness clips. A
handler that can query it instead is the largest unrealized payoff in
the architecture.

- [ ] A handler binds **`vm`**: a read-only, stable projection of the
      frozen frame it is deciding about — `vm.locals()`, `vm.frames()`,
      `vm.console()`, `vm.condition`, `vm.source`, and `vm.caller` to
      read further up the stack. Absent in a root program, which has no
      frame to decide about. A projection, not `CallFrame` itself —
      handler programs must not couple to interpreter internals, or
      `8_HARNESS`'s version-drift problem gets much worse.
- [ ] **Read only, with no exceptions.** Nothing executes and no host
      callback enters the VM, so reading is safe; writing would break
      the spine, since recovery is re-execution and a heap poke is not
      in the log. `vm` has no methods that change anything — the
      decision is a *returned value* (Step D2), which is why this rule
      needs no carve-out.
- [ ] Lazy and fuel-metered. A live heap can be large and cyclic;
      access fetches on demand rather than serializing up front.
- [ ] **The report shrinks to a pointer.** Once `vm` is queryable,
      Step D1's rendered condition report collapses to one line —
      "trapped at line 30, bound as `vm`" — and the handler pulls what
      it needs. Less permanent context, not more.
- [ ] **Salvage on abandon.** Today only work that went through tool
      calls survives a rewrite (artifacts by id); in-memory computation
      is lost. A handler can pull partial results out of the dying
      frame and `append_history` them before replacing it. "Rewrite the
      program but keep what it computed" becomes possible for the first
      time.
- [ ] Anything the handler learns is a **transient view** — after a
      crash the frame does not exist. What matters must be appended or
      passed through the returned decision.
- [ ] The same projection powers the TUI debugger pane, so this is not
      a single-purpose API.

---

## Part G — The TUI

`9_TUI`/`19_UX` own the surfaces; this part lists only what code mode
changes. The TUI is the observation instrument for this phase
(`DESIGN.md`, "Product surface"), so it is not optional polish — Part
G2's transcript reading depends on it.

### Step G1 — The code view shows the completion, and only that

- [ ] The source pane renders the model-written program alone, never
      the card or the reported turns. This mostly **falls out of
      storage rather than needing a filter**: the card and the record
      are regenerated per request and never logged, so a program entry
      holds only the completion (Part B). Assert it —
      a program entry whose stored source contains the card is a bug
      in the transport, and the test belongs next to the golden-render
      test.
- [ ] The pane streams as the program is written. This is the best
      debugging affordance the design offers — but it is a debug
      surface, not a progress indicator for an ordinary user.

### Step G1b — Streamed prose, recovered

Code mode does not lose streaming; it loses *streamed prose*. Today the
first token of "Let me check the ledger…" lands in about a second. Here
the first user-visible output is `say()`, which cannot run until the
whole program is generated — and the gap widens exactly as the phase
succeeds, since the goal is longer programs.

- [ ] **The `//:` contract.** A comment line prefixed `//:` is
      user-facing narration: the chat pane renders it as it streams,
      wherever it appears in the program, so narration can sit above
      the section it describes. A per-line marker rather than a
      `BEGIN`/`END` pair on purpose — a forgotten terminator would
      swallow the whole program into the chat pane, and nesting would
      be undefined.
- [ ] **Streamed, never separately stored.** The text is already in the
      log inside the program's own source; recording it again as a
      message would double-store a body, which DESIGN.md's exchange
      rules forbid. The pane extracts it from the source, identically
      live and on replay.
- [ ] **Rejected: executing statements as they stream.** The program is
      not valid until complete, and partial execution would wreck
      re-execution semantics. The gap is closed by streaming what the
      model writes, never by running it early.

### Step G2 — A document view

- [ ] A dedicated view showing the **exact bytes sent** — card, every
      turn, role markers included — for the last (or current) request.
      Not mixed into the code pane: one answers "what did it write",
      the other
      "what did it see". Under code mode the second question is the
      whole game, and today's equivalent is scattered across the
      request-rendering tests.
- [ ] Reachable from `View::FullDebug`, alongside the existing
      debugging gestures.

### Step G3 — The chat pane

- [ ] There are no assistant text turns to render any more. The pane's
      content model becomes **history entries, rendered**: user messages,
      `say()` output, and appended notes.
- [ ] A program entry renders collapsed — a one-line affordance naming
      its status and size, expanding into the source pane — not inline
      source. A 200-line program inlined in chat destroys the pane,
      and the whole point of the phase is that programs get longer.

### Step G3b — Tool activity, live

- [ ] The chat pane renders the running program's calls as they land —
      "read `foo.rs`", "edited `bar.rs`" with a diff — from the `Call`
      and `Result` events, not from anything the model emits. This is
      the familiar coding-agent surface, and it is *better* here: the
      calls stream from a running program rather than arriving one per
      turn.

### Step G4 — The program stack

- [ ] The debugger gains a view of the **program stack** (Part D): which
      program is suspended at which raise site, and which handler is
      running above it. `programs_for` is a flat per-branch list today;
      a branch now has depth.
- [ ] **Naming, deliberately:** `frame` stays reserved for the VM call
      stack (`CallFrame`, `VM::frames()`, the existing stack pane) —
      `17_BRANCHES` fixed that and this phase must not blur it. The new
      nesting is the **program stack**, whose levels are *programs*.
      Two stacks are now visible at once: one program's call frames,
      and the stack of programs. Label both in the UI.

### Step G5 — Compaction must be visible

- [ ] A compaction (Part E) renders as a marker in the chat pane naming
      what it collapsed. The user must never silently lose conversation
      they can see.
- [ ] A compacted stub is inspectable: the original is still in the log,
      so the pane can expand it on demand even though it no longer
      renders into the model's document. This is the human-facing half
      of "never drop an id, only content".

### Step G6 — What gets simpler

- [ ] The navigator lists agents and forks — both are minds, including
      one a program made with `fork()` (Step C3), since a fork writes
      its own programs. Nothing about a program's *execution* appears
      there. No filter, no origin flag, no auto-follow rule; the deleted
      forks plan needed all three, because it created a branch per
      dispatch. A `fork()` is created deliberately by a program, so
      there is nothing to hide.
- [ ] The `e` rewrite gesture (`19_UX` Part A) — typing literal JS to
      replace a suspended program — stops being a special debugging
      back door and becomes exactly what the model does: a human
      writing a handler program by hand. Same mechanism, one authority
      up the hierarchy. Check whether its `FullDebug`-only restriction
      still makes sense once that is true.

---

## Part H — The regression harness

Placed here, after the system exists, because a measurement of nothing
is nothing. Small on purpose: the card is the only steering wheel and it
cannot be turned blind, but "not blind" is a handful of tasks and a
habit of reading transcripts, not a benchmark suite.

- [ ] **Four or five fixed tasks, scripted, no network**, each one where
      a large program is the right answer: fan-out over N inputs,
      retry-and-branch, a pipeline with a judgment call in the middle.
      Each has a checkable success condition. This exists so that
      tuning the card cannot silently break something that worked.
- [ ] **Three numbers per run**, and no more until one of them fails to
      answer a question: median program length (statements), LLM
      round-trips per user request, and task success. Adding metrics
      before they are needed measures the wrong things — this list was
      seven, and the register proxy in it turned out to be a compliance
      check.
- [ ] **Reading transcripts is the real instrument**, and the
      raise-placement diagnostic is a reading exercise, not an
      aggregate. `raise()` is the only way the mind can summon itself,
      so every relocation of the gravity well runs through a raise
      site. Raises clustered immediately after a tool call, on data the
      program could have handled, mean the rhythm moved inside the
      program rather than going away. Raises at genuine judgment points
      are the design working. Two variants to spot there: a raise whose
      *payload* carries the data, and an `append_history` of raw
      content followed immediately by a raise. Both are self-addressed
      tool results.
- [ ] **Tune, and record what moved what.** Card, seed exemplar, raise
      pricing. That log is the most valuable thing this part produces.
- [ ] **Watch two failure shapes.** *Long and wrong*: longer programs
      that trap more or redo work are not a win, and recovery has to be
      seen working rather than assumed. *Self-reinforcement downward*:
      the model's prior programs are its few-shot examples, so a timid
      first program compounds — if early runs are short, add the worked
      exemplar pair (Step C4) and re-run.
- [ ] **Not built**: an A/B against the old chat path. It would require
      keeping that path wired into a harness to establish a baseline
      already well understood, and the effect it would measure — ten
      round trips versus one — is not subtle enough to need a control
      arm. The two rendering comparisons (plain text vs. a literal,
      role-delimited vs. one document) are deferred for the same
      reason: second-order, and worth doing only once the thing works.

---

## Part I — Deletions

Only after Parts B–H are green. The measure of this phase is what it
removes.

- [ ] The chat message list: `render_messages`, positional
      outcome pairing, `render_fork`'s dangling-call handling, the
      Assistant/Tool rendering.
- [ ] Tool schemas and the `run_program`/`resume`/`answer` tool
      definitions (`machine.rs`, `host/llm.rs`, `host/deepseek.rs`) —
      the *tool* definitions, not the program verbs that share two of
      those names.
- [ ] Just the harness-verb corner of the `tools.` namespace (Step
      C1): `tools.spawn`, `tools.ask`, and `tools.tool_result` become
      the bare globals `spawn`, `ask`, and `artifact`; `tools.agent`
      (today's combined spawn-then-ask) has no direct successor — the
      new vocabulary composes `spawn()` and `.ask()` in JS instead
      (`17_BRANCHES`'s own rule), so it is dropped rather than renamed.
      The namespace itself is not deleted — `tools.*` for configured
      capabilities (`read_file`, `bash`, …) stays for the life of this
      phase and past it.
- [ ] The artifact menu as a rendered, accumulating surface
      (`menu_rows` as context; the log remains the store).
- [ ] The answer budget and report clipping (`DEFAULT_ANSWER_BUDGET`,
      `WHAT_MAX_BYTES`, `PAYLOAD_MAX_BYTES`) — no subject left.
- [ ] The `Condition`-as-tool-result settlement protocol.
- [ ] Update `DESIGN.md` in the same commit as the deletion that
      invalidates each claim, not afterwards.

---

## Rejected: program forks (the deleted `20_PROGRAM_FORKS.md`)

Recorded because the reasoning is still instructive and the idea will
resurface.

That plan modelled a `run_program` dispatch as a `Fork`, so a program's
raise/resume interior grew on a child branch and only the `Return`
crossed back. Reusing `Fork` rather than inventing a `Program` event was
right, and its rejection of "the `run_program` event doubles as its own
root" correctly identified that `Tree::branch_of` climbs by payload type
and cannot answer "keep climbing" and "stop" to two callers of one
climb.

It was abandoned for four reasons, in increasing order of importance:

1. **Unconditional forking bought nothing measurable.** Only `Post`,
   `Turn`, and `Fork` render; `Call`/`Result`/`Console` never enter
   context. A program that never raises costs one Turn plus one tool
   message either way — identical with or without a fork. The savings
   existed only for raising programs.
2. **Four of its five parts were compensation for the first.** A
   navigator filter, an auto-follow rule, a message-addressing prompt,
   and artifact rediscovery all existed to repair side effects of the
   fork. By the project's own test — judge a design by what it removes
   — it was upside down.
3. **It kept two settlement models.** It moved events into a fork while
   keeping the protocol where a `Condition` *is* the dispatch's tool
   result. Every special case it needed — an origin flag in rendering,
   a `Return`-goes-to-the-parent carve-out, an episode-closing rule,
   "the mind's prose must land outside", fork-aware reconciliation —
   came from that seam. Closing the seam properly (a dispatch is an
   ordinary exchange, answered explicitly) was a larger change than the
   phase scoped.
4. **The problem it solved is not the problem.** Context growth is
   downstream of the model writing ten small programs where one large
   one belongs. Forking would have made ten small programs cheap
   instead of making them unnecessary.

Concretely, it also under-costed the work: `outcomes_of_turn` scans the
rendering leaf's own path, so moving the interior off-branch leaves the
parent's dispatch `tool_use` unanswered; `unmatched()` flags any Turn
with fewer outcomes than tool calls as `InterruptedRun`, which a live
fork trips on every open; `programs_for` folds along one path, so the
parent's program pane loses the run; and `handback` partitions the
artifact menu along the rendering path, so the completion report's menu
would have come back **empty** — a regression, not a feature to add
later.

Recorded for completeness: if code mode were ever abandoned, the
fallback is *not* this plan either. It is the much smaller thing the
fork was an elaborate way of achieving — fold a concluded program's
interior in `render_messages`, keeping the dispatch turn and replacing
its tool result with the final report. Same rendered bytes, one
rendering rule, no tree change.

## Open questions

- Whether the single transport (chat, thinking on, no prefill) leaves
  enough register on the table to be worth revisiting — the variants are
  documented in Part A and deliberately unbuilt. Program length is the
  number that would say so.
- **Superseded by Step B1:** nothing historic is live, so the "is
  `code()` callable?" question is gone. What goes with it is the cheap
  version of a **helper library** — the model appending reusable
  functions callable from later programs, so it accumulates its own
  vocabulary across a session instead of re-deriving it. That would
  compound directly on this phase's goal, and it is the one thing the
  plain-text format costs. The model can still copy a helper
  forward; whether a live region is worth reintroducing is open.
- Two rendering questions deferred in Part H rather than settled here:
  plain-text user turns vs. a JS literal, and the role-delimited layout
  (built) vs. one continuous document.
- How much the model appends per program, and whether compaction
  actually reclaims. This phase trades a *structural* leak for a
  *behavioural* one: everything in history is there because a mind chose
  it, so growth is bounded by the model's discipline. Measure it in
  Part H alongside program size.
- Whether the human fork gesture (`17_BRANCHES` Part D) still wants a
  `Fork` at all once branches exist only for minds, or whether it is
  better expressed as a new agent seeded with a projection of the
  current history.
- Whether streaming the source pane (Step G1) is enough of a progress
  signal on its own, or whether the card must also push an early
  `say()`. With no bare turns the user sees nothing between their
  message and the first `say()`, and a long program makes that gap
  long.

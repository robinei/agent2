# Phase 24 — narration arrives while the program is still being written

Small phase, one feature. It exists because of a gap found during
phase 23: **the card promised streaming narration and nothing
implemented it.**

## The gap

`//: ` was a narration convention — comment lines the model wrote,
which the card said "stream to whoever is watching as they are
written." Grep the harness and there is no extraction anywhere. So
`//:` narration reached nobody except someone reading raw JavaScript,
and the card was asserting something false about the model's own
environment.

Phase 23 deleted the convention and moved narration onto `tell()`,
which at least delivers — at execution time. This phase makes the
leading part of it deliver *during generation*, which is what a
responsive chat actually requires.

The requirement, stated plainly: **a user must never have to read the
generated JavaScript to know what is happening**, and messages must
not be postponed until a long program has finished streaming. Code
mode's whole thesis is fewer, longer programs, so the generation
window is exactly where a chat would otherwise go silent.

## The mechanism

As the completion streams:

1. Re-parse the accumulated prefix with `oxc_parser` — the parser
   `interp` already uses. It is error-recovering, so a truncated tail
   produces diagnostics that are simply ignored.
2. Walk the leading top-level statements. Take the run of
   `ExpressionStatement → CallExpression` whose callee is `tell` and
   whose sole argument is a string literal, or a template with no
   interpolation. Stop at the first statement that is not one.
3. Dispatch each such call that has not been dispatched yet: log the
   `Call::Send` and deliver, exactly as a running program would.
4. **Blank** each dispatched statement's span — overwrite those bytes
   with spaces — and compile the result as usual. The compiler never
   sees the call, so it cannot emit it twice.

That is the whole feature. It lives in `host/mod.rs`'s
`on_llm_response`, beside the fence-stripping that already happens
there.

## Why each piece is the way it is

- **No resumable parser.** The original framing of this idea wanted
  `interp` to answer "incomplete, need more." It does not need to:
  re-parsing the accumulated prefix from scratch on each chunk is
  quadratic over a few KB against a parser that runs at MB/s, which is
  not measurable. There is no "need more" protocol to design.
- **No hand-written scanner.** A second JS lexer living beside oxc's
  is precisely the duplication phase 23 spent itself deleting
  (`entry.rs`, `transport.rs`, `runner.rs` all died for it), and
  string literals — escapes, quote styles, templates — are exactly
  where it would drift.
- **Blank, never drop.** `Call::site` is a byte offset into the stored
  source, used to annotate a program per call site in reports. Slicing
  the prefix off shifts every later offset; overwriting with spaces of
  equal length keeps them identical, so `Turn.source` stays byte-exact
  what the model wrote.
- **No duplicate-suppression bookkeeping.** An earlier draft reached
  for reuse-by-id to reconcile a speculative send against the real
  run. Unnecessary: blanking means the call is only ever emitted once.
  And there is no recovery path to worry about either — **nothing in
  this system re-executes a stored program** (`DESIGN.md`, the
  dependency spine). A rewrite is a new completion, which streams like
  any other and gets the same treatment.
- **Literal arguments only, unconditional, top level, stop at the
  first non-`tell`.** Nothing has executed yet, so nothing computed is
  available to interpolate anyway — a `tell` inside an `if` cannot be
  dispatched because the condition has not run. This rule is
  statically decidable and keeps the dispatched set exactly equal to
  the set that certainly would have run.
- **Settle the extent before dispatching.** Only dispatch once the
  parser has seen the *next* statement begin, or the stream has ended.
  `tell("a")` looks complete until the next token turns out to be
  `.then(...)`. Costs nothing — narration blocks run several lines.

## Truncation

A truncated completion is never compiled (`Cause::Truncated`), but its
leading narration may already have been delivered. That is acceptable
and does not need undoing: a leading `tell()` states **intent**, and
intent is true at the moment it is uttered regardless of what the
program does next. The user does not care whether a program ran, or
failed on an execution error rather than a truncation; they care that
the chat kept up.

## Steps

**24.1** — Establish oxc's behaviour on a truncated tail. Specifically
whether `ParserReturn::panicked` can ever discard an otherwise-good
prefix, and whether per-chunk or per-newline is the better re-parse
trigger. Twenty minutes and a test, not a design question. Gate: a
test parsing a deliberately truncated program and asserting the
leading complete statements still come back.

**24.2** — The prefix scan: accumulated source in, list of
`(span, text)` for dispatchable leading `tell()` calls out. Pure, no
IO. Gate: unit tests for single/double quotes, escapes, a
non-interpolated template, an interpolated one (rejected), a `tell`
inside an `if` (rejected), a non-`tell` first statement (empty), and
an unterminated trailing statement (not yet dispatchable).

**24.3** — Wire into `on_llm_response`: dispatch, blank, compile.
Gate: a scripted session test asserting the `Call::Send` is logged
once, that `Turn.source` is byte-identical to what arrived, and that
every later `Call::site` still resolves to the right span.

**24.4** — Card pass. The verb guidance is already right from phase
23; check the exemplars still model the open-then-report rhythm now
that the opening `tell()` genuinely lands first.

## Not in scope

Rendering. The TUI is phase 23's Pass D, which is already told not to
assume a turn's messages arrive only at turn boundaries.

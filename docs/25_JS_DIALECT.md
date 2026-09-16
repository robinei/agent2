# Phase 25 — the dialect stops surprising the prior

A small phase with one organising idea, arrived at from six live runs
on 2026-09-16 and from an observation about `pi`: a coding agent whose
system prompt is little more than its tool list is still effective,
because bash/read/write/edit sits square in the middle of the training
distribution. Code mode does not. The model writes JavaScript out of
its prior, and every place this dialect answers differently is paid for
in one of two currencies.

**A loud divergence costs a round trip.** The program traps, the trap
is reported, the next program works around it. That is affordable, the
error message is the teacher, and it needs nothing in the card.

**A silent divergence costs the task.** The program gets an answer,
the answer is wrong, and nothing anywhere says so. `try17` reported
"removed 26, kept 3" with three of the removals wrong; `try12` reported
a clean sweep over a repo it had broken. Silence is the expensive
failure mode of code mode generally, and a silently divergent primitive
is the cheapest way to manufacture it.

**A capability that exists and is not advertised costs everything it
would have saved.** `Edit.*` — nine pure editing primitives built in
doc 10 Step F1, every box ticked — was never mentioned in the card that
phase 20 wrote from scratch. Six runs hand-rolled line-number splicing
against it: stale offsets that deleted a brace, a mid-codepoint trap, a
regex that rewrote `#[allow(dead_code)] // why` into `#[allow()] // why`.
`Edit.applyEdits` validates disjointness and applies right-to-left
precisely so offsets cannot go stale, which is the bug that broke
`try12` and the reason a bottom-up paragraph went into the card two
days later. The prose was teaching a discipline the runtime already
enforced.

So the rule this phase works by:

> Advertise what answers differently. Let what stops the program teach
> itself. Never leave something built and unmentioned.

## What already landed

- `b5e32e4` — `for (const [k, v] of aMap)` and `[...aMap]`. A `Map` is
  the obvious way to group work by key; iterating one trapped with
  "cannot read .length of a map", and the recovery attempt trapped too.
  Three of `try13`'s four programs went on it.
- `c4f0e6f` — `const out = []; out[0] = x` appends instead of erroring.
  Every index write past the end was rejected, which is right for
  `out[3]` on an empty array (it would leave a hole) but wrong for the
  most ordinary line in JavaScript, which leaves none.
- `3766428` — the `Edit.*` block in the card, the bottom-up paragraph
  deleted, and the probe-loop exemplar rewritten to manufacture no
  indices at all.
- `c4f0e6f` — the three silent answers below, as three card lines.
- `144ac79` — `replace_file(path, content, expected_version)`. Not a
  dialect issue but the same rule: two of three live runs put the
  content where the version went, so the signature was wrong.

## The audit, as it stands

Every builtin namespace in `interp/src/builtin/mod.rs` is standard
JavaScript except `Edit` and `Map.isMap`/`Set.isSet`. The array
higher-order methods, `Promise.all`/`allSettled`, async functions,
`try`/`catch`/`finally`, template literals, destructuring, regex,
`Map`/`Set` are all present and all reachable by the prior unprompted.
The `Edit` omission was a one-off, not a pattern — but nothing prevents
the next one, which is 25.5.

Loud and left alone, deliberately: strict function arity (a *compile*
error, so it costs nothing at runtime), labeled `break`/`continue`,
`Promise.race`/`any`, `new Promise`, BigInt, getters/setters, static
class members, array holes.

## 25.0 — `agent score <log…>` (landed)

The instrument the rest of this phase is judged by, and the thing that
stops each question being answered by a throwaway JSONL parser that
agrees with nothing. One pass over a finished log, sharing its
definitions with `eval::tasks::fold` — a test asserts the two agree on
the same tree, which is the guard against them drifting.

What it reports, and why each: `calls_per_program` (the thesis in one
number — a change that keeps tasks passing while this falls has given
the advantage back), `provider_ms` split out of `span_ms` (the same
task ran in 34s and 1195s on 2026-09-16; the second spent 99.6% of
itself waiting on the endpoint, so raw wall clock is unreadable),
`trap_messages` rather than a count (a trap is the dialect surprising
the model, and *which* surprise is the whole finding), `handovers`
separated from `raises` (`next_program` lowers to a raise, so a bare
count conflates opposite behaviours), and `silent` (a run whose
programs never reached a person).

It earned itself immediately: run over the week's logs it found a
second trap in `try18` that hand-reading had missed —
`localeCompare` was not implemented, so `sites.sort((a, b) =>
a.path.localeCompare(b.path))`, which is simply how a string sort is
written, trapped as a call to `undefined` and cost that run a program.
Added in the same commit: code-point order, no locale, since this
dialect has no locale data and inventing one would make the result
depend on something the program cannot see.

## 25.1 — String indexing

`"aéb".length` is 4, not 3: strings count UTF-8 bytes. Indexing
mid-codepoint is a hard error, which killed `try18`'s program while it
was inspecting comment spans on a file whose header contains an em
dash. Offsets a program *receives* (from `indexOf`, `match`, `split`)
are self-consistent in byte space and fine; offsets it *manufactures*
(`line[19]`, `slice(0, 80)`, `padStart(n)`, `.length` for column math)
are where it breaks. Surfacing `Edit.*` removes much of the reason to
manufacture them, so this is less urgent than it was — but `.length`
stays silently wrong.

Three options, costed:

- **(C) Stop trapping; clamp to the boundary.** One site,
  `vm/dispatch.rs:186`, plus the same decision in `slice`/`substring`,
  which already have `round_up`/`round_down` helpers at
  `builtin/string.rs:683-699`. About an hour. Removes the crash, keeps
  byte `.length`, so the silent part survives.
- **(B) Code-point semantics, UTF-8 storage.** `RcStr`'s `Header` is
  `{count: Cell<usize>, len: usize}` = 16 bytes and `MAX_STRING_LEN` is
  256 MiB, so `len` fits in `u32`: make it `{count, len: u32, char_len:
  u32}` — same 16 bytes, no memory cost. `.length` becomes O(1),
  `len == char_len` means pure ASCII so index math stays byte math and
  O(1), and only non-ASCII strings pay an O(n) walk. Surface is ~29
  string builtins plus `GetLength` and `IndexGet`; most are already
  boundary-aware. 1–2 days, gated by test262, and it should *raise* the
  conformance number. Residual divergence: astral chars count 1 where
  JS counts 2.
- **(A) True UTF-16 storage.** Breaks `RcStr`'s invariant ("exactly
  `header.len` bytes of valid UTF-8"), which `Borrow<str>`, `Hash`/`Eq`
  and `IndexMap<RcStr, _>::get("k")` all lean on, and every JSON and
  tool-result boundary starts converting. Multi-day, high risk, buys
  only exact emoji parity. **No.**

Recommendation: C now, B as a considered change.

Gate for C: `"é"[1]` returns `"é"` rather than erroring; a test asserts
it, and the test at `compiler/tests/lang_basics.rs:121` that currently
pins the error is rewritten rather than deleted, so the change is
visible in review. `cargo test -p interp`.

Gate for B: `cargo test -p interp`, `cargo run -p conformance` with the
expectations file updated in the same commit, and
`"aéb".length === 3`.

## 25.2 — Relational operators do not coerce

`1 < "2"` is `false`. `"10" > 9` is `false`. Every program here parses
numbers out of command output, and a number parsed out of stdout is a
string until something says `Number(x)` — so `if (count > 10)` against
an unconverted value silently takes the wrong branch, forever, with no
error anywhere. This is the most dangerous entry on the list: it is
both silent and near-certain to be hit.

Two ways to stop it being silent:

- **Card line** (done, `c4f0e6f`). Cheapest, and the weakest: prose
  lost to prose three times on 2026-09-16 alone.
- **Make it loud.** A mixed-type relational comparison becomes a
  `TypeError` rather than `false`. This diverges *further* from JS in
  order to diverge more safely, which is the trade this dialect already
  makes everywhere else — `Edit.*` is documented as "fail-loud pure
  editing helpers", and `ToPrimitive` is already refused rather than
  guessed at. Then the card line comes back out.

Open decision. Recommendation: make it loud.

Gate: `1 < "2"` raises `TypeError` naming both operand types;
`"a" < "b"` and `1 < 2` are untouched; `cargo test -p interp` and the
conformance expectations updated in the same commit.

## 25.3 — `==` against objects, and other quiet falses

`[5] == 5` is `false` (no `ToPrimitive`). `JSON.stringify(new Map())`
is `"[]"`, not `"{}"`. Both silent, both far less likely to be reached
than 25.2. Recorded, not scheduled — do them if 25.2 lands and the
same treatment is obviously right.

## 25.4 — `instanceof Error`

A caught runtime error materialises as a plain `{ name, message }`, so
`e instanceof Error` is `false` and `catch (e) { if (e instanceof
Error) … }` — which is ordinary defensive JavaScript — takes the wrong
branch silently. Currently a card line. Cheapest real fix is to give
those objects an `Error.prototype` link so `instanceof` walks to it;
`Object.getPrototypeOf` already returns real prototypes for every other
value, so the machinery exists.

Gate: `try { null.x } catch (e) { return e instanceof Error }` is
`true`, `e.name` is unchanged, and no new `Error` subclass machinery is
implied.

## 25.5 — Nothing built stays unmentioned

The standing fix for the `Edit` class of failure, and the most valuable
step here. A test in `agent/src/card.rs` that walks the builtin table
and fails when a **non-standard** namespace — one a model cannot know
from its prior — is absent from `CARD`. The allowlist of "standard, no
need to mention" is written down explicitly in the test, so adding a
new namespace forces a decision about advertising it, in review, at the
moment it is added.

Gate: delete the `Edit.*` block from the card and the test fails
naming `Edit`; restore it and it passes. `cargo test -p agent`.

## 25.6 — Measure it, do not argue it

None of the above should be judged by rereading the card. Each change
is a hypothesis about behaviour, and the scoring is the one agreed for
card work: **calls per program**, round trips, correctness against a
real checker, and wall clock separated from provider latency — the
`try18` run spent 98% of its 1195 seconds waiting on the endpoint, so
raw wall clock reads as noise.

Gate: before/after on the task set at n≥3, and a change that fixes one
task while costing another is not an improvement.

## Not in scope

Card *prose*. This phase adds no engineering-hygiene guidance; the
lesson of 2026-09-16 is that every such paragraph got obeyed literally
and wrongly until an exemplar showed it in place, and that the card
grew 36% in a session under purely additive pressure. Card changes here
are a capability block, three silent-answer lines, and one deletion.

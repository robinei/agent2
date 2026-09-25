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

## 25.4 — `instanceof Error` — **done, 2026-09-25**

A caught runtime error materialised as a plain `{ name, message }`, so
`e instanceof Error` was `false` and `catch (e) { if (e instanceof
Error) … }` — which is ordinary defensive JavaScript — took the wrong
branch silently. `` `${e}` `` was `[object Object]`, which is the
second half of the same silence: the log line that should carry the
message carried nothing.

The fix is the one this entry proposed: a `TypeTag::Error` row, so
`Error.prototype` exists and is what every error links to. One VM
helper (`VM::alloc_error`) builds all of them — the errors the VM
raises, `new Error(…)`/`TypeError(…)` (the registry's constructor
handlers), and the harness's own tool errors — so no source can drift
from the rest.
String coercion recognises an error *by that prototype link*, never by
having a `name` and a `message`: `{ name: "x", message: "y" }` is still
`[object Object]`, because a tool result with those two fields is a
record, not an error.

Gate, met: `try { null.x } catch (e) { return e instanceof Error }` is
`true`; `e.name` and `Object.keys(e)` are unchanged; no `Error`
subclass machinery was added. test262 moved 14 tests Fail → Pass (6 of
them `Error/prototype/toString`) and none the other way.

## 25.4b — the error *classes* — **done, 2026-09-25**

25.4 left one prototype for every name, so `e instanceof TypeError` was
`false` — including of a `TypeError`. A program that wanted to treat a
bad value differently from a bad type had `e.name` and nothing else,
and the one thing `instanceof` could tell it was that an error was an
error.

Each name is now its own class: a `TypeTag`, a constructor row, and a
prototype chaining to `Error.prototype`, which chains to
`Object.prototype`. That two-rung chain is the hierarchy — a type error
is `instanceof TypeError` and `instanceof Error` both, and `instanceof
RangeError` not at all. `TypeTag::ERRORS` is the one list; the registry
rows and `VM::alloc_error`'s name → class mapping both read it, so a
name cannot get a global without also getting the prototype that
answers for it.

**Three of the ten have no producer in this runtime, and that is the
point.** `URIError` needs an `encodeURI`/`decodeURI` there is none of;
`AggregateError` needs `Promise.any`, which is a compile error;
`SuppressedError` needs `using`/`DisposableStack`, which do not exist.
They are declared for the *program's* own `throw` — a model writing
`throw new URIError(…)` in a decoder it wrote, or catching one from
pasted library code, should get a class rather than a `ReferenceError`
about the name. Their constructors are spec-shaped for the same reason:
`new AggregateError(errors, message)` materializes `.errors` as a real
array (through the same `iterable_elements` that serves `new Set(x)`),
and `new SuppressedError(error, suppressed, message)` sets both halves
of the pair, because the code that catches one of these is the code
that reads those fields.

**`ValueError` is in the set, and is not a JS name.** It is this
dialect's own kind for "right type, impossible value", one of the five
kinds a program can actually catch — `fail()` raises `TypeError`,
`ValueError`, `RangeError`, `SyntaxError` and `ReferenceError`
resumably; everything else ends the program. Omitting it would have left
an error a program can catch with no class to test for, which is the
opposite of the point. It is a deliberate dialect addition, with a
global constructor like the rest.

**But it was doing four jobs, and two of them have JS names.** Of its
~145 raise sites, 36 moved: every out-of-range *magnitude* — a negative
index, an array write past `length`, a shift amount outside `[0, 63]`, a
radix outside `[2, 36]`, a `toFixed` precision, a `repeat` count, a code
point, a string length, every typed-array `byteOffset`/`length` bound —
is a `RangeError`, and `JSON.parse` failures and malformed regexps are
`SyntaxError`. The first of those is not a judgement call: the spec
*names* a `JSON.parse` failure a `SyntaxError`, so a program catching
one by the book caught nothing. The name is what a program branches on,
and the repair differs — `catch (e) { if (e instanceof RangeError)
shrinkTheSlice() }` is not what a malformed document calls for.

What kept the name is what the name was for: a value of the right type
and the right size that this dialect still cannot use — a closure or a
`RegExp` handed to `JSON.stringify`, a structure nested past the
serializer's depth limit, the `edit` builtins' ambiguity failures.
Corrupt-pointer checks kept it too, and should not have; see §25.4c.

`ToolError` deliberately gets no class. It is the harness's concept
(`agent/src/machine.rs`), and `interp` should not learn the name of
something the harness invented; it stays `instanceof Error` and
branchable on `e.name`, which is what `alloc_error`'s default arm is
for.

Gate, met: each source is `instanceof` its own class and `Error`; a
`TypeError` is not a `RangeError`; `` `${e}` `` is still `name:
message`; `{ name, message }` is still `[object Object]`;
`Object.keys(err)` is still `["name","message"]`; every class prototype
is empty.

test262 moved **996 Fail → Pass** and **6 Pass → Fail** (8388 → 9378
pass). The 996 are `assert.throws(TypeError, …)` and its siblings:
`assert.throws` compares `thrown.constructor !== expectedErrorConstructor`
by *identity*, so while every error's `.constructor` was `Error` it
matched only `assert.throws(Error, …)`. The 6 are the same identity
check losing a false pass — each is a test for a feature this dialect
does not have (`Array.prototype.forEach.call`,
`Map.prototype.getOrInsertComputed`, `Number.prototype.toLocaleString`),
which raises `TypeError: cannot call a undefined as a function`, and
which `assert.throws(Error, …)` used to accept because *every* error's
constructor was `Error`. They passed while testing nothing; they now
fail for the reason they should always have failed.

The card's second dialect row is gone with this — there is no
divergence left to warn about — leaving one (`1 < "2"`).

## 25.4c — catchable and resumable are different questions — **done, 2026-09-25**

```js
let x = {};
try { x--; } catch (e) { /* never ran */ }   // died with UNCAUGHT TypeError
```

Two characters away from the error, a handler that did not run. The
cause was one enum answering two questions.

`ResumeMode` documented itself as being about stack hygiene:
`PushValueThenContinue` meant "the failed instruction's operands were
consumed, so pushing a replacement result and advancing ip resumes as if
it succeeded". That is the host's `raise`/`resume` concern and nothing
else. But `VM::step` reused the same flag as the gate for
`unwind_to_handler` — that is, for whether a JS `catch` ever sees the
error at all.

`IncLocal` (`x++`, `x--`) reads its local by *peek*. Nothing is
consumed, so there is genuinely no slot for a substituted value, so it
was `NotResumable` — and therefore uncatchable. The stack reason is
true; the catchability conclusion does not follow from it.
`unwind_to_handler` runs `self.stack.truncate(h.stack_len)`: it resets
the stack to the `try`'s own baseline, so **what the failed instruction
did or did not pop has no bearing on catching**. The justification did
not survive contact with the code.

The enum was already two things in one variant, as its own audit table
admitted — "no result slot" (`IncLocal`) beside "invariant violation"
(a dangling heap pointer). It is three now:

| | host may substitute a value? | JS `catch` sees it? |
|---|---|---|
| `Resumable` | yes | yes |
| `NoResultSlot` | no | **yes** |
| `InvariantViolation` | no | no |

Resumable is `mode == Resumable`; catchable is `mode !=
InvariantViolation`. Two predicates over one enum, each asked by exactly
one caller: `resume_with` and `step`.

`NoResultSlot` has two tenants: `IncLocal`, and an uncaught `Throw`
(which owes a statement no value). The second is nominally catchable and
never actually caught — it is only *constructed* after the handler
search has already failed, so the same search in `step` fails again and
it escalates unchanged.

`InvariantViolation` holds what a program must never swallow: a
dangling heap pointer, a stack underflow, bytecode the compiler should
not have emitted, and `Deadlock`. The first three are the VM being
broken, and a `catch` that hid one would turn a debuggable crash into a
wrong answer. `Deadlock` is not a broken invariant — nothing is wrong —
but it shares the one rule, because every strand is parked and there is
nothing to carry on with.

The corrupt-pointer checks §25.4b left behind moved here: 37 sites that
said `vm.fail(ErrorKind::ValueError, "bad object pointer")` and were
therefore resumable *and* catchable now go through `fail_invariant`.
Their `ErrorKind` is unchanged and no longer means anything — an
`InvariantViolation` never becomes a JS value, so no program reads the
name.

Gate, met: `try { x--; } catch (e) { … }` catches, and reports
`e.name === "TypeError"` with the message intact; the same bytecode that
reads a dangling object pointer still escalates from inside a `try`.

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

The suite, as built: `dead-code-sweep` (probe loop, Rust/cargo) and
`skipped-tests` (probe loop, Python/unittest) are the same shape on
surfaces with nothing in common, so a sentence fitted to one is visibly
worthless on the other; `plain-question` fails a run that reaches for
machinery a question does not need, which is the ditch every other task
and every card sentence pushes toward; `ambiguous-config` fails both
guessing past an ambiguity and reporting it and stopping.

The variants to measure, in `evals/cards/`: `minimal` (5.6KB of 17.3 —
the response format, the verbs, `Edit.*`, the three silent divergences,
and the fact that a program ends when it ends; every line of
engineering guidance removed) and `no-exemplars` (the full card,
nothing opening `messages`). Between them they answer what the prose is
worth and what the examples are worth, and the axis to read is **calls
per program**, not pass rate: a minimal card will very likely still
complete tasks, at two or three calls per program, which is a tool loop
wearing JavaScript and gives the whole advantage back.

## 25.6a — The first measurement: thinking off is not a shortcut

The question that opened this (2026-09-16: "could thinking=off even be
potentially a good approach?") has an answer now, on four tasks at n=3
with the checkers held fixed:

                    off      on
  ambiguous-config  0/3  ->  3/3
  dead-code-sweep   0/3  ->  1/1   (2 cut off at the cap)
  plain-question    3/3  ->  3/3
  skipped-tests     0/2  ->  1/2   (1 cut off)

  calls/program     2.5  ->  23.0  (dead-code-sweep)
                    4.8  ->  10.8  (skipped-tests)
  handovers         1.5  ->  0.5   (skipped-tests)

Three of eleven becomes eight of nine. But the number that matters for
the thesis is the second block: **calls per program rises and handovers
fall**. Thinking off is not "the same work, cheaper" — it produces the
transcript shape, small programs that hand themselves a to-do list,
which is the tool loop this architecture exists to escape. Thinking on
produces the program shape: one program doing twenty-three calls'
worth of work.

`plain-question` is identical at 3/3 either way, which is the control
doing its job — the task with no work in it is not where effort helps.

The cost is wall clock per run: 300-420s against 6-40s, and three of
twelve runs hit the 420s cap. That is a real trade, and it is the trade
the suite exists to price.

## 25.6b — The pi comparison, and what it costs to think up front

`dead-code-sweep`, n=3 a side, same model
(`opencode-go/deepseek-v4-flash`), same fixture, same confinement, and
the same checker judging both — `evals/pi_score.py` folds a pi session
into the shape `agent score` produces precisely so that correctness,
the measurement most vulnerable to being eyeballed, is decided by
identical code.

  pi (tool loop)
     8 trips   8 calls   in 25,087 (21,504 cached)   out  2,205    19.9s  PASS
    10 trips  15 calls   in 47,537 (34,560 cached)   out  4,825    56.5s  PASS
     9 trips  13 calls   in 33,347 (29,440 cached)   out  2,516   121.8s  PASS

  us (code mode)
     3 programs 40 calls in 32,166 (22,912 cached)   out 65,212   311.0s  FAIL
     1 program  36 calls in  8,065 ( 7,808 cached)   out 37,068   349.6s  FAIL
     2 programs 23 calls in 20,081 (15,872 cached)   out 60,059   546.2s  cut off

  median          trips  9 -> 2      in 33,347 -> 20,081
                  out 2,516 -> 60,059      56.5s -> 349.6s      3/3 -> 0/2

**The mechanism works and the trade is bad.** Round trips fall 4.5x and
input tokens fall 40%, exactly as claimed — but less than "one program
instead of nine" suggests, because 86% of what a tool loop re-sends
comes back from the prefix cache and costs almost nothing. Meanwhile
output tokens rise **24x**, and output is the expensive half in both
money and latency, being generated serially.

Almost all of that is reasoning, not program: 206KB of thinking against
10-27KB of source, versus pi's 5.8KB. So the honest statement of what
code mode does is not "fewer round trips" — it is **deliberation moved
from per-step and informed by results, to all up front and
uninformed**. A tool loop reasons about the next move with the last
result in hand; a program has to anticipate every branch it might meet,
including the ones it never reaches.

That reframes the ablation waiting behind this. The question is no
longer "how much of the card earns its keep for correctness" but
**"how much of the card is causing the reasoning blowup"** — every
paragraph about running a control, proving the check can fail, guarding
a stale offset and handling a broken baseline is another branch the
model must reason about before writing a line. `plain-question` shows
the floor is fine at 0.0 calls and 0.6KB of thinking, so the cost is
specific to planning a whole task in advance.

Caveats worth keeping attached to these numbers: our card is one day
old and pi is a mature product; one task; and three of our six runs
across both baselines were cut off by a provider that also gave pi a
122-second run and a no-completion. The direction of the gap is far
larger than that noise, but a second task — `skipped-tests`, the same
shape on another surface — is what would separate "this task" from
"this design".

## 25.6c — Three kinds of trap, and why none are discounted

A failing run that trapped is not automatically a verdict on the agent,
so `drive.py` classifies the trap *message* — once, in a reviewable
table — rather than deciding per run, which is where a disappointing
result would otherwise get reclassified into something comfortable:

  gap      standard JavaScript this dialect does not implement. Ours to
           fix, and a cost a tool loop never pays, because `bash` and
           `read` have no dialect to be unfaithful to.
  program  the model's own bug — a wrong argument type, a regex that
           matches nothing it meant. A verdict on the agent.
  guard    a primitive refusing what it was built to refuse.
           `Edit.replaceOnce` declining an ambiguous needle is the
           design working, and counting it as a defect would penalise
           the safety it exists to provide.

**Reported beside the pass count, never subtracted from it.** Two
reasons. A gap's cost is not only the failure but the recovery — a trap
buys a round trip and a re-plan, which inflates reasoning and latency
whether or not the run goes on to pass, so discounting the failure
while keeping the inflated cost columns would be inconsistent in our
own favour. And the class is open-ended: JavaScript is enormous, the
tail is never finished, and that asymmetry against a tool loop is
structural rather than incidental. Hiding it would flatter this design
in precisely the dimension where it is weakest.

The deflating measurement that motivated writing it down: swept across
every kept run of all five card variants, the whole suite contains
exactly **one** genuine fidelity gap — `cannot read property 'catch' on
promise` — in one run. Everything else is a program error or a guard
working. The gaps that cost whole runs on 2026-09-16 (byte-indexed
strings, `Map` iteration, `localeCompare`) all happened on the real
repository, not here. So discounting would move today's numbers by
nothing, and **the 13x reasoning gap to pi is not explained by our
interpreter's incompleteness** — worth knowing before spending on
fidelity work in the expectation that it closes the gap.

## 25.7 — What the suite still cannot ask

**Nothing answers an `ask()`.** The internal eval module had one fixed
non-answer for this (`NO_SCRIPTED_ANSWER`, and its own doc explains why
a *cooperative* simulated user is worse than none: it hands the agent a
clean answer to every ambiguity it invents and flatters it into
passing). The external driver has nothing — a run that asks ends with
the question outstanding. That is the correct end state for
`ambiguous-config`, which is why it can be checked today, but it rules
out every multi-turn task, and multi-turn continuity is a whole
mechanism the suite currently cannot reach. The original plan's answer
is an LLM-as-user flag driving the real TUI; the smaller step is a
`--reply` path on `agent session` so the driver can discharge a pending
`ask` the way the in-process harness does.

**A verified checker can still check the wrong thing.** The first
`ambiguous-config` checker required exactly one program, and failed a
run that read the file in one, handed the text to a second, recognised
the ambiguity there and asked exactly the right question — the handover
the card endorses, for the reason it endorses it. The fixture
verification passed throughout, because the fixtures were written to
match the same belief the checker encoded. Verification catches a
checker that cannot fail; it cannot catch one that fails the right
answer. The only thing that caught it was reading a failing run, which
is what `--keep` is for.

## Not in scope

Card *prose*. This phase adds no engineering-hygiene guidance; the
lesson of 2026-09-16 is that every such paragraph got obeyed literally
and wrongly until an exemplar showed it in place, and that the card
grew 36% in a session under purely additive pressure. Card changes here
are a capability block, three silent-answer lines, and one deletion.

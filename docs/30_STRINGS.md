# Phase 30 — strings stop being bytes

Read `25_JS_DIALECT.md` first. Its organising idea is the whole case for
this file: *a loud divergence costs a round trip, a silent divergence
costs the task*. The byte-string model was chosen as a loud divergence —
`.length` counts bytes, indexing inside a character throws — and it has
been quietly manufacturing silent ones.

This file contradicts one bullet of `interp/src/vm/mod.rs`'s module doc
("Strings are UTF-8 byte sequences… That is a deliberate choice and it
stays") and the corresponding row of `agent/card/card.md`. Per
`DESIGN.md`'s rule, that is a design change, and it is the point.

---

## The problem

### What was already known

Three bugs in one file on 2026-09-24, all found by reading a live
session rather than by a test, all recorded in
`interp/tests/string_is_walked_by_character.rs`:

- `for (const ch of s)` lowered to `c[i]` while `i < c.length` and
  trapped on the first byte past ASCII. Fixed by making `IterSource`
  yield characters (`interp/src/builtin/map.rs`, dispatched from
  `interp/src/compiler/control_flow.rs:459`).
- `s.charAt(i)` and `s.at(i)` returned `byte as char`. `"—b"[0]` was
  `"—"` and `"—b".charAt(0)` was `"â"` — `s[0] === s.charAt(0)` false at
  a valid boundary, and the returned character was not in the string.
  Fixed in `interp/src/builtin/string.rs`.
- `s[i]` at a mid-codepoint offset throws; `"aéb".length` is 4 here and
  3 in JS.

The first two are the loud kind, caught and fixed. The third is the
contract. So far, so deliberate.

### What is actually there

Everything below was measured on today's tree. None of it is in the
test file, and none of it is loud.

| you write | you get | JS gives |
|---|---|---|
| `"aéb".slice(0, 2)` | `"a"` | `"aé"` |
| `"a".padStart(3, "💩")` | a string of **length 9** | `"\uD83D\uDCA9a"`, length 3 |
| `"😀".padStart(4, "-")` | `"😀"` — no padding at all | `"--😀"` |
| `String.fromCharCode(0xD83D, 0xDE00)` | `""` | `"😀"` |
| `"\uD800".length` | `7` | `1` |
| `"\uD800".split("")` | `["\uFFFD","d","8","0","0"]` | `["\uD800"]` |
| `"a😀b".indexOf("b")` | `5` | `3` |
| `"a😀b".match(/b/).index` | `5` | `3` |
| `/./u.exec("😀")[0].length` | `4` | `2` |
| `"😀".charCodeAt(0)` | `TypeError: cannot call a undefined` | `55357` |

Four of these deserve naming individually.

**`slice` and `substring` silently return the wrong string.**
`extract_substring` (`interp/src/builtin/string.rs:754`) clamps `start`
*up* to a boundary and `end` *down*, then checks `start_clamped < start
|| end_clamped > end` before erroring. `clamp_start` never returns less
than its argument except past the end of the string, and `clamp_end`
never returns more. **That error branch is dead for every in-range
index.** The documented "mid-codepoint is an error" contract does not
hold for the end index: `"aéb".slice(0, 2)` quietly returns one
character where two were asked for. This is the exact shape
`25_JS_DIALECT.md` says costs the task.

**`padStart`/`padEnd` violate their own definition.** They measure the
target length in bytes and append the pad in whole characters, so
`"a".padStart(3, "💩")` returns a string whose `.length` — by this
dialect's own byte rule — is 9, not 3. In the other direction,
`"😀".padStart(4, "-")` pads nothing, because the string is already
"4 long". There is no reading of the contract under which either answer
is right.

**`String.fromCharCode` cannot produce a non-BMP character.**
`str_from_char_code` (`interp/src/builtin/string.rs:695`) masks each
argument to 16 bits and calls `char::from_u32`, which returns `None` for
a surrogate, and the `if let Some(c)` then *drops it silently*. Every
surrogate pair — the only way `fromCharCode` can express an astral
character — produces the empty string. A program that builds text from
code units gets nothing and no error.

**Source literals are mangled before we see them.** `"\uD800"` compiles
to 7 bytes: U+FFFD followed by the literal characters `d800`. This is
not `RcStr`; it is `oxc`'s `StringLiteral.value`, an `Atom` backed by
`&str`, which structurally cannot hold a lone surrogate and does not say
so. `interp/src/compiler/expr.rs:22` reads that field and interns it.
**This is independent of any representation choice and must be fixed
separately** — see Stage 5, and see the honesty check at the end of the
recommendation.

**And `charCodeAt`, `codePointAt`, `normalize`, `isWellFormed`,
`toWellFormed` do not exist at all.** There is no way in this dialect to
ask what code unit is at position `i`. The registry
(`interp/src/builtin/mod.rs:499-542`) has 24 string methods and none of
them is one of those. A model that reaches for the JS spelling of "read
one unit of text" gets `cannot call a undefined as a function`.

### The diagnosis

These are not five bugs. They are one bug five times: **every string
operation has to decide, locally, what a "position" is, and they decide
differently.** `length` says bytes. `padStart` says bytes for the target
and characters for the pad. `slice` says bytes but rounds. `split("")`
says characters. `for…of` says characters (since today). `charAt` said
raw bytes until this morning. Regex match indices say bytes.
`fromCharCode` says UTF-16 code units and then throws them away.

The representation does not force this — you could make byte offsets
consistent everywhere. But nothing *checks* it, the unit is invisible in
a `usize`, and the surrounding language (every JS the model has ever
seen) uses a different one. So the drift is not a series of accidents;
it is the steady state. Three were found this morning by reading one
log; four more were found this afternoon by asking seven questions.

**That is the case for changing the representation: not that UTF-16 is
better in the abstract, but that a `usize` which means "UTF-16 code
unit" matches every reference a maintainer or a model will consult,
while a `usize` which means "UTF-8 byte" matches none of them and is
checked by nothing.**

---

## Options, briefly

The representation question is settled (`u16` code units — see the next
section). This section exists so the reader can see what was traded
away, not to reopen it.

| | `Vec<u16>`-style (chosen) | Two-representation | WTF-8 | UTF-8 + correct semantics |
|---|---|---|---|---|
| `Value` size | 16 bytes, unchanged | 16 bytes, unchanged | 16 bytes | 16 bytes |
| `Borrow<str>` | **impossible** | impossible on the wide arm | impossible (WTF-8 ⊅ UTF-8) | kept, free |
| JSON boundary | transcode both ways | transcode on the wide arm only | near-free (ASCII/BMP identical) | free |
| Memory, ASCII | ×2 | ×1 | ×1 | ×1 |
| Lone surrogates | yes | yes | yes | **no** |
| Index O(1) | yes | yes | no — needs a cache | no — needs a cache or an ASCII bit |
| Work | large | very large | large + a bespoke encoder | small |

**Two-representation (Latin-1 + UTF-16, V8's `SeqOneByteString` /
`SeqTwoByteString`).** The correct answer for a production engine and
the only one that does not pay ×2 on ASCII. It costs a tag bit in the
header, a branch in *every* accessor, and two code paths through all
~40 string builtins instead of one — the single biggest multiplier on a
change that is already the largest in the crate. It is also strictly
addable later: nothing in the chosen design forecloses it, because the
external API is `as_units() -> &[u16]` and a Latin-1 arm would only
change what is behind it (at the cost of that method becoming
`Cow`-shaped). Deferred, deliberately.

**WTF-8.** Gets lone surrogates *and* keeps ASCII at one byte, which
makes it look like the dominant option. It loses on the thing the whole
change is for: an index is still a byte offset, so `length` still is not
JS's `length`, and every operation still needs a cache or a scan. You
would do all of the migration work and keep the papercut. It also means
hand-writing an encoder/decoder and losing `str`'s standard library.

**UTF-8 with an `is_ascii` bit and code-unit-correct semantics.** The
cheap answer, and it must be said plainly what it would have bought:
*every single entry in the table above except the last three*.
`"aéb".length === 3`, `slice` correct, `padStart` correct, `indexOf`
correct, regex indices correct, `charCodeAt` implementable — all of it,
with `Borrow<str>` intact, no transcode at the JSON boundary, no
doubling, and perhaps a fifth of the work. What it cannot do is hold
`"\uD800"`, and it pays an O(n) scan (or a cached index) for a
non-ASCII string's `s[i]`.

The counter-argument, which is the decision: the ASCII fast path is
exact but it is a *fast path*, and the slow path would be written once
and then be wrong forever after, because nothing in this repo's normal
traffic exercises it. That is the same failure mode as a guard that
passes while covering 4% of the file it names (`39b5559`, `736093d`) —
green because nothing ran, not because nothing was wrong. A representation whose
correct behaviour is on the path nobody runs is not a fix; it is the
current situation with a longer fuse.

---

## The representation

`interp/src/rc_str.rs` keeps its shape. One heap block, `[ Header |
units… ]`, reached through one `NonNull`. What changes is the element
type.

```rust
#[repr(C)]
struct Header {
    count: Cell<usize>,
    len: usize,       // now CODE UNITS, high bit = all units < 0x80
}
```

- `HEADER_SIZE` is 16 (two `usize`), `HEADER_ALIGN` is 8. `[u16]` needs
  align 2 ≤ 8, and 16 is a multiple of 2, so the units still sit at a
  fixed padding-free offset. The layout argument in the module doc
  survives verbatim; only the constant in it changes.
- Allocation size is `HEADER_SIZE + len * 2`. Today's comment ("cannot
  overflow for any real string") still holds, but `layout_for` should
  stop doing the arithmetic by hand and use
  `Layout::new::<Header>().extend(Layout::array::<u16>(len)?)?` — which
  checks, and which is also where the trailing-padding question gets
  answered by the standard library rather than by a comment.
- **Thin pointer, 16-byte `Value`, one allocation, one indirection,
  non-atomic refcount: all preserved**, because every one of them lives
  in the header or the pointer, not in the element type.
- `Value::String`'s clone stays a refcount bump. `Eq`'s pointer
  short-circuit stays O(1).

**The `is_ascii` bit.** Steal the high bit of `len` (a string of 2^63
code units is not a thing). It costs nothing — construction already
walks every unit — and it pays for itself at the JSON boundary, where
measurement (below) shows the all-ASCII scan is 22.6 µs of the 52 µs
cost of narrowing 100 KB back to UTF-8. Do not add a third header field:
that takes the header from 16 to 24 bytes and makes every short string
worse.

**`ThinVec` was checked and rejected.** It is already a dependency and
it is thin, so it looks like it should just work. It has no refcount —
zero hits for `Rc` or `AtomicUsize` in the crate — and its `Clone` does
`with_capacity(len)` plus an element-by-element copy. `vm/value.rs:13`
says exactly why that is fatal: *"the `String` variant owns an `RcStr`
whose clone must bump a refcount"*. Every `Value` clone — stack push,
local read, argument pass, property read — would become an allocation
and an O(n) copy at double width, and the pointer-identity `Eq` would go
O(n) with it. The useful part of the negative result: `ThinVec` does
prove the `[ Header | data ]` layout carries a `u16` payload without
trouble. It is the missing header field, not the layout, that rules it
out.

### The safe `&[u16]` view

Provide **both**, in this order:

```rust
impl RcStr { pub fn as_units(&self) -> &[u16] { … } }   // from Stage 2
impl Deref for RcStr { type Target = [u16]; … }          // from Stage 3
```

`as_units()` is the primitive and it is what the call sites should read
as — `s.as_units()[i]` says "this index is a code unit" at the point of
use, which is the whole disease being treated. `Deref` is pure
ergonomics: it makes `s.len()`, `s.iter()`, `s.is_empty()`,
`s.starts_with(…)` and `&s[a..b]` work.

**Add `Deref` late, not early, and that is a migration decision rather
than a taste one.** With `Deref<Target = [u16]>` in place from the
start, `s.len()` compiles at every existing call site and silently
changes meaning at all of them. Without it, every one of those sites is
a compile error that a human has to look at. The compiler is the only
tool that can enumerate them; do not disarm it and then go looking by
hand. Concretely: the probe below found **186 errors in `interp` (lib)
and 237 including unit tests** when the `str` views were removed. That
list is the work item, and it should be generated, not guessed.

Neither view is `unsafe` at the call site. `as_units` is one
`slice::from_raw_parts` inside `rc_str.rs`, exactly like `as_str` today,
minus the `from_utf8_unchecked` — so the change *removes* one unsafe
invariant rather than adding one.

### The name

Rename `RcStr` to **`JsString`**. It will no longer hold a `str` and
this repo has just paid for a name that did not mean what it said. The
role is unchanged, so the rename is cosmetic; the cost is 296 mentions
across 34 files, one `sed`, zero semantic risk. Do it **in the same
commit as the payload swap** so there is no window in which the name
lies. `FieldName` (`vm/instr.rs:34`) is already an alias and needs no
change.

---

## `Borrow<str>`, and what replaces it

This is the section that decides whether the change works.

The module doc calls the `Borrow<str>` + `Hash`/`Eq` contract
load-bearing for `IndexMap<RcStr, _>::get("k")` — "object property
lookup by `&str` literal, used everywhere". **Measured, it is used in 22
places in `interp/src` outside tests, and they split into two kinds that
need two different answers.** Three more are in `rc_str.rs`'s own test.
Nothing in `agent/` uses it at all.

The measurement: delete `Deref`, `Borrow<str>`, `AsRef<str>` and rename
`as_str`, then `cargo check -p interp --all-targets --message-format=short`
and bucket the diagnostics. 25 `E0277` (the `Borrow` sites), 199
`E0599` (`as_str`/`Deref` method calls), 11 `E0308`, 2 others.

### Kind A — a `&str` literal as the key (11 sites, 7 distinct literals)

`vm/methods.rs:328` (`"name"`, `"message"`), `vm/dispatch.rs:2515`,
`:2524`, `:2621` (`"length"`), `:2696` (`"size"`), `:2962`, `:2968`
(`"message"`), `builtin/array.rs:61` (`"length"`),
`builtin/edit.rs:870`, `:883` (`"old"`, `"new"`).

**Answer: `Borrow<[u16]>` plus a compile-time `wide!` macro.** Verified
working on stable, allocation-free, O(1), with hashes that agree:

```rust
pub const fn ascii_wide<const N: usize>(s: &str) -> [u16; N] { … }

macro_rules! wide {
    ($s:literal) => {{
        const W: [u16; $s.len()] = $crate::rc_str::ascii_wide($s);
        &W as &'static [u16]
    }};
}
```

`ascii_wide` asserts each byte is `< 0x80` at *const-eval* time, so a
non-ASCII literal is a compile error rather than a wrong lookup. The
site reads `obj.map.get(wide!("length"))`. The hash agreement is the
part that has to be right and was checked directly: `impl Hash for
RcStr` delegates to `self.as_units()`, `<[u16] as Hash>` writes a length
prefix then the elements, and the two hashers produce identical output
for the same content. This is the same contract as today, one type
lower.

### Kind B — a `field: &str` parameter threaded from an instruction (11 sites)

`vm/dispatch.rs:330`, `:683`, `:715`, `:716`, `:725`, `:775`, `:776`,
`:785`, `:912`, `:917`; `vm/methods.rs:1267`. All of them are fed by six
signatures:

- `dispatch.rs:279  named_get_property(&mut self, receiver: &Value, field: &str)`
- `dispatch.rs:473  type_proto_lookup(…, field: &str, …)`
- `dispatch.rs:489  typed_array_virtual(&self, field: &str, …)`
- `dispatch.rs:508  data_view_virtual(&self, field: &str, …)`
- `dispatch.rs:666  named_set_property(…, field: &str, …)`
- `dispatch.rs:907  own_prop_contains(&self, value: &Value, key: &str)`
- `methods.rs:1259  resolve_proto_chain(&self, obj_ptr: ObjectPtr, field: &str)`

**Answer: change the signatures to `&RcStr`, and 11 of the 22 sites stop
needing any `Borrow` impl at all.** This is not a workaround, it is a
simplification that was available before and is forced now. Look at the
call sites:

```
dispatch.rs:272        self.named_get_property(receiver, field.as_str())
builtin/object.rs:188  vm.own_prop_contains(&recv, key.as_str())
builtin/object.rs:204  vm.own_prop_contains(&recv, key.as_str())
```

The caller already holds an `RcStr` — `Instr::ObjGet(FieldName)`'s
operand, or a coerced `Value::String` — and downgrades it to `&str` so
that the callee can upgrade it back through `Borrow`. Removing the
round trip removes three `.as_str()` calls as well as eleven `Borrow`
uses, and makes the type of a property key one thing throughout
dispatch.

**The cost of Kind B** is the five `match field { … }` blocks inside
those functions (`dispatch.rs:290`, `:310`, `:340`, `:495`, `:514`,
≈24 literal arms total, plus `methods.rs:1647` and `dispatch.rs:28`).
You cannot `match` a `&[u16]` against string patterns. They become
`if`-chains against `wide!` constants. This sounds worse than it is:
rustc already lowers a `match` on `&str` to a length-dispatched chain of
`memcmp`s, so the machine code is the same class and only the source
gets uglier. A small `match_wide!` macro keeps it legible; an
`IndexMap<&'static [u16], Virtual>` built once is the alternative if the
chains get long.

### Routes considered and not taken

**Interning property names into a `Symbol(u32)`.** Strictly better at
the point of lookup (compare two `u32`s) and it is where this ends up if
property access ever becomes hot. It is a separate change with a
separate justification — an intern table, a lifetime for it, a story for
runtime-computed keys — and bundling it here would mean two large risky
changes landing as one. No.

**A cached hash that matches across encodings.** Would let an `RcStr`
answer to both `&str` and `&[u16]`. It does not work: `Hash` agreeing is
not enough, `Eq` must agree too, and `Borrow<T>`'s contract requires the
borrowed value to *be* the key's content. Comparing UTF-8 bytes to UTF-16
units requires decoding one of them, at which point it is a helper
function and not a `Borrow` impl. Which is fine — see `eq_str` below —
but it is not a map key route.

### The `Hash`/`Eq`/`Ord`/`Borrow` block, restated

This is the same thread as the section above and must not be answered
twice. After the change, `rc_str.rs`'s comparison block reads:

| impl | before | after | note |
|---|---|---|---|
| `Hash` | `self.as_str().hash(h)` | `self.as_units().hash(h)` | matches `<[u16] as Hash>`, verified |
| `Eq` | ptr-eq ‖ `str` compare | ptr-eq ‖ `[u16]` compare | fast path unchanged |
| `Ord` | `str::cmp` (code-point order) | `[u16]::cmp` (code-unit order) | **this is a fix** — JS `<` on strings is defined as code-unit comparison, so `"\uFF3A" < "\u{10000}"` flips to the right answer, and `localeCompare` follows |
| `Borrow<str>` | yes | **gone** | replaced by `Borrow<[u16]>` |
| `AsRef<str>` | yes | gone | replaced by `as_units()` |
| `Deref` | `str` | `[u16]`, added in Stage 3 | see above |
| `PartialEq<str>`, `PartialEq<&str>` | yes | **replaced by `eq_str(&self, &str) -> bool`** | compares `self.as_units()` against `s.encode_utf16()` without allocating; keeps the ~30 `s == "literal"` sites cheap and readable |
| `From<&str>`, `From<String>`, `From<&String>` | memcpy | transcode, ASCII fast path | the only constructors; unchanged signatures, so every host-side `RcStr::from("…")` keeps compiling |
| `Display` | `str::fmt` | narrow to a `String` first, U+FFFD for unpaired | lossy by construction; see the JSON boundary |
| `FromIterator<&str>` | collect into `String` | collect into `Vec<u16>` | |

---

## What `as_str()` becomes, by file

269 textual `as_str()` matches across the workspace, but most are
`String::as_str` and unrelated. The honest count is the probe's: **186
compile errors in `interp` (lib), 237 with unit tests**, distributed:

| file | errors | what happens |
|---|---|---|
| `interp/src/builtin/string.rs` | 71 | rewritten over `&[u16]` — the bulk of the work |
| `interp/src/vm/dispatch.rs` | 41 | `field: &RcStr` (Kind B), `wide!` (Kind A), string index/`length`/spread paths |
| `interp/src/builtin/edit.rs` | 29 | transcode at the boundary — see below |
| `interp/src/builtin/regexp.rs` | 22 | `find_from_ucs2`/`find_from_utf16`, indices already in units |
| `interp/src/vm/methods.rs` | 20 | `preview`, `to_js_string`, `stack_value_to_json`, `resolve_proto_chain` |
| `interp/src/rc_str.rs` | 10 | the impls above |
| `interp/src/builtin/json.rs` | 9 | needs its own string serializer — see below |
| `interp/src/builtin/array.rs` | 6 | `join`, `wide!("length")` |
| `interp/src/vm/value.rs` | 5 | `MapKey` hashing, `Debug` |
| `object/typedarray/number/mod/map/console` | 10 | scattered one-liners |
| `optimizer`, `compiler/emit`, `testutil` | 3 | one-liners |
| compiler tests + `vm/tests.rs` | 51 | assertions comparing against literals → `eq_str` |

Each one resolves to exactly one of four things:

1. **Transcode** — `s.to_string()` where the consumer genuinely wants
   UTF-8 (diagnostics, `Display`, the JSON boundary, `similar`).
2. **Move to `&[u16]`** — the string builtins, dispatch's index path.
3. **`eq_str`/`wide!`** — comparisons against literals.
4. **Becomes lossy** — anywhere a lone surrogate could reach a UTF-8
   sink. There is no fallible option here that is worth taking; see the
   policy below.

`agent/` is nearly untouched: 38 `Value::String` sites and 8 `RcStr`
mentions, and all but a handful are `RcStr::from(&str)` (signature
unchanged) or `s.as_str().to_owned()` (becomes `s.to_string()`).
`agent/src/machine.rs:5110  value_json` and the three
`agent/src/debug/runner.rs` constructions are the whole list.

---

## The rest of the system

### The JSON boundary, measured

`stack_value_to_json` (`vm/methods.rs:2062`) and `json_to_stack_value`
(`:2210`) are the crossing, plus `value_json` in
`agent/src/machine.rs:5110`. Every tool result, `history.fetch` value,
note and log line goes through them. Measured on this machine, 100 KB of
realistic ASCII source text, mean of 200 runs:

```
utf8 -> utf16, encode_utf16():                 86.0 µs
utf8 -> utf16, ASCII widen (b as u16):          2.7 µs
utf16 -> utf8, String::from_utf16():           62.4 µs
utf16 -> utf8, ASCII narrow incl. scan:        52.0 µs
  of which the all-ASCII scan alone:           22.6 µs
String::clone (memcpy, the floor):              0.9 µs
serde's own escape scan over the same string:  28.4 µs
```

**In: 2.7 µs per 100 KB with an ASCII fast path** — a tenth of the
escape scan `serde_json` already performs on the same bytes while
parsing them. **Out: ~30 µs per 100 KB** once the header's `is_ascii`
bit removes the 22.6 µs scan. For the common case in this harness — a
100 KB file read, crossing twice — the change costs about 35 µs against
a round trip measured in seconds. It is not a consideration.

**Memory is ×2 and that is real**: 100 KB in becomes 200 KB held. For
short strings the allocator's size classes absorb much of it (a
16-byte header plus an 8-unit name is 24 → 32 bytes, both landing in the
32-byte bin). For a session holding a few large files it is a few
hundred kilobytes. Acceptable; and the two-representation escape hatch
stays open behind `as_units()`.

**The policy that has to be decided: unpaired surrogates going out.**
Today a mid-character slice *throws*. Under `u16` it succeeds and
produces a lone surrogate, which is correct JS — and which then has no
UTF-8 form when it reaches `serde_json::Value::String`. Options are
error, or replace with U+FFFD. **Replace, and do it in one function so
it is countable.** Erroring would make `stack_value_to_json` fallible on
data the program legitimately produced, and would turn a display problem
into a crashed run. But note what has been traded: *a loud failure for a
quiet one*, against the grain of `25_JS_DIALECT.md`. Mitigations, both
cheap: give the replacement path a counter the debugger can show, and
ship `isWellFormed`/`toWellFormed` (Stage 3) so a program can ask.

`JSON.parse`/`JSON.stringify` are worse than a policy question.
`serde_json` **rejects** a lone-surrogate escape on parse —
`JSON.parse("\"\\ud834\"")` fails today with *"unexpected end of hex
escape"* — and cannot emit one on stringify. `built-ins/JSON/stringify/
value-string-escape-unicode.js` requires `JSON.stringify("\uD834")` to
be `"\ud834"`. So `interp/src/builtin/json.rs` needs its own string
serializer for the `Value::String` case (≈60 lines: escape, and emit
`\udXXX` for an unpaired unit) and its own hex-escape handling on the
parse side. This is Stage 6 and it is the one place where routing
through `serde_json` stops being free.

### Regex

`regress 0.10` has a **`utf16` feature** that this repo does not enable,
providing `Regex::find_from_utf16(&[u16], start)` and
`find_from_ucs2(&[u16], start)`. The two differ in exactly the way JS
does: `Utf16Input` pairs surrogates (the `u` flag), `Ucs2Input` does not
(no `u` flag). Positions come back as **code-unit offsets**. The engine
already has the right shape; we have been feeding it the wrong input
type.

So `interp/src/builtin/regexp.rs` (`build_exec_result:31`, `try_reg_exp`)
and the six regex-taking functions in `string.rs` (`str_split:26`,
`str_replace:229`, `str_replace_all:274`, `str_match:464`,
`str_match_all:438`, `str_search:507`, plus `push_replacement:344`)
switch input type and get correct `index`, `lastIndex` and
`match.indices` for free.

**Cost, stated plainly:** `Utf16Input` and `Ucs2Input` both declare
`CODE_UNITS_ARE_BYTES = false`, which disables regress's `memchr`-backed
literal-prefix prefilter (`classicalbacktrack.rs:1078`, `scm.rs:118`).
Regex-heavy programs — and agent programs use `split(/…/)`, `match`,
`replace` constantly — will get slower on the scan-to-first-candidate.
Nothing measured this; it should be, on a representative `replace` over
a 100 KB file, before Stage 4 is called done.

**Does it help conformance?** Barely — see the numbers. 1,296 RegExp
files fail; the attributable ones are about six. The rest are `Symbol.*`
protocol methods, the regexp-modifiers proposal, and regex *parser*
strictness. But it removes a whole class of silent wrongness
(`"a😀b".match(/b/).index === 5`) and it is a prerequisite for anything
else in the area being correct.

### Source text, spans, diagnostics

Unchanged, and this is worth saying because it looks like it should
change. `Span` (`interp/src/span.rs`) and `Diagnostic`
(`interp/src/diag.rs:34  line_col`) index the *source*, which stays a
UTF-8 `&str` held by the host, parsed by `oxc`. Nothing about the
runtime string representation touches them. `Diagnostic::render` keeps
working byte-for-byte.

What *does* change is literal lowering — `compiler/expr.rs:22`,
`compiler/literals.rs:88`, `compiler/class.rs:237`,
`compiler/call.rs:659`, `compiler/destructure.rs:41`/`:72` — which
currently reads `oxc`'s already-cooked `&str`. For everything except
lone surrogates, `RcStr::from(&str)` transcodes at compile time and they
need no edit at all beyond the type. For lone surrogates, see Stage 5.

### `edit.rs` and the diff

`interp/src/builtin/edit.rs` is 1,507 lines built on byte offsets:
`headIndex`, `braceRange`, `lines_of:176`, `brace_range_from:370`,
`enclosing_pair:458`, and error messages that literally say *"byte
index"*. Its offsets are **visible to the program**, so if they stay
bytes while `indexOf` returns code units, the change manufactures a new
papercut of exactly the kind it is meant to remove.

**Recommendation: transcode at the `Edit.*` boundary, not throughout.**
`edit_text:23` already normalises the receiver; have it produce a
`String`, keep the entire 1,500-line body working on UTF-8 exactly as it
does now, and convert offsets at the two edges — code units in, bytes
internally, code units out. For ASCII text (which is what `Edit.*`
operates on in every observed run) the conversion is the identity and
the `is_ascii` bit makes it free. This turns ~500 lines of risky rewrite
into ~120 lines of boundary code, and the error messages change from
"byte index" to "index".

`agent/src/host/tools.rs`'s `similar::TextDiff<'_, '_, str>` integration
is **unaffected**. It lives on the host side of the JSON boundary and
works on `String` that never was an `RcStr`.

### The card

`agent/card/card.md:204` states the byte rule to the model in the
"Three places this dialect answers differently" table. After the change
`"aéb".length` is `3` and **the row should be deleted, not corrected** —
the divergence is gone, and the table's purpose is to list divergences.
That leaves two rows and requires updating the heading. `agent/src/card.rs`'s
`EXPECTED_LEN` (currently `23086`) must be updated in the same commit;
that is what the guard is for.

---

## The conformance estimate

**+50 to +75 tests, against a baseline of 8,321 passing.** Roughly
+0.7% of the pass count, +0.13% of the 53,658-file corpus. Here is how
that number was reached, because the number matters less than whether it
can be checked.

Baseline, confirmed from `conformance/expectations.json`: 8,321 pass /
30,212 fail / 15,125 skip.

**Step 1 — scope.** 1,993 failures under `built-ins/String`,
`built-ins/RegExp`, `built-ins/JSON` and
`language/expressions/template`.

**Step 2 — could encoding possibly matter?** Strip the frontmatter and
comments from each failing test and keep only those whose *executable
code* contains a non-ASCII character or a `\u` escape above 0x7F.
**154 of 1,993.** (Doing this without stripping comments gives 337 and
is wrong: test262's `info:` blocks are full of `≥` and `—`, and files
like `slice/S15.5.4.13_A1_T11.js` contain nothing wider than `\u0041`.)

**Step 3 — is it failing on an assertion, or on a missing global?** Run
each of the 154 individually (`./target/release/conformance <path>`,
~70 ms each) and read the cause histogram. 94 fail on
`UncaughtException` — a real behavioural mismatch. The other 60 fail on
`TypeError`, `ReferenceError (Symbol …)` or a semantic rejection, which
encoding cannot help.

**Step 4 — classify the 94 by hand.**

| cause | count | converts? |
|---|---|---|
| `ToPrimitive` on objects (`valueOf`/`toString` overrides): `indexOf/S15.5.4.7_A4_*`, `lastIndexOf/A4_*`, `slice/A1_T11-13`, `substring/A1_T10-13`, `match/A1_T11-12`, `replace/A1_T11-13`, `search/A1_T10-13`, `split/separator-override-*` | 24 | no — a documented divergence, unrelated |
| RegExp parser strictness and the modifiers proposal: `early-err-modifiers-*`, `syntax-err-arithmetic-modifiers-*`, `regexp-modifiers/*`, `unicode_restricted_*`, `named-groups/*-property-names-invalid` | 35 | no |
| `JSON.parse` whitespace: `15.12.1.1-0-*`, `invalid-whitespace` | 8 | no |
| `trim` whitespace set (JS includes `\uFEFF`, Rust's `is_whitespace` does not): `trim/15.5.4.20-*` | 8 | not caused by encoding, but naturally fixed while rewriting `trim` over units |
| `replaceAll` `$`-substitution: `getSubstitution-0x0024-*` | 5 | no — the `é` is incidental |
| **directly attributable**: `at/returns-code-unit`, `padStart/normal-operation`, `padEnd/normal-operation`, `JSON/stringify/value-string-escape-unicode`, `RegExp/dotall/{with,without}-dotall{,-unicode}`, `RegExp/prototype/exec/u-{captured,lastindex}-value` | **10** | **yes** |
| enabled only by the change: `isWellFormed/returns-boolean`, `toWellFormed/returns-well-formed-string` | 2 | yes, if the methods are added |
| unclassified remainder | 2 | unknown |

**Step 5 — what the change makes implementable.** The bigger prize is
not in the non-ASCII bucket at all. `charCodeAt`, `codePointAt`,
`isWellFormed` and `toWellFormed` are 57 files of which **44 are
substantive** (the rest are `length.js`/`name.js`/`prop-desc.js`, which
need property descriptors we do not have). These methods are
*undefinable* over UTF-8 bytes and one-liners over `&[u16]`. Discounting
for the ones that also need `ToPrimitive` or `Symbol` coercion: **+30 to
+45**. `String.prototype.at` adds up to 8 more (currently 0/11).

So: **10 directly, +2 unlocked, +30–45 from the new methods, +8 from
`trim`'s whitespace set riding along.** Call it **+50 to +75**, and
regard anything above +90 or below +35 as a sign the classification was
wrong.

**Reproduce it:**
```
cd /home/robin/Code/agent2
timeout 590 cargo test -p conformance -- --ignored --nocapture   # ~160s, the gate
./target/release/conformance built-ins/String/prototype/padStart/  # a subset
```

**This is not a good enough reason on its own, and the document should
not pretend otherwise.** +0.13% of the corpus does not justify the
largest change the crate has seen. The justification is the seven silent
wrong answers in the problem section, the fact that three more were
found this morning and four more this afternoon, and the structural
argument that a unit nothing checks will keep drifting. Conformance is
the *instrument*, not the motive: it is the only falsifiable thing in
this plan, which is why the number is here.

---

## Stages and gates

**Stop-half-way honesty, up front: Stages 2 and 3 are one atomic
change.** Once `Header.len` counts code units, `.length` counts code
units, and every builtin that consumed a byte offset is wrong until it
is fixed. There is no intermediate state in which the interpreter works
and the representation is half-swapped. Everything before Stage 2 and
everything after Stage 3 is independently stoppable and shippable. Plan
for Stage 2+3 to land as one commit that is large and reviewed as a
unit; do not try to split it.

Baseline before anything: `8321 / 30212 / 15125`, `cargo test -p interp`
1,140 passing, `cargo test -p agent` green.

### Stage 1 — narrow the surface (representation-neutral)

Change the six `field: &str` signatures to `&RcStr`; introduce `wide!`
and `ascii_wide` (unused for now, with their own unit test); replace the
11 Kind-A literal lookups with named `const` keys; make every string
builtin reach its receiver through `str_receiver`/`string_receiver`
(`builtin/mod.rs:727`, `:736`) rather than ad hoc.

```
timeout 590 cargo test -p interp && timeout 590 cargo test -p agent
timeout 590 cargo test -p conformance -- --ignored --nocapture
```
**Gate:** all green; conformance **exactly** `8321 / 30212 / 15125`. A
delta of any size here means the "neutral" refactor was not neutral —
stop and find out why before going further.

### Stage 2+3 — the payload swap and code-unit semantics (atomic)

`rc_str.rs` → `[u16]`, `is_ascii` bit, `as_units()`, **no `Deref` yet**,
rename to `JsString`, `Borrow<[u16]>`, `eq_str`. Then fix every one of
the ~186 compile errors. Then rewrite the builtins: `.length`
(`dispatch.rs:2601`), `s[i]` (`dispatch.rs:216-251`), and in
`builtin/string.rs` — `str_slice:167`, `str_substring:192`,
`str_index_of:115`, `str_last_index_of:133`, `str_includes:100`,
`str_starts_with:150`, `str_ends_with:158`, `str_pad_start:538`,
`str_pad_end:562`, `str_repeat:586`, `str_char_at:632`, `str_at:661`,
`str_concat:682`, `str_split:26`, `str_from_char_code:695`,
`str_from_code_point:712`, and **delete** `clamp_start:736`,
`clamp_end:746` and `char_at_byte:647` outright — they exist only to
paper over byte offsets. `extract_substring:754` becomes a plain slice.
Add `charCodeAt`, `codePointAt`, `isWellFormed`, `toWellFormed` to
`builtin/mod.rs`'s table and `string.rs`. Fix `trim`'s whitespace set
while you are in there. Add `Deref<Target = [u16]>` **last**, after the
error list is empty, as a separate commit.

```
timeout 590 cargo test -p interp && timeout 590 cargo test -p agent
timeout 590 cargo test -p conformance -- --ignored --nocapture
```
**Gate:** all green. Conformance pass **≥ 8,360** (+40 or better) and
**no test that passed before may fail** — check the `--check` diff, not
the totals, because a wash could be +50/−50. Update
`interp/tests/string_is_walked_by_character.rs` to assert the new
answers; it is the file that records this contract.

### Stage 4 — regex over code units

Enable regress's `utf16` feature. Route by the `u` flag:
`find_from_utf16` for unicode mode, `find_from_ucs2` otherwise. Rewrite
`regexp.rs:31 build_exec_result` and the seven regex consumers in
`string.rs`.

```
timeout 590 cargo test -p conformance -- --ignored --nocapture
./target/release/conformance built-ins/RegExp
```
**Gate:** RegExp subset ≥ 190 pass (today's), total pass not lower than
Stage 3. **Plus a measurement that does not exist yet:** time a
`replace(/…/g, …)` over a 100 KB file before and after. If it is more
than 3× slower, stop and consider keeping the UTF-8 path for
all-ASCII subjects behind the `is_ascii` bit.

### Stage 5 — string literals, and the reason any of this was worth it

Stop reading `oxc`'s cooked `Atom`. Re-cook string literals and template
cooked-values from the raw source slice (`lit.span`) into `Vec<u16>`,
handling `\uXXXX`, `\u{…}`, surrogate pairs and **lone surrogates**
per the spec. ≈250 lines, one new module, touching
`compiler/expr.rs:22`, `compiler/literals.rs:88`, `compiler/class.rs:237`,
`compiler/call.rs:659`, `compiler/destructure.rs:41`/`:72`,
`analyzer/const_fns.rs:41`.

**Gate:** `"\uD800".length === 1`, `"\uD800".charCodeAt(0) === 55296`,
`"\uD83D\uDE00".length === 2`. Conformance not lower.

**This stage is not optional, and that is the most important sentence in
this plan.** Without it a source literal still cannot hold a lone
surrogate, which means the *one* capability that `u16` has and
"UTF-8 with an ASCII bit" does not is still missing, and the whole
change reduces to code-unit indexing — which the cheap option would have
delivered for a fifth of the work. If Stage 5 is going to be skipped,
revisit the decision instead.

### Stage 6 — the JSON boundary

`stack_value_to_json`, `json_to_stack_value`, `value_json`: ASCII fast
paths both ways, U+FFFD for unpaired surrogates on the way out, with a
counter. `builtin/json.rs`: a string serializer emitting `\udXXX`, and
hex-escape handling on parse that does not route lone surrogates through
serde.

**Gate:** `built-ins/JSON` subset ≥ 23 pass (today 22);
`JSON.stringify("\uD834") === '"\\ud834"'`; `cargo test -p agent` green.

### Stage 7 — `edit.rs` and the card

Boundary transcode in `edit_text:23`, offset conversion at the two
edges, "byte index" out of the messages. Delete the `.length` row from
`card.md`, update `EXPECTED_LEN`.

**Gate:** `cargo test -p agent` green including `the_card_is_stable`;
`cargo test -p interp` green; a manual `Edit.replaceOnce` over a file
with an em dash returns the right offsets.

---

## What it costs

**Size.** ~2,000–2,800 changed or new lines across ~30 files, plus
`conformance/expectations.json` churn. The distribution is lopsided:
`builtin/string.rs` (≈700 lines rewritten), the 186 mechanical
compile-error fixes, the new literal cooker (≈250), the JSON serializer
(≈60), `edit.rs`'s boundary (≈120), `rc_str.rs` itself (≈150 of 369).

**Risk, ranked.**

1. **Stage 2+3 is atomic and large.** One commit, no working
   intermediate. If it goes wrong it goes wrong all at once. Mitigation:
   Stage 1 shrinks it; the compiler enumerates the work; the gate is a
   53,658-file diff, not a summary.
2. **`Deref` added early would make the migration silent.** Guard
   against this explicitly in review — it is one line that converts 200
   compile errors into 200 unreviewed behaviour changes.
3. **Regex performance** is the one number nobody has. It could be
   fine; it could be 5×. Measure before Stage 4 ships.
4. **The U+FFFD policy trades a loud failure for a quiet one** at the
   boundary this project cares most about. Counter it with the counter
   and with `isWellFormed`, and accept that it is a real loss.

**Irreversible.** Nothing, in the usual sense — it is one crate and git
works. The things that are *practically* hard to undo: the
`expectations.json` rebaseline (a bad one hides a regression forever,
which this repo has been bitten by twice — `c48661c`, `736093d`); the
`&str` → `&RcStr` signature change, which is an improvement and will not
want undoing; and the card edit, which changes what the model believes
about a primitive and will show up in run behaviour before it shows up
in a test.

---

## What stays broken

- **`ToPrimitive` on objects** — 24 of the String failures examined, and
  the single largest attributable bucket in the area. Entirely
  unrelated, entirely unaddressed.
- **`Symbol`** — no `Symbol.iterator`, `Symbol.match`, `Symbol.replace`,
  `Symbol.split`. Worth ~65 of the non-ASCII String/RegExp failures
  alone and far more overall.
- **RegExp parser strictness and the modifiers proposal** — ~35 of the
  examined failures. A regress-version question, not ours.
- **`normalize`** — still absent. It needs a Unicode normalisation
  table, which is a dependency decision (`unicode-normalization`), not a
  representation one. 14 files.
- **`localeCompare`** remains code-unit ordering with no locale support.
  This becomes *more* correct under the change (JS's `<` is defined as
  code-unit comparison) but `intl402` — 1,239 failures — stays dark.
- **ASCII memory is doubled.** The two-representation fix stays on the
  shelf; `as_units()` keeps the door open.
- **The regex prefilter.** `memchr` over a `[u16]` input is off, and
  regress offers no way to turn it back on.
- **`JSON.parse` whitespace and `replaceAll`'s `$`-substitution** — 13
  of the examined failures, small and separate.
- **Unpaired surrogates leaving the VM are lossy and always will be.**
  JSON has no representation for them. The only question was whether to
  be loud or quiet about it, and this plan chooses quiet with a counter.

# Phase 0 — Small cleanups

Self-contained cleanups to land before the reorg (Phase 1), so renames and
deletions happen while everything is still in one file per concern.

## Ground rules (apply to every task below)

- **No behavior change.** Every task here is a refactor or a docs fix.
- After each task: `cargo fmt && cargo clippy && cargo test` must pass clean.
- One commit per task, message prefixed `cleanup:`.
- Do not touch `types.rs` / `tree.rs` (the unbuilt agent harness) and do not
  start anything from later phase files.

## Task 1: Trim redundant stack instructions (`Dup`/`Swap`/`Rot`)

`Instr::Dup`, `Instr::Swap`, `Instr::Rot` are strictly special cases of the
generalized reach instructions already in `vm.rs`:

- `Dup` ≡ `Pick(0)`
- `Swap` ≡ `Dig(1)`
- `Rot` ≡ `Dig(2)`

Remove the three specific variants:

1. Grep for every construction/match of `Instr::Dup`, `Instr::Swap`,
   `Instr::Rot` across `compiler.rs`, `optimizer.rs`, `vm.rs` (including
   tests). Replace emissions with the `Pick`/`Dig` equivalent.
2. In the VM dispatch, ensure `Pick`/`Dig` handle the small-n cases with no
   extra cost (e.g. `Dig(1)` should be a plain swap of the top two slots, not
   a general rotate). Add a fast path inside the `Pick`/`Dig` arms if one is
   not already there.
3. In `optimizer.rs`, update any peephole pattern that matched the removed
   variants to match the canonical forms instead. Search the `pe_*` helpers
   and the `Reduction` table.
4. Update the instruction doc comments in `vm.rs` (the block around
   `Pick`/`Dig`/`Nip` references the removed names — rewrite it so the
   generalized forms are primary, with the old names mentioned only as
   "formerly").
5. Update tests that asserted on the removed instructions.

**Acceptance:** `Dup`, `Swap`, `Rot` no longer exist anywhere in `src/`;
all tests pass; the alloc-count tests (exact-count assertions) still pass
unchanged — if an alloc count shifts, the fast path in step 2 is wrong.

## Task 2: Mark drifted design docs as historical

The root-level plan docs describe a previous architecture and must not be
read as current ground truth. Known drift (verify each, there may be more):

- `COMPILER_PLAN.md` references `heap[0]`, `HeapValue`, `ObjGetDyn`/
  `ObjSetDyn`, `Alloc`, and a `Read`/`Write`/`variables` HashMap. Current
  code: typed heaps (`arrays`/`objects`/`closures`), state at `objects[0]`,
  `IndexGet`/`IndexSet`, `EnterFrame` as sole frame setup.
- `ALLOCS.md`, `PERF.md`, `PROPAGATION_PLAN.md`: plans that have since been
  executed (see git log: the perf/alloc commit series).

For each file, do **not** rewrite history. Instead:

1. Add a banner at the very top:
   ```
   > **Status: HISTORICAL.** This document records the plan as designed at
   > the time. The code has since evolved; where they disagree, the code and
   > its module docs win. Known drift: <one-line list for this file>.
   ```
2. Fill in the known-drift list per file by skimming it against the current
   code. Spend at most ~15 minutes per file; the goal is a warning label,
   not an audit.
3. Sections of `COMPILER_PLAN.md` that are still accurate and load-bearing
   (e.g. the locked decision that `state` is the only durable surface) should
   be listed in the banner as "still authoritative: §…".

**Acceptance:** every root `.md` plan doc opens with a status banner; no
other content changes beyond the banner.

## Task 3: Sweep stale comments referencing pre-refactor names

After the `HeapValue` split and `StackValue → Value` / `Number → Float`
renames, some comments still use old names. Grep `src/` for: `HeapValue`,
`StackValue`, `heap[0]`, `Value::Number`, `ObjGetDyn`, `ObjSetDyn`, `Alloc `
(the old instruction), and fix comments to current names. Code is already
correct — this touches comments and doc-strings only.

**Acceptance:** the greps above return no hits in comments that present the
old name as current (a comment explicitly describing history, like "the old
type-specific ArrGet/ArrSet", may stay).

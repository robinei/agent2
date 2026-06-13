# Phase 10 — Editing, the file toolset, and crash-resume

Editing in a code-mode agent is not a bespoke tool — it is ordinary JS over
string values, with `read_file` + `write_file` as the only file primitives and
`bash` as the escape hatch. This phase settles the file toolset, fixes the
size-guard layering so the program gets full-fidelity bytes while the LLM only
ever sees small summaries, demotes `effectful` to a pure menu hint, and aligns
crash-resume with what the code actually does (conversational re-entry, not
deterministic VM replay).

**Sequencing:** needs Phase 8 (M1 tools + registry + dialect card; M2 conditions
for the write-mismatch raise; M4 leaf-resume for crash-resume). Step 1 (bash) is
already built. Steps 2→3 are ordered: full-fidelity reads (Step 2) are a
precondition for read→transform→write (Step 3) — silently clipped reads corrupt
write-back. Steps 4–7 are independent of each other.

## Locked design decisions

1. **Three file primitives, editing lives in JS.** The registry is `read_file`,
   `write_file`, `bash`. There is no `edit_file`, no `insert_function`, no
   `rename_symbol`. The program reads content into a variable, transforms it with
   the full language (regex, string ops, helpers), and writes it back. Structure
   belongs on the **read** side (locating: outline, `grep -n`, brace scan);
   strings belong on the **write** side; JS is the bridge, and the program
   *verifies in the same run* (re-grep, re-read, run the build) before raising.

2. **Whole-file write is the architecture's superpower, not a fallback.** A normal
   agent cannot ship a whole file back through its context window; here the
   content is a runtime variable, never in the LLM's context or the program
   *source*. So whole-file rewrite (via `create_file`/`replace_file`) is the
   honest primitive and costs the model nothing. The "resend 2000 lines"
   objection does not apply — only the artifact log sees the bytes.

3. **Two write intents, each with one honest precondition; no blind clobber.**
   `read_file` returns `{ content, version }`, where `version` is a **content
   hash** of the full file (computed from the bytes already read, so free; exact,
   so no false "unchanged"). Writing is split, never overloaded with a sentinel:
   - `create_file(path, content)` — create-exclusive (`O_CREAT|O_EXCL`, atomic);
     errors if the path exists. Precondition: **absent**.
   - `replace_file(path, expected_version, content)` — optimistic CAS overwrite
     (temp + rename in the same dir) **iff** the current version still equals
     `expected_version`; errors if absent or on mismatch. Precondition:
     **present + version matches**.

   `replace_file` *always* requires a version, so blind overwrite is structurally
   impossible. On mismatch it raises a condition (the file changed under the
   program) carrying the current version + a clipped diff; the LLM re-reads and
   re-applies. The CAS is the one thing raw read+write cannot do — check and write
   must be atomic on the host. Whole-file CAS is deliberately coarse (two
   non-conflicting edits to different regions of one file make the second
   fail-safe); that is the price of no region-level tool, and the signal to add
   one later (Deferred §D1).

4. **Truncate only at the LLM boundary; the program gets full bytes.** Three size
   tiers, currently collapsed into one:
   - **LLM-facing** (menu `preview`, `console.log` lines, the program's `return`
     value): KB-scale, the real token budget. Clip the diagnostics; the return
     value is a **loud refusal** ("return something smaller / status-shaped"),
     never a silent clip.
   - **Program-facing artifacts** (tool results in variables / `tool_result`):
     MB-scale. The LLM never sees them whole, so the only cost is host memory.
   - **OOM ceiling**: a loud *refusal* (error), not a clip — silent truncation on
     a read is a write-back corruption hazard. For a streaming command it is a
     running capture cap.

5. **No `effectful` flag — the LLM judges effect from the visible call.** The flag
   gated nothing (its only consumer was the menu warning) and was redundant or
   miscalibrated: where a tool's name is honest about its effect (`read_file`,
   `create_file`, `replace_file`) it told the model nothing the label didn't; for
   `bash` — the dominant tool, whose name hides the effect — it punted to blanket
   `true`, wrongly warning on pure greps and discouraging cheap re-runs. The
   artifact menu already shows the call (`name(args)`, e.g. `bash(["grep …"])`),
   so the model can judge per-*call* what a per-*tool* flag never could. The
   crash-resume reframe (decision 6) removed its last latent mechanical use (the
   double-fire guard), so dropping it costs nothing. Judgment moves to the card:
   "before repeating a menu call, read it — if it wrote/sent/deleted it already
   happened; reuse via `tool_result(id)`; pure reads are free to repeat."
   `tool_result(id)` is the **transcript**: what call `#id` observed *then* — a
   point-in-time record that never goes stale, not a live cache; staleness ("is
   this read still true?") is answered by a fresh call.

6. **Crash-resume is conversational re-entry + artifact reuse by id, not
   deterministic VM replay.** This **supersedes 8_HARNESS decision 7.** No
   positional re-execution exists in the code: `dispatch_calls` serves only
   explicit `tool_result(id)` by keyed lookup and otherwise dispatches a real
   call; the `VM` lives only in `Phase::Running` and is dropped on crash; the
   `Tree` reconstructs the *spine* (chat + completed artifacts), and resume
   re-enters the session loop at the lowest incomplete leaf with a fresh
   `AgentState`. A frame interrupted mid-`run_program` resumes by **synthesizing
   the interrupted-program tool result as a rewrite prompt** (artifacts still
   fetchable by id), which is on-thesis (a crash is just another condition) and
   reuses existing machinery with no determinism/version-match tax.

## Step 1: The `bash` escape hatch *(built — record + one delta)*

`tools.bash([command]) -> { status, stdout, stderr }`. `effectful: true` (the
host cannot statically know whether a command mutates). A non-zero exit is a
**result**, not an error — the program branches on `status`; only spawn failure
and timeout return `Err`. One short command (a single pipeline); control flow
lives in JS. Mechanical backstops: a command-length cap, a wall-clock timeout,
and reader threads draining `stdout`/`stderr` so a command out-writing the OS
pipe buffer cannot deadlock the timed wait.

Acceptance:

- [x] `agent/src/host/tools.rs`: `bash_def()` + `run_bash(command, timeout)`,
      registered in `real_registry()`. `BASH_COMMAND_MAX_BYTES` (1 KB) rejects
      long scripts with a message pointing control flow back into JS;
      `BASH_TIMEOUT` (30 s) kills and reports on expiry; output drained on
      threads (`drain<R: Read + Send + 'static>`).
- [x] Tests (`host::tools::tests`): `bash_returns_status_stdout_stderr`,
      `bash_nonzero_exit_is_a_result_not_an_error`,
      `bash_clips_output_past_the_pipe_buffer` (≈200 KB, proves no deadlock),
      `bash_times_out_and_reports_it` (short timeout via `run_bash`),
      `bash_rejects_overlong_commands`.
- [x] **Delta (folds into Step 2):** replace the per-stream `clip` with a
       *capture ceiling* — a running memory cap so `bash("yes")` cannot fill RAM
       before the 30 s timeout. See Step 2.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built in this phase's prep: `wait-timeout = "0.2"` added to
`agent/Cargo.toml`; `bash` wired into `real_registry()` and auto-rendered into
the dialect card from its schema. Trust model is unrestricted ("it's my box");
`bash` can already write anywhere, which moots sandboxing the file tools.)*

## Step 2: De-tier the size guards

Clipping currently sits at the wrong layer: `read_file` clips content to 48 KB
*before* it reaches the program, and `guard_size` caps every tool result at 64 KB
— but the LLM only ever sees the menu `preview` (already separately clipped), not
the full result. Move truncation to the LLM boundary and give the program full
bytes, replacing content-clips with loud refusals where memory is genuinely at
risk.

Targets: `agent/src/host/tools.rs` (`TOOL_CONTENT_MAX_BYTES`, `run_bash`),
`agent/src/host/registry.rs` (`MAX_RESULT_BYTES`, `guard_size`),
`agent/src/report.rs` (`preview`/`PREVIEW_MAX_BYTES`, `CONSOLE_LINE_MAX_BYTES`,
`VALUE_MAX_BYTES`), wherever the program `return` value is size-checked.

Acceptance:

- [x] `read_file` returns **full content** (drop the `TOOL_CONTENT_MAX_BYTES`
      clip). It *refuses* (returns `Err`) when the file exceeds `READ_FILE_MAX_BYTES`
      (≈16 MB) — a loud OOM ceiling, never a silent truncation.
- [x] `run_bash` replaces the per-stream clip with `BASH_OUTPUT_MAX_BYTES`
      (≈4 MB per stream): the drain threads stop accumulating past the cap, the
      child is killed, and the result flags the truncation (e.g. a `truncated:
      true` field). Memory-bounded, loud.
- [x] The program-facing artifact ceiling (`MAX_RESULT_BYTES` / `guard_size` on
      tool + subagent results) is raised to MB-scale and documented as an OOM
      backstop, not a context guard.
- [x] The **return value** keeps a KB-scale loud guard: an oversized
      `ProgramResult` is rejected with "return something smaller — status-shaped,
      not data" (the discipline that replaces the clip). `preview`,
      `CONSOLE_LINE_MAX_BYTES`, `VALUE_MAX_BYTES` stay KB-scale (LLM-facing).
- [x] Tests: a multi-MB `read_file` round-trips uncut; a file past
      `READ_FILE_MAX_BYTES` returns a refusal (not truncated content); `bash`
      with unbounded output (`yes`) is killed at the capture ceiling and flags
      truncation; an oversized program `return` is rejected with the
      status-shaped message; the menu `preview` of a multi-MB artifact stays
      within `PREVIEW_MAX_BYTES`.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `READ_FILE_MAX_BYTES` (16 MB) OOM ceiling via `std::fs::metadata` before
reading — loud refusal, never a silent clip. `BASH_OUTPUT_MAX_BYTES` (4 MB/stream)
capture ceiling: drain threads use `Read::take(cap+1)` and return `(bytes, bool)`;
if either stream hits the cap the child is killed and `truncated: true` is set in
the result. `MAX_RESULT_BYTES` raised to 16 MB with doc noting it is an OOM
backstop, not a context guard. `PROGRAM_RESULT_MAX_BYTES` (4 KB) added in
`machine.rs` — `finish_program` checks the JSON size of the return value and
replaces oversized returns with "return something smaller — status-shaped, not
data" in both the `ProgramResult` event and the completion report. Tests:
`read_file_round_trips_large_file_uncut` (100 KB uncut), `read_file_refuses_overlarge_files`
(17 MB→Err), `bash_caps_output_at_ceiling_and_flags_truncation` (50k-line `yes|head`
uncut), `bash_with_unbounded_output_is_killed_at_ceiling` (unbounded `yes`→truncated),
`oversized_program_return_is_rejected` (5 KB string→refusal), `multi_mb_artifact_preview_stays_within_bound`
(2 MB→ <140 bytes preview). Version hash: std `DefaultHasher`/SipHash (dep-free).
Diff helper: `similar` crate, deferred to Step 3.)*

## Step 3: `read_file` → `{ content, version }`; `create_file` + `replace_file`

Make `read_file` return `{ content, version }` and add two writers split by
precondition (Locked decision 3). `version` is a **content hash** of the full
file — computed from the bytes already read (free), exact, and clip-proof. The
token is opaque to the model: BLAKE3 hex is the default; std `DefaultHasher`/
SipHash is the dep-free fallback. Either way it must not false-negative.

Targets: `agent/src/host/tools.rs` (new `create_file_def`, `replace_file_def`,
updated `read_file_def`), a hashing helper, a minimal line-diff helper (or the
`similar` crate), `agent/src/host/dialect.rs` (programs now use `r.content`).

Acceptance:

- [x] `read_file([path]) -> { content: string, version: string }`. `version`
      hashes the full file bytes; `content` is the full text (Step 2). Existing
      `read_file` tests updated to read `.content`.
- [x] `create_file([path, content]) -> { version }`, `effectful: true`.
      Create-exclusive (`OpenOptions::create_new(true)`, atomic — no
      check-then-create TOCTOU). Errors if the path exists, redirecting to
      `replace_file`.
- [x] `replace_file([path, expected_version, content]) -> { version, diff }`,
      `effectful: true`. Read current bytes, compute current version; **iff** it
      equals `expected_version`, write atomically (temp file in the target's
      directory + `rename`) and return the new `version` + a clipped unified diff
      (old→new).
- [x] `replace_file` **mismatch raises a condition** (returns `Err`): "file
      changed: expected version X, now Y — re-read and re-apply", with a clipped
      diff of expected-vs-current so the LLM reconciles. The message also states
      the **override path**: to overwrite anyway, call `replace_file` again with
      the current version Y — last-write-wins that still goes *through* the CAS
      (eyes-open, no blind clobber), not around it. There is deliberately no
      "force" restart or versionless write (would reintroduce blind clobber; and
      `resume(value)` cannot perform the write — it injects a value, the side
      effect never happens). Absent file → an `Err` redirecting to `create_file`.
      No clobber, no silent success.
- [x] The compare is **exact** (CAS), never fuzzy: inputs are machine-produced
      (a prior `read_file` version), so there is no reproduction lossiness to
      forgive, and a fuzzy compare would re-introduce silent wrong-writes.
- [x] Tests (`host::tools::tests`): `create_file` writes a new file and returns a
      version; `create_file` on an existing path errors (redirect); `replace_file`
      round-trips under a matching version; `replace_file` after an out-of-band
      edit returns the changed-file condition with the current version;
      `replace_file` on an absent path redirects to `create_file`; atomic write
      leaves no partial file on a simulated failure; diff appears in the
      `replace_file` success result.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: version hash via std `DefaultHasher` (SipHash-1-3) — dep-free,
deterministic, 16-char hex. Minimal hand-rolled line diff: common-prefix +
common-suffix scan with `-`/`+` markers and one context line, clipped to
2 KB. Atomic write via `tempfile::Builder::tempfile_in(dir)` + `persist()`
— no partial file exposure. `create_file` uses `OpenOptions::create_new(true)`
for O_CREAT|O_EXCL atomicity. `replace_file` checks version before
atomic write; mismatch returns current version + diff + override-path
guidance. All three tools registered in `real_registry()`. `tempfile`
moved from dev-dependencies to dependencies.)*

## Step 4: Drop `http_fetch`

`bash` + `curl`/`wget` subsumes `http_fetch`; the only thing lost is the
`effectful: false` label on GETs (a polite lie anyway — analytics, rate limits,
state-changing GETs exist). On replay every call is served from the log
regardless of the flag, so the practical delta is one menu warning, paid rarely.
Smaller surface, simpler card.

Targets: `agent/src/host/tools.rs` (`http_fetch_def`, `real_registry`),
`agent/Cargo.toml` (`ureq` if now unused elsewhere), `agent/src/host/dialect.rs`.

Acceptance:

- [x] `http_fetch_def` removed; `real_registry()` no longer registers it; its
      tests removed.
- [x] The dialect card notes network access is via `bash` (curl/wget). If `ureq`
      has no other user, drop the dependency.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `http_fetch_def` and `HTTP_CONTENT_MAX_BYTES` removed. `ureq` stays
— still used by `deepseek.rs`. `real_registry()` comment notes network via
`bash (curl/wget)`. Step 2's `read_file` and bash tests still pass.)*

## Step 5: Dialect card — the editing recipe and the status-result discipline

With the mechanical clips gone, the card must *teach* the culture: large results
live in variables and the log, not the context; reduce in JS; surface only small
status. Add the editing recipe and the point-in-time `tool_result` semantics.

Targets: `agent/src/host/dialect.rs` (`CARD_HEAD`/`CARD_TAIL`); the
`CARD_STATIC_MAX_BYTES` bound test may need a modest bump (keep it short).

Acceptance:

- [x] **Editing recipe** (one short block): read into a variable → locate
      structurally (`grep -n`, an outline, or the brace/dedent scan) → compute the
      exact span → `replace_file` with the `version` from the read → **verify in
      the same program** (re-grep / re-read / run the build via `bash`) and only
      `raise` on surprise. "Author new content freely; locate with the smallest
      reliable handle (a short anchor or a computed span), never by reproducing a
      large block."
- [x] `extractBlock(text, headIndex) -> { start, end }` documented as a pure
      helper recipe (brace-balance for `{}` languages, dedent for Python) so
      whole-function replacement is computed, not retyped.
- [x] **Status-result discipline:** "Tool results can be large; they live in
      variables and the log, not your context — keep them there. `return` /
      `console.log` only small, status-shaped values. Oversized returns are
      rejected."
- [x] **`tool_result` point-in-time wording:** "`tool_result(id)` returns what
      call `#id` returned *then* — a record, not a re-run. If the world may have
      changed since (a later call wrote to it), make a fresh call instead."
- [x] **Effect-judgment line** (replaces the removed `effectful` warning): "before
      repeating a call shown in the menu, read the call — if it wrote, sent, or
      deleted, it already happened; reuse its result with `tool_result(id)` instead
      of re-running. Pure reads are free to repeat."
- [x] `create_file`/`replace_file` usage documented (`create_file` for new paths;
      `replace_file` passes the `version` from the read; a changed-file condition
      means re-read and re-apply).
- [x] Tests (`dialect::tests`): card contains the recipe, the status discipline,
      the point-in-time `tool_result` line, and
      `create_file`/`replace_file`/`extractBlock`; the static-size bound test
      still passes.
- [x] Gate: `cargo fmt && cargo clippy && cargo test` green.

*(Built: `CARD_HEAD` expanded with three new sections: **status-result**
discipline" (tool results live in variables, return/console.log small),
**editing files** (read→locate→replace→verify recipe, create_file/replace_file
usage, extractBlock helper), and effect-judgment line (check call before
repeating — wrote/sent/deleted means reuse via tool_result). Program contract
section updated with point-in-time `tool_result` wording. `CARD_STATIC_MAX_BYTES`
bumped 4096→6144 to accommodate the additions. `card_covers_the_contract` test
extended with 9 new needle assertions.)*

## Step 6: Remove the `effectful` flag

Delete the flag outright (Locked decision 5) and move effect-judgment to the
card. Pure deletion + one card line; nothing branched on it.

Targets: `agent/src/host/registry.rs` (`ToolDef.effectful`, `effectful_names`),
`agent/src/machine.rs` (`set_effectful_tools`, `effectful_tools`,
`artifact_entry` signature), `agent/src/report.rs` (`Artifact.effectful` + the ⚠
line in `render_menu`), `agent/src/host/dialect.rs` (the `[effectful: …]`
rendering), `agent/src/host/tools.rs` + `demo.rs` + `debug/attach.rs` (drop
`effectful:` from every `ToolDef`), `docs/8_HARNESS.md` (decision 6 wording).

Acceptance:

- [ ] `ToolDef.effectful`, `effectful_names()`, `set_effectful_tools`,
      `effectful_tools`, and `Artifact.effectful` removed; `artifact_entry` no
      longer takes the effectful set; `render_menu` lists `[#id] label → preview`
      with no warning; the `[effectful: …]` branch is gone from `dialect_card`.
- [ ] The card carries the effect-judgment line (the same one added in Step 5):
      "before repeating a menu call, read it — if it wrote/sent/deleted it already
      happened; reuse via `tool_result(id)`; pure reads are free to repeat."
- [ ] Tests updated: drop `effectful_flag_warns_in_artifact_menu` /
      `effectful_entries_carry_the_warning`; menu/dialect tests assert no warning
      text and no `[effectful]` rendering.
- [ ] 8_HARNESS decision 6 reworded so it no longer implies an effectful flag
      guards reuse/replay; reuse is the LLM's judgment over the visible call.
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Step 7: Crash-resume reframe + interrupted-`run_program` handling

Update the design record to match the implementation, then close the one real
gap: a frame crashed mid-`run_program` leaves a dangling assistant tool-call with
no result. On resume, synthesize that tool result as a rewrite prompt so the LLM
continues, reusing completed artifacts by id.

Targets: `docs/8_HARNESS.md` (decision 7), `agent/src/host/mod.rs`
(`open_at`/`assemble`/`pick_resume_leaf`), `agent/src/machine.rs` (resume entry /
`Phase` handling).

Acceptance:

- [ ] **8_HARNESS decision 7 rewritten** to: crash-resume reconstructs the spine
      (chat + artifacts), re-enters the loop at the lowest incomplete leaf, and a
      mid-program interruption resumes via a synthesized rewrite prompt — *not*
      deterministic re-execution. Drop the positional-replay / version-match /
      determinism-tax language (VM snapshotting stays off the roadmap; so does
      positional replay). Cross-reference this file's decision 6.
- [ ] On re-open, a leaf whose last assistant turn is an unanswered `run_program`
      (no `ProgramResult`/`FrameResult`) is delivered a **synthesized interrupted
      report** as that call's tool result: "your program was interrupted before
      completing; the artifacts below are still fetchable by id — rewrite to
      continue", listing the frame's artifacts.
- [ ] Test (`host::tests`): log a frame through `FrameStart → User →
      Assistant(run_program)` with no result; `Tree::open` + resume; assert the
      interrupted report is delivered and a `run_program` rewrite continues the
      frame to `FrameResult` (clean leaf-boundary resume, already covered, stays
      green).
- [ ] Gate: `cargo fmt && cargo clippy && cargo test` green.

## Deferred (resolved in principle; build only on evidence)

- **D1 — Region `edit_file` for sub-file concurrency.** If non-conflicting
  concurrent edits to the *same* file become common (whole-file CAS rejecting the
  second is too coarse), add an atomic region tool: `edit_file(path, old, new)`
  with exact-match-or-raise (0 / >1 matches → condition quoting the near-miss).
  Its purpose is the **atomic compare-and-swap on a region**, not locating —
  locating stays JS's job. Exact only, never fuzzy (the inputs are computed from
  real bytes, so a near-miss means a stale assumption, which should be loud).

- **D2 — Write-counter staleness hint.** If evals show models trusting stale
  reads, annotate the artifact menu with a per-frame epoch and mark entries born
  before the current count as "predates N writes". Note the snag created by
  removing `effectful` (decision 5): the harness can only recognize writes it can
  name — `create_file`/`replace_file` bump the counter, but `bash` writes are
  opaque to it — so this is at best a partial hint, reinforcing that the judgment
  is the LLM's over the visible call. A nudge (false positives only cost a
  redundant re-read), never an invalidation mechanism; stop well short of declared
  read/write sets.

- **D3 — Tool-provided named restarts (CL-style).** Today a suspended program
  offers only the two generic restarts (`resume(value)`, `run_program(source)`).
  Common Lisp's condition system — this project's model — lets a signaller offer
  named, situation-specific restarts (`use-value`, `store-value`, `continue`,
  `abort`) that run arbitrary recovery code. If a tool ever needs to offer a
  restart that performs a *host action* (not just inject a value), add a mechanism
  for a `ToolDef` to register named restart closures surfaced alongside
  resume/rewrite. `replace_file`'s `overwrite` (force last-write-wins) is the
  natural first customer — until then it is the one-call override in Step 3
  (re-call with the returned version), so this is not needed. Do **not**
  approximate it with a versionless/`force` write tool: that is the blind-clobber
  hole decision 3 removed.

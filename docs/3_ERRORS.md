# Phase 3 — Runtime errors & condition-system integration

Goal: a runtime failure produces a source-anchored, value-bearing report good
enough for an LLM to repair the program in one shot — and, where sound, the
host can *resume* execution instead of restarting. The condition system's
"continue with value" restart should work for ordinary runtime errors, not
just explicit `raise`.

**Design decision (locked):** keep `Result<T, VMError>` as the internal
channel so `?` keeps doing the control flow. The VM never decides policy; it
returns enriched errors. The *host* (the LLM loop) decides whether an error
becomes a condition with restarts. Do not thread `StepResult` through
internal helpers.

## Execution protocol (read first)

Steps land **strictly in order**, one commit per step (`errors:` prefix),
with `cargo fmt && cargo clippy && cargo test` green before each commit.
Every step ends with an **Acceptance** checklist. Run each check literally
(the greps are commands, not suggestions) and paste the results into the
commit message. **A step is not done until every one of its acceptance
checks passes, and the next step must not be started before that.** If a
check cannot pass, stop and record why in this doc next to the check — do
not reinterpret the check, and do not skip ahead.

## Step 1: VM retains spans and source

`VM::for_program` currently drops `Program.spans` and `Program.source`, so
runtime errors cannot be rendered against source. Store both on the `VM`
(`spans: Vec<u32>`, `source: Arc<str>`; both cheap). `VM::new(code)` (the
hand-assembled-instruction path used by tests) leaves them empty and
rendering degrades gracefully to "at ip N".

**Acceptance (Step 1):**
- `VM` has `spans` + `source` fields; `for_program` populates them from the
  `Program`; `VM::new` leaves them empty.
- A test asserts that after `for_program`, `vm.spans.len() == vm.code.len()`.
- No other behavior change; full suite green.

## Step 2: Enrich `VMError`

Replace the unit-variant enum with:

```rust
pub struct VMError {
    pub kind: ErrorKind,     // the current nine variants, renamed from VMError
    pub ip: CodeAddr,        // instruction that failed
    pub message: String,     // operation + operands, human/LLM-readable
    pub resume: ResumeMode,  // see Step 3
}
```

Mechanics:

- Add `impl VM { fn fail(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError }`
  which captures `self.ip` (and sets `resume` per Step 3). Builtins receive
  `&mut VM`, so they use the same helper.
- Migrate every error site (`ok_or(VMError::…)` → `ok_or_else(|| self.fail(…))`,
  ditto `return Err(…)`). This is large but mechanical; do it with generic
  messages first ("type error in Add"), commit, then improve messages
  (Step 4) separately.
- Add `impl VM { pub fn render_error(&self, e: &VMError) -> String }`: map
  `spans[e.ip]` to a `Diagnostic { span, message }` and reuse
  `Diagnostic::render` against `self.source`. Falls back to
  `"<message> (at instruction {ip})"` when spans/source are empty.
- `OutOfFuel` keeps a fixed message; it is not a program bug.
- **Test migration is part of this step**, not an afterthought: ~84
  `VMError::<Variant>` references in test code break when the enum becomes
  a struct. Decision (do not improvise an alternative mid-migration):
  `testutil::run_runtime_err` and the `vm/tests.rs` `run_err` helper keep
  returning the full `VMError`; assertions compare `err.kind` against
  `ErrorKind::…`. Add `testutil::run_err_kind(src) -> ErrorKind` as sugar
  for tests that only care about the kind.

**Acceptance (Step 2):**
- Every error construction goes through `fail`:
  `grep -rn 'VMError {' interp/src` hits only the `fail` helper itself.
- The unit variants are gone from call sites:
  `grep -rn 'VMError::' interp/src` → zero hits (the variants live on
  `ErrorKind` now).
- `render_error` has tests for both paths: a compiled program's error
  renders with the source line + caret; a `VM::new` program's error renders
  as "at instruction {ip}".
- **Which sites error is unchanged** — the pre-existing error tests pass
  with only the mechanical `.kind` rename, no expectation changes.

## Step 3: Resume classification

Add to the error:

```rust
pub enum ResumeMode {
    /// Internal invariant broken (compiler bug / host misuse). Never resume.
    NotResumable,
    /// The failed instruction's operands were consumed; pushing a
    /// replacement result and advancing ip resumes as if it succeeded.
    PushValueThenContinue,
    /// Nothing was consumed; fix the budget/input and step() again
    /// (ip unchanged). Currently: OutOfFuel.
    RetrySameInstr,
}
```

and the host-facing API:

```rust
impl VM {
    /// Apply the resume fixup for a PushValueThenContinue error:
    /// push `value`, advance ip past the failed instruction.
    pub fn resume_with(&mut self, e: &VMError, value: Value) -> Result<(), …>;
}
```

The core work is an **audit of every error site** for its stack state at the
moment of error:

- Classify `StackUnderflow`, `BadReturn`, `BadCall`, `BadLocal`, `BadAlloc`,
  and `BadArg` as `NotResumable` (these indicate compiler bugs or host
  misuse — codegen always produces balanced stacks and valid indices).
  That accounts for six of the nine kinds; `TypeError`/`ValueError` are
  audited per-site below, and `OutOfFuel` is `RetrySameInstr`.
- For each `TypeError`/`ValueError` site in `step()` and `builtin/mod.rs`,
  determine whether the instruction's operands are already popped when the
  error is constructed. The `unary_num!`/`binary_num!`/`binary_int!` macros
  and `take_args` pop first — those sites are `PushValueThenContinue` as-is.
  Sites that error **before** popping (peek-style checks, `IndexSet`
  inspecting the container, arity checks in `check_arity!`) must either be
  normalized to pop-first or classified `NotResumable` with a one-line
  comment saying why. **The invariant to establish and document:** a
  `PushValueThenContinue` error is only constructed after the instruction's
  full operand consumption, so `resume_with` needs no per-instruction stack
  fixup.
- Multi-result instructions: `Return`, `EnterFrame`, `Call*` mutate frames —
  classify their failure paths `NotResumable` (frame state is half-built).
- Record the audit as a table in a module doc comment in `vm/mod.rs`
  (instruction/site → classification → why), so future instruction authors
  follow the invariant.

Add tests per classification: e.g. `return [] - 1;` (TypeError in Sub) →
`resume_with(Float(0))` → program completes with the substituted value;
OutOfFuel → top up fuel → completes; a `NotResumable` resume attempt errors.

**Acceptance (Step 3):**
- All nine `ErrorKind`s have a classification; none is "unmentioned"
  (`BadArg` included).
- The audit table in the `vm/mod.rs` module doc has one row per error
  site: row count matches `grep -rc 'fail(' interp/src/vm interp/src/builtin`
  (state the two numbers in the commit message; explain any delta).
- The pop-first invariant is documented at the `ResumeMode` definition.
- At least one test per `ResumeMode` variant exists, including a
  `resume_with` attempt on a `NotResumable` error returning an error.
- Every pop-first normalization made during the audit has its own test and
  its own audit-table row (per the ground rules).

## Step 4: Message quality

Improve messages at the high-traffic sites with operation + operand types +
short value previews:

- Add `pub(crate) fn type_name(v: &Value) -> &'static str` ("undefined",
  "null", "boolean", "number", "string", "array", "object", "function") and
  `fn preview(vm: &VM, v: &Value) -> String` (strings quoted + truncated to
  ~40 chars, arrays/objects as `[array of 12]` / `{object with keys a, b, …}`).
- Target sites, in priority order: arithmetic/comparison coercion failures,
  `IndexGet`/`IndexSet` (wrong container type, negative index, OOB write,
  mid-codepoint), `ObjGet`/`ObjSet` on non-objects, `CallDyn` on a
  non-callable, builtin arity/type failures (use `BuiltinMeta::name`),
  `Invoke` result-shape errors, JSON depth/cycle errors.
- Example target quality:
  `cannot subtract: left operand is an array ([array of 3]), right is number (1)`
  rendered under the source line with a caret.

**Acceptance (Step 4):**
- For **each** target site in the priority list above, at least one test
  asserts a message substring that includes the operand's type name (and
  the preview where one is specified). One test per site, listed in the
  commit message against the site it covers — a site with no test is not
  done.
- `type_name` and `preview` have direct unit tests (all `Value` variants;
  string truncation at the boundary).

## Step 5: `raise` payloads and the blessed resume path

- Extend `Instr::Raise(RcStr)` → `Raise(RcStr, ArgCount)`. Compiler: allow
  `raise("name")` and `raise("name", payload)` (payload = any expression;
  more than one extra arg is a compile error). The condition name must remain
  a string literal.
- `step()` pops the payload values and returns
  `StepResult::Raise { condition: String, payload: Option<Value> }`. ip is
  **advanced past the Raise** before returning (unlike today), and the
  documented contract becomes: the conceptual stack effect of `Raise` is
  `(payload?) -> result`; the host resumes by pushing one result value and
  calling `step()` again. Add `VM::resume_raise(value)` mirroring
  `resume_with` (push only — ip already advanced).
- Update the `StepResult::Raise` doc comment: remove the suggestion that the
  host may patch `code`/`ip` to arbitrary restart points. The supported
  restarts are (a) continue-with-value via `resume_raise`, and (b) abandon
  this VM and run a rewritten program in a fresh VM (prior tool results
  stay available to it via the event log — artifact ids / positional
  replay, see 8_HARNESS decisions 5–7). In-place code patching is
  explicitly unsupported (live `Fn`/`Closure` values hold code addresses
  that a recompile invalidates).
- Update existing Raise tests. The ones doing the manual `ip += 1` dance
  this step obsoletes are `raise_yields_effect_and_resumes_as_expression`
  (`compiler/tests/effects.rs`) and `invoke_interleaved_with_raise`
  (`vm/tests.rs`) — both must switch to `resume_raise`. Add payload
  round-trip and resume tests.

**Acceptance (Step 5):**
- No test manually fixes up ip after a Raise:
  `grep -rn 'ip += 1' interp/src` → zero hits in test code.
- Tests cover: `raise("name")` (no payload), `raise("name", expr)` (payload
  arrives in `StepResult::Raise`), `raise("name", a, b)` → compile error,
  non-literal condition name → compile error, and `resume_raise` feeding a
  value back as the expression result.
- The `StepResult::Raise` doc comment no longer suggests patching
  `code`/`ip`: `grep -n 'modify vm state' interp/src/vm/mod.rs` → zero hits.

## Ground rules

(Sequencing and commit discipline live in "Execution protocol" at the top.)

- Step 2's site migration must not change which sites error — only what the
  error carries. Step 3's pop-first normalizations are behavior-adjacent:
  each one needs a test showing the observable behavior (which error, what
  the stack looks like to `resume_with`) and a line in the audit table.

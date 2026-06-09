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

## Step 1: VM retains spans and source

`VM::for_program` currently drops `Program.spans` and `Program.source`, so
runtime errors cannot be rendered against source. Store both on the `VM`
(`spans: Vec<u32>`, `source: Arc<str>`; both cheap). `VM::new(code)` (the
hand-assembled-instruction path used by tests) leaves them empty and
rendering degrades gracefully to "at ip N".

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

- Classify `StackUnderflow`, `BadReturn`, `BadCall`, `BadLocal`, `BadAlloc`
  as `NotResumable` (these indicate compiler bugs — codegen always produces
  balanced stacks and valid indices).
- For each `TypeError`/`ValueError` site in `step()` and `builtin.rs`,
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

Add tests per classification: e.g. `state.x = [] - 1` (TypeError in Sub) →
`resume_with(Float(0))` → program completes with the substituted value;
OutOfFuel → top up fuel → completes; a `NotResumable` resume attempt errors.

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
  this VM, persist `state_to_json()`, recompile a rewritten program, and
  re-seed a fresh VM (`for_program`). In-place code patching is explicitly
  unsupported (live `Fn`/`Closure` values hold code addresses that a
  recompile invalidates).
- Update existing Raise tests; add payload round-trip and resume tests.

## Ground rules

- Steps land in order; each is a separate commit (`errors:` prefix) with
  green `cargo fmt && cargo clippy && cargo test`.
- Step 2's site migration must not change which sites error — only what the
  error carries. Step 3's pop-first normalizations are behavior-adjacent:
  each one needs a test showing the observable behavior (which error, what
  the stack looks like to `resume_with`) and a line in the audit table.

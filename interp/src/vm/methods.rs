use super::*;
use crate::js_string::keys;

use crate::compiler::{ConstVal, namespace_constants};
use crate::diag::Diagnostic;

/// How much of an uncaught thrown string reaches the reader. Long
/// enough for the harness's refusals, which are a short paragraph and
/// exist to say what to write instead.
const UNCAUGHT_MESSAGE_MAX_BYTES: usize = 2048;

/// How far a `[[Prototype]]` walk goes before giving up. A cap, not a limit
/// anyone should reach: it exists so a malformed chain (a cycle built by a
/// bad `Object.setPrototypeOf`) ends a property read instead of hanging the
/// VM.
const MAX_PROTO_DEPTH: u32 = 100;

/// JS `Error.prototype.toString`: `name`, `": "`, `message` — dropping the
/// separator when either half is empty, so a nameless error renders as its
/// message alone and a messageless one as its bare name.
///
/// Spelled out here rather than in the formatter because both string writers
/// need it and neither can call into JS to run a `toString`.
fn error_to_string(name: &str, message: &str) -> String {
    match (name.is_empty(), message.is_empty()) {
        (true, _) => message.to_owned(),
        (false, true) => name.to_owned(),
        (false, false) => format!("{name}: {message}"),
    }
}

/// Whether a value is an *object* for `instanceof` purposes (Step 2b). JS
/// `instanceof` spec: "If Type(relObj) is not Object, return false." The
/// "object" types are the heap/structural types (Object/Array/Map/Set/
/// RegExp/Closure/Builtin/Bound/Promise); primitives (String/Number/Bool/
/// Null/Undefined) and the internal `Upval` marker are not. This gates the
/// proto-chain walk in `instanceof` — `value_proto` returns a prototype for
/// primitives too (the wrapper type's, for `Object.getPrototypeOf`), so the
/// walk must be gated on this check, not on `value_proto` returning `Some`.
fn is_object_for_instanceof(val: &Value) -> bool {
    matches!(
        val,
        Value::Object(_)
            | Value::Array(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::RegExp(_)
            | Value::Closure { .. }
            | Value::Builtin(_)
            | Value::Bound(_)
            | Value::Promise(_)
            | Value::TypedArray(_)
            | Value::DataView(_)
            | Value::ArrayBuffer(_)
    )
}

impl VM {
    pub fn new(code: Vec<Instr>) -> Self {
        VM {
            finished: false,
            code,
            arrays: Vec::new(),
            objects: Vec::new(),
            closures: Vec::new(),
            maps: Vec::new(),
            sets: Vec::new(),
            buffers: Vec::new(),
            typed_arrays: Vec::new(),
            data_views: Vec::new(),
            cells: Vec::new(),
            promises: Vec::new(),
            outbox: Vec::new(),
            continuations: Vec::new(),
            ready: VecDeque::new(),
            root_ip: 0,
            inflight: 0,
            settling: false,
            settle_uncaught: None,
            handlers: Vec::new(),
            stack: Vec::new(),
            // Root frame so that Local is valid from the start.
            callstack: vec![CallFrame {
                arg_count: 0,
                local_count: 0,
                return_addr: 0,
                prev_fp: 0,
                arguments_cache: None,
                pending_closure: u32::MAX,
                this_val: Value::Undefined,
                new_obj: None,
                reclaim_below: 0,
                completion: Completion::Normal,
            }],
            ip: 0,
            fp: 0,
            cur_local_count: 0,
            spans: Vec::new(),
            source: Arc::from(""),
            console_lines: Vec::new(),
            debug: crate::debuginfo::DebugTable::default(),
            prototypes: Vec::new(),
            namespaces: Vec::new(),
        }
    }

    /// The VM is broken: a dangling heap pointer, bytecode the compiler
    /// should not have emitted, a host API called out of order. Neither
    /// resumable nor catchable.
    ///
    /// **`kind` is a label here, not a classification.** Many of these sites
    /// say `TypeError` or `ValueError` because that is what they said before
    /// there was anywhere else to put them, and it does not matter: an
    /// `InvariantViolation` never becomes a JS value, so no program ever
    /// reads the name. What matters is that it does not reach a `catch` —
    /// `fail`'s per-kind default would make a `ValueError` resumable *and*
    /// catchable, and a program that swallowed a corrupt-heap report would
    /// carry on over the wreckage.
    pub fn fail_invariant(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError {
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume: ResumeMode::InvariantViolation,
            payload: None,
        }
    }

    /// A language error the host cannot resume, because the failed
    /// instruction consumed nothing and so owes the stack no result — but
    /// an ordinary error to the *program*, which may `catch` it.
    ///
    /// The whole population is `IncLocal` (`x++`/`x--`), which reads its
    /// local by peek, and `Throw` with no handler, which owes a statement
    /// no value. Both were `NotResumable` and therefore uncatchable, and
    /// for `IncLocal` that meant `try { x--; } catch (e) {}` around a
    /// non-number died uncaught.
    pub fn fail_no_result_slot(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError {
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume: ResumeMode::NoResultSlot,
            payload: None,
        }
    }

    /// Construct an error at the current instruction pointer. Every runtime
    /// error site goes through this (or `fail_invariant` / the static
    /// `VMError::fail_at`) so `ip` and `resume` are captured consistently.
    /// A type error that says what it wanted and what it got.
    ///
    /// **A trap the model cannot act on is a trap it restarts around.**
    /// `compiler/tests/lang_basics.rs` pins the wording of the
    /// `dispatch.rs` messages for exactly this reason, naming the live
    /// incident of 2026-09-15 where a bare `"type error"` left a
    /// program unable to tell what had failed. The builtins were never
    /// brought under that rule and kept 44 of them — one of which,
    /// `slice` on an object, is the whole of what a run on 2026-09-19
    /// was told when it wrote `history.fetch(46).slice(0, 2000)`
    /// against a `{ content, version }` result. It guessed right, and
    /// paid a round trip to do it.
    ///
    /// `describe_operand` bounds what it prints — a structural summary
    /// for objects and arrays, never their contents — so this is safe
    /// on a multi-megabyte value.
    pub fn type_error(&self, expected: &str, got: &Value) -> VMError {
        self.fail(
            ErrorKind::TypeError,
            format!("expected {expected}, got {}", self.describe_operand(got)),
        )
    }

    pub fn fail(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError {
        let resume = match kind {
            // Invariant violations: compiler bug or host misuse — never resume.
            ErrorKind::StackUnderflow
            | ErrorKind::BadReturn
            | ErrorKind::BadCall
            | ErrorKind::BadAlloc
            | ErrorKind::BadArg
            | ErrorKind::BadLocal => ResumeMode::InvariantViolation,
            // The language kinds: most sites pop their operands first
            // (macros, take_args, check_arity!), so the default is
            // resumable. A site that errors before popping overrides with
            // `fail_no_result_slot`, and a corrupt-heap check with
            // `fail_invariant` — the kind alone cannot tell them apart,
            // which is why those two constructors exist.
            ErrorKind::TypeError
            | ErrorKind::ValueError
            | ErrorKind::ReferenceError
            | ErrorKind::RangeError
            | ErrorKind::SyntaxError => ResumeMode::Resumable,
            // An escaped program-level throw: the operand was consumed, but
            // a `throw` owes the stack no result a substituted value could
            // fill. Catchable in principle and never caught in fact — it is
            // built only once the handler search has already failed.
            ErrorKind::UncaughtException => ResumeMode::NoResultSlot,
            // Circular awaits: every strand is parked, so there is no
            // execution state a substituted value could resume — and none
            // for a `catch` to continue into either.
            ErrorKind::Deadlock => ResumeMode::InvariantViolation,
        };
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume,
            payload: None,
        }
    }

    /// The error for a method builtin whose receiver (arg 0) is not the type it
    /// handles. Returns a `TypeError` — the `CallBuiltin` and `dispatch_call`
    /// sites intercept Object receivers before calling the builtin to resolve
    /// the method via the unified own-properties → proto-chain walk, so a
    /// builtin handler should never see an Object receiver. Every method-builtin
    /// receiver check routes its mismatch arm through here (directly or via the
    /// `Args::*_receiver` helpers).
    /// A method called on something it does not work on.
    ///
    /// **It had the receiver all along and threw it away.** The
    /// parameter was `_recv` and the message was the bare word "type
    /// error", so `history.fetch(46).slice(0, 2000)` against a
    /// `{ content, version }` result told a live run of 2026-09-19
    /// exactly this and nothing else:
    ///
    /// ```text
    /// in `slice`: type error
    /// ```
    ///
    /// It guessed `.content`, guessed right, and paid a round trip for
    /// it. `compiler/tests/lang_basics.rs` already pins the `dispatch.rs`
    /// messages against precisely this regression — naming the live
    /// incident of 2026-09-15, where a placeholder left a program
    /// unable to tell what had failed and it restarted instead of
    /// resuming. The builtins were never brought under that rule.
    ///
    /// The caller passes what the method *does* accept, because this
    /// function serves thirty-six of them and cannot know.
    /// `describe_operand` bounds what it prints — a structural summary,
    /// never contents — so this is safe on a huge value.
    pub(crate) fn method_receiver_error(&self, recv: &Value, accepts: &str) -> VMError {
        self.fail(
            ErrorKind::TypeError,
            format!("expected {accepts}, got {}", self.describe_operand(recv)),
        )
    }

    /// Resume after a `Raise`: push the host-chosen result value (ip was
    /// already advanced past the Raise by `step()`).
    pub fn resume_raise(&mut self, value: Value) {
        self.stack.push(value);
    }

    /// Answer the outstanding `StepResult::Settle` with its value: push it
    /// where the call's arguments were (ip was already advanced past the
    /// `Settle` by `step()`) and let the frame carry on. The mirror of
    /// `resume_raise`, and the whole point of `Instr::Settle` — the call
    /// returns a value without ever having been a promise.
    ///
    /// A `Settle` is answered exactly once; answering when none is
    /// outstanding is host misuse and errors without touching the stack,
    /// because pushing there would corrupt the frame it landed in.
    pub fn push_settled(&mut self, value: Value) -> Result<(), VMError> {
        if !self.settling {
            return Err(self.fail_invariant(
                ErrorKind::BadArg,
                "push_settled without an outstanding Settle",
            ));
        }
        self.settling = false;
        self.stack.push(value);
        Ok(())
    }

    /// Fail the outstanding `StepResult::Settle`: throw `errval` into the
    /// frame that made the call, at the call site. Unlike a rejected
    /// promise — which belongs to whoever awaits it, and which a *sync*
    /// caller could never catch — this is an ordinary throw, so a
    /// `try`/`catch` around the call sees it and an uncaught one escapes
    /// to the caller the way any other failed call in a sync function
    /// does.
    ///
    /// The three destinations are `Instr::Throw`'s own, in its order,
    /// because a failed call is a throw and should not get a second set
    /// of rules: a reachable handler catches it; failing that, an
    /// enclosing async call's promise rejects (the failure belongs to
    /// whoever awaits that call, not to the parked code below it); and
    /// failing that it is uncaught, recorded for the next `step` to
    /// raise as the program's trap.
    ///
    /// The value slot the call owed the stack is never filled, and never
    /// needs to be: unwinding truncates the stack to the handler's
    /// snapshot, rejecting a strand discards its frames, and an uncaught
    /// throw ends the program before another instruction runs.
    pub fn settle_throw(&mut self, errval: Value) -> Result<ThrowOutcome, VMError> {
        if !self.settling {
            return Err(self.fail_invariant(
                ErrorKind::BadArg,
                "settle_throw without an outstanding Settle",
            ));
        }
        self.settling = false;
        if self.reachable_handler() {
            self.unwind_to_handler(errval);
            return Ok(ThrowOutcome::Caught);
        }
        if self.in_strand() {
            self.reject_strand(errval.clone())?;
            return Ok(ThrowOutcome::Caught);
        }
        self.settle_uncaught = Some(errval.clone());
        Ok(ThrowOutcome::Uncaught(errval))
    }

    /// Unwind to the innermost `try` handler with `value` as the thrown
    /// value: pop the handler entry, truncate the value/call stacks to its
    /// snapshot, restore `fp` (and the local-count mirror), push `value`
    /// (the catch binding), and jump to the catch address. Returns `false` —
    /// consuming `value` — when no handler is active; callers that need the
    /// value back on failure check `handlers` first (see `throw_value`).
    pub(super) fn unwind_to_handler(&mut self, value: Value) -> bool {
        let Some(h) = self.handlers.pop() else {
            return false;
        };
        self.stack.truncate(h.stack_len);
        self.callstack.truncate(h.callstack_len);
        self.fp = h.fp;
        // The handler's frame is intact (TryEnter ran inside it after its
        // EnterFrame), so its local_count is current.
        self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
        self.stack.push(value);
        self.ip = h.catch_ip;
        true
    }

    /// Host API: throw `value` into the program — the bridge between tool
    /// failures and program-level handling. Unwinds to the nearest `try`
    /// handler (resume with `step()`), or hands the value back as
    /// `Uncaught` so the host can escalate per its policy.
    pub fn throw_value(&mut self, value: Value) -> ThrowOutcome {
        if self.handlers.is_empty() {
            ThrowOutcome::Uncaught(value)
        } else {
            self.unwind_to_handler(value);
            ThrowOutcome::Caught
        }
    }

    /// Materialize a catchable VM error as the `{ name, message }` object a
    /// `catch` binding receives: `name` from the error kind, `message` the
    /// fully rendered diagnostic (line/col + source line). Built by
    /// [`Self::alloc_error`], so a `TypeError` the VM raised is `instanceof
    /// Error` exactly like one the program wrote itself.
    pub(super) fn error_to_thrown(&mut self, e: &VMError) -> Value {
        let name = JsString::from(format!("{:?}", e.kind).as_str());
        let message = JsString::from(self.render_error(e).as_str());
        self.alloc_error(name, message)
    }

    /// The `{ name: "TypeError", message }` object a promise-chaining cycle
    /// rejects with (the scheduler-side mirror of the `Await` cycle check).
    fn cycle_error_value(&mut self, msg: &str) -> Value {
        self.alloc_error(JsString::from("TypeError"), JsString::from(msg))
    }

    /// Render an uncaught thrown value: an `{ name, message }` error object
    /// (the `new Error(...)` shape) formats as `uncaught {name}: {message}`;
    /// anything else falls back to a preview of the value.
    pub(super) fn uncaught_message(&self, value: &Value) -> String {
        if let Value::Object(p) = value
            && let Some(obj) = self.objects.get(*p as usize)
            && let (Some(Value::String(name)), Some(Value::String(msg))) =
                (obj.map.get(keys::NAME), obj.map.get(keys::MESSAGE))
        {
            return format!("uncaught {name}: {msg}");
        }
        // **A thrown string is a message, not a value being previewed.**
        // This used to go through `preview`, which cuts a string at 40
        // bytes — right for naming a value inside some *other* error,
        // fatally wrong here, because an uncaught throw is the last
        // thing the program gets to say.
        //
        // The harness refuses a bad call by throwing its explanation as
        // a string (`Runner::settle_err`), so every one of those
        // reached the model as its first 40 characters. Live on
        // 2026-09-24: a program that called `history.keep` with an
        // array was handed
        //
        //   uncaught exception: "keep_history(result) needs a tool result…"
        //
        // and none of the sentence that says to map `keep` over the
        // rows instead. The refusal had been written that morning
        // precisely so it would teach.
        //
        // Bounded, because a program may throw something bulky, but
        // bounded where a paragraph fits rather than where a phrase
        // does. The `{name, message}` branch above is not truncated at
        // all; this is the same treatment for the same kind of text.
        if let Value::String(s) = value {
            let s = &s.to_utf8_lossy();
            if s.len() <= UNCAUGHT_MESSAGE_MAX_BYTES {
                return format!("uncaught exception: \"{}\"", s.escape_debug());
            }
            let mut end = UNCAUGHT_MESSAGE_MAX_BYTES;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            return format!(
                "uncaught exception: \"{}…\" [truncated; {} bytes total]",
                s[..end].escape_debug(),
                s.len()
            );
        }
        format!("uncaught exception: {}", self.preview(value))
    }

    /// Apply the resume fixup for a [`ResumeMode::Resumable`] error: push
    /// `value`, advance ip past the failed instruction.
    ///
    /// Errors for the other two modes, and for different reasons — a
    /// `NoResultSlot` error has nowhere to put the value, an
    /// `InvariantViolation` has nothing trustworthy to continue on.
    pub fn resume_with(&mut self, e: &VMError, value: Value) -> Result<(), VMError> {
        if !e.resume.is_resumable() {
            return Err(self.fail(
                ErrorKind::BadArg,
                format!(
                    "cannot resume: error {:?} is {:?}, not Resumable",
                    e.kind, e.resume
                ),
            ));
        }
        self.stack.push(value);
        self.ip = e.ip + 1;
        Ok(())
    }

    /// Human-readable preview of a value for error messages. Strings are
    /// quoted and truncated to ~40 chars; arrays/objects show a summary
    /// like `[array of 12]` / `{object with keys a, b, …}`.
    pub fn preview(&self, v: &Value) -> String {
        match v {
            Value::String(s) => {
                let s = &s.to_utf8_lossy();
                if s.len() <= 42 {
                    format!("\"{}\"", s.escape_debug())
                } else {
                    // Floor the cut to a char boundary: a byte slice at 40
                    // panics if it lands inside a multi-byte codepoint.
                    let mut end = 40;
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("\"{}…\"", s[..end].escape_debug())
                }
            }
            Value::Array(p) => {
                if let Some(arr) = self.arrays.get(*p as usize) {
                    format!("[array of {}]", arr.len())
                } else {
                    "[array]".to_string()
                }
            }
            Value::Object(p) => {
                if let Some(obj) = self.objects.get(*p as usize) {
                    let mut keys: Vec<String> = obj.map.keys().map(|k| k.to_string()).collect();
                    keys.sort();
                    if keys.len() <= 4 {
                        format!("{{object with keys {}}}", keys.join(", "))
                    } else {
                        format!("{{object with keys {}, …}}", keys[..3].join(", "))
                    }
                } else {
                    "{object}".to_string()
                }
            }
            Value::RegExp(r) => {
                format!("/{}/{}", r.pattern, r.flags)
            }
            other => other.type_name().to_string(),
        }
    }

    /// Render an error against the VM's source (when available): `spans[ip]`
    /// is a byte range, so this underlines the whole offending expression
    /// rather than just caret its first byte (a zero-width span still
    /// renders as a single caret — see [`Diagnostic::render`]). Falls back
    /// to a plain "at instruction {ip}" format when spans/source are empty
    /// (hand-assembled code via `VM::new`).
    pub fn render_error(&self, e: &VMError) -> String {
        let ip = e.ip as usize;
        if ip < self.spans.len() && !self.source.is_empty() {
            let span = self.spans[ip];
            let diag = Diagnostic {
                span,
                message: e.message.clone(),
                kind: crate::diag::DiagKind::Semantic,
            };
            diag.render(&self.source)
        } else {
            format!("{} (at instruction {ip})", e.message)
        }
    }

    /// Construct a VM to run a compiled `Program`, seeding the host const
    /// `input` (the frame's caller-provided JSON) at `objects[0]` and an
    /// empty `attachments`. See [`for_program_with`](Self::for_program_with).
    pub fn for_program(program: Program, input: serde_json::Value) -> Result<Self, VMError> {
        Self::for_program_with(program, input, serde_json::Value::Null)
    }

    /// Construct a VM with both host-seeded read-only consts: `input` (the
    /// frame's caller-provided JSON) at `objects[0]`, and `attachments`
    /// (this run's authored content) at `objects[1]`. Each `Null`/non-object
    /// seed yields an empty object. Both slots are reserved up front, before
    /// any nested seeding, so the compiler's fixed `Object(0)`/`Object(1)`
    /// references stay stable; nested values land at `objects[2..]`.
    pub fn for_program_with(
        program: Program,
        input: serde_json::Value,
        attachments: serde_json::Value,
    ) -> Result<Self, VMError> {
        let mut vm = VM::new(program.code);
        vm.install_const_fn_closures(0, &mut std::collections::HashMap::new());
        vm.spans = program.spans;
        vm.source = program.source;
        vm.debug = program.debug;
        vm.seed_host_consts(input, attachments)?;
        Ok(vm)
    }

    /// A VM with **no code**, ready to be fed fragments by an incremental
    /// evaluator. The host-seeded consts are in place, so `input` and
    /// `attachments` resolve from the first fragment onward; `code`, `spans`
    /// and `source` grow as fragments are appended.
    ///
    /// The root frame `VM::new` installs is the frame every fragment shares —
    /// it is never unwound between them, which is the whole point.
    pub fn for_incremental(
        input: serde_json::Value,
        attachments: serde_json::Value,
    ) -> Result<Self, VMError> {
        let mut vm = VM::new(Vec::new());
        vm.seed_host_consts(input, attachments)?;
        Ok(vm)
    }

    /// Allocate one canonical `Closure` per unique `PushFn` code address in
    /// `code[from..]`, recording the mapping in `canonical`.
    ///
    /// `PushFn` is emitted only for **const-fns** (Step 2e), which are
    /// single-identity: a `function F(){}` declaration is one function
    /// object, so every value-reference must resolve to the same `ptr`
    /// (baked into the instruction here; no runtime addr→ptr map). This is
    /// what keeps `F === F`, a shared `.prototype`, and `new F() instanceof
    /// F` correct. Genuine per-evaluation function values (expressions,
    /// non-const declarations, capturing closures) use `ClosureNew`, which
    /// allocates a fresh entry each time for JS per-instance identity.
    ///
    /// `from` and a caller-owned `canonical` are what let this run again over
    /// appended code: an incremental evaluator keeps the map alive across
    /// fragments, so a `PushFn` in a later fragment naming a const-fn from an
    /// earlier one resolves to the closure already allocated for it.
    pub(crate) fn install_const_fn_closures(
        &mut self,
        from: usize,
        canonical: &mut std::collections::HashMap<CodeAddr, ClosurePtr>,
    ) {
        for i in from..self.code.len() {
            if let Instr::PushFn(addr, _, arity) = self.code[i] {
                let cptr = *canonical.entry(addr).or_insert_with(|| {
                    let idx = self.closures.len() as ClosurePtr;
                    self.closures.push(Closure {
                        upvals: ThinVec::new(),
                        prototype: None,
                        arity,
                        props: None,
                    });
                    idx
                });
                if let Instr::PushFn(_, ptr, _) = &mut self.code[i] {
                    *ptr = cptr;
                }
            }
        }
    }

    /// Install `input` (`objects[0]`) and `attachments` (`objects[1]`), the
    /// two host-seeded read-only consts every program sees by name.
    fn seed_host_consts(
        &mut self,
        input: serde_json::Value,
        attachments: serde_json::Value,
    ) -> Result<(), VMError> {
        // Reserve the two fixed slots before seeding either's nested values.
        self.objects.push(ObjData {
            proto: None,
            map: IndexMap::new(),
            ..Default::default()
        }); // objects[0] = input
        self.objects.push(ObjData {
            proto: None,
            map: IndexMap::new(),
            ..Default::default()
        }); // objects[1] = attachments
        let input_entries = self.seed_const_object(input)?;
        let attachment_entries = self.seed_const_object(attachments)?;
        // Step 2b: chain the host-seeded objects to `Object.prototype`,
        // matching JS (`Object.getPrototypeOf(input) === Object.prototype`
        // for a parsed JSON object). The prototype is allocated after
        // seeding (so it lands at a stable index beyond the nested values),
        // and the fixed `Object(0)`/`Object(1)` references are untouched —
        // only the `proto` field is set.
        let object_proto = self.prototype_for(crate::vm::instr::TypeTag::Object)?;
        self.objects[0] = ObjData {
            proto: Some(object_proto),
            map: input_entries,
            ..Default::default()
        };
        self.objects[1] = ObjData {
            proto: Some(object_proto),
            map: attachment_entries,
            ..Default::default()
        };
        Ok(())
    }

    /// Build the entry map for a host-seeded const from a JSON object;
    /// nested arrays/objects allocate into `objects` (addresses computed at
    /// runtime). A `Null`/non-object seed yields an empty map.
    fn seed_const_object(
        &mut self,
        json: serde_json::Value,
    ) -> Result<IndexMap<JsString, Value>, VMError> {
        let serde_json::Value::Object(map) = json else {
            return Ok(IndexMap::new());
        };
        let mut entries = IndexMap::with_capacity(map.len());
        for (k, v) in &map {
            let sv = self.json_to_stack_value(v, 0)?;
            entries.insert(JsString::from(k.as_str()), sv);
        }
        Ok(entries)
    }

    // ── debugger introspection (9_TUI Step 1) ────────────────────────

    /// The function owning the instruction at `ip` (debug-table index and
    /// entry): the innermost function whose source span contains
    /// `spans[ip]`. `None` without debug info (`VM::new` programs).
    pub fn function_at(&self, ip: CodeAddr) -> Option<(usize, &crate::debuginfo::FnDebug)> {
        let span = *self.spans.get(ip as usize)?;
        // Containment is checked against the instruction's start — the debug
        // table's function ranges are keyed the same way (by node start).
        let idx = self.debug.function_at_span(span.start)?;
        Some((idx, &self.debug.functions[idx]))
    }

    /// The call-site spelling of the prelude helper the instruction
    /// pointer is inside — `.map`, `Array.from` — or `None` in the
    /// user's own code.
    ///
    /// Used to name a trap that has no user source to point at; see
    /// [`crate::prelude::call_site_spelling`].
    pub(crate) fn inside_prelude_helper(&self) -> Option<&'static str> {
        let (_, f) = self.function_at(self.ip)?;
        crate::prelude::call_site_spelling(f.name.as_str())
    }

    /// Read-only views of the live call frames, outermost (root) first.
    /// Parked async continuations are not on the callstack and do not
    /// appear; this is the running strand's stack.
    /// Each live frame's **return address**, innermost first — where
    /// execution resumes in the caller once that frame returns.
    ///
    /// **This is how a failure inside the runtime gets a position in
    /// the caller's source.** A builtin raising from a helper the
    /// prelude calls — `String.prototype.replace` reaching
    /// `__replaceStr` — reports an `ip` inside the prelude, which is
    /// nobody's code: the host rebases it against the reply and gets
    /// zero, so the diagnostic points at no line at all. Walking out
    /// through these finds the first address that *is* in the caller's
    /// own text, which is the line they can act on.
    pub fn frame_return_addrs(&self) -> Vec<u32> {
        self.callstack.iter().rev().map(|f| f.return_addr).collect()
    }

    pub fn frames(&self) -> Vec<FrameView<'_>> {
        let n = self.callstack.len();
        // fp chain: the top frame's base is `self.fp`; each frame stores
        // the previous frame's base.
        let mut fps = vec![0usize; n];
        let mut fp = self.fp;
        for i in (0..n).rev() {
            fps[i] = fp as usize;
            fp = self.callstack[i].prev_fp;
        }
        (0..n)
            .map(|i| {
                let floor = (fps[i] + self.callstack[i].local_count as usize).min(self.stack.len());
                let ceil = if i + 1 < n {
                    fps[i + 1]
                } else {
                    self.stack.len()
                }
                .max(floor);
                // A frame's code position: the resume address stored by the
                // call above it; the top frame is at `self.ip`.
                let code_pos = if i + 1 < n {
                    self.callstack[i + 1].return_addr
                } else {
                    self.ip
                };
                let fn_at = self.function_at(code_pos);
                FrameView {
                    fn_index: fn_at.map(|(idx, _)| idx),
                    fn_debug: fn_at.map(|(_, f)| f),
                    fp: fps[i],
                    locals: &self.stack[fps[i].min(floor)..floor],
                    temps: &self.stack[floor..ceil],
                }
            })
            .collect()
    }

    /// Render instructions `[start, end)` as `ip  instr  @line`, with a
    /// `── name ──` header wherever the owning function changes — function
    /// names at function block starts, for the disassembly pane.
    pub fn disasm(&self, start: CodeAddr, end: CodeAddr) -> String {
        let end = end.min(self.code.len() as CodeAddr);
        let mut out = String::new();
        let mut cur_fn = usize::MAX;
        for ip in start..end {
            if let Some((idx, f)) = self.function_at(ip)
                && idx != cur_fn
            {
                out.push_str("── ");
                out.push_str(&f.name);
                out.push_str(" ──\n");
                cur_fn = idx;
            }
            out.push_str(&self.disasm_line(ip));
            out.push('\n');
        }
        out
    }

    /// One disassembly line: `ip  instr  @line` (the line annotation is
    /// omitted without source/spans).
    pub fn disasm_line(&self, ip: CodeAddr) -> String {
        let instr = match self.code.get(ip as usize) {
            Some(i) => format!("{i:?}"),
            None => return format!("{ip:>5}  <out of range>"),
        };
        match self.spans.get(ip as usize) {
            Some(&sp) if !self.source.is_empty() => {
                let (line, _, _) = crate::diag::line_col(&self.source, sp.start);
                format!("{ip:>5}  {instr:<32} @{line}")
            }
            _ => format!("{ip:>5}  {instr}"),
        }
    }

    // ── heap access helpers ──────────────────────────────────────────

    /// Lowest stack index the current frame's expression temporaries may
    /// occupy. Args live below `fp`, locals in `[fp, fp + local_count)`, and
    /// temporaries above that. Stack-manipulation ops (Pick/Dig/Nip/Pop) must
    /// not reach below this floor into locals, args, or the caller's stack.
    pub(super) fn frame_floor(&self) -> usize {
        self.fp as usize + self.cur_local_count as usize
    }

    /// Settle a promise with its tool call's result. Host API: called between
    /// a `StepResult::Pending` yield and the next `step()`. Settling a
    /// promise that is not `Pending` (already settled, or a bad id) is host
    /// misuse and errors without changing anything.
    pub fn resolve_promise(&mut self, id: PromisePtr, value: Value) -> Result<(), VMError> {
        self.settle_and_wake(id, PromiseState::Resolved(value))?;
        self.inflight = self.inflight.saturating_sub(1);
        Ok(())
    }

    /// Settle a promise as rejected, with the error value the program's
    /// `await` will escalate. Same contract as [`VM::resolve_promise`].
    pub fn reject_promise(&mut self, id: PromisePtr, errval: Value) -> Result<(), VMError> {
        self.settle_and_wake(id, PromiseState::Rejected(errval))?;
        self.inflight = self.inflight.saturating_sub(1);
        Ok(())
    }

    /// How many continuation records this VM has ever created — i.e.
    /// how many times a frame has been suspended. Zero is the property
    /// the settle-at-dispatch verbs are supposed to have: the frame
    /// that made the call is still the one that receives the value.
    pub fn continuation_count(&self) -> usize {
        self.continuations.len()
    }

    /// How many promises this VM has ever allocated. A settle-at-dispatch
    /// call allocates none — that is the whole claim.
    pub fn promise_count(&self) -> usize {
        self.promises.len()
    }

    /// Allocate a fresh `Pending` promise. Used by `Instr::Invoke` (leaf tool
    /// promises) and by `Instr::AsyncEnter` (an async call's own promise,
    /// Tier 2).
    pub(super) fn alloc_promise(&mut self) -> PromisePtr {
        let id = self.promises.len() as PromisePtr;
        self.promises.push(PromiseState::Pending {
            waiters: Vec::new(),
        });
        id
    }

    /// Settle a promise and move its waiters onto the ready queue (FIFO).
    /// Every settlement — host APIs, async frame completion, strand
    /// rejection — routes through here, so cascading stays VM-side
    /// (commitment 3): the host only ever settles leaf tool promises.
    pub(super) fn settle_and_wake(
        &mut self,
        id: PromisePtr,
        settled: PromiseState,
    ) -> Result<(), VMError> {
        let payload = match &settled {
            PromiseState::Resolved(v) => ResumePayload::Resolved(v.clone()),
            PromiseState::Rejected(v) => ResumePayload::Rejected(v.clone()),
            PromiseState::Pending { .. } => {
                return Err(
                    self.fail_invariant(ErrorKind::BadArg, "cannot settle a promise to Pending")
                );
            }
        };
        match self.promises.get(id as usize) {
            Some(PromiseState::Pending { .. }) => {}
            Some(_) => {
                return Err(self.fail_invariant(
                    ErrorKind::BadArg,
                    format!("promise {id} is already settled"),
                ));
            }
            None => {
                return Err(self.fail_invariant(ErrorKind::BadArg, format!("bad promise id {id}")));
            }
        }
        let old = std::mem::replace(&mut self.promises[id as usize], settled);
        let PromiseState::Pending { waiters } = old else {
            unreachable!("checked Pending above");
        };
        for w in waiters {
            self.ready.push_back((w, payload.clone()));
        }
        Ok(())
    }

    // ── Tier 2: suspend / resume / schedule ──────────────────────────

    /// Callstack index of the innermost async frame: the base of the strand
    /// a throw would escape into, and the owner of the promise that throw
    /// rejects. Everything at or above `base` is that call's own execution
    /// (the async frame plus any sync calls it made); everything below
    /// belongs to someone else — the parked root, or a caller that already
    /// took the call's promise and moved on.
    ///
    /// Scanning from the top, rather than asking whether `callstack[1]` is a
    /// resumed frame, is what makes a *directly called* async function a
    /// strand at all: it sits at whatever depth its caller happens to be at,
    /// and a body that throws before it ever suspends never becomes a
    /// scheduler-resumed frame. With the narrower test, such a throw escaped
    /// to the caller instead of rejecting.
    pub(super) fn strand_base(&self) -> Option<usize> {
        self.callstack
            .iter()
            .rposition(|f| f.completion.promise().is_some())
    }

    /// Whether execution is inside an async call's own frames — i.e. whether
    /// there is a promise for an escaping throw to reject.
    pub(super) fn in_strand(&self) -> bool {
        self.strand_base().is_some()
    }

    /// Whether the innermost `try` handler may catch a throw from the
    /// current position. Handlers belonging to frames *below* the innermost
    /// async frame are walled off: a throw escaping an async call rejects
    /// that call's promise, and must not unwind into the code below, which
    /// is either parked (a resumed strand sits above the root's own region)
    /// or has already moved past the call holding its promise. So
    /// `try { b(); } catch` around a call to an async `b` whose body throws
    /// catches nothing — in JS it catches nothing either, because the call
    /// returned a rejected promise instead of throwing.
    ///
    /// A `TryEnter` in frame `k` records `callstack_len == k + 1`, so "at or
    /// above the strand base `k`" is exactly `callstack_len > k`.
    pub(super) fn reachable_handler(&self) -> bool {
        match (self.handlers.last(), self.strand_base()) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(h), Some(base)) => h.callstack_len > base,
        }
    }

    /// The last step of every async-frame exit — `Return`, suspension, and
    /// rejection alike: the frame's region (and the caller's call group under
    /// it) is already off the stack, and the promise it settled now goes to
    /// whoever is waiting for it. A frame entered by a call hands the promise
    /// back as the call's value; a scheduler-resumed frame has nobody below
    /// it and falls through to the scheduler. Keeping the three exits on one
    /// rule is why a throw and a `return` leave an async call in the same
    /// shape.
    pub(super) fn leave_async_frame(
        &mut self,
        completion: Completion,
        return_addr: CodeAddr,
    ) -> Result<(), VMError> {
        match completion {
            Completion::AsyncCall(pid) => {
                self.stack.push(Value::Promise(pid));
                self.ip = return_addr;
                Ok(())
            }
            Completion::Resumed(_) => self.schedule(),
            Completion::Normal => Err(self.fail_invariant(
                ErrorKind::BadReturn,
                "async exit from a frame that owns no promise",
            )),
        }
    }

    /// Tier 2 suspension: at a pending `await` in an async function frame,
    /// copy the frame (stack region + metadata + its own handler entries)
    /// into a continuation record registered as a waiter on `awaiting`, and
    /// pop the frame, reusing the `Return` machinery. The frame leaves the
    /// way any async frame leaves (`leave_async_frame`): its promise —
    /// unsettled here, since the call has not finished — goes to the caller
    /// as the call's value, or, for a frame the scheduler resumed, control
    /// falls through to the scheduler.
    pub(super) fn suspend_current_frame(&mut self, awaiting: PromisePtr) -> Result<(), VMError> {
        // **Only an async frame can get here.** The frame has owned its
        // promise since its `AsyncEnter` prologue, so there is nothing to
        // mint: `await` is confined to async bodies by the parser, and the
        // one emitter that used to put an `Await` in a plain arrow — the
        // settle-at-dispatch verbs — emits `Settle` instead, which does not
        // suspend anything. A sync frame reaching here is a compiler bug,
        // not a case to handle: minting a promise for it would be inventing
        // a value the source never asked for, and would silently move a
        // throw that belongs to the caller onto a promise nobody awaits.
        //
        // Read before anything is torn down — past this point the frame is in
        // pieces and there is no coherent state to fail from.
        let Some(entered_as) = self.callstack.last().map(|f| f.completion) else {
            return Err(self.fail(ErrorKind::BadReturn, "suspend without a frame"));
        };
        let (promise, exit_as) = match entered_as {
            Completion::Normal => {
                return Err(self.fail_invariant(
                    ErrorKind::BadReturn,
                    "cannot suspend a frame that is not async",
                ));
            }
            Completion::AsyncCall(pid) | Completion::Resumed(pid) => (pid, entered_as),
        };
        // The Await consumes its operand: the promise leaves the stack now;
        // resume pushes the settled value in its place and continues past
        // the Await.
        self.stack.pop();
        let resume_ip = self.ip + 1;
        // `await_span` feeds `span_pos` (a line:col point, for await-chain
        // rendering) — only the start is meaningful there.
        let await_span = self
            .spans
            .get(self.ip as usize)
            .map(|s| s.start)
            .unwrap_or(0);
        // Split off this frame's own handler entries (a `TryEnter` in this
        // frame snapshots `callstack_len` == the current depth), storing
        // `stack_len` fp-relative so resume can re-base them.
        let depth = self.callstack.len();
        let fp = self.fp as usize;
        let mut saved_handlers: Vec<SavedHandler> = Vec::new();
        while self
            .handlers
            .last()
            .is_some_and(|h| h.callstack_len == depth)
        {
            let h = self.handlers.pop().unwrap();
            saved_handlers.push(SavedHandler {
                catch_ip: h.catch_ip,
                rel_stack_len: h.stack_len - fp,
            });
        }
        saved_handlers.reverse(); // outermost first: re-push order on resume
        let saved_stack: Vec<Value> = self.stack.split_off(fp);
        let frame = self
            .callstack
            .pop()
            .ok_or_else(|| self.fail(ErrorKind::BadReturn, "suspend without a frame"))?;
        self.fp = frame.prev_fp;
        self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
        // Reclaim the call-group slots the caller left just under `fp` (callee /
        // receiver, read in place rather than shifted away). `split_off(fp)` took
        // the frame's region; these placeholders are now the top of the stack.
        // A suspending call completes by returning a promise to the caller, so —
        // like `Return` — that promise must land at `fp - reclaim_below`.
        self.stack
            .truncate(self.stack.len() - frame.reclaim_below as usize);
        let cont_id = self.continuations.len() as u32;
        self.continuations.push(Some(Continuation {
            resume_ip,
            saved_stack,
            arg_count: frame.arg_count,
            local_count: frame.local_count,
            arguments_cache: frame.arguments_cache,
            saved_handlers,
            promise,
            awaiting,
            await_span,
        }));
        match self.promises.get_mut(awaiting as usize) {
            Some(PromiseState::Pending { waiters }) => waiters.push(cont_id),
            _ => {
                return Err(
                    self.fail_invariant(ErrorKind::ValueError, "bad awaited promise pointer")
                );
            }
        }
        self.leave_async_frame(exit_as, frame.return_addr)
    }

    /// The scheduler: resume the next woken continuation (deterministic
    /// FIFO), or — when nothing is ready — park back on the root strand's
    /// blocking `Await`, which re-executes (yielding to the host, or
    /// detecting deadlock). Called at await points and strand completion
    /// only, never preempting synchronous code.
    pub(super) fn schedule(&mut self) -> Result<(), VMError> {
        'next: while let Some((id, mut payload)) = self.ready.pop_front() {
            // Adoption (JS: a promise never resolves to a promise): a
            // continuation woken with a promise value re-waits on the
            // adoptee instead of resuming. Mirrors the `Await` chain-follow.
            let mut seen: Vec<PromisePtr> = Vec::new();
            while let ResumePayload::Resolved(Value::Promise(inner)) = payload {
                if seen.contains(&inner) {
                    let msg = "chaining cycle detected: promise resolves to itself";
                    payload = ResumePayload::Rejected(self.cycle_error_value(msg));
                    break;
                }
                seen.push(inner);
                match self.promises.get_mut(inner as usize) {
                    Some(PromiseState::Resolved(v)) => payload = ResumePayload::Resolved(v.clone()),
                    Some(PromiseState::Rejected(v)) => payload = ResumePayload::Rejected(v.clone()),
                    Some(PromiseState::Pending { waiters }) => {
                        waiters.push(id);
                        if let Some(Some(c)) = self.continuations.get_mut(id as usize) {
                            c.awaiting = inner; // keep await-chain rendering true
                        }
                        continue 'next;
                    }
                    None => {
                        return Err(self
                            .fail_invariant(ErrorKind::ValueError, "bad adopted promise pointer"));
                    }
                }
            }
            let cont = self
                .continuations
                .get_mut(id as usize)
                .and_then(Option::take)
                .ok_or_else(|| {
                    self.fail_invariant(ErrorKind::ValueError, format!("bad continuation id {id}"))
                })?;
            // A rejection arriving at a frame with no handler around its
            // await needs no frame materialization: the rejection
            // propagates straight to this call's own promise.
            if let ResumePayload::Rejected(errval) = &payload
                && cont.saved_handlers.is_empty()
            {
                self.settle_and_wake(cont.promise, PromiseState::Rejected(errval.clone()))?;
                continue;
            }
            self.resume_continuation(cont, payload);
            return Ok(());
        }
        // Nothing ready: the root's Await re-executes against its parked
        // region (the zero-stack invariant guarantees it is intact).
        self.ip = self.root_ip;
        Ok(())
    }

    /// Resume = re-push and jump: re-create the suspended frame at the
    /// current stack top in `Resumed` completion mode (it keeps the same
    /// promise, but has no caller below it now), re-base its handler entries,
    /// then push the resolved value and jump past the await — or unwind a
    /// rejection to the innermost re-based handler. No `EnterFrame` runs.
    fn resume_continuation(&mut self, cont: Continuation, payload: ResumePayload) {
        let new_fp = self.stack.len() as u32;
        self.stack.extend(cont.saved_stack);
        self.callstack.push(CallFrame {
            arg_count: cont.arg_count,
            local_count: cont.local_count,
            return_addr: 0,
            prev_fp: self.fp,
            pending_closure: u32::MAX,
            arguments_cache: cont.arguments_cache,
            this_val: Value::Undefined,
            new_obj: None,
            reclaim_below: 0,
            completion: Completion::Resumed(cont.promise),
        });
        self.fp = new_fp;
        self.cur_local_count = cont.local_count;
        let depth = self.callstack.len();
        for h in cont.saved_handlers {
            self.handlers.push(HandlerEntry {
                catch_ip: h.catch_ip,
                stack_len: new_fp as usize + h.rel_stack_len,
                callstack_len: depth,
                fp: new_fp,
            });
        }
        match payload {
            ResumePayload::Resolved(v) => {
                self.stack.push(v);
                self.ip = cont.resume_ip;
            }
            // `saved_handlers` was non-empty (the scheduler short-circuits
            // the handlerless case), so the innermost handler is this
            // frame's own — `try { await p } catch` across a suspension.
            ResumePayload::Rejected(errval) => {
                self.unwind_to_handler(errval);
            }
        }
    }

    /// An uncaught throw / rejection / catchable VM error escaping an async
    /// call: reject that call's promise (waking waiters), discard the call's
    /// own frames and handler entries, and leave the frame exactly as a
    /// `Return` would — the caller gets the (rejected) promise as the call's
    /// value, or a resumed strand falls through to the scheduler. Everything
    /// below the strand base is untouched: it is either the parked root
    /// region or a caller this throw must not reach.
    pub(super) fn reject_strand(&mut self, errval: Value) -> Result<(), VMError> {
        let Some(base) = self.strand_base() else {
            return Err(self.fail_invariant(ErrorKind::BadReturn, "reject_strand outside a strand"));
        };
        let (completion, return_addr, prev_fp, reclaim_below) = {
            let f = &self.callstack[base];
            (f.completion, f.return_addr, f.prev_fp, f.reclaim_below)
        };
        let Some(pid) = completion.promise() else {
            unreachable!("strand_base only ever names a frame that owns a promise");
        };
        // The strand base frame's fp: the current fp when no sync frames
        // sit above it, else the first such frame's saved prev_fp.
        let base_fp = if self.callstack.len() > base + 1 {
            self.callstack[base + 1].prev_fp
        } else {
            self.fp
        };
        // Drop the frame's region *and* the call group the caller left under
        // it (callee/receiver), like `Return`'s `keep_below`: the promise
        // `leave_async_frame` pushes has to land where the call's value goes.
        // A resumed frame reclaims nothing — it has no call group.
        self.stack
            .truncate(base_fp as usize - reclaim_below as usize);
        self.fp = prev_fp;
        self.callstack.truncate(base);
        self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
        while self.handlers.last().is_some_and(|h| h.callstack_len > base) {
            self.handlers.pop();
        }
        self.settle_and_wake(pid, PromiseState::Rejected(errval))?;
        self.leave_async_frame(completion, return_addr)
    }

    /// The dedicated deadlock error (commitment 4's payoff: promises only
    /// come from tool calls and async calls, so a blocked root with nothing
    /// ready, nothing in the outbox, and nothing in flight is *provably*
    /// stuck — circular awaits). Names the await chain.
    pub(super) fn deadlock_error(&self, awaiting: PromisePtr) -> VMError {
        let chain = self.render_await_chain(awaiting);
        self.fail(
            ErrorKind::Deadlock,
            format!("deadlock: circular await — {chain}"),
        )
    }

    /// Reconstruct the await chain from promise waiter links and the
    /// continuation records' await spans: there is no stack to read for a
    /// suspended chain, so diagnostics walk the heap records instead.
    pub(super) fn render_await_chain(&self, root: PromisePtr) -> String {
        let mut out = String::from("top level awaits");
        let mut seen: Vec<PromisePtr> = Vec::new();
        let mut pid = root;
        loop {
            if seen.contains(&pid) {
                out.push_str(&format!(" promise {pid} (the cycle)"));
                break;
            }
            seen.push(pid);
            // The continuation that would settle `pid`, if any (a leaf tool
            // promise has none).
            match self
                .continuations
                .iter()
                .flatten()
                .find(|c| c.promise == pid)
            {
                Some(c) => {
                    out.push_str(&format!(
                        " promise {pid} (async call suspended at {}), which awaits",
                        self.span_pos(c.await_span)
                    ));
                    pid = c.awaiting;
                }
                None => {
                    out.push_str(&format!(" promise {pid}"));
                    break;
                }
            }
        }
        out
    }

    /// `line:col` of a source span, degrading to `?` for hand-assembled
    /// programs with no source.
    fn span_pos(&self, span: u32) -> String {
        if self.source.is_empty() {
            return "?".to_string();
        }
        let (line, col, _) = crate::diag::line_col(&self.source, span);
        format!("{line}:{col}")
    }

    /// Push a string value onto the stack. Strings live inline as `JsString`, not
    /// in `heap`, so this is just a stack push (no heap slot, no growth). The
    /// builtin/string-producing counterpart to `alloc_array`/`alloc_object`.
    pub(crate) fn push_str_value(&mut self, s: impl Into<JsString>) {
        self.stack.push(Value::String(s.into()));
    }

    pub(crate) fn alloc_array(&mut self, arr: ThinVec<Value>) -> Value {
        let addr = self.arrays.len() as ArrayPtr;
        self.arrays.push(arr);
        Value::Array(addr)
    }

    /// Compile and allocate a `Value::RegExp` from a pattern + flags string.
    /// Shared by the `RegExp` constructor handler (`regexp_ctor`) and
    /// `construct_builtin` (the `new RegExp(…)` path). Validates flags and
    /// compiles via `regress`; an invalid pattern or flags → `ValueError`.
    pub(crate) fn alloc_regexp(
        &mut self,
        pattern: JsString,
        flags: JsString,
    ) -> Result<Value, VMError> {
        let flags_str = flags.to_utf8_lossy();
        for c in flags_str.chars() {
            if !matches!(c, 'g' | 'i' | 'm' | 's' | 'u' | 'y' | 'd' | 'v') {
                return Err(self.fail(
                    ErrorKind::SyntaxError,
                    format!("invalid regular expression flags: {flags_str}"),
                ));
            }
        }
        let compiled =
            match regress::Regex::with_flags(&pattern.to_utf8_lossy(), flags_str.as_str()) {
                Ok(re) => re,
                Err(e) => {
                    return Err(self.fail(
                        ErrorKind::SyntaxError,
                        format!("invalid regular expression: {e}"),
                    ));
                }
            };
        let rx_data = RegExpData {
            pattern,
            flags,
            compiled,
            last_index: std::cell::Cell::new(0),
        };
        Ok(Value::RegExp(RcRegExp::new(rx_data)))
    }

    pub(crate) fn alloc_object(&mut self, obj: IndexMap<FieldName, Value>) -> Value {
        // Step 2b: plain objects chain to `Object.prototype` (matching JS —
        // `Object.getPrototypeOf({}) === Object.prototype`, `{} instanceof
        // Object` is true). The prototype is lazily allocated; the cost is
        // one `prototype_for` call (a side-table lookup + one push on first
        // allocation, a pure lookup after). Objects that should NOT chain
        // (builtin prototypes/namespaces, `Object.create(null)` instances)
        // push `ObjData` directly rather than calling here.
        let proto = self.prototype_for(crate::vm::instr::TypeTag::Object).ok();
        // Compute `addr` AFTER `prototype_for` — it may push to `self.objects`
        // (lazily allocating `Object.prototype`), which would make an
        // earlier `addr` stale.
        let addr = self.objects.len() as ObjectPtr;
        self.objects.push(ObjData {
            proto,
            map: obj,
            ..Default::default()
        });
        Value::Object(addr)
    }

    /// Allocate an error object: `{ name, message }` linked to
    /// `Error.prototype`.
    ///
    /// **The one place that knows how an error is built.** Every value a
    /// program can `catch` comes through here — the errors the VM raises
    /// (`error_to_thrown`), `new Error(…)`/`TypeError(…)` (the registry's
    /// constructor handlers), and the host's own failures (the harness's
    /// tool errors) — so
    /// `e instanceof Error` and `` `${e}` `` cannot be true of one source and
    /// false of the next. Before this existed each source built its own plain
    /// object literal, and a caught error was `instanceof Error` nowhere.
    ///
    /// The two fields are own properties (so `Object.keys(e)` is
    /// `["name", "message"]` and `JSON.stringify(e)` still round-trips);
    /// only the proto link marks it as an error.
    pub fn alloc_error(&mut self, name: JsString, message: JsString) -> Value {
        // The `name` picks the class, so this one mapping serves all three
        // sources at once: `error_to_thrown`'s `{:?}` of an `ErrorKind`
        // (only ever `TypeError`/`ValueError`/`RangeError`/`SyntaxError`/
        // `ReferenceError` — the other kinds are not resumable and end the
        // program instead of becoming a value), the `TypeTag::name()` each constructor handler passes in,
        // and the harness's `"ToolError"`, which has no class and lands on
        // the base.
        let tag = crate::vm::instr::TypeTag::error_tag_for_name(&name);
        let mut map = IndexMap::new();
        map.insert(JsString::from("name"), Value::String(name));
        map.insert(JsString::from("message"), Value::String(message));
        // Compute `addr` AFTER `prototype_for` — it pushes to `self.objects`
        // when the prototype is not yet materialized, which would make an
        // earlier `addr` stale (the same trap `alloc_object` documents).
        let proto = self.prototype_for(tag).ok();
        let addr = self.objects.len() as ObjectPtr;
        self.objects.push(ObjData {
            proto,
            map,
            ..Default::default()
        });
        Value::Object(addr)
    }

    /// The `name` and `message` of an error object, or `None` for anything
    /// that is not one — where "is one" means *its prototype chain reaches
    /// `Error.prototype`*, never its shape. String coercion asks this, so
    /// `` `${e}` `` is `"TypeError: cannot read property 'x' on null"` while
    /// the ordinary object literal `{ name: "x", message: "y" }` stays
    /// `"[object Object]"`: duck-typing here would quietly reclassify every
    /// two-field record a program happens to build.
    ///
    /// **Takes `&self` and never allocates**: `prototype_ptr` peeks at the
    /// side table instead of materializing `Error.prototype`, so a program
    /// that never made an error answers `None` after one lookup — which is
    /// what lets `write_js_units`, which cannot call back into JS, do this at
    /// all.
    ///
    /// The fallbacks are `Error.prototype`'s own defaults in JS: an absent
    /// `name` reads as `"Error"`, an absent `message` as `""`.
    fn error_parts(&self, val: &Value, depth: usize) -> Option<(JsString, JsString)> {
        let Value::Object(p) = val else {
            return None;
        };
        let err_proto = self.prototype_ptr(crate::vm::instr::TypeTag::Error)?;
        let obj = self.objects.get(*p as usize)?;
        let mut cur = obj.proto;
        // Same depth cap as `resolve_proto_chain`: a malformed chain must not
        // hang the formatter.
        let mut found = false;
        for _ in 0..MAX_PROTO_DEPTH {
            match cur {
                Some(ptr) if ptr == err_proto => {
                    found = true;
                    break;
                }
                Some(ptr) => cur = self.objects.get(ptr as usize).and_then(|o| o.proto),
                None => break,
            }
        }
        if !found {
            return None;
        }
        let field = |key: &[u16], default: &str| -> JsString {
            match obj.map.get(key) {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Undefined) => JsString::from(default),
                Some(other) => self.to_js_string(other, depth + 1),
            }
        };
        Some((field(keys::NAME, "Error"), field(keys::MESSAGE, "")))
    }

    /// Walk own `map` → `proto` chain → `Undefined`. Own hit returns
    /// immediately; `proto: None` returns `Undefined` with one branch and
    /// never enters the loop. A depth cap guards against malformed cycles.
    /// Stack effect: none (pure property resolution).
    ///
    /// Step 2b: when the walk reaches a builtin prototype (`kind ==
    /// BuiltinPrototype`), the `constructor` virtual rung is resolved —
    /// `[].constructor === Array`, `(5).constructor === Number` — without
    /// materializing it into the (empty) map, so enumerability
    /// (`Object.keys(Array.prototype) === []`) holds.
    pub(crate) fn resolve_proto_chain(
        &self,
        obj_ptr: ObjectPtr,
        field: &JsString,
    ) -> Result<Value, VMError> {
        let mut cur = obj_ptr;
        for _depth in 0..MAX_PROTO_DEPTH {
            let obj = self
                .objects
                .get(cur as usize)
                .ok_or_else(|| self.fail_invariant(ErrorKind::TypeError, "bad object pointer"))?;
            if let Some(v) = obj.map.get(field) {
                return Ok(v.clone());
            }
            // Virtual rung: `constructor` on a builtin prototype.
            if obj.kind == ObjKind::BuiltinPrototype
                && field.eq_str("constructor")
                && let Some(v) = self.prototype_constructor(cur)
            {
                return Ok(v);
            }
            match obj.proto {
                Some(parent) => cur = parent,
                None => return Ok(Value::Undefined),
            }
        }
        Ok(Value::Undefined)
    }

    /// Walk a prototype chain from `start`, reporting whether `target` appears
    /// on it. Reuses the same `MAX_PROTO_DEPTH` cap as `resolve_proto_chain`, so
    /// a cyclic chain terminates (returns `false`). Shared by `instanceof` and
    /// the cyclic-prototype check in `set_object_proto`.
    pub(crate) fn proto_chain_contains(
        &self,
        start: ObjectPtr,
        target: ObjectPtr,
    ) -> Result<bool, VMError> {
        let mut cur = Some(start);
        for _ in 0..MAX_PROTO_DEPTH {
            match cur {
                Some(p) if p == target => return Ok(true),
                Some(p) => {
                    let obj = self.objects.get(p as usize).ok_or_else(|| {
                        self.fail_invariant(ErrorKind::TypeError, "bad object pointer")
                    })?;
                    cur = obj.proto;
                }
                None => return Ok(false),
            }
        }
        Ok(false)
    }

    /// Set an `Object`'s `[[Prototype]]` to `val` (an `Object` or `Null`).
    /// Rejects a non-Object/non-Null `val`, a non-extensible (Sealed/Frozen)
    /// receiver, and a cycle (the new proto's chain must not reach the
    /// receiver), matching JS. Used by `Object.setPrototypeOf`.
    pub(crate) fn set_object_proto(
        &mut self,
        obj_ptr: ObjectPtr,
        val: Value,
    ) -> Result<(), VMError> {
        let new_proto = match val {
            Value::Object(p) => Some(p),
            Value::Null => None,
            _ => {
                return Err(self.fail(ErrorKind::TypeError, "prototype must be an object or null"));
            }
        };
        // Integrity gate (Step 2a): a non-extensible object's prototype is
        // locked — JS `Object.setPrototypeOf` throws on Sealed/Frozen.
        let integrity = self
            .objects
            .get(obj_ptr as usize)
            .map_or(IntegrityLevel::Extensible, |o| o.integrity);
        if matches!(
            integrity,
            IntegrityLevel::NonExtensible | IntegrityLevel::Sealed | IntegrityLevel::Frozen
        ) {
            return Err(self.fail(
                ErrorKind::TypeError,
                "cannot set prototype of a non-extensible object",
            ));
        }
        if let Some(proto_ptr) = new_proto
            && self.proto_chain_contains(proto_ptr, obj_ptr)?
        {
            return Err(self.fail(ErrorKind::TypeError, "cyclic prototype chain"));
        }
        self.objects[obj_ptr as usize].proto = new_proto;
        Ok(())
    }

    /// `x instanceof F`: walk `x`'s prototype chain looking for `F.prototype`.
    /// Step 2b folds the structural `TypeTag` fast path and the user-class
    /// walk into one path: the RHS is evaluated to a real constructor value
    /// (`Value::Closure`/`Value::Bound`/`Value::Builtin`-constructor), its
    /// `.prototype` is resolved (the type's frozen prototype for builtins,
    /// the lazily-allocated `F.prototype` for user closures), and `x`'s
    /// `[[Prototype]]` chain is walked via [`Self::value_proto`] — so
    /// `[] instanceof Array`, `m instanceof Map`, `f instanceof Function`,
    /// `x instanceof Object`, and `new F() instanceof F` all take the same
    /// walk with no `TypeTag` special-case.
    ///
    /// Primitives (`String`/`Number`/`Bool`/`Null`/`Undefined`) have no
    /// `[[Prototype]]` chain for `instanceof` purposes (JS: a primitive is
    /// never `instanceof` anything), so they return `false` immediately.
    /// RHS must be callable with a `.prototype` — else `TypeError`. A
    /// method/namespace `Value::Builtin` has no `.prototype`, so it yields
    /// `false` (matching JS where `x instanceof Math.max` is false).
    pub(crate) fn instanceof(&mut self, lhs: Value, rhs: Value) -> Result<bool, VMError> {
        // Resolve the RHS's `.prototype` (the target we walk `lhs`'s chain
        // looking for). A non-callable RHS is a TypeError; a callable without
        // a `.prototype` (method/namespace builtin) yields `false`.
        let target_proto = match rhs {
            Value::Closure { ptr, .. } => self.resolve_prototype(ptr)?,
            Value::Bound(b) => match &b.callable {
                Value::Closure { ptr, .. } => self.resolve_prototype(*ptr)?,
                // A Bound wrapping a builtin constructor: use the
                // constructor's type prototype.
                Value::Builtin(b) => match b.constructor_type_tag() {
                    Some(tag) => self.prototype_for(tag)?,
                    None => return Ok(false),
                },
                _ => {
                    return Err(self.fail(
                        ErrorKind::TypeError,
                        "right-hand side of `instanceof` is not callable",
                    ));
                }
            },
            Value::Builtin(b) => match b.constructor_type_tag() {
                Some(tag) => self.prototype_for(tag)?,
                None => return Ok(false),
            },
            _ => {
                return Err(self.fail(
                    ErrorKind::TypeError,
                    "right-hand side of `instanceof` is not callable",
                ));
            }
        };
        // Primitives never participate in `instanceof` (JS: `"x" instanceof
        // String` is false, `5 instanceof Object` is false — the spec's first
        // step is "If Type(relObj) is not Object, return false"). Only
        // objects and structural heap types (Array/Map/Set/RegExp/Closure/
        // Builtin/Bound/Promise) have a `[[Prototype]]` chain to walk.
        // `value_proto` returns a prototype for primitives too (the wrapper
        // type's prototype, for `Object.getPrototypeOf`), so gate the walk
        // on the LHS being an object type, not on `value_proto` returning
        // `Some`.
        if !is_object_for_instanceof(&lhs) {
            return Ok(false);
        }
        let start = match self.value_proto(&lhs)? {
            Some(p) => p,
            None => return Ok(false),
        };
        // Walk from `lhs`'s `[[Prototype]]` looking for `target_proto`.
        // `proto_chain_contains(start, target)` checks `start` itself and
        // walks up — exactly `instanceof`'s semantics.
        self.proto_chain_contains(start, target_proto)
    }

    /// The `[[Prototype]]` of any value as an `ObjectPtr` (Step 2b). For an
    /// `Object`, it is `obj.proto` (the explicit field — `Some(Object.prototype)`
    /// for plain objects, `Some(F.prototype)` for `new F()`, `None` for
    /// `Object.create(null)`). For structural heap types (Array/Map/Set/
    /// RegExp) and callables (Closure/Builtin/Bound), it is the type's
    /// frozen builtin prototype (lazily allocated). For primitives
    /// (String/Number/Bool), it is the wrapper type's prototype — matching
    /// JS `Object.getPrototypeOf` which returns the wrapper prototype for
    /// primitives. `Null`/`Undefined`/`Upval` have no `[[Prototype]]`
    /// (`None`); `Object.getPrototypeOf` throws for null/undefined.
    ///
    /// This is the one place the `[[Prototype]]` mapping is defined, shared
    /// by `instanceof`, `Object.getPrototypeOf`, and the `get_property`
    /// proto-chain walk — the seed of the convergence target's `get_property`
    /// /`set_property` pair.
    pub(crate) fn value_proto(&mut self, val: &Value) -> Result<Option<ObjectPtr>, VMError> {
        Ok(match val {
            Value::Object(p) => self
                .objects
                .get(*p as usize)
                .map(|o| o.proto)
                .unwrap_or(None),
            Value::Array(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::Array)?),
            Value::Map(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::Map)?),
            Value::Set(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::Set)?),
            Value::RegExp(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::RegExp)?),
            Value::Closure { .. } | Value::Bound(_) | Value::Builtin(_) => {
                Some(self.prototype_for(crate::vm::instr::TypeTag::Function)?)
            }
            Value::String(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::String)?),
            Value::Float(_) | Value::PosInt(_) | Value::NegInt(_) => {
                Some(self.prototype_for(crate::vm::instr::TypeTag::Number)?)
            }
            Value::Bool(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::Boolean)?),
            // Promises chain to Object.prototype (no Promise.prototype in
            // this dialect — promises are transient tool-call values).
            Value::Promise(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::Object)?),
            Value::ArrayBuffer(_) => {
                Some(self.prototype_for(crate::vm::instr::TypeTag::ArrayBuffer)?)
            }
            Value::TypedArray(p) => {
                let tag = self
                    .typed_arrays
                    .get(*p as usize)
                    .map(|v| v.kind.type_tag())
                    .unwrap_or(crate::vm::instr::TypeTag::Float64Array);
                Some(self.prototype_for(tag)?)
            }
            Value::DataView(_) => Some(self.prototype_for(crate::vm::instr::TypeTag::DataView)?),
            Value::Null | Value::Undefined | Value::Upval(_) => None,
        })
    }

    /// Resolve the `constructor` virtual rung on a builtin prototype
    /// (Step 2b). If `proto_ptr` is one of the lazily-allocated builtin
    /// prototypes (`Array.prototype`, `Object.prototype`, …), return the
    /// corresponding constructor `Value::Builtin` (`ArrayCtor`, …); else
    /// `None`. This is what makes `[].constructor === Array`,
    /// `(5).constructor === Number`, etc. hold: a property read walks the
    /// chain, hits the type's frozen prototype, and resolves `constructor`
    /// to the constructor value without materializing it into the map (so
    /// `Object.keys(Array.prototype) === []` still holds — enumerability).
    pub(crate) fn prototype_constructor(&self, proto_ptr: ObjectPtr) -> Option<Value> {
        // Reverse-map a prototype ptr to its type tag by consulting the side
        // table through `prototype_ptr` (which keys by `tag as usize`), so the
        // index↔tag correspondence lives in one place (`TypeTag::ALL`) and
        // cannot drift if the enum is reordered.
        let tag = crate::vm::instr::TypeTag::ALL
            .into_iter()
            .find(|&tag| self.prototype_ptr(tag) == Some(proto_ptr))?;
        crate::builtin::Builtin::for_type_tag(tag).map(Value::Builtin)
    }

    /// Resolve a Closure's `.prototype`, lazily allocating an empty object on
    /// first access. Shared by `get_property` (for `F.prototype` property
    /// reads) and `instanceof` (for walking the chain).
    pub(crate) fn resolve_prototype(&mut self, ptr: ClosurePtr) -> Result<ObjectPtr, VMError> {
        if let Some(proto_ptr) = self.closures.get(ptr as usize).and_then(|c| c.prototype) {
            return Ok(proto_ptr);
        }
        let new_map = IndexMap::new();
        let proto_ptr = self.objects.len() as ObjectPtr;
        self.objects.push(ObjData {
            proto: None,
            map: new_map,
            ..Default::default()
        });
        self.closures[ptr as usize].prototype = Some(proto_ptr);
        Ok(proto_ptr)
    }

    // ── Step 2a: per-type builtin prototypes ──────────────────────────

    /// Get the `ObjectPtr` of a builtin type's frozen prototype, allocating
    /// it lazily on first access. The prototype is a frozen `ObjData` with
    /// `kind: BuiltinPrototype` (no JSON form) and an empty `map` — methods
    /// are *virtual rungs* resolved from the `builtins!` registry, not
    /// materialized, so `Object.keys(Array.prototype)` is `[]` and `for-in`
    /// shows no method names (Step 2a Part 2 enumerability). All prototypes
    /// chain to `Object.prototype` (`proto: Some(..)`); `Object.prototype`
    /// itself chains to `null` (`proto: None`), matching JS. The side table
    /// (`self.prototypes`, indexed by `TypeTag as usize`) caches the ptr so
    /// repeated calls return the same object — identity matters for
    /// `Object.getPrototypeOf([]) === Array.prototype`.
    ///
    /// The `TypePrototype::overridden` flag (Step 3) is initialized to
    /// `false` (the `TypePrototype::default`) — no guard branch is emitted
    /// on the hot path while prototypes are frozen.
    pub fn prototype_for(&mut self, tag: crate::vm::instr::TypeTag) -> Result<ObjectPtr, VMError> {
        let idx = tag as usize;
        if let Some(entry) = self.prototypes.get(idx)
            && let Some(p) = entry.ptr
        {
            return Ok(p);
        }
        // Object.prototype is the root: proto = None. Every other prototype
        // chains to it, so allocate it first (recursively, but the recursion
        // bottoms out immediately at the Object arm).
        //
        // The error subclasses are the one two-rung chain: `TypeError.prototype`
        // → `Error.prototype` → `Object.prototype`. That middle rung is the
        // whole error hierarchy — it is why a type error is `instanceof
        // TypeError` *and* `instanceof Error` while not being `instanceof
        // RangeError`. Chaining them straight to `Object.prototype` like
        // everything else would have made `e instanceof Error` false for
        // every error but a bare one, which is the silent wrong branch the
        // `Error` prototype was added to close.
        let proto = match tag {
            crate::vm::instr::TypeTag::Object => None,
            // `Error` itself is not a subclass of itself: it takes the
            // ordinary `Object.prototype` rung, and the recursion below
            // bottoms out there rather than looping.
            t if t.is_error() && !matches!(t, crate::vm::instr::TypeTag::Error) => {
                Some(self.prototype_for(crate::vm::instr::TypeTag::Error)?)
            }
            _ => Some(self.prototype_for(crate::vm::instr::TypeTag::Object)?),
        };
        let ptr = self.objects.len() as ObjectPtr;
        self.objects.push(ObjData {
            proto,
            map: IndexMap::new(),
            integrity: IntegrityLevel::Frozen,
            kind: ObjKind::BuiltinPrototype,
        });
        // Grow the side table to fit this index (lazy: starts empty).
        if idx >= self.prototypes.len() {
            self.prototypes
                .resize(crate::vm::instr::TypeTag::COUNT, Default::default());
        }
        self.prototypes[idx].ptr = Some(ptr);
        Ok(ptr)
    }

    /// Read-only peek at a prototype's `ObjectPtr` if already allocated, or
    /// `None` if it has not been lazily materialized yet. Does *not* allocate.
    pub fn prototype_ptr(&self, tag: crate::vm::instr::TypeTag) -> Option<ObjectPtr> {
        self.prototypes.get(tag as usize).and_then(|tp| tp.ptr)
    }

    // ── Step 2a Part 2: namespace objects + native constructors ──────────

    /// Get the `ObjectPtr` of a global namespace object (`Math`, `JSON`),
    /// allocating it lazily on first access. The object is a frozen
    /// `ObjData` with `kind: BuiltinNamespace` (no JSON form) carrying its
    /// statics/constants as own properties — materialized, since a namespace
    /// is a plain object not a constructor (methods like `Math.max` are
    /// `Value::Builtin` entries). The side table (`self.namespaces`, indexed
    /// by `GlobalId as usize`) caches the ptr so repeated calls return the
    /// same object — identity matters for `Math === Math`.
    pub fn namespace_for(&mut self, g: crate::vm::instr::GlobalId) -> Result<ObjectPtr, VMError> {
        let idx = g as usize;
        if let Some(Some(p)) = self.namespaces.get(idx) {
            return Ok(*p);
        }
        // Compute the prototype first (may allocate into `objects`) so the
        // borrow checker is happy and the ptr is stable for the push below.
        let proto = Some(self.prototype_for(crate::vm::instr::TypeTag::Object)?);
        let ptr = self.objects.len() as ObjectPtr;
        self.objects.push(ObjData {
            proto,
            map: IndexMap::new(),
            integrity: IntegrityLevel::Frozen,
            kind: ObjKind::BuiltinNamespace,
        });
        // Populate own properties from the registry: every `BuiltinKind::Namespace(ns)`
        // row whose `ns` matches this global becomes a `Value::Builtin` entry.
        let ns_name = match g {
            crate::vm::instr::GlobalId::Math => "Math",
            crate::vm::instr::GlobalId::JSON => "JSON",
        };
        let mut map = std::mem::take(&mut self.objects[ptr as usize].map);
        // Compile-time constants (Math.PI, Math.E) folded into the map.
        for &(ns, member, val) in namespace_constants() {
            if ns == ns_name {
                let key = JsString::from(member);
                let v = match val {
                    ConstVal::Float(f) => Value::Float(f),
                    ConstVal::PosInt(n) => Value::PosInt(n),
                };
                map.insert(key, v);
            }
        }
        // Static functions: projected from the `builtins!` registry — the
        // same rows the compiler's fast path uses. No hand-maintained name
        // list (Step 2a Part 3 item B): a namespace method added to the
        // registry appears here with no second edit.
        for (ns, member, b) in crate::builtin::Builtin::namespace_statics() {
            if ns == ns_name {
                map.insert(JsString::from(member), Value::Builtin(b));
            }
        }
        self.objects[ptr as usize].map = map;
        // Grow the side table to fit this index (lazy: starts empty).
        if idx >= self.namespaces.len() {
            self.namespaces.resize(2, None); // GlobalId::COUNT == 2
        }
        self.namespaces[idx] = Some(ptr);
        Ok(ptr)
    }

    /// Read-only peek at a namespace's `ObjectPtr` if already allocated, or
    /// `None` if it has not been lazily materialized yet. Does *not* allocate.
    pub fn namespace_ptr(&self, g: crate::vm::instr::GlobalId) -> Option<ObjectPtr> {
        self.namespaces.get(g as usize).copied().flatten()
    }

    /// Resolve a bare name to a value at runtime — the fallback when the
    /// compiler does not statically recognize an identifier. Used by
    /// [`Instr::PushName`]. Names in the builtin registry resolve directly —
    /// the error classes included, since each is an ordinary constructor row
    /// now; hardcoded globals
    /// (`undefined`, `NaN`, `Infinity`) resolve to their literal values;
    /// everything else raises a `ReferenceError` whose message includes the
    /// name — preserving the name-level signal in the failure histogram.
    pub fn resolve_name(&mut self, name: &JsString) -> Result<Value, VMError> {
        if let Some(b) = crate::builtin::Builtin::for_constructor(name) {
            return Ok(Value::Builtin(b));
        }
        // Every error class — `Error` and each subclass — is a real
        // constructor row, so all of them resolve above and each carries its
        // own `.prototype`. They used to be listed here as aliases for the
        // `Function` constructor, which kept `typeof TypeError === "function"`
        // true (test262 checks it) at the price of making `e instanceof
        // TypeError` walk to `Function.prototype` and miss. Nothing is left
        // to alias.
        crate::match_wide!(name => {
            "undefined" => Ok(Value::Undefined),
            "NaN" => Ok(Value::Float(f64::NAN)),
            "Infinity" => Ok(Value::Float(f64::INFINITY)),
            _ => Err(self.fail(
                crate::vm::ErrorKind::ReferenceError,
                format!("{name} is not defined"),
            )),
        })
    }

    /// Materialize the element sequence of a value this dialect treats as
    /// "iterable" for `new Map(..)`/`new Set(..)`. There is still no general
    /// iterator protocol — but `for-of`/spread no longer need one either:
    /// they normalize through `builtin::iter_source` (added 2026-09-16,
    /// itself a fix for the same *kind* of gap — `Map`/`Set` not iterating),
    /// which turns a `Map` into its `[key, value]` pairs and a `Set` into
    /// its values before the existing array/string index loop runs. This
    /// helper can't call `iter_source` directly (it takes stack-based
    /// `Args`, and `map_construct`/`set_construct` run outside the
    /// compiled-bytecode path that supplies those), so it reimplements the
    /// same two normalizations by hand, plus `String` (which `iter_source`
    /// leaves alone, since native byte-indexing already makes a string
    /// loop-indexable — but a constructor needs actual *members*, not just
    /// something indexable). Recognizes:
    /// - `Array`: its elements, as-is.
    /// - `String`: one `Value::String` per Unicode scalar value (so
    ///   `"abc"` yields three one-character strings). Note this is *not*
    ///   the dialect's usual "strings are UTF-8 bytes" indexing rule —
    ///   walking raw byte offsets would split multi-byte characters into
    ///   fragments that are not valid `JsString`s on their own, so member-
    ///   ship in the resulting `Set`/`Map` would be nonsensical for
    ///   anything outside ASCII. `chars()` is the one sane reading of
    ///   "iterate a string" here; ASCII input (the tested case) is
    ///   identical either way.
    /// - `Set`: its values, in insertion order (same values `iter_source`
    ///   would produce for a `for-of`).
    /// - `Map`: its entries, in insertion order, each freshly boxed as a
    ///   `[key, value]` 2-element array — mirroring real JS, where iterating
    ///   a `Map` (`for (const [k, v] of someMap)`, or spreading one) yields
    ///   entries, not bare keys (again, matching `iter_source`).
    ///
    /// Found by the 2026-09-17 `sweep-200` eval: `new Set(x)` rejected every
    /// argument except a plain `Array` while claiming to require "an
    /// iterable" — rejecting `new Set("abc")`, `new Set(otherSet)`, and
    /// `new Set(map.keys())` (itself a plain `Array` — this dialect's
    /// `Map`/`Set` accessor methods already materialize eagerly — so that
    /// last case worked by accident once `Array` did, but the other two
    /// didn't). Returns `Ok(None)` when `arg` is none of these; the caller
    /// turns that into a `TypeError` naming the constructor.
    pub(crate) fn iterable_elements(&mut self, arg: &Value) -> Result<Option<Vec<Value>>, VMError> {
        match arg {
            Value::Array(p) => {
                let arr = self
                    .arrays
                    .get(*p as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                Ok(Some(arr.iter().cloned().collect()))
            }
            Value::String(s) => Ok(Some(
                crate::units::code_points(s.as_units())
                    .map(|cp| Value::String(JsString::from_units(cp)))
                    .collect(),
            )),
            Value::Set(p) => {
                let set = self
                    .sets
                    .get(*p as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                Ok(Some(set.iter().map(|k| k.0.clone()).collect()))
            }
            Value::Map(p) => {
                let pairs: Vec<(Value, Value)> = {
                    let map = self
                        .maps
                        .get(*p as usize)
                        .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                    map.iter().map(|(k, v)| (k.0.clone(), v.clone())).collect()
                };
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let pair: ThinVec<Value> = vec![k, v].into();
                    out.push(self.alloc_array(pair));
                }
                Ok(Some(out))
            }
            _ => Ok(None),
        }
    }

    /// Construct a `Value::Map` from an optional iterable of `[key, value]`
    /// pairs. The **one** native Map construction body (Step 2a Part 3 item C):
    /// `new Map(entries)` and any other entry point share this. `arg` is
    /// `Value::Undefined` for the no-arg case (`new Map()`). Any value
    /// [`iterable_elements`] recognizes is accepted; each yielded element must
    /// still itself be an `[key, value]`-shaped `Array` (a `String`'s
    /// characters, e.g., are not, so `new Map("ab")` still throws, matching
    /// real JS).
    pub(crate) fn map_construct(&mut self, arg: Value) -> Result<Value, VMError> {
        let mut map: IndexMap<MapKey, Value> = IndexMap::new();
        if !matches!(arg, Value::Undefined) {
            let Some(entries) = self.iterable_elements(&arg)? else {
                return Err(self.fail(
                    ErrorKind::TypeError,
                    "Map argument must be an iterable of [key, value] pairs",
                ));
            };
            for entry in entries {
                let pair_ptr = match entry {
                    Value::Array(p) => p,
                    _ => {
                        return Err(self.fail(ErrorKind::TypeError, "type error"));
                    }
                };
                let pair = self
                    .arrays
                    .get(pair_ptr as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                if pair.len() < 2 {
                    continue;
                }
                map.insert(MapKey(pair[0].clone()), pair[1].clone());
            }
        }
        let addr = self.maps.len() as MapPtr;
        self.maps.push(map);
        Ok(Value::Map(addr))
    }

    /// Construct a `Value::Set` from an optional iterable of values. The
    /// **one** native Set construction body (Step 2a Part 3 item C):
    /// `new Set(iterable)` and any other entry point share this. `arg` is
    /// `Value::Undefined` for the no-arg case (`new Set()`). Any value
    /// [`iterable_elements`] recognizes is accepted — see that doc comment
    /// for the full list and the eval finding that drove it.
    pub(crate) fn set_construct(&mut self, arg: Value) -> Result<Value, VMError> {
        let mut set: IndexSet<MapKey> = IndexSet::new();
        if !matches!(arg, Value::Undefined) {
            let Some(elements) = self.iterable_elements(&arg)? else {
                return Err(self.fail(ErrorKind::TypeError, "Set argument must be an iterable"));
            };
            for v in elements {
                set.insert(MapKey(v));
            }
        }
        let addr = self.sets.len() as SetPtr;
        self.sets.push(set);
        Ok(Value::Set(addr))
    }

    /// Construct a value via a native constructor's `new` path. Step 2a
    /// Part 2: `new Map()`/`new Set()`/`new RegExp()`/`new Array()`/etc.
    /// dispatches here. A **pure value-producer** (Step 2a Part 3 item D):
    /// consumes the `nargs` args **and** the callee placeholder slot just
    /// below them from the stack, pushes the constructed result, but does
    /// **not** advance `self.ip` — the caller (`Instr::New`) owns the ip
    /// step past both `New` and the dead `NewReturn`. No `Vec::remove`
    /// mid-stack (item E): the stack is `[...caller, callee, args…]`, we
    /// truncate to `base` (below the callee) and push, giving
    /// `[...caller, result]` in one O(1) truncate + push.
    pub fn construct_builtin(
        &mut self,
        b: crate::builtin::Builtin,
        nargs: u32,
    ) -> Result<(), VMError> {
        let tag = b
            .constructor_type_tag()
            .expect("construct_builtin called on a non-constructor builtin");
        let n = nargs as usize;
        // Stack: [...caller, callee_placeholder, arg0, ..., argN-1].
        // `base` is just below the callee; args start at `base + 1`.
        if self.stack.len() < n + 1 {
            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
        }
        let base = self.stack.len() - n - 1;
        let args = crate::builtin::Args {
            base: base + 1,
            argc: n,
        };
        let result = match tag {
            // Types whose `*_ctor` handler **is** the construction body
            // (callable as both `T(...)` and `new T(...)`): delegate to the
            // handler directly.
            crate::vm::instr::TypeTag::Array => crate::builtin::array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Object => crate::builtin::object_ctor(self, args)?,
            crate::vm::instr::TypeTag::RegExp => crate::builtin::regexp_ctor(self, args)?,
            crate::vm::instr::TypeTag::Number => crate::builtin::number_ctor(self, args)?,
            crate::vm::instr::TypeTag::String => crate::builtin::string_ctor(self, args)?,
            crate::vm::instr::TypeTag::Boolean => crate::builtin::boolean_ctor(self, args)?,
            // Map/Set require `new` — their `*_ctor` handlers throw, so the
            // construction body lives in `map_construct`/`set_construct`
            // (one body per type, shared by all entry points).
            crate::vm::instr::TypeTag::Map => {
                let arg = if n == 0 {
                    Value::Undefined
                } else {
                    args.get(self, 0).clone()
                };
                self.map_construct(arg)?
            }
            crate::vm::instr::TypeTag::Set => {
                let arg = if n == 0 {
                    Value::Undefined
                } else {
                    args.get(self, 0).clone()
                };
                self.set_construct(arg)?
            }
            // `Function` is a constructor in JS but has no `Builtin` row here
            // (no `new Function(body)` support); unreachable from the
            // registry. `TypeTag::Function` keys the prototype side table only.
            crate::vm::instr::TypeTag::Function => {
                self.stack.truncate(base);
                return Err(self.fail(
                    ErrorKind::TypeError,
                    "`new Function` is not supported (use function expressions)",
                ));
            }
            crate::vm::instr::TypeTag::ArrayBuffer => crate::builtin::arraybuffer_ctor(self, args)?,
            crate::vm::instr::TypeTag::Int8Array => crate::builtin::int8array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Uint8Array => crate::builtin::uint8array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Uint8ClampedArray => {
                crate::builtin::uint8clamped_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::Int16Array => crate::builtin::int16array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Uint16Array => crate::builtin::uint16array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Int32Array => crate::builtin::int32array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Uint32Array => crate::builtin::uint32array_ctor(self, args)?,
            crate::vm::instr::TypeTag::Float32Array => {
                crate::builtin::float32array_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::Float64Array => {
                crate::builtin::float64array_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::BigInt64Array => {
                crate::builtin::bigint64array_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::BigUint64Array => {
                crate::builtin::biguint64array_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::DataView => crate::builtin::dataview_ctor(self, args)?,
            // Every error class shares one construction body; the class is
            // the tag, which `alloc_error` recovers from the name it writes.
            crate::vm::instr::TypeTag::Error => crate::builtin::error_ctor(self, args)?,
            crate::vm::instr::TypeTag::TypeError => crate::builtin::type_error_ctor(self, args)?,
            crate::vm::instr::TypeTag::ValueError => crate::builtin::value_error_ctor(self, args)?,
            crate::vm::instr::TypeTag::RangeError => crate::builtin::range_error_ctor(self, args)?,
            crate::vm::instr::TypeTag::SyntaxError => {
                crate::builtin::syntax_error_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::ReferenceError => {
                crate::builtin::reference_error_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::EvalError => crate::builtin::eval_error_ctor(self, args)?,
            crate::vm::instr::TypeTag::URIError => crate::builtin::uri_error_ctor(self, args)?,
            // The two that do not share the one-message body: their extra
            // own properties (`errors`; `error`/`suppressed`) are the reason
            // the class exists at all.
            crate::vm::instr::TypeTag::AggregateError => {
                crate::builtin::aggregate_error_ctor(self, args)?
            }
            crate::vm::instr::TypeTag::SuppressedError => {
                crate::builtin::suppressed_error_ctor(self, args)?
            }
        };
        self.stack.truncate(base);
        self.stack.push(result);
        Ok(())
    }

    pub(super) fn alloc_closure(
        &mut self,
        addr: CodeAddr,
        upvals: ThinVec<Value>,
        arity: u16,
    ) -> Value {
        let idx = self.closures.len() as ClosurePtr;
        self.closures.push(Closure {
            upvals,
            prototype: None,
            arity,
            props: None,
        });
        Value::Closure { addr, ptr: idx }
    }

    /// Write the JS `ToString` representation of `val` into `buf`. Strings in
    /// the heap are copied by slicing (zero extra allocation); other types are
    /// converted and appended. Used by `to_js_string` (which wraps a buffer) and
    /// directly by `Add` to avoid intermediate clones.
    pub(super) fn write_js_string(&self, val: &Value, depth: usize, buf: &mut String) {
        if depth > MAX_JSON_DEPTH {
            return;
        }
        match val {
            Value::Undefined => buf.push_str("undefined"),
            Value::Null => buf.push_str("null"),
            Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
            Value::PosInt(u) => buf.push_str(&u.to_string()),
            Value::NegInt(i) => buf.push_str(&i.to_string()),
            Value::Float(n) => buf.push_str(&js_number_to_string(*n)),
            Value::String(s) => buf.push_str(&s.to_utf8_lossy()),
            Value::Closure { .. } | Value::Builtin(_) | Value::Bound(_) => {
                buf.push_str("function () { [native code] }");
            }
            Value::Upval(_) => {}
            Value::ArrayBuffer(_) => buf.push_str("[object ArrayBuffer]"),
            Value::TypedArray(_) => buf.push_str("[object TypedArray]"),
            Value::DataView(_) => buf.push_str("[object DataView]"),
            Value::Array(p) => {
                if let Some(arr) = self.arrays.get(*p as usize) {
                    for (i, v) in arr.iter().enumerate() {
                        if i > 0 {
                            buf.push(',');
                        }
                        match v {
                            Value::Null | Value::Undefined => {}
                            _ => self.write_js_string(v, depth + 1, buf),
                        }
                    }
                }
            }
            Value::Object(_) => match self.error_parts(val, depth) {
                Some((name, message)) => {
                    let (name, message) = (name.to_utf8_lossy(), message.to_utf8_lossy());
                    buf.push_str(&error_to_string(&name, &message));
                }
                None => buf.push_str("[object Object]"),
            },
            Value::Promise(_) => buf.push_str("[object Promise]"),
            Value::RegExp(r) => {
                buf.push('/');
                buf.push_str(&r.pattern.to_utf8_lossy());
                buf.push('/');
                buf.push_str(&r.flags.to_utf8_lossy());
            }
            Value::Map(_) => buf.push_str("[object Map]"),
            Value::Set(_) => buf.push_str("[object Set]"),
        }
    }

    /// JS `String(x)` / `ToString`. Delegates to [`write_js_string`], assembling
    /// in a growable `String` and freezing to an immutable `JsString` once.
    pub(crate) fn to_js_string(&self, val: &Value, depth: usize) -> JsString {
        // Fast path: an existing string is already an `JsString` — share it (a
        // refcount bump) instead of copying its units through a fresh buffer.
        if let Value::String(s) = val {
            return s.clone();
        }
        let mut out: Vec<u16> = Vec::new();
        self.write_js_units(val, depth, &mut out);
        JsString::from_units(&out)
    }

    /// `write_js_string`, but appending code units.
    ///
    /// **The string cases memcpy and everything else is formatted then
    /// widened.** Every leaf but a string, a regexp source, an error's
    /// `name`/`message` and an array element renders as ASCII
    /// (`"undefined"`, a number, `[object Object]`), so the widening is a
    /// byte-to-unit map over a handful of characters.
    /// Routing the string cases through UTF-8 instead would put a transcode
    /// on both sides of every `+`.
    pub(super) fn write_js_units(&self, val: &Value, depth: usize, buf: &mut Vec<u16>) {
        if depth > MAX_JSON_DEPTH {
            return;
        }
        match val {
            Value::String(s) => buf.extend_from_slice(s.as_units()),
            Value::Array(p) => {
                if let Some(arr) = self.arrays.get(*p as usize) {
                    for (i, v) in arr.iter().enumerate() {
                        if i > 0 {
                            buf.push(b',' as u16);
                        }
                        match v {
                            Value::Null | Value::Undefined => {}
                            _ => self.write_js_units(v, depth + 1, buf),
                        }
                    }
                }
            }
            Value::RegExp(r) => {
                buf.push(b'/' as u16);
                buf.extend_from_slice(r.pattern.as_units());
                buf.push(b'/' as u16);
                buf.extend_from_slice(r.flags.as_units());
            }
            // An error renders from its own `name`/`message`, which are
            // `JsString`s: append their units rather than letting the `other`
            // arm below widen them back from UTF-8, which would mangle any
            // lone surrogate a tool put in a message.
            Value::Object(_) => match self.error_parts(val, depth) {
                Some((name, message)) => {
                    let (name, message) = (name.as_units(), message.as_units());
                    if !name.is_empty() {
                        buf.extend_from_slice(name);
                        if !message.is_empty() {
                            buf.push(b':' as u16);
                            buf.push(b' ' as u16);
                        }
                    }
                    buf.extend_from_slice(message);
                }
                None => buf.extend(crate::units::from_str("[object Object]")),
            },
            other => {
                let mut tmp = String::new();
                self.write_js_string(other, depth, &mut tmp);
                buf.extend(crate::units::from_str(&tmp));
            }
        }
    }

    pub(super) fn peek(&self) -> Result<&Value, VMError> {
        self.stack
            .last()
            .ok_or_else(|| self.fail(ErrorKind::StackUnderflow, "stack underflow"))
    }

    pub(super) fn pop(&mut self) -> Result<Value, VMError> {
        self.stack
            .pop()
            .ok_or_else(|| self.fail(ErrorKind::StackUnderflow, "stack underflow"))
    }

    pub(super) fn pop_int(&mut self) -> Result<i64, VMError> {
        self.stack
            .pop()
            .ok_or_else(|| self.fail(ErrorKind::StackUnderflow, "stack underflow"))?
            .as_i64()
            .ok_or_else(|| self.fail(ErrorKind::TypeError, "type error"))
    }

    /// Pop a value and require it to be a String; return it (a refcount bump).
    pub(super) fn pop_string(&mut self) -> Result<JsString, VMError> {
        match self
            .stack
            .pop()
            .ok_or_else(|| self.fail(ErrorKind::StackUnderflow, "stack underflow"))?
        {
            Value::String(s) => Ok(s),
            _ => Err(self.fail(ErrorKind::TypeError, "type error")),
        }
    }

    /// Borrow the `&str` of an already-popped string value. The borrow is tied
    /// to `val` (not the VM), so — unlike when strings lived in the heap — the
    /// caller may freely mutate the VM while it is live. Use for read-only
    /// builtins that push a scalar result without cloning the string.
    pub(crate) fn str_from<'a>(&self, val: &'a Value) -> Result<&'a [u16], VMError> {
        match val {
            Value::String(s) => Ok(s.as_units()),
            _ => Err(self.fail(ErrorKind::TypeError, "type error")),
        }
    }

    /// Extract an owned `JsString` from an already-popped string value — a refcount
    /// bump, sharing the same allocation. The clone sibling of `str_from`; use
    /// it when the builtin must retain the string past a borrow of the VM.
    pub(crate) fn string_from(&self, val: &Value) -> Result<JsString, VMError> {
        self.string_arg(val, None)
    }

    /// [`string_from`](Self::string_from) with the argument's name, when
    /// the caller has one.
    ///
    /// **The message is the whole value of the check.** This used to
    /// fail with the literal text `"type error"` — no argument, no
    /// expectation, no value — and a model handed
    /// `in \`replaceLines\`: type error` cannot tell which of four
    /// arguments was wrong or what arrived instead. Five traps in the
    /// kept corpus say exactly that, across four different builtins.
    ///
    /// `as_non_neg_usize` in `builtin/edit.rs` had it right all along,
    /// one line further down the same call: `"start must be a
    /// non-negative integer, got 2.5"`.
    pub(crate) fn string_arg(&self, val: &Value, label: Option<&str>) -> Result<JsString, VMError> {
        match val {
            Value::String(s) => Ok(s.clone()),
            other => Err(self.fail(
                ErrorKind::TypeError,
                match label {
                    Some(l) => format!("{l} must be a string, got {}", other.type_name()),
                    None => format!("expected a string, got {}", other.type_name()),
                },
            )),
        }
    }

    // ── JSON conversion helpers ──────────────────────────────────────

    /// Convert a stack `Value` (heap refs resolved through this VM) to
    /// JSON. Public: hosts render program results and condition payloads
    /// with it (the debugger today, `ProgramResult` events in Phase 8).
    pub fn stack_value_to_json(
        &self,
        val: &Value,
        depth: usize,
    ) -> Result<serde_json::Value, VMError> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.fail(
                ErrorKind::ValueError,
                format!(
                    "cannot serialize to JSON: nesting exceeds max depth {MAX_JSON_DEPTH} (value is too deeply nested or cyclic)"
                ),
            ));
        }
        Ok(match val {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            // Integers carry through losslessly — both map onto a native
            // serde_json::Number (this is the whole point of mirroring it).
            Value::PosInt(u) => serde_json::Value::Number(serde_json::Number::from(*u)),
            Value::NegInt(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
            // A function/closure has no JSON representation, and an Upval marker
            // is an internal indirection that should never reach here: fail
            // loudly rather than silently dropping it.
            Value::Closure { .. }
            | Value::Builtin(_)
            | Value::Bound(_)
            | Value::Upval(_)
            | Value::ArrayBuffer(_)
            | Value::TypedArray(_)
            | Value::DataView(_) => {
                return Err(self.fail(
                    ErrorKind::ValueError,
                    format!("cannot serialize a {} to JSON", val.type_name()),
                ));
            }
            // A promise is a transient value (like Closure) with no JSON
            // form. Reaching the persistence boundary with one is the classic
            // missing-`await` mistake, so say so.
            Value::Promise(_) => {
                return Err(self.fail(
                    ErrorKind::ValueError,
                    "cannot serialize a promise to JSON (did you forget `await`?)",
                ));
            }
            // `undefined` has no JSON form. Like JS `JSON.stringify`, it is
            // *dropped* in an object and coerced to *null* in an array (handled
            // at those parent sites below); reaching here means it is the root
            // value, where JS.stringify returns the JS value `undefined` — no
            // JSON — so we surface an error rather than inventing one.
            Value::Undefined => {
                return Err(self.fail(
                    ErrorKind::ValueError,
                    "cannot serialize undefined to JSON (it has no JSON form; \
                     JSON.stringify returns undefined for it in JS)",
                ));
            }
            Value::Float(n) => {
                // Preserve integer formatting when possible (f64-only VM
                // internals, but JSON consumers care about int vs float).
                if float_is_int(*n) && *n >= (i64::MIN as f64) && *n <= (i64::MAX as f64) {
                    serde_json::Value::Number(serde_json::Number::from(*n as i64))
                } else {
                    // NaN/Infinity have no JSON representation -> null, rather
                    // than silently coercing to 0.
                    match serde_json::Number::from_f64(*n) {
                        Some(num) => serde_json::Value::Number(num),
                        None => serde_json::Value::Null,
                    }
                }
            }
            // **The crossing, and the one place the loss happens.** An
            // unpaired surrogate is a value the program was entitled to
            // produce and JSON has no representation for it; erroring here
            // would make this fallible on legitimate data and turn a display
            // problem into a crashed run. `to_utf8_lossy` is the single
            // function that decides, so the loss is countable.
            Value::String(s) => serde_json::Value::String(s.to_utf8_lossy()),
            Value::Array(p) => {
                let arr = self
                    .arrays
                    .get(*p as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                serde_json::Value::Array(
                    arr.iter()
                        .map(|v| match v {
                            // JS: `undefined` array slots stringify to `null`.
                            Value::Undefined => Ok(serde_json::Value::Null),
                            _ => self.stack_value_to_json(v, depth + 1),
                        })
                        .collect::<Result<_, _>>()?,
                )
            }
            Value::Object(p) => {
                let obj = self
                    .objects
                    .get(*p as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                // JSON boundary (guardrail 2): builtin prototypes and
                // namespaces (Step 2a Part 2: `Math`, `JSON`) are reflective
                // artifacts with no JSON form, unlike a user
                // `Object.freeze`'d plain object whose data still serializes
                // (Step 2d). `kind` distinguishes them.
                if matches!(
                    obj.kind,
                    ObjKind::BuiltinPrototype | ObjKind::BuiltinNamespace
                ) {
                    return Err(self.fail(
                        ErrorKind::ValueError,
                        "cannot serialize a builtin prototype/namespace to JSON",
                    ));
                }
                let mut map = serde_json::Map::new();
                for (k, v) in obj.map.iter() {
                    // JS: properties whose value is `undefined` are omitted.
                    if matches!(v, Value::Undefined) {
                        continue;
                    }
                    map.insert(k.to_string(), self.stack_value_to_json(v, depth + 1)?);
                }
                serde_json::Value::Object(map)
            }
            Value::RegExp(_) => {
                return Err(self.fail(ErrorKind::ValueError, "cannot serialize a RegExp to JSON"));
            }
            Value::Map(p) => {
                let map = self
                    .maps
                    .get(*p as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                let entries: Result<Vec<_>, _> = map
                    .iter()
                    .map(|(k, v)| {
                        let key_json = self.stack_value_to_json(&k.0, depth + 1)?;
                        let val_json = self.stack_value_to_json(v, depth + 1)?;
                        Ok(serde_json::Value::Array(vec![key_json, val_json]))
                    })
                    .collect();
                serde_json::Value::Array(entries?)
            }
            Value::Set(p) => {
                let set = self
                    .sets
                    .get(*p as usize)
                    .ok_or_else(|| self.fail_invariant(ErrorKind::ValueError, "value error"))?;
                let entries: Result<Vec<_>, _> = set
                    .iter()
                    .map(|k| self.stack_value_to_json(&k.0, depth + 1))
                    .collect();
                serde_json::Value::Array(entries?)
            }
        })
    }

    /// Convert JSON to a stack `Value` (containers allocated in this VM's
    /// heap). Public for the same reason as `stack_value_to_json`: hosts
    /// feed JSON tool results back in via `resolve_promise`/
    /// `reject_promise` (Phase 8 step machine). Start with `depth = 0`.
    pub fn json_to_stack_value(
        &mut self,
        json: &serde_json::Value,
        depth: usize,
    ) -> Result<Value, VMError> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.fail(
                ErrorKind::ValueError,
                format!("cannot parse JSON: nesting exceeds max depth {MAX_JSON_DEPTH}"),
            ));
        }
        Ok(match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                // Mirror serde's own split: non-negative -> PosInt (full u64),
                // negative -> NegInt, fractions -> Number. Check as_u64 first so
                // non-negatives become canonical PosInt.
                if let Some(u) = n.as_u64() {
                    Value::PosInt(u)
                } else if let Some(i) = n.as_i64() {
                    Value::NegInt(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::String(JsString::from(s.as_str())),
            serde_json::Value::Array(arr) => {
                let vals: ThinVec<Value> = arr
                    .iter()
                    .map(|v| self.json_to_stack_value(v, depth + 1))
                    .collect::<Result<_, _>>()?;
                self.alloc_array(vals)
            }
            serde_json::Value::Object(obj) => {
                let mut map = IndexMap::new();
                for (k, v) in obj {
                    map.insert(
                        JsString::from(k.as_str()),
                        self.json_to_stack_value(v, depth + 1)?,
                    );
                }
                self.alloc_object(map)
            }
        })
    }

    #[inline]
    pub(crate) fn validate_func_addr(&self, addr: CodeAddr) -> Result<CodeAddr, VMError> {
        if addr as usize >= self.code.len() {
            return Err(self.fail(ErrorKind::BadCall, "bad call target"));
        }
        Ok(addr)
    }

    #[inline]
    pub(crate) fn validate_jump_addr(&self, addr: CodeAddr) -> Result<CodeAddr, VMError> {
        if addr as usize > self.code.len() {
            return Err(self.fail(ErrorKind::BadCall, "bad jump target"));
        }
        Ok(addr)
    }

    /// Shared dispatch for `CallDyn`/`CallSpread`/bind/`new`. The args
    /// are the top `nargs` stack values (arg 0 deepest). `this_val` is the
    /// receiver (for a user function it becomes the frame field; for a builtin it
    /// is spliced as arg 0). `below` is the number of dead call-group slots the
    /// caller left *just under* the args (the callee value and/or receiver, read
    /// in place rather than shifted out): the `Closure` path reclaims them via
    /// the frame's `reclaim_below` on `Return`, leaving the args untouched; the
    /// rarer `Builtin`/error paths compact them away first.
    pub(crate) fn dispatch_call(
        &mut self,
        callable: Value,
        this_val: Value,
        nargs: u32,
        below: u32,
    ) -> Result<(), VMError> {
        match callable {
            Value::Closure { addr, ptr } => {
                // No shift: args stay on top, the `below` placeholders stay under
                // `fp`, and `Return` truncates to `fp - below`.
                self.call_function(addr, nargs, ptr, this_val, below)?
            }
            Value::Bound(bound) => {
                // Prepend bound args (deeper than the call-site args, which are
                // at top) and recurse with the Bound's own this_val, overriding
                // any call-site receiver. The bound args compose correctly with
                // spreads — the call-site args region (the top `nargs` values)
                // stays contiguous; bound args are inserted just below it.
                let prepend_count = bound.bound_args.len() as u32;
                let insert_idx = self.stack.len() - nargs as usize;
                for (offset, val) in bound.bound_args.iter().enumerate() {
                    self.stack.insert(insert_idx + offset, val.clone());
                }
                self.dispatch_call(
                    bound.callable.clone(),
                    bound.this_val.clone(),
                    nargs + prepend_count,
                    below,
                )?;
            }
            Value::Builtin(b) => {
                // Builtins read args positionally from the top and self-truncate,
                // so the below-args placeholders must go first (rare: a builtin
                // arriving as a runtime value).
                if below > 0 {
                    let args_start = self.stack.len() - nargs as usize;
                    self.stack.drain(args_start - below as usize..args_start);
                }
                let nargs_with_recv = if matches!(this_val, Value::Undefined) {
                    nargs
                } else {
                    let insert_idx = self.stack.len() - nargs as usize;
                    self.stack.insert(insert_idx, this_val.clone());
                    nargs + 1
                };
                // An Object receiver may shadow the builtin method with an
                // own/proto property of the same name; the shared helper
                // resolves and dispatches it, else runs the builtin. (Step 2c:
                // uniform shadowing, replacing the retired `MethodOnObject`.)
                self.call_builtin_or_shadow(b, nargs_with_recv)?;
            }
            _ => {
                let keep = self
                    .stack
                    .len()
                    .saturating_sub(nargs as usize + below as usize);
                self.stack.truncate(keep);
                let msg = format!("cannot call a {} as a function", callable.type_name());
                return Err(self.fail(ErrorKind::TypeError, msg));
            }
        }
        Ok(())
    }

    pub(crate) fn call_function(
        &mut self,
        addr: CodeAddr,
        nargs: u32,
        closure_ptr: ClosurePtr,
        this_val: Value,
        reclaim_below: u32,
    ) -> Result<(), VMError> {
        let addr = self.validate_func_addr(addr)?;
        if nargs as usize > self.stack.len() {
            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
        }
        self.callstack.push(CallFrame {
            arg_count: nargs,
            local_count: nargs,
            return_addr: self.ip + 1,
            prev_fp: self.fp,
            arguments_cache: None,
            pending_closure: closure_ptr,
            this_val,
            new_obj: None,
            reclaim_below,
            completion: Completion::Normal,
        });
        self.ip = addr;
        self.fp = (self.stack.len() as u32) - nargs;
        self.cur_local_count = nargs;
        Ok(())
    }

    /// Execute until an effect, completion, or error. A *catchable* error —
    /// anything but an [`ResumeMode::InvariantViolation`] — raised
    /// while a *reachable* `try` handler is active is materialized as a
    /// `{ name, message }` error object and unwound to the handler instead
    /// of escalating (6_LANGUAGE Part B). Inside a resumed strand with no
    /// reachable handler, the same error rejects the strand's promise
    /// (7_ASYNC Tier 2) — an async call's failure is its promise's
    /// rejection, never an unwind into the parked code below. Everything
    /// else — the invariant violations, where the VM is broken or there is
    /// no execution left — escalates, so a program cannot trap its own kill
    /// switch. The gate is deliberately *not* `is_resumable()`: whether the
    /// failed instruction consumed its operands is the host's concern, and
    /// `unwind_to_handler` truncates the stack to the `try`'s snapshot
    /// regardless. While the two shared an answer, `try { x--; }` on a
    /// non-number died uncaught. `raise` is unaffected: it
    /// yields `StepResult::Raise` (an `Ok`), never an error, so no `try`
    /// can swallow it.
    ///
    /// `fuel` is this call's instruction budget — a slice, not a total.
    /// Running dry yields `Ok(StepResult::OutOfFuel)` with nothing
    /// consumed; call `step` again to continue (`fuel = 0` yields
    /// immediately). The host owns the total per-program budget by
    /// counting slices; debuggers single-step with `fuel = 1` (9_TUI).
    pub fn step(&mut self, mut fuel: u64) -> Result<StepResult, VMError> {
        loop {
            match self.dispatch(&mut fuel) {
                Err(e) if e.resume.is_catchable() => {
                    if self.reachable_handler() {
                        let thrown = self.error_to_thrown(&e);
                        self.unwind_to_handler(thrown);
                    } else if self.in_strand() {
                        let thrown = self.error_to_thrown(&e);
                        self.reject_strand(thrown)?;
                    } else {
                        return Err(e);
                    }
                }
                other => return other,
            }
        }
    }
}

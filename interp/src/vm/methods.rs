use super::*;

use crate::diag::Diagnostic;
use crate::vm::value::MapKey;
use indexmap::IndexSet;
use std::collections::HashMap;

impl VM {
    pub fn new(code: Vec<Instr>) -> Self {
        VM {
            code,
            arrays: Vec::new(),
            objects: Vec::new(),
            closures: Vec::new(),
            maps: Vec::new(),
            sets: Vec::new(),
            cells: Vec::new(),
            promises: Vec::new(),
            outbox: Vec::new(),
            continuations: Vec::new(),
            ready: VecDeque::new(),
            root_ip: 0,
            inflight: 0,
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
                completion: Completion::Normal,
            }],
            ip: 0,
            fp: 0,
            cur_local_count: 0,
            spans: Vec::new(),
            source: Arc::from(""),
            console_lines: Vec::new(),
            debug: crate::debuginfo::DebugTable::default(),
        }
    }

    /// Like `fail` but always sets `NotResumable`. For sites that error
    /// before consuming all instruction operands (peek-style checks) and for
    /// invariant violations (bad heap/cell pointers) where `fail`'s per-kind
    /// default would wrongly mark the error resumable. See the Step 3 audit
    /// table at `ResumeMode`.
    pub fn fail_not_resumable(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError {
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume: ResumeMode::NotResumable,
            payload: None,
        }
    }

    /// Construct an error at the current instruction pointer. Every runtime
    /// error site goes through this (or `fail_not_resumable` / the static
    /// `VMError::fail_at`) so `ip` and `resume` are captured consistently.
    pub fn fail(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError {
        let resume = match kind {
            // Invariant violations: compiler bug or host misuse — never resume.
            ErrorKind::StackUnderflow
            | ErrorKind::BadReturn
            | ErrorKind::BadCall
            | ErrorKind::BadAlloc
            | ErrorKind::BadArg
            | ErrorKind::BadLocal => ResumeMode::NotResumable,
            // TypeError / ValueError: most sites pop operands first (macros,
            // take_args, check_arity!). Default to PushValueThenContinue;
            // specific sites that error before popping override below.
            ErrorKind::TypeError | ErrorKind::ValueError => ResumeMode::PushValueThenContinue,
            // An escaped program-level throw: the operand was consumed, but
            // a `throw` has no result slot a substituted value could fill.
            ErrorKind::UncaughtException => ResumeMode::NotResumable,
            // Circular awaits: every strand is parked, so there is no
            // execution state a substituted value could resume.
            ErrorKind::Deadlock => ResumeMode::NotResumable,
            // Internal control-flow signal, always intercepted at the call site
            // and never surfaced; the mode is irrelevant.
            ErrorKind::MethodOnObject => ResumeMode::NotResumable,
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
    /// handles. If the receiver is an `Object`, raise the
    /// [`ErrorKind::MethodOnObject`] control signal so the call site re-routes
    /// to the object's own same-named property; otherwise a genuine
    /// `TypeError`. Every method-builtin receiver check routes its mismatch arm
    /// through here (directly or via the `Args::*_receiver` helpers), so the
    /// signal is raised *exactly* where we know the receiver is an object —
    /// never inferred from an arbitrary error.
    pub(crate) fn method_receiver_error(&self, recv: &Value) -> VMError {
        match recv {
            Value::Object(_) => self.fail(ErrorKind::MethodOnObject, ""),
            _ => self.fail(ErrorKind::TypeError, "type error"),
        }
    }

    /// Resume after a `Raise`: push the host-chosen result value (ip was
    /// already advanced past the Raise by `step()`).
    pub fn resume_raise(&mut self, value: Value) {
        self.stack.push(value);
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

    /// Materialize a catchable VM error as the plain `{ name, message }`
    /// object a `catch` binding receives: `name` from the error kind,
    /// `message` the fully rendered diagnostic (line/col + source line).
    pub(super) fn error_to_thrown(&mut self, e: &VMError) -> Value {
        let mut obj = IndexMap::new();
        obj.insert(
            RcStr::from("name"),
            Value::String(RcStr::from(format!("{:?}", e.kind).as_str())),
        );
        obj.insert(
            RcStr::from("message"),
            Value::String(RcStr::from(self.render_error(e).as_str())),
        );
        self.alloc_object(obj)
    }

    /// The `{ name: "TypeError", message }` object a promise-chaining cycle
    /// rejects with (the scheduler-side mirror of the `Await` cycle check).
    fn cycle_error_value(&mut self, msg: &str) -> Value {
        let mut obj = IndexMap::new();
        obj.insert(RcStr::from("name"), Value::String(RcStr::from("TypeError")));
        obj.insert(RcStr::from("message"), Value::String(RcStr::from(msg)));
        self.alloc_object(obj)
    }

    /// Render an uncaught thrown value: an `{ name, message }` error object
    /// (the `new Error(...)` shape) formats as `uncaught {name}: {message}`;
    /// anything else falls back to a preview of the value.
    pub(super) fn uncaught_message(&self, value: &Value) -> String {
        if let Value::Object(p) = value {
            if let Some(obj) = self.objects.get(*p as usize) {
                if let (Some(Value::String(name)), Some(Value::String(msg))) =
                    (obj.map.get("name"), obj.map.get("message"))
                {
                    return format!("uncaught {}: {}", name.as_str(), msg.as_str());
                }
            }
        }
        format!("uncaught exception: {}", self.preview(value))
    }

    /// Apply the resume fixup for a PushValueThenContinue error:
    /// push `value`, advance ip past the failed instruction.
    /// Errors if this error's resume mode is not `PushValueThenContinue`.
    pub fn resume_with(&mut self, e: &VMError, value: Value) -> Result<(), VMError> {
        if !matches!(e.resume, ResumeMode::PushValueThenContinue) {
            return Err(self.fail(
                ErrorKind::BadArg,
                format!(
                    "cannot resume: error {:?} is not PushValueThenContinue",
                    e.kind
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
                let s = s.as_str();
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
                    let mut keys: Vec<&str> = obj.map.keys().map(|k| k.as_str()).collect();
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
                format!("/{}/{}", r.pattern.as_str(), r.flags.as_str())
            }
            other => other.type_name().to_string(),
        }
    }

    /// Render an error against the VM's source (when available). Falls back
    /// to a plain "at instruction {ip}" format when spans/source are empty
    /// (hand-assembled code via `VM::new`).
    pub fn render_error(&self, e: &VMError) -> String {
        let ip = e.ip as usize;
        if ip < self.spans.len() && !self.source.is_empty() {
            let span = self.spans[ip];
            let diag = Diagnostic {
                span,
                message: e.message.clone(),
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
        // Allocate one canonical `Closure` per unique `PushFn` code address
        // for non-capturing functions, so that `f === f` holds (same ptr every
        // push). The ptr is baked into the instruction; no runtime addr→ptr map.
        {
            let mut canonical: std::collections::HashMap<CodeAddr, ClosurePtr> =
                std::collections::HashMap::new();
            for instr in &mut vm.code {
                if let Instr::PushFn(addr, ptr) = instr {
                    let cptr = *canonical.entry(*addr).or_insert_with(|| {
                        let idx = vm.closures.len() as ClosurePtr;
                        vm.closures.push(Closure {
                            upvals: ThinVec::new(),
                            prototype: None,
                        });
                        idx
                    });
                    *ptr = cptr;
                }
            }
        }
        vm.spans = program.spans;
        vm.source = program.source;
        vm.debug = program.debug;
        // Reserve the two fixed slots before seeding either's nested values.
        vm.objects.push(ObjData {
            proto: None,
            map: IndexMap::new(),
        }); // objects[0] = input
        vm.objects.push(ObjData {
            proto: None,
            map: IndexMap::new(),
        }); // objects[1] = attachments
        let input_entries = vm.seed_const_object(input)?;
        let attachment_entries = vm.seed_const_object(attachments)?;
        vm.objects[0] = ObjData {
            proto: None,
            map: input_entries,
        };
        vm.objects[1] = ObjData {
            proto: None,
            map: attachment_entries,
        };
        Ok(vm)
    }

    /// Build the entry map for a host-seeded const from a JSON object;
    /// nested arrays/objects allocate into `objects` (addresses computed at
    /// runtime). A `Null`/non-object seed yields an empty map.
    fn seed_const_object(
        &mut self,
        json: serde_json::Value,
    ) -> Result<IndexMap<RcStr, Value>, VMError> {
        let serde_json::Value::Object(map) = json else {
            return Ok(IndexMap::new());
        };
        let mut entries = IndexMap::with_capacity(map.len());
        for (k, v) in &map {
            let sv = self.json_to_stack_value(v, 0)?;
            entries.insert(RcStr::from(k.as_str()), sv);
        }
        Ok(entries)
    }

    // ── debugger introspection (9_TUI Step 1) ────────────────────────

    /// The function owning the instruction at `ip` (debug-table index and
    /// entry): the innermost function whose source span contains
    /// `spans[ip]`. `None` without debug info (`VM::new` programs).
    pub fn function_at(&self, ip: CodeAddr) -> Option<(usize, &crate::debuginfo::FnDebug)> {
        let span = *self.spans.get(ip as usize)?;
        let idx = self.debug.function_at_span(span)?;
        Some((idx, &self.debug.functions[idx]))
    }

    /// Read-only views of the live call frames, outermost (root) first.
    /// Parked async continuations are not on the callstack and do not
    /// appear; this is the running strand's stack.
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
            if let Some((idx, f)) = self.function_at(ip) {
                if idx != cur_fn {
                    out.push_str("── ");
                    out.push_str(&f.name);
                    out.push_str(" ──\n");
                    cur_fn = idx;
                }
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
                let (line, _, _) = crate::diag::line_col(&self.source, sp);
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

    /// Allocate a fresh `Pending` promise. Used by `Instr::Invoke` (leaf tool
    /// promises) and by first suspension of an async call (Tier 2).
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
                return Err(self
                    .fail_not_resumable(ErrorKind::BadArg, "cannot settle a promise to Pending"));
            }
        };
        match self.promises.get(id as usize) {
            Some(PromiseState::Pending { .. }) => {}
            Some(_) => {
                return Err(self.fail_not_resumable(
                    ErrorKind::BadArg,
                    format!("promise {id} is already settled"),
                ));
            }
            None => {
                return Err(
                    self.fail_not_resumable(ErrorKind::BadArg, format!("bad promise id {id}"))
                );
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

    /// Whether execution is currently inside a scheduler-resumed strand.
    /// Resumed frames are only ever pushed while the root frame alone is
    /// live, so a strand's base — when one exists — is `callstack[1]`.
    pub(super) fn in_strand(&self) -> bool {
        self.callstack.len() > 1
            && matches!(self.callstack[1].completion, Completion::ResolvePromise(_))
    }

    /// Whether the innermost `try` handler may catch a throw from the
    /// current position. Inside a resumed strand, the parked root strand's
    /// handlers (entries pushed at top level, `callstack_len == 1`) are
    /// walled off: a throw escaping the strand rejects its promise instead
    /// of unwinding into code that isn't executing.
    pub(super) fn reachable_handler(&self) -> bool {
        match self.handlers.last() {
            None => false,
            Some(h) => !self.in_strand() || h.callstack_len > 1,
        }
    }

    /// Tier 2 suspension: at a pending `await` in an async function frame,
    /// copy the frame (stack region + metadata + its own handler entries)
    /// into a continuation record registered as a waiter on `awaiting`, and
    /// pop the frame, reusing the `Return` machinery. On *first* suspension
    /// (frame entered by a direct call) a fresh promise is pushed onto the
    /// caller's stack as the call's return value — the caller, sync or
    /// async, just continues. On *re-suspension* (frame entered by scheduler
    /// resume) control falls through to the scheduler.
    pub(super) fn suspend_current_frame(&mut self, awaiting: PromisePtr) -> Result<(), VMError> {
        // The Await consumes its operand: the promise leaves the stack now;
        // resume pushes the settled value in its place and continues past
        // the Await.
        self.stack.pop();
        let resume_ip = self.ip + 1;
        let await_span = self.spans.get(self.ip as usize).copied().unwrap_or(0);
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
        let (promise, first_suspension) = match frame.completion {
            Completion::Normal => (self.alloc_promise(), true),
            Completion::ResolvePromise(pid) => (pid, false),
        };
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
                    self.fail_not_resumable(ErrorKind::ValueError, "bad awaited promise pointer")
                );
            }
        }
        if first_suspension {
            self.stack.push(Value::Promise(promise));
            self.ip = frame.return_addr;
        } else {
            self.schedule()?;
        }
        Ok(())
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
                        return Err(self.fail_not_resumable(
                            ErrorKind::ValueError,
                            "bad adopted promise pointer",
                        ));
                    }
                }
            }
            let cont = self
                .continuations
                .get_mut(id as usize)
                .and_then(Option::take)
                .ok_or_else(|| {
                    self.fail_not_resumable(
                        ErrorKind::ValueError,
                        format!("bad continuation id {id}"),
                    )
                })?;
            // A rejection arriving at a frame with no handler around its
            // await needs no frame materialization: the rejection
            // propagates straight to this call's own promise.
            if let ResumePayload::Rejected(errval) = &payload {
                if cont.saved_handlers.is_empty() {
                    self.settle_and_wake(cont.promise, PromiseState::Rejected(errval.clone()))?;
                    continue;
                }
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
    /// current stack top in `ResolvePromise` completion mode (a resumed
    /// frame has no caller below it), re-base its saved handler entries,
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
            completion: Completion::ResolvePromise(cont.promise),
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

    /// An uncaught throw / rejection / catchable VM error escaping a
    /// resumed strand: reject the strand's promise (waking waiters),
    /// discard the strand's frames and handler entries, and fall through
    /// to the scheduler. The parked root region below is untouched.
    pub(super) fn reject_strand(&mut self, errval: Value) -> Result<(), VMError> {
        let Completion::ResolvePromise(pid) = self.callstack[1].completion else {
            return Err(
                self.fail_not_resumable(ErrorKind::BadReturn, "reject_strand outside a strand")
            );
        };
        // The strand base frame's fp: the current fp when no sync frames
        // sit above it, else the first such frame's saved prev_fp.
        let strand_fp = if self.callstack.len() > 2 {
            self.callstack[2].prev_fp
        } else {
            self.fp
        };
        self.stack.truncate(strand_fp as usize);
        self.fp = self.callstack[1].prev_fp;
        self.callstack.truncate(1);
        self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
        while self.handlers.last().is_some_and(|h| h.callstack_len > 1) {
            self.handlers.pop();
        }
        self.settle_and_wake(pid, PromiseState::Rejected(errval))?;
        self.schedule()
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

    /// Push a string value onto the stack. Strings live inline as `RcStr`, not
    /// in `heap`, so this is just a stack push (no heap slot, no growth). The
    /// builtin/string-producing counterpart to `alloc_array`/`alloc_object`.
    pub(crate) fn push_str_value(&mut self, s: impl Into<RcStr>) {
        self.stack.push(Value::String(s.into()));
    }

    pub(crate) fn alloc_array(&mut self, arr: ThinVec<Value>) -> Value {
        let addr = self.arrays.len() as ArrayPtr;
        self.arrays.push(arr);
        Value::Array(addr)
    }

    pub(crate) fn alloc_object(&mut self, obj: IndexMap<FieldName, Value>) -> Value {
        let addr = self.objects.len() as ObjectPtr;
        self.objects.push(ObjData {
            proto: None,
            map: obj,
        });
        Value::Object(addr)
    }

    /// Walk own `map` → `proto` chain → `Undefined`. Own hit returns
    /// immediately; `proto: None` returns `Undefined` with one branch and
    /// never enters the loop. A depth cap guards against malformed cycles.
    /// Stack effect: none (pure property resolution).
    pub(crate) fn resolve_proto_chain(
        &self,
        obj_ptr: ObjectPtr,
        field: &str,
    ) -> Result<Value, VMError> {
        const MAX_PROTO_DEPTH: u32 = 100;
        let mut cur = obj_ptr;
        for _depth in 0..MAX_PROTO_DEPTH {
            let obj = self.objects.get(cur as usize).ok_or_else(|| {
                self.fail_not_resumable(ErrorKind::TypeError, "bad object pointer")
            })?;
            if let Some(v) = obj.map.get(field) {
                return Ok(v.clone());
            }
            match obj.proto {
                Some(parent) => cur = parent,
                None => return Ok(Value::Undefined),
            }
        }
        Ok(Value::Undefined)
    }

    pub(crate) fn alloc_map(&mut self, map: IndexMap<MapKey, Value>) -> Value {
        let addr = self.maps.len() as MapPtr;
        self.maps.push(map);
        Value::Map(addr)
    }

    pub(crate) fn alloc_set(&mut self, set: IndexSet<MapKey>) -> Value {
        let addr = self.sets.len() as SetPtr;
        self.sets.push(set);
        Value::Set(addr)
    }

    pub(super) fn alloc_closure(&mut self, addr: CodeAddr, upvals: ThinVec<Value>) -> Value {
        let idx = self.closures.len() as ClosurePtr;
        self.closures.push(Closure {
            upvals,
            prototype: None,
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
            Value::String(s) => buf.push_str(s.as_str()),
            Value::Closure { .. } | Value::Builtin(_) => {
                buf.push_str("function () { [native code] }");
            }
            Value::Upval(_) => {}
            Value::Array(p) => {
                if let Some(arr) = self.arrays.get(*p as usize) {
                    for (i, v) in arr.iter().enumerate() {
                        if i > 0 {
                            buf.push_str(",");
                        }
                        match v {
                            Value::Null | Value::Undefined => {}
                            _ => self.write_js_string(v, depth + 1, buf),
                        }
                    }
                }
            }
            Value::Object(_) => buf.push_str("[object Object]"),
            Value::Promise(_) => buf.push_str("[object Promise]"),
            Value::RegExp(r) => {
                buf.push('/');
                buf.push_str(r.pattern.as_str());
                buf.push('/');
                if !r.flags.as_str().is_empty() {
                    buf.push_str(r.flags.as_str());
                }
            }
            Value::Map(_) => buf.push_str("[object Map]"),
            Value::Set(_) => buf.push_str("[object Set]"),
        }
    }

    /// JS `String(x)` / `ToString`. Delegates to [`write_js_string`], assembling
    /// in a growable `String` and freezing to an immutable `RcStr` once.
    pub(crate) fn to_js_string(&self, val: &Value, depth: usize) -> RcStr {
        // Fast path: an existing string is already an `RcStr` — share it (a
        // refcount bump) instead of copying its bytes through a fresh buffer.
        if let Value::String(s) = val {
            return s.clone();
        }
        let mut out = String::new();
        self.write_js_string(val, depth, &mut out);
        RcStr::from(out)
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
    pub(super) fn pop_string(&mut self) -> Result<RcStr, VMError> {
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
    pub(crate) fn str_from<'a>(&self, val: &'a Value) -> Result<&'a str, VMError> {
        match val {
            Value::String(s) => Ok(s.as_str()),
            _ => Err(self.fail(ErrorKind::TypeError, "type error")),
        }
    }

    /// Extract an owned `RcStr` from an already-popped string value — a refcount
    /// bump, sharing the same allocation. The clone sibling of `str_from`; use
    /// it when the builtin must retain the string past a borrow of the VM.
    pub(crate) fn string_from(&self, val: &Value) -> Result<RcStr, VMError> {
        match val {
            Value::String(s) => Ok(s.clone()),
            _ => Err(self.fail(ErrorKind::TypeError, "type error")),
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
            Value::Closure { .. } | Value::Builtin(_) | Value::Upval(_) => {
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
            Value::Undefined => return Err(self.fail(ErrorKind::ValueError, "value error")),
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
            Value::String(s) => serde_json::Value::String(s.as_str().to_owned()),
            Value::Array(p) => {
                let arr = self
                    .arrays
                    .get(*p as usize)
                    .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
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
                    .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
                let mut map = serde_json::Map::new();
                for (k, v) in obj.map.iter() {
                    // JS: properties whose value is `undefined` are omitted.
                    if matches!(v, Value::Undefined) {
                        continue;
                    }
                    map.insert(
                        k.as_str().to_owned(),
                        self.stack_value_to_json(v, depth + 1)?,
                    );
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
                    .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
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
                    .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
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
            serde_json::Value::String(s) => Value::String(RcStr::from(s.as_str())),
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
                        RcStr::from(k.as_str()),
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

    /// Re-route a method-builtin call that landed on an `Object` receiver to
    /// that object's own same-named property. Invoked when `Builtin::call`
    /// raised [`ErrorKind::MethodOnObject`]: the receiver (arg 0) is an
    /// `Object`, and the args `[recv, arg1, …]` are still on the stack (the
    /// builtin epilogue forwarded the signal without truncating). Drops the
    /// receiver and dispatches the property as a function — this is where
    /// shadowing is actually confirmed — or, if the object has no such
    /// property, raises a real `TypeError`. Delegates `ip` management to
    /// `dispatch_call`, so both call sites compose correctly.
    pub(crate) fn reroute_method_to_object(
        &mut self,
        b: crate::builtin::Builtin,
        argc: u32,
        _this_val: Value,
    ) -> Result<(), VMError> {
        // Invariant: the signalling handler raised `MethodOnObject` at its
        // receiver check, before touching the stack — so all `argc` args
        // (receiver + explicit args) are still present. If a handler ever
        // mutates the stack before signalling, this underflows; assert loudly
        // in debug builds rather than silently rerouting a corrupt frame.
        debug_assert!(
            self.stack.len() >= argc as usize,
            "method handler must raise MethodOnObject before mutating the stack"
        );
        let base = self.stack.len() - argc as usize;
        let name = b.meta().name;
        let recv_ptr = match &self.stack[base] {
            Value::Object(p) => *p,
            // The signal is only ever raised for an Object receiver.
            _ => return Err(self.fail_not_resumable(ErrorKind::BadCall, "reroute: non-object")),
        };
        let method = self
            .objects
            .get(recv_ptr as usize)
            .and_then(|o| o.map.get(name))
            .cloned();
        match method {
            Some(f) => {
                let recv = self.stack.remove(base); // drop the receiver; args shift down
                self.dispatch_call(f, recv, argc - 1)
            }
            None => {
                self.stack.truncate(base);
                let msg = format!("'{name}' is not a method of this object");
                Err(self.fail(ErrorKind::TypeError, msg))
            }
        }
    }

    /// Shared dispatch for `CallDyn` and `CallSpread`: the args are already
    /// on the stack in left-to-right order (arg 0 deepest), with the callable
    /// already popped.  `this_val` is the receiver for user functions/closures
    /// (set as the frame field); for builtins it is spliced as arg 0.
    /// Handles `Builtin`, `Closure`, and non-callable.
    pub(crate) fn dispatch_call(
        &mut self,
        callable: Value,
        this_val: Value,
        nargs: u32,
    ) -> Result<(), VMError> {
        match callable {
            Value::Builtin(b) => {
                let nargs_with_recv = if matches!(this_val, Value::Undefined) {
                    nargs
                } else {
                    let insert_idx = self.stack.len() - nargs as usize;
                    self.stack.insert(insert_idx, this_val.clone());
                    nargs + 1
                };
                match b.call(self, nargs_with_recv) {
                    Ok(()) => self.ip += 1,
                    Err(e) if e.kind == ErrorKind::MethodOnObject => {
                        self.reroute_method_to_object(b, nargs, this_val)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            Value::Closure { addr, ptr } => self.call_function(addr, nargs, ptr, this_val)?,
            _ => {
                let keep = self.stack.len().saturating_sub(nargs as usize);
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
            completion: Completion::Normal,
        });
        self.ip = addr;
        self.fp = (self.stack.len() as u32) - nargs;
        self.cur_local_count = nargs;
        Ok(())
    }

    /// Execute until an effect, completion, or error. A *catchable* error —
    /// a `TypeError`/`ValueError` whose operands were fully consumed
    /// (`PushValueThenContinue`, the Phase 3 pop-first invariant) — raised
    /// while a *reachable* `try` handler is active is materialized as a
    /// `{ name, message }` error object and unwound to the handler instead
    /// of escalating (6_LANGUAGE Part B). Inside a resumed strand with no
    /// reachable handler, the same error rejects the strand's promise
    /// (7_ASYNC Tier 2) — an async call's failure is its promise's
    /// rejection, never an unwind into the parked code below. Everything
    /// else (`NotResumable` invariant errors) escalates as before, so a
    /// program cannot trap its own kill switch. `raise` is unaffected: it
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
                Err(e) if matches!(e.resume, ResumeMode::PushValueThenContinue) => {
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

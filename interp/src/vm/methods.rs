use super::*;

use crate::diag::Diagnostic;

impl VM {
    pub fn new(code: Vec<Instr>) -> Self {
        VM {
            code,
            arrays: Vec::new(),
            objects: Vec::new(),
            closures: Vec::new(),
            cells: Vec::new(),
            promises: Vec::new(),
            outbox: Vec::new(),
            handlers: Vec::new(),
            stack: Vec::new(),
            // Root frame so that Local is valid from the start.
            callstack: vec![CallFrame {
                arg_count: 0,
                local_count: 0,
                return_addr: 0,
                prev_fp: 0,
                arguments_cache: None,
                pending_upvals: SmallVec::new(),
            }],
            ip: 0,
            fp: 0,
            cur_local_count: 0,
            fuel: DEFAULT_FUEL,
            spans: Vec::new(),
            source: Arc::from(""),
            console_lines: Vec::new(),
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
            // OutOfFuel: nothing was consumed; fix fuel and step() again.
            ErrorKind::OutOfFuel => ResumeMode::RetrySameInstr,
            // TypeError / ValueError: most sites pop operands first (macros,
            // take_args, check_arity!). Default to PushValueThenContinue;
            // specific sites that error before popping override below.
            ErrorKind::TypeError | ErrorKind::ValueError => ResumeMode::PushValueThenContinue,
            // An escaped program-level throw: the operand was consumed, but
            // a `throw` has no result slot a substituted value could fill.
            ErrorKind::UncaughtException => ResumeMode::NotResumable,
        };
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume,
            payload: None,
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

    /// Render an uncaught thrown value: an `{ name, message }` error object
    /// (the `new Error(...)` shape) formats as `uncaught {name}: {message}`;
    /// anything else falls back to a preview of the value.
    pub(super) fn uncaught_message(&self, value: &Value) -> String {
        if let Value::Object(p) = value {
            if let Some(obj) = self.objects.get(*p as usize) {
                if let (Some(Value::String(name)), Some(Value::String(msg))) =
                    (obj.get("name"), obj.get("message"))
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
                    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
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

    /// Construct a VM to run a compiled `Program`, with the host-seeded `input`
    /// value installed at `objects[0]`. `input` is the read-only const available
    /// to the program; `Null`/non-object seeds yield an empty object.
    /// `objects[0]` is the only pre-seeded slot and `Object(0)` stays stable
    /// for the whole program.
    pub fn for_program(program: Program, input: serde_json::Value) -> Result<Self, VMError> {
        let mut vm = VM::new(program.code);
        vm.spans = program.spans;
        vm.source = program.source;
        // Reserve objects[0] for `input` (filled in just below).
        vm.objects.push(IndexMap::new());
        // Seed input's nested values (arrays/objects land at objects[1..]; their
        // addresses are computed at runtime and stored in the input map).
        if let serde_json::Value::Object(map) = input {
            let mut entries = IndexMap::with_capacity(map.len());
            for (k, v) in &map {
                let sv = vm.json_to_stack_value(v, 0)?;
                entries.insert(RcStr::from(k.as_str()), sv);
            }
            if let Some(o) = vm.objects.get_mut(0) {
                *o = entries;
            }
        }
        Ok(vm)
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
        self.settle_promise(id, PromiseState::Resolved(value))
    }

    /// Settle a promise as rejected, with the error value the program's
    /// `await` will escalate. Same contract as [`VM::resolve_promise`].
    pub fn reject_promise(&mut self, id: PromisePtr, errval: Value) -> Result<(), VMError> {
        self.settle_promise(id, PromiseState::Rejected(errval))
    }

    fn settle_promise(&mut self, id: PromisePtr, settled: PromiseState) -> Result<(), VMError> {
        match self.promises.get_mut(id as usize) {
            Some(state @ PromiseState::Pending { .. }) => {
                *state = settled;
                Ok(())
            }
            Some(_) => Err(self.fail_not_resumable(
                ErrorKind::BadArg,
                format!("promise {id} is already settled"),
            )),
            None => Err(self.fail_not_resumable(ErrorKind::BadArg, format!("bad promise id {id}"))),
        }
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

    pub(super) fn alloc_object(&mut self, obj: IndexMap<FieldName, Value>) -> Value {
        let addr = self.objects.len() as ObjectPtr;
        self.objects.push(obj);
        Value::Object(addr)
    }

    pub(super) fn alloc_closure(&mut self, addr: CodeAddr, upvals: ThinVec<Value>) -> Value {
        let idx = self.closures.len() as ClosurePtr;
        self.closures.push(Closure { addr, upvals });
        Value::Closure(idx)
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
            Value::Fn(_) | Value::Builtin(_) => {
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
            Value::Closure(_) => {
                buf.push_str("function () { [native code] }");
            }
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

    pub(crate) fn stack_value_to_json(
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
            Value::Fn(_) | Value::Builtin(_) | Value::Upval(_) => {
                return Err(self.fail(
                    ErrorKind::ValueError,
                    format!("cannot serialize a {} to JSON", val.type_name()),
                ));
            }
            // A promise is a transient value (like Fn/Closure) with no JSON
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
                for (k, v) in obj.iter() {
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
            // A closure has no JSON representation (see Fn above).
            Value::Closure(_) => return Err(self.fail(ErrorKind::ValueError, "value error")),
        })
    }

    pub(crate) fn json_to_stack_value(
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

    /// Shared dispatch for `CallDyn` and `CallSpread`: the args are already
    /// on the stack in left-to-right order (arg 0 deepest), with the callable
    /// already popped.  Handles `Builtin`, `Fn`, `Closure`, and non-callable.
    pub(crate) fn dispatch_call(&mut self, callable: Value, nargs: u32) -> Result<(), VMError> {
        match callable {
            Value::Builtin(b) => {
                b.call(self, nargs)?;
                self.ip += 1;
            }
            Value::Fn(addr) => self.call_function(addr, nargs, SmallVec::new())?,
            Value::Closure(p) => {
                let closure = self.closures.get(p as usize).ok_or_else(|| {
                    self.fail_not_resumable(ErrorKind::ValueError, "bad closure pointer")
                })?;
                let addr = closure.addr;
                let upvals: SmallVec<[Value; 8]> = closure.upvals.iter().cloned().collect();
                self.call_function(addr, nargs, upvals)?
            }
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
        upvals: SmallVec<[Value; 8]>,
    ) -> Result<(), VMError> {
        let addr = self.validate_func_addr(addr)?;
        if nargs as usize > self.stack.len() {
            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
        }
        // `fp` points at arg 0: the args ARE the callee's leading
        // locals (slots 0..nargs). The prologue `EnterFrame` then
        // normalizes them to exactly `nparams`. No copy.
        self.callstack.push(CallFrame {
            arg_count: nargs,
            local_count: nargs,
            return_addr: self.ip + 1,
            prev_fp: self.fp,
            arguments_cache: None,
            pending_upvals: upvals,
        });
        self.ip = addr;
        self.fp = (self.stack.len() as u32) - nargs;
        self.cur_local_count = nargs;
        Ok(())
    }

    /// Execute until an effect, completion, or error. A *catchable* error —
    /// a `TypeError`/`ValueError` whose operands were fully consumed
    /// (`PushValueThenContinue`, the Phase 3 pop-first invariant) — raised
    /// while a `try` handler is active is materialized as a
    /// `{ name, message }` error object and unwound to the handler instead
    /// of escalating (6_LANGUAGE Part B). Everything else (`OutOfFuel`,
    /// `NotResumable` invariant errors) escalates as before, so a program
    /// cannot trap its own kill switch. `raise` is unaffected: it yields
    /// `StepResult::Raise` (an `Ok`), never an error, so no `try` can
    /// swallow it.
    pub fn step(&mut self) -> Result<StepResult, VMError> {
        loop {
            match self.dispatch() {
                Err(e)
                    if matches!(e.resume, ResumeMode::PushValueThenContinue)
                        && !self.handlers.is_empty() =>
                {
                    let thrown = self.error_to_thrown(&e);
                    self.unwind_to_handler(thrown);
                }
                other => return other,
            }
        }
    }
}

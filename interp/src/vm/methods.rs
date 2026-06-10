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
        }
    }

    /// Construct an error from the current instruction pointer. Every runtime
    /// error site must go through this helper so `ip` and `resume` are
    /// captured consistently. The `resume` field defaults to `NotResumable`;
    /// sites that qualify for a softer mode override it after the fact
    /// (see Step 3 audit).
    /// Like `fail` but always sets `NotResumable`. For sites that error before
    /// consuming all instruction operands (peek-style checks, etc.).
    pub fn fail_not_resumable(&self, kind: ErrorKind, msg: impl Into<String>) -> VMError {
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume: ResumeMode::NotResumable,
        }
    }

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
        };
        VMError {
            kind,
            ip: self.ip,
            message: msg.into(),
            resume,
        }
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
                    format!("\"{}…\"", s[..40].escape_debug())
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

    /// If the instruction at `ip` is an `Invoke`, return its name and arg
    /// count as owned values (releasing the borrow on `self.code` so the
    /// caller can mutate the stack while batching consecutive invokes).
    pub(super) fn invoke_at(&self, ip: CodeAddr) -> Option<(String, u32)> {
        match self.code.get(ip as usize) {
            Some(Instr::Invoke(name, nargs)) => Some((name.as_str().to_owned(), *nargs)),
            _ => None,
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
            return Err(self.fail(ErrorKind::ValueError, "value error"));
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
                return Err(self.fail(ErrorKind::ValueError, "value error"));
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
            return Err(self.fail(ErrorKind::ValueError, "value error"));
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
}

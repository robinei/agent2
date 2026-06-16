use super::*;

/// The missing-`await` hint, appended to property/index access errors when
/// the receiver is a promise — the misuse LLMs actually commit under this
/// dialect (`tools.f(x).field` instead of `(await tools.f(x)).field`).
fn await_hint(v: &Value) -> &'static str {
    match v {
        Value::Promise(_) => " (did you forget `await`?)?",
        _ => "",
    }
}

/// Look up a named property on a RegExp value. Returns the JS-standard
/// properties that would be on `RegExp.prototype`.
fn regexp_prop(r: &RcRegExp, field: &str) -> Value {
    match field {
        "source" => Value::String(r.pattern.clone()),
        "flags" => Value::String(r.flags.clone()),
        "global" => Value::Bool(r.flags.contains('g')),
        "ignoreCase" => Value::Bool(r.flags.contains('i')),
        "multiline" => Value::Bool(r.flags.contains('m')),
        "dotAll" => Value::Bool(r.flags.contains('s')),
        "unicode" => Value::Bool(r.flags.contains('u')),
        "sticky" => Value::Bool(r.flags.contains('y')),
        "lastIndex" => Value::PosInt(r.last_index.get() as u64),
        _ => Value::Undefined,
    }
}

impl VM {
    pub(crate) fn dispatch(&mut self, fuel: &mut u64) -> Result<StepResult, VMError> {
        // ── macros for repetitive instruction shapes ─────────────────

        /// Pop one operand, coerce ToNumber (JS), apply f64→f64, push Number.
        /// A non-coercible operand (array/object/function) is a TypeError; an
        /// `undefined` or unparseable string coerces to NaN and propagates.
        macro_rules! unary_num {
            ($op:expr) => {{
                let val = self.pop()?;
                match val.to_number() {
                    Some(n) => {
                        self.stack.push(Value::Float($op(n)));
                        self.ip += 1;
                    }
                    None => {
                        let msg = format!(
                            "cannot coerce {} ({}) to number",
                            val.type_name(),
                            self.preview(&val)
                        );
                        return Err(self.fail(ErrorKind::TypeError, msg));
                    }
                }
            }};
        }

        /// Pop rhs then lhs, coerce both ToNumber (JS), apply f64→f64→f64, push
        /// Number. Strings/booleans/null coerce; arrays/objects/functions are a
        /// TypeError; undefined/unparseable strings become NaN.
        macro_rules! binary_num {
            ($op:expr) => {{
                let rhs = self.pop()?;
                let lhs = self.pop()?;
                match (lhs.to_number(), rhs.to_number()) {
                    (Some(a), Some(b)) => {
                        self.stack.push(Value::Float($op(a, b)));
                        self.ip += 1;
                    }
                    _ => {
                        let msg = format!(
                            "cannot coerce {} ({}) and {} ({}) to number",
                            lhs.type_name(),
                            self.preview(&lhs),
                            rhs.type_name(),
                            self.preview(&rhs)
                        );
                        return Err(self.fail(ErrorKind::TypeError, msg));
                    }
                }
            }};
        }

        /// Pop rhs then lhs (integer-valued: Int, or integer-valued Number),
        /// apply i64→i64→i64, push Number.
        macro_rules! binary_int {
            ($op:expr) => {{
                let rhs = self.pop()?;
                let lhs = self.pop()?;
                match (lhs.as_i64(), rhs.as_i64()) {
                    (Some(a), Some(b)) => {
                        self.stack.push(Value::Float($op(a, b) as f64));
                        self.ip += 1;
                    }
                    _ => {
                        let msg = format!(
                            "expected integer operands, got {} ({}) and {} ({})",
                            lhs.type_name(),
                            self.preview(&lhs),
                            rhs.type_name(),
                            self.preview(&rhs)
                        );
                        return Err(self.fail(ErrorKind::TypeError, msg));
                    }
                }
            }};
        }

        /// Pop rhs then lhs, compare with self.compare(), push Bool.
        macro_rules! cmp_op {
            ($cmp:tt $expected:ident) => {{
                let rhs = self.pop()?;
                let lhs = self.pop()?;
                let result = lhs
                    .compare(&rhs)
                    .map(|ord| ord $cmp std::cmp::Ordering::$expected)
                    .unwrap_or(false);
                self.stack.push(Value::Bool(result));
                self.ip += 1;
            }};
        }

        // ── main dispatch loop ───────────────────────────────────────

        loop {
            if self.ip as usize >= self.code.len() {
                return Ok(StepResult::Done {
                    value: Value::Undefined,
                    unstarted: std::mem::take(&mut self.outbox),
                });
            }
            if *fuel == 0 {
                return Ok(StepResult::OutOfFuel);
            }
            *fuel -= 1;
            match &self.code[self.ip as usize] {
                // ── stack manipulation ───────────────────────────
                Instr::PushUndefined => {
                    self.stack.push(Value::Undefined);
                    self.ip += 1;
                }
                Instr::PushNull => {
                    self.stack.push(Value::Null);
                    self.ip += 1;
                }
                Instr::PushBool(b) => {
                    self.stack.push(Value::Bool(*b));
                    self.ip += 1;
                }
                Instr::PushPosInt(u) => {
                    self.stack.push(Value::PosInt(*u));
                    self.ip += 1;
                }
                Instr::PushNegInt(i) => {
                    self.stack.push(Value::NegInt(*i));
                    self.ip += 1;
                }
                Instr::PushFloat(f) => {
                    self.stack.push(Value::Float(*f));
                    self.ip += 1;
                }
                Instr::PushStr(s) => {
                    self.stack.push(Value::String(s.clone()));
                    self.ip += 1;
                }
                Instr::PushArray(h) => {
                    self.stack.push(Value::Array(*h));
                    self.ip += 1;
                }
                Instr::PushObject(h) => {
                    self.stack.push(Value::Object(*h));
                    self.ip += 1;
                }
                Instr::PushFn(addr) => {
                    self.stack.push(Value::Fn(*addr));
                    self.ip += 1;
                }
                Instr::PushBuiltin(b) => {
                    self.stack.push(Value::Builtin(*b));
                    self.ip += 1;
                }

                Instr::Pop(n) => {
                    // Only expression temporaries may be popped, never locals
                    // or args belonging to the current/caller frame.
                    if self.stack.len() < self.frame_floor() + *n {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    self.stack.truncate(self.stack.len() - n);
                    self.ip += 1;
                }

                Instr::Pick(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    // The picked value sits at len-1-n; it (and everything above)
                    // must be a temporary, not a local/arg.
                    if len < self.frame_floor() + n + 1 {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let val = self.stack[len - 1 - n].clone();
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::Dig(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    if len < self.frame_floor() + n + 1 {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    // Fast paths for the common small-n cases, matching the
                    // performance of the former Dup/Swap/Rot dispatch.
                    match n {
                        0 => {} // no-op
                        1 => self.stack.swap(len - 1, len - 2),
                        2 => {
                            self.stack.swap(len - 3, len - 2);
                            self.stack.swap(len - 2, len - 1);
                        }
                        _ => {
                            let val = self.stack.remove(len - 1 - n);
                            self.stack.push(val);
                        }
                    }
                    self.ip += 1;
                }

                Instr::Nip(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    if len < self.frame_floor() + n + 1 {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    // Remove n values directly below the top, leaving the top
                    // in place. Nip(1) ≡ Dig(1); Pop; Nip(n) is the inverse of
                    // Dig(n): where Dig moves element len-1-n to the top,
                    // Nip drops it.
                    let start = len - 1 - n;
                    self.stack.drain(start..start + n);
                    self.ip += 1;
                }

                // ── control flow ─────────────────────────────────
                Instr::Call(addr, nargs) => self.call_function(*addr, *nargs, SmallVec::new())?,

                Instr::CallDyn(nargs) => {
                    let nargs = *nargs;
                    let callable = self.pop()?;
                    self.dispatch_call(callable, nargs)?;
                }

                Instr::CallBuiltin(b, argc) => {
                    let b = *b;
                    let argc = *argc;
                    // Happy path: the builtin runs. If it lands on an Object
                    // receiver (the `MethodOnObject` signal), the args are still
                    // on the stack — re-route the call to the object's own
                    // same-named property so user properties shadow builtin
                    // method names (push, trim, …). `reroute` sets `ip`.
                    match b.call(self, argc) {
                        Ok(()) => self.ip += 1,
                        Err(e) if e.kind == ErrorKind::MethodOnObject => {
                            self.reroute_method_to_object(b, argc)?;
                        }
                        Err(e) => return Err(e),
                    }
                }

                Instr::CallSpread => {
                    let callable = self.pop()?;
                    let arr_ptr = match self.pop()? {
                        Value::Array(p) => p,
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "spread call arguments must be an array",
                            ));
                        }
                    };
                    let ip = self.ip;
                    let elements: ThinVec<Value> = self
                        .arrays
                        .get(arr_ptr as usize)
                        .ok_or_else(|| {
                            VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                        })?
                        .clone();
                    let nargs = elements.len() as u32;
                    for val in elements {
                        self.stack.push(val);
                    }
                    self.dispatch_call(callable, nargs)?;
                }

                Instr::ClosureNew(addr, captures) => {
                    let addr = self.validate_func_addr(*addr)?;
                    // Collect into stack-allocated SmallVec instead of cloning
                    // the ThinVec from self.code. LocalIndex is u32 (Copy).
                    let mut upvals: SmallVec<[Value; 8]> = SmallVec::new();
                    for slot in captures.iter() {
                        if (*slot as u32) >= self.cur_local_count {
                            return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                        }
                        // Copy the slot verbatim: a Boxed slot carries its Upval
                        // handle (shared, by-reference), a Plain slot its value
                        // (a by-value snapshot).
                        upvals.push(self.stack[(self.fp + *slot as u32) as usize].clone());
                    }
                    let closure = self.alloc_closure(addr, ThinVec::from(upvals.as_slice()));
                    self.stack.push(closure);
                    self.ip += 1;
                }

                Instr::Return(nrets) => {
                    let frame = self
                        .callstack
                        .pop()
                        .ok_or_else(|| self.fail(ErrorKind::BadReturn, "bad return"))?;
                    // `fp` points at the frame base (arg 0 / local 0), which is
                    // where the caller pushed the args — so the return value(s)
                    // replace the whole frame, restoring the caller's stack.
                    let keep_below = self.fp as usize;
                    let n = *nrets;
                    if self.stack.len() < keep_below + n {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    if let Completion::ResolvePromise(pid) = frame.completion {
                        // A scheduler-resumed async frame has no caller below:
                        // resolve its promise with the return value (waking
                        // waiters) and fall through to the scheduler.
                        let value = if n == 0 {
                            Value::Undefined
                        } else {
                            self.stack[self.stack.len() - n].clone()
                        };
                        self.stack.truncate(keep_below);
                        self.fp = frame.prev_fp;
                        self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
                        self.settle_and_wake(pid, PromiseState::Resolved(value))?;
                        self.schedule()?;
                        continue;
                    }
                    let ret_start = self.stack.len() - n;
                    // Move (don't clone) each return value down to the frame
                    // base; the source region is truncated away immediately, so
                    // cloning would just bump-then-drop a refcount for string/
                    // heap returns. Ascending order is safe: dest indices
                    // (`keep_below..`) are <= source indices (`ret_start..`), so
                    // a source slot is never read after being overwritten.
                    for i in 0..n {
                        let v = std::mem::replace(&mut self.stack[ret_start + i], Value::Undefined);
                        self.stack[keep_below + i] = v;
                    }
                    self.stack.truncate(keep_below + n);
                    self.ip = frame.return_addr;
                    self.fp = frame.prev_fp;
                    if self.callstack.is_empty() {
                        // Root frame returned — capture the top-level value
                        // (peek, so the stack is still inspectable after Done).
                        // Tool calls started but never awaited are reported as
                        // `unstarted`; the host decides whether to run them.
                        let value = self.stack.last().cloned().unwrap_or(Value::Undefined);
                        return Ok(StepResult::Done {
                            value,
                            unstarted: std::mem::take(&mut self.outbox),
                        });
                    }
                    // Refresh the local-count cache from the restored caller
                    // frame (returns are far rarer than local accesses).
                    self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
                }

                Instr::Jump(addr) => {
                    self.ip = self.validate_jump_addr(*addr)?;
                }

                Instr::JFalse(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    let val = self.pop()?;
                    if !val.is_truthy() {
                        self.ip = addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::JTrue(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    let val = self.pop()?;
                    if val.is_truthy() {
                        self.ip = addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::JNotNullish(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    // Taken: leave the value for the branch that proceeds with
                    // it. Fall-through: pop it — the emitter short-circuits
                    // past the value (see the instruction doc).
                    let val = self.peek()?;
                    if !matches!(val, Value::Null | Value::Undefined) {
                        self.ip = addr;
                    } else {
                        self.stack.pop();
                        self.ip += 1;
                    }
                }

                Instr::Label(_) => {
                    // Eliminated in a pre-pass; no-op at runtime.
                    self.ip += 1;
                }

                // ── frame access ────────────────────────────────
                Instr::EnterFrame(nparams, build_args, local_kinds) => {
                    let nparams = *nparams;
                    let build_args = *build_args;
                    // Collect slot kinds into a stack-allocated SmallVec (zero
                    // alloc for the typical ≤2 locals) instead of cloning the
                    // ThinVec from self.code.
                    let local_kinds: SmallVec<[SlotKind; 32]> =
                        local_kinds.iter().copied().collect();
                    let frame = self
                        .callstack
                        .last()
                        .ok_or_else(|| self.fail(ErrorKind::BadArg, "bad argument"))?;
                    let argc = frame.arg_count;
                    // The args arrived as the leading locals at [fp, fp + argc).
                    // 1. Materialize the `arguments` array (from the actual args)
                    //    BEFORE normalizing, if the body uses it.
                    if build_args {
                        let base = self.fp as usize;
                        if base + argc as usize > self.stack.len() {
                            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                        }
                        let args: ThinVec<Value> = self.stack[base..base + argc as usize]
                            .iter()
                            .cloned()
                            .collect();
                        let arr = self.alloc_array(args);
                        if let Value::Array(p) = arr {
                            self.callstack.last_mut().unwrap().arguments_cache = Some(p);
                        }
                    }
                    // 2. Normalize the arg region to exactly `nparams` slots:
                    //    drop surplus args, or pad missing params with Undefined.
                    let want = self.fp as usize + nparams as usize;
                    if self.stack.len() > want {
                        self.stack.truncate(want);
                    } else {
                        self.stack.resize(want, Value::Undefined);
                    }
                    // 3. Install the closure's captured environment as the upval
                    //    locals, now landing at [fp + nparams, fp + nparams + K).
                    let upvals =
                        std::mem::take(&mut self.callstack.last_mut().unwrap().pending_upvals);
                    let k = upvals.len() as u32;
                    for uv in upvals {
                        self.stack.push(uv);
                    }
                    // 4. Allocate the declared (non-param) own locals + self-ref
                    // slot (Boxed → fresh cell + Upval).
                    for kind in &local_kinds {
                        let slot = match kind {
                            SlotKind::Plain => Value::Undefined,
                            SlotKind::Boxed => {
                                let idx = self.cells.len() as CellIndex;
                                self.cells.push(Value::Undefined);
                                Value::Upval(idx)
                            }
                        };
                        self.stack.push(slot);
                    }
                    let total = nparams as u32 + k + local_kinds.len() as u32;
                    self.callstack.last_mut().unwrap().local_count = total;
                    self.cur_local_count = total;
                    self.ip += 1;
                }

                Instr::Arguments => {
                    let frame = self
                        .callstack
                        .last()
                        .ok_or_else(|| self.fail(ErrorKind::BadArg, "bad argument"))?;
                    // Reuse the cached array when this frame already built one
                    // (functions that use `arguments` build it eagerly in the
                    // prologue's EnterFrame; the lazy path here serves the root
                    // frame, which has no args).
                    if let Some(ptr) = frame.arguments_cache {
                        self.stack.push(Value::Array(ptr));
                        self.ip += 1;
                        continue;
                    }
                    // Build it from the frame's args (arg 0 at fp). Copy them out
                    // before touching the heap.
                    let argc = frame.arg_count;
                    let base = self.fp as usize;
                    if base + argc as usize > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let args: ThinVec<Value> = self.stack[base..base + argc as usize]
                        .iter()
                        .cloned()
                        .collect();
                    let arr = self.alloc_array(args);
                    let ptr = match arr {
                        Value::Array(p) => p,
                        _ => unreachable!("alloc_array returns an Array"),
                    };
                    self.callstack.last_mut().unwrap().arguments_cache = Some(ptr);
                    self.stack.push(arr);
                    self.ip += 1;
                }

                Instr::GetLocal(local) => {
                    if (*local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    // A Boxed slot holds an Upval marker; dereference it so the
                    // value — never the marker — reaches the expression stack.
                    let val = match &self.stack[(self.fp + *local as u32) as usize] {
                        Value::Upval(c) => self
                            .cells
                            .get(*c as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                            .clone(),
                        other => other.clone(),
                    };
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::LoadThis => {
                    let this_val = self.callstack.last().unwrap().this_val.clone();
                    self.stack.push(this_val);
                    self.ip += 1;
                }

                Instr::SetLocal(local) => {
                    let local = *local;
                    if (local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    let val = self.pop()?;
                    let slot = (self.fp + local as u32) as usize;
                    // Write through a Boxed slot to its shared cell; a Plain slot
                    // is overwritten in place.
                    match self.stack[slot] {
                        Value::Upval(c) => {
                            let ip = self.ip;
                            *self.cells.get_mut(c as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::ValueError, "bad cell pointer")
                            })? = val;
                        }
                        _ => self.stack[slot] = val,
                    }
                    self.ip += 1;
                }

                Instr::TeeLocal(local) => {
                    if (*local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    let val = self.peek()?.clone();
                    let slot = (self.fp + *local as u32) as usize;
                    // Like SetLocal but peeks: the value stays on the stack
                    // (assignment is an expression) while still writing to the
                    // local. Replaces Pick(0); SetLocal (formerly Dup; SetLocal).
                    match self.stack[slot] {
                        Value::Upval(c) => {
                            let ip = self.ip;
                            *self.cells.get_mut(c as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::ValueError, "bad cell pointer")
                            })? = val;
                        }
                        _ => self.stack[slot] = val,
                    }
                    self.ip += 1;
                }

                Instr::FreshCell(local) => {
                    if (*local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    let slot = (self.fp + *local as u32) as usize;
                    // Read the current value, dereferencing an existing Upval.
                    let val = match &self.stack[slot] {
                        Value::Upval(c) => self
                            .cells
                            .get(*c as usize)
                            .ok_or_else(|| {
                                self.fail_not_resumable(ErrorKind::ValueError, "bad cell pointer")
                            })?
                            .clone(),
                        other => other.clone(),
                    };
                    // Allocate a fresh cell seeded with that value and point the
                    // slot at it, so subsequent captures see a per-iteration cell.
                    let idx = self.cells.len() as CellIndex;
                    self.cells.push(val);
                    self.stack[slot] = Value::Upval(idx);
                    self.ip += 1;
                }

                Instr::IncLocal(local, p, mode) => {
                    if *local as u32 >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    // Read current value (dereferencing boxed slots).
                    let old = match &self.stack[(self.fp + *local as u32) as usize] {
                        Value::Upval(c) => self
                            .cells
                            .get(*c as usize)
                            .ok_or_else(|| {
                                self.fail_not_resumable(ErrorKind::ValueError, "bad cell pointer")
                            })?
                            .clone(),
                        other => other.clone(),
                    };
                    // NotResumable: reads local by peek (no stack pop), so operand not consumed.
                    let old_num = old.to_number().ok_or_else(|| {
                        self.fail_not_resumable(ErrorKind::TypeError, "type error")
                    })?;
                    // Compute new value: subtract p (p = -1 for ++, p = 1 for --).
                    let new_num = old_num - *p;
                    let new_val = Value::Float(new_num);
                    // Store the new value.
                    let slot = (self.fp + *local as u32) as usize;
                    match self.stack[slot] {
                        Value::Upval(c) => {
                            let ip = self.ip;
                            *self.cells.get_mut(c as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::ValueError, "bad cell pointer")
                            })? = new_val;
                        }
                        _ => self.stack[slot] = new_val,
                    }
                    // Push the appropriate result: old for postfix, new for prefix.
                    let result = match mode {
                        UpdateMode::Prefix => Value::Float(new_num),
                        UpdateMode::Postfix => Value::Float(old_num),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                // ── type queries ────────────────────────────────
                Instr::TypeOf => {
                    let val = self.pop()?;
                    let tag = match val {
                        Value::Undefined => "undefined",
                        Value::Null => "object",
                        Value::Bool(_) => "boolean",
                        Value::Float(_) | Value::PosInt(_) | Value::NegInt(_) => "number",
                        Value::String(_) => "string",
                        Value::Fn(_) | Value::Builtin(_) => "function",
                        Value::Array(_)
                        | Value::Object(_)
                        | Value::Promise(_)
                        | Value::RegExp(_)
                        | Value::Map(_)
                        | Value::Set(_) => "object",
                        Value::Closure(_) => "function",
                        Value::Upval(_) => {
                            return Err(self.fail(ErrorKind::ValueError, "value error"));
                        }
                    };
                    self.push_str_value(tag);
                    self.ip += 1;
                }

                // ── type predicates ─────────────────────────────
                Instr::IsNull => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Null)));
                    self.ip += 1;
                }
                Instr::IsBool => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Bool(_))));
                    self.ip += 1;
                }
                Instr::IsFloat => {
                    // True only for a Number with a fractional part (an Int is
                    // never a float). Use IsNum to test "is any number".
                    let val = self.pop()?;
                    let is_float = matches!(val, Value::Float(n) if !float_is_int(n));
                    self.stack.push(Value::Bool(is_float));
                    self.ip += 1;
                }
                Instr::IsNum => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(
                        val,
                        Value::Float(_) | Value::PosInt(_) | Value::NegInt(_)
                    )));
                    self.ip += 1;
                }
                Instr::IsStr => {
                    let val = self.pop()?;
                    let is_str = matches!(val, Value::String(_));
                    self.stack.push(Value::Bool(is_str));
                    self.ip += 1;
                }
                Instr::IsObj => {
                    let val = self.pop()?;
                    let is_obj = matches!(val, Value::Object(_) | Value::RegExp(_));
                    self.stack.push(Value::Bool(is_obj));
                    self.ip += 1;
                }
                Instr::IsMap => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Map(_))));
                    self.ip += 1;
                }
                Instr::IsSet => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Set(_))));
                    self.ip += 1;
                }

                // ── unary operators ─────────────────────────────
                Instr::Neg => unary_num!(|n: f64| -n),

                Instr::Not => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(!val.is_truthy()));
                    self.ip += 1;
                }

                Instr::BitNot => {
                    let val = self.pop()?;
                    match val.as_i64() {
                        Some(i) => {
                            self.stack.push(Value::Float(!i as f64));
                            self.ip += 1;
                        }
                        None => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    }
                }

                // ── binary operators ────────────────────────────
                Instr::Add => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    // JS `+`: if either operand is a string, concatenate (ToString
                    // both); otherwise add numerically (ToNumber both). An
                    // array/object/function in the numeric path is a TypeError
                    // (we do not ToPrimitive it — see the note on `loose_equal`).
                    let result = if lhs.is_string() || rhs.is_string() {
                        // Pre-size the buffer when both operands are strings (the
                        // common concat path), saving incremental growth.
                        let cap = match (lhs.str_byte_len(), rhs.str_byte_len()) {
                            (Some(a), Some(b)) => a + b,
                            _ => 0,
                        };
                        let mut s = String::with_capacity(cap);
                        self.write_js_string(&lhs, 0, &mut s);
                        self.write_js_string(&rhs, 0, &mut s);
                        Value::String(RcStr::from(s))
                    } else {
                        match (lhs.to_number(), rhs.to_number()) {
                            (Some(a), Some(b)) => Value::Float(a + b),
                            _ => {
                                let msg = format!(
                                    "cannot add {} ({}) and {} ({})",
                                    lhs.type_name(),
                                    self.preview(&lhs),
                                    rhs.type_name(),
                                    self.preview(&rhs)
                                );
                                return Err(self.fail(ErrorKind::TypeError, msg));
                            }
                        }
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::Sub => binary_num!(|a: f64, b: f64| a - b),
                Instr::Mul => binary_num!(|a: f64, b: f64| a * b),
                // JS `/`: never throws — a zero divisor yields ±Infinity (or NaN
                // for 0/0), which f64 division produces directly.
                Instr::Div => binary_num!(|a: f64, b: f64| a / b),
                // JS `%`: float remainder with the dividend's sign; `x % 0` is
                // NaN. Rust's f64 `%` matches this exactly.
                Instr::Mod => binary_num!(|a: f64, b: f64| a % b),
                Instr::Pow => binary_num!(|a: f64, b: f64| a.powf(b)),

                Instr::Eq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(lhs.strict_equal(&rhs)));
                    self.ip += 1;
                }
                Instr::Neq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(!lhs.strict_equal(&rhs)));
                    self.ip += 1;
                }
                Instr::LooseEq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(lhs.loose_equal(&rhs)));
                    self.ip += 1;
                }
                Instr::LooseNeq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(!lhs.loose_equal(&rhs)));
                    self.ip += 1;
                }

                Instr::Lt => cmp_op!(== Less),
                Instr::Gt => cmp_op!(== Greater),
                Instr::LtEq => cmp_op!(!= Greater),
                Instr::GtEq => cmp_op!(!= Less),

                Instr::And => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(if lhs.is_truthy() { rhs } else { lhs });
                    self.ip += 1;
                }
                Instr::Or => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(if lhs.is_truthy() { lhs } else { rhs });
                    self.ip += 1;
                }

                Instr::BitAnd => binary_int!(|a: i64, b: i64| a & b),
                Instr::BitOr => binary_int!(|a: i64, b: i64| a | b),
                Instr::BitXor => binary_int!(|a: i64, b: i64| a ^ b),
                // Shift count must be in [0, 63]; anything else would overflow
                // (a panic in debug builds) so reject it as a ValueError.
                Instr::BitLhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    self.stack.push(Value::Float((a << b) as f64));
                    self.ip += 1;
                }
                Instr::BitRhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    self.stack.push(Value::Float((a >> b) as f64));
                    self.ip += 1;
                }
                Instr::BitURhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    self.stack
                        .push(Value::Float(((a as u64) >> (b as u32)) as f64));
                    self.ip += 1;
                }

                // ── object operations ───────────────────────────
                Instr::RegExpNew => {
                    // Stack: [..., pattern_str, flags_str] (flags on top).
                    let flags_val = self.pop()?;
                    let pattern_val = self.pop()?;
                    let pattern = self.str_from(&pattern_val)?;
                    let flags_str = self.str_from(&flags_val)?;
                    // Validate flags: only g, i, m, s, u, y, d, v are valid.
                    for c in flags_str.chars() {
                        if !matches!(c, 'g' | 'i' | 'm' | 's' | 'u' | 'y' | 'd' | 'v') {
                            return Err(self.fail(
                                ErrorKind::ValueError,
                                format!("invalid regular expression flags: {flags_str}"),
                            ));
                        }
                    }
                    let compiled = match regress::Regex::with_flags(pattern, flags_str) {
                        Ok(re) => re,
                        Err(e) => {
                            return Err(self.fail(
                                ErrorKind::ValueError,
                                format!("invalid regular expression: {e}"),
                            ));
                        }
                    };
                    let rx_data = RegExpData {
                        pattern: self.string_from(&pattern_val)?,
                        flags: self.string_from(&flags_val)?,
                        compiled,
                        last_index: std::cell::Cell::new(0),
                    };
                    self.stack.push(Value::RegExp(RcRegExp::new(rx_data)));
                    self.ip += 1;
                }

                Instr::SetNew => {
                    let arg = self.pop()?;
                    let mut set: IndexSet<MapKey> = IndexSet::new();
                    if let Value::Array(p) = arg {
                        let arr = self
                            .arrays
                            .get(p as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
                        for v in arr.iter() {
                            set.insert(MapKey(v.clone()));
                        }
                    } else if !matches!(arg, Value::Undefined) {
                        return Err(self.fail(ErrorKind::TypeError, "type error"));
                    }
                    let addr = self.sets.len() as SetPtr;
                    self.sets.push(set);
                    self.stack.push(Value::Set(addr));
                    self.ip += 1;
                }

                Instr::MapNew => {
                    let arg = self.pop()?;
                    let mut map: IndexMap<MapKey, Value> = IndexMap::new();
                    if let Value::Array(p) = arg {
                        let entries = self
                            .arrays
                            .get(p as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
                        for entry in entries.iter() {
                            let pair_ptr = match entry {
                                Value::Array(p) => *p,
                                _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                            };
                            let pair = self
                                .arrays
                                .get(pair_ptr as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?;
                            if pair.len() < 2 {
                                continue;
                            }
                            map.insert(MapKey(pair[0].clone()), pair[1].clone());
                        }
                    } else if !matches!(arg, Value::Undefined) {
                        return Err(self.fail(ErrorKind::TypeError, "type error"));
                    }
                    let addr = self.maps.len() as MapPtr;
                    self.maps.push(map);
                    self.stack.push(Value::Map(addr));
                    self.ip += 1;
                }

                Instr::ObjNew(fields) => {
                    let n = fields.len();
                    if n > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let split = self.stack.len() - n;
                    let vals: SmallVec<[Value; 16]> = self.stack.drain(split..).collect();
                    let mut obj = IndexMap::new();
                    // Left-to-right: field 0's value is the deepest (first
                    // pushed), so values line up with fields in order. `field`
                    // clones are refcount bumps on the interned `RcStr` key.
                    for (field, val) in fields.iter().zip(vals) {
                        obj.insert(field.clone(), val);
                    }
                    let obj_ptr = self.alloc_object(obj);
                    self.stack.push(obj_ptr);
                    self.ip += 1;
                }

                Instr::ObjGet(field) => {
                    let field_str = field.as_str(); // borrows self.code
                    // Check for RegExp first: properties are computed from
                    // the RegExpData without a backing object in self.objects.
                    match self.stack.last() {
                        Some(Value::RegExp(r)) => {
                            let val = regexp_prop(r, field_str);
                            self.stack.pop();
                            self.stack.push(val);
                            self.ip += 1;
                        }
                        Some(Value::Object(p)) => {
                            let obj_ptr = *p;
                            let val = match self.objects.get(obj_ptr as usize) {
                                Some(obj) => {
                                    obj.get(field_str).cloned().unwrap_or(Value::Undefined)
                                }
                                _ => Value::Undefined,
                            };
                            self.stack.pop();
                            self.stack.push(val);
                            self.ip += 1;
                        }
                        _ => {
                            let recv = self.pop()?;
                            let msg = format!(
                                "cannot read property on {}{}",
                                recv.type_name(),
                                await_hint(&recv)
                            );
                            return Err(self.fail(ErrorKind::TypeError, msg));
                        }
                    }
                }

                Instr::ObjSet(field, mode) => {
                    let field = field.clone();
                    let mode = *mode;
                    let val = self.pop()?;
                    // Peek the receiver to check type.
                    match self.stack.last() {
                        Some(Value::RegExp(r)) => {
                            // `lastIndex` is writable (the `/g` cursor); every
                            // other RegExp property is read-only and the write
                            // is accepted silently.
                            if field.as_str() == "lastIndex" {
                                let n = val.to_number().unwrap_or(0.0);
                                let n = if n.is_finite() && n >= 0.0 {
                                    n as usize
                                } else {
                                    0
                                };
                                r.last_index.set(n);
                            }
                            let result = match mode {
                                SetMode::Old => val,
                                SetMode::New => val,
                            };
                            self.stack.pop();
                            self.stack.push(result);
                            self.ip += 1;
                        }
                        Some(Value::Object(p)) => {
                            let obj_ptr = *p;
                            let obj = match self.objects.get_mut(obj_ptr as usize) {
                                Some(o) => o,
                                _ => {
                                    return Err(self.fail_not_resumable(
                                        ErrorKind::TypeError,
                                        "bad object pointer",
                                    ));
                                }
                            };
                            let result = match mode {
                                SetMode::Old => {
                                    let old = obj.get(&field).cloned().unwrap_or(Value::Undefined);
                                    if let Some(slot) = obj.get_mut(&field) {
                                        *slot = val;
                                    } else {
                                        obj.insert(field, val);
                                    }
                                    old
                                }
                                SetMode::New => {
                                    let result = val.clone();
                                    if let Some(slot) = obj.get_mut(&field) {
                                        *slot = val;
                                    } else {
                                        obj.insert(field, val);
                                    }
                                    result
                                }
                            };
                            self.stack.pop();
                            self.stack.push(result);
                            self.ip += 1;
                        }
                        _ => {
                            let recv = self.pop()?;
                            let msg = format!(
                                "cannot set property on {}{}",
                                recv.type_name(),
                                await_hint(&recv)
                            );
                            return Err(self.fail(ErrorKind::TypeError, msg));
                        }
                    }
                }

                // Runtime-polymorphic computed read. Dispatch on the container
                // type: array (int index), object (ToString key), or string
                // (byte-offset char). A char result needs a fresh allocation, so
                // it is computed under the heap borrow and allocated after.
                Instr::IndexGet => {
                    let key = self.pop()?;
                    let container = self.pop()?;
                    let val =
                        match &container {
                            // String char-indexing: strings are inline values now, so
                            // this no longer routes through the heap. The single-char
                            // result is a fresh `RcStr`.
                            Value::String(s) => {
                                let s = s.as_str();
                                let idx = key
                                    .as_i64()
                                    .ok_or_else(|| self.fail(ErrorKind::TypeError, "type error"))?;
                                if idx < 0 {
                                    return Err(self.fail(ErrorKind::ValueError, "value error"));
                                }
                                let idx = idx as usize;
                                if idx >= s.len() {
                                    // JS: an out-of-range char index is `undefined`.
                                    Value::Undefined
                                } else if !s.is_char_boundary(idx) {
                                    return Err(self.fail(ErrorKind::ValueError, "value error"));
                                } else {
                                    let ch = s[idx..].chars().next().unwrap();
                                    Value::String(RcStr::from(ch.to_string()))
                                }
                            }
                            Value::Array(p) => {
                                let arr = self.arrays.get(*p as usize).ok_or_else(|| {
                                    self.fail(ErrorKind::ValueError, "value error")
                                })?;
                                let idx = key
                                    .as_i64()
                                    .ok_or_else(|| self.fail(ErrorKind::TypeError, "type error"))?;
                                if idx < 0 {
                                    return Err(self.fail(ErrorKind::ValueError, "value error"));
                                }
                                // JS: an out-of-bounds index reads as `undefined`.
                                arr.get(idx as usize).cloned().unwrap_or(Value::Undefined)
                            }
                            Value::Object(p) => {
                                let obj = self.objects.get(*p as usize).ok_or_else(|| {
                                    self.fail(ErrorKind::ValueError, "value error")
                                })?;
                                // JS coerces a computed key with ToString.
                                let field = self.to_js_string(&key, 0);
                                // JS: a missing property reads as `undefined`.
                                obj.get(field.as_str()).cloned().unwrap_or(Value::Undefined)
                            }
                            _ => {
                                let msg = format!(
                                    "cannot index into {} with {}{}",
                                    container.type_name(),
                                    self.preview(&key),
                                    await_hint(&container)
                                );
                                return Err(self.fail(ErrorKind::TypeError, msg));
                            }
                        };
                    self.stack.push(val);
                    self.ip += 1;
                }

                // Runtime-polymorphic computed write. Arrays index by int (OOB or
                // negative is an error — no hole-growing); objects key by the
                // ToString'd key; strings are immutable (TypeError).
                Instr::IndexSet(mode) => {
                    let mode = *mode;
                    let val = self.pop()?;
                    let key = self.pop()?;
                    let container = self.pop()?;
                    let is_array = match &container {
                        Value::Array(_) => true,
                        Value::Object(_) => false,
                        // Strings are immutable; closures aren't indexable.
                        _ => {
                            let msg = format!(
                                "cannot index-set on {}{}",
                                container.type_name(),
                                await_hint(&container)
                            );
                            return Err(self.fail(ErrorKind::TypeError, msg));
                        }
                    };
                    let old = if matches!(mode, SetMode::Old) {
                        // Read the previous value before the write (for postfix
                        // `++`/`--` on computed targets).
                        if is_array {
                            let idx = key
                                .as_i64()
                                .ok_or_else(|| self.fail(ErrorKind::TypeError, "type error"))?;
                            if idx < 0 {
                                return Err(self.fail(ErrorKind::ValueError, "value error"));
                            }
                            match &container {
                                Value::Array(p) => self
                                    .arrays
                                    .get(*p as usize)
                                    .and_then(|a| a.get(idx as usize).cloned())
                                    .unwrap_or(Value::Undefined),
                                _ => unreachable!(),
                            }
                        } else {
                            let field = self.to_js_string(&key, 0);
                            match &container {
                                Value::Object(p) => self
                                    .objects
                                    .get(*p as usize)
                                    .and_then(|o| o.get(field.as_str()).cloned())
                                    .unwrap_or(Value::Undefined),
                                _ => unreachable!(),
                            }
                        }
                    } else {
                        Value::Undefined // placeholder, unused
                    };
                    // The value left on the stack: the assigned value (`New`) or
                    // the previous one (`Old`). Computed before the store, which
                    // moves `val`; the clone is a refcount bump for strings.
                    let result = match mode {
                        SetMode::New => val.clone(),
                        SetMode::Old => old,
                    };
                    if is_array {
                        let idx = key
                            .as_i64()
                            .ok_or_else(|| self.fail(ErrorKind::TypeError, "type error"))?;
                        if idx < 0 {
                            return Err(self.fail(ErrorKind::ValueError, "value error"));
                        }
                        let idx = idx as usize;
                        let p = match &container {
                            Value::Array(p) => *p,
                            _ => unreachable!(),
                        };
                        let ip = self.ip;
                        let arr = self.arrays.get_mut(p as usize).ok_or_else(|| {
                            VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                        })?;
                        if idx >= arr.len() {
                            let len = arr.len();
                            return Err(self.fail(
                                ErrorKind::ValueError,
                                format!(
                                    "cannot write array index {idx}: out of bounds (length {len})"
                                ),
                            ));
                        }
                        arr[idx] = val;
                    } else {
                        let field = self.to_js_string(&key, 0);
                        let p = match &container {
                            Value::Object(p) => *p,
                            _ => unreachable!(),
                        };
                        let ip = self.ip;
                        let obj = self.objects.get_mut(p as usize).ok_or_else(|| {
                            VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                        })?;
                        obj.insert(field, val);
                    }
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::ObjHas => {
                    let field = self.pop_string()?;
                    let obj_ptr =
                        match self.stack.pop().ok_or_else(|| {
                            self.fail(ErrorKind::StackUnderflow, "stack underflow")
                        })? {
                            Value::Object(p) => p,
                            _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                        };
                    let has = self
                        .objects
                        .get(obj_ptr as usize)
                        .ok_or_else(|| {
                            self.fail_not_resumable(ErrorKind::TypeError, "bad object pointer")
                        })?
                        .contains_key(field.as_str());
                    self.stack.push(Value::Bool(has));
                    self.ip += 1;
                }

                Instr::ObjDelete => {
                    let field = self.pop_string()?;
                    let obj_ptr =
                        match self.stack.pop().ok_or_else(|| {
                            self.fail(ErrorKind::StackUnderflow, "stack underflow")
                        })? {
                            Value::Object(p) => p,
                            _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                        };
                    // shift_remove keeps the remaining keys in insertion order.
                    let ip = self.ip;
                    let existed = self
                        .objects
                        .get_mut(obj_ptr as usize)
                        .ok_or_else(|| {
                            VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                        })?
                        .shift_remove(field.as_str())
                        .is_some();
                    self.stack.push(Value::Bool(existed));
                    self.ip += 1;
                }

                Instr::ObjExtend => {
                    let src = self.pop()?;
                    let obj_ptr = match self.pop()? {
                        Value::Object(p) => p,
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "object spread target must be an object",
                            ));
                        }
                    };
                    // null/undefined src is a no-op (JS semantics).
                    // Non-object, non-null/undefined src is TypeError
                    // (divergence: JS would copy index keys from arrays/strings).
                    let ip = self.ip;
                    match src {
                        Value::Null | Value::Undefined => {}
                        Value::Object(src_ptr) => {
                            let entries: SmallVec<[(FieldName, Value); 8]> = self
                                .objects
                                .get(src_ptr as usize)
                                .ok_or_else(|| {
                                    VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                                })?
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            let obj = self.objects.get_mut(obj_ptr as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                            })?;
                            obj.extend(entries);
                        }
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "object spread source must be an object (or null/undefined)",
                            ));
                        }
                    }
                    self.stack.push(Value::Object(obj_ptr));
                    self.ip += 1;
                }

                // ── array operations ────────────────────────────
                Instr::ArrNew(n) => {
                    let n = *n as usize;
                    if n > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let split = self.stack.len() - n;
                    // Left-to-right: first pushed becomes element 0.
                    let vals: ThinVec<Value> = self.stack.drain(split..).collect();
                    let arr_ptr = self.alloc_array(vals);
                    self.stack.push(arr_ptr);
                    self.ip += 1;
                }

                Instr::ArrExtend => {
                    let src = self.pop()?;
                    let arr_ptr = match self.pop()? {
                        Value::Array(p) => p,
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "array spread target must be an array",
                            ));
                        }
                    };
                    let ip = self.ip;
                    match src {
                        Value::Array(src_ptr) => {
                            let src_elts: ThinVec<Value> = self
                                .arrays
                                .get(src_ptr as usize)
                                .ok_or_else(|| {
                                    VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                                })?
                                .clone();
                            let arr = self.arrays.get_mut(arr_ptr as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                            })?;
                            arr.extend(src_elts);
                        }
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "array spread source must be an array",
                            ));
                        }
                    }
                    self.stack.push(Value::Array(arr_ptr));
                    self.ip += 1;
                }

                Instr::ArrPush => {
                    let val = self.pop()?;
                    let arr_ptr = match self.pop()? {
                        Value::Array(p) => p,
                        _ => {
                            return Err(self
                                .fail(ErrorKind::TypeError, "array push target must be an array"));
                        }
                    };
                    let ip = self.ip;
                    let arr = self.arrays.get_mut(arr_ptr as usize).ok_or_else(|| {
                        VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                    })?;
                    arr.push(val);
                    self.stack.push(Value::Array(arr_ptr));
                    self.ip += 1;
                }

                Instr::GetLength => {
                    let val = self.pop()?;
                    // `.length`: intrinsic byte/element count for strings and
                    // arrays; on an *object* a plain property read (JS — e.g. a
                    // RegExp match result stores its own `length`), `undefined`
                    // when absent. Anything else (incl. map/set) is a
                    // `TypeError` — for-of lowering relies on that (it iterates
                    // only array/string, via `idx < ArrLength`).
                    let result = match val {
                        Value::String(s) => Value::Float(s.len() as f64),
                        Value::Array(p) => Value::Float(
                            self.arrays
                                .get(p as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                                .len() as f64,
                        ),
                        Value::Object(p) => self
                            .objects
                            .get(p as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                            .get("length")
                            .cloned()
                            .unwrap_or(Value::Undefined),
                        _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::GetSize => {
                    let val = self.pop()?;
                    // `.size`: intrinsic entry count for maps and sets; on an
                    // *object* the `size` property (`undefined` when absent).
                    let result = match val {
                        Value::Map(p) => Value::Float(
                            self.maps
                                .get(p as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                                .len() as f64,
                        ),
                        Value::Set(p) => Value::Float(
                            self.sets
                                .get(p as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                                .len() as f64,
                        ),
                        Value::Object(p) => self
                            .objects
                            .get(p as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                            .get("size")
                            .cloned()
                            .unwrap_or(Value::Undefined),
                        _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::ToStr => {
                    let val = self.pop()?;
                    let s = self.to_js_string(&val, 0);
                    self.stack.push(Value::String(s));
                    self.ip += 1;
                }

                Instr::ToNum => {
                    // ToNumber, matching the arithmetic operators' coercion: an
                    // array/object/function has no numeric form (TypeError).
                    let val = self.pop()?;
                    match val.to_number() {
                        Some(num) => self.stack.push(Value::Float(num)),
                        None => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    }
                    self.ip += 1;
                }

                Instr::ToBool => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(val.is_truthy()));
                    self.ip += 1;
                }

                // ── external effects ───────────────────────────
                Instr::Invoke(name, nargs) => {
                    // Start (don't perform) the tool call: allocate a Pending
                    // promise, record the call in the outbox, push the promise,
                    // and continue executing. The host sees the accumulated
                    // outbox only when an `Await` blocks on a pending promise,
                    // so fan-out composes across arbitrary control flow.
                    let name = name.as_str().to_owned();
                    let n = *nargs as usize;
                    if n > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let args = self.stack.split_off(self.stack.len() - n);
                    let id = self.alloc_promise();
                    self.outbox.push(InvokeCall {
                        promise: id,
                        name,
                        args,
                    });
                    self.stack.push(Value::Promise(id));
                    self.ip += 1;
                }

                Instr::Await => {
                    // Re-executing instruction: the operand is PEEKED while
                    // pending so the same Await can run again after the host
                    // settles the promise (`StepResult::Pending` leaves ip
                    // unchanged). A non-promise passes through unchanged.
                    let top = self.peek()?;
                    let id = match top {
                        Value::Promise(id) => *id,
                        _ => {
                            // `await x` on a plain value: the value IS the
                            // result; leave it in place.
                            self.ip += 1;
                            continue;
                        }
                    };
                    let state = self.promises.get(id as usize).ok_or_else(|| {
                        self.fail_not_resumable(ErrorKind::ValueError, "bad promise pointer")
                    })?;
                    match state {
                        PromiseState::Resolved(v) => {
                            // A promise resolved with a promise adopts it (JS:
                            // an async function returning a promise chains —
                            // a promise never resolves to a promise). Follow
                            // the chain and re-await the innermost promise in
                            // place; a cycle is the JS "chaining cycle" error.
                            if let Value::Promise(inner) = v {
                                let mut seen = vec![id];
                                let mut cur = *inner;
                                loop {
                                    if seen.contains(&cur) {
                                        self.stack.pop();
                                        return Err(self.fail(
                                            ErrorKind::TypeError,
                                            "chaining cycle detected: promise resolves to itself",
                                        ));
                                    }
                                    seen.push(cur);
                                    match self.promises.get(cur as usize) {
                                        Some(PromiseState::Resolved(Value::Promise(j))) => {
                                            cur = *j;
                                        }
                                        _ => break,
                                    }
                                }
                                *self.stack.last_mut().unwrap() = Value::Promise(cur);
                                continue; // re-execute the Await on the adoptee
                            }
                            let v = v.clone();
                            self.stack.pop();
                            self.stack.push(v);
                            self.ip += 1;
                        }
                        PromiseState::Rejected(errval) => {
                            // Inside a `try`, the rejection value itself is
                            // what `catch` receives (JS semantics: the reason
                            // passes through raw, not wrapped in an error
                            // object).
                            if self.reachable_handler() {
                                let errval = errval.clone();
                                self.stack.pop(); // the promise operand
                                self.unwind_to_handler(errval);
                                continue;
                            }
                            // Unhandled inside a resumed strand: reject the
                            // strand's own promise (propagation through an
                            // awaiting chain, as in JS) — never the host.
                            if self.in_strand() {
                                let errval = errval.clone();
                                self.stack.pop();
                                self.reject_strand(errval)?;
                                continue;
                            }
                            // Escalate via the Phase 3 path: pop the operand
                            // first (pop-first invariant), then fail resumably —
                            // the host may substitute a value for the rejection
                            // (`PushValueThenContinue`).
                            let msg = format!(
                                "awaited promise rejected with {} ({})",
                                errval.type_name(),
                                self.preview(errval)
                            );
                            self.stack.pop();
                            return Err(self.fail(ErrorKind::ValueError, msg));
                        }
                        PromiseState::Pending { .. } => {
                            // Below top level this Await is inside an async
                            // function's own frame (the parser confines
                            // `await` there): suspend exactly that frame —
                            // Tier 2's single-frame snapshot.
                            if self.callstack.len() > 1 {
                                self.suspend_current_frame(id)?;
                                continue;
                            }
                            // Top level: the root strand parks in place. Run
                            // ready continuations above the parked region
                            // first; the root's Await re-executes when the
                            // queue drains.
                            if !self.ready.is_empty() {
                                self.root_ip = self.ip;
                                self.schedule()?;
                                continue;
                            }
                            // Nothing ready: yield to the host — everything
                            // started since the last yield, ip unchanged
                            // (this Await re-executes on the next step),
                            // the promise stays on the stack. With nothing
                            // in flight either, no settlement can ever
                            // arrive: deadlock.
                            if !self.outbox.is_empty() || self.inflight > 0 {
                                let calls = std::mem::take(&mut self.outbox);
                                self.inflight += calls.len();
                                return Ok(StepResult::Pending { calls });
                            }
                            return Err(self.deadlock_error(id));
                        }
                    }
                }

                Instr::Raise(condition, argc) => {
                    let argc = *argc;
                    // Instruction contract: argc is 0 or 1 (the compiler
                    // enforces this at the source level; hand-assembled code
                    // violating it would leave stray values on the stack).
                    if argc > 1 {
                        return Err(self.fail(
                            ErrorKind::BadArg,
                            format!("Raise supports at most one payload, got {argc}"),
                        ));
                    }
                    let payload = if argc > 0 {
                        Some(self.stack.pop().ok_or_else(|| {
                            self.fail(ErrorKind::StackUnderflow, "stack underflow")
                        })?)
                    } else {
                        None
                    };
                    // Advance ip past the Raise before returning: the
                    // conceptual stack effect is (payload?) -> result,
                    // so the host resumes by pushing a result value and
                    // calling step() again (or using VM::resume_raise).
                    self.ip += 1;
                    return Ok(StepResult::Raise {
                        condition: condition.as_str().to_owned(),
                        payload,
                    });
                }

                // ── exceptions (6_LANGUAGE Part B) ──────────────
                Instr::TryEnter(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    self.handlers.push(HandlerEntry {
                        catch_ip: addr,
                        stack_len: self.stack.len(),
                        callstack_len: self.callstack.len(),
                        fp: self.fp,
                    });
                    self.ip += 1;
                }

                Instr::TryExit => {
                    // An unmatched TryExit is a compiler bug (NotResumable).
                    if self.handlers.pop().is_none() {
                        return Err(
                            self.fail(ErrorKind::BadArg, "TryExit without an active handler")
                        );
                    }
                    self.ip += 1;
                }

                Instr::Throw => {
                    let value = self.pop()?;
                    if self.reachable_handler() {
                        self.unwind_to_handler(value);
                        continue;
                    }
                    // A throw escaping a resumed strand rejects its promise
                    // (7_ASYNC Tier 2) — it must not unwind into the parked
                    // root strand's handlers, which belong to code that
                    // isn't executing.
                    if self.in_strand() {
                        self.reject_strand(value)?;
                        continue;
                    }
                    // NotResumable (via `fail`'s kind classification):
                    // the operand was consumed, but a `throw` has no
                    // result slot — pushing a replacement value would
                    // corrupt the statement-level stack. The thrown
                    // value rides along in `payload` so the host gets
                    // the program's own error structurally, not just a
                    // rendering.
                    let msg = self.uncaught_message(&value);
                    let mut err = self.fail(ErrorKind::UncaughtException, msg);
                    err.payload = Some(value);
                    return Err(err);
                }
            }
        }
    }
}

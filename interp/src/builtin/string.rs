use thin_vec::ThinVec;

use crate::builtin::Args;
use crate::builtin::regexp::{build_exec_result, find_all, find_at, try_reg_exp};
use crate::units;
use crate::vm::{ErrorKind, JsString, VM, VMError, Value};

// ── String static implementations ────────────────────────────────────────────

/// `String(x)` / `new String(x)` — the constructor as a plain call.
/// `String()` → `""`; with one arg, ToString. The `new` path would box
/// (`new String("hi")` → a String wrapper object); here it is a documented
/// divergence — we have no boxed primitives, so `new String("hi")` returns
/// the primitive `"hi"` (Step 2b keeps method compat without boxing).
pub fn string_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    if args.argc == 0 {
        return Ok(Value::String(JsString::new()));
    }
    Ok(Value::String(vm.to_js_string(args.get(vm, 0), 0)))
}

// ── string method implementations ────────────────────────────────────────────

/// `s.split(delim[, limit])` → array of substrings. Delimiter may be a
/// string or RegExp; capturing groups in a RegExp delimiter are spliced
/// into the result (JS semantics).
pub fn str_split(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let delim = args.get(vm, 1);
    if matches!(delim, Value::Undefined) {
        let parts: ThinVec<Value> = thin_vec::thin_vec![Value::String(s)];
        return Ok(vm.alloc_array(parts));
    }
    let limit = match args.get(vm, 2) {
        Value::Undefined => None,
        v => {
            let lim = v
                .to_number()
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
            if lim.is_nan() || lim.is_infinite() || lim < 0.0 {
                None
            } else {
                Some((lim as u32) as usize)
            }
        }
    };
    // RegExp delimiter path. Capturing groups in the delimiter are spliced
    // into the result (JS semantics: `"a1b".split(/(\d)/)` → ["a","1","b"]).
    if let Some(rx) = try_reg_exp(vm, delim) {
        let text = s.as_units();
        let mut parts: ThinVec<Value> = ThinVec::new();
        let mut last = 0;
        'outer: for m in find_all(rx, text) {
            if limit.is_some_and(|lim| parts.len() >= lim) {
                break;
            }
            parts.push(Value::String(JsString::from_units(
                &text[last..m.range.start],
            )));
            for cap in &m.captures {
                if limit.is_some_and(|lim| parts.len() >= lim) {
                    break 'outer;
                }
                parts.push(match cap {
                    Some(r) => Value::String(JsString::from_units(&text[r.clone()])),
                    None => Value::Undefined,
                });
            }
            last = m.range.end;
        }
        // Push the remainder.
        if limit.is_none_or(|lim| parts.len() < lim) {
            parts.push(Value::String(JsString::from_units(&text[last..])));
        }
        return Ok(vm.alloc_array(parts));
    }
    // String delimiter path.
    let delim_s = vm.string_from(delim)?;
    let text = s.as_units();
    let sep = delim_s.as_units();
    let parts: ThinVec<Value> = if sep.is_empty() {
        // **Code units, not code points.** `"😀".split("")` is
        // `["\uD83D", "\uDE00"]` in JS: the empty separator splits between
        // every pair of *units*, which is the one place in this file where
        // a surrogate pair is deliberately cut in half.
        let each: ThinVec<Value> = text
            .iter()
            .map(|&u| Value::String(JsString::from_units(&[u])))
            .collect();
        match limit {
            Some(lim) => each.into_iter().take(lim).collect(),
            None => each,
        }
    } else {
        let mut splits: ThinVec<Value> = ThinVec::new();
        let mut last = 0;
        while let Some(at) = units::find(text, sep, last) {
            splits.push(Value::String(JsString::from_units(&text[last..at])));
            last = at + sep.len();
        }
        splits.push(Value::String(JsString::from_units(&text[last..])));
        match limit {
            Some(lim) => splits.into_iter().take(lim).collect(),
            None => splits,
        }
    };
    Ok(vm.alloc_array(parts))
}

/// `s.includes(needle[, start])` → bool. An absent needle is coerced to
/// the string `"undefined"` (matching JS).
pub fn str_includes(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.string_receiver(vm)?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let hay = haystack.as_units();
    let start = (start.max(0) as usize).min(hay.len());
    Ok(Value::Bool(
        units::find(hay, needle.as_units(), start).is_some(),
    ))
}

/// `s.indexOf(needle[, start])` → int (or -1). An absent needle is coerced to
/// the string `"undefined"` (matching JS).
pub fn str_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.string_receiver(vm)?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let hay = haystack.as_units();
    let start = (start.max(0) as usize).min(hay.len());
    let pos = units::find(hay, needle.as_units(), start).map(|p| p as f64);
    Ok(Value::int_from_f64(pos.unwrap_or(-1.0)))
}

/// `s.lastIndexOf(needle[, start])` → int (or -1). An absent needle is
/// coerced to the string `"undefined"` (matching JS).
pub fn str_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.string_receiver(vm)?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let hay = haystack.as_units();
    let start = match args.get(vm, 2) {
        Value::Undefined => hay.len() as i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let from = (start.max(0) as usize).min(hay.len());
    let pos = units::rfind(hay, needle.as_units(), from).map(|p| p as f64);
    Ok(Value::int_from_f64(pos.unwrap_or(-1.0)))
}

/// `s.startsWith(prefix)` → bool. An absent prefix is coerced to the string
/// `"undefined"` (matching JS).
pub fn str_starts_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.string_receiver(vm)?;
    let prefix = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::Bool(
        haystack.as_units().starts_with(prefix.as_units()),
    ))
}

/// `s.endsWith(suffix)` → bool. An absent suffix is coerced to the string
/// `"undefined"` (matching JS).
pub fn str_ends_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.string_receiver(vm)?;
    let suffix = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::Bool(
        haystack.as_units().ends_with(suffix.as_units()),
    ))
}

/// `s.slice(start[, end])` → substring over a half-open code-unit range.
/// JS semantics: negative indices count from end, everything clamps,
/// `start ≥ end` → `""`.
///
/// **There is no error case left.** This used to clamp each end to a UTF-8
/// character boundary and then check whether it had moved — a check that was
/// dead for every in-range index, because the clamps could only ever move
/// inwards. `"aéb".slice(0, 2)` quietly returned one character where two were
/// asked for. Over code units the indices mean what the caller meant.
pub fn str_slice(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let len = s.as_units().len() as i64;

    let to_offset = |v: &Value, default: i64| -> Result<i64, VMError> {
        if matches!(v, Value::Undefined) {
            return Ok(default);
        }
        let n = v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if n < 0 {
            Ok((n + len).max(0))
        } else {
            Ok(n.min(len))
        }
    };

    let start = to_offset(args.get(vm, 1), 0)? as usize;
    let end = to_offset(args.get(vm, 2), len)?.max(0) as usize;
    Ok(substring(&s, start, end))
}

/// `s.substring(start[, end])` → substring. Like `slice` but swaps
/// arguments when `start > end` and treats negative values as 0.
pub fn str_substring(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let len = s.as_units().len() as i64;

    let to_offset = |v: &Value, default: i64| -> i64 {
        if matches!(v, Value::Undefined) {
            return default;
        }
        v.to_number()
            .map(|n| {
                let i = n as i64;
                i.max(0).min(len)
            })
            .unwrap_or(default)
    };

    let mut start = to_offset(args.get(vm, 1), 0) as usize;
    let mut end = to_offset(args.get(vm, 2), len) as usize;

    if start > end {
        std::mem::swap(&mut start, &mut end);
    }

    Ok(substring(&s, start, end))
}

/// `s.trim()` → trimmed string.
pub fn str_trim(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let u = s.as_units();
    let (a, b) = units::trim_range(u);
    Ok(Value::String(JsString::from_units(&u[a..b])))
}

/// `s.trimStart()` → left-trimmed string.
pub fn str_trim_start(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let u = s.as_units();
    Ok(Value::String(JsString::from_units(
        &u[units::trim_start_index(u)..],
    )))
}

/// `s.trimEnd()` → right-trimmed string.
pub fn str_trim_end(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let u = s.as_units();
    Ok(Value::String(JsString::from_units(
        &u[..units::trim_end_index(u)],
    )))
}

/// `s.replace(pattern, replacement)` — pattern may be a string or RegExp.
/// With a string pattern, replaces only the first occurrence.
/// With a RegExp without the `g` flag, replaces only the first match.
/// With a RegExp with the `g` flag, replaces all matches.
/// Supports JS replacement patterns: `$$`, `$&`, ``$` ``, `$'`, `$1`..`$9`.
pub fn str_replace(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let replacement = vm.to_js_string(args.get(vm, 2), 0);
    if let Some(rx) = try_reg_exp(vm, args.get(vm, 1)) {
        let text = s.as_units();
        if rx.has_flag(b'g') {
            let mut out: Vec<u16> = Vec::new();
            let mut last = 0;
            for m in find_all(rx, text) {
                out.extend_from_slice(&text[last..m.range.start]);
                if !push_replacement(&mut out, replacement.as_units(), text, &m) {
                    return Err(vm.fail(ErrorKind::RangeError, TOO_LARGE));
                }
                last = m.range.end;
            }
            out.extend_from_slice(&text[last..]);
            return Ok(Value::String(JsString::from_units(&out)));
        } else {
            if let Some(m) = find_at(rx, text, 0) {
                let mut out: Vec<u16> = Vec::with_capacity(text.len());
                out.extend_from_slice(&text[..m.range.start]);
                if !push_replacement(&mut out, replacement.as_units(), text, &m) {
                    return Err(vm.fail(ErrorKind::RangeError, TOO_LARGE));
                }
                out.extend_from_slice(&text[m.range.end..]);
                return Ok(Value::String(JsString::from_units(&out)));
            }
            return Ok(Value::String(s));
        }
    }
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    let text = s.as_units();
    let pat = pattern.as_units();
    if let Some(idx) = units::find(text, pat, 0) {
        let repl = replacement.as_units();
        let mut out: Vec<u16> = Vec::with_capacity(text.len() - pat.len() + repl.len());
        out.extend_from_slice(&text[..idx]);
        out.extend_from_slice(repl);
        out.extend_from_slice(&text[idx + pat.len()..]);
        Ok(Value::String(JsString::from_units(&out)))
    } else {
        Ok(Value::String(s))
    }
}

/// `s.replaceAll(pattern, replacement)` — pattern may be a string or
/// RegExp. If pattern is a RegExp, it must have the `g` flag (per JS spec).
/// Supports JS replacement patterns: `$$`, `$&`, ``$` ``, `$'`, `$1`..`$9`.
pub fn str_replace_all(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let replacement = vm.to_js_string(args.get(vm, 2), 0);
    if let Some(rx) = try_reg_exp(vm, args.get(vm, 1)) {
        if !rx.has_flag(b'g') {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "replaceAll must be called with a global RegExp — add the `g` flag, as in `/…/g`",
            ));
        }
        let text = s.as_units();
        let mut out: Vec<u16> = Vec::new();
        let mut last = 0;
        for m in find_all(rx, text) {
            out.extend_from_slice(&text[last..m.range.start]);
            if !push_replacement(&mut out, replacement.as_units(), text, &m) {
                return Err(vm.fail(ErrorKind::RangeError, TOO_LARGE));
            }
            last = m.range.end;
        }
        out.extend_from_slice(&text[last..]);
        return Ok(Value::String(JsString::from_units(&out)));
    }
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    let text = s.as_units();
    let pat = pattern.as_units();
    let repl = replacement.as_units();
    if pat.is_empty() {
        // **An empty search matches at every position, including the two
        // ends.** `"abc".replaceAll("", "-")` is `"-a-b-c-"` and
        // `"".replaceAll("", "x")` is `"x"`; returning the receiver unchanged
        // (which is what a naive "nothing to find" guard does) fails both.
        if repl.len().saturating_mul(text.len() + 1) > MAX_STRING_LEN {
            return Err(vm.fail(ErrorKind::RangeError, TOO_LARGE));
        }
        let mut out: Vec<u16> = Vec::with_capacity(text.len() + repl.len() * (text.len() + 1));
        out.extend_from_slice(repl);
        for &u in text {
            out.push(u);
            out.extend_from_slice(repl);
        }
        return Ok(Value::String(JsString::from_units(&out)));
    }
    let mut out: Vec<u16> = Vec::with_capacity(text.len());
    let mut last = 0;
    while let Some(at) = units::find(text, pat, last) {
        out.extend_from_slice(&text[last..at]);
        out.extend_from_slice(repl);
        last = at + pat.len();
    }
    out.extend_from_slice(&text[last..]);
    Ok(Value::String(JsString::from_units(&out)))
}

/// `s.localeCompare(other)` → -1 / 0 / 1.
///
/// Plain code-unit order, no locale and no collation: this dialect has
/// no locale data and inventing one would make the result depend on
/// something the program cannot see. That matches what it is actually
/// reached for — `xs.sort((a, b) => a.localeCompare(b))` is simply how
/// a string sort is written, and every other spelling of it works here
/// already.
///
/// Code-unit order is also what JS's `<` on strings is defined as, so this
/// and the relational operators now agree where the old code-point ordering
/// made them differ above the BMP.
///
/// Observed live 2026-09-16: a program sorting edit sites by path wrote
/// exactly that comparator and trapped with "cannot call a undefined as
/// a function", costing the run a program. Found by `agent score` on
/// the log, which is what that command is for.
pub fn str_locale_compare(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let other = vm.to_js_string(args.get(vm, 1), 0);
    Ok(match s.as_units().cmp(other.as_units()) {
        std::cmp::Ordering::Less => Value::NegInt(-1),
        std::cmp::Ordering::Equal => Value::PosInt(0),
        std::cmp::Ordering::Greater => Value::PosInt(1),
    })
}

/// Upper bound on a built string's code-unit length. JS engines cap string
/// length (V8 ≈2^30) and throw `RangeError`; this dialect has no `RangeError`
/// kind, so the string builders raise a loud `ValueError` instead of attempting
/// a multi-gigabyte allocation that would OOM the whole process. test262's
/// `staging/sm/String/replace-math.js` builds a 2^36-char (~64 GiB) string by
/// expanding a 2^20-char `$1` capture 2^16 times in one `replace`, which is
/// what motivated this guard. 256M units is far above any realistic agent
/// string.
pub(crate) const MAX_STRING_LEN: usize = 256 * 1024 * 1024;

/// Diagnostic raised when a string builder would exceed [`MAX_STRING_LEN`].
const TOO_LARGE: &str = "result string too large (max 256MiB)";

/// Append the JS replacement pattern to `out`, substituting `$n`, `$<name>`,
/// `$&`, ``$` ``, `$'`, and `$$` from the match's captures. Returns `false` if
/// the result would exceed [`MAX_STRING_LEN`] (checked before each token, so a
/// single overshoot is bounded by one `text` length); the caller turns that
/// into a `ValueError` rather than building an unbounded string.
#[must_use]
fn push_replacement(out: &mut Vec<u16>, repl: &[u16], text: &[u16], m: &regress::Match) -> bool {
    let mut i = 0;
    while i < repl.len() {
        if out.len() > MAX_STRING_LEN {
            return false;
        }
        let c = repl[i];
        if c != b'$' as u16 {
            out.push(c);
            i += 1;
            continue;
        }
        let next = match repl.get(i + 1) {
            Some(&ch) => ch,
            None => {
                out.push(c);
                break;
            }
        };
        i += 2;
        match next as u8 as char {
            '$' if next < 0x80 => out.push(b'$' as u16),
            '&' if next < 0x80 => out.extend_from_slice(&text[m.range.clone()]),
            '`' if next < 0x80 => out.extend_from_slice(&text[..m.range.start]),
            '\'' if next < 0x80 => out.extend_from_slice(&text[m.range.end..]),
            '0'..='9' if next < 0x80 => {
                let mut n = (next - b'0' as u16) as usize;
                while let Some(&c2) = repl.get(i) {
                    if !(b'0' as u16..=b'9' as u16).contains(&c2) {
                        break;
                    }
                    i += 1;
                    n = n
                        .saturating_mul(10)
                        .saturating_add((c2 - b'0' as u16) as usize);
                }
                if n > 0
                    && n <= m.captures.len()
                    && let Some(cap) = m.captures.get(n - 1)
                    && let Some(range) = cap
                {
                    out.extend_from_slice(&text[range.clone()]);
                }
            }
            '<' if next < 0x80 => {
                // `$<name>` — named group reference. If unterminated, emit
                // the consumed text literally (matching JS leniency).
                let start = i;
                let mut closed = None;
                while i < repl.len() {
                    if repl[i] == b'>' as u16 {
                        closed = Some(i);
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                match closed {
                    Some(end) => {
                        let name = crate::js_string::units_to_utf8_lossy(&repl[start..end], false);
                        if let Some(range) = m.named_group(&name) {
                            out.extend_from_slice(&text[range]);
                        }
                    }
                    None => {
                        out.push(b'$' as u16);
                        out.push(b'<' as u16);
                        out.extend_from_slice(&repl[start..]);
                    }
                }
            }
            _ => {
                out.push(b'$' as u16);
                i -= 1;
            }
        }
    }
    true
}

/// `s.match(pattern)` — pattern may be a string or RegExp.
/// Without the `g` flag: returns the same as `pattern.exec(s)`.
/// With the `g` flag: returns an array of all full-match strings (no captures).
/// `s.matchAll(re)` → an **array** of exec-shaped match objects (each
/// `{ "0": full, "1": cap1, …, index, input }`), one per non-overlapping
/// match — the capture-aware, single-call, always-terminating counterpart
/// to the stateful `exec` loop. JS returns a lazy iterator, but this
/// dialect has no generators, so an array is the faithful shape. Requires
/// a global (`/g`) RegExp, like JS; it does not consult or mutate
/// `lastIndex`.
pub fn str_match_all(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let rx = match try_reg_exp(vm, args.get(vm, 1)) {
        Some(rx) => rx.clone(),
        None => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "matchAll must be called with a global RegExp — add the `g` flag, as in `/…/g`",
            ));
        }
    };
    if !rx.has_flag(b'g') {
        return Err(vm.fail(
            ErrorKind::TypeError,
            "matchAll must be called with a global RegExp",
        ));
    }
    let matches = find_all(&rx, s.as_units());
    let mut out: ThinVec<Value> = ThinVec::with_capacity(matches.len());
    for m in matches {
        let obj = build_exec_result(vm, &m, s.clone());
        out.push(vm.alloc_object(obj));
    }
    Ok(vm.alloc_array(out))
}

pub fn str_match(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    if let Some(rx) = try_reg_exp(vm, args.get(vm, 1)) {
        let text = s.as_units();
        if rx.has_flag(b'g') {
            let matches: ThinVec<Value> = find_all(rx, text)
                .into_iter()
                .map(|m| Value::String(JsString::from_units(&text[m.range])))
                .collect();
            if matches.is_empty() {
                return Ok(Value::Null);
            }
            return Ok(vm.alloc_array(matches));
        } else {
            // Non-global: same result shape as exec().
            let m = match find_at(rx, text, 0) {
                Some(m) => m,
                None => return Ok(Value::Null),
            };
            let obj = build_exec_result(vm, &m, s.clone());
            return Ok(vm.alloc_object(obj));
        }
    }
    // String pattern: treat as a literal (not a RegExp).
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    let pat = pattern.as_units();
    if pat.is_empty() {
        // Empty string: return [""] (JS: empty string matches at start of string).
        return Ok(vm.alloc_array(thin_vec::thin_vec![Value::String(JsString::new())]));
    }
    if units::find(s.as_units(), pat, 0).is_some() {
        return Ok(vm.alloc_array(thin_vec::thin_vec![Value::String(pattern)]));
    }
    Ok(Value::Null)
}

/// `s.search(pattern)` — pattern may be a string or RegExp.
/// Returns the index of the first match, or -1 if not found.
pub fn str_search(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx: i64 = if let Some(rx) = try_reg_exp(vm, args.get(vm, 1)) {
        find_at(rx, s.as_units(), 0)
            .map(|m| m.range.start as i64)
            .unwrap_or(-1)
    } else {
        let pattern = vm.to_js_string(args.get(vm, 1), 0);
        units::find(s.as_units(), pattern.as_units(), 0)
            .map(|i| i as i64)
            .unwrap_or(-1)
    };
    if idx >= 0 {
        Ok(Value::PosInt(idx as u64))
    } else {
        Ok(Value::NegInt(idx))
    }
}

/// `s.toLowerCase()` → lowercase string.
pub fn str_to_lower_case(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::String(JsString::from_units(&units::to_lowercase(
        s.as_units(),
    ))))
}

/// `s.toUpperCase()` → uppercase string.
pub fn str_to_upper_case(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::String(JsString::from_units(&units::to_uppercase(
        s.as_units(),
    ))))
}

/// `s.padStart(targetLength[, padString])` → padded string.
///
/// **Target and filler are both measured in code units**, which is the only
/// reading under which the result has the length that was asked for.
/// `"a".padStart(3, "💩")` used to measure the target in bytes and append the
/// filler in whole characters, producing a string of `.length` 9; in the
/// other direction `"😀".padStart(4, "-")` padded nothing, because the
/// receiver was already "4 long". Truncating the filler mid-pair is what the
/// spec says to do.
pub fn str_pad_start(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    pad(vm, args, true)
}

/// `s.padEnd(targetLength[, padString])` → padded string. See `padStart`.
pub fn str_pad_end(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    pad(vm, args, false)
}

fn pad(vm: &mut VM, args: Args, at_start: bool) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let target_len = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let pad: JsString = match args.get(vm, 2) {
        Value::Undefined => JsString::from(" "),
        v => vm.to_js_string(v, 0),
    };
    let text = s.as_units();
    let filler = pad.as_units();
    if target_len.is_nan() || target_len <= text.len() as f64 || filler.is_empty() {
        return Ok(Value::String(s));
    }
    if target_len > MAX_STRING_LEN as f64 {
        return Err(vm.fail(ErrorKind::RangeError, TOO_LARGE));
    }
    let target = target_len as usize;
    let needed = target - text.len();
    let mut out: Vec<u16> = Vec::with_capacity(target);
    if !at_start {
        out.extend_from_slice(text);
    }
    for i in 0..needed {
        out.push(filler[i % filler.len()]);
    }
    if at_start {
        out.extend_from_slice(text);
    }
    Ok(Value::String(JsString::from_units(&out)))
}

/// `s.repeat(count)` → repeated string. A count outside `[0, REPEAT_MAX]` is
/// a `RangeError`, the name JS gives it.
pub fn str_repeat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let count = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    if count < 0.0 || count.is_infinite() {
        return Err(vm.fail(
            ErrorKind::RangeError,
            format!("repeat count must be a finite number >= 0; got {count}").as_str(),
        ));
    }
    // Bound the allocation, but raise loudly instead of silently truncating
    // (a silent cap produces a wrong-length string with no signal).
    const REPEAT_MAX: usize = 1_000_000;
    let n = count as usize;
    if n > REPEAT_MAX {
        return Err(vm.fail(
            ErrorKind::RangeError,
            "repeat count too large (max 1000000)",
        ));
    }
    let text = s.as_units();
    let mut out: Vec<u16> = Vec::with_capacity(text.len().saturating_mul(n));
    for _ in 0..n {
        out.extend_from_slice(text);
    }
    Ok(Value::String(JsString::from_units(&out)))
}

/// `s.charAt(index)` → the **one code unit** at that index, or the empty
/// string when the index is out of range.
///
/// **The same answer as `s[index]`, because it is the same question.** Both
/// read a code-unit index. Before 2026-09-24 `charAt` handed back
/// `byte as char` — the raw UTF-8 byte reinterpreted as a codepoint — so on
/// `"—b"` `s[0]` gave `"—"` and `s.charAt(0)` gave `"â"`: `s[0] ===
/// s.charAt(0)` was *false* at a perfectly valid boundary, and the character
/// `charAt` returned was not in the string at all.
pub fn str_char_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))? as i64;
    let u = s.as_units();
    if idx < 0 || idx as usize >= u.len() {
        return Ok(Value::String(JsString::new()));
    }
    Ok(Value::String(JsString::from_units(&u[idx as usize..][..1])))
}

/// `s.at(index)` → the code unit at index (negative counts from the end), or
/// `undefined` when out of range.
pub fn str_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let u = s.as_units();
    let len = u.len() as i64;
    let i = if idx < 0.0 {
        idx as i64 + len
    } else {
        idx as i64
    };
    if i < 0 || i >= len {
        return Ok(Value::Undefined);
    }
    Ok(Value::String(JsString::from_units(&u[i as usize..][..1])))
}

/// `s.charCodeAt(index)` → the numeric value of the code unit there, or `NaN`.
///
/// **Undefinable over UTF-8 bytes, a one-liner over code units.** There was
/// no way in this dialect to ask what unit is at position `i`; a program that
/// reached for the JS spelling got `cannot call a undefined as a function`.
pub fn str_char_code_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let u = s.as_units();
    if idx.is_nan() || idx < 0.0 || idx >= u.len() as f64 {
        return Ok(Value::Float(f64::NAN));
    }
    Ok(Value::PosInt(u[idx as usize] as u64))
}

/// `s.codePointAt(index)` → the code point starting there, or `undefined`.
/// An unpaired surrogate reads as itself, which is what the spec says.
pub fn str_code_point_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let u = s.as_units();
    if idx.is_nan() || idx < 0.0 || idx >= u.len() as f64 {
        return Ok(Value::Undefined);
    }
    match units::code_point_at(u, idx as usize) {
        Some((cp, _)) => Ok(Value::PosInt(cp as u64)),
        None => Ok(Value::Undefined),
    }
}

/// `s.isWellFormed()` → whether every surrogate in `s` is paired.
///
/// One of the two mitigations for the U+FFFD policy at the JSON boundary: a
/// program that cares whether its string will survive the crossing can ask.
pub fn str_is_well_formed(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::Bool(units::is_well_formed(s.as_units())))
}

/// `s.toWellFormed()` → `s` with each unpaired surrogate replaced by U+FFFD.
pub fn str_to_well_formed(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    if units::is_well_formed(s.as_units()) {
        return Ok(Value::String(s));
    }
    Ok(Value::String(JsString::from_units(&units::to_well_formed(
        s.as_units(),
    ))))
}

/// `s.normalize([form])` is **not** here, deliberately: it needs a Unicode
/// normalisation table, which is a dependency decision and not a
/// representation one. See `docs/30_STRINGS.md`'s "what stays broken".
/// `s.concat(str1, str2, …)` → concatenated string. Receiver must be a string.
pub fn str_concat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    // Receiver must be a string (the getter defers an Object receiver); the
    // *arguments* are coerced, matching JS `concat`.
    let recv = args.string_receiver(vm)?;
    let mut out: Vec<u16> = recv.as_units().to_vec();
    for i in 1..args.argc {
        let piece = vm.to_js_string(args.get(vm, i), 0);
        out.extend_from_slice(piece.as_units());
    }
    Ok(Value::String(JsString::from_units(&out)))
}

/// `String.fromCharCode(c1, c2, …)` → string from UTF-16 code units.
///
/// **Each argument truncates to 16 bits and is kept.** It used to be masked,
/// passed to `char::from_u32` and then *dropped* when that returned `None` —
/// which is every surrogate, and a surrogate pair is the only way
/// `fromCharCode` can express an astral character. So
/// `String.fromCharCode(0xD83D, 0xDE00)` produced the empty string, with no
/// error: a program building text from code units got nothing back.
pub fn str_from_char_code(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut out: Vec<u16> = Vec::with_capacity(args.argc);
    for i in 0..args.argc {
        let n = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        out.push(to_uint16(n));
    }
    Ok(Value::String(JsString::from_units(&out)))
}

/// `ToUint16` — the spec's modular truncation, not a saturating `as`.
fn to_uint16(n: f64) -> u16 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc();
    let m = i.rem_euclid(65536.0);
    m as u16
}

/// `String.fromCodePoint(c1, c2, …)` → string from Unicode code points.
///
/// A lone surrogate value is a *valid* argument here (`fromCodePoint(0xD800)`
/// is `"\uD800"`); only a non-integer or an out-of-range value is rejected.
pub fn str_from_code_point(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut out: Vec<u16> = Vec::with_capacity(args.argc);
    for i in 0..args.argc {
        let n = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if !n.is_finite() || n.trunc() != n || n < 0.0 || n > 0x10FFFF as f64 {
            return Err(vm.fail(
                ErrorKind::RangeError,
                format!("{n} is not a valid code point (want an integer in [0, 0x10FFFF])")
                    .as_str(),
            ));
        }
        let code = n as u32;
        if code < 0x10000 {
            out.push(code as u16);
        } else {
            let c = code - 0x10000;
            out.push(0xD800 + (c >> 10) as u16);
            out.push(0xDC00 + (c & 0x3FF) as u16);
        }
    }
    Ok(Value::String(JsString::from_units(&out)))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// A half-open code-unit range of `s`, clamped, as a new string.
///
/// This replaces `extract_substring`, `clamp_start` and `clamp_end`, which
/// existed only to paper over byte offsets and between them produced the
/// wrong string rather than the error they advertised.
fn substring(s: &JsString, start: usize, end: usize) -> Value {
    let u = s.as_units();
    let start = start.min(u.len());
    let end = end.min(u.len());
    if start >= end {
        return Value::String(JsString::new());
    }
    Value::String(JsString::from_units(&u[start..end]))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{
        ErrorKind, Value,
        builtin::Builtin,
        testutil::{self, run_instrs},
        vm::Instr,
    };

    // ── StrSplit ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_split() {
        let out = run_instrs(vec![
            Instr::PushStr("a,b,c".into()),
            Instr::PushStr(",".into()),
            Instr::CallBuiltin(Builtin::StrSplit, 2),
        ]);
        // result is an array
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_str_split_with_limit() {
        let out = run_instrs(vec![
            Instr::PushStr("a,b,c".into()),
            Instr::PushStr(",".into()),
            Instr::PushPosInt(2),
            Instr::CallBuiltin(Builtin::StrSplit, 3),
        ]);
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    // ── StrIncludes ────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_includes() {
        let out = run_instrs(vec![
            Instr::PushStr("hello world".into()),
            Instr::PushStr("world".into()),
            Instr::CallBuiltin(Builtin::StrIncludes, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_includes_not_found() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("x".into()),
            Instr::CallBuiltin(Builtin::StrIncludes, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(false)]);
    }

    // ── StrIndexOf ─────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_index_of() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("l".into()),
            Instr::CallBuiltin(Builtin::StrIndexOf, 2),
        ]);
        assert_eq!(out, vec![Value::PosInt(2)]);
    }

    #[test]
    fn call_builtin_str_index_of_not_found() {
        let out = run_instrs(vec![
            Instr::PushStr("abc".into()),
            Instr::PushStr("x".into()),
            Instr::CallBuiltin(Builtin::StrIndexOf, 2),
        ]);
        assert_eq!(out, vec![Value::NegInt(-1)]);
    }

    // ── StrLastIndexOf ─────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_last_index_of() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("l".into()),
            Instr::CallBuiltin(Builtin::StrLastIndexOf, 2),
        ]);
        assert_eq!(out, vec![Value::PosInt(3)]);
    }

    // ── negative `start` clamps to 0, matching JS (rather than failing) ─

    #[test]
    fn call_builtin_str_index_of_negative_start_clamps() {
        // "hello".indexOf("h", -5) === 0
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("h".into()),
            Instr::PushNegInt(-5),
            Instr::CallBuiltin(Builtin::StrIndexOf, 3),
        ]);
        assert_eq!(out, vec![Value::PosInt(0)]);
    }

    #[test]
    fn call_builtin_str_includes_negative_start_clamps() {
        // "hello".includes("h", -5) === true
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("h".into()),
            Instr::PushNegInt(-5),
            Instr::CallBuiltin(Builtin::StrIncludes, 3),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_last_index_of_negative_start_clamps() {
        // "hello".lastIndexOf("l", -3) === -1 (only an index-0 match qualifies)
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("l".into()),
            Instr::PushNegInt(-3),
            Instr::CallBuiltin(Builtin::StrLastIndexOf, 3),
        ]);
        assert_eq!(out, vec![Value::NegInt(-1)]);
    }

    // ── StrStartsWith / StrEndsWith ────────────────────────────────────

    #[test]
    fn call_builtin_str_starts_with() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("hel".into()),
            Instr::CallBuiltin(Builtin::StrStartsWith, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_str_ends_with() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushStr("lo".into()),
            Instr::CallBuiltin(Builtin::StrEndsWith, 2),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    // ── StrSlice ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_slice() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushPosInt(1),
            Instr::PushPosInt(4),
            Instr::CallBuiltin(Builtin::StrSlice, 3),
        ]);
        match &out[0] {
            Value::String(s) => assert!(s.eq_str("ell")),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_str_slice_single_arg() {
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::PushPosInt(2),
            Instr::CallBuiltin(Builtin::StrSlice, 2),
        ]);
        match &out[0] {
            Value::String(s) => assert!(s.eq_str("llo")),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── StrTrim ────────────────────────────────────────────────────────

    #[test]
    fn call_builtin_str_trim() {
        let out = run_instrs(vec![
            Instr::PushStr("  hi  ".into()),
            Instr::CallBuiltin(Builtin::StrTrim, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert!(s.eq_str("hi")),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── Step 3: JS contract fixes ─────────────────────────────────────

    #[test]
    fn js_split_limit_truncates_not_splitn() {
        // JS: "a,b,c".split(",", 2) → ["a", "b"]
        assert_eq!(
            testutil::run_ret("return 'a,b,c'.split(',', 2);"),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn js_split_empty_string_to_chars() {
        // JS: "abc".split("") → ["a", "b", "c"]
        assert_eq!(
            testutil::run_ret("return 'abc'.split('');"),
            serde_json::json!(["a", "b", "c"])
        );
    }

    #[test]
    fn js_split_limit_coercion() {
        // JS: limit 0 → []; negative → effectively no limit
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',', 0);"),
            serde_json::json!([])
        );
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',', -1);"),
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',', 2.9);"),
            serde_json::json!(["a", "b"])
        );
    }

    // ── Step 4b: string method tests ──────────────────────────────────

    #[test]
    fn string_replace_and_replace_all() {
        // replace only the first occurrence.
        assert_eq!(
            testutil::run_ret("return 'aba'.replace('a', 'x');"),
            serde_json::json!("xba")
        );
        // replaceAll replaces all.
        assert_eq!(
            testutil::run_ret("return 'aba'.replaceAll('a', 'x');"),
            serde_json::json!("xbx")
        );
    }

    #[test]
    fn string_case_methods() {
        assert_eq!(
            testutil::run_ret("return 'Hello'.toLowerCase();"),
            serde_json::json!("hello")
        );
        assert_eq!(
            testutil::run_ret("return 'Hello'.toUpperCase();"),
            serde_json::json!("HELLO")
        );
    }

    #[test]
    fn string_pad() {
        assert_eq!(
            testutil::run_ret("return '5'.padStart(3, '0');"),
            serde_json::json!("005")
        );
        assert_eq!(
            testutil::run_ret("return '5'.padEnd(3, '0');"),
            serde_json::json!("500")
        );
    }

    #[test]
    fn string_repeat() {
        assert_eq!(
            testutil::run_ret("return 'ab'.repeat(0);"),
            serde_json::json!("")
        );
        assert_eq!(
            testutil::run_ret("return 'ab'.repeat(2);"),
            serde_json::json!("abab")
        );
        // Negative → `RangeError`, as in JS.
        assert_eq!(
            testutil::run_err_kind("return 'ab'.repeat(-1);"),
            ErrorKind::RangeError
        );
        // Over the cap → a loud `RangeError`, not a silently-truncated
        // string.
        assert_eq!(
            testutil::run_err_kind("return 'x'.repeat(2000000);"),
            ErrorKind::RangeError
        );
        // A large-but-allowed count still works.
        assert_eq!(
            testutil::run_ret("return 'x'.repeat(20000).length;"),
            serde_json::json!(20000)
        );
    }

    #[test]
    fn split_includes_capture_groups() {
        // JS: a capturing delimiter splices its groups into the result.
        assert_eq!(
            testutil::run_ret(r"return 'a1b2c'.split(/(\d)/);"),
            serde_json::json!(["a", "1", "b", "2", "c"])
        );
        // Non-capturing delimiter: gaps only (unchanged behavior).
        assert_eq!(
            testutil::run_ret(r"return 'a1b2c'.split(/\d/);"),
            serde_json::json!(["a", "b", "c"])
        );
        // Limit counts total array elements, captures included.
        assert_eq!(
            testutil::run_ret(r"return 'a1b2c'.split(/(\d)/, 3);"),
            serde_json::json!(["a", "1", "b"])
        );
    }

    #[test]
    fn replace_supports_named_group_token() {
        assert_eq!(
            testutil::run_ret(r"return 'x5'.replace(/(?<d>\d)/, '[$<d>]');"),
            serde_json::json!("x[5]")
        );
    }

    #[test]
    fn replace_with_function_replacer() {
        // Regex, non-global → first match; callback gets (match, ...caps, index, str).
        assert_eq!(
            testutil::run_ret(r"return 'a1b2'.replace(/(\d)/, (m, d) => '[' + d + ']');"),
            serde_json::json!("a[1]b2")
        );
        // Global regex → every match.
        assert_eq!(
            testutil::run_ret(r"return 'a1b2'.replace(/\d/g, m => '#');"),
            serde_json::json!("a#b#")
        );
        // String pattern → first occurrence; callback gets (match, index, str).
        assert_eq!(
            testutil::run_ret("return 'a.a'.replace('a', (m, i) => i);"),
            serde_json::json!("0.a")
        );
        // The callback's offset/input args are correct.
        assert_eq!(
            testutil::run_ret(r"return 'xy'.replace(/y/, (m, i, s) => i + ':' + s);"),
            serde_json::json!("x1:xy")
        );
    }

    #[test]
    fn replace_all_with_function_replacer() {
        // Global regex with a capture group.
        assert_eq!(
            testutil::run_ret(r"return 'a1b2'.replaceAll(/(\d)/g, (m, d) => d + d);"),
            serde_json::json!("a11b22")
        );
        // String pattern → all occurrences.
        assert_eq!(
            testutil::run_ret("return 'aaa'.replaceAll('a', (m, i) => i);"),
            serde_json::json!("012")
        );
        // A non-global regex to replaceAll still throws (matchAll enforces it).
        assert_eq!(
            testutil::run_err_kind("return 'ab'.replaceAll(/a/, m => m);"),
            ErrorKind::TypeError
        );
    }

    #[test]
    fn replace_string_replacer_still_works() {
        // The string-replacer path (delegated to the builtin) is unchanged.
        assert_eq!(
            testutil::run_ret(r"return 'a1b2'.replace(/(\d)/, '<$1>');"),
            serde_json::json!("a<1>b2")
        );
        assert_eq!(
            testutil::run_ret(r"return 'a1b2'.replaceAll(/(\d)/g, '<$1>');"),
            serde_json::json!("a<1>b<2>")
        );
    }

    #[test]
    fn string_trim_variants() {
        assert_eq!(
            testutil::run_ret("return '  a '.trimStart();"),
            serde_json::json!("a ")
        );
        assert_eq!(
            testutil::run_ret("return '  a '.trimEnd();"),
            serde_json::json!("  a")
        );
    }

    #[test]
    fn string_char_at_and_at() {
        // charAt returns empty string for OOB.
        assert_eq!(
            testutil::run_ret("return 'abc'.charAt(5);"),
            serde_json::json!("")
        );
        // at returns undefined for OOB.
        let v = testutil::run_val("return 'abc'.at(5);");
        assert_eq!(v, Value::Undefined);
        // at with negative index.
        assert_eq!(
            testutil::run_ret("return 'abc'.at(-1);"),
            serde_json::json!("c")
        );
    }

    #[test]
    fn string_concat() {
        assert_eq!(
            testutil::run_ret("return 'a'.concat('b', 'c');"),
            serde_json::json!("abc")
        );
    }

    #[test]
    fn js_slice_negative_indexes_and_clamping() {
        // JS: "abcdef".slice(-3) → "def"
        assert_eq!(
            testutil::run_ret("return 'abcdef'.slice(-3);"),
            serde_json::json!("def")
        );
        // JS: "abc".slice(2, 1) → ""
        assert_eq!(
            testutil::run_ret("return 'abc'.slice(2, 1);"),
            serde_json::json!("")
        );
        // JS: "abc".slice(0, 99) → "abc"
        assert_eq!(
            testutil::run_ret("return 'abc'.slice(0, 99);"),
            serde_json::json!("abc")
        );
    }

    #[test]
    fn js_static_arity_is_strict_runtime_is_relaxed() {
        // A method-name builtin call defers arity to runtime: for a matching
        // (string) receiver it just runs the builtin, which is lenient
        // (JS-faithful) — extra args ignored, missing optional args default.
        // No compile error, no dynamic property-read error.
        assert_eq!(
            // extra 4th arg ignored; limit 2 keeps both parts
            testutil::run_ret("return 'a,b'.split(',', 2, 3);"),
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            // missing separator → split yields [self]
            testutil::run_ret("return 'abc'.split();"),
            serde_json::json!(["abc"])
        );

        // Runtime: calling split with only a receiver via direct VM call.
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::CallBuiltin(Builtin::StrSplit, 1),
        ]);
        // Should return ["hello"] (split with undefined delimiter → [self])
        match &out[0] {
            Value::Array(_) => {} // pass
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn js_includes_absent_needle_coerces_to_string_undefined() {
        // Runtime via direct VM: calling includes with only a receiver.
        let out = run_instrs(vec![
            Instr::PushStr("undefined!".into()),
            Instr::CallBuiltin(Builtin::StrIncludes, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn js_slice_no_args_returns_whole_string() {
        // Runtime via direct VM: slice with no args returns the whole string.
        let out = run_instrs(vec![
            Instr::PushStr("hello".into()),
            Instr::CallBuiltin(Builtin::StrSlice, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert!(s.eq_str("hello")),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn replace_with_dollar_references() {
        assert_eq!(
            testutil::run_ret("return 'hello world'.replace(/world/, '[$&]');"),
            serde_json::json!("hello [world]")
        );
        assert_eq!(
            testutil::run_ret("return 'abc def'.replace(/(\\w+)\\s+(\\w+)/, '$2 $1');"),
            serde_json::json!("def abc")
        );
        assert_eq!(
            testutil::run_ret("return 'cost: 5'.replace(/\\d+/, '$$$&');"),
            serde_json::json!("cost: $5")
        );
    }

    #[test]
    fn replace_all_with_dollar_references() {
        assert_eq!(
            testutil::run_ret("return 'a,b,c'.replaceAll(/(\\w)/g, '[$1]');"),
            serde_json::json!("[a],[b],[c]")
        );
    }

    #[test]
    fn string_from_char_code() {
        assert_eq!(
            testutil::run_ret("return String.fromCharCode(72, 105, 33);"),
            serde_json::json!("Hi!")
        );
        assert_eq!(
            testutil::run_ret("return String.fromCharCode();"),
            serde_json::json!("")
        );
    }

    #[test]
    fn string_from_code_point() {
        assert_eq!(
            testutil::run_ret("return String.fromCodePoint(97, 98, 99);"),
            serde_json::json!("abc")
        );
        assert_eq!(
            testutil::run_ret("return String.fromCodePoint();"),
            serde_json::json!("")
        );
    }

    #[test]
    fn string_substring() {
        assert_eq!(
            testutil::run_ret("return 'hello'.substring(1, 4);"),
            serde_json::json!("ell")
        );
        assert_eq!(
            testutil::run_ret("return 'hello'.substring(4, 1);"),
            serde_json::json!("ell")
        );
        assert_eq!(
            testutil::run_ret("return 'hello'.substring(1);"),
            serde_json::json!("ello")
        );
        assert_eq!(
            testutil::run_ret("return 'hello'.substring(-3, 2);"),
            serde_json::json!("he")
        );
    }

    /// `xs.sort((a, b) => a.localeCompare(b))` is simply how a string
    /// sort is written; it used to trap as a call to `undefined`.
    #[test]
    fn locale_compare_orders_by_code_point() {
        assert_eq!(
            testutil::eval_str("['b','a','c'].sort((x, y) => x.localeCompare(y)).join('')"),
            "abc"
        );
        assert_eq!(testutil::run_ret("return 'a'.localeCompare('a');"), 0);
        assert_eq!(testutil::run_ret("return 'b'.localeCompare('a');"), 1);
        assert_eq!(testutil::run_ret("return 'a'.localeCompare('b');"), -1);
    }
}

use thin_vec::ThinVec;

use crate::builtin::Args;
use crate::builtin::regexp::{build_exec_result, try_reg_exp};
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};

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
        let text = s.as_str();
        let mut parts: ThinVec<Value> = ThinVec::new();
        let mut last = 0;
        'outer: for m in rx.compiled.find_iter(text) {
            if limit.is_some_and(|lim| parts.len() >= lim) {
                break;
            }
            parts.push(Value::String(RcStr::from(&text[last..m.range.start])));
            for cap in &m.captures {
                if limit.is_some_and(|lim| parts.len() >= lim) {
                    break 'outer;
                }
                parts.push(match cap {
                    Some(r) => Value::String(RcStr::from(&text[r.clone()])),
                    None => Value::Undefined,
                });
            }
            last = m.range.end;
        }
        // Push the remainder.
        if limit.is_none_or(|lim| parts.len() < lim) {
            parts.push(Value::String(RcStr::from(&text[last..])));
        }
        return Ok(vm.alloc_array(parts));
    }
    // String delimiter path.
    let delim_s = vm.string_from(delim)?;
    let parts: ThinVec<Value> = if delim_s.is_empty() {
        let chars: ThinVec<Value> = s
            .chars()
            .map(|c| Value::String(RcStr::from(c.to_string())))
            .collect();
        match limit {
            Some(lim) => chars.into_iter().take(lim).collect(),
            None => chars,
        }
    } else {
        let splits: ThinVec<Value> = s
            .split(delim_s.as_str())
            .map(|p| Value::String(RcStr::from(p)))
            .collect();
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
    let haystack = args.str_receiver(vm)?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let start = clamp_start(haystack, start.max(0) as usize);
    Ok(Value::Bool(haystack[start..].contains(needle.as_str())))
}

/// `s.indexOf(needle[, start])` → int (or -1). An absent needle is coerced to
/// the string `"undefined"` (matching JS).
pub fn str_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.str_receiver(vm)?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => 0i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let start = clamp_start(haystack, start.max(0) as usize);
    let pos = haystack[start..]
        .find(needle.as_str())
        .map(|p| (p + start) as f64);
    Ok(Value::int_from_f64(pos.unwrap_or(-1.0)))
}

/// `s.lastIndexOf(needle[, start])` → int (or -1). An absent needle is
/// coerced to the string `"undefined"` (matching JS).
pub fn str_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.str_receiver(vm)?;
    let needle = vm.to_js_string(args.get(vm, 1), 0);
    let start = match args.get(vm, 2) {
        Value::Undefined => haystack.len() as i64,
        v => v
            .as_i64()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?,
    };
    let from = start.max(0) as usize;
    let end = clamp_end(haystack, from + needle.len());
    let pos = haystack[..end].rfind(needle.as_str()).map(|p| p as f64);
    Ok(Value::int_from_f64(pos.unwrap_or(-1.0)))
}

/// `s.startsWith(prefix)` → bool. An absent prefix is coerced to the string
/// `"undefined"` (matching JS).
pub fn str_starts_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.str_receiver(vm)?;
    let prefix = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::Bool(haystack.starts_with(prefix.as_str())))
}

/// `s.endsWith(suffix)` → bool. An absent suffix is coerced to the string
/// `"undefined"` (matching JS).
pub fn str_ends_with(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let haystack = args.str_receiver(vm)?;
    let suffix = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::Bool(haystack.ends_with(suffix.as_str())))
}

/// `s.slice(start[, end])` → substring over a half-open byte range.
/// JS semantics: negative indices count from end, everything clamps,
/// `start ≥ end` → `""`. Only mid-codepoint is an error (byte-string divergence).
pub fn str_slice(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let len = s.len() as i64;

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
    extract_substring(vm, &s, start, end)
}

/// `s.substring(start[, end])` → substring. Like `slice` but swaps
/// arguments when `start > end` and treats negative values as 0.
pub fn str_substring(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let len = s.len() as i64;

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

    extract_substring(vm, &s, start, end)
}

/// `s.trim()` → trimmed string.
pub fn str_trim(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::String(RcStr::from(s.trim())))
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
        let text = s.as_str();
        if rx.flags.contains('g') {
            let mut out = String::new();
            let mut last = 0;
            for m in rx.compiled.find_iter(text) {
                out.push_str(&text[last..m.range.start]);
                push_replacement(&mut out, replacement.as_str(), text, &m);
                last = m.range.end;
            }
            out.push_str(&text[last..]);
            return Ok(Value::String(RcStr::from(out)));
        } else {
            if let Some(m) = rx.compiled.find(text) {
                let mut out = String::with_capacity(s.len());
                out.push_str(&text[..m.range.start]);
                push_replacement(&mut out, replacement.as_str(), text, &m);
                out.push_str(&text[m.range.end..]);
                return Ok(Value::String(RcStr::from(out)));
            }
            return Ok(Value::String(s));
        }
    }
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    if let Some(idx) = s.find(pattern.as_str()) {
        let mut out = String::with_capacity(s.len() - pattern.len() + replacement.len());
        out.push_str(&s[..idx]);
        out.push_str(replacement.as_str());
        out.push_str(&s[idx + pattern.len()..]);
        Ok(Value::String(RcStr::from(out)))
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
        if !rx.flags.contains('g') {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "replaceAll must be called with a global RegExp",
            ));
        }
        let text = s.as_str();
        let mut out = String::new();
        let mut last = 0;
        for m in rx.compiled.find_iter(text) {
            out.push_str(&text[last..m.range.start]);
            push_replacement(&mut out, replacement.as_str(), text, &m);
            last = m.range.end;
        }
        out.push_str(&text[last..]);
        return Ok(Value::String(RcStr::from(out)));
    }
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    Ok(Value::String(RcStr::from(
        s.replace(pattern.as_str(), replacement.as_str()),
    )))
}

/// Append the JS replacement pattern to `out`, substituting `$n`, `$<name>`,
/// `$&`, ``$` ``, `$'`, and `$$` from the match's captures.
fn push_replacement(out: &mut String, repl: &str, text: &str, m: &regress::Match) {
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let next = match chars.peek() {
            Some(&ch) => ch,
            None => {
                out.push('$');
                break;
            }
        };
        match next {
            '$' => {
                chars.next();
                out.push('$');
            }
            '&' => {
                chars.next();
                out.push_str(&text[m.range.clone()]);
            }
            '`' => {
                chars.next();
                out.push_str(&text[..m.range.start]);
            }
            '\'' => {
                chars.next();
                out.push_str(&text[m.range.end..]);
            }
            '0'..='9' => {
                chars.next();
                let mut n = (next as u32 - '0' as u32) as usize;
                while let Some(&c2) = chars.peek() {
                    if !c2.is_ascii_digit() {
                        break;
                    }
                    chars.next();
                    n = n
                        .saturating_mul(10)
                        .saturating_add((c2 as u32 - '0' as u32) as usize);
                }
                if n > 0 && n <= m.captures.len() {
                    if let Some(cap) = m.captures.get(n - 1) {
                        if let Some(range) = cap {
                            out.push_str(&text[range.clone()]);
                        }
                    }
                }
            }
            '<' => {
                // `$<name>` — named group reference. If unterminated, emit
                // the consumed text literally (matching JS leniency).
                chars.next(); // consume '<'
                let mut name = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '>' {
                        closed = true;
                        break;
                    }
                    name.push(c2);
                }
                if closed {
                    if let Some(range) = m.named_group(&name) {
                        out.push_str(&text[range]);
                    }
                } else {
                    out.push_str("$<");
                    out.push_str(&name);
                }
            }
            _ => {
                out.push('$');
            }
        }
    }
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
                "matchAll must be called with a global RegExp",
            ));
        }
    };
    if !rx.flags.contains('g') {
        return Err(vm.fail(
            ErrorKind::TypeError,
            "matchAll must be called with a global RegExp",
        ));
    }
    let matches: Vec<regress::Match> = rx.compiled.find_iter(s.as_str()).collect();
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
        let text = s.as_str();
        if rx.flags.contains('g') {
            let matches: ThinVec<Value> = rx
                .compiled
                .find_iter(text)
                .map(|m| Value::String(RcStr::from(&text[m.range])))
                .collect();
            if matches.is_empty() {
                return Ok(Value::Null);
            }
            return Ok(vm.alloc_array(matches));
        } else {
            // Non-global: same result shape as exec().
            let m = match rx.compiled.find(text) {
                Some(m) => m,
                None => return Ok(Value::Null),
            };
            let obj = build_exec_result(vm, &m, s.clone());
            return Ok(vm.alloc_object(obj));
        }
    }
    // String pattern: treat as a literal (not a RegExp).
    let pattern = vm.to_js_string(args.get(vm, 1), 0);
    let pat = pattern.as_str();
    if pat.is_empty() {
        // Empty string: return [""] (JS: empty string matches at start of string).
        return Ok(vm.alloc_array(thin_vec::thin_vec![Value::String(RcStr::from(""))]));
    }
    if let Some(idx) = s.find(pat) {
        return Ok(
            vm.alloc_array(thin_vec::thin_vec![Value::String(RcStr::from(
                &s[idx..idx + pat.len()]
            ))]),
        );
    }
    Ok(Value::Null)
}

/// `s.search(pattern)` — pattern may be a string or RegExp.
/// Returns the index of the first match, or -1 if not found.
pub fn str_search(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx: i64 = if let Some(rx) = try_reg_exp(vm, args.get(vm, 1)) {
        rx.compiled
            .find(s.as_str())
            .map(|m| m.range.start as i64)
            .unwrap_or(-1)
    } else {
        let pattern = vm.to_js_string(args.get(vm, 1), 0);
        s.find(pattern.as_str()).map(|i| i as i64).unwrap_or(-1)
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
    Ok(Value::String(RcStr::from(s.to_lowercase())))
}

/// `s.toUpperCase()` → uppercase string.
pub fn str_to_upper_case(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::String(RcStr::from(s.to_uppercase())))
}

/// `s.padStart(targetLength[, padString])` → padded string.
pub fn str_pad_start(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let len = args.get(vm, 1);
    let target_len = len
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))? as usize;
    let pad: RcStr = match args.get(vm, 2) {
        Value::Undefined => RcStr::from(" "),
        v => vm.to_js_string(v, 0),
    };
    if s.len() >= target_len || pad.is_empty() {
        return Ok(Value::String(s));
    }
    let needed = target_len - s.len();
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut out = String::with_capacity(target_len);
    for i in 0..needed {
        out.push(pad_chars[i % pad_chars.len()]);
    }
    out.push_str(&s);
    Ok(Value::String(RcStr::from(out)))
}

/// `s.padEnd(targetLength[, padString])` → padded string.
pub fn str_pad_end(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let len = args.get(vm, 1);
    let target_len = len
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))? as usize;
    let pad: RcStr = match args.get(vm, 2) {
        Value::Undefined => RcStr::from(" "),
        v => vm.to_js_string(v, 0),
    };
    if s.len() >= target_len || pad.is_empty() {
        return Ok(Value::String(s));
    }
    let needed = target_len - s.len();
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut out = String::with_capacity(target_len);
    out.push_str(&s);
    for i in 0..needed {
        out.push(pad_chars[i % pad_chars.len()]);
    }
    Ok(Value::String(RcStr::from(out)))
}

/// `s.repeat(count)` → repeated string. Negative counts → ValueError.
pub fn str_repeat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let count = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    if count < 0.0 || count.is_infinite() {
        return Err(vm.fail(ErrorKind::ValueError, "value error"));
    }
    // Bound the allocation, but raise loudly instead of silently truncating
    // (a silent cap produces a wrong-length string with no signal).
    const REPEAT_MAX: usize = 1_000_000;
    let n = count as usize;
    if n > REPEAT_MAX {
        return Err(vm.fail(
            ErrorKind::ValueError,
            "repeat count too large (max 1000000)",
        ));
    }
    Ok(Value::String(RcStr::from(s.as_str().repeat(n))))
}

/// `s.trimStart()` → left-trimmed string.
pub fn str_trim_start(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::String(RcStr::from(s.trim_start())))
}

/// `s.trimEnd()` → right-trimmed string.
pub fn str_trim_end(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    Ok(Value::String(RcStr::from(s.trim_end())))
}

/// `s.charAt(index)` → single character (UTF-8 byte range) or empty string.
pub fn str_char_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))? as i64;
    if idx < 0 || idx as usize >= s.len() {
        return Ok(Value::String(RcStr::from("")));
    }
    let byte = s.as_bytes()[idx as usize];
    // Return the single byte as a char (charAt is per-byte in our string model)
    Ok(Value::String(RcStr::from((byte as char).to_string())))
}

/// `s.at(index)` → character at index (negative counts from end), or undefined.
pub fn str_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = args.string_receiver(vm)?;
    let idx = args
        .get(vm, 1)
        .to_number()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let len = s.len() as i64;
    let i = if idx < 0.0 {
        idx as i64 + len
    } else {
        idx as i64
    };
    if i < 0 || i as usize >= s.len() {
        return Ok(Value::Undefined);
    }
    let byte = s.as_bytes()[i as usize];
    Ok(Value::String(RcStr::from((byte as char).to_string())))
}

/// `s.concat(str1, str2, …)` → concatenated string. Receiver must be a string.
pub fn str_concat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    // Receiver must be a string (the getter defers an Object receiver); the
    // *arguments* are coerced, matching JS `concat`.
    let mut out = args.str_receiver(vm)?.to_string();
    for i in 1..args.argc {
        let piece = vm.to_js_string(args.get(vm, i), 0);
        out.push_str(piece.as_str());
    }
    Ok(Value::String(RcStr::from(out)))
}

/// `String.fromCharCode(c1, c2, …)` → string from character codes.
/// Each argument is truncated to a 16-bit value (expects UTF-16 code units).
pub fn str_from_char_code(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut out = String::with_capacity(args.argc * 4);
    for i in 0..args.argc {
        let n = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        let code = (n as u32) & 0xFFFF;
        if let Some(c) = char::from_u32(code) {
            out.push(c);
        }
    }
    Ok(Value::String(RcStr::from(out)))
}

/// `String.fromCodePoint(c1, c2, …)` → string from Unicode code points.
/// Each argument must be a valid code point (0..=0x10FFFF, excluding surrogates).
pub fn str_from_code_point(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let mut out = String::with_capacity(args.argc * 4);
    for i in 0..args.argc {
        let n = args
            .get(vm, i)
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        let code = n as u32;
        if code > 0x10FFFF || (0xD800..=0xDFFF).contains(&code) {
            return Err(vm.fail(ErrorKind::ValueError, "value error"));
        }
        if let Some(c) = char::from_u32(code) {
            out.push(c);
        }
    }
    Ok(Value::String(RcStr::from(out)))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Clamp a byte offset into `[0, s.len()]` and round it up to the next UTF-8
/// char boundary, so it can always be used as a slice start. Used to apply JS's
/// "start position" arguments (which clamp rather than error) on our byte-string
/// representation.
fn clamp_start(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Clamp a byte offset into `[0, s.len()]` and round it down to the previous
/// UTF-8 char boundary, so it can safely be used as an end-of-slice boundary.
fn clamp_end(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn extract_substring(vm: &mut VM, s: &str, start: usize, end: usize) -> Result<Value, VMError> {
    if start >= end {
        return Ok(Value::String(RcStr::from("")));
    }

    let start_clamped = clamp_start(s, start.min(s.len()));
    let end_clamped = clamp_end(s, end.min(s.len()));

    if start_clamped < start || end_clamped > end {
        return Err(vm.fail(ErrorKind::ValueError, "value error"));
    }

    Ok(Value::String(RcStr::from(&s[start_clamped..end_clamped])))
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
            Value::String(s) => assert_eq!(s.as_str(), "ell"),
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
            Value::String(s) => assert_eq!(s.as_str(), "llo"),
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
            Value::String(s) => assert_eq!(s.as_str(), "hi"),
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
        // Negative → ValueError (RangeError in JS).
        assert_eq!(
            testutil::run_err_kind("return 'ab'.repeat(-1);"),
            ErrorKind::ValueError
        );
        // Over the cap → loud ValueError, not a silently-truncated string.
        assert_eq!(
            testutil::run_err_kind("return 'x'.repeat(2000000);"),
            ErrorKind::ValueError
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
        // 'a,b'.split(',', 2, 3) still reaches a builtin — argc=4 > max=3 falls
        // through to dynamic dispatch, errors at runtime.
        let err = testutil::run_runtime_err("'a,b'.split(',', 2, 3);");
        assert_eq!(err.kind, ErrorKind::TypeError);

        // 'abc'.split() — acr=1 < min=2 falls through to dynamic dispatch,
        // errors at runtime.
        let err = testutil::run_runtime_err("'abc'.split();");
        assert_eq!(err.kind, ErrorKind::TypeError);

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
            Value::String(s) => assert_eq!(s.as_str(), "hello"),
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
}

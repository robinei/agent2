use indexmap::IndexMap;

use crate::builtin::Args;
use crate::builtin::regexp::try_reg_exp;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};

// ── Edit.replaceOnce ──────────────────────────────────────────────────────────

/// `Edit.replaceOnce(text, old, new)` → string.
/// Replace `old` (string or RegExp) with `new` iff `old` matches exactly once
/// in `text`. Errors with the actual match count on ambiguity.
pub fn edit_replace_once(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let replacement = vm.to_js_string(args.get(vm, 2), 0);
    let old_val = args.get(vm, 1);

    if let Some(rx) = try_reg_exp(vm, old_val) {
        let matches: Vec<_> = rx.compiled.find_iter(text).collect();
        let n = matches.len();
        if n != 1 {
            return Err(vm.fail(
                ErrorKind::ValueError,
                format!("replaceOnce expected 1 match, found {n}"),
            ));
        }
        let m = &matches[0];
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..m.range.start]);
        out.push_str(replacement.as_str());
        out.push_str(&text[m.range.end..]);
        Ok(Value::String(RcStr::from(out)))
    } else {
        let old_s = vm.to_js_string(old_val, 0);
        let old = old_s.as_str();
        if old.is_empty() {
            return Err(vm.fail(
                ErrorKind::ValueError,
                "replaceOnce: empty pattern is not supported",
            ));
        }
        let indices: Vec<_> = text.match_indices(old).collect();
        let n = indices.len();
        if n != 1 {
            return Err(vm.fail(
                ErrorKind::ValueError,
                format!("replaceOnce expected 1 match, found {n}"),
            ));
        }
        let (pos, _) = indices[0];
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..pos]);
        out.push_str(replacement.as_str());
        out.push_str(&text[pos + old.len()..]);
        Ok(Value::String(RcStr::from(out)))
    }
}

// ── Edit.replaceCount ─────────────────────────────────────────────────────────

/// `Edit.replaceCount(text, old, new)` → `{ result, count }`.
/// Replace every occurrence of `old` (string or RegExp) with `new` and
/// return the result string plus the match count.
pub fn edit_replace_count(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let replacement = vm.to_js_string(args.get(vm, 2), 0);
    let old_val = args.get(vm, 1);

    if let Some(rx) = try_reg_exp(vm, old_val) {
        let mut out = String::new();
        let mut last = 0;
        let mut count: u64 = 0;
        for m in rx.compiled.find_iter(text) {
            out.push_str(&text[last..m.range.start]);
            out.push_str(replacement.as_str());
            last = m.range.end;
            count += 1;
        }
        out.push_str(&text[last..]);
        return obj_result_count(vm, &out, count);
    }

    let old_s = vm.to_js_string(old_val, 0);
    let old = old_s.as_str();
    if old.is_empty() {
        return Err(vm.fail(
            ErrorKind::ValueError,
            "replaceCount: empty pattern is not supported",
        ));
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut count: u64 = 0;
    for (pos, _) in text.match_indices(old) {
        out.push_str(&text[last..pos]);
        out.push_str(replacement.as_str());
        last = pos + old.len();
        count += 1;
    }
    out.push_str(&text[last..]);
    obj_result_count(vm, &out, count)
}

fn obj_result_count(vm: &mut VM, result: &str, count: u64) -> Result<Value, VMError> {
    let mut obj = IndexMap::new();
    obj.insert(RcStr::from("result"), Value::String(RcStr::from(result)));
    obj.insert(RcStr::from("count"), Value::PosInt(count));
    Ok(vm.alloc_object(obj))
}

// ── Edit.count ────────────────────────────────────────────────────────────────

/// `Edit.count(text, needle)` → number.
/// Count non-overlapping occurrences of `needle` (string or RegExp) in `text`.
pub fn edit_count(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text = vm.str_from(args.get(vm, 0))?;
    let needle = args.get(vm, 1);

    let count = if let Some(rx) = try_reg_exp(vm, needle) {
        rx.compiled.find_iter(text).count() as u64
    } else {
        let needle_s = vm.to_js_string(needle, 0);
        if needle_s.is_empty() {
            return Err(vm.fail(
                ErrorKind::ValueError,
                "count: empty needle is not supported",
            ));
        }
        text.match_indices(needle_s.as_str()).count() as u64
    };
    Ok(Value::PosInt(count))
}

// ── Edit.extractBlock ─────────────────────────────────────────────────────────

/// `Edit.extractBlock(text, headIndex)` → `{ start, end }`.
/// From `headIndex` (byte offset), find the nearest `{` and balance braces
/// to return the block range (exclusive end).  Errors if no brace is found
/// or the braces are unbalanced.
pub fn edit_extract_block(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let head = as_non_neg_usize(vm, args.get(vm, 1), "headIndex")?;
    if head >= text.len() {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!("headIndex {head} is past end of text (len {})", text.len()),
        ));
    }
    let open = text[head..].find('{').map(|i| head + i).ok_or_else(|| {
        vm.fail(
            ErrorKind::ValueError,
            format!("no opening brace found at or after byte index {head}"),
        )
    })?;
    let close =
        balance_to(text, open, '{', '}').map_err(|msg| vm.fail(ErrorKind::ValueError, msg))?;
    build_range_obj(vm, open, close)
}

// ── Edit.extractByIndent ──────────────────────────────────────────────────────

/// `Edit.extractByIndent(text, lineIndex)` → `{ start, end }`.
/// Starting at 0-indexed `lineIndex`, collect lines with greater indentation
/// until a line with dedented (or same-level) indent.  Returns the byte
/// range of the block (exclusive end).  Blank lines within the block are
/// included and do not terminate it.
pub fn edit_extract_by_indent(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let line_idx = as_non_neg_usize(vm, args.get(vm, 1), "lineIndex")?;

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if line_idx >= lines.len() {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!(
                "lineIndex {line_idx} is out of range ({} lines)",
                lines.len()
            ),
        ));
    }

    let base_indent = indent_width(lines[line_idx]);
    let start: usize = lines[..line_idx].iter().map(|l| l.len()).sum();
    let mut end = start;

    // Include the anchor line.
    end += lines[line_idx].len();

    // Walk forward — skip blank lines, stop at dedent.
    let mut i = line_idx + 1;
    while i < lines.len() {
        let line = lines[i];
        if is_blank_line(line) {
            end += line.len();
            i += 1;
            continue;
        }
        let ind = indent_width(line);
        if ind <= base_indent {
            break;
        }
        end += line.len();
        i += 1;
    }

    build_range_obj(vm, start, end)
}

fn indent_width(line: &str) -> usize {
    line.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

fn is_blank_line(line: &str) -> bool {
    line.trim() == "" || line.trim() == "\n" || line == "\n"
}

// ── Edit.extractEnclosing ─────────────────────────────────────────────────────

/// `Edit.extractEnclosing(text, index, open, close)` → `{ start, end }`.
/// Find the innermost pair of `open`/`close` characters that enclose
/// `index` (byte offset).  `open` and `close` must be single characters.
/// Errors if no enclosing pair is found.
pub fn edit_extract_enclosing(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let idx = as_non_neg_usize(vm, args.get(vm, 1), "index")?;

    let open = args.get(vm, 2);
    let close_val = args.get(vm, 3);
    let open_ch = char_from_val(vm, open, "open")?;
    let close_ch = char_from_val(vm, close_val, "close")?;

    if idx >= text.len() {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!("index {idx} is past end of text (len {})", text.len()),
        ));
    }

    let pairs = balanced_pairs(text, open_ch, close_ch);
    let best = pairs
        .iter()
        .filter(|(s, e)| *s < idx && idx < *e)
        .min_by_key(|(s, e)| e - s)
        .ok_or_else(|| {
            vm.fail(
                ErrorKind::ValueError,
                format!("no enclosing `{open_ch}`…`{close_ch}` pair around byte index {idx}"),
            )
        })?;

    build_range_obj(vm, best.0, best.1)
}

fn char_from_val(vm: &VM, val: &Value, label: &str) -> Result<char, VMError> {
    let s = vm
        .str_from(val)
        .map_err(|_| vm.fail(ErrorKind::ValueError, format!("{label} must be a string")))?;
    if s.len() != 1 {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!(
                "{label} must be a single character, got {len} chars",
                len = s.len()
            ),
        ));
    }
    Ok(s.chars().next().unwrap())
}

// ── brace / delimiter helpers ─────────────────────────────────────────────────

/// Balance from `open_pos` (inclusive) to the matching `close`. Returns
/// the exclusive end position (one past the closing char). Skips
/// single/double-quoted strings and `\\` escapes to avoid false positives.
fn balance_to(text: &str, open_pos: usize, open_ch: char, close_ch: char) -> Result<usize, String> {
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut i = open_pos;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                i = skip_quoted(bytes, i)?;
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i = skip_line_comment(bytes, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = skip_block_comment(bytes, i)?;
                continue;
            }
            _ => {}
        }

        let ch = b as char;
        if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            depth -= 1;
            if depth == 0 {
                return Ok(i + 1); // exclusive end
            }
        }
        i += 1;
    }

    Err(format!(
        "unbalanced `{open_ch}`…`{close_ch}` starting at byte index {open_pos}"
    ))
}

/// Find all balanced `(open, close)` pairs, recording their (start, end) with
/// exclusive end. Skips JS string literals and comments.
fn balanced_pairs(text: &str, open_ch: char, close_ch: char) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut pairs = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                i = match skip_quoted(bytes, i) {
                    Ok(pos) => pos,
                    Err(_) => i + 1,
                };
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i = skip_line_comment(bytes, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = match skip_block_comment(bytes, i) {
                    Ok(pos) => pos,
                    Err(_) => i + 1,
                };
                continue;
            }
            _ => {}
        }

        let ch = b as char;
        if ch == open_ch {
            stack.push(i);
        } else if ch == close_ch {
            if let Some(start) = stack.pop() {
                pairs.push((start, i + 1)); // exclusive end
            }
        }
        i += 1;
    }

    pairs
}

fn skip_quoted(bytes: &[u8], start: usize) -> Result<usize, String> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return Ok(i + 1);
        }
        i += 1;
    }
    Err("unterminated string literal".into())
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut i = start + 2;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return Ok(i + 2);
        }
        i += 1;
    }
    Err("unterminated block comment".into())
}

// ── Edit.replaceLines ─────────────────────────────────────────────────────────

/// `Edit.replaceLines(text, start, end, newText)` → string.
/// Replace 1-indexed lines `start` through `end` (inclusive) with `newText`.
/// Errors on an invalid or reversed range.
pub fn edit_replace_lines(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let start = as_non_neg_usize(vm, args.get(vm, 1), "start")?;
    let end = as_non_neg_usize(vm, args.get(vm, 2), "end")?;
    let new_text = vm.to_js_string(args.get(vm, 3), 0);

    if start < 1 || end < 1 || start > end {
        return Err(vm.fail(ErrorKind::ValueError, format!(
            "replaceLines: invalid range [{start}, {end}] — use 1-indexed inclusive, start ≤ end"
        )));
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if end > lines.len() {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!("replaceLines: end {end} exceeds line count {}", lines.len()),
        ));
    }

    let prefix_len: usize = lines[..(start - 1)].iter().map(|l| l.len()).sum();
    let suffix_start: usize = lines[..end].iter().map(|l| l.len()).sum();

    let mut out = String::with_capacity(prefix_len + new_text.len() + (text.len() - suffix_start));
    out.push_str(&text[..prefix_len]);
    out.push_str(new_text.as_str());
    out.push_str(&text[suffix_start..]);
    Ok(Value::String(RcStr::from(out)))
}

// ── Edit.insertAt ─────────────────────────────────────────────────────────────

/// `Edit.insertAt(text, lineNo, newText)` → string.
/// Insert `newText` before 1-indexed `lineNo`.  `lineNo` may be one past
/// the last line to append (a trailing newline is added if needed).
/// Errors on out-of-range.
pub fn edit_insert_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();
    let line_no = as_non_neg_usize(vm, args.get(vm, 1), "lineNo")?;
    let new_text = vm.to_js_string(args.get(vm, 2), 0);

    if line_no < 1 {
        return Err(vm.fail(ErrorKind::ValueError, "insertAt: lineNo must be >= 1"));
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let max_line = if text.is_empty() {
        1 // empty text → lineNo 1 is valid (one-past-last)
    } else {
        lines.len() + 1 // one past last is valid for append
    };

    if line_no > max_line {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!("insertAt: lineNo {line_no} is out of range (max {max_line})"),
        ));
    }

    if line_no == max_line {
        // Append.
        let mut out = String::with_capacity(text.len() + new_text.len() + 1);
        out.push_str(text);
        if !text.is_empty() && !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(new_text.as_str());
        return Ok(Value::String(RcStr::from(out)));
    }

    let insert_pos: usize = lines[..(line_no - 1)].iter().map(|l| l.len()).sum();
    let mut out = String::with_capacity(text.len() + new_text.len());
    out.push_str(&text[..insert_pos]);
    out.push_str(new_text.as_str());
    out.push_str(&text[insert_pos..]);
    Ok(Value::String(RcStr::from(out)))
}

// ── Edit.applyEdits ───────────────────────────────────────────────────────────

/// `Edit.applyEdits(text, edits)` → string.
/// Apply a list of `[{ old, new }, …]` string replacements atomically.
/// Each `old` must appear exactly once; all spans must be disjoint.
/// Edits are applied right-to-left so offsets stay stable.  Errors with
/// the index and content of the offending edit on ambiguity.
pub fn edit_apply_edits(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = vm.string_from(args.get(vm, 0))?;
    let text = text_s.as_str();

    let edits_val = args.get(vm, 1);
    let edits = parse_edits(vm, edits_val)?;

    let mut spans: Vec<(usize, usize, usize, &RcStr)> = Vec::new();
    // (edit_idx, match_start, match_end_excl, replacement)

    for (i, (old_s, new_s)) in edits.iter().enumerate() {
        let old = old_s.as_str();
        if old.is_empty() {
            return Err(vm.fail(
                ErrorKind::ValueError,
                format!("applyEdits: edit[{i}].old is empty"),
            ));
        }
        let matches: Vec<usize> = text.match_indices(old).map(|(p, _)| p).collect();
        if matches.len() != 1 {
            return Err(vm.fail(
                ErrorKind::ValueError,
                format!(
                    "applyEdits: edit[{i}] old={old:?} expected 1 match, found {}",
                    matches.len()
                ),
            ));
        }
        spans.push((i, matches[0], matches[0] + old.len(), new_s));
    }

    // Sort by position descending for right-to-left application.
    spans.sort_by(|a, b| b.1.cmp(&a.1));

    // Check disjointness: spans sorted by start descending, so
    // w[1].end must be <= w[0].start.
    for w in spans.windows(2) {
        let (i1, s1, _, _) = w[0];
        let (i2, _, e2, _) = w[1];
        if e2 > s1 {
            return Err(vm.fail(
                ErrorKind::ValueError,
                format!(
                    "applyEdits: overlapping edits at indices {i2} and {i1} — \
                     spans must be disjoint"
                ),
            ));
        }
    }

    // Apply right-to-left (already sorted descending).
    let mut result = String::from(text);
    for (_, start, end, replacement) in &spans {
        let r = replacement.as_str();
        result.replace_range(*start..*end, r);
    }

    Ok(Value::String(RcStr::from(result)))
}

/// Parse the edits arg: an array of `{ old, new }` objects.
fn parse_edits<'a>(vm: &'a VM, val: &'a Value) -> Result<Vec<(RcStr, RcStr)>, VMError> {
    let arr_ptr = match val {
        Value::Array(p) => *p as usize,
        _ => {
            return Err(vm.fail(
                ErrorKind::ValueError,
                "applyEdits: edits must be an array of { old, new } objects",
            ));
        }
    };
    let arr = vm
        .arrays
        .get(arr_ptr)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "applyEdits: bad array pointer"))?;

    let mut out = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let obj_ptr = match elem {
            Value::Object(p) => *p as usize,
            _ => {
                return Err(vm.fail(
                    ErrorKind::ValueError,
                    format!(
                        "applyEdits: edits[{i}] must be an object {{ old, new }}, got {elem:?}"
                    ),
                ));
            }
        };
        let obj = vm
            .objects
            .get(obj_ptr)
            .ok_or_else(|| vm.fail(ErrorKind::ValueError, "applyEdits: bad object pointer"))?;

        let old = obj
            .get("old")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                vm.fail(
                    ErrorKind::ValueError,
                    format!("applyEdits: edits[{i}].old must be a string"),
                )
            })?;
        let new = obj
            .get("new")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                vm.fail(
                    ErrorKind::ValueError,
                    format!("applyEdits: edits[{i}].new must be a string"),
                )
            })?;
        out.push((old, new));
    }
    Ok(out)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn build_range_obj(vm: &mut VM, start: usize, end: usize) -> Result<Value, VMError> {
    let mut obj = IndexMap::new();
    obj.insert(RcStr::from("start"), Value::PosInt(start as u64));
    obj.insert(RcStr::from("end"), Value::PosInt(end as u64));
    Ok(vm.alloc_object(obj))
}

/// Extract a non-negative integer (as `usize`) from a `Value`, using
/// `as_f64()` for the number accessor and rejecting negatives.
fn as_non_neg_usize(vm: &VM, val: &Value, label: &str) -> Result<usize, VMError> {
    let n = val
        .as_f64()
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, format!("{label} must be a number")))?;
    if n < 0.0 || n.fract() != 0.0 {
        return Err(vm.fail(
            ErrorKind::ValueError,
            format!("{label} must be a non-negative integer, got {n}"),
        ));
    }
    Ok(n as usize)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::testutil::{self};
    use crate::vm::ErrorKind;
    use serde_json::json;

    // ── replaceOnce ─────────────────────────────────────────────────────

    #[test]
    fn replace_once_success() {
        let out = testutil::run_ret("return Edit.replaceOnce('hello world', 'world', 'earth');");
        assert_eq!(out, json!("hello earth"));
    }

    #[test]
    fn replace_once_zero_matches_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceOnce('hello', 'x', 'y');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    #[test]
    fn replace_once_multiple_matches_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceOnce('xaxbx', 'x', 'y');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    #[test]
    fn replace_once_with_regexp() {
        let out = testutil::run_ret("return Edit.replaceOnce('abc 123 def', /\\d+/, 'num');");
        assert_eq!(out, json!("abc num def"));
    }

    #[test]
    fn replace_once_regexp_multiple_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceOnce('a1 b2 c3', /\\d/, 'num');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── replaceCount ────────────────────────────────────────────────────

    #[test]
    fn replace_count_success() {
        let out = testutil::run_ret("return Edit.replaceCount('xaxbx', 'x', 'y');");
        assert_eq!(out, json!({"result": "yayby", "count": 3}));
    }

    #[test]
    fn replace_count_zero_returns_same() {
        let out = testutil::run_ret("return Edit.replaceCount('hello', 'x', 'y');");
        assert_eq!(out, json!({"result": "hello", "count": 0}));
    }

    #[test]
    fn replace_count_with_regexp() {
        let out = testutil::run_ret("return Edit.replaceCount('a1 b2 c3', /\\d/g, 'X');");
        assert_eq!(out, json!({"result": "aX bX cX", "count": 3}));
    }

    // ── count ───────────────────────────────────────────────────────────

    #[test]
    fn count_success() {
        let out = testutil::run_ret("return Edit.count('xaxbx', 'x');");
        assert_eq!(out, json!(3));
    }

    #[test]
    fn count_zero() {
        let out = testutil::run_ret("return Edit.count('hello', 'z');");
        assert_eq!(out, json!(0));
    }

    #[test]
    fn count_with_regexp() {
        let out = testutil::run_ret("return Edit.count('a1 b2 c3', /\\d+/g);");
        assert_eq!(out, json!(3));
    }

    // ── extractBlock ────────────────────────────────────────────────────

    #[test]
    fn extract_block_success() {
        let out = testutil::run_ret(
            "const code = 'function f() {\\n  return 1;\\n}\\n';
             return Edit.extractBlock(code, 0);",
        );
        let s = out["start"].as_u64().unwrap();
        let e = out["end"].as_u64().unwrap();
        assert!(s < e, "got start={s} end={e}");
    }

    #[test]
    fn extract_block_no_brace_errors() {
        let kind = testutil::run_err_kind("return Edit.extractBlock('hello', 0);");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    #[test]
    fn extract_block_unbalanced_errors() {
        let kind = testutil::run_err_kind("return Edit.extractBlock('{ open', 0);");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── extractByIndent ─────────────────────────────────────────────────

    #[test]
    fn extract_by_indent_success() {
        let out = testutil::run_ret(
            "const code = 'def f():\\n  a = 1\\n  b = 2\\nx = 3\\n';
             return Edit.extractByIndent(code, 1);",
        );
        let s = out["start"].as_u64().unwrap();
        let e = out["end"].as_u64().unwrap();
        // The block should cover lines 1 and 2 (the indented body)
        assert!(s < e, "got start={s} end={e}");
    }

    #[test]
    fn extract_by_indent_out_of_range_errors() {
        let kind = testutil::run_err_kind("return Edit.extractByIndent('a\\nb\\n', 5);");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── extractEnclosing ────────────────────────────────────────────────

    #[test]
    fn extract_enclosing_success() {
        let out = testutil::run_ret(
            "const code = 'foo(bar(baz), qux)';
             return Edit.extractEnclosing(code, 10, '(', ')');",
        );
        let s = out["start"].as_u64().unwrap();
        let e = out["end"].as_u64().unwrap();
        assert!(s < e, "got start={s} end={e}");
    }

    #[test]
    fn extract_enclosing_no_pair_errors() {
        let kind = testutil::run_err_kind("return Edit.extractEnclosing('hello', 2, '{', '}');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── replaceLines ────────────────────────────────────────────────────

    #[test]
    fn replace_lines_success() {
        let out = testutil::run_ret("return Edit.replaceLines('a\\nb\\nc\\n', 2, 2, 'B\\nB2');");
        // Literal replacement: "b\n" becomes "B\nB2", so "a\nB\nB2c\n"
        assert_eq!(out, json!("a\nB\nB2c\n"));
    }

    #[test]
    fn replace_lines_invalid_range_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceLines('a\\nb\\n', 3, 2, 'x');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    #[test]
    fn replace_lines_out_of_range_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceLines('a\\nb\\n', 99, 99, 'x');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── insertAt ────────────────────────────────────────────────────────

    #[test]
    fn insert_at_beginning() {
        let out = testutil::run_ret("return Edit.insertAt('b\\nc\\n', 1, 'a\\n');");
        assert_eq!(out, json!("a\nb\nc\n"));
    }

    #[test]
    fn insert_at_middle() {
        let out = testutil::run_ret("return Edit.insertAt('a\\nc\\n', 2, 'b\\n');");
        assert_eq!(out, json!("a\nb\nc\n"));
    }

    #[test]
    fn insert_at_end() {
        let out = testutil::run_ret("return Edit.insertAt('a\\nb\\n', 3, 'c\\n');");
        assert_eq!(out, json!("a\nb\nc\n"));
    }

    #[test]
    fn insert_at_out_of_range_errors() {
        let kind = testutil::run_err_kind("return Edit.insertAt('a\\nb\\n', 5, 'x');");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── applyEdits ──────────────────────────────────────────────────────

    #[test]
    fn apply_edits_success() {
        let out = testutil::run_ret(
            "return Edit.applyEdits('hello cruel world', [{ old: 'cruel ', new: '' }]);",
        );
        assert_eq!(out, json!("hello world"));
    }

    #[test]
    fn apply_edits_multiple() {
        let out = testutil::run_ret(
            "return Edit.applyEdits('first second third', \
             [{ old: 'first ', new: '1-' }, { old: 'second ', new: '2-' }, { old: 'third', new: '3' }]);",
        );
        assert_eq!(out, json!("1-2-3"));
    }

    #[test]
    fn apply_edits_ambiguous_errors() {
        let kind =
            testutil::run_err_kind("return Edit.applyEdits('x x', [{ old: 'x', new: 'y' }]);");
        assert_eq!(kind, ErrorKind::ValueError);
    }

    #[test]
    fn apply_edits_overlapping_errors() {
        let kind = testutil::run_err_kind(
            "return Edit.applyEdits('hello world', \
             [{ old: 'hello', new: 'hi' }, { old: 'ello', new: 'i' }]);",
        );
        assert_eq!(kind, ErrorKind::ValueError);
    }

    // ── arity / lint parity ─────────────────────────────────────────────

    #[test]
    fn edit_replace_once_too_few_args() {
        let errs = testutil::compile_errs("Edit.replaceOnce();");
        let msg = errs.join("\n");
        assert!(msg.contains("replaceOnce"), "expected arity error: {msg}");
    }

    #[test]
    fn edit_count_too_few_args() {
        let errs = testutil::compile_errs("Edit.count('x');");
        let msg = errs.join("\n");
        assert!(msg.contains("count"), "expected arity error: {msg}");
    }

    #[test]
    fn edit_replace_once_catchable() {
        // A ValueError from replaceOnce can be caught (resumable, not a raise).
        let out = testutil::run_ret(
            "try { return Edit.replaceOnce('hello', 'x', 'y'); } catch (e) { return 'caught'; }",
        );
        assert_eq!(out, json!("caught"));
    }

    #[test]
    fn edit_apply_edits_error_is_catchable() {
        let out = testutil::run_ret(
            "try { return Edit.applyEdits('x x', [{ old: 'x', new: 'y' }]); } \
             catch (e) { return 'caught mismatch'; }",
        );
        assert_eq!(out, json!("caught mismatch"));
    }
}

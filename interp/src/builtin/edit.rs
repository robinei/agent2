//! `Edit.*` — the text-surgery verbs, and the three names their failures
//! carry.
//!
//! **These are not JS functions, but their errors land in a JS `catch`.**
//! A program branches on `e.name`, so every refusal here has to pick one
//! of the nine standard classes; there is no house class to fall back on,
//! and inventing one was the mistake this replaced. The rule, in the order
//! a call meets it:
//!
//! - **`TypeError`** — an argument is the wrong type. `edits` is not an
//!   array, `edits[3].old` is not a string, `open` is not a string,
//!   `lineNo` is not a number. Nothing about the *text* matters yet.
//! - **`SyntaxError`** — the text is not well-formed as the source it was
//!   handed to `extractBlock` as: unbalanced braces, an unterminated
//!   string literal, an unterminated block comment. This is the one name
//!   that changes the repair. A `RangeError` invites another index; a
//!   `SyntaxError` says no index will work, because the brace scanner ran
//!   off the end of a file that is truncated or is not C-family source.
//! - **`RangeError`** — everything else: the arguments are the right
//!   types and the call names a place or a shape the text does not have.
//!   An index past the end, `start > end`, a `lineNo` below 1, an empty
//!   pattern, a needle that matches three times instead of once, two
//!   edits whose spans overlap, an edit that would indent a line twice
//!   over.
//!
//! The last group is the widest, and deliberately so: `TypeError` must not
//! become the bucket every refusal falls into, because a program that
//! catches one has been told something false — the argument's type was
//! fine. The spec's own precedent for "right type, unusable value" is
//! `String.prototype.normalize("nope")`, which is a `RangeError` for a
//! string argument whose *value* is not one the operation accepts.

use indexmap::IndexMap;

use crate::builtin::Args;
use crate::builtin::regexp::try_reg_exp;
use crate::js_string::keys;
use crate::vm::{ErrorKind, JsString, VM, VMError, Value};

// ── Edit.replaceOnce ──────────────────────────────────────────────────────────

/// The `text` every `Edit.*` takes first, with an error that names the
/// mistake instead of its symptom.
///
/// `vm.string_from` says only "type error", and the value that reaches
/// it is usually `undefined` produced two lines earlier. Eight traps
/// across the runs of 2026-09-18 said `in \`replaceOnce\`: type error`
/// while naming a call that was not the error, which is the least
/// useful thing a message can do.
///
/// Every `Edit.*` returns the new text now, so the commonest source of
/// that `undefined` is gone with it: a `.result` taken off a verb that
/// never had one. What is left is an ordinary mistake — the wrong
/// variable, or the object a tool returned instead of the string
/// inside it — and both are worth naming precisely.
fn edit_text(vm: &mut VM, args: &Args, who: &str) -> Result<String, VMError> {
    let v = args.get(vm, 0).clone();
    if matches!(v, Value::Undefined) {
        let msg = format!(
            "{who}(text, …): `text` is undefined. Every `Edit.*` takes the text and \
             returns the new text, so this is a value that was never a string — a \
             misspelled variable, or a property the object does not have. The call that \
             produced it is the one to look at, not this one."
        );
        return Err(vm.fail(ErrorKind::TypeError, msg));
    }
    // **And the same mistake the other way up.** The message above
    // catches a `.result` taken off a function that does not have one;
    // this catches the `.result` that was never taken. Live on
    // 2026-09-20, a run wrote
    //
    //   const withSkips = Edit.replaceAll(text, old, "");
    //   Edit.replaceOnce(withSkips, …);
    //
    // and got `in `replaceOnce`: type error` — the whole message —
    // while the object it was handed was sitting there announcing what
    // it was. That cost the run its task.
    if let Value::Object(p) = &v {
        let keys: Vec<String> = vm
            .objects
            .get(*p as usize)
            .map(|o| o.map.keys().map(|k| k.to_string()).collect())
            .unwrap_or_default();
        if !keys.is_empty() {
            // **Name the way out, not just the fault.** This used to be
            // specific to the `{ result, count }` object `replaceCount`
            // returned, which no longer exists — but the shape it
            // caught does: a tool's result handed over whole where the
            // string inside it was meant. `read_file` gives
            // `{ content, version }`, and `Edit.replaceOnce(f, …)` is
            // the same mistake with a different object.
            let msg = format!(
                "{who}(text, …): `text` is an object, not a string — it has {}. Pass the \
                 property holding the text, not the object.",
                keys.iter()
                    .map(|k| format!("`.{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(vm.fail(ErrorKind::TypeError, msg));
        }
    }
    // Anything else wrong: say which argument and what arrived.
    //
    // **The receiver narrows here and the 1,500 lines below stay on UTF-8.**
    // `Edit.*` is built on byte offsets throughout — `lines_of`,
    // `brace_range_from`, `enclosing_pair`, the comment/string scanners that
    // walk `as_bytes()`. Rewriting all of it over code units would be ~500
    // lines of risk for text that is ASCII in every observed run. Instead the
    // conversion happens at the two edges where an offset is *visible to the
    // program* — `unit_to_byte` on the way in, `byte_to_unit` on the way out —
    // and for ASCII both are the identity.
    Ok(vm.string_arg(&v, Some("text"))?.to_utf8_lossy())
}

/// A code-unit offset as a byte offset into `text`.
///
/// **The `Edit.*` boundary, one of two.** An index a program computed with
/// `indexOf` counts code units; the body below counts bytes. Identity for
/// ASCII, which is what this operates on in every observed run. An offset
/// that falls between the halves of a surrogate pair resolves to the start of
/// that character — there is no byte there to point at.
fn unit_to_byte(text: &str, target: usize) -> usize {
    if text.is_ascii() {
        return target.min(text.len());
    }
    let mut units = 0;
    for (b, c) in text.char_indices() {
        if units >= target {
            return b;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// A byte offset as a code-unit offset into `text`. See [`unit_to_byte`].
fn byte_to_unit(text: &str, target: usize) -> usize {
    if text.is_ascii() {
        return target.min(text.len());
    }
    let mut units = 0;
    for (b, c) in text.char_indices() {
        if b >= target {
            return units;
        }
        units += c.len_utf16();
    }
    units
}

/// `Edit.replaceOnce(text, old, new)` → string.
/// Replace `old` (string or RegExp) with `new` iff `old` matches exactly once
/// in `text`. Errors with the actual match count on ambiguity.
/// What a failed `replaceOnce` should say.
///
/// "expected 1 match, found 4" names the problem and not the remedy,
/// and a trap is only affordable when it teaches — a program pays one
/// round trip for it either way, so the message is where the round trip
/// either buys something or does not. Observed 2026-09-16: two of three
/// eval runs trapped here, on `#[allow(dead_code)]`, which occurs four
/// times in one file. Widening the needle to include the line beneath
/// it is the fix, and nothing said so.
fn ambiguous(n: usize, needle: &str, lines: &[usize]) -> String {
    match_count_error("replaceOnce", n, needle, lines)
}

/// Whether an edit would leave a line indented twice over.
///
/// **The shape, exactly:** the match starts partway into its line,
/// everything before it on that line is whitespace, and the
/// replacement begins with that same whitespace. Applying it writes
/// `pre + pre + …`, so a four-space line becomes an eight-space one and
/// the file stops parsing.
///
/// Live twice on 2026-09-20, both on `skipped-tests`, both deleting a
/// decorator:
///
/// ```text
/// old: '@unittest.skip("…")\n    def test_base_rate(self):'
/// new: '    def test_base_rate(self):'
/// ```
///
/// The `@` is four spaces into its line and `old` does not include
/// them, so the edit takes the decorator and leaves its indentation
/// sitting in front of a `def` that brought its own. One run reported
/// the mistake itself — "my earlier edit removed the decorator without
/// its leading indentation" — a whole reply after the fact.
///
/// Refusing rather than warning, because the card promises these verbs
/// "throw rather than landing somewhere you did not mean", and because
/// a program that *wants* doubled indentation writes it in `new`
/// against an `old` that starts at the line's beginning.
/// **And the other half: `new` that ends the line instead of starting
/// it.** The check above wanted `new` to begin with the same
/// indentation, which is the case where the two collide. A `new` that
/// is *empty* — deleting the decorator outright — slips past it, and
/// the indentation is orphaned rather than doubled: it sits at the
/// head of the line with the next line's content pulled up behind it.
/// Same corruption, one branch further on.
///
/// Live on 2026-09-20 at current HEAD, `skipped-tests` again:
///
/// ```text
/// old: '@unittest.skip("rates were in flux")\n'
/// new: ''
/// ```
///
/// against `    @unittest.skip("rates were in flux")\n    def
/// test_base_rate(self):`. The four spaces before the `@` survived and
/// the `def` kept its own, so the method was defined eight columns in
/// and its body was no longer indented relative to it —
/// `IndentationError: expected an indented block after function
/// definition on line 7`. The run read the failure, told the person
/// the suite was broken, and called `finish(text)`.
fn doubles_indentation(text: &str, start: usize, old: &str, new: &str) -> Option<String> {
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let pre = &text[line_start..start];
    if pre.is_empty() || !pre.chars().all(|c| c == ' ' || c == '\t') {
        return None;
    }
    if new.starts_with(pre) {
        return Some(format!(
            "the match starts {} column(s) into its line, after indentation that `old` does not \
             include, and `new` begins with that same indentation — applying it would leave the \
             line indented twice over. Put the leading whitespace in `old` as well, or take it \
             off the front of `new`.",
            pre.len()
        ));
    }
    // The line is being ended here — by a `new` that closes it, or by
    // one that is not there at all — so nothing follows `pre` on it and
    // the next line's content comes up behind that indentation.
    if old.ends_with('\n') && (new.is_empty() || new.ends_with('\n')) {
        return Some(format!(
            "the match starts {} column(s) into its line, after indentation that `old` does not \
             include, and `old` ends the line — so that indentation would be left with nothing \
             on the line and the next line pulled up behind it. Put the leading whitespace in \
             `old` as well, so the whole line goes.",
            pre.len()
        ));
    }
    None
}

/// The 1-based line each byte offset falls on.
///
/// **Where the matches are is the half the advice was missing.**
/// "Widen it with the surrounding text" tells a program what to do and
/// not where to do it, so the next program re-reads the file and hunts
/// for occurrences the failing call had already found. The offsets are
/// in hand at the moment of the error; spending them is free, and
/// `Edit.replaceLines(text, n, n, …)` takes a line number directly.
fn lines_of(text: &str, offsets: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut starts: Vec<usize> = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    offsets
        .map(|off| starts.partition_point(|&s| s <= off))
        .collect()
}

/// The same message for every needle-based edit, because the two ways
/// to miss have different remedies and one message cannot carry both.
///
/// **Found several** — widen it. That was the 2026-09-16 case.
///
/// **Found none** — the needle was *composed* rather than copied.
/// Observed 2026-09-17 across four `applyEdits` traps, three of them
/// zero-match: `fn trim()` where the source reads `fn trim(s: &str) ->
/// &str {`, and an `@unittest.skip` line reconstructed with the wrong
/// indentation. The text was written from memory of what the file
/// probably says. `applyEdits` had one message for both counts and so
/// told a program to widen a needle that was not there at all.
fn match_count_error(what: &str, n: usize, needle: &str, lines: &[usize]) -> String {
    let mut shown: String = needle.chars().take(50).collect();
    if shown.len() < needle.len() {
        shown.push('…');
    }
    if n == 0 {
        format!(
            "{what} found no match for `{shown}` — it has to appear exactly as written, \
             whitespace and all. Copy it out of the content you read rather than writing \
             out what you expect to be there."
        )
    } else {
        // Four is enough to see the shape of the repetition; a
        // hundred-match needle would otherwise push the rest of the
        // report out of the way to say the same thing.
        let mut where_ = lines
            .iter()
            .take(4)
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if lines.len() > 4 {
            where_.push_str(", …");
        }
        let at = if lines.is_empty() {
            String::new()
        } else {
            format!(
                ", at line{} {where_}",
                if lines.len() == 1 { "" } else { "s" }
            )
        };
        format!(
            "{what} expected 1 match, found {n} of `{shown}`{at} — widen it with the \
             surrounding text (the line above or below) until it names one place, or name \
             the line you mean with `Edit.replaceLines`"
        )
    }
}

pub fn edit_replace_once(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = edit_text(vm, &args, "replaceOnce")?;
    let text = text_s.as_str();
    let replacement = vm.to_js_string(args.get(vm, 2), 0).to_utf8_lossy();
    let old_val = args.get(vm, 1);

    if let Some(rx) = try_reg_exp(vm, old_val) {
        let matches: Vec<_> = rx.compiled.find_iter(text).collect();
        let n = matches.len();
        if n != 1 {
            let shown = format!("{:?}", rx.compiled);
            let lines = lines_of(text, matches.iter().map(|m| m.range.start));
            return Err(vm.fail(ErrorKind::RangeError, ambiguous(n, &shown, &lines)));
        }
        let m = &matches[0];
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..m.range.start]);
        out.push_str(replacement.as_str());
        out.push_str(&text[m.range.end..]);
        Ok(Value::String(JsString::from(out)))
    } else {
        let old_s = vm.to_js_string(old_val, 0).to_utf8_lossy();
        let old = old_s.as_str();
        if old.is_empty() {
            return Err(vm.fail(
                ErrorKind::RangeError,
                "replaceOnce: empty pattern is not supported",
            ));
        }
        let indices: Vec<_> = text.match_indices(old).collect();
        let n = indices.len();
        if n != 1 {
            let lines = lines_of(text, indices.iter().map(|(i, _)| *i));
            return Err(vm.fail(ErrorKind::RangeError, ambiguous(n, old, &lines)));
        }
        let (pos, _) = indices[0];
        if let Some(why) = doubles_indentation(text, pos, old, replacement.as_str()) {
            return Err(vm.fail(
                ErrorKind::RangeError,
                format!("replaceOnce: {why}").as_str(),
            ));
        }
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..pos]);
        out.push_str(replacement.as_str());
        out.push_str(&text[pos + old.len()..]);
        Ok(Value::String(JsString::from(out)))
    }
}

// ── Edit.replaceAll ───────────────────────────────────────────────────────────

/// `Edit.replaceAll(text, old, new)` → string.
/// Replace every occurrence of `old` (string or RegExp) with `new`.
///
/// **Was `replaceCount`, returning `{ result, count }`** — the one verb
/// in the family that did not hand back the text, which cost six traps
/// across five runs in the kept corpus: a `.result` taken off a sibling
/// that never had one, and the `undefined` surfacing at the *next*
/// call. The count it bundled is `Edit.count(text, old)`, which counts
/// the same non-overlapping occurrences and was always there. The name
/// follows JavaScript's own `String.prototype.replaceAll`, which
/// returns a string, so the shape is the one already expected.
pub fn edit_replace_all(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = edit_text(vm, &args, "replaceAll")?;
    let text = text_s.as_str();
    let replacement = vm.to_js_string(args.get(vm, 2), 0).to_utf8_lossy();
    let old_val = args.get(vm, 1);

    if let Some(rx) = try_reg_exp(vm, old_val) {
        let mut out = String::new();
        let mut last = 0;
        for m in rx.compiled.find_iter(text) {
            out.push_str(&text[last..m.range.start]);
            out.push_str(replacement.as_str());
            last = m.range.end;
        }
        out.push_str(&text[last..]);
        return Ok(Value::String(JsString::from(out.as_str())));
    }

    let old_s = vm.to_js_string(old_val, 0).to_utf8_lossy();
    let old = old_s.as_str();
    if old.is_empty() {
        return Err(vm.fail(
            ErrorKind::RangeError,
            "replaceAll: empty pattern is not supported",
        ));
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (pos, _) in text.match_indices(old) {
        // **Every match, because `replaceAll` applies to every
        // match.** Its siblings check one site; this one had no check
        // at all, and it is the verb a program reaches for to strip a
        // decorator from several methods at once — which is exactly
        // the edit that orphans indentation, once per method.
        if let Some(why) = doubles_indentation(text, pos, old, replacement.as_str()) {
            return Err(vm.fail(ErrorKind::RangeError, format!("replaceAll: {why}").as_str()));
        }
        out.push_str(&text[last..pos]);
        out.push_str(replacement.as_str());
        last = pos + old.len();
    }
    out.push_str(&text[last..]);
    Ok(Value::String(JsString::from(out.as_str())))
}

// ── Edit.count ────────────────────────────────────────────────────────────────

/// `Edit.count(text, needle)` → number.
/// Count non-overlapping occurrences of `needle` (string or RegExp) in `text`.
pub fn edit_count(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text = vm.str_from(args.get(vm, 0))?;
    let needle = args.get(vm, 1);

    let count = if let Some(rx) = try_reg_exp(vm, needle) {
        crate::builtin::regexp::find_all(rx, text).len() as u64
    } else {
        let needle_s = vm.to_js_string(needle, 0);
        let needle_u = needle_s.as_units();
        if needle_u.is_empty() {
            return Err(vm.fail(
                ErrorKind::RangeError,
                "count: empty needle is not supported",
            ));
        }
        let mut n = 0u64;
        let mut at = 0;
        while let Some(i) = crate::units::find(text, needle_u, at) {
            n += 1;
            at = i + needle_u.len();
        }
        n
    };
    Ok(Value::PosInt(count))
}

// ── Edit.extractBlock ─────────────────────────────────────────────────────────

/// `Edit.extractBlock(text, headIndex)` → `{ start, end }`.
/// From `headIndex` (byte offset), find the nearest `{` and balance braces
/// to return the block range (exclusive end).  Errors if no brace is found
/// or the braces are unbalanced.
pub fn edit_extract_block(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = edit_text(vm, &args, "extractBlock")?;
    let text = text_s.as_str();
    let head_unit = as_non_neg_usize(vm, args.get(vm, 1), "headIndex")?;
    let text_units = byte_to_unit(text, text.len());
    if head_unit >= text_units {
        return Err(vm.fail(
            ErrorKind::RangeError,
            format!("headIndex {head_unit} is past end of text (len {text_units})"),
        ));
    }
    let head = unit_to_byte(text, head_unit);
    let open = text[head..].find('{').map(|i| head + i).ok_or_else(|| {
        vm.fail(
            ErrorKind::RangeError,
            format!("no opening brace found at or after index {head_unit}"),
        )
    })?;
    let close =
        balance_to(text, open, '{', '}').map_err(|msg| vm.fail(ErrorKind::SyntaxError, msg))?;
    build_range_obj(vm, text, open, close)
}

// ── Edit.extractByIndent ──────────────────────────────────────────────────────

/// `Edit.extractByIndent(text, lineIndex)` → `{ start, end }`.
/// Starting at 0-indexed `lineIndex`, collect lines with greater indentation
/// until a line with dedented (or same-level) indent.  Returns the byte
/// range of the block (exclusive end).  Blank lines within the block are
/// included and do not terminate it.
pub fn edit_extract_by_indent(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = edit_text(vm, &args, "extractByIndent")?;
    let text = text_s.as_str();
    let line_idx = as_non_neg_usize(vm, args.get(vm, 1), "lineIndex")?;

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if line_idx >= lines.len() {
        return Err(vm.fail(
            ErrorKind::RangeError,
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

    build_range_obj(vm, text, start, end)
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
    let text_s = edit_text(vm, &args, "extractEnclosing")?;
    let text = text_s.as_str();
    let idx_unit = as_non_neg_usize(vm, args.get(vm, 1), "index")?;

    let open = args.get(vm, 2);
    let close_val = args.get(vm, 3);
    let open_ch = char_from_val(vm, open, "open")?;
    let close_ch = char_from_val(vm, close_val, "close")?;

    let text_units = byte_to_unit(text, text.len());
    if idx_unit >= text_units {
        return Err(vm.fail(
            ErrorKind::RangeError,
            format!("index {idx_unit} is past end of text (len {text_units})"),
        ));
    }
    let idx = unit_to_byte(text, idx_unit);

    let pairs = balanced_pairs(text, open_ch, close_ch);
    let best = pairs
        .iter()
        .filter(|(s, e)| *s < idx && idx < *e)
        .min_by_key(|(s, e)| e - s)
        .ok_or_else(|| {
            vm.fail(
                ErrorKind::RangeError,
                format!(
                    "no enclosing `{open_ch}`…`{close_ch}` pair around index {idx_unit}{}",
                    block_hint(text, idx, open_ch, close_ch)
                ),
            )
        })?;

    build_range_obj(vm, text, best.0, best.1)
}

/// The sentence that names `extractBlock`, when the index the caller
/// passed looks like a definition *head* rather than a byte inside the
/// span they want.
///
/// **The two verbs are one letter apart in meaning and the wrong one is
/// the plausible pick.** A live run held `i = text.indexOf("fn compact(")`,
/// wanted that function's body, and wrote
/// `Edit.extractEnclosing(text, i, "{", "}")`. The call failed
/// correctly — `fn compact(` is at module level, and that byte is
/// inside its parameter list, enclosed by nothing — and the message
/// said only that, so the run had no way to get from a true statement
/// to the verb it wanted, `extractBlock(text, headIndex)`.
///
/// Naming the other verb in the error rather than in the card is
/// deliberate: it reaches exactly the run that guessed wrong, at the
/// moment it guessed, and costs nothing in the prompt every other run
/// pays for. Card prose measured here has twice made the behaviour it
/// named *worse*.
///
/// Only for braces, and only when there is a `{` at or after `idx` —
/// that is precisely the condition under which `extractBlock(text, idx)`
/// would have found something, so the suggestion is never a guess.
fn block_hint(text: &str, idx: usize, open_ch: char, close_ch: char) -> String {
    if open_ch != '{' || close_ch != '}' || !text[idx..].contains('{') {
        return String::new();
    }
    format!(
        " — that byte is not inside any pair. If it is the start of a \
         definition head (from `indexOf(\"fn name(\")`), the block you \
         want is `Edit.extractBlock(text, {idx})`: the block whose head \
         starts there, rather than one around it."
    )
}

fn char_from_val(vm: &VM, val: &Value, label: &str) -> Result<char, VMError> {
    let s = vm
        .str_from(val)
        .map_err(|_| vm.fail(ErrorKind::TypeError, format!("{label} must be a string")))?;
    // One *code point*: `open`/`close` are delimiters like `(` or `{`, and a
    // surrogate pair is one character even though it is two units.
    let mut cps = crate::units::code_points(s);
    let first = cps.next();
    if first.is_none() || cps.next().is_some() {
        return Err(vm.fail(
            ErrorKind::RangeError,
            format!(
                "{label} must be a single character, got {len} chars",
                len = crate::units::code_points(s).count()
            ),
        ));
    }
    let (cp, _) = crate::units::code_point_at(first.expect("checked above"), 0)
        .expect("a code point slice is non-empty");
    char::from_u32(cp).ok_or_else(|| {
        vm.fail(
            ErrorKind::RangeError,
            format!("{label} must be a character, not an unpaired surrogate"),
        )
    })
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
        "unbalanced `{open_ch}`…`{close_ch}` starting at index {open_pos}"
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
        } else if ch == close_ch
            && let Some(start) = stack.pop()
        {
            pairs.push((start, i + 1)); // exclusive end
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
    let text_s = edit_text(vm, &args, "replaceLines")?;
    let text = text_s.as_str();
    let start = as_non_neg_usize(vm, args.get(vm, 1), "start")?;
    let end = as_non_neg_usize(vm, args.get(vm, 2), "end")?;
    let new_text = vm.to_js_string(args.get(vm, 3), 0).to_utf8_lossy();

    if start < 1 || end < 1 || start > end {
        return Err(vm.fail(ErrorKind::RangeError, format!(
            "replaceLines: invalid range [{start}, {end}] — use 1-indexed inclusive, start ≤ end"
        )));
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if end > lines.len() {
        return Err(vm.fail(
            ErrorKind::RangeError,
            format!("replaceLines: end {end} exceeds line count {}", lines.len()),
        ));
    }

    let prefix_len: usize = lines[..(start - 1)].iter().map(|l| l.len()).sum();
    let suffix_start: usize = lines[..end].iter().map(|l| l.len()).sum();

    let mut out = String::with_capacity(prefix_len + new_text.len() + (text.len() - suffix_start));
    out.push_str(&text[..prefix_len]);
    out.push_str(new_text.as_str());
    out.push_str(&text[suffix_start..]);
    Ok(Value::String(JsString::from(out)))
}

// ── Edit.insertAt ─────────────────────────────────────────────────────────────

/// `Edit.insertAt(text, lineNo, newText)` → string.
/// Insert `newText` before 1-indexed `lineNo`.  `lineNo` may be one past
/// the last line to append (a trailing newline is added if needed).
/// Errors on out-of-range.
pub fn edit_insert_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = edit_text(vm, &args, "insertAt")?;
    let text = text_s.as_str();
    let line_no = as_non_neg_usize(vm, args.get(vm, 1), "lineNo")?;
    let new_text = vm.to_js_string(args.get(vm, 2), 0).to_utf8_lossy();

    if line_no < 1 {
        return Err(vm.fail(ErrorKind::RangeError, "insertAt: lineNo must be >= 1"));
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let max_line = if text.is_empty() {
        1 // empty text → lineNo 1 is valid (one-past-last)
    } else {
        lines.len() + 1 // one past last is valid for append
    };

    if line_no > max_line {
        return Err(vm.fail(
            ErrorKind::RangeError,
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
        return Ok(Value::String(JsString::from(out)));
    }

    let insert_pos: usize = lines[..(line_no - 1)].iter().map(|l| l.len()).sum();
    let mut out = String::with_capacity(text.len() + new_text.len());
    out.push_str(&text[..insert_pos]);
    out.push_str(new_text.as_str());
    out.push_str(&text[insert_pos..]);
    Ok(Value::String(JsString::from(out)))
}

// ── Edit.applyEdits ───────────────────────────────────────────────────────────

/// `Edit.applyEdits(text, edits)` → string.
/// Apply a list of `[{ old, new }, …]` string replacements atomically.
/// Each `old` must appear exactly once; all spans must be disjoint.
/// Edits are applied right-to-left so offsets stay stable.  Errors with
/// the index and content of the offending edit on ambiguity.
pub fn edit_apply_edits(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let text_s = edit_text(vm, &args, "applyEdits")?;
    let text = text_s.as_str();

    let edits_val = args.get(vm, 1);
    let edits = parse_edits(vm, edits_val)?;

    let mut spans: Vec<(usize, usize, usize, &String)> = Vec::new();
    // (edit_idx, match_start, match_end_excl, replacement)

    for (i, (old_s, new_s)) in edits.iter().enumerate() {
        let old = old_s.as_str();
        if old.is_empty() {
            return Err(vm.fail(
                ErrorKind::RangeError,
                format!("applyEdits: edit[{i}].old is empty"),
            ));
        }
        let matches: Vec<usize> = text.match_indices(old).map(|(p, _)| p).collect();
        if matches.len() != 1 {
            return Err(vm.fail(
                ErrorKind::RangeError,
                format!(
                    "applyEdits edit[{i}]: {}",
                    match_count_error(
                        "this edit's `old`",
                        matches.len(),
                        old,
                        &lines_of(text, matches.iter().copied())
                    )
                ),
            ));
        }
        if let Some(why) = doubles_indentation(text, matches[0], old, new_s) {
            return Err(vm.fail(
                ErrorKind::RangeError,
                format!("applyEdits edit[{i}]: {why}").as_str(),
            ));
        }
        spans.push((i, matches[0], matches[0] + old.len(), new_s));
    }

    // Sort by position descending for right-to-left application.
    spans.sort_by_key(|&(_, start, _, _)| std::cmp::Reverse(start));

    // Check disjointness: spans sorted by start descending, so
    // w[1].end must be <= w[0].start.
    for w in spans.windows(2) {
        let (i1, s1, _, _) = w[0];
        let (i2, _, e2, _) = w[1];
        if e2 > s1 {
            return Err(vm.fail(
                ErrorKind::RangeError,
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
        result.replace_range(*start..*end, replacement);
    }

    Ok(Value::String(JsString::from(result)))
}

/// Parse the edits arg: an array of `{ old, new }` objects.
///
/// Narrowed to UTF-8 here, like the receiver: `applyEdits` matches and splices
/// against `text`, which is a `String` from `edit_text`.
fn parse_edits<'a>(vm: &'a VM, val: &'a Value) -> Result<Vec<(String, String)>, VMError> {
    let arr_ptr = match val {
        Value::Array(p) => *p as usize,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "applyEdits: edits must be an array of { old, new } objects",
            ));
        }
    };
    let arr = vm
        .arrays
        .get(arr_ptr)
        .ok_or_else(|| vm.fail_invariant(ErrorKind::BadPointer, "applyEdits: bad array pointer"))?;

    let mut out = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let obj_ptr = match elem {
            Value::Object(p) => *p as usize,
            _ => {
                return Err(vm.fail(
                    ErrorKind::TypeError,
                    format!(
                        "applyEdits: edits[{i}] must be an object {{ old, new }}, got {elem:?}"
                    ),
                ));
            }
        };
        let obj = vm.objects.get(obj_ptr).ok_or_else(|| {
            vm.fail_invariant(ErrorKind::BadPointer, "applyEdits: bad object pointer")
        })?;

        let old = obj
            .map
            .get(keys::OLD)
            .and_then(|v| match v {
                Value::String(s) => Some(s.to_utf8_lossy()),
                _ => None,
            })
            .ok_or_else(|| {
                vm.fail(
                    ErrorKind::TypeError,
                    format!("applyEdits: edits[{i}].old must be a string"),
                )
            })?;
        let new = obj
            .map
            .get(keys::NEW)
            .and_then(|v| match v {
                Value::String(s) => Some(s.to_utf8_lossy()),
                _ => None,
            })
            .ok_or_else(|| {
                vm.fail(
                    ErrorKind::TypeError,
                    format!("applyEdits: edits[{i}].new must be a string"),
                )
            })?;
        out.push((old, new));
    }
    Ok(out)
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// `{ start, end }` — the other `Edit.*` boundary. `start` and `end` arrive
/// as byte offsets from the body and leave as the code-unit offsets the
/// program's own `indexOf`/`slice` speak.
fn build_range_obj(vm: &mut VM, text: &str, start: usize, end: usize) -> Result<Value, VMError> {
    let mut obj = IndexMap::new();
    obj.insert(
        JsString::from("start"),
        Value::PosInt(byte_to_unit(text, start) as u64),
    );
    obj.insert(
        JsString::from("end"),
        Value::PosInt(byte_to_unit(text, end) as u64),
    );
    Ok(vm.alloc_object(obj))
}

/// Extract a non-negative integer (as `usize`) from a `Value`, using
/// `as_f64()` for the number accessor and rejecting negatives.
fn as_non_neg_usize(vm: &VM, val: &Value, label: &str) -> Result<usize, VMError> {
    let n = val
        .as_f64()
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, format!("{label} must be a number")))?;
    if n < 0.0 || n.fract() != 0.0 {
        return Err(vm.fail(
            ErrorKind::RangeError,
            format!("{label} must be a non-negative integer, got {n}"),
        ));
    }
    Ok(n as usize)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod match_count_tests {
    use super::{lines_of, match_count_error};

    /// **The two ways to miss have different remedies**, and one
    /// message cannot carry both. Three of four `applyEdits` traps on
    /// 2026-09-17 were zero-match — a needle composed from memory
    /// rather than copied — and the message told the program to widen
    /// something that was not in the file at all.
    #[test]
    fn a_missing_needle_and_an_ambiguous_one_advise_differently() {
        let none = match_count_error("replaceOnce", 0, "fn trim()", &[]);
        assert!(none.contains("found no match"), "{none}");
        assert!(
            none.contains("Copy it out of the content you read"),
            "{none}"
        );
        assert!(
            !none.contains("widen"),
            "wrong remedy for a missing needle: {none}"
        );

        let many = match_count_error("replaceOnce", 4, "#[allow(dead_code)]", &[3, 11, 19, 27]);
        assert!(many.contains("found 4"), "{many}");
        assert!(many.contains("widen it"), "{many}");
        assert!(
            !many.contains("Copy it out"),
            "wrong remedy for an ambiguous one: {many}"
        );
        // **And where they are.** "Widen it" says what to do and not
        // where; the offsets were in hand at the moment of the error,
        // so the next program can name a line instead of re-reading the
        // file to find what this call had already found.
        assert!(many.contains("at lines 3, 11, 19, 27"), "{many}");
        assert!(
            many.contains("replaceLines"),
            "and the verb that takes one: {many}"
        );

        // Five or more is still four and an ellipsis — one trap must
        // not crowd out the report around it.
        let lots = match_count_error("replaceOnce", 9, "x", &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert!(lots.contains("at lines 1, 2, 3, 4, …"), "{lots}");
    }

    /// Byte offsets become 1-based line numbers, including the first
    /// line, which has no newline before it to count.
    #[test]
    fn offsets_become_line_numbers() {
        let text = "aa\nbb\ncc\n";
        assert_eq!(lines_of(text, [0usize, 3, 6].into_iter()), vec![1, 2, 3]);
        assert_eq!(lines_of(text, [1usize].into_iter()), vec![1]);
    }

    /// **An object where the text was meant names its own keys.** This
    /// used to be specific to the `{ result, count }` that
    /// `replaceCount` returned; every `Edit.*` hands back the text
    /// now, so the object that turns up here is a tool's result passed
    /// whole — `read_file`'s `{ content, version }` is the common one —
    /// and the message says which property to reach for.
    #[test]
    fn an_object_where_the_text_was_meant_names_its_keys() {
        let forgot = crate::testutil::run_runtime_err(
            "const f = { content: 'aa', version: 'v1' }; Edit.replaceOnce(f, 'a', 'b');",
        );
        assert_eq!(forgot.kind, crate::ErrorKind::TypeError);
        assert!(
            forgot.message.contains("`.content`") && forgot.message.contains("`.version`"),
            "names what it was handed: {}",
            forgot.message
        );
        assert!(
            forgot
                .message
                .contains("Pass the property holding the text"),
            "and what to write: {}",
            forgot.message
        );

        let too_far = crate::testutil::run_runtime_err(
            "const s = Edit.replaceOnce('ab', 'a', 'x'); Edit.replaceOnce(s.result, 'x', 'y');",
        );
        assert!(
            too_far.message.contains("is undefined"),
            "the mirror image still says its own thing: {}",
            too_far.message
        );
    }

    /// **The indentation the needle left behind.** Live twice on
    /// 2026-09-20, both deleting a decorator whose leading spaces the
    /// needle did not include, both producing an `IndentationError`
    /// the run then reported as a success.
    #[test]
    fn an_edit_that_would_double_an_indent_is_refused() {
        let src = "class T:\n    @skip(\"x\")\n    def t(self):\n        pass\n";
        let err = crate::testutil::run_runtime_err(&format!(
            "Edit.replaceOnce({src:?}, '@skip(\"x\")\\n    def t(self):', '    def t(self):');"
        ));
        assert_eq!(err.kind, crate::ErrorKind::RangeError);
        assert!(
            err.message.contains("indented twice over"),
            "says what would happen: {}",
            err.message
        );
        assert!(
            err.message.contains("Put the leading whitespace in `old`"),
            "and what to write: {}",
            err.message
        );

        // Including the indentation in `old` is the fix, and works.
        let out = crate::testutil::run_ret(&format!(
            "return Edit.replaceOnce({src:?}, '    @skip(\"x\")\\n    def t(self):', '    def t(self):');"
        ));
        assert_eq!(
            out,
            serde_json::json!("class T:\n    def t(self):\n        pass\n")
        );

        // And an ordinary mid-line replacement is untouched: what
        // precedes the match is not whitespace, so nothing doubles.
        let out = crate::testutil::run_ret("return Edit.replaceOnce('let a = b;', 'b', '  c');");
        assert_eq!(out, serde_json::json!("let a =   c;"));
    }

    /// Long needles are clipped so one trap cannot dominate a report.
    #[test]
    fn a_long_needle_is_clipped() {
        let msg = match_count_error("replaceOnce", 0, &"x".repeat(400), &[]);
        assert!(msg.contains('…'), "{msg}");
        assert!(msg.len() < 300, "{} bytes", msg.len());
    }
}

#[cfg(test)]
mod orphaned_indentation_tests {
    use crate::testutil;

    fn msg(src: &str) -> String {
        testutil::run_ret(&format!(
            "try {{ {src} }} catch (e) {{ return e.message; }}"
        ))
        .as_str()
        .unwrap_or_default()
        .to_owned()
    }

    /// **The edit that broke a file at current HEAD.** Deleting a
    /// decorator with a needle that starts after its indentation leaves
    /// that indentation on a line of its own, and the next line comes
    /// up behind it: the method ends up defined eight columns in with
    /// a body no longer indented relative to it. Python calls that
    /// `IndentationError: expected an indented block`; the run that
    /// wrote it read the failure, said the suite was broken and
    /// stopped.
    ///
    /// The sibling check wanted `new` to *begin* with the indentation,
    /// so an empty `new` slipped past it.
    #[test]
    fn deleting_a_line_without_its_indentation_is_refused() {
        let file = r#"'class T:\n    @skip("x")\n    def a(self):\n        pass\n'"#;
        for call in [
            format!(r#"Edit.replaceAll({file}, '@skip("x")\n', "")"#),
            format!(r#"Edit.replaceOnce({file}, '@skip("x")\n', "")"#),
            format!(r#"Edit.applyEdits({file}, [{{ old: '@skip("x")\n', new: "" }}])"#),
        ] {
            let m = msg(&call);
            assert!(
                m.contains("ends the line") && m.contains("pulled up behind it"),
                "{call}\n  got: {m}"
            );
            assert!(m.contains("4 column(s)"), "names the column: {m}");
        }
    }

    /// Written with the indentation included, it is an ordinary edit.
    #[test]
    fn deleting_the_whole_line_is_fine() {
        let out = testutil::run_ret(
            r#"return Edit.replaceAll('class T:\n    @skip("x")\n    def a(self):\n', '    @skip("x")\n', "");"#,
        );
        assert_eq!(out.as_str().unwrap(), "class T:\n    def a(self):\n");
    }

    /// And a replacement that keeps the line is untouched by either
    /// branch — the check is about a line being *ended*, not about
    /// every mid-line match.
    #[test]
    fn a_mid_line_replacement_that_keeps_the_line_is_allowed() {
        let out = testutil::run_ret(
            r#"return Edit.replaceOnce('class T:\n    foo = 1\n', 'foo = 1', 'bar = 2');"#,
        );
        assert_eq!(out.as_str().unwrap(), "class T:\n    bar = 2\n");
    }
}

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

    /// The message names the mistake, not the call that tripped over
    /// it: a `.result` on a verb that returns the text is `undefined`,
    /// and the *next* `Edit` call is where it surfaces.
    #[test]
    fn undefined_text_points_at_the_call_that_produced_it() {
        let e = testutil::run_runtime_err(
            "return Edit.replaceOnce(Edit.replaceOnce('a', 'a', 'b').result, 'x', 'y');",
        )
        .message;
        assert!(e.contains("`text` is undefined"), "{e}");
        assert!(
            e.contains("returns the new text"),
            "says what the family does, so the `.result` is obviously wrong: {e}"
        );
        assert!(
            e.contains("not this one"),
            "and points back at the call that produced it: {e}"
        );
    }

    #[test]
    fn replace_once_zero_matches_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceOnce('hello', 'x', 'y');");
        assert_eq!(kind, ErrorKind::RangeError);
    }

    #[test]
    fn replace_once_multiple_matches_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceOnce('xaxbx', 'x', 'y');");
        assert_eq!(kind, ErrorKind::RangeError);
    }

    #[test]
    fn replace_once_with_regexp() {
        let out = testutil::run_ret("return Edit.replaceOnce('abc 123 def', /\\d+/, 'num');");
        assert_eq!(out, json!("abc num def"));
    }

    #[test]
    fn replace_once_regexp_multiple_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceOnce('a1 b2 c3', /\\d/, 'num');");
        assert_eq!(kind, ErrorKind::RangeError);
    }

    // ── replaceAll ──────────────────────────────────────────────────────

    /// The text, like every other `Edit.*`. The count it used to bundle
    /// is `Edit.count`, over the same non-overlapping matches.
    #[test]
    fn replace_all_returns_the_text() {
        let out = testutil::run_ret("return Edit.replaceAll('xaxbx', 'x', 'y');");
        assert_eq!(out, json!("yayby"));
        let n = testutil::run_ret("return Edit.count('xaxbx', 'x');");
        assert_eq!(n, json!(3), "the number is a separate question");
    }

    #[test]
    fn replace_all_zero_returns_same() {
        let out = testutil::run_ret("return Edit.replaceAll('hello', 'x', 'y');");
        assert_eq!(out, json!("hello"));
    }

    #[test]
    fn replace_all_with_regexp() {
        let out = testutil::run_ret("return Edit.replaceAll('a1 b2 c3', /\\d/g, 'X');");
        assert_eq!(out, json!("aX bX cX"));
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
        assert_eq!(kind, ErrorKind::RangeError);
    }

    #[test]
    fn extract_block_unbalanced_errors() {
        let kind = testutil::run_err_kind("return Edit.extractBlock('{ open', 0);");
        assert_eq!(kind, ErrorKind::SyntaxError);
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
        assert_eq!(kind, ErrorKind::RangeError);
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
        assert_eq!(kind, ErrorKind::RangeError);
    }

    /// **The error names the verb the caller wanted.** A live run held
    /// `i = text.indexOf("fn compact(")`, wanted the function body, and
    /// wrote `Edit.extractEnclosing(text, i, "{", "}")`. It failed
    /// correctly — that byte is inside the parameter list of a
    /// module-level `fn`, enclosed by nothing — and the message said
    /// only that. A true statement the reader cannot act on is how two
    /// turns get spent; `extractBlock(text, headIndex)` was one line
    /// away.
    #[test]
    fn extract_enclosing_on_a_definition_head_names_extract_block() {
        let err = testutil::run_runtime_err(
            "const text = 'mod m {}\\nfn compact(a: u32) -> u32 { a }\\n';
             return Edit.extractEnclosing(text, text.indexOf('fn compact('), '{', '}');",
        );
        assert_eq!(err.kind, ErrorKind::RangeError);
        assert!(
            err.message.contains("Edit.extractBlock(text, "),
            "the message does not name the verb that works: {}",
            err.message
        );

        // The suggestion is not a guess: with no `{` left in the text,
        // `extractBlock` would fail too, so it is not offered.
        let bare = testutil::run_runtime_err("return Edit.extractEnclosing('hello', 2, '{', '}');");
        assert!(
            !bare.message.contains("extractBlock"),
            "offered a verb that would also fail: {}",
            bare.message
        );

        // And it is about braces. `(`…`)` has no `extractBlock`.
        let parens =
            testutil::run_runtime_err("return Edit.extractEnclosing('a { b } c', 0, '(', ')');");
        assert!(
            !parens.message.contains("extractBlock"),
            "extractBlock balances braces only: {}",
            parens.message
        );
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
        assert_eq!(kind, ErrorKind::RangeError);
    }

    #[test]
    fn replace_lines_out_of_range_errors() {
        let kind = testutil::run_err_kind("return Edit.replaceLines('a\\nb\\n', 99, 99, 'x');");
        assert_eq!(kind, ErrorKind::RangeError);
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
        assert_eq!(kind, ErrorKind::RangeError);
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
        assert_eq!(kind, ErrorKind::RangeError);
    }

    #[test]
    fn apply_edits_overlapping_errors() {
        let kind = testutil::run_err_kind(
            "return Edit.applyEdits('hello world', \
             [{ old: 'hello', new: 'hi' }, { old: 'ello', new: 'i' }]);",
        );
        assert_eq!(kind, ErrorKind::RangeError);
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

    /// The message has to name the remedy, not only the count — see
    /// `ambiguous`'s own doc for the runs that argued for it.
    #[test]
    fn replace_once_ambiguity_says_how_to_disambiguate() {
        let err = testutil::run_runtime_err(
            "return Edit.replaceOnce('#[a]\\nfn x\\n#[a]\\nfn y', '#[a]', '');",
        );
        assert!(err.message.contains("found 2"), "{}", err.message);
        assert!(
            err.message.contains("widen") && err.message.contains("surrounding"),
            "the trap has to teach: {}",
            err.message
        );
    }

    #[test]
    fn replace_once_absence_says_it_must_match_exactly() {
        let err = testutil::run_runtime_err("return Edit.replaceOnce('abc', 'zz', '');");
        assert!(err.message.contains("no match"), "{}", err.message);
        assert!(err.message.contains("whitespace"), "{}", err.message);
    }
}

#[cfg(test)]
mod arg_messages {
    /// **Every `Edit.*` diagnoses the mistake, not its symptom.**
    ///
    /// A run on 2026-09-21 wrote
    /// `const { result } = Edit.replaceLines(content, …)` — `.result`
    /// taken off a function that returns the string itself — and got
    /// `in \`replaceLines\`: type error` for its trouble. The helper
    /// that explains exactly that existed already and only three of the
    /// nine builtins used it.
    #[test]
    fn every_edit_builtin_explains_an_undefined_text() {
        for (src, who) in [
            ("Edit.replaceLines(undefined, 1, 2, \"x\");", "replaceLines"),
            ("Edit.insertAt(undefined, 1, \"x\");", "insertAt"),
            ("Edit.extractBlock(undefined, 1);", "extractBlock"),
            ("Edit.replaceOnce(undefined, \"a\", \"b\");", "replaceOnce"),
            ("Edit.applyEdits(undefined, []);", "applyEdits"),
        ] {
            let e = crate::testutil::run_runtime_err(src);
            assert!(
                e.message.contains("returns the new text"),
                "{who} should name the mistake, said: {}",
                e.message
            );
        }
    }

    /// A wrong *type* still says which argument and what arrived — the
    /// generic path, for everything that is not `undefined`.
    #[test]
    fn a_wrong_type_says_which_argument_and_what() {
        let e = crate::testutil::run_runtime_err("Edit.insertAt(42, 1, \"x\");");
        assert!(e.message.contains("text must be a string"), "{}", e.message);
        assert!(e.message.contains("number"), "{}", e.message);

        // The range errors beside it were already right; they stay.
        let e = crate::testutil::run_runtime_err("Edit.replaceLines(\"a\\nb\", 3, 1, \"x\");");
        assert!(e.message.contains("invalid range [3, 1]"), "{}", e.message);
    }
}

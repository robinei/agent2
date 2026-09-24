//! Real tools (8_HARNESS M1, 10_EDITING): file read, HTTP fetch, bash,
//! create_file, replace_file.  Handlers run blocking on the session
//! loop's worker threads.
//!
//! Size-guard tiers (10_EDITING Step 2):
//! - Program-facing artifacts get full bytes with MB-scale OOM ceilings.
//! - The LLM boundary clips independently (report.rs).
//!
//! File versioning (10_EDITING Step 3):
//! - `read_file` returns `{ content, version }` where `version` is a
//!   content hash (SipHash-1-3 via std `DefaultHasher`).
//! - `create_file` is create-exclusive (O_CREAT|O_EXCL).
//! - `replace_file` is optimistic CAS: temp + rename iff version matches.

use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use wait_timeout::ChildExt;

use super::registry::{ToolDef, ToolRegistry};
use crate::report::clip;

/// OOM ceiling for `read_file`: files larger than this are refused
/// (loud error, never a silent clip).  Program-facing — the LLM never
/// sees full bytes.
pub const READ_FILE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Per-stream capture ceiling for `bash` stdout/stderr.  The drain
/// threads stop accumulating past this and the child is killed.
const BASH_OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Sanity ceiling on a command, against a runaway generation rather
/// than against style. It used to be 1 KB, and it *rejected*.
///
/// The rule it was enforcing — "one short pipeline, control flow in the
/// JS program" — is real, and the tool's own description states it. But
/// it is a preference about where logic reads best, not where the
/// design's advantage comes from: a program that batches two hundred
/// items through one heredoc costs exactly the same single completion
/// as one that loops in JS. Refusing spent a whole completion to
/// enforce a style rule, and the objective the style serves is
/// completions.
///
/// Measured on `sweep-200`, 2026-09-17: the model wrote a 2,204-byte
/// Python AST analysis into `bash`, was refused, and its next program
/// re-did the work as regex `matchAll` over Python source. It passed —
/// but it traded a correct approach for a fragile one and paid a
/// completion for the privilege, in the one task where completions are
/// the whole measurement.
///
/// So the length is reported now (`CompletionReport`'s note, at
/// [`BASH_COMMAND_LONG_BYTES`]) rather than refused: the work happens,
/// and the nudge arrives at the moment that earned it. There are still
/// good reasons to prefer JS — values stay in variables usable across
/// calls, a trap names a line instead of opaque shell output, and the
/// 30s/4MB ceilings apply to the whole script — and the note says so.
const BASH_COMMAND_MAX_BYTES: usize = 64 * 1024;
/// Where a command stops being "one short pipeline" and the completion
/// report says so. **A note, not a refusal** — see the cap above.
pub const BASH_COMMAND_LONG_BYTES: usize = 1024;

/// Wall-clock cap on a single command. `bash` is the first tool that
/// can hang indefinitely; a timeout turns that into a condition.
const BASH_TIMEOUT: Duration = Duration::from_secs(30);

/// The M1 registry: file tools + `bash` escape hatch + `create_file`/
/// `replace_file` writers (10_EDITING Step 3), plus the structural tools
/// `outline` + `parse_errors` (10_EDITING F2).  Network access is via
/// `bash` (curl/wget).
pub fn real_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(read_file_def());
    registry.register(bash_def());
    registry.register(create_file_def());
    registry.register(replace_file_def());
    registry.register(super::structural::outline_def());
    registry.register(super::structural::parse_errors_def());
    registry.register(wait_until_def());
    registry
}

// ── helpers ────────────────────────────────────────────────────────

/// Content hash of file bytes as a hex string.  Uses std
/// `DefaultHasher` (SipHash-1-3) — dep-free, fast, and the inputs are
/// machine-produced (a prior `read_file` version), so false negatives
/// are not a practical concern.
fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// A line diff in `git diff -U3` form, reporting **every** changed
/// region rather than the span between the first and last difference.
///
/// What this replaces was a common-prefix + common-suffix scan: one
/// hunk covering everything between the outermost differences, every
/// `-` line first and every `+` line after. On a live run that renamed
/// `note_display` to `note_json` at four sites in a 500 KB file (lines
/// ~4805 to ~7586) it emitted `@@ -4801,2786 +4801,2786 @@` and ~5,500
/// body lines for four one-line edits. `replace_file` clips the result
/// to 2 KB, and because the `-` side came first the clip removed the
/// `+` side *entirely*: the model read 37 deletions and no additions
/// for an edit that added exactly as many lines as it removed. The card
/// sells `diff` as the cheap check that a write landed, and the shipped
/// `04-many` example teaches `w.diff ? "changed" : "NO CHANGE"` — so a
/// diff that describes the opposite of what happened is worse than none.
///
/// **The diff itself is `similar`'s** (Apache-2.0, no transitive
/// dependencies, the engine behind `insta`). That follows the argument
/// already written beside `sha2` in Cargo.toml — "not a sentence worth
/// defending when the standard crate is this small" — and it applies
/// harder here, because what we are replacing *is* a hand-rolled diff
/// that shipped a confident lie; hand-rolling a second one repeats the
/// bet in the one place where being subtly wrong is the whole defect.
/// It also emits git's exact hunk headers (`,1` elided, an empty side
/// numbered from the line before) and git's `\ No newline at end of
/// file` marker, so the output is the format every model has already
/// read a million times.
///
/// This wrapper owns everything about *fitting in the channel*, which
/// is the part the crate cannot know:
///
/// - **`(no change)` for identical bytes**, unchanged and exact.
///   `replace_file` keys on that string and `delivered_tail` in
///   report.rs keys on the absent `diff` field it produces. A
///   `sweep-40` run that wrote identical bytes and reported 24
///   deletions is why that path exists.
/// - **A byte budget of its own** ([`DIFF_MAX_BYTES`]), under the 2 KB
///   `clip` at the call sites. The clip stays, but it must never be
///   the thing that truncates: a clip cuts mid-stream and reports only
///   a byte count, which is exactly how the `+` side vanished without
///   a word. Here the diff states its own truncation, in whole hunks,
///   and the clip becomes a no-op.
/// - **A per-op line cap** ([`DIFF_MAX_OP_LINES`]) so a large
///   replacement is elided *symmetrically* — some `-`, some `+`, and a
///   count for each — rather than spending the whole budget on the
///   removals and never reaching the additions. It is applied only when
///   the hunk does not fit whole, so an ordinary edit is never elided
///   for a rule's sake.
/// - **Long-line handling** ([`DIFF_MAX_LINE_BYTES`]). `read_file` has
///   no line-length assumption, so minified JS and single-line JSON are
///   real inputs. Printing a 500 KB line the way git does would leave
///   the reader nothing; a one-line-for-one-line replacement is instead
///   shown as a window around the first byte that differs.
///
/// **Cost bound.** The diff runs under [`DIFF_TIMEOUT`]; past it
/// `similar` stops searching for the minimal edit script and returns a
/// coarser but still correct one. On a 1 MB, 20,000-line file with four
/// scattered edits the whole call is single-digit milliseconds, so the
/// bound is only reachable by adversarial input. The cost of the bound
/// is determinism under load, which is worth it: the alternative to a
/// non-minimal diff is a blocked worker thread.
///
/// Checked against `git diff --no-index -U3` on four real edits to
/// `agent/src/report.rs` — a rename at five sites, a function inserted,
/// a 25-line block deleted, an indentation change — and byte-identical
/// to it on all four. The one deliberate omission is git's
/// function-context hint after the second `@@`: that is a language
/// heuristic, and a confidently wrong function name is the exact shape
/// of failure this change exists to remove. `outline` is the tool that
/// actually knows where a function starts.
fn diff_lines(old: &str, new: &str) -> String {
    if old == new {
        return "(no change)".into();
    }
    let diff = similar::TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old, new);
    render_diff(&diff)
}

/// Lines of unchanged context around each hunk — `git diff -U3`'s
/// default, and enough to place an edit in code the reader has not seen.
const DIFF_CONTEXT: usize = 3;

/// Byte budget [`diff_lines`] holds *itself* to, below the 2 KB `clip`
/// at the call sites. See the note on the clip above.
const DIFF_MAX_BYTES: usize = 1800;

/// Lines shown per side of a single insert/delete/replace before it is
/// elided with a count. Ordinary edits are far under it, so their
/// output is byte-identical to `git diff -U3`; the cap exists so that a
/// 2,786-line replacement spends half the budget on `-` and half on
/// `+` instead of all of it on `-`.
const DIFF_MAX_OP_LINES: usize = 20;

/// A line longer than this is clipped, with its true length stated.
/// Wide enough that no ordinary source line is touched.
const DIFF_MAX_LINE_BYTES: usize = 300;

/// Bytes of a long line shown either side of where it starts (and
/// stops) differing from its counterpart.
const DIFF_LINE_LEAD: usize = 48;

/// Wall-clock bound on the edit-script search. See [`diff_lines`].
const DIFF_TIMEOUT: Duration = Duration::from_millis(250);

fn render_diff(diff: &similar::TextDiff<'_, '_, str>) -> String {
    use similar::DiffOp;

    let (mut dels, mut ins) = (0usize, 0usize);
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_len, .. } => dels += old_len,
            DiffOp::Insert { new_len, .. } => ins += new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                dels += old_len;
                ins += new_len;
            }
        }
    }

    let mut unified = diff.unified_diff();
    unified.context_radius(DIFF_CONTEXT);
    let hunks: Vec<_> = unified.iter_hunks().collect();

    let mut out = String::new();
    let mut shown = 0usize;
    for (i, hunk) in hunks.iter().enumerate() {
        // Try it git's way first — every line, no elision — and only
        // fall back to the symmetric cap if that does not fit. A 25-line
        // deletion is an ordinary edit and there is usually room for it;
        // capping it when there was room would be inventing a difference
        // from `git diff -U3` for nothing.
        let room = DIFF_MAX_BYTES.saturating_sub(out.len());
        let text = render_hunk(diff, hunk, usize::MAX, room)
            .unwrap_or_else(|| render_hunk(diff, hunk, DIFF_MAX_OP_LINES, usize::MAX).unwrap());
        if i > 0 && out.len() + text.len() > DIFF_MAX_BYTES {
            break;
        }
        out.push_str(&text);
        shown += 1;
        if out.len() >= DIFF_MAX_BYTES {
            break;
        }
    }

    // A single hunk can still overrun. Cut on a line boundary rather
    // than leaving it to `clip`, so the note below survives.
    let mut cut_mid_hunk = false;
    if out.len() > DIFF_MAX_BYTES {
        let at = floor_boundary(&out, DIFF_MAX_BYTES);
        let at = out[..at].rfind('\n').map_or(0, |p| p + 1);
        out.truncate(at);
        cut_mid_hunk = true;
    }

    if shown < hunks.len() || cut_mid_hunk {
        out.push_str(&format!(
            "… {dels} line{} removed and {ins} added across {} region{}; \
             the first {shown} shown here{} …\n",
            plural(dels),
            hunks.len(),
            plural(hunks.len()),
            if cut_mid_hunk {
                ", the last cut short"
            } else {
                ""
            },
        ));
    }
    out
}

/// One hunk, with each op's `-` and `+` runs capped at `cap` lines.
/// Returns `None` if the body passed `abort_at` bytes, so the caller can
/// try again with a cap rather than build a megabyte it cannot use.
///
/// The header is git's, minus the trailing function-context hint git
/// appends after the second `@@`. That hint is a language heuristic, and
/// a confidently wrong function name is the exact failure mode this
/// whole change exists to remove; `outline` is the tool that actually
/// knows where a function starts.
fn render_hunk(
    diff: &similar::TextDiff<'_, '_, str>,
    hunk: &similar::udiff::UnifiedDiffHunk<'_, '_, '_, str>,
    cap: usize,
    abort_at: usize,
) -> Option<String> {
    let mut out = format!("{}\n", hunk.header());
    for op in hunk.ops() {
        render_op(&mut out, diff, op, cap);
        if out.len() > abort_at {
            return None;
        }
    }
    Some(out)
}

/// One insert / delete / replace / equal run. Deletions and insertions
/// are capped independently so a large replacement elides both sides,
/// and the insertions are buffered so they still follow the deletions
/// in git's order.
fn render_op(
    out: &mut String,
    diff: &similar::TextDiff<'_, '_, str>,
    op: &similar::DiffOp,
    cap: usize,
) {
    use similar::{ChangeTag, DiffOp};

    // The degenerate input `read_file` makes possible: a file that is
    // one enormous line (minified JS, a JSON blob). Printed whole, the
    // two lines would fill the budget and — being identical for their
    // first few thousand bytes — would show the reader nothing.
    if let DiffOp::Replace {
        old_len: 1,
        new_len: 1,
        ..
    } = *op
    {
        let changes: Vec<_> = diff.iter_changes(op).collect();
        let (a, b) = (changes[0].value(), changes[1].value());
        if trimmed(a).len() > DIFF_MAX_LINE_BYTES || trimmed(b).len() > DIFF_MAX_LINE_BYTES {
            render_long_pair(out, &changes[0], &changes[1]);
            return;
        }
    }

    let (mut del_total, mut del_shown) = (0usize, 0usize);
    let (mut ins_total, mut ins_shown) = (0usize, 0usize);
    let mut inserted = String::new();
    for change in diff.iter_changes(op) {
        match change.tag() {
            // Context runs are already bounded by the context radius.
            ChangeTag::Equal => push_change(out, ' ', &change),
            ChangeTag::Delete => {
                del_total += 1;
                if del_shown < cap {
                    push_change(out, '-', &change);
                    del_shown += 1;
                }
            }
            ChangeTag::Insert => {
                ins_total += 1;
                if ins_shown < cap {
                    push_change(&mut inserted, '+', &change);
                    ins_shown += 1;
                }
            }
        }
    }
    if del_total > del_shown {
        out.push_str(&format!(
            "… {} more line{} removed here …\n",
            del_total - del_shown,
            plural(del_total - del_shown)
        ));
    }
    out.push_str(&inserted);
    if ins_total > ins_shown {
        out.push_str(&format!(
            "… {} more line{} added here …\n",
            ins_total - ins_shown,
            plural(ins_total - ins_shown)
        ));
    }
}

/// One body line, git's `\ No newline at end of file` marker included,
/// clipped if it is longer than any line anyone reads.
fn push_change(out: &mut String, marker: char, change: &similar::Change<&str>) {
    let text = trimmed(change.value());
    out.push(marker);
    if text.len() > DIFF_MAX_LINE_BYTES {
        out.push_str(&text[..floor_boundary(text, DIFF_MAX_LINE_BYTES)]);
        out.push_str(&format!("… [clipped; the line is {} bytes]", text.len()));
    } else {
        out.push_str(text);
    }
    out.push('\n');
    if change.missing_newline() {
        out.push_str("\\ No newline at end of file\n");
    }
}

/// A one-line-for-one-line replacement where at least one side is far
/// too long to print: show where the two lines actually diverge, with a
/// window of context, instead of two near-identical walls of bytes.
fn render_long_pair(out: &mut String, old: &similar::Change<&str>, new: &similar::Change<&str>) {
    let (a, b) = (trimmed(old.value()), trimmed(new.value()));
    let head = common_prefix(a, b);
    let tail = common_suffix(a, b, head);
    out.push_str(&format!(
        "… one line, {} bytes → {} bytes; identical up to byte {head}, \
         and again from byte {} (old) / {} (new). The window between is shown …\n",
        a.len(),
        b.len(),
        a.len() - tail,
        b.len() - tail,
    ));
    out.push('-');
    out.push_str(&window(a, head, a.len() - tail));
    out.push('\n');
    if old.missing_newline() {
        out.push_str("\\ No newline at end of file\n");
    }
    out.push('+');
    out.push_str(&window(b, head, b.len() - tail));
    out.push('\n');
    if new.missing_newline() {
        out.push_str("\\ No newline at end of file\n");
    }
}

/// `[from, to)` plus [`DIFF_LINE_LEAD`] bytes either side, elided to
/// [`DIFF_MAX_LINE_BYTES`], with `…` wherever bytes were dropped.
fn window(line: &str, from: usize, to: usize) -> String {
    let start = floor_boundary(line, from.saturating_sub(DIFF_LINE_LEAD));
    let want = (to + DIFF_LINE_LEAD).min(line.len());
    let end = floor_boundary(line, want.min(start + DIFF_MAX_LINE_BYTES));
    let mut out = String::new();
    // Both elisions carry their size: a bare `…` at the front would
    // leave the reader guessing how far into the line the window sits,
    // and the byte offsets in the note above are only usable against it.
    if start > 0 {
        out.push_str(&format!("…[{start} bytes elided]"));
    }
    out.push_str(&line[start..end]);
    if end < line.len() {
        out.push_str(&format!("…[{} bytes elided]", line.len() - end));
    }
    out
}

/// Bytes shared at the front of two lines, on a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let limit = a.len().min(b.len());
    let mut i = 0;
    while i < limit && a.as_bytes()[i] == b.as_bytes()[i] {
        i += 1;
    }
    floor_boundary(a, i)
}

/// Bytes shared at the end of two lines, never overlapping `head`.
fn common_suffix(a: &str, b: &str, head: usize) -> usize {
    let limit = a.len().min(b.len()) - head;
    let mut i = 0;
    while i < limit && a.as_bytes()[a.len() - 1 - i] == b.as_bytes()[b.len() - 1 - i] {
        i += 1;
    }
    // Snap inwards on both sides: the suffix is byte-identical, so one
    // index is a char boundary exactly when the other is.
    while i > 0 && !(a.is_char_boundary(a.len() - i) && b.is_char_boundary(b.len() - i)) {
        i -= 1;
    }
    i
}

/// A line without its terminator. `\r` is deliberately kept: a CRLF/LF
/// conversion is a real edit, and `str::lines()` hiding it is one of
/// the reasons this does not use `str::lines()`.
fn trimmed(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Atomic write via temp file + rename in the target's directory.
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp_")
        .suffix("_edit")
        .tempfile_in(dir)
        .map_err(|e| format!("cannot create temp file in {dir:?}: {e}"))?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())
        .map_err(|e| format!("writing temp file: {e}"))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|e| format!("syncing temp file: {e}"))?;
    tmp.persist(path)
        .map_err(|e| format!("rename to {path:?}: {}", e.error))?;
    Ok(())
}

fn read_file_def() -> ToolDef {
    ToolDef {
        name: "read_file".into(),
        description: "Read a UTF-8 text file. The text lands on the record, not in front of you. `version` is a content hash — hand it to `replace_file` so the write fails if the file moved under you. `from`/`to` are 1-based inclusive lines."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "name": "path", "type": "string", "description": "absolute or cwd-relative path" },
                { "name": "from", "type": "integer", "description": "first line, 1-based (optional)" },
                { "name": "to", "type": "integer", "description": "last line, inclusive (optional)" }
            ],
            "minItems": 1,
            "maxItems": 3
        }),
        guidelines: vec![
            "**If you will read this yourself — not just compute on it — you must `history.peek(r)` in this same reply.** Nothing else shows it to you: the result is a variable, and printing it back is replaced by the id of the row it repeats. `history.keep(r)` instead only if you will read it again later — its bytes are in every request from here on, where a peek's are in one.".into(),
            "Read files with this, not with `cat` or `sed` through bash. The `version` it hands back is what makes a later write atomic; a file read any other way has to be read again before you can safely write it."
                .into(),
            "Hold onto `version` and hand it to `replace_file`, so a write fails rather than clobbering a file that moved.".into(),
        ],
        example: Some("const f = await tools.read_file(\"README.md\");".into()),
        // **No `truncated`.** It was declared and the handler cannot
        // produce it: the returns line is `{ content, version }` on
        // every path, and a range is an explicit `from`/`to` the caller
        // asked for rather than a clip anyone needs telling about.
        // `bash` does have one — it fires at the 4MB stream cap — which
        // is presumably where this was copied from. A field a program
        // can branch on and never see is a dead branch in every program
        // that checks it.
        returns: Some("{ content: string; version: string; id: number }".into()),
        show_once: false,
        handler: Box::new(|args| {
            let path = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("read_file(path) needs a string path")?;
            let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
            if meta.len() > READ_FILE_MAX_BYTES as u64 {
                return Err(format!(
                    "file is {} bytes (limit {READ_FILE_MAX_BYTES}); \
                     read something smaller",
                    meta.len()
                ));
            }
            let whole = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            // The hash covers the whole file even when a span was asked
            // for: it is what `replace_file` checks, and a version
            // computed over a slice would let a write succeed against a
            // file that changed everywhere else.
            let version = hash_bytes(whole.as_bytes());
            let (start, end) = (
                args.get(1).and_then(|v| v.as_u64()),
                args.get(2).and_then(|v| v.as_u64()),
            );
            let content = match (start, end) {
                (None, None) => whole,
                (Some(a), b) => {
                    let total = whole.lines().count();
                    if a == 0 {
                        return Err("read_file line numbers are 1-based; got 0".into());
                    }
                    let last = b.unwrap_or(total as u64);
                    if a as usize > total {
                        return Err(format!(
                            "start_line {a} is past the end of {path} ({total} lines)"
                        ));
                    }
                    whole
                        .lines()
                        .skip(a as usize - 1)
                        .take((last.saturating_sub(a) + 1) as usize)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                (None, Some(_)) => {
                    return Err(
                        "read_file([path, start, end]) needs a start when given an end".into(),
                    );
                }
            };
            Ok(json!({ "content": content, "version": version }))
        }),
    }
}

fn create_file_def() -> ToolDef {
    ToolDef {
        name: "create_file".into(),
        description: "Create a new file atomically. Errors if the path exists — for an existing file use `replace_file`."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "name": "path", "type": "string", "description": "absolute or cwd-relative path" },
                { "name": "content", "type": "string", "description": "UTF-8 content to write" }
            ],
            "minItems": 2,
            "maxItems": 2
        }),
        guidelines: Vec::new(),
        example: Some("await tools.create_file(\"notes.md\", body);".into()),
        returns: Some("{ version: string; id: number }".into()),
        show_once: false,
        handler: Box::new(|args| {
            let path = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("create_file(path, content) needs a string path")?;
            let content = args
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or("create_file(path, content) needs string content")?;
            let p = Path::new(path);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        format!(
                            "{path}: already exists — use replace_file(path, content, version) \
                             instead (read the file first for its version)"
                        )
                    } else {
                        format!("{path}: {e}")
                    }
                })?;
            atomic_write(p, content)?;
            let version = hash_bytes(content.as_bytes());
            Ok(json!({ "version": version }))
        }),
    }
}

fn replace_file_def() -> ToolDef {
    ToolDef {
        name: "replace_file".into(),
        description: "Write atomically iff the file still matches `version` (from `read_file`). On mismatch, errors with the current version rather than clobbering."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "name": "path", "type": "string", "description": "absolute or cwd-relative path" },
                { "name": "content", "type": "string", "description": "new UTF-8 content" },
                { "name": "version", "type": "string", "description": "expected version (from read_file)" }
            ],
            "minItems": 3,
            "maxItems": 3
        }),
        guidelines: vec![
            "Name the edit by text you have in hand — `Edit.replaceOnce` refuses an ambiguous one — rather than computing a line number and splicing.".into(),
            "`diff` comes back with the new version — the lines that actually changed. Read it: it is the cheap check that the edit landed where you meant, and it costs nothing where re-reading the file costs a call and its bytes. It is absent only when the write changed nothing.".into(),
            "What the diff cannot tell you is whether the result *works*. Run the thing that would fail, in this same program.".into(),
        ],
        // **`new` is a reserved word.** The example read
        // `Edit.replaceOnce(f.content, old, new)` and a model copying
        // it gets "Unexpected token" — while the card's own `Edit`
        // declaration, two screens up, correctly writes the parameter
        // as `new_`. An example nobody can copy is worse than none.
        example: Some("const { diff } = await tools.replace_file(p, Edit.replaceOnce(f.content, old, replacement), f.version);".into()),
        returns: Some("{ version: string; diff?: string; id: number }".into()),
        show_once: true,
        handler: Box::new(|args| {
            let path = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("replace_file(path, content, expected_version) needs a string path")?;
            // Content before version: it is the order a model writes
            // unprompted, and two live runs out of three got the old
            // (path, version, content) wrong — passing file text where
            // the hash goes, which fails as a version mismatch and reads
            // like someone else edited the file. `fs.writeFile(path,
            // content)` is the shape everyone has; the CAS token is the
            // afterthought and belongs last.
            let new_content = args
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or("replace_file(path, content, expected_version) needs string content")?;
            let expected = args
                .get(2)
                .and_then(|v| v.as_str())
                .ok_or("replace_file(path, content, expected_version) needs a version string")?;
            let p = Path::new(path);

            let current = std::fs::read_to_string(p).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "{path}: no such file — use create_file(path, content) \
                         for new files"
                    )
                } else {
                    format!("{path}: {e}")
                }
            })?;
            let current_version = hash_bytes(current.as_bytes());

            if current_version != expected {
                let diff = clip(&diff_lines(&current, new_content), 2048);
                return Err(format!(
                    "file changed: expected version {expected}, now {current_version} — \
                     re-read and re-apply. To overwrite anyway, call replace_file \
                     again with version {current_version} (last-write-wins that still \
                     goes through the CAS, never a blind clobber).\n\
                     diff (expected→your content):\n{diff}"
                ));
            }

            atomic_write(p, new_content)?;
            let new_version = hash_bytes(new_content.as_bytes());
            let diff = clip(&diff_lines(&current, new_content), 2048);
            let mut result = json!({ "version": new_version });
            if !diff.contains("(no change)") {
                result["diff"] = json!(diff);
            }
            Ok(result)
        }),
    }
}

fn bash_def() -> ToolDef {
    ToolDef {
        name: "bash".into(),
        description: "A shell command. 30s timeout, 4MB per stream. A single pipeline is what it does best; multi-step logic usually reads better in the JS program, though a script here is allowed when it is the right tool. Runs with `pipefail`, so the status is the failing stage's, and a `| head` that truncates still succeeds. Non-zero is a result, not an error; a command that could not run at all rejects."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "name": "command", "type": "string", "description": "shell command (kept short)" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        guidelines: vec![
            "**If you will read this yourself — not just compute on it — you must `history.peek(r)` in this same reply.** Nothing else shows it to you: the result is a variable, and printing it back is replaced by the id of the row it repeats. `history.keep(r)` instead only if you will read it again later — its bytes are in every request from here on, where a peek's are in one.".into(),
            "Read `status` before `stdout`. A command that ran and failed writes nothing, and nothing reads as \"found no problems\".".into(),
            "Ask once for everything it will answer at once: change all the candidates, run it once, and read which ones it names back. A per-item loop is the fallback.".into(),
            "Prefer looping in the program rather than in the command: the values stay in variables you can use in the next call and return at the end, and a mistake stops at a line instead of somewhere inside a heredoc. When a script really is the right tool — a parser, something with no JS equivalent — write the script.".into(),
        ],
        example: Some("const r = await tools.bash(\"make check 2>&1\");".into()),
        returns: Some(
            "{ status: number; stdout: string; stderr: string; truncated?: boolean; id: number }"
                .into(),
        ),
        show_once: false,
        handler: Box::new(|args| {
            // Accept the command as a string, or as an argv array joined
            // with spaces (`bash(["mkdir","-p","/x"])` → "mkdir -p /x") —
            // a common reflex from Node's spawn/execFile, including the
            // wrapped-string `bash(["mkdir -p /x"])`. Plain tokens are the
            // norm, so joining removes a sharp edge rather than failing.
            let command: String = match args.get(0) {
                Some(v) if v.is_string() => v.as_str().unwrap().to_owned(),
                Some(v) if v.is_array() => {
                    let mut words = Vec::new();
                    for part in v.as_array().unwrap() {
                        match part.as_str() {
                            Some(w) => words.push(w),
                            None => {
                                return Err("bash([...]) array must hold only \
                                            strings (argv words)"
                                    .into());
                            }
                        }
                    }
                    words.join(" ")
                }
                _ => {
                    return Err("bash takes the command as a string — \
                                e.g. tools.bash(\"mkdir -p /x && ls /x\") — or an \
                                array of words joined with spaces"
                        .into());
                }
            };
            if command.len() > BASH_COMMAND_MAX_BYTES {
                return Err(format!(
                    "command is {} bytes, past the {BASH_COMMAND_MAX_BYTES}-byte ceiling \
                     — that is a runaway, not a script",
                    command.len()
                ));
            }
            run_bash(&command, BASH_TIMEOUT)
        }),
    }
}

/// The process's current directory, as a thing that must be held still.
///
/// `bash` inherits the process cwd, and so do the relative paths
/// `read_file`/`replace_file` resolve. A test (or an eval run) that
/// points the cwd at a temporary directory is therefore reaching into
/// global state every other concurrent test shares — and when that
/// directory is *deleted* while a `bash` subprocess starts in it, the
/// shell writes
/// "error retrieving current directory: getcwd: cannot access parent
/// directories" onto that subprocess's stderr, which is how this was
/// found: `bash_returns_status_stdout_stderr` failed three runs in four
/// at full parallelism and passed every time alone.
///
/// `scripted` already had a mutex for this, but scoped to itself, so
/// it serialised the eval tests against each other and not against the
/// `bash` tests here. One process has one cwd, so the lock belongs
/// beside the thing that inherits it.
#[cfg(test)]
pub(crate) static PROCESS_CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Spawn `bash -o pipefail -c <command>`, enforce the timeout, and shape
/// the outcome into `{ status, stdout, stderr, truncated? }`. Only the
/// harness-level failures (spawn failed, timed out) return `Err` — a
/// command that runs and exits non-zero is a normal result the program
/// branches on.
///
/// **`pipefail` is on, and that is the point.** A shell pipeline's
/// status is its *last* stage's, so `cargo test 2>&1 | tail -5` reports
/// `tail`'s success whatever the tests did, and `cmd | grep x | head`
/// reports 0 when `cmd` never ran. The card used to carry a paragraph
/// asking every program to write `set -o pipefail` itself, and a live
/// `skipped-tests` run on 2026-09-17 shows what that costs when one
/// forgets: it un-skipped a test, ran `python3 -m unittest … | tail
/// -15` against a file its own edit had left syntactically broken, read
/// `tail`'s 0, kept the change, and reported a clean sweep. Every
/// verdict in that loop was `tail` succeeding.
///
/// An instruction the model must remember at every call site is worse
/// than a fact about the tool it must know once — and this is *our*
/// bash, with no compatibility contract to keep. The trade is a
/// different surprise, stated in the tool's own description: `grep`
/// exits 1 when it matches nothing, so an unmatched grep pipeline now
/// reports failure. Between a silent false negative ("clean sweep" over
/// work never done) and a loud false positive, the loud one is the one
/// a program can see.
///
/// `stdout`/`stderr` are drained on reader threads with a capture ceiling
/// per stream so a command that out-writes the OS pipe buffer can't
/// deadlock the timed wait or fill RAM.  If either stream hits the
/// ceiling the child is killed and the result flags `truncated: true`.
fn run_bash(command: &str, timeout: Duration) -> Result<serde_json::Value, String> {
    let mut child = Command::new("bash")
        .arg("-o")
        .arg("pipefail")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // **Nothing downstream is a terminal.** See `strip_ansi`.
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("PYTHON_COLORS", "0")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "0")
        .env_remove("FORCE_COLOR")
        .spawn()
        .map_err(|e| format!("spawning bash: {e}"))?;

    fn drain<R: Read + Send + 'static>(
        pipe: Option<R>,
        cap: usize,
    ) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
        let pipe = pipe.expect("piped");
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut taken = pipe.take(cap as u64 + 1);
            let _ = taken.read_to_end(&mut buf);
            let truncated = buf.len() > cap;
            if truncated {
                buf.truncate(cap);
            }
            (buf, truncated)
        })
    }
    let out_reader = drain(child.stdout.take(), BASH_OUTPUT_MAX_BYTES);
    let err_reader = drain(child.stderr.take(), BASH_OUTPUT_MAX_BYTES);

    let status = child
        .wait_timeout(timeout)
        .map_err(|e| format!("waiting on bash: {e}"))?;
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "command timed out after {}s and was killed; run something faster or \
                 split the work",
                timeout.as_secs()
            ));
        }
    };

    let (stdout, out_trunc) = out_reader.join().unwrap_or_default();
    let (stderr, err_trunc) = err_reader.join().unwrap_or_default();
    if out_trunc || err_trunc {
        let _ = child.kill();
        let _ = child.wait();
    }

    // **"It ran and said no" and "it never ran" are different events.**
    // A non-zero status is a *verdict* for almost everything we run —
    // `cargo check` failing is the answer a dead-code probe wants,
    // `npm test` failing is what tells a loop to revert, `grep` exits 1
    // on no matches — so a non-zero exit resolves like any other result
    // and the program branches on it. That is why this does not simply
    // reject: making the common case an exception turns every probe
    // loop into a try/catch around expected control flow.
    //
    // But 126/127 are not verdicts. They are bash reporting that it
    // could not execute the command at all — not found, not executable
    // — and the line this function already draws ("only harness-level
    // failures return Err") puts them on the other side: failing to
    // spawn *the command* is the same event as failing to spawn bash,
    // one level down. Resolved, they are the silent false negative the
    // card has three paragraphs about: `{status: 127, stdout: ""}`, and
    // a program reading stdout sees nothing and concludes there was
    // nothing to find.
    //
    // Signals stay a verdict deliberately, except when bash itself was
    // signalled (`code()` is `None`). A child killed by the OOM killer
    // surfaces as the shell's 137, which is indistinguishable from a
    // program that chose to exit 137, and guessing wrong there would
    // reject a real result.
    let code = status.code();
    let stderr_text = String::from_utf8_lossy(&stderr);
    // Truncation is a signal *we* sent: the ceiling was hit, we killed
    // the child, and the bytes already captured are a real (flagged)
    // result. So it is settled before the checks below, which are about
    // deaths nobody here asked for.
    match code {
        None if !(out_trunc || err_trunc) => {
            return Err(format!(
                "the shell was killed by a signal before the command finished{}",
                clip_stderr(&stderr_text)
            ));
        }
        Some(127) => {
            return Err(format!(
                "command not found — nothing ran, so there is no result to read{}",
                clip_stderr(&stderr_text)
            ));
        }
        Some(126) => {
            return Err(format!(
                "command found but not executable — nothing ran, so there is no \
                 result to read{}",
                clip_stderr(&stderr_text)
            ));
        }
        _ => {}
    }

    // **SIGPIPE is not a failure of the pipeline; it is how `head`
    // works.** `pipefail` reports the rightmost non-zero stage, and
    // `grep … | head -40` that actually truncates leaves `grep` killed
    // by SIGPIPE — 141 — for doing exactly what was asked. Turning on
    // `pipefail` without this makes one of the commonest idioms a
    // program writes report failure on success, which is a worse lie
    // than the one it fixes.
    //
    // Nothing else is hidden by it: `pipefail` takes the *rightmost*
    // non-zero status, so a stage that failed for a real reason still
    // wins over an upstream 141, and a missing command still surfaces
    // as the 127 above.
    let code = match code {
        Some(141) => Some(0),
        other => other,
    };

    let mut result = json!({
        "status": code,
        "stdout": strip_ansi(&String::from_utf8_lossy(&stdout)),
        "stderr": strip_ansi(&String::from_utf8_lossy(&stderr)),
    });
    if out_trunc || err_trunc {
        result["truncated"] = json!(true);
    }
    Ok(result)
}

/// Terminal escape sequences out of captured output.
///
/// **Nothing downstream of this tool is a terminal.** The output goes
/// to a JavaScript program that matches on it and to a document the
/// model reads; in both places an escape is invisible punctuation that
/// breaks a match for no reason anyone can see.
///
/// Measured 2026-09-19. A run wrote exactly the right program — probe
/// the tests with the skips removed, keep the ones that pass — and
/// matched `/test_\w+ \(.+\) \.\.\. ok/` against
/// `test_base_rate (…) ... \x1b[32mok\x1b[0m`. The set came back empty,
/// the file was written back byte-identical, and the model reported
/// "un-skipped (none)" in good faith. Nothing in the console it was
/// shown next would have told it why: the escapes render as colour, or
/// as nothing.
///
/// The `NO_COLOR`/`TERM=dumb` environment above asks tools not to emit
/// these; this is what makes it true of the ones that do anyway. Both,
/// because the environment is the polite request and this is the
/// guarantee.
///
/// CSI sequences (`ESC [ … final`) and the two-byte escapes around
/// them. Deliberately not a full terminal emulator: no cursor
/// movement is replayed, nothing is reflowed. A byte that is not
/// display control is kept.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameter and intermediate bytes, then a final in
            // `@`–`~`.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST (`ESC \`).
            Some(']') => {
                let mut prev_esc = false;
                for c in chars.by_ref() {
                    if c == '\u{7}' || (prev_esc && c == '\\') {
                        break;
                    }
                    prev_esc = c == '\u{1b}';
                }
            }
            // A bare two-byte escape, or a trailing lone ESC.
            Some(_) | None => {}
        }
    }
    out
}

/// bash's own complaint, appended to a rejection so the program is told
/// *which* command was missing rather than only that one was.
fn clip_stderr(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        return String::new();
    }
    format!(": {}", crate::report::clip_short(text, 200))
}

/// Current wall-clock time as epoch milliseconds — matches `Date.now()`,
/// which is how a program builds a `wait_until` deadline.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Longest a single `wait_until` call may block for. This tool's whole
/// point is a real, uninterruptible wall-clock block on a worker thread —
/// there is no mechanism to cancel one in flight — so a miscalculated
/// deadline (wrong units, a bad `Date.now()` arithmetic slip) would
/// otherwise wedge that thread for as long as the mistake says, same
/// failure shape `bash`'s own timeout guards against (`BASH_TIMEOUT`),
/// just with a much longer legitimate use case behind it. Loop with
/// several calls for a wait longer than this.
const WAIT_UNTIL_MAX: Duration = Duration::from_secs(15 * 60);

fn wait_until_def() -> ToolDef {
    ToolDef {
        name: "wait_until".into(),
        description: "Block until wall-clock `Date.now()` reaches this absolute deadline, then resolve null. A deadline already past resolves at once."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "name": "deadlineMs", "type": "integer", "description": "epoch milliseconds to wait until" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        guidelines: Vec::new(),
        example: Some("await tools.wait_until(Date.now() + 5000);".into()),
        returns: Some("null".into()),
        show_once: false,
        handler: Box::new(|args| {
            let target_ms = args
                .get(0)
                .and_then(|v| v.as_i64())
                .ok_or("wait_until(epoch_ms) needs a numeric epoch-ms deadline")?;
            let delta = Duration::from_millis((target_ms - now_ms()).max(0) as u64);
            if delta > WAIT_UNTIL_MAX {
                return Err(format!(
                    "wait_until deadline is {delta:?} away (limit {WAIT_UNTIL_MAX:?}) — \
                     likely a unit mistake (epoch_ms, not seconds). For a longer wait, \
                     loop with several wait_until calls instead of one."
                ));
            }
            std::thread::sleep(delta);
            Ok(json!(null))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn read_file(args: serde_json::Value) -> Result<serde_json::Value, String> {
        (read_file_def().handler)(args)
    }

    fn create_file(args: serde_json::Value) -> Result<serde_json::Value, String> {
        (create_file_def().handler)(args)
    }

    fn replace_file(args: serde_json::Value) -> Result<serde_json::Value, String> {
        (replace_file_def().handler)(args)
    }

    fn temp_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        (dir, path)
    }

    #[test]
    fn read_file_returns_content_and_version() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "hello from a file").unwrap();
        let result = read_file(json!([file.path().to_str().unwrap()])).unwrap();
        assert_eq!(result["content"], json!("hello from a file"));
        assert!(result["version"].is_string());
        assert!(!result["version"].as_str().unwrap().is_empty());
    }

    #[test]
    fn read_file_reads_a_line_span_and_still_versions_the_whole_file() {
        // `outline` hands back start_line/end_line; without a span read
        // those numbers point at something only `sed` could fetch. The
        // version must still cover the whole file, or a write could
        // succeed against a file that changed outside the span read.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "one\ntwo\nthree\nfour\nfive").unwrap();
        let path = file.path().to_str().unwrap();

        let span = read_file(json!([path, 2, 4])).unwrap();
        assert_eq!(span["content"], json!("two\nthree\nfour"));

        let whole = read_file(json!([path])).unwrap();
        assert_eq!(
            span["version"], whole["version"],
            "the hash covers the file, not the span"
        );

        // An open-ended span runs to the end.
        let tail = read_file(json!([path, 4])).unwrap();
        assert_eq!(tail["content"], json!("four\nfive"));

        // 1-based, and a start past the end is an error rather than an
        // empty string that reads like a truthful answer.
        assert!(read_file(json!([path, 0, 2])).is_err());
        assert!(read_file(json!([path, 99])).is_err());
    }

    #[test]
    fn read_file_version_is_stable() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "stable content").unwrap();
        let v1 = read_file(json!([file.path().to_str().unwrap()])).unwrap()["version"].clone();
        let v2 = read_file(json!([file.path().to_str().unwrap()])).unwrap()["version"].clone();
        assert_eq!(v1, v2, "version must be deterministic for the same bytes");
    }

    #[test]
    fn read_file_version_changes_with_content() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "content A").unwrap();
        let v1 = read_file(json!([file.path().to_str().unwrap()])).unwrap()["version"].clone();
        write!(file, "content B").unwrap();
        let v2 = read_file(json!([file.path().to_str().unwrap()])).unwrap()["version"].clone();
        assert_ne!(v1, v2, "version must change when content changes");
    }

    #[test]
    fn read_file_round_trips_large_file_uncut() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        // 100 KB: above the old 48 KB clip, well below the 16 MB ceiling.
        let content = "x".repeat(100_000);
        write!(file, "{content}").unwrap();
        let result = read_file(json!([file.path().to_str().unwrap()])).unwrap();
        let text = result["content"].as_str().unwrap();
        assert_eq!(text.len(), 100_000, "full fidelity, no clip");
        assert!(text.starts_with('x'));
    }

    #[test]
    fn read_file_refuses_overlarge_files() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        // Create a file just over the OOM ceiling.
        let content = "y".repeat(READ_FILE_MAX_BYTES + 1);
        write!(file, "{content}").unwrap();
        let err = read_file(json!([file.path().to_str().unwrap()])).unwrap_err();
        assert!(err.contains("limit"), "{err}");
        assert!(err.contains("read something smaller"), "{err}");
    }

    #[test]
    fn read_file_reports_missing_paths() {
        let err = read_file(json!(["/no/such/file/anywhere"])).unwrap_err();
        assert!(err.contains("/no/such/file/anywhere"), "{err}");
        let err = read_file(json!([42])).unwrap_err();
        assert!(err.contains("needs a string path"), "{err}");
    }

    // ── create_file ─────────────────────────────────────────────────

    #[test]
    fn create_file_writes_and_returns_version() {
        let (_dir, path) = temp_path();
        let result = create_file(json!([path.to_str().unwrap(), "hello world"])).unwrap();
        assert!(result["version"].is_string());
        assert!(!result["version"].as_str().unwrap().is_empty());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "hello world");
    }

    #[test]
    fn create_file_on_existing_path_errors() {
        let (_dir, path) = temp_path();
        create_file(json!([path.to_str().unwrap(), "first"])).unwrap();
        let err = create_file(json!([path.to_str().unwrap(), "second"])).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("replace_file"), "{err}");
    }

    // ── replace_file ────────────────────────────────────────────────

    fn read_version(path: &std::path::Path) -> String {
        let content = std::fs::read_to_string(path).unwrap();
        hash_bytes(content.as_bytes())
    }

    #[test]
    fn replace_file_round_trips_under_matching_version() {
        let (_dir, path) = temp_path();
        let result = create_file(json!([path.to_str().unwrap(), "original"])).unwrap();
        let version = result["version"].as_str().unwrap();

        let result = replace_file(json!([path.to_str().unwrap(), "modified", version])).unwrap();
        assert!(result["version"].is_string());
        assert_ne!(result["version"].as_str().unwrap(), version);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "modified");
    }

    #[test]
    fn replace_file_after_out_of_band_edit_returns_mismatch_condition() {
        let (_dir, path) = temp_path();
        let result = create_file(json!([path.to_str().unwrap(), "first write"])).unwrap();
        let old_version = result["version"].as_str().unwrap();

        // Out-of-band edit: someone else wrote to the file.
        std::fs::write(&path, "second write").unwrap();
        let current_version = read_version(&path);
        assert_ne!(current_version, old_version);

        let err =
            replace_file(json!([path.to_str().unwrap(), "third write", old_version])).unwrap_err();
        assert!(err.contains("file changed"), "{err}");
        assert!(err.contains(old_version), "{err}");
        assert!(err.contains(&current_version), "{err}");
        assert!(err.contains("re-read and re-apply"), "{err}");
        // Diff appears in the error message.
        assert!(err.contains("@@"), "{err}");
    }

    #[test]
    fn replace_file_on_absent_path_redirects_to_create_file() {
        let (_dir, path) = temp_path();
        let err =
            replace_file(json!([path.to_str().unwrap(), "content", "any-version"])).unwrap_err();
        assert!(err.contains("no such file"), "{err}");
        assert!(err.contains("create_file"), "{err}");
    }

    #[test]
    fn replace_file_success_includes_diff() {
        let (_dir, path) = temp_path();
        let result = create_file(json!([path.to_str().unwrap(), "line1\nline2\nline3\n"])).unwrap();
        let version = result["version"].as_str().unwrap();

        let result = replace_file(json!([
            path.to_str().unwrap(),
            "line1\nline2b\nline3\n",
            version
        ]))
        .unwrap();
        assert!(result["diff"].is_string());
        let diff = result["diff"].as_str().unwrap();
        assert!(diff.contains("-line2"), "{diff}");
        assert!(diff.contains("+line2b"), "{diff}");
    }

    #[test]
    fn atomic_write_leaves_no_partial_file_on_simulated_failure() {
        let (_dir, path) = temp_path();
        // Write initial content so we have a valid version.
        let result = create_file(json!([path.to_str().unwrap(), "initial"])).unwrap();
        let version = result["version"].as_str().unwrap();

        // The temp-file + rename strategy makes partial writes impossible:
        // either the rename succeeds (new content visible) or it doesn't
        // (old content preserved).  We verify that `persist` semantics
        // hold: the file is either fully the new content or untouched.
        let ok = replace_file(json!([path.to_str().unwrap(), "replaced", version]));
        match ok {
            Ok(_) => {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced");
            }
            Err(_) => {
                // On any error the old content must still be intact.
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "initial");
            }
        }
    }

    /// Every `bash` test goes through here, and every one of them holds
    /// [`PROCESS_CWD`] for the call: the subprocess inherits the
    /// process cwd, so a concurrent test moving it (or dropping the
    /// `TempDir` it moved to) lands in this one's stderr.
    fn bash(args: serde_json::Value) -> Result<serde_json::Value, String> {
        let _cwd = PROCESS_CWD.lock().unwrap_or_else(|e| e.into_inner());
        (bash_def().handler)(args)
    }

    /// **The exact failure this was written for.** A colourised
    /// `unittest -v` line, matched by the regex a live run actually
    /// used. Before the strip the set came back empty and the run
    /// silently changed nothing.
    #[test]
    fn a_colourised_line_matches_the_pattern_a_program_would_write() {
        let raw = "test_base_rate (m.T.test_base_rate) ... \u{1b}[32mok\u{1b}[0m\n\
                   test_total_world (m.T.test_total_world) ... \u{1b}[31mFAIL\u{1b}[0m\n";
        let clean = strip_ansi(raw);
        assert_eq!(
            clean,
            "test_base_rate (m.T.test_base_rate) ... ok\n\
             test_total_world (m.T.test_total_world) ... FAIL\n"
        );
    }

    /// Nothing that is not display control is touched — including the
    /// brackets, dots and backslashes that look like escapes.
    #[test]
    fn strip_ansi_leaves_ordinary_text_alone() {
        for text in [
            "plain",
            "a[32mb",
            "path\\to\\file [ok] (1.2s) 100%",
            "",
            "unicode: é — ✓",
        ] {
            assert_eq!(strip_ansi(text), text);
        }
    }

    /// Hyperlinks and title-setting are OSC, which ends at BEL or at
    /// `ESC \\` rather than at a letter.
    #[test]
    fn strip_ansi_takes_osc_sequences_whole() {
        assert_eq!(strip_ansi("a\u{1b}]0;my title\u{7}b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}]8;;http://x\u{1b}\\b"), "ab");
    }

    /// A truncated stream can end mid-escape; that must not eat the
    /// rest of the output or panic.
    #[test]
    fn strip_ansi_survives_a_cut_off_escape() {
        assert_eq!(strip_ansi("ok\u{1b}"), "ok");
        assert_eq!(strip_ansi("ok\u{1b}["), "ok");
        assert_eq!(strip_ansi("ok\u{1b}[32"), "ok");
    }

    /// End to end through the tool itself, not just the helper.
    #[test]
    fn bash_output_reaches_the_program_without_escapes() {
        let out = bash(json!(["printf 'a\\033[31mred\\033[0mb\\n'"])).unwrap();
        assert_eq!(out["stdout"].as_str().unwrap(), "aredb\n");
    }

    #[test]
    fn bash_returns_status_stdout_stderr() {
        let result = bash(json!(["echo out; echo err 1>&2"])).unwrap();
        assert_eq!(result["status"], json!(0));
        assert_eq!(result["stdout"], json!("out\n"));
        assert_eq!(result["stderr"], json!("err\n"));
    }

    #[test]
    fn bash_nonzero_exit_is_a_result_not_an_error() {
        let result = bash(json!(["exit 3"])).unwrap();
        assert_eq!(result["status"], json!(3));
    }

    /// **A pipeline's status is the first failing stage's.** The shell's
    /// default is the *last* stage's, which is how `cargo test 2>&1 |
    /// tail -5` reports `tail`'s success whatever the tests did. A live
    /// `skipped-tests` run on 2026-09-17 un-skipped a test, ran the
    /// suite against a file its own edit had left syntactically broken,
    /// read `tail`'s 0, kept the change and reported a clean sweep.
    #[test]
    fn a_pipelines_status_is_the_first_failing_stage_not_the_last() {
        let result = bash(json!(["exit 3 | tail -5"])).unwrap();
        assert_eq!(result["status"], json!(3), "tail's 0 would hide it");
    }

    /// **`| head` that truncates is a success.** `pipefail` reports the
    /// rightmost non-zero stage, and a `grep … | head -40` that
    /// actually truncates leaves `grep` killed by SIGPIPE — 141 — for
    /// doing exactly what was asked. Turning `pipefail` on without
    /// normalising that makes one of the commonest idioms a program
    /// writes report failure on success: a worse lie than the one
    /// `pipefail` fixes.
    #[test]
    fn a_pipeline_cut_short_by_head_is_not_a_failure() {
        let result = bash(json!(["seq 1 100000 | head -3"])).unwrap();
        assert_eq!(result["status"], json!(0), "SIGPIPE is how head works");
        assert_eq!(result["stdout"], json!("1\n2\n3\n"));
    }

    /// And it hides nothing: a stage that failed for a real reason is
    /// to the right of the SIGPIPE'd one, and `pipefail` takes the
    /// rightmost non-zero.
    #[test]
    fn a_real_failure_still_wins_over_an_upstream_sigpipe() {
        let result = bash(json!(["seq 1 100000 | head -3 | grep nothing"])).unwrap();
        assert_eq!(
            result["status"],
            json!(1),
            "grep found nothing, and says so"
        );
    }

    /// The other half of that trade, stated in the tool's description
    /// because it is the one thing `pipefail` makes noisier: an
    /// unmatched `grep` is a non-zero pipeline now.
    #[test]
    fn an_unmatched_grep_pipeline_reports_non_zero() {
        let result = bash(json!(["echo hello | grep nothing | cat"])).unwrap();
        assert_ne!(result["status"], json!(0));
        assert_eq!(result["stdout"], json!(""));
    }

    /// **"It ran and said no" is a result; "it never ran" is not.** A
    /// command bash could not execute resolved as `{status: 127,
    /// stdout: ""}`, and a program reading stdout saw nothing and
    /// concluded there was nothing to find — the silent false negative
    /// the card spends three paragraphs on. It rejects now, on the same
    /// line this function already drew: failing to spawn *the command*
    /// is the event failing to spawn bash is, one level down.
    #[test]
    fn a_command_that_could_not_run_rejects_instead_of_resolving_empty() {
        let err = bash(json!(["definitely-not-a-real-binary-xyz"])).unwrap_err();
        assert!(err.contains("command not found"), "{err}");
        // bash's own complaint comes with it, so the program is told
        // *which* command was missing.
        assert!(err.contains("definitely-not-a-real-binary-xyz"), "{err}");
    }

    /// Ran and failed is still a result — the distinction above is not
    /// an excuse to reject verdicts. This is the shape every probe loop
    /// in the card is built on.
    #[test]
    fn a_command_that_ran_and_failed_is_still_a_result() {
        let result = bash(json!(["ls /definitely/not/here"])).unwrap();
        assert_ne!(result["status"], json!(0));
        assert!(
            result["stderr"].as_str().unwrap().contains("No such file"),
            "{result}"
        );
    }

    #[test]
    fn bash_accepts_argv_array_joined_with_spaces() {
        // The observed reflex: passing the command as an argv array, or a
        // string wrapped in one. Both are accepted, joined with spaces, and
        // run identically to the bare-string form.
        let argv = bash(json!([["echo", "hi", "there"]])).unwrap();
        assert_eq!(argv["stdout"], json!("hi there\n"));
        let wrapped = bash(json!([["echo wrapped"]])).unwrap();
        assert_eq!(wrapped["stdout"], json!("wrapped\n"));
        // A non-string element is still a loud error.
        let err = bash(json!([["echo", 7]])).unwrap_err();
        assert!(err.contains("only strings"), "{err}");
    }

    #[test]
    fn bash_caps_output_at_ceiling_and_flags_truncation() {
        // ~200 KB of output: exceeds the OS pipe buffer (proves no
        // deadlock) and triggers the capture-ceiling truncation if we set
        // a low cap. Use the real `run_bash` with a small cap for this
        // test by setting BASH_OUTPUT_MAX_BYTES low — but since that's a
        // const, instead test with `yes` piped through `head`.
        let result = bash(json!(["yes ABCDEFGH | head -n 50000"])).unwrap();
        let stdout = result["stdout"].as_str().unwrap();
        // 50000 lines of "ABCDEFGH\n" = ~450 KB, which is under the 4 MB
        // ceiling — so it passes through uncut. But let's just verify we
        // get a lot of output.
        assert!(
            stdout.len() > 100_000,
            "large output uncut: {}",
            stdout.len()
        );
    }

    #[test]
    fn bash_with_unbounded_output_is_killed_at_ceiling() {
        // `yes` writes forever; the capture ceiling (4 MB/stream) stops
        // accumulation and tries to kill the child.
        let result = bash(json!(["yes"])).unwrap();
        // Either the stream hit the ceiling and flagged truncation, or
        // the 30s timeout killed it — both are correct behaviors.
        let truncated = result
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let stdout_len = result["stdout"].as_str().unwrap().len();
        if truncated {
            assert!(
                stdout_len <= BASH_OUTPUT_MAX_BYTES,
                "truncated stdout at ceiling: {stdout_len}"
            );
        }
        // In either case, the output is bounded.
        assert!(stdout_len < BASH_OUTPUT_MAX_BYTES + 100, "output bounded");
    }

    #[test]
    fn bash_times_out_and_reports_it() {
        let err = run_bash("sleep 60", Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn bash_runs_a_script_and_refuses_only_a_runaway() {
        // Past the sanity ceiling: still refused, and the message says
        // what that ceiling is about.
        let runaway = format!("echo {}", "x".repeat(BASH_COMMAND_MAX_BYTES));
        let err = bash(json!([runaway])).unwrap_err();
        assert!(err.contains("runaway"), "{err}");

        // A script — well past the old 1 KB limit, nowhere near the
        // ceiling — runs. It used to be refused, which cost a whole
        // completion to enforce a preference about where logic reads
        // best; the completion report notes the length instead.
        let script = format!("true # {}", "x".repeat(BASH_COMMAND_LONG_BYTES * 3));
        assert!(script.len() > BASH_COMMAND_LONG_BYTES);
        let out = bash(json!([script])).expect("a long command runs");
        assert_eq!(out["status"], 0, "{out}");
    }

    // ── wait_until ───────────────────────────────────────────────────

    fn wait_until(args: serde_json::Value) -> Result<serde_json::Value, String> {
        (wait_until_def().handler)(args)
    }

    #[test]
    fn wait_until_a_past_deadline_resolves_immediately() {
        let start = std::time::Instant::now();
        let result = wait_until(json!([0])).unwrap();
        assert_eq!(result, json!(null));
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wait_until_blocks_until_the_deadline() {
        let deadline = now_ms() + 100;
        let start = std::time::Instant::now();
        wait_until(json!([deadline])).unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wait_until_rejects_a_non_numeric_deadline() {
        let err = wait_until(json!(["soon"])).unwrap_err();
        assert!(err.contains("epoch-ms"), "{err}");
    }

    #[test]
    fn wait_until_rejects_a_deadline_past_the_cap_instead_of_blocking() {
        let start = std::time::Instant::now();
        let far_future = now_ms() + Duration::from_secs(3600).as_millis() as i64; // 1h > 15m cap
        let err = wait_until(json!([far_future])).unwrap_err();
        assert!(err.contains("limit"), "{err}");
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "rejected fast, did not sleep: {:?}",
            start.elapsed()
        );
    }

    /// **Every field a tool promises is a field it delivers, and every
    /// field it delivers is one it promised.**
    ///
    /// `returns` is the model's only statement of a result's shape — it
    /// becomes the declaration's return type in the manifest, and a
    /// program is written against it before any result exists. Nothing
    /// checked it against a real call.
    ///
    /// Both directions, because they fail differently. A promised field
    /// that never arrives is a program written for a shape that does
    /// not exist. A delivered field that was never promised is one the
    /// model can only find by accident — and `outline` shipped a
    /// declared `"variable"` kind that no input could produce until
    /// 4dc78f1, which is this test's shape one level down.
    #[test]
    fn every_returned_field_is_declared_and_every_declared_field_arrives() {
        // A field name in a `{ a: T; b?: U }` return type. `?` marks a
        // field that may legitimately be absent, so it is allowed to
        // miss the delivered side but not to arrive undeclared.
        fn declared(returns: &str) -> (Vec<String>, Vec<String>) {
            let (mut all, mut required) = (Vec::new(), Vec::new());
            // Split on `;` at depth zero only. A nested shape —
            // `{ items: Array<{ name: string; kind: … }> }` — must not
            // contribute `name` and `kind` as though they were fields
            // of the result itself, which is what the first cut of this
            // did and what made it fail on `outline`.
            let body = returns.trim().trim_start_matches('{').trim_end_matches('}');
            let (mut depth, mut start) = (0i32, 0usize);
            let mut parts: Vec<&str> = Vec::new();
            for (i, ch) in body.char_indices() {
                match ch {
                    '{' | '<' | '(' | '[' => depth += 1,
                    '}' | '>' | ')' | ']' => depth -= 1,
                    ';' if depth == 0 => {
                        parts.push(&body[start..i]);
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            parts.push(&body[start..]);
            for part in parts {
                let Some((lhs, _)) = part.split_once(':') else {
                    continue;
                };
                let name = lhs.trim();
                let (bare, optional) = match name.strip_suffix('?') {
                    Some(b) => (b, true),
                    None => (name, false),
                };
                if bare.is_empty() {
                    continue;
                }
                all.push(bare.to_owned());
                if !optional {
                    required.push(bare.to_owned());
                }
            }
            (all, required)
        }

        let dir = tempfile::TempDir::new().unwrap();
        let existing = dir.path().join("a.rs");
        std::fs::write(&existing, "fn f() {}\n").unwrap();
        let existing = existing.to_str().unwrap().to_owned();
        let version =
            (read_file_def().handler)(serde_json::json!([existing.clone()])).unwrap()["version"]
                .as_str()
                .unwrap()
                .to_owned();
        let fresh = dir.path().join("new.txt").to_str().unwrap().to_owned();

        // One live call per tool, in the shape its own signature asks
        // for. `wait_until` takes a deadline already past, which its
        // description says resolves at once.
        let calls: Vec<(ToolDef, serde_json::Value)> = vec![
            (read_file_def(), serde_json::json!([existing.clone()])),
            (bash_def(), serde_json::json!(["echo hi"])),
            (create_file_def(), serde_json::json!([fresh, "x"])),
            (
                replace_file_def(),
                serde_json::json!([existing.clone(), "fn g() {}\n", version]),
            ),
            (
                super::super::structural::outline_def(),
                serde_json::json!([existing.clone()]),
            ),
            (
                super::super::structural::parse_errors_def(),
                serde_json::json!([existing.clone()]),
            ),
            (wait_until_def(), serde_json::json!([1.0])),
        ];

        for (def, args) in calls {
            let Some(returns) = def.returns.clone() else {
                continue;
            };
            if returns.trim() == "null" {
                continue;
            }
            let got = (def.handler)(args).unwrap_or_else(|e| panic!("{}: {e}", def.name));
            if got.as_object().is_none() {
                panic!("{}: returns {returns} but delivered {got}", def.name);
            }
            let (all, required) = declared(&returns);
            // **What the model sees, not what the handler returned.**
            // `machine.rs` injects `id` into every object result before
            // it is logged, so that is part of the shape a program is
            // written against — and every one of these declarations
            // denied it existed while the card told the model to use
            // `f.id`. Testing the handler alone could not see that;
            // this is the layer the promise is actually made about.
            let mut got = got;
            if got.as_object().is_some() {
                got.as_object_mut()
                    .expect("checked")
                    .insert("id".into(), serde_json::json!(1));
            }
            let obj = got.as_object().expect("checked");
            for field in &required {
                assert!(
                    obj.contains_key(field),
                    "{} promises `{field}` in `{returns}` and did not deliver it: {}",
                    def.name,
                    serde_json::to_string(&got).unwrap_or_default()
                );
            }
            for field in obj.keys() {
                assert!(
                    all.contains(field),
                    "{} delivered `{field}`, which `{returns}` never mentions — \
                     the model can only find it by accident",
                    def.name
                );
            }
        }
    }

    // ── diff_lines ─────────────────────────────────────────────────
    //
    // The case these exist for: a live run renamed `note_display` to
    // `note_json` at four sites in a 500 KB file and the old
    // prefix/suffix scan reported one 2,786-line hunk whose `+` side
    // the 2 KB clip then removed entirely. See `diff_lines`.
    //
    // The output was also checked against `git diff -U3` on four real
    // edits to `agent/src/report.rs` — a rename at five sites, a
    // function inserted, a 25-line block deleted, an indentation
    // change — and is byte-identical to it bar git's function-context
    // hint after the second `@@`. `matches_git_unified_format` pins a
    // small case of that; the rest is what `walk_hunks` enforces.

    /// Every changed line in a rendered diff, `-`/`+` marker included.
    fn changed_lines(diff: &str) -> Vec<&str> {
        diff.lines()
            .filter(|l| l.starts_with('-') || l.starts_with('+'))
            .filter(|l| !l.starts_with("@@"))
            .collect()
    }

    /// Replay a diff against both files, asserting that **every** line
    /// it prints is at the line number its hunk header implies.
    ///
    /// This is the property the card claims for `outline`'s
    /// `start_line` — "an edit anchor, not just a fact" — and a diff's
    /// numbers are acted on the same way, by `Edit.replaceLines`. An
    /// off-by-one here is worse than printing no number at all.
    fn walk_hunks(diff: &str, old: &str, new: &str) -> usize {
        let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
        let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
        let strip = |s: &str| s.strip_suffix('\n').unwrap_or(s).to_owned();
        let (mut o, mut n) = (0usize, 0usize);
        let mut hunks = 0;
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix("@@ -") {
                let (old_part, rest) = rest.split_once(" +").unwrap();
                let new_part = rest.split_once(" @@").unwrap().0;
                let num =
                    |p: &str| -> usize { p.split(',').next().unwrap().parse::<usize>().unwrap() };
                // 1-based; a zero-length side is numbered from the line
                // before, which is 0 at the top of the file.
                o = num(old_part).max(1) - 1;
                n = num(new_part).max(1) - 1;
                hunks += 1;
                continue;
            }
            let Some(marker) = line.chars().next() else {
                continue;
            };
            let body = &line[1..];
            match marker {
                ' ' => {
                    assert_eq!(strip(old_lines[o]), body, "context at old line {}", o + 1);
                    assert_eq!(strip(new_lines[n]), body, "context at new line {}", n + 1);
                    o += 1;
                    n += 1;
                }
                '-' => {
                    assert_eq!(strip(old_lines[o]), body, "removal at old line {}", o + 1);
                    o += 1;
                }
                '+' => {
                    assert_eq!(strip(new_lines[n]), body, "addition at new line {}", n + 1);
                    n += 1;
                }
                // `\ No newline…` and the `…` notes carry no line.
                _ => {}
            }
        }
        hunks
    }

    /// The motivating file: 7,600 lines, one signature repeated at four
    /// scattered sites, renamed at all four.
    fn rename_case() -> (String, String) {
        let sig = "pub(crate) fn note_display(value: &serde_json::Value) -> String {";
        let sites = [4804usize, 5500, 6800, 7585];
        let mut old = String::new();
        for i in 0..7600 {
            if sites.contains(&i) {
                old.push_str(sig);
                old.push('\n');
            } else {
                old.push_str(&format!("    // filler line {i}\n"));
            }
        }
        let new = old.replace("note_display", "note_json");
        (old, new)
    }

    #[test]
    fn scattered_edits_produce_one_small_hunk_each() {
        let (old, new) = rename_case();
        let diff = diff_lines(&old, &new);
        let headers: Vec<&str> = diff.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(headers.len(), 4, "one hunk per site, got:\n{diff}");
        for h in &headers {
            // Seven lines a side: three context, the change, three more.
            assert!(h.ends_with(",7 @@"), "hunk should be tiny: {h}\n{diff}");
        }
        let changed = changed_lines(&diff);
        assert_eq!(
            changed.iter().filter(|l| l.starts_with('-')).count(),
            4,
            "{diff}"
        );
        // The `+` side is the half the clip used to eat.
        assert_eq!(
            changed.iter().filter(|l| l.starts_with('+')).count(),
            4,
            "{diff}"
        );
        assert!(diff.contains("+pub(crate) fn note_json"), "{diff}");
        assert!(!diff.contains("…"), "nothing should be elided:\n{diff}");
        assert!(
            diff.len() < 2048,
            "the whole diff must survive the 2 KB clip, was {} bytes:\n{diff}",
            diff.len()
        );
        // And the hunks are where the edits are, not at line 1.
        assert!(diff.contains("@@ -4802,7 +4802,7 @@"), "{diff}");
        assert!(diff.contains("@@ -7583,7 +7583,7 @@"), "{diff}");
    }

    #[test]
    fn every_printed_line_is_at_the_number_its_hunk_claims() {
        let (old, new) = rename_case();
        assert_eq!(walk_hunks(&diff_lines(&old, &new), &old, &new), 4);

        // The same guarantee on shapes that move the two sides out of
        // step with each other.
        let cases = [
            (
                "a\nb\nc\nd\ne\nf\ng\nh\n",
                "a\nb\nX\nY\nZ\nc\nd\ne\nf\ng\nh\n",
            ),
            ("a\nb\nc\nd\ne\nf\ng\nh\n", "a\ne\nf\ng\nh\n"),
            ("a\nb\nc\nd\ne\nf\ng\nh\n", "A\nb\nc\nd\ne\nf\ng\nH\n"),
            ("", "a\nb\n"),
            ("a\nb\n", ""),
            ("a\nb\nc", "a\nB\nc"),
        ];
        for (old, new) in cases {
            assert!(walk_hunks(&diff_lines(old, new), old, new) > 0, "{old:?}");
        }
    }

    #[test]
    fn matches_git_unified_format() {
        // Both bodies are `git diff --no-index -U3` verbatim, with only
        // the `---`/`+++` file header and git's function-context hint
        // removed — the two things a diff of a buffer cannot have.

        // Close enough together that git merges them into one hunk.
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let new = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n";
        assert_eq!(
            diff_lines(old, new),
            [
                "@@ -1,10 +1,10 @@",
                " one",
                " two",
                "-three",
                "+THREE",
                " four",
                " five",
                " six",
                " seven",
                " eight",
                " nine",
                "-ten",
                "+TEN",
                "",
            ]
            .join("\n")
        );

        // Far enough apart that git splits them.
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new: String = (1..=20)
            .map(|i| match i {
                3 | 14 => format!("L{i}\n"),
                _ => format!("l{i}\n"),
            })
            .collect();
        assert_eq!(
            diff_lines(&old, &new),
            [
                "@@ -1,6 +1,6 @@",
                " l1",
                " l2",
                "-l3",
                "+L3",
                " l4",
                " l5",
                " l6",
                "@@ -11,7 +11,7 @@",
                " l11",
                " l12",
                " l13",
                "-l14",
                "+L14",
                " l15",
                " l16",
                " l17",
                "",
            ]
            .join("\n")
        );
    }

    #[test]
    fn identical_bytes_are_exactly_no_change() {
        // `replace_file` keys on this string and `delivered_tail` keys
        // on the absent `diff` field it produces.
        assert_eq!(diff_lines("a\nb\nc\n", "a\nb\nc\n"), "(no change)");
        assert_eq!(diff_lines("", ""), "(no change)");
    }

    #[test]
    fn pure_insertion_shows_only_additions() {
        let diff = diff_lines("a\nb\nc\n", "a\nb\nX\nY\nc\n");
        assert_eq!(changed_lines(&diff), vec!["+X", "+Y"], "{diff}");
        assert!(diff.starts_with("@@ -1,3 +1,5 @@\n"), "{diff}");
    }

    #[test]
    fn pure_deletion_shows_only_removals() {
        let diff = diff_lines("a\nb\nX\nY\nc\n", "a\nb\nc\n");
        assert_eq!(changed_lines(&diff), vec!["-X", "-Y"], "{diff}");
        assert!(diff.starts_with("@@ -1,5 +1,3 @@\n"), "{diff}");
    }

    #[test]
    fn edits_at_the_very_first_and_very_last_line() {
        let old = "first\nb\nc\nd\nlast\n";
        let new = "FIRST\nb\nc\nd\nLAST\n";
        let diff = diff_lines(old, new);
        // Close enough together that the context merges them into one.
        assert!(diff.starts_with("@@ -1,5 +1,5 @@\n"), "{diff}");
        assert_eq!(
            changed_lines(&diff),
            vec!["-first", "+FIRST", "-last", "+LAST"],
            "{diff}"
        );
        assert_eq!(walk_hunks(&diff, old, new), 1);
    }

    #[test]
    fn far_apart_first_and_last_line_edits_stay_two_hunks() {
        let mut old = String::from("first\n");
        for i in 0..200 {
            old.push_str(&format!("body {i}\n"));
        }
        old.push_str("last\n");
        let new = old
            .replace("first\n", "FIRST\n")
            .replace("last\n", "LAST\n");
        let diff = diff_lines(&old, &new);
        assert_eq!(walk_hunks(&diff, &old, &new), 2, "{diff}");
        assert!(diff.starts_with("@@ -1,4 +1,4 @@\n"), "{diff}");
        assert!(diff.contains("@@ -199,4 +199,4 @@"), "{diff}");
    }

    #[test]
    fn empty_on_either_side() {
        let diff = diff_lines("", "a\nb\n");
        assert!(diff.starts_with("@@ -0,0 +1,2 @@\n"), "{diff}");
        assert_eq!(changed_lines(&diff), vec!["+a", "+b"], "{diff}");

        let diff = diff_lines("a\nb\n", "");
        assert!(diff.starts_with("@@ -1,2 +0,0 @@\n"), "{diff}");
        assert_eq!(changed_lines(&diff), vec!["-a", "-b"], "{diff}");
    }

    #[test]
    fn a_dropped_trailing_newline_is_a_change_not_no_change() {
        // `str::lines()` drops the final terminator, so this edit would
        // render as `(no change)` for anything built on it — the precise
        // lie `(no change)` exists to prevent.
        let diff = diff_lines("a\nb\nc\n", "a\nb\nc");
        assert_ne!(diff, "(no change)");
        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
        assert_eq!(changed_lines(&diff), vec!["-c", "+c"], "{diff}");

        // And the other direction.
        let diff = diff_lines("a\nb\nc", "a\nb\nc\n");
        assert_ne!(diff, "(no change)");
        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    }

    #[test]
    fn a_file_with_no_trailing_newline_diffs_normally() {
        let diff = diff_lines("a\nb\nc", "a\nB\nc");
        assert_eq!(changed_lines(&diff), vec!["-b", "+B"], "{diff}");
        assert!(diff.starts_with("@@ -1,3 +1,3 @@\n"), "{diff}");
        // The unchanged last line still carries the marker, as git does.
        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    }

    #[test]
    fn a_line_ending_conversion_is_not_no_change() {
        // `str::lines()` also eats the `\r` of a CRLF pair.
        let diff = diff_lines("a\r\nb\r\n", "a\nb\n");
        assert_ne!(diff, "(no change)");
        assert_eq!(changed_lines(&diff).len(), 4, "{diff}");
    }

    #[test]
    fn alternating_lines_with_two_far_apart_edits_stay_two_hunks() {
        // Nothing in this file is unique, which is the shape that makes
        // a naive anchor diff give up and a prefix/suffix scan report
        // the whole 1,980-line span as changed.
        let mut old = String::new();
        for i in 0..2000 {
            old.push_str(if i % 2 == 0 { "a\n" } else { "b\n" });
        }
        let mut lines: Vec<&str> = old.split_inclusive('\n').collect();
        lines[10] = "c\n";
        lines[1990] = "d\n";
        let new: String = lines.concat();

        let diff = diff_lines(&old, &new);
        assert_eq!(walk_hunks(&diff, &old, &new), 2, "{diff}");
        assert_eq!(changed_lines(&diff), vec!["-a", "+c", "-a", "+d"], "{diff}");
        assert!(diff.len() < 256, "{} bytes:\n{diff}", diff.len());
    }

    #[test]
    fn a_large_replacement_elides_both_sides_not_just_the_additions() {
        // The original defect, in miniature: 400 lines replaced by 400.
        // A truncation that keeps every `-` and no `+` is what taught a
        // model that an edit had deleted code it had in fact rewritten.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..400 {
            old.push_str(&format!("old body line {i}\n"));
            new.push_str(&format!("new body line {i}\n"));
        }
        let diff = diff_lines(&old, &new);
        let changed = changed_lines(&diff);
        let dels = changed.iter().filter(|l| l.starts_with('-')).count();
        let adds = changed.iter().filter(|l| l.starts_with('+')).count();
        assert_eq!(dels, DIFF_MAX_OP_LINES, "{diff}");
        assert_eq!(adds, DIFF_MAX_OP_LINES, "{diff}");
        assert!(diff.contains("380 more lines removed here"), "{diff}");
        assert!(diff.contains("380 more lines added here"), "{diff}");
        assert!(diff.len() < 2048, "{} bytes", diff.len());
    }

    #[test]
    fn a_hunk_that_fits_is_not_elided_at_all() {
        // The cap must not invent a difference from `git diff -U3` when
        // there was room: a 25-line deletion is an ordinary edit.
        let mut old = String::new();
        for i in 0..40 {
            old.push_str(&format!("line {i}\n"));
        }
        let new: String = old
            .split_inclusive('\n')
            .enumerate()
            .filter(|(i, _)| !(5..30).contains(i))
            .map(|(_, l)| l)
            .collect();
        let diff = diff_lines(&old, &new);
        assert!(!diff.contains("…"), "{diff}");
        assert_eq!(changed_lines(&diff).len(), 25, "{diff}");
        assert_eq!(walk_hunks(&diff, &old, &new), 1);
    }

    #[test]
    fn many_regions_are_counted_rather_than_half_shown() {
        // 300 scattered one-line edits: far more hunks than fit.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..3000 {
            old.push_str(&format!("line {i}\n"));
            new.push_str(&format!(
                "{} {i}\n",
                if i % 10 == 0 { "LINE" } else { "line" }
            ));
        }
        let diff = diff_lines(&old, &new);
        assert!(
            diff.contains("300 lines removed and 300 added across 300 regions"),
            "{diff}"
        );
        // Under the clip, so the note is the last word rather than
        // something `clip` cuts off mid-sentence.
        assert!(diff.len() < 2048, "{} bytes", diff.len());
        assert_eq!(clip(&diff, 2048), diff);
        // Both sides present in what did fit — the original failure was
        // a clip that kept every `-` and no `+`.
        let changed = changed_lines(&diff);
        assert!(changed.iter().any(|l| l.starts_with('-')), "{diff}");
        assert!(changed.iter().any(|l| l.starts_with('+')), "{diff}");
    }

    #[test]
    fn a_file_that_is_one_enormous_line_shows_where_it_differs() {
        // Minified JS and single-line JSON are real inputs: `read_file`
        // makes no assumption about line length. Printing the line the
        // way git does would spend the whole budget on bytes identical
        // on both sides.
        let mut old = String::from("var DATA=[");
        for i in 0..20000 {
            old.push_str(&format!("{{\"k{i}\":\"vvvvvvvv\"}},"));
        }
        old.push_str("];");
        let new = old.replace("\"k9000\":\"vvvvvvvv\"", "\"k9000\":\"REPLACED\"");
        assert!(old.len() > 400_000 && !old.contains('\n'));

        let diff = diff_lines(&old, &new);
        assert!(diff.starts_with("@@ -1 +1 @@\n"), "{diff}");
        assert!(diff.len() < 800, "{} bytes:\n{diff}", diff.len());
        // The changed bytes themselves, in context, on both sides.
        assert!(diff.contains("-…[") && diff.contains("+…["), "{diff}");
        assert!(diff.contains("\"k9000\":\"vvvvvvvv\""), "{diff}");
        assert!(diff.contains("\"k9000\":\"REPLACED\""), "{diff}");
        assert!(diff.contains("bytes elided"), "{diff}");
        // And it says how long the line really is, both sides.
        assert!(
            diff.contains(&format!(
                "one line, {} bytes → {} bytes",
                old.len(),
                new.len()
            )),
            "{diff}"
        );
    }

    #[test]
    fn an_ordinary_long_line_is_clipped_with_its_true_length() {
        let long = "x".repeat(5000);
        let old = format!("a\n{long}\nb\n");
        let new = format!("a\n{long}\nb\nc\n");
        let diff = diff_lines(&old, &new);
        assert!(diff.contains("[clipped; the line is 5000 bytes]"), "{diff}");
        assert!(diff.contains("+c"), "{diff}");
        assert!(diff.len() < 800, "{} bytes", diff.len());
    }

    #[test]
    fn a_half_megabyte_file_diffs_in_well_under_the_timeout() {
        // 500 KB files are the stated scale; the timeout is a fence
        // against adversarial input, not something a real edit nears.
        let mut old = String::new();
        for i in 0..20000 {
            old.push_str(&format!("    let value_{i} = compute_{i}(input, {i});\n"));
        }
        assert!(old.len() > 500_000);
        let new = old
            .replace("compute_4804(", "computed_4804(")
            .replace("compute_19900(", "computed_19900(");
        let started = std::time::Instant::now();
        let diff = diff_lines(&old, &new);
        assert!(
            started.elapsed() < DIFF_TIMEOUT,
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(walk_hunks(&diff, &old, &new), 2, "{diff}");
    }

    #[test]
    fn cas_mismatch_error_carries_a_readable_multi_hunk_diff() {
        let (_dir, path) = temp_path();
        let mut on_disk = String::new();
        for i in 0..400 {
            on_disk.push_str(&format!("line {i}\n"));
        }
        let result = create_file(json!([path.to_str().unwrap(), on_disk])).unwrap();
        let stale = result["version"].as_str().unwrap().to_owned();
        std::fs::write(&path, on_disk.replace("line 5\n", "line 5 edited\n")).unwrap();

        let mine = on_disk.replace("line 300\n", "line 300 mine\n");
        let err = replace_file(json!([path.to_str().unwrap(), mine, stale])).unwrap_err();
        assert!(err.contains("diff (expected→your content)"), "{err}");
        // Both divergences, each in its own hunk, not one 295-line blob.
        assert!(err.contains("-line 5 edited"), "{err}");
        assert!(err.contains("+line 300 mine"), "{err}");
        assert_eq!(err.matches("@@").count(), 4, "{err}");
    }
}

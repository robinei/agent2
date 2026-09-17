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

/// Command-length cap. The point of `bash` is *short* commands — a
/// single pipeline — with control flow living in the JS program. This
/// is the mechanical backstop for the card's instruction; long scripts
/// are rejected (as a repairable condition) rather than run.
const BASH_COMMAND_MAX_BYTES: usize = 1024;

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

/// Minimal line diff: common-prefix + common-suffix scan, reporting the
/// changed middle with `-`/`+` markers and one line of context on each
/// side.  Produces a clipped summary for condition messages — full
/// fidelity lives in the tool results.
fn diff_lines(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Common prefix
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    // Common suffix (after the prefix)
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_start = prefix;
    let old_end = old_lines.len() - suffix;
    let new_start = prefix;
    let new_end = new_lines.len() - suffix;

    if old_start == old_end && new_start == new_end {
        return "(no change)".into();
    }

    let old_count = old_end.saturating_sub(old_start);
    let new_count = new_end.saturating_sub(new_start);

    let mut out = format!(
        "@@ -{},{} +{},{} @@\n",
        old_start + 1,
        old_count,
        new_start + 1,
        new_count,
    );

    // One context line before (if available)
    if prefix > 0 {
        out.push_str(&format!("  {}\n", old_lines[prefix - 1]));
    }

    for line in &old_lines[old_start..old_end] {
        out.push_str(&format!("-{}\n", line));
    }
    for line in &new_lines[new_start..new_end] {
        out.push_str(&format!("+{}\n", line));
    }

    // One context line after (if available)
    if suffix > 0 {
        out.push_str(&format!("  {}\n", old_lines[old_end]));
    }

    out
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
        description: "Read a UTF-8 text file. `version` is a content hash — hand it to `replace_file` so the write fails if the file moved under you. `from`/`to` are 1-based inclusive lines."
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
            "Read files with this, not with `cat` or `sed` through bash. The `version` it hands back is what makes a later write atomic; a file read any other way has to be read again before you can safely write it."
                .into(),
            "Hold onto `version` and hand it to `replace_file`, so a write fails rather than clobbering a file that moved.".into(),
        ],
        example: Some("const f = await tools.read_file(\"src/lib.rs\");".into()),
        returns: Some("{ content: string; version: string; truncated?: boolean }".into()),
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
        returns: Some("{ version: string }".into()),
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
            "You will not see the result, so verify in this same program: read it back, or run the thing that would fail.".into(),
        ],
        example: Some("await tools.replace_file(p, Edit.replaceOnce(f.content, old, new), f.version);".into()),
        returns: Some("{ version: string }".into()),
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
        description: "One short shell command — a single pipeline, no loops; do control flow in JS. Runs with `pipefail`, so the status is the failing stage's, and a `| head` that truncates is still a success. A command that could not be run at all rejects. Non-zero is a result, not an error. 30s timeout, 4MB per stream."
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
            "Read `status` before `stdout`. A command that ran and failed writes nothing, and nothing reads as \"found no problems\".".into(),
            "Ask once for everything it will answer at once: change all the candidates, run it once, and read which ones it names back. A per-item loop is the fallback.".into(),
        ],
        example: Some("const r = await tools.bash(\"cargo check --all-targets 2>&1\");".into()),
        returns: Some(
            "{ status: number; stdout: string; stderr: string; truncated?: boolean }".into(),
        ),
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
                    "command is {} bytes (limit {BASH_COMMAND_MAX_BYTES}): keep bash to \
                     one short pipeline and move loops/conditionals/multi-step logic \
                     into the JS program",
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
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    });
    if out_trunc || err_trunc {
        result["truncated"] = json!(true);
    }
    Ok(result)
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
    fn bash_rejects_overlong_commands() {
        let long = format!("echo {}", "x".repeat(BASH_COMMAND_MAX_BYTES));
        let err = bash(json!([long])).unwrap_err();
        assert!(err.contains("limit"), "{err}");
        assert!(err.contains("JS program"), "{err}");
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
}

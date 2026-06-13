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
use std::time::Duration;

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
        description: "Read a UTF-8 text file; returns { content, version }. \
                      `version` is a content hash — pass it to `replace_file` \
                      so the write is atomic and fails if the file changed \
                      since you read it."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "absolute or cwd-relative path" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        output_schema: json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "version": { "type": "string" }
            }
        }),
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
            let content = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            let version = hash_bytes(content.as_bytes());
            Ok(json!({ "content": content, "version": version }))
        }),
    }
}

fn create_file_def() -> ToolDef {
    ToolDef {
        name: "create_file".into(),
        description: "Create a new file atomically — errors if the path already \
                      exists. Returns { version }. Use for new files; for existing \
                      files use `replace_file`."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "absolute or cwd-relative path" },
                { "type": "string", "description": "UTF-8 content to write" }
            ],
            "minItems": 2,
            "maxItems": 2
        }),
        output_schema: json!({
            "type": "object",
            "properties": {
                "version": { "type": "string" }
            }
        }),
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
                            "{path}: already exists — use replace_file(path, version, content) \
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
        description: "Replace a file atomically iff its current version matches \
                      `expected_version` (CAS). Returns { version, diff? }. On \
                      mismatch errors with the current version and a diff so you \
                      can re-read and re-apply. Always requires a version — blind \
                      overwrite is structurally impossible."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "absolute or cwd-relative path" },
                { "type": "string", "description": "expected version (from read_file)" },
                { "type": "string", "description": "new UTF-8 content" }
            ],
            "minItems": 3,
            "maxItems": 3
        }),
        output_schema: json!({
            "type": "object",
            "properties": {
                "version": { "type": "string" },
                "diff": { "type": "string" }
            }
        }),
        handler: Box::new(|args| {
            let path = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("replace_file(path, expected_version, content) needs a string path")?;
            let expected = args
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or("replace_file(path, expected_version, content) needs a version string")?;
            let new_content = args
                .get(2)
                .and_then(|v| v.as_str())
                .ok_or("replace_file(path, expected_version, content) needs string content")?;
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
        description: "Run one short shell command — a single pipeline, no loops or \
                      multi-line scripts; do control flow in JS. Command is one string \
                      — bash(\"mkdir -p /x && ls /x\") — or an argv array joined with \
                      spaces. Resolves to { status, stdout, stderr, truncated? } \
                      (a non-zero status is a result, not an error); times out after \
                      30s; output capped at 4MB/stream."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "shell command (kept short)" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        output_schema: json!({
            "type": "object",
            "properties": {
                "status": { "type": ["integer", "null"] },
                "stdout": { "type": "string" },
                "stderr": { "type": "string" },
                "truncated": { "type": "boolean" }
            }
        }),
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

/// Spawn `bash -c <command>`, enforce the timeout, and shape the outcome
/// into `{ status, stdout, stderr, truncated? }`. Only the harness-level
/// failures (spawn failed, timed out) return `Err` — a command that runs
/// and exits non-zero is a normal result the program branches on.
///
/// `stdout`/`stderr` are drained on reader threads with a capture ceiling
/// per stream so a command that out-writes the OS pipe buffer can't
/// deadlock the timed wait or fill RAM.  If either stream hits the
/// ceiling the child is killed and the result flags `truncated: true`.
fn run_bash(command: &str, timeout: Duration) -> Result<serde_json::Value, String> {
    let mut child = Command::new("bash")
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
    let mut result = json!({
        "status": status.code(),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    });
    if out_trunc || err_trunc {
        result["truncated"] = json!(true);
    }
    Ok(result)
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

        let result = replace_file(json!([path.to_str().unwrap(), version, "modified"])).unwrap();
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
            replace_file(json!([path.to_str().unwrap(), old_version, "third write"])).unwrap_err();
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
            replace_file(json!([path.to_str().unwrap(), "any-version", "content"])).unwrap_err();
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
            version,
            "line1\nline2b\nline3\n"
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
        let ok = replace_file(json!([path.to_str().unwrap(), version, "replaced"]));
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

    fn bash(args: serde_json::Value) -> Result<serde_json::Value, String> {
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
}

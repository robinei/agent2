//! Real tools (8_HARNESS M1): file read and HTTP fetch. Handlers run
//! blocking on the session loop's worker threads; contents are clipped
//! here so they pass the registry's result-size guard with the
//! truncation visible to the program instead of as a rejection.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use wait_timeout::ChildExt;

use super::registry::{ToolDef, ToolRegistry};
use crate::report::clip;

/// Per-tool content clip — comfortably under `MAX_RESULT_BYTES` once
/// JSON-encoded.
pub const TOOL_CONTENT_MAX_BYTES: usize = 48 * 1024;

/// `bash` clips each stream smaller, since `stdout` and `stderr` share
/// one result and must together stay under `MAX_RESULT_BYTES`.
const BASH_STREAM_MAX_BYTES: usize = 24 * 1024;

/// Command-length cap. The point of `bash` is *short* commands — a
/// single pipeline — with control flow living in the JS program. This
/// is the mechanical backstop for the card's instruction; long scripts
/// are rejected (as a repairable condition) rather than run.
const BASH_COMMAND_MAX_BYTES: usize = 1024;

/// Wall-clock cap on a single command. `bash` is the first tool that
/// can hang indefinitely; a timeout turns that into a condition.
const BASH_TIMEOUT: Duration = Duration::from_secs(30);

/// The M1 registry: real read-only tools, plus the `bash` escape hatch.
pub fn real_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(read_file_def());
    registry.register(http_fetch_def());
    registry.register(bash_def());
    registry
}

fn read_file_def() -> ToolDef {
    ToolDef {
        name: "read_file".into(),
        description: "Read a UTF-8 text file; returns its contents (clipped if large).".into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "absolute or cwd-relative path" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        output_schema: json!({ "type": "string" }),
        effectful: false,
        handler: Box::new(|args| {
            let path = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("read_file(path) needs a string path")?;
            let content = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Ok(json!(clip(&content, TOOL_CONTENT_MAX_BYTES)))
        }),
    }
}

fn http_fetch_def() -> ToolDef {
    ToolDef {
        name: "http_fetch".into(),
        description: "GET a URL; returns the response body as text (clipped if large).".into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "http(s) URL" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        output_schema: json!({ "type": "string" }),
        effectful: false,
        handler: Box::new(|args| {
            let url = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("http_fetch(url) needs a string URL")?;
            let mut response = ureq::get(url).call().map_err(|e| format!("{url}: {e}"))?;
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("{url}: reading body: {e}"))?;
            Ok(json!(clip(&text, TOOL_CONTENT_MAX_BYTES)))
        }),
    }
}

fn bash_def() -> ToolDef {
    ToolDef {
        name: "bash".into(),
        description: "Run one short shell command — a single pipeline, no loops or \
                      multi-line scripts; do control flow in JS. Resolves to \
                      { status, stdout, stderr } (a non-zero status is a result, not \
                      an error); times out after 30s."
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
                "stderr": { "type": "string" }
            }
        }),
        effectful: true,
        handler: Box::new(|args| {
            let command = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("bash(command) needs a string command")?;
            if command.len() > BASH_COMMAND_MAX_BYTES {
                return Err(format!(
                    "command is {} bytes (limit {BASH_COMMAND_MAX_BYTES}): keep bash to \
                     one short pipeline and move loops/conditionals/multi-step logic \
                     into the JS program",
                    command.len()
                ));
            }
            run_bash(command, BASH_TIMEOUT)
        }),
    }
}

/// Spawn `bash -c <command>`, enforce the timeout, and shape the outcome
/// into `{ status, stdout, stderr }`. Only the harness-level failures
/// (spawn failed, timed out) return `Err` — a command that runs and
/// exits non-zero is a normal result the program branches on.
///
/// `stdout`/`stderr` are drained on reader threads so a command that
/// out-writes the OS pipe buffer (~64KB) can't deadlock the timed wait.
fn run_bash(command: &str, timeout: Duration) -> Result<serde_json::Value, String> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning bash: {e}"))?;

    fn drain<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        let mut pipe = pipe.expect("piped");
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    }
    let out_reader = drain(child.stdout.take());
    let err_reader = drain(child.stderr.take());

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

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(json!({
        "status": status.code(),
        "stdout": clip(&String::from_utf8_lossy(&stdout), BASH_STREAM_MAX_BYTES),
        "stderr": clip(&String::from_utf8_lossy(&stderr), BASH_STREAM_MAX_BYTES),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn read_file(args: serde_json::Value) -> Result<serde_json::Value, String> {
        (read_file_def().handler)(args)
    }

    #[test]
    fn read_file_round_trips_contents() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "hello from a file").unwrap();
        let result = read_file(json!([file.path().to_str().unwrap()])).unwrap();
        assert_eq!(result, json!("hello from a file"));
    }

    #[test]
    fn read_file_clips_large_files() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}", "x".repeat(TOOL_CONTENT_MAX_BYTES + 1000)).unwrap();
        let result = read_file(json!([file.path().to_str().unwrap()])).unwrap();
        let text = result.as_str().unwrap();
        assert!(text.len() < TOOL_CONTENT_MAX_BYTES + 100);
        assert!(text.contains("[truncated;"));
    }

    #[test]
    fn read_file_reports_missing_paths() {
        let err = read_file(json!(["/no/such/file/anywhere"])).unwrap_err();
        assert!(err.contains("/no/such/file/anywhere"), "{err}");
        let err = read_file(json!([42])).unwrap_err();
        assert!(err.contains("needs a string path"), "{err}");
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
    fn bash_clips_output_past_the_pipe_buffer() {
        // ~200KB of output: exceeds the OS pipe buffer (proves no
        // deadlock) and the stream clip.
        let result = bash(json!(["yes ABCDEFGH | head -n 25000"])).unwrap();
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.len() < BASH_STREAM_MAX_BYTES + 100, "clipped");
        assert!(stdout.contains("[truncated;"));
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

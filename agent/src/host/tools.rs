//! Real tools (8_HARNESS M1): file read and HTTP fetch. Handlers run
//! blocking on the session loop's worker threads; contents are clipped
//! here so they pass the registry's result-size guard with the
//! truncation visible to the program instead of as a rejection.

use serde_json::json;

use super::registry::{ToolDef, ToolRegistry};
use crate::report::clip;

/// Per-tool content clip — comfortably under `MAX_RESULT_BYTES` once
/// JSON-encoded.
pub const TOOL_CONTENT_MAX_BYTES: usize = 48 * 1024;

/// The M1 registry: real read-only tools.
pub fn real_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(read_file_def());
    registry.register(http_fetch_def());
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
}

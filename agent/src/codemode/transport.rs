//! The one transport (phase 20 doc, Part A "The transport").
//!
//! DeepSeek only, chat + thinking, no prefill (the doc's "why not
//! prefill" / "why not raw completion" bullets record the rejected
//! alternatives and why — see `docs/20_CODE_MODE.md`). [`request_body`]
//! builds the request as a pure function of a [`Document`], tested on
//! string fixtures with no network. [`complete`] is the one thing in
//! this module that touches the network — a real HTTP POST and SSE
//! read against the OpenCode/DeepSeek endpoint. It does not reuse
//! `host/deepseek.rs`'s `parse_sse`: that function's shape is tied to
//! `LlmTurn`'s tool-call accumulation, a concept code mode has no use
//! for (no `tools` array is ever sent, so the model has no way to
//! emit a tool-call delta) — duplicating the ~30 lines that matter
//! here keeps this module's only dependency on the rest of the crate
//! being the types it already needs, not a private function reached
//! into across the phase boundary. Part H's opt-in live-model harness
//! is what actually calls this outside of manual/example use — that
//! harness itself is not built here.

use std::io::BufRead;

use serde_json::json;

use super::document::{ChatRole, Document};

/// `deepseek-v4-pro` / `deepseek-v4-flash` (Step A1: "there is no
/// separate reasoner model"). Left to the caller rather than fixed
/// here — this module owns request *shape*, not model selection.
pub struct DeepSeekCodeModeRequest<'a> {
    pub model: &'a str,
    /// Generous on purpose (Step A1): must cover reasoning tokens
    /// *plus* a long program, since a truncated completion is a
    /// condition, not silent data loss, and a tight ceiling just
    /// makes that condition fire more.
    pub max_tokens: u32,
}

/// Build the DeepSeek chat-completions body for one code-mode
/// request. `doc.messages[0]` is the card (`System`); everything after
/// it alternates `User`/`Assistant` (Part B's role-delimited layout).
///
/// Deliberately absent, each for a reason recorded in the doc:
/// - **no `tools` array** — there is no function-calling schema in
///   code mode; the model's whole answer is chat content, not a tool
///   call (Step C1: "No `tools.` namespace").
/// - **no `stop` sequences** — end-of-turn handles stopping on this
///   transport; a stop sequence keyed on a fence is only needed by
///   the (unbuilt) raw-completion variant, which has no fence to key
///   on either.
/// - **no prefill** — no trailing `assistant` message with `prefix:
///   true`. See the doc's "why not prefill".
pub fn request_body(doc: &Document, req: &DeepSeekCodeModeRequest) -> serde_json::Value {
    let system = doc
        .messages
        .first()
        .filter(|m| m.role == ChatRole::System)
        .map(|m| m.content.as_str())
        .unwrap_or_default();
    let messages: Vec<serde_json::Value> = std::iter::once(json!({
        "role": "system",
        "content": system,
    }))
    .chain(doc.messages.iter().skip(1).map(|m| {
        let role = match m.role {
            ChatRole::System => "system", // unreachable past index 0; kept exhaustive
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };
        json!({ "role": role, "content": m.content })
    }))
    .collect();

    json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "max_tokens": req.max_tokens,
        "thinking": { "type": "enabled", "reasoning_effort": "max" },
    })
}

/// Everything a real completion tells the harness. `finish_reason` is
/// how a truncated completion is told apart from thinking that simply
/// finished: `"length"` means `max_tokens` was hit before the model
/// stopped on its own, which Step A1 says is a condition to raise, not
/// a crash — and, since reasoning tokens count against the same
/// budget, `finish_reason == "length"` with `thinking` non-empty but
/// `text` empty is "ran out while thinking", the two needing different
/// fixes (lower `reasoning_effort` vs. write a shorter program).
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    pub text: String,
    pub thinking: Option<String>,
    pub finish_reason: Option<String>,
}

impl Completion {
    /// `max_tokens` was hit before the model stopped on its own — the
    /// no-fence rule's stop-marker job on this transport (Step A1: "a
    /// runaway completion must be truncated rather than parsed as
    /// program source, and truncation is a condition").
    pub fn was_truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

/// Everything needed to place one live call, deliberately not reading
/// the environment itself — `DEEPSEEK_API_KEY`/`DEEPSEEK_BASE_URL` are
/// `host/deepseek.rs`'s own convention (`DeepSeekClient::from_env`);
/// callers here (Part H's harness, or a manual probe) read the
/// environment themselves and construct one of these, so this module
/// stays a pure function of its arguments and is not implicitly
/// coupled to how the rest of the crate names its env vars.
pub struct Endpoint<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    /// Required by the OpenCode/DeepSeek endpoint — a request missing
    /// `x-opencode-session` is refused outright (`MissingSessionID`),
    /// found by actually calling it rather than assumed from reading
    /// `host/deepseek.rs`'s own doc comment on this same requirement
    /// (which this module deliberately does not import code from —
    /// its *behavior* still had to be matched independently). Stable
    /// across calls in one conversation for routing and prompt-cache
    /// affinity (`host/deepseek.rs`'s doc comment, and the endpoint's
    /// own docs: <https://opencode.ai/docs/go/#where-can-i-use-it>);
    /// a caller making unrelated one-off calls can mint a fresh one
    /// each time.
    pub session_id: &'a str,
}

/// Send one code-mode request and return the completion. The only
/// function in this module that touches the network — everything else
/// is a pure function tested on fixtures. Errors are stringly, matching
/// `LlmClient::complete`'s own convention (`host/llm.rs`) — this is
/// deliberately not `LlmClient` itself, since that trait's `chunk`
/// callback and `Cancel` token exist for a live session's needs
/// (streaming to a UI, interruption) that a probe or a batch harness
/// does not have.
pub fn complete(
    doc: &Document,
    req: &DeepSeekCodeModeRequest,
    endpoint: &Endpoint,
) -> Result<Completion, String> {
    let body = request_body(doc, req);
    let url = format!(
        "{}/chat/completions",
        endpoint.base_url.trim_end_matches('/')
    );
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", endpoint.api_key))
        .header("User-Agent", "agent2-codemode-probe/0.1")
        .header("x-opencode-session", endpoint.session_id)
        .send_json(&body)
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|_| "(unreadable body)".into());
        return Err(format!("http {status}: {text}"));
    }
    let reader = std::io::BufReader::new(response.body_mut().as_reader());
    parse_sse(reader)
}

/// Accumulate `content`/`reasoning_content` deltas from an SSE stream
/// into one [`Completion`]. No tool-call accumulation — code mode
/// never sends a `tools` array, so the model has no shape to emit one
/// in, and this parser has nothing to do with that concept at all.
fn parse_sse(reader: impl BufRead) -> Result<Completion, String> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut finish_reason = None;

    for line in reader.lines() {
        let line = line.map_err(|e| format!("stream read failed: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else {
            continue; // empty keep-alive lines, comments
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let event: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("bad SSE chunk: {e}: {data}"))?;
        if let Some(err) = event.get("error") {
            return Err(format!("stream error: {err}"));
        }
        let choice = &event["choices"][0];
        if let Some(r) = choice["finish_reason"].as_str() {
            finish_reason = Some(r.to_owned());
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["reasoning_content"].as_str() {
            thinking.push_str(t);
        }
        if let Some(t) = delta["content"].as_str() {
            text.push_str(t);
        }
    }

    Ok(Completion {
        text,
        thinking: (!thinking.is_empty()).then_some(thinking),
        finish_reason,
    })
}

#[cfg(test)]
mod sse_tests {
    use super::*;

    // Same fixture shape as `host/deepseek.rs`'s own `sse()` helper —
    // deliberately not shared across the phase boundary (see this
    // module's doc), so it is reproduced rather than imported.
    fn sse(events: &[&str]) -> String {
        let mut out = String::new();
        for e in events {
            out.push_str("data: ");
            out.push_str(e);
            out.push_str("\n\n");
        }
        out.push_str("data: [DONE]\n\n");
        out
    }

    #[test]
    fn accumulates_text_and_thinking_across_chunks() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"reasoning_content":"let me "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
            r#"{"choices":[{"delta":{"content":"say('hi'); "}}]}"#,
            r#"{"choices":[{"delta":{"content":"return 1;"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let completion = parse_sse(stream.as_bytes()).unwrap();
        assert_eq!(completion.text, "say('hi'); return 1;");
        assert_eq!(completion.thinking.as_deref(), Some("let me think"));
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert!(!completion.was_truncated());
    }

    #[test]
    fn no_thinking_content_is_none_not_empty_string() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"content":"1;"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let completion = parse_sse(stream.as_bytes()).unwrap();
        assert_eq!(completion.thinking, None);
    }

    #[test]
    fn finish_reason_length_is_a_truncated_completion() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"content":"const x = "}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        let completion = parse_sse(stream.as_bytes()).unwrap();
        assert!(completion.was_truncated());
    }

    #[test]
    fn a_stream_error_event_is_reported() {
        let stream = "data: {\"error\":{\"message\":\"rate limited\"}}\n\n";
        assert!(parse_sse(stream.as_bytes()).is_err());
    }

    #[test]
    fn keepalive_and_comment_lines_are_ignored() {
        let stream = ": keepalive\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"1;\"}}]}\n\ndata: [DONE]\n\n";
        let completion = parse_sse(stream.as_bytes()).unwrap();
        assert_eq!(completion.text, "1;");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::document;
    use crate::codemode::entry::{Entry, ProgramOutcome};
    use crate::types::EventId;

    fn sample_document() -> Document {
        let log = vec![
            (
                EventId::new(1),
                Entry::Message {
                    from: "robin".into(),
                    text: "go".into(),
                },
            ),
            (
                EventId::new(2),
                Entry::Program {
                    source: "1;".into(),
                    outcome: ProgramOutcome::Completed,
                },
            ),
        ];
        document::render("CARD", &log).unwrap()
    }

    #[test]
    fn system_role_carries_the_card() {
        let body = request_body(
            &sample_document(),
            &DeepSeekCodeModeRequest {
                model: "deepseek-v4-pro",
                max_tokens: 32_000,
            },
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "CARD");
    }

    #[test]
    fn roles_alternate_and_end_on_the_open_user_turn() {
        // system, then user/assistant alternating, ending on `user`
        // — the turn a real request is always sent to get answered
        // (Step B1: "what triggers a completion").
        let body = request_body(
            &sample_document(),
            &DeepSeekCodeModeRequest {
                model: "deepseek-v4-pro",
                max_tokens: 32_000,
            },
        );
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
    }

    #[test]
    fn thinking_is_enabled_at_max_effort() {
        let body = request_body(
            &sample_document(),
            &DeepSeekCodeModeRequest {
                model: "deepseek-v4-pro",
                max_tokens: 32_000,
            },
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["reasoning_effort"], "max");
    }

    #[test]
    fn no_tools_array_and_no_prefix_flag_anywhere() {
        let body = request_body(
            &sample_document(),
            &DeepSeekCodeModeRequest {
                model: "deepseek-v4-pro",
                max_tokens: 32_000,
            },
        );
        assert!(body.get("tools").is_none());
        for m in body["messages"].as_array().unwrap() {
            assert!(m.get("prefix").is_none());
        }
    }

    #[test]
    fn max_tokens_is_sent_verbatim() {
        let body = request_body(
            &sample_document(),
            &DeepSeekCodeModeRequest {
                model: "deepseek-v4-pro",
                max_tokens: 12_345,
            },
        );
        assert_eq!(body["max_tokens"], 12_345);
    }
}

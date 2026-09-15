//! DeepSeek client (8_HARNESS M1): the real `LlmClient`, blocking
//! `ureq` + SSE on the session loop's LLM worker thread.
//!
//! DeepSeek speaks the OpenAI chat-completions format natively, so the
//! wire mapping is: `Document.messages` → role objects (`System`/
//! `User`/`Assistant`, one each per `ChatRole`), streamed deltas → the
//! chunk callback (`reasoning_content` → `Thinking`, `content` → `Text`).
//! There is no `tools` array in the request under code mode
//! (23_ONE_AGENT's substitution table: the tool list a request used to
//! carry on every call is gone; the card is the surface) and no tool-call
//! argument
//! accumulation in the response — the model's whole reply is program
//! text, accumulated the same way `content` always was. The request
//! builder and SSE parser are pure functions — unit tests run on string
//! fixtures, never the network.

use std::io::BufRead;

use crate::document::{ChatMessage, ChatRole, Document};
use crate::host::llm::{Cancel, LlmChunk, LlmClient};
use crate::machine::LlmTurn;

const DEFAULT_MODEL: &str = "deepseek-v4-flash";
const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

pub struct DeepSeekClient {
    api_key: String,
    model: String,
    base_url: String,
    thinking: bool,
    agent: ureq::Agent,
    // Stable for the client's lifetime (one per session): the "OpenCode
    // Go" endpoint requires `x-opencode-session` to route a conversation
    // consistently and enable prompt caching — see
    // https://opencode.ai/docs/go/#where-can-i-use-it.
    session_id: String,
}

impl DeepSeekClient {
    /// Key from `DEEPSEEK_API_KEY` (required), model from
    /// `DEEPSEEK_MODEL`, base URL from `DEEPSEEK_BASE_URL`. Thinking is on
    /// by default (the API's own default); set `DEEPSEEK_NO_THINKING` (to
    /// any value) to send `"thinking": {"type": "disabled"}`.
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| "DEEPSEEK_API_KEY is not set".to_owned())?;
        let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let base_url =
            std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let thinking = std::env::var("DEEPSEEK_NO_THINKING").is_err();
        Ok(Self::new(api_key, model, base_url, thinking))
    }

    pub fn new(api_key: String, model: String, base_url: String, thinking: bool) -> Self {
        // Completions stream for minutes: connect gets a timeout, the
        // body read deliberately does not.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(std::time::Duration::from_secs(15)))
            .build();
        DeepSeekClient {
            api_key,
            model,
            base_url,
            thinking,
            agent: config.into(),
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

impl LlmClient for DeepSeekClient {
    fn complete(
        &self,
        request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = request_body(request, &self.model, self.thinking);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .header("User-Agent", "agent2/0.1")
            .header("x-opencode-session", &self.session_id)
            .send_json(&body)
            .map_err(|e| format!("deepseek request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|_| "(unreadable body)".into());
            return Err(format!("deepseek http {status}: {text}"));
        }
        let reader = std::io::BufReader::new(response.body_mut().as_reader());
        parse_sse(reader, cancel, chunk)
    }
}

/// The chat-completions request body (OpenAI format, `stream: true`).
/// Assistant `thinking` is never sent back: DeepSeek requires
/// `reasoning_content` to be excluded from the next-turn context.
///
/// **No `tools` array.** Code mode never offers a function-calling
/// schema — the model's whole response is the program, not a call into
/// one of a menu of functions — so there is nothing here to build one
/// from, unlike the pre-23 wire format this replaced.
fn request_body(request: &Document, model: &str, thinking: bool) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_json).collect();
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if !thinking {
        body["thinking"] = serde_json::json!({ "type": "disabled" });
    }
    body
}

/// Each `ChatMessage` maps to exactly one API role **by its `ChatRole`**,
/// never by a flag. `Document.messages[0]` is always `System` (the
/// snapshotted card + charter, `document::render`'s own invariant); every
/// `Assistant` message is one program's bare `source`, no tool-call
/// wrapper; every `User` message is a post (or the harness's own report,
/// which renders as one) — there is no `Tool`-role message left to emit,
/// because there is no separate tool-result channel under code mode.
fn message_json(message: &ChatMessage) -> serde_json::Value {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    };
    serde_json::json!({ "role": role, "content": message.content })
}

#[derive(Default)]
struct Accumulated {
    source: String,
    thinking: String,
    /// `"length"` once seen — DeepSeek's own signal that `max_tokens`
    /// was hit before the model stopped on its own. Anything else
    /// (`"stop"`, `"tool_calls"` — never sent since there is no `tools`
    /// array to trigger it, a stray if the API sends it anyway) is an
    /// ordinary, complete turn.
    finish_reason: Option<String>,
}

/// Parse a chat-completions SSE stream into the final assistant turn,
/// forwarding deltas to `chunk` as they arrive.
///
/// **Truncation detection** (23_ONE_AGENT A5, porting the deleted POC's
/// `Completion::was_truncated` — `git show 690561d:agent/src/codemode/
/// transport.rs`): `finish_reason == "length"` means the completion
/// ended because the token budget ran out, not because the model chose
/// to stop — the resulting `source` may be a program cut off mid-token,
/// which **must never reach the compiler** (`types.rs`'s
/// `Cause::Truncated` doc: it might still parse and run, half-written,
/// which is strictly worse than a clean failure the repair loop can see
/// and retry). This function only *detects* the condition and reports it
/// on `LlmTurn.truncated`; enforcing "never compile" is the caller's
/// job, all the way up through the session loop to whichever layer logs
/// `Cause::Truncated` — this parser has no compiler to withhold the text
/// from.
fn parse_sse(
    reader: impl BufRead,
    cancel: &Cancel,
    chunk: &mut dyn FnMut(LlmChunk),
) -> Result<LlmTurn, String> {
    let mut acc = Accumulated::default();

    for line in reader.lines() {
        // The one place a cancellation lands: between SSE lines, so an
        // interrupted generation stops streaming within a chunk rather
        // than at the end of a completion that may run for minutes.
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
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
            return Err(format!("deepseek stream error: {err}"));
        }
        let choice = &event["choices"][0];
        if let Some(r) = choice["finish_reason"].as_str() {
            acc.finish_reason = Some(r.to_owned());
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["reasoning_content"].as_str()
            && !t.is_empty()
        {
            acc.thinking.push_str(t);
            chunk(LlmChunk::Thinking(t.to_owned()));
        }
        if let Some(t) = delta["content"].as_str()
            && !t.is_empty()
        {
            acc.source.push_str(t);
            chunk(LlmChunk::Text(t.to_owned()));
        }
    }

    Ok(LlmTurn {
        source: acc.source,
        thinking: (!acc.thinking.is_empty()).then_some(acc.thinking),
        truncated: acc.finish_reason.as_deref() == Some("length"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(messages: Vec<ChatMessage>) -> Document {
        Document { messages }
    }

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.into(),
        }
    }

    #[test]
    fn request_body_maps_messages_and_omits_tools() {
        let request = doc(vec![
            msg(ChatRole::System, "card"),
            msg(ChatRole::User, "go"),
            msg(ChatRole::Assistant, "return 1;"),
            msg(ChatRole::User, "## program completed"),
        ]);
        let body = request_body(&request, "deepseek-v4-pro", true);

        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["stream"], true);
        // Thinking on is the API's own default: no field sent at all.
        assert!(body.get("thinking").is_none());
        // No function-calling schema under code mode — there is no
        // longer a `tools` array to send at all.
        assert!(body.get("tools").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "card");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "return 1;");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "## program completed");
    }

    #[test]
    fn request_body_disables_thinking_on_request() {
        let request = doc(vec![]);
        let body = request_body(&request, "deepseek-v4-flash", false);
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
    }

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
    fn parse_sse_accumulates_text_and_thinking() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"role":"assistant","reasoning_content":"let me "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
            r#"{"choices":[{"delta":{"content":"const x = "}}]}"#,
            r#"{"choices":[{"delta":{"content":"42;"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let mut chunks = Vec::new();
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |c| {
            chunks.push(match c {
                LlmChunk::Text(t) => format!("T:{t}"),
                LlmChunk::Thinking(t) => format!("R:{t}"),
            });
        })
        .unwrap();

        assert_eq!(turn.source, "const x = 42;");
        assert_eq!(turn.thinking.as_deref(), Some("let me think"));
        assert!(!turn.truncated);
        assert_eq!(chunks, ["R:let me ", "R:think", "T:const x = ", "T:42;"]);
    }

    #[test]
    fn finish_reason_length_marks_the_turn_truncated() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"content":"const x = "}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}).unwrap();
        assert!(
            turn.truncated,
            "max_tokens was hit before the model stopped"
        );
        // The cut-off text still comes back — detection, not
        // suppression: the caller decides what "never compile" means.
        assert_eq!(turn.source, "const x = ");
    }

    #[test]
    fn an_ordinary_stop_is_not_truncated() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"content":"1;"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}).unwrap();
        assert!(!turn.truncated);
    }

    #[test]
    fn parse_sse_surfaces_stream_errors() {
        let stream = "data: {\"error\":{\"message\":\"rate limited\"}}\n\n";
        let err = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(err.contains("rate limited"), "{err}");
    }
}

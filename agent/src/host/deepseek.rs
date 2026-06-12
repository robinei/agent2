//! DeepSeek client (8_HARNESS M1): the real `LlmClient`, blocking
//! `ureq` + SSE on the session loop's LLM worker thread.
//!
//! DeepSeek speaks the OpenAI chat-completions format natively, so the
//! wire mapping is: `Message` → role objects, `ToolSpec` → function
//! tools, streamed deltas → the chunk callback (`reasoning_content` →
//! `Thinking`, `content` → `Text`), tool-call argument fragments
//! accumulated by index. The request builder and SSE parser are pure
//! functions — unit tests run on string fixtures, never the network.

use std::io::BufRead;

use crate::host::llm::{LlmChunk, LlmClient};
use crate::machine::LlmRequest;
use crate::types::{Message, ToolCall};

const DEFAULT_MODEL: &str = "deepseek-v4-pro";
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

pub struct DeepSeekClient {
    api_key: String,
    model: String,
    base_url: String,
    agent: ureq::Agent,
}

impl DeepSeekClient {
    /// Key from `DEEPSEEK_API_KEY` (required), model from
    /// `DEEPSEEK_MODEL`, base URL from `DEEPSEEK_BASE_URL`.
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| "DEEPSEEK_API_KEY is not set".to_owned())?;
        let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let base_url =
            std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        Ok(Self::new(api_key, model, base_url))
    }

    pub fn new(api_key: String, model: String, base_url: String) -> Self {
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
            agent: config.into(),
        }
    }
}

impl LlmClient for DeepSeekClient {
    fn complete(
        &mut self,
        request: &LlmRequest,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<Message, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = request_body(request, &self.model);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.api_key))
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
        parse_sse(reader, chunk)
    }
}

/// The chat-completions request body (OpenAI format, `stream: true`).
/// Assistant `thinking` is never sent back: DeepSeek requires
/// `reasoning_content` to be excluded from the next-turn context.
fn request_body(request: &LlmRequest, model: &str) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_json).collect();
    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect();
    serde_json::json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "stream": true,
    })
}

fn message_json(message: &Message) -> serde_json::Value {
    match message {
        Message::System { text } => serde_json::json!({ "role": "system", "content": text }),
        Message::User { text } => serde_json::json!({ "role": "user", "content": text }),
        Message::Tool { call_id, text, .. } => serde_json::json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": text,
        }),
        Message::Assistant {
            text, tool_calls, ..
        } => {
            let mut obj = serde_json::json!({ "role": "assistant", "content": text });
            if !tool_calls.is_empty() {
                obj["tool_calls"] = tool_calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                // OpenAI format: arguments are a JSON *string*.
                                "arguments": c.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
            }
            obj
        }
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Parse a chat-completions SSE stream into the final assistant
/// message, forwarding deltas to `chunk` as they arrive.
fn parse_sse(reader: impl BufRead, chunk: &mut dyn FnMut(LlmChunk)) -> Result<Message, String> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: Vec<PartialCall> = Vec::new();

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
            return Err(format!("deepseek stream error: {err}"));
        }
        let delta = &event["choices"][0]["delta"];
        if let Some(t) = delta["reasoning_content"].as_str()
            && !t.is_empty()
        {
            thinking.push_str(t);
            chunk(LlmChunk::Thinking(t.to_owned()));
        }
        if let Some(t) = delta["content"].as_str()
            && !t.is_empty()
        {
            text.push_str(t);
            chunk(LlmChunk::Text(t.to_owned()));
        }
        if let Some(deltas) = delta["tool_calls"].as_array() {
            for tc in deltas {
                let index = tc["index"].as_u64().unwrap_or(0) as usize;
                while calls.len() <= index {
                    calls.push(PartialCall::default());
                }
                let call = &mut calls[index];
                if let Some(id) = tc["id"].as_str() {
                    call.id.push_str(id);
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    call.arguments.push_str(args);
                }
            }
        }
    }

    let tool_calls = calls
        .into_iter()
        .filter(|c| !c.name.is_empty())
        .map(|c| {
            let arguments = if c.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&c.arguments).map_err(|e| {
                    format!("tool call `{}` arguments are not valid JSON: {e}", c.name)
                })?
            };
            Ok(ToolCall {
                id: c.id,
                name: c.name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    Ok(Message::Assistant {
        text,
        thinking: (!thinking.is_empty()).then_some(thinking),
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{resume_spec, run_program_spec};
    use serde_json::json;

    #[test]
    fn request_body_maps_messages_and_tools() {
        let request = LlmRequest {
            messages: vec![
                Message::System {
                    text: "card".into(),
                },
                Message::User { text: "go".into() },
                Message::Assistant {
                    text: String::new(),
                    thinking: Some("hidden".into()),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "run_program".into(),
                        arguments: json!({ "source": "return 1;" }),
                    }],
                },
                Message::Tool {
                    name: "run_program".into(),
                    call_id: "c1".into(),
                    text: "## program completed".into(),
                },
            ],
            tools: vec![resume_spec(), run_program_spec()],
        };
        let body = request_body(&request, "deepseek-v4-pro");

        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["stream"], true);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        // Assistant: tool-call arguments are a JSON string; thinking
        // (reasoning_content) is never sent back.
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"source":"return 1;"}"#
        );
        assert!(messages[2].get("reasoning_content").is_none());
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "c1");
        // Tools: full definitions, OpenAI function shape.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["function"]["name"], "resume");
        assert_eq!(tools[1]["function"]["name"], "run_program");
        assert!(tools[1]["function"]["parameters"]["properties"]["source"].is_object());
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
            r#"{"choices":[{"delta":{"content":"the answer "}}]}"#,
            r#"{"choices":[{"delta":{"content":"is 42"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let mut chunks = Vec::new();
        let message = parse_sse(stream.as_bytes(), &mut |c| {
            chunks.push(match c {
                LlmChunk::Text(t) => format!("T:{t}"),
                LlmChunk::Thinking(t) => format!("R:{t}"),
            });
        })
        .unwrap();

        let Message::Assistant {
            text,
            thinking,
            tool_calls,
        } = message
        else {
            panic!("not an assistant message");
        };
        assert_eq!(text, "the answer is 42");
        assert_eq!(thinking.as_deref(), Some("let me think"));
        assert!(tool_calls.is_empty());
        assert_eq!(chunks, ["R:let me ", "R:think", "T:the answer ", "T:is 42"]);
    }

    #[test]
    fn parse_sse_reassembles_tool_call_fragments() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"run_program","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"source\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"return 6*7;\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let message = parse_sse(stream.as_bytes(), &mut |_| {}).unwrap();

        let Message::Assistant { tool_calls, .. } = message else {
            panic!("not an assistant message");
        };
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].name, "run_program");
        assert_eq!(tool_calls[0].arguments, json!({ "source": "return 6*7;" }));
    }

    #[test]
    fn parse_sse_surfaces_stream_errors() {
        let stream = "data: {\"error\":{\"message\":\"rate limited\"}}\n\n";
        let err = parse_sse(stream.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.contains("rate limited"), "{err}");
    }
}

//! **The Responses API**, which is how OpenAI's `*-codex` models are
//! reached and the only way they can be.
//!
//! A different wire format, not a dialect of the other one. Chat
//! completions takes `messages` and streams `choices[].delta`;
//! this takes typed `input` items and streams named events. So it is
//! its own [`LlmClient`], chosen by `host::provider`, and nothing above
//! that module can tell which one it holds.
//!
//! **The notebook is the easy case here.** Responses defines items for
//! `message`, `reasoning`, `function_call` and `function_call_output`,
//! and a tool-calling agent has to keep the last two paired up by id.
//! A notebook reply is text in an assistant turn and a report is text
//! in a user turn, so only `message` / `input_text` / `output_text` are
//! ever built. The pairing that cost this project two bugs on the
//! completions side does not arise.
//!
//! **Untested against a live endpoint.** Written from the API's shape
//! with no credits to spend, so what is asserted here is the body sent
//! and the events parsed, not that the server accepts them. Two things
//! are therefore deliberately lenient: any event whose type ends in
//! `.delta` contributes its `delta` field, routed by whether the type
//! mentions reasoning, and usage is read from whichever of the two
//! spellings arrives. A strict parser written blind fails closed on a
//! name I guessed wrong, and the failure would look like the model
//! saying nothing.

use std::io::BufRead;
use std::time::Duration;

use super::Usage;
use super::llm::{Cancel, LlmChunk, LlmClient};
use super::provider::Config;
use crate::document::{ChatMessage, ChatRole, Document};
use crate::machine::LlmTurn;

/// How long to wait for the first byte, and then between bytes. A
/// reasoning model can think for minutes before it says anything, so
/// the bound that matters is *idle*, not total.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub struct OpenAiResponses {
    api_key: String,
    model: String,
    base_url: String,
    thinking: bool,
    effort: Option<String>,
    max_tokens: Option<u32>,
    /// **One id for this process, sent on every request.**
    ///
    /// The backend routes by it, and a prompt cache is only a cache if
    /// requests keep landing on the same machine. Measured on
    /// 2026-09-22: without these headers, six identical 7.3k-token
    /// requests hit once, at 47%. With them, five of six hit at 98%.
    session_id: String,
    /// From the token's own claims. Part of the same routing, not of
    /// authorisation — the request is accepted without it.
    account_id: Option<String>,
    agent: ureq::Agent,
}

impl OpenAiResponses {
    pub fn from_config(config: &Config) -> Self {
        Self {
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            thinking: config.thinking,
            effort: config.effort.clone(),
            max_tokens: config.max_tokens,
            session_id: uuid::Uuid::new_v4().to_string(),
            account_id: super::openai_oauth::account_id(),
            agent: ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_recv_body(Some(IDLE_TIMEOUT))
                .build()
                .into(),
        }
    }
}

/// One `input` item per message.
///
/// The card goes in `developer`, which is what the newer models call
/// the role that used to be `system`; `input_text` is what a user turn
/// carries and `output_text` what an assistant turn carries, and the
/// two are not interchangeable.
fn input_item(message: &ChatMessage) -> serde_json::Value {
    let (role, kind) = match message.role {
        ChatRole::System => ("developer", "input_text"),
        ChatRole::User => ("user", "input_text"),
        ChatRole::Assistant => ("assistant", "output_text"),
    };
    serde_json::json!({
        "type": "message",
        "role": role,
        "content": [{ "type": kind, "text": message.content }],
    })
}

pub(crate) fn request_body(
    request: &Document,
    model: &str,
    thinking: bool,
    effort: Option<&str>,
    max_tokens: Option<u32>,
) -> serde_json::Value {
    let input: Vec<serde_json::Value> = request.messages.iter().map(input_item).collect();
    let mut body = serde_json::json!({
        "model": model,
        "input": input,
        "stream": true,
        // **Stateless.** The document is re-rendered from the log every
        // turn — that is the whole design — so there is no server-side
        // conversation to continue and nothing to store. Prefix caching
        // still applies; `previous_response_id` would be a second
        // source of truth for what the model has seen, and this project
        // has exactly one.
        "store": false,
    });
    if thinking && let Some(effort) = effort {
        // **A summary must be asked for, or no reasoning comes back at
        // all.** Raw chain-of-thought is not returned by these models —
        // normal for a frontier reasoning model — but with `summary`
        // the stream carries `response.reasoning_summary_text.delta`
        // and `Part::Thinking` is populated. Without it the log records
        // no reasoning, which costs this project the one thing it has
        // been measuring all week and quietly empties the corpus the
        // logs are meant to become.
        //
        // A summary is a paraphrase, not the stream: counting drafted
        // blocks in it is a weaker measurement than counting them in
        // raw reasoning, and anything derived from it should say so.
        let summary =
            super::provider::var("REASONING_SUMMARY").unwrap_or_else(|| "auto".to_owned());
        body["reasoning"] = if summary == "off" {
            serde_json::json!({ "effort": effort })
        } else {
            serde_json::json!({ "effort": effort, "summary": summary })
        };
    }
    if let Some(n) = max_tokens {
        body["max_output_tokens"] = serde_json::json!(n);
    }
    body
}

/// Parse the event stream into the finished turn, forwarding deltas as
/// they arrive.
///
/// **What is streamed must reconstruct what is returned** — the session
/// feeds the notebook from `chunk` and never reads the value this
/// returns (`host/mod.rs`'s `on_chunk`). A transport that assembles its
/// reply only in the return value delivers nothing runnable, which cost
/// three A/B runs on the completions side before anyone noticed.
pub(crate) fn parse_events(
    reader: impl BufRead,
    cancel: &Cancel,
    chunk: &mut dyn FnMut(LlmChunk),
) -> Result<LlmTurn, String> {
    let mut source = String::new();
    let mut thinking = String::new();
    let mut usage = Usage::default();
    let mut incomplete = false;
    let mut ended = false;
    // Which output item the last text delta belonged to.
    let mut item: Option<u64> = None;

    for line in reader.lines() {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let line = line.map_err(|e| format!("stream read failed: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else {
            continue; // `event:` lines and blank separators
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let event: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("bad event: {e}: {data}"))?;
        let kind = event["type"].as_str().unwrap_or_default();

        if kind == "error" || kind == "response.failed" {
            let msg = event["message"]
                .as_str()
                .or_else(|| event["response"]["error"]["message"].as_str())
                .unwrap_or("unknown error");
            return Err(format!("responses stream error: {msg}"));
        }
        // Lenient by design — see the module note. Reasoning summaries
        // arrive under more than one name and are not the program.
        if kind.ends_with(".delta")
            && let Some(t) = event["delta"].as_str()
            && !t.is_empty()
        {
            if kind.contains("reasoning") {
                thinking.push_str(t);
                chunk(LlmChunk::Thinking(t.to_owned()));
            } else if kind.contains("output_text") {
                // **A new output item starts a new line.**
                //
                // A turn can hold several `message` items and their
                // text is concatenated here. Joined with nothing, the
                // seam lands mid-line: `sweep-8` on 2026-09-22 produced
                // `…after the edit.```js` and the fence, no longer at
                // the start of a line, was not a fence — `split_cells`
                // saw prose, the program never ran, and the task failed
                // with the model looking to blame. Another run repeated
                // a sentence with no space between the copies.
                //
                // One newline, not two: separate items are not always
                // separate paragraphs, and a soft break is enough to
                // put a fence where a fence can be seen.
                let index = event["output_index"].as_u64();
                if item.is_some() && index != item && !source.is_empty() && !source.ends_with('\n')
                {
                    source.push('\n');
                    chunk(LlmChunk::Text("\n".to_owned()));
                }
                item = index;
                source.push_str(t);
                chunk(LlmChunk::Text(t.to_owned()));
            }
            continue;
        }
        if kind == "response.completed" || kind == "response.incomplete" {
            ended = true;
            let r = &event["response"];
            if r["status"] == "incomplete"
                && r["incomplete_details"]["reason"] == "max_output_tokens"
            {
                incomplete = true;
            }
            let u = &r["usage"];
            usage = Usage {
                prompt: u["input_tokens"].as_u64().unwrap_or(0),
                cached: u["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                completion: u["output_tokens"].as_u64().unwrap_or(0),
                reasoning: u["output_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                // The window this request went against, so the
                // log carries both halves of the measurement.
                window: crate::host::context_tokens().map(|n| n as u64),
            };
        }
    }

    // **A connection that just stops has not finished.** Same rule as
    // the completions client, and for the same reason: returning `Ok`
    // there makes a dropped connection indistinguishable from a
    // finished reply, and the model's half-sentence becomes its whole
    // turn with nothing anywhere saying otherwise.
    if !ended {
        return Err(format!(
            "stream ended without a completion event after {} bytes of reply",
            source.len()
        ));
    }
    Ok(LlmTurn {
        source,
        thinking: (!thinking.is_empty()).then_some(thinking),
        truncated: incomplete,
        usage: (usage != Usage::default()).then_some(usage),
        reply: None,
    })
}

impl LlmClient for OpenAiResponses {
    /// **No retry loop, and that is a known gap.** The completions
    /// client retries before its first text chunk because
    /// `opencode.ai/zen` returned HTTP 530 on 5 of 11 runs on
    /// 2026-09-19. This endpoint's failure modes are unmeasured — there
    /// were no credits to measure them with — and a retry policy
    /// written for failures nobody has seen is a guess that hides the
    /// first real one. Add it when a live run shows what it should be
    /// retrying.
    fn complete(
        &self,
        request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        let base = self.base_url.trim_end_matches('/');
        let url = if base.ends_with("/responses") {
            base.to_owned()
        } else {
            format!("{base}/responses")
        };
        let mut body = request_body(
            request,
            &self.model,
            self.thinking,
            self.effort.as_deref(),
            self.max_tokens,
        );
        // The same id in the body and the headers: one asks for the
        // prefix to be cached, the others ask for this request to reach
        // the machine holding it.
        body["prompt_cache_key"] = serde_json::json!(self.session_id);
        let mut req = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .header("User-Agent", "agent2/0.1")
            .header("originator", "agent2")
            .header("session_id", &self.session_id)
            .header("x-client-request-id", &self.session_id)
            .header("x-session-affinity", &self.session_id);
        if let Some(account) = &self.account_id {
            req = req.header("chatgpt-account-id", account);
        }
        let got = req
            .send_json(&body)
            .map_err(|e| format!("responses request failed: {e}"))?;
        let status = got.status();
        if !status.is_success() {
            let text = got
                .into_body()
                .read_to_string()
                .unwrap_or_else(|e| format!("<body unreadable: {e}>"));
            return Err(format!("responses http {status}: {text}"));
        }
        parse_events(
            std::io::BufReader::new(got.into_body().into_reader()),
            cancel,
            chunk,
        )
    }

    fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_owned(),
            call: None,
            result_for: None,
            thinking: None,
        }
    }

    fn doc(messages: Vec<ChatMessage>) -> Document {
        Document {
            preamble: 1,
            messages,
        }
    }

    fn sse(events: &[&str]) -> String {
        events
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<Vec<_>>()
            .concat()
    }

    /// **The three item types a notebook ever needs**, and the roles
    /// they carry. `input_text` and `output_text` are not
    /// interchangeable: a reply sent as `input_text` is the model being
    /// shown its own turn as though someone else had written it.
    #[test]
    fn a_document_becomes_message_items() {
        let body = request_body(
            &doc(vec![
                msg(ChatRole::System, "the card"),
                msg(ChatRole::User, "do it"),
                msg(ChatRole::Assistant, "```js\nreturn 1;\n```"),
                msg(ChatRole::User, "## program completed"),
            ]),
            "gpt-5.1-codex",
            true,
            Some("high"),
            Some(32000),
        );
        let input = body["input"].as_array().expect("input items");
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["content"][0]["text"], "```js\nreturn 1;\n```");
        assert_eq!(input[3]["content"][0]["type"], "input_text");
        for item in input {
            assert_eq!(item["type"], "message");
        }
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 32000);
        assert_eq!(body["store"], false, "the log is the only state");
        assert!(body.get("messages").is_none(), "that is the other API");
        assert!(body.get("tools").is_none(), "a notebook offers none");
    }

    /// **The summary is requested by default**, because without it
    /// these models return no reasoning at all and `Part::Thinking` is
    /// empty — which silently ends the measurement this project runs
    /// on. `AGENT2_REASONING_SUMMARY=off` opts out.
    #[test]
    fn a_reasoning_summary_is_asked_for_by_default() {
        let body = request_body(
            &doc(vec![msg(ChatRole::System, "card")]),
            "gpt-5.6-sol",
            true,
            Some("high"),
            None,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(
            body["reasoning"]["summary"], "auto",
            "no summary means no reasoning text comes back: {body}"
        );
    }

    /// Thinking off means no `reasoning` block at all, not one set to
    /// nothing.
    #[test]
    fn reasoning_is_absent_when_thinking_is_off() {
        let body = request_body(
            &doc(vec![msg(ChatRole::System, "card")]),
            "gpt-5",
            false,
            Some("high"),
            None,
        );
        assert!(body.get("reasoning").is_none(), "{body}");
        assert!(body.get("max_output_tokens").is_none(), "{body}");
    }

    /// **What is streamed must reconstruct what is returned.** The same
    /// contract the completions client is held to, asserted here before
    /// this transport has ever run — because that is the bug that cost
    /// three A/B runs there, and it was invisible to every other test.
    #[test]
    fn the_chunks_add_up_to_the_turn() {
        let stream = sse(&[
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"weighing it"}"#,
            r#"{"type":"response.output_text.delta","delta":"Reading them.\n\n```js\n"}"#,
            r#"{"type":"response.output_text.delta","delta":"return 1;\n```\n"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":120,"output_tokens":40,"input_tokens_details":{"cached_tokens":64},"output_tokens_details":{"reasoning_tokens":12}}}}"#,
        ]);
        let mut streamed = String::new();
        let turn = parse_events(stream.as_bytes(), &Cancel::new(), &mut |c| {
            if let LlmChunk::Text(t) = c {
                streamed.push_str(&t);
            }
        })
        .expect("parses");
        assert_eq!(streamed, turn.source);
        assert!(!crate::notebook::split_cells(&turn.source).is_empty());
        assert_eq!(turn.thinking.as_deref(), Some("weighing it"));
        assert!(!turn.truncated);
        let u = turn.usage.expect("usage");
        assert_eq!(
            (u.prompt, u.cached, u.completion, u.reasoning),
            (120, 64, 40, 12)
        );
    }

    /// **A turn's output items are joined at a line boundary.**
    ///
    /// Live on `sweep-8`, 2026-09-22: two items concatenated with
    /// nothing between them produced `…after the edit.```js`, the fence
    /// was no longer at the start of a line, `split_cells` found no
    /// cell, and the run did nothing while looking like the model's
    /// fault. The task failed for a missing newline.
    #[test]
    fn output_items_do_not_run_into_each_other() {
        let stream = sse(&[
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"Reading them first."}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"```js\nreturn 1;\n```"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{}}}"#,
        ]);
        let mut streamed = String::new();
        let turn = parse_events(stream.as_bytes(), &Cancel::new(), &mut |c| {
            if let LlmChunk::Text(t) = c {
                streamed.push_str(&t);
            }
        })
        .expect("parses");
        assert_eq!(turn.source, "Reading them first.\n```js\nreturn 1;\n```");
        assert_eq!(
            crate::notebook::split_cells(&turn.source).len(),
            1,
            "the fence was not at a line start: {:?}",
            turn.source
        );
        assert_eq!(
            streamed, turn.source,
            "the session would see something else"
        );
    }

    /// And one item's deltas are never broken up — the separator is
    /// between items, not between chunks.
    #[test]
    fn deltas_of_one_item_are_joined_verbatim() {
        let stream = sse(&[
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"half a "}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"sentence"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{}}}"#,
        ]);
        let turn = parse_events(stream.as_bytes(), &Cancel::new(), &mut |_| {}).expect("parses");
        assert_eq!(turn.source, "half a sentence");
    }

    /// Running out of output budget is a truncation, and `types.rs`
    /// says a truncated completion must never reach the compiler.
    #[test]
    fn hitting_the_output_ceiling_marks_the_turn_truncated() {
        let stream = sse(&[
            r#"{"type":"response.output_text.delta","delta":"```js\nconst x = "}"#,
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{}}}"#,
        ]);
        let turn = parse_events(stream.as_bytes(), &Cancel::new(), &mut |_| {}).expect("parses");
        assert!(turn.truncated, "the ceiling was not reported");
    }

    /// A connection that simply stops is a lost completion, not a short
    /// one.
    #[test]
    fn a_stream_that_never_completes_is_an_error() {
        let stream = sse(&[r#"{"type":"response.output_text.delta","delta":"half a "}"#]);
        let err = parse_events(stream.as_bytes(), &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(err.contains("without a completion event"), "{err}");
    }

    /// An error event is an error, not an empty reply.
    #[test]
    fn a_failure_event_is_reported() {
        let stream = sse(&[
            r#"{"type":"response.failed","response":{"error":{"message":"model not found"}}}"#,
        ]);
        let err = parse_events(stream.as_bytes(), &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(err.contains("model not found"), "{err}");
    }
}

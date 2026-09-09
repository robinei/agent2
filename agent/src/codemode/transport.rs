//! The one transport (phase 20 doc, Part A "The transport").
//!
//! DeepSeek only, chat + thinking, no prefill (the doc's "why not
//! prefill" / "why not raw completion" bullets record the rejected
//! alternatives and why — see `docs/20_CODE_MODE.md`). This builds
//! the request body as a pure function of a [`Document`] and asserts
//! its shape; it does not perform network I/O, so it stays in
//! `cargo test`'s no-network world. The actual HTTP round trip and
//! response parsing belong to Part H's opt-in, live-model harness —
//! not built here.

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

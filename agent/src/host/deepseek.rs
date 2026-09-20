//! DeepSeek client (8_HARNESS M1): the real `LlmClient`, blocking
//! `ureq` + SSE on the session loop's LLM worker thread.
//!
//! DeepSeek speaks the OpenAI chat-completions format natively, so the
//! wire mapping is: `Document.messages` → role objects (`System`/
//! `User`/`Assistant`/`Tool`, one each per `ChatRole`), streamed deltas →
//! the chunk callback (`reasoning_content` → `Thinking`, `content` →
//! `Text`). Under `Transport::Program` there is no `tools` array in the
//! request at all (23_ONE_AGENT's substitution table: the tool list a
//! request used to carry on every call is gone; the card is the surface)
//! and no tool-call argument accumulation in the response — the model's
//! whole reply is program text, accumulated the same way `content`
//! always was. Under `Transport::RunProgram` the same program instead
//! rides a single advertised `run_program` tool: the request grows a
//! `tools` array (`request_body`) and the response is read back out of
//! `delta.tool_calls[0].function.arguments` (`parse_sse`) rather than
//! `delta.content`. Either way the result handed back up is the same
//! bare `LlmTurn` — this module is the one place the two containers ever
//! differ; everything above it (`host/mod.rs`, `machine.rs`) sees one
//! shape. The request builder and SSE parser are pure functions — unit
//! tests run on string fixtures, never the network.

use std::io::BufRead;
use std::time::SystemTime;

use crate::document::{ChatMessage, ChatRole, Document};
use crate::host::llm::{Cancel, LlmChunk, LlmClient};
use crate::machine::LlmTurn;
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// Reasoning effort, sent as `reasoning_effort`. `high` because that is
/// what one would realistically run — the harness is measured in the
/// configuration it is used in, not a cheaper one chosen to make the
/// numbers move.
const DEFAULT_EFFORT: &str = "high";
const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

pub struct DeepSeekClient {
    api_key: String,
    model: String,
    base_url: String,
    thinking: bool,
    /// Pinned reasoning level, sent as `reasoning_effort`. `None` leaves
    /// the field off and lets the API choose.
    effort: Option<String>,
    max_tokens: Option<u32>,
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
    ///
    /// Reasoning effort defaults to [`DEFAULT_EFFORT`] and is overridden
    /// by `DEEPSEEK_REASONING_EFFORT` —
    /// `minimal`/`low`/`medium`/`high`/`xhigh`/`max`, sent as
    /// `reasoning_effort`.
    ///
    /// It is pinned rather than left to the API's own default because
    /// `DESIGN.md`'s M5 measures this harness against a third-party
    /// agent on the same model, and an unpinned level makes "same model"
    /// untrue in the one way that would silently explain away a
    /// difference. The wire format is the one `pi` uses for this
    /// provider — `reasoning_effort` to set a level, `thinking: {"type":
    /// "disabled"}` to turn it off, the latter already byte-identical to
    /// what `DEEPSEEK_NO_THINKING` sends — so both sides are set the
    /// same way and can be checked against each other.
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| "DEEPSEEK_API_KEY is not set".to_owned())?;
        let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let base_url =
            std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let thinking = std::env::var("DEEPSEEK_NO_THINKING").is_err();
        let max_tokens = std::env::var("DEEPSEEK_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|n| *n > 0);
        let effort = std::env::var("DEEPSEEK_REASONING_EFFORT")
            .ok()
            .or_else(|| Some(DEFAULT_EFFORT.to_owned()));
        Ok(Self::new(api_key, model, base_url, thinking)
            .with_effort(effort)
            .with_max_tokens(max_tokens))
    }

    /// Pin the reasoning level (builder form, so `new`'s signature is
    /// untouched for its existing callers).
    /// A ceiling on the completion, from `DEEPSEEK_MAX_TOKENS`.
    ///
    /// **Unset by default, and that is right for a hosted provider**,
    /// whose own ceiling is generous and whose replies legitimately run
    /// to thousands of tokens. A local server is the other case: its
    /// default can be small enough that a reasoning model spends the
    /// whole budget thinking and returns empty `content` with
    /// `finish_reason: length`. That failure is survivable — the branch
    /// is asked again with a marker saying the reply arrived empty —
    /// but surviving it every turn is not a plan, and the server's own
    /// documentation says to send this.
    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    pub fn new(api_key: String, model: String, base_url: String, thinking: bool) -> Self {
        // Completions stream for minutes, so the body read cannot be
        // held to a short deadline — but "no deadline at all" means a
        // dead socket hangs the branch forever, with the TUI showing a
        // session that is simply never going to continue. Seen after the
        // machine slept mid-request: ten minutes on a connection nothing
        // was ever coming back on.
        //
        // Two bounds, both deliberately far past anything healthy.
        // `recv_response` covers time to the first response headers,
        // which is where a dead connection sits. It was 180s, chosen
        // because a direct probe showed headers arriving in under a
        // second — and that was wrong twice in one evening: under load
        // this endpoint *queues* rather than refusing, so a busy moment
        // looks exactly like a dead socket and two real runs were killed
        // mid-task by their own client. A timeout meant to catch a
        // hardware event should never be tight enough to catch a slow
        // one.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(std::time::Duration::from_secs(15)))
            .timeout_recv_response(Some(std::time::Duration::from_secs(10 * 60)))
            .timeout_recv_body(Some(std::time::Duration::from_secs(30 * 60)))
            .build();
        DeepSeekClient {
            api_key,
            model,
            base_url,
            thinking,
            effort: None,
            max_tokens: None,
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
        // Taken off the request, not looked up again: this is the
        // container the document in hand was actually *rendered* for, so
        // the body we build and the SSE shape we expect back cannot
        // disagree with it. Both used to read `AGENT2_TRANSPORT`
        // independently and were kept in agreement only by a comment.
        let body = request_body(
            request,
            &self.model,
            self.thinking,
            self.effort.as_deref(),
            self.max_tokens,
        );
        // **Retried only before a single byte has been streamed.**
        // Everything below this loop hands chunks straight to the
        // notebook, which appends them to a reply that is already on the
        // log — so a retry *there* would write the reply twice. Here
        // nothing has been emitted yet and the attempt can simply be
        // forgotten, which is exactly the shape of the failure this is
        // for: `opencode.ai/zen` returned HTTP 530 (`Upstream response
        // was not valid JSON`) on 5 of 11 runs on 2026-09-19, always
        // before the stream opened, and the agent exited 1 having done
        // nothing. Half a suite, lost to a fault that clears on its own.
        for attempt in 1..=MAX_ATTEMPTS {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            let last = attempt == MAX_ATTEMPTS;
            // Both clocks, because the gap between them is the only way
            // to see a suspend from in here. See `slept_since`.
            let (started_mono, started_wall) = (std::time::Instant::now(), SystemTime::now());
            match self
                .agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", self.api_key))
                .header("User-Agent", "agent2/0.1")
                .header("x-opencode-session", &self.session_id)
                .send_json(&body)
            {
                Ok(mut got) => {
                    let status = got.status();
                    if status.is_success() {
                        // **The stream is read here, inside the retry.**
                        // A stream that dies before its first *text*
                        // chunk has written nothing: only text reaches
                        // `notebook_stream` (`host/mod.rs`'s `if
                        // !thinking`), so no `Reply` was opened and no
                        // part was logged. That failure is as safe to
                        // retry as one before the headers, and it is
                        // the one a slow model actually hits — a local
                        // Qwen3.6-35B lost a run to it on 2026-09-19,
                        // reasoning for a long time and then dropping
                        // the connection with nothing said.
                        //
                        // Once a text chunk *has* gone through, never:
                        // the reply is on the log and a second attempt
                        // would write it twice.
                        let mut wrote = false;
                        let reader = std::io::BufReader::new(got.body_mut().as_reader());
                        let mut counting = |c: LlmChunk| {
                            wrote |= matches!(c, LlmChunk::Text(_));
                            chunk(c);
                        };
                        match parse_sse(reader, cancel, &mut counting) {
                            Ok(turn) => return Ok(turn),
                            Err(e) if last || wrote => return Err(e),
                            Err(_) => {}
                        }
                    } else {
                        let text = got
                            .body_mut()
                            .read_to_string()
                            .unwrap_or_else(|_| "(unreadable body)".into());
                        let failed = format!("deepseek http {status}: {text}");
                        // A 4xx is the request's own fault and says so
                        // the same way however often it is asked.
                        if last || !retryable(status.as_u16()) {
                            return Err(failed);
                        }
                    }
                }
                // No status at all: refused, reset, resolved nowhere.
                // Nothing reached the model, so asking again is free of
                // doubt — **except on a timeout**, which is the one
                // transport failure that is not fast.
                //
                // `timeout_recv_response` is ten minutes on purpose
                // (see `new`): this endpoint *queues* under load rather
                // than refusing, so a busy moment looks exactly like a
                // dead socket and a tight bound killed two real runs
                // mid-task. A queued request that is retried simply
                // queues again — and three attempts at ten minutes is
                // half an hour of a branch doing nothing, where before
                // this retry existed it was ten. Retrying a timeout
                // costs the most and buys the least.
                // **A timeout is not retried — unless the machine
                // slept.** A queued request that is retried simply
                // queues again, so the generous ten-minute bound exists
                // to wait one out rather than to kill it. A socket the
                // kernel tore down during a suspend is the opposite
                // case: nothing is coming back on it, ever, and the
                // request never reached the model. That is a hardware
                // event, and the run should survive the lid closing.
                Err(ureq::Error::Timeout(which)) => {
                    if last || !slept_since(started_mono, started_wall) {
                        return Err(format!("deepseek request timed out ({which})"));
                    }
                }
                Err(e) => {
                    if last {
                        return Err(format!("deepseek request failed: {e}"));
                    }
                }
            }
            std::thread::sleep(backoff(attempt));
        }
        Err("every attempt failed".into())
    }
}

/// How many times a request is sent before the failure is the
/// caller's.
///
/// **Seven, because the doubling has to reach what the waits are
/// for.** [`backoff`] says a local server answering 503 while it loads
/// a model off disk "needs ten to twenty" seconds; five attempts
/// totals six, so it never got there, and the number said three while
/// the constant said five. Seven reaches 25.2s — 0.4, 0.8, 1.6, 3.2,
/// 6.4, 12.8 — which covers a cold model load and a proxy wobble both.
///
/// It costs nothing when nothing is wrong: no attempt after the first
/// happens unless one has already failed. Measured on 2026-09-20,
/// three eval runs were lost to an upstream 530 that outlasted six
/// seconds of retries, two of them on `plain-question`, which is one
/// request long.
const MAX_ATTEMPTS: usize = 7;

/// Whether the machine was suspended while this request was in flight.
///
/// **Two clocks disagree across a suspend, and that is the whole
/// trick.** `Instant` is `CLOCK_MONOTONIC`, which does not advance
/// while the machine is asleep; `SystemTime` is the wall clock, which
/// does. So a request that ran for three monotonic minutes and sixteen
/// wall minutes spent thirteen of them suspended — and the socket it
/// was holding did not survive that.
///
/// The threshold is loose because the question is not "how long" but
/// "did the machine stop": ordinary clock drift and NTP steps are
/// seconds, a suspend worth noticing is minutes.
fn slept_since(mono: std::time::Instant, wall: SystemTime) -> bool {
    let wall_elapsed = wall.elapsed().unwrap_or_default();
    wall_elapsed.saturating_sub(mono.elapsed()) > std::time::Duration::from_secs(60)
}

/// Whether an HTTP status is worth asking again about.
///
/// Every 5xx, which covers the proxy's own 530, plus 429 (rate
/// limited) and 408 (the server gave up waiting). Deliberately *not*
/// 4xx otherwise: a 401 or a 400 is the request's own fault and says
/// so the same way however often it is asked.
fn retryable(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// How long to wait before attempt `n + 1`: 0.4s, 0.8s, 1.6s, 3.2s,
/// 6.4s, 12.8s — 25.2s across [`MAX_ATTEMPTS`].
///
/// **Two faults with very different clocks.** A proxy's 530 clears in
/// about a second, so the first waits are short. A local server
/// answering 503 while it loads a model off disk needs ten to twenty,
/// and three fast attempts spent the whole budget in 1.2s and gave up
/// before it had finished reading the weights. Doubling covers both
/// without making the common case slow: nothing waits at all unless
/// something has already failed.
fn backoff(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_millis(400 * (1 << (attempt - 1)) as u64)
}

/// The chat-completions request body (OpenAI format, `stream: true`).
/// Assistant `thinking` is never sent back: DeepSeek requires
/// `reasoning_content` to be excluded from the next-turn context.
///
/// **`tools` under `Transport::Program`: absent.** Code mode's default
/// container never offers a function-calling schema — the model's whole
/// response is the program, not a call into one of a menu of functions —
/// so there is nothing here to build one from, unlike the pre-23 wire
/// format this replaced.
///
/// **Under `Transport::RunProgram`: exactly one.** `RUN_PROGRAM_TOOL`,
/// taking a single required string argument (`source`), with
/// `tool_choice: "auto"` rather than forcing it — the model still has to
/// choose to call it on every turn, same as `Program` mode's implicit
/// choice to reply at all, and a forced call would make a refusal or a
/// clarifying question (both real, both already handled elsewhere)
/// impossible to express on the wire.
fn request_body(
    request: &Document,
    model: &str,
    thinking: bool,
    effort: Option<&str>,
    max_tokens: Option<u32>,
) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_json).collect();
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        // Usage rides the last content chunk, but only if asked for.
        // Without this the endpoint sends `"usage": null` on every
        // chunk and nothing at the end — which is how two probes on
        // 2026-09-16 concluded, wrongly, that token counts were
        // unavailable from this provider at all.
        "stream_options": { "include_usage": true },
    });
    if let Some(n) = max_tokens {
        body["max_tokens"] = serde_json::json!(n);
    }
    if !thinking {
        // Exactly what `pi` sends to disable on this provider, so
        // "both off" is the same request on both sides.
        body["thinking"] = serde_json::json!({ "type": "disabled" });
    } else if let Some(effort) = effort {
        // **Both fields, together.** The provider wants
        // `thinking: {"type": "enabled"}` alongside `reasoning_effort`
        // — in the OpenAI SDK the first arrives via `extra_body`, which
        // is the same top-level key on the wire. Sending the level
        // alone leaves it to the API's own default, so two runs at
        // different "levels" can be the same request: enough to make an
        // effort comparison measure nothing, which is what it was about
        // to do here.
        body["thinking"] = serde_json::json!({ "type": "enabled" });
        body["reasoning_effort"] = serde_json::json!(effort);
    }
    body
}

/// Each `ChatMessage` maps to exactly one API role **by its `ChatRole`**,
/// never by a flag. `Document.messages[0]` is always `System` (the
/// snapshotted card + charter, `document::render`'s own invariant);
/// every `User` message is a post or the harness's own report, and every
/// `Assistant` message is the model's reply verbatim.
fn message_json(message: &ChatMessage) -> serde_json::Value {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    };
    serde_json::json!({ "role": role, "content": message.content })
}

/// What a completion cost, as the provider counted it.
///
/// `reasoning` is the part of `completion` spent thinking rather than
/// writing the program, which is the split a long run's wall clock
/// turns on: a 310-second run measured 131KB of reasoning against 10KB
/// of program, and this is that same fact in the provider's own units.
/// `cached` is the share of the prompt served from the prefix cache —
/// the number that decides how much a re-sent context actually costs,
/// and therefore how large the round-trip advantage really is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub prompt: u64,
    pub cached: u64,
    pub completion: u64,
    pub reasoning: u64,
}

#[derive(Default)]
struct Accumulated {
    /// `Transport::Program`: the whole program, built from `content`
    /// deltas. `Transport::RunProgram`: unused — `tool_args` below
    /// carries the program instead, because that transport's `content`
    /// deltas are prose alongside the call, never the program itself.
    source: String,
    thinking: String,
    /// `Transport::RunProgram` only: `content` deltas, i.e. the model's
    /// prose — *the message a person reads* in this container
    /// (`LlmTurn.reply`'s own doc). `Transport::Program`: unused, since
    /// `content` there already **is** the program and lives in `source`
    /// above instead.
    reply: String,
    /// `"length"` once seen — DeepSeek's own signal that `max_tokens`
    /// was hit before the model stopped on its own. Anything else
    /// (`"stop"`, or `"tool_calls"` under `Transport::RunProgram`, once
    /// the call is complete) is an ordinary, finished turn.
    finish_reason: Option<String>,
    usage: Usage,
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
///
/// **`transport` only changes where the program comes from at the very
/// end.** Every delta still streams through `chunk` live exactly as
/// before (`Transport::RunProgram`'s `content` deltas are prose, but a
/// human or UI watching the stream still wants to see them arrive); the
/// difference is which accumulator the final `LlmTurn.source` is read
/// out of — `acc.source` under `Program`, the parsed
/// `acc.tool_args["source"]` under `RunProgram`.
fn parse_sse(
    reader: impl BufRead,
    cancel: &Cancel,
    chunk: &mut dyn FnMut(LlmChunk),
) -> Result<LlmTurn, String> {
    let mut acc = Accumulated::default();
    // Whether the stream said it was over, rather than simply stopping.
    let mut ended = false;

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
            ended = true;
            break;
        }
        let event: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("bad SSE chunk: {e}: {data}"))?;
        if let Some(err) = event.get("error") {
            return Err(format!("deepseek stream error: {err}"));
        }
        // On the last content chunk, not a chunk of its own: read it
        // before touching `choices`, which is empty on the trailer.
        if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
            acc.usage = Usage {
                prompt: u["prompt_tokens"].as_u64().unwrap_or(0),
                cached: u["prompt_cache_hit_tokens"]
                    .as_u64()
                    .or_else(|| u["prompt_tokens_details"]["cached_tokens"].as_u64())
                    .unwrap_or(0),
                completion: u["completion_tokens"].as_u64().unwrap_or(0),
                reasoning: u["completion_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            };
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
            chunk(LlmChunk::Text(t.to_owned()));
            acc.source.push_str(t);
        }
    }

    // **A connection that just stops has not finished.** SSE ends with
    // `[DONE]`, or at least with a `finish_reason`; a reader that hits
    // EOF before either did not receive a completion, it lost one.
    //
    // Returning `Ok` there made a dropped connection indistinguishable
    // from a finished reply: the model's half-sentence became its whole
    // turn, `truncated: false`, with nothing anywhere saying otherwise.
    // Found by testing the retry path against a server that hangs up
    // mid-stream, 2026-09-19 — the retry could never have fired,
    // because the failure it was written for was not being reported as
    // one.
    if !ended && acc.finish_reason.is_none() {
        return Err(format!(
            "stream ended without `[DONE]` or a finish_reason after {} bytes of reply",
            acc.source.len()
        ));
    }
    let truncated = acc.finish_reason.as_deref() == Some("length");
    let source = acc.source;

    Ok(LlmTurn {
        source,
        thinking: (!acc.thinking.is_empty()).then_some(acc.thinking),
        truncated,
        usage: (acc.usage != Usage::default()).then_some(acc.usage),
        // `acc.reply` only ever accumulates under `Transport::RunProgram`
        // (the `content`-delta match above), so this is `None` under
        // `Transport::Program` unconditionally, matching `LlmTurn.reply`'s
        // own doc, and `None` under `Transport::RunProgram` too whenever
        // the model called `run_program` with no prose beside it.
        reply: (!acc.reply.is_empty()).then_some(acc.reply),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(messages: Vec<ChatMessage>) -> Document {
        Document {
            messages,
            preamble: 0,
        }
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
        let body = request_body(&request, "deepseek-v4-pro", true, None, None);

        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["stream"], true);
        // Thinking on is the API's own default: no field sent at all.
        assert!(body.get("thinking").is_none());
        // No function-calling schema under Transport::Program — there is
        // no `tools` array to send at all.
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
    fn request_body_pins_reasoning_effort_when_asked() {
        // `pi` sends `reasoning_effort: "<level>"` on this provider, and
        // `thinking: {type: "disabled"}` to turn it off. Matching both
        // exactly is what makes `DESIGN.md`'s M5 a comparison of two
        // harnesses rather than of two reasoning budgets.
        let request = Document {
            messages: vec![ChatMessage {
                role: ChatRole::System,
                content: "c".into(),
            }],
            preamble: 0,
        };
        let body = request_body(&request, "deepseek-v4-flash", true, Some("medium"), None);
        assert_eq!(body["reasoning_effort"], json!("medium"));
        // The level needs the enable flag beside it; alone it is a
        // request the API may answer at whatever default it likes.
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));

        // Unpinned: no field at all, and the API picks.
        let body = request_body(&request, "deepseek-v4-flash", true, None, None);
        assert!(body.get("reasoning_effort").is_none());
    }

    /// **Unset means unsent.** A hosted provider's own ceiling is
    /// generous and its replies legitimately run to thousands of
    /// tokens, so a default here would truncate real work. A local
    /// server is the other case and says to send one.
    #[test]
    fn max_tokens_is_sent_only_when_asked_for() {
        let request = doc(vec![]);
        let without = request_body(&request, "m", true, None, None);
        assert!(without.get("max_tokens").is_none());
        let with = request_body(&request, "m", true, None, Some(4096));
        assert_eq!(with["max_tokens"], json!(4096));
    }

    #[test]
    fn request_body_disables_thinking_on_request() {
        let request = doc(vec![]);
        let body = request_body(&request, "deepseek-v4-flash", false, None, None);
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
    }

    /// A one-shot HTTP server that answers each connection from
    /// `replies` in order, then hangs up. Returns its base URL.
    ///
    /// Enough to exercise `complete`'s retry loop, which has had three
    /// changes today and no end-to-end coverage: every other test in
    /// this file drives `parse_sse` directly and never sees the loop
    /// around it.
    fn serve(replies: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for body in replies {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                // Drain the request head so the client's write completes.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}")
                        .as_bytes(),
                );
                let _ = sock.flush();
                // Dropping the socket ends the body — an abrupt close
                // for a reply that did not send `[DONE]`.
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    fn delta(field: &str, text: &str) -> String {
        format!(r#"{{"choices":[{{"delta":{{"{field}":"{text}"}}}}]}}"#)
    }

    /// SSE that **stops** rather than ending: no `[DONE]`, no
    /// `finish_reason` — a connection dropped mid-stream. `sse` always
    /// appends `[DONE]`, so it cannot express this.
    fn sse_cut(events: &[&str]) -> String {
        events.iter().map(|e| format!("data: {e}\n\n")).collect()
    }

    /// **A stream that dies before saying anything is retried.** Only
    /// text chunks reach `notebook_stream`, so a reasoning-only stream
    /// that drops has written nothing to the log — the case a slow
    /// local model actually hits.
    #[test]
    fn a_stream_that_died_before_any_text_is_retried() {
        let dies_in_reasoning = sse_cut(&[&delta("reasoning_content", "thinking hard")]);
        let good = sse(&[
            &delta("content", "the answer"),
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let (base, _h) = serve(vec![dies_in_reasoning, good]);

        let client = DeepSeekClient::new("k".into(), "m".into(), base, true);
        let mut text = String::new();
        let turn = client
            .complete(
                &doc(vec![msg(ChatRole::User, "hi")]),
                &Cancel::new(),
                &mut |c| {
                    if let LlmChunk::Text(t) = c {
                        text.push_str(&t);
                    }
                },
            )
            .expect("the retry got a real reply");
        assert_eq!(turn.source, "the answer");
        assert_eq!(text, "the answer", "and the text arrived exactly once");
    }

    /// **But one that already spoke is not.** Its words are on the log;
    /// a second attempt would write the reply twice.
    #[test]
    fn a_stream_that_died_after_speaking_is_not_retried() {
        let dies_mid_reply = sse_cut(&[&delta("content", "half a sen")]);
        let would_be_second = sse(&[
            &delta("content", "a whole different answer"),
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let (base, _h) = serve(vec![dies_mid_reply, would_be_second]);

        let client = DeepSeekClient::new("k".into(), "m".into(), base, true);
        let mut text = String::new();
        let err = client
            .complete(
                &doc(vec![msg(ChatRole::User, "hi")]),
                &Cancel::new(),
                &mut |c| {
                    if let LlmChunk::Text(t) = c {
                        text.push_str(&t);
                    }
                },
            )
            .expect_err("a reply already on the log is not written twice");
        assert!(err.contains("stream"), "{err}");
        assert_eq!(text, "half a sen", "what was said stands, and nothing more");
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

    /// A suspend is two clocks disagreeing: monotonic time stops, wall
    /// time does not. Ordinary drift is seconds and must not look like
    /// one.
    #[test]
    fn a_suspend_is_visible_as_a_gap_between_the_clocks() {
        use std::time::{Duration, Instant, SystemTime};
        let now = Instant::now();
        // Thirteen wall minutes against no monotonic time at all: the
        // shape of the 2026-09-19 run that slept mid-request.
        assert!(slept_since(
            now,
            SystemTime::now() - Duration::from_secs(13 * 60)
        ));
        // A slow request that really did run for those minutes is not.
        assert!(!slept_since(now, SystemTime::now()));
        // Nor is a few seconds of drift or an NTP step.
        assert!(!slept_since(
            now,
            SystemTime::now() - Duration::from_secs(20)
        ));
    }

    /// What is worth asking again about, and what is the request's own
    /// fault however often it is asked.
    #[test]
    fn only_a_transient_status_is_retried() {
        for status in [408, 429, 500, 502, 503, 504, 520, 530, 599] {
            assert!(retryable(status), "{status}");
        }
        for status in [200, 400, 401, 403, 404, 409, 422] {
            assert!(!retryable(status), "{status}");
        }
    }

    /// The budget has to cover two faults with different clocks: a
    /// proxy's 530, which clears in about a second, and a local
    /// server's 503 while it loads a model off disk, which takes ten to
    /// twenty.
    ///
    /// Only `MAX_ATTEMPTS - 1` waits ever happen — the last attempt
    /// returns its failure rather than sleeping on it.
    #[test]
    fn the_retry_budget_covers_a_cold_model_load() {
        let waits: Vec<u128> = (1..MAX_ATTEMPTS).map(|n| backoff(n).as_millis()).collect();
        assert!(waits.windows(2).all(|w| w[1] > w[0]), "grows: {waits:?}");
        assert!(waits[0] <= 500, "the first retry is quick: {waits:?}");
        // **The upper number in that sentence is the bar.** This
        // asserted `6_000..20_000` and passed at exactly 6,000 — the
        // bottom of a range whose own doc says a cold load takes ten to
        // twenty seconds, so the budget it guarded never reached the
        // case it was named for.
        let total: u128 = waits.iter().sum();
        assert!(
            (20_000..60_000).contains(&total),
            "the budget has to outlast a twenty-second load: {total}ms: {waits:?}"
        );
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

    /// **`evals/drive.py` keeps a copy of these, and prints a warning
    /// off it.** The client falls back to a paid endpoint when the
    /// environment says nothing, so exporting a key and nothing else
    /// points a whole arm at a provider that bills. The driver says so
    /// before the first run — and it can only say so correctly while
    /// its copy matches this one.
    #[test]
    fn the_client_defaults_are_what_the_eval_driver_says_they_are() {
        let driver = include_str!("../../../evals/drive.py");
        for (name, value) in [
            ("CLIENT_DEFAULT_BASE_URL", DEFAULT_BASE_URL),
            ("CLIENT_DEFAULT_MODEL", DEFAULT_MODEL),
        ] {
            assert!(
                driver.contains(&format!("{name} = \"{value}\"")),
                "evals/drive.py's {name} is not `{value}` — its warning about \
                 which endpoint is about to be billed would be wrong"
            );
        }
    }
}

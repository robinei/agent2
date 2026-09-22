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
/// **The default endpoint costs nothing.** It used to be the paid
/// provider, so exporting `DEEPSEEK_API_KEY` and nothing else pointed
/// a session — or a fourteen-run eval arm — at a billed API with no
/// word anywhere that it had. The money is the smaller half: a default
/// that spends is one nobody can safely try things against.
///
/// The paid provider is opt-in now, by setting `DEEPSEEK_BASE_URL` and
/// `DEEPSEEK_MODEL`, and `evals/drive.py` prints which endpoint it is
/// about to use before the first run either way.
const DEFAULT_MODEL: &str = "Qwen3.8-27B";

/// Reasoning effort, sent as `reasoning_effort`. `high` because that is
/// what one would realistically run — the harness is measured in the
/// configuration it is used in, not a cheaper one chosen to make the
/// numbers move.
const DEFAULT_EFFORT: &str = "high";
const DEFAULT_BASE_URL: &str = "http://192.168.1.216:8080/v1";

/// Whether a base URL is somewhere on this machine or this network —
/// which is the same question as "does reaching it cost anything".
///
/// Used for one thing only: an endpoint that cannot bill has no reason
/// to demand a credential, and requiring one would make the free
/// default unusable without a placeholder nobody reads.
///
/// **Parsed as an address, not matched as a prefix.** The first
/// version tested `starts_with("192.168.")`, which is true of
/// `192.168.1.216.example.com` — a name anybody can register, pointing
/// anywhere, and treated as free. `Ipv4Addr` decides it instead, which
/// also gets `172.16.0.0/12` right, and a hostname is remote unless it
/// is literally `localhost`.
fn is_local(base_url: &str) -> bool {
    let after_scheme = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let host = after_scheme.split('/').next().unwrap_or("");
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host.rsplit_once(':').map_or(host, |(h, _)| h));
    if host == "localhost" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => false,
    }
}

pub struct DeepSeekClient {
    api_key: String,
    model: String,
    base_url: String,
    thinking: bool,
    /// Pinned reasoning level, sent as `reasoning_effort`. `None` leaves
    /// the field off and lets the API choose.
    effort: Option<String>,
    max_tokens: Option<u32>,
    /// **Offer `run_program` and let the program ride in the call**
    /// (`AGENT2_RUN_PROGRAM`), instead of being the reply itself.
    ///
    /// Only this file changes. The call's `source` argument is wrapped
    /// back into a ```js cell before it leaves `parse_sse`, so every
    /// path below — the notebook splitter, the runner, the document,
    /// the log — receives an ordinary markdown reply and cannot tell
    /// the difference. That is the whole of the transport: a wrapper
    /// on the wire, unwrapped on arrival.
    run_program: bool,
    /// **This endpoint is OpenAI's own**, which takes a different shape
    /// for two fields: it rejects the `thinking` object DeepSeek and
    /// opencode want, and it refuses `max_tokens` on the reasoning
    /// models in favour of `max_completion_tokens`. Derived from the
    /// base URL rather than configured, because it is a fact about
    /// where the request is going, not a choice anyone makes.
    openai_shape: bool,
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
        let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let base_url =
            std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        // A key is still required of anything that could charge for
        // the answer, and the error still names the variable. A local
        // server ignores whatever is sent, so asking for one there
        // would only teach people to export a placeholder — and the
        // habit of exporting a placeholder is exactly what made the
        // old paid default silent.
        let api_key = match std::env::var("DEEPSEEK_API_KEY") {
            Ok(key) => key,
            Err(_) if is_local(&base_url) => "local".to_owned(),
            Err(_) => {
                return Err(format!(
                    "DEEPSEEK_API_KEY is not set, and {base_url} is not on this machine"
                ));
            }
        };
        let thinking = std::env::var("DEEPSEEK_NO_THINKING").is_err();
        let max_tokens = std::env::var("DEEPSEEK_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|n| *n > 0);
        let effort = std::env::var("DEEPSEEK_REASONING_EFFORT")
            .ok()
            .or_else(|| Some(DEFAULT_EFFORT.to_owned()));
        let run_program = std::env::var("AGENT2_RUN_PROGRAM").is_ok_and(|v| v != "0");
        Ok(Self::new(api_key, model, base_url, thinking)
            .with_effort(effort)
            .with_max_tokens(max_tokens)
            .with_run_program(run_program))
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

    pub fn with_run_program(mut self, run_program: bool) -> Self {
        self.run_program = run_program;
        self
    }

    pub fn new(api_key: String, model: String, base_url: String, thinking: bool) -> Self {
        let openai_shape = base_url.contains("api.openai.com");
        // Completions stream for minutes, so the body read cannot be
        // held to a short deadline — but "no deadline at all" means a
        // dead socket hangs the branch forever, with the TUI showing a
        // session that is simply never going to continue.
        //
        // **The two knobs are not what their names suggest**, and the
        // comment that used to sit here had them backwards. Measured
        // against a local server on 2026-09-20
        // (`recv_response_caps_the_whole_response`,
        // `recv_body_is_an_idle_bound`):
        //
        // - `timeout_recv_response` is a ceiling on the **whole
        //   response**, body included — not on time-to-headers. With a
        //   3s value against a server that sent headers instantly and
        //   then went silent, the read failed at 3.0s naming "receive
        //   response".
        // - `timeout_recv_body` is an **idle** bound: it restarts on
        //   every byte. A 3s value survived a trickle of one keep-alive
        //   per second for eight seconds.
        //
        // So the old pair — 10 minutes "to the first headers" and 30
        // for the body — was a hard 10-minute cap on every completion,
        // with the idle bound set past it and therefore unreachable.
        // The LAN box routinely spends longer than that on one program,
        // which is what "two real runs killed mid-task by their own
        // client" actually was; and because the kill lands before any
        // text chunk, the retry loop re-sends and the branch sits
        // silent for a multiple of it, writing nothing to the log.
        //
        // The roles are now the right way round. The ceiling is
        // generous because a slow model is not a broken one; the idle
        // bound is what catches a peer that has gone away, and it is
        // the tight one because silence is the symptom that actually
        // distinguishes dead from slow.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(std::time::Duration::from_secs(15)))
            .timeout_recv_response(Some(COMPLETION_CEILING))
            .timeout_recv_body(Some(SOCKET_IDLE))
            .build();
        DeepSeekClient {
            api_key,
            model,
            base_url,
            thinking,
            effort: None,
            max_tokens: None,
            run_program: false,
            openai_shape,
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
        // The one place the transport touches the document. Everything
        // upstream rendered markdown; this hands the model its own past
        // programs in the shape it emits them, exemplars included.
        let shaped;
        let request = if self.run_program {
            shaped = request.clone().into_tool_calls();
            &shaped
        } else {
            request
        };
        let body = request_body(
            request,
            &self.model,
            self.thinking,
            self.effort.as_deref(),
            self.max_tokens,
            self.run_program,
            self.openai_shape,
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
                        match parse_sse(reader, cancel, &mut counting, self.run_program) {
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
                // The bounds are generous on purpose (see `new`): this
                // endpoint *queues* under load rather than refusing, so
                // a busy moment looks exactly like a dead socket, and a
                // tight bound killed two real runs mid-task. A queued
                // request that is retried simply queues again — and
                // seven attempts at the ceiling is most of a day of a
                // branch doing nothing. Retrying a timeout costs the
                // most and buys the least.
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

/// **A ceiling on one whole completion**, body included — ureq's
/// `timeout_recv_response`, whose name describes a phase it does not
/// actually bound. Deliberately far past anything healthy: a slow model
/// is not a broken one, and the bound that catches a broken one is
/// [`SOCKET_IDLE`].
const COMPLETION_CEILING: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// **No bytes at all for this long means the peer is gone** — ureq's
/// `timeout_recv_body`, which restarts on every byte received.
///
/// This is the one that has to be tight, because silence is what
/// separates a dead socket from a slow one: a generating model emits
/// deltas continuously, and the long legitimate gap is the wait before
/// the first token, which is minutes rather than tens of them.
const SOCKET_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

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
/// The one tool offered under `run_program`. The description says as
/// little as possible: the card is still the surface, and every rule
/// about what a program may contain lives there. This says only where
/// the program goes.
fn run_program_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "run_program",
            "description": "Run a program. This is the only way to do anything.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "The program, as JavaScript."
                    }
                },
                "required": ["source"]
            }
        }
    })
}

fn request_body(
    request: &Document,
    model: &str,
    thinking: bool,
    effort: Option<&str>,
    max_tokens: Option<u32>,
    run_program: bool,
    openai_shape: bool,
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
    if run_program {
        body["tools"] = serde_json::json!([run_program_tool()]);
    }
    if let Some(n) = max_tokens {
        body[if openai_shape { "max_completion_tokens" } else { "max_tokens" }] =
            serde_json::json!(n);
    }
    if openai_shape {
        // No `thinking` object at all — OpenAI errors on the unknown
        // field. `reasoning_effort` is its own knob there and is sent
        // on its own; the levels this harness may pass that OpenAI does
        // not know (`xhigh`, `max`) are the caller's problem to avoid.
        if let Some(effort) = effort.filter(|_| thinking) {
            body["reasoning_effort"] = serde_json::json!(effort);
        }
    } else if !thinking {
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
    // A turn the call transport lifted: the program goes back as the
    // call it was, so the model's own history shows it acting the way
    // it is being told to act.
    if let Some((id, source)) = &message.call {
        let mut out = serde_json::json!({
            "role": "assistant",
            "content": message.content,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {
                    "name": "run_program",
                    "arguments": serde_json::json!({ "source": source }).to_string(),
                }
            }]
        });
        // **Always present on a call, even when empty.** This provider
        // refuses an assistant turn carrying `tool_calls` unless its
        // reasoning comes back with it, and the turns that have none
        // are the *exemplars* — static files, written once, with no
        // reasoning to carry. Sending the model's own thinking fixed
        // two runs in five and left three failing on the worked
        // examples, which the error named and I did not read.
        out["reasoning_content"] =
            serde_json::json!(message.thinking.as_deref().unwrap_or(""));
        return out;
    }
    // And the report that answers one goes back in the `tool` role. The
    // API requires a tool message after a tool call; a bare user turn
    // there is rejected by some providers and, worse, accepted by
    // others as a turn the model never took.
    if let Some(id) = &message.result_for {
        return serde_json::json!({
            "role": "tool",
            "tool_call_id": id,
            "content": message.content,
        });
    }
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    };
    let mut out = serde_json::json!({ "role": role, "content": message.content });
    if let Some(t) = &message.thinking {
        out["reasoning_content"] = serde_json::json!(t);
    }
    out
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
    /// `run_program` only: each call's `function.arguments`, keyed by
    /// the delta's `index` and in that order.
    ///
    /// **Keyed, because a turn may hold more than one call.** Read as
    /// `tool_calls[0]` of each delta — ignoring `index` — a second call
    /// appends its fragments to the first one's buffer, and what was
    /// two programs becomes one string of invalid JSON. The turn then
    /// fails with a complaint about the JSON, which is the last place
    /// anyone would look.
    ///
    /// Two calls are two cells, which is what the notebook has always
    /// called a reply that runs more than one thing.
    tool_args: std::collections::BTreeMap<u64, String>,
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
    run_program: bool,
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
        if run_program
            && let Some(calls) = delta["tool_calls"].as_array()
        {
            for call in calls {
                let Some(t) = call["function"]["arguments"].as_str() else {
                    continue;
                };
                if t.is_empty() {
                    continue;
                }
                let index = call["index"].as_u64().unwrap_or(0);
                acc.tool_args.entry(index).or_default().push_str(t);
            }
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
    // **Unwrap the call into the reply it stands for.** Under
    // `run_program` the program arrived as a JSON string argument and
    // the `content` deltas were prose beside it; a markdown reply with
    // that prose and one ```js cell is the same thing in the shape
    // every path below this one already reads. So the transport is
    // this function and nothing else.
    //
    // A truncated completion is left alone: the arguments are a cut-off
    // JSON string that will not parse, and `Cause::Truncated` must be
    // reported rather than masked by a parse error about it.
    let source = if run_program && !truncated && !acc.tool_args.is_empty() {
        let mut fences = String::new();
        for raw in acc.tool_args.values() {
            let args: serde_json::Value = serde_json::from_str(raw)
                .map_err(|e| format!("run_program arguments are not JSON: {e}: {raw}"))?;
            let program = args["source"]
                .as_str()
                .ok_or_else(|| format!("run_program call has no `source` string: {raw}"))?;
            if !fences.is_empty() {
                fences.push('\n');
            }
            fences.push_str(&format!("```js\n{program}\n```\n"));
        }
        // **And the cell has to go through `chunk` too**, because the
        // session never reads the value this function returns: it feeds
        // the notebook from the deltas as they arrive
        // (`host/mod.rs`'s `on_chunk`) and finalises a reply that is
        // already on the log. The `content` deltas under this transport
        // are prose alone, so a reply assembled from them has no cell
        // in it — the program runs nowhere, the branch rests, and the
        // run ends having read four files and stopped. That was 0 of 5
        // on `sweep-8`, and `agent sample`, which *does* read the
        // return value, scored the same document 12 of 12.
        //
        // Emitted whole and last rather than streamed: the arguments
        // arrive as fragments of an escaped JSON string, so there is no
        // prefix of them that is a program.
        let suffix = if acc.source.trim().is_empty() {
            fences
        } else {
            format!("\n\n{fences}")
        };
        chunk(LlmChunk::Text(suffix.clone()));
        format!("{}{suffix}", acc.source)
    } else {
        acc.source
    };

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
            call: None,
            result_for: None,
            thinking: None,
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
        let body = request_body(&request, "deepseek-v4-pro", true, None, None, false, false);

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
            messages: vec![ChatMessage { role: ChatRole::System, content: "c".into(), call: None, result_for: None, thinking: None }],
            preamble: 0,
        };
        let body = request_body(&request, "deepseek-v4-flash", true, Some("medium"), None, false, false);
        assert_eq!(body["reasoning_effort"], json!("medium"));
        // The level needs the enable flag beside it; alone it is a
        // request the API may answer at whatever default it likes.
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));

        // Unpinned: no field at all, and the API picks.
        let body = request_body(&request, "deepseek-v4-flash", true, None, None, false, false);
        assert!(body.get("reasoning_effort").is_none());
    }

    /// **Unset means unsent.** A hosted provider's own ceiling is
    /// generous and its replies legitimately run to thousands of
    /// tokens, so a default here would truncate real work. A local
    /// server is the other case and says to send one.
    #[test]
    fn max_tokens_is_sent_only_when_asked_for() {
        let request = doc(vec![]);
        let without = request_body(&request, "m", true, None, None, false, false);
        assert!(without.get("max_tokens").is_none());
        let with = request_body(&request, "m", true, None, Some(4096), false, false);
        assert_eq!(with["max_tokens"], json!(4096));
    }

    #[test]
    fn request_body_disables_thinking_on_request() {
        let request = doc(vec![]);
        let body = request_body(&request, "deepseek-v4-flash", false, None, None, false, false);
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

    /// A server that answers, sends `head`, then goes silent forever
    /// without closing — the dead-peer shape. Holds the socket open on
    /// a parked thread so nothing closes it.
    fn serve_then_go_silent(head: String) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.flush();
            // Never write again, never close.
            std::thread::sleep(std::time::Duration::from_secs(600));
            drop(sock);
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// **`timeout_recv_body` is an idle bound**: it restarts on every
    /// byte received. That is what makes it the right knob for "the
    /// peer has gone away" — a completion that streams for twenty
    /// minutes is healthy, and a socket silent for ten is not. Pinned
    /// for the same reason as the test above: `SOCKET_IDLE` is only
    /// safe to set tight while this holds.
    #[test]
    fn recv_body_is_an_idle_bound() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
            let _ = sock.flush();
            // A byte every second for eight seconds: healthy trickle.
            for _ in 0..8 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if sock.write_all(b": keep-alive\n\n").is_err() {
                    return;
                }
                let _ = sock.flush();
            }
        });
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
            .timeout_recv_body(Some(std::time::Duration::from_secs(3)))
            .build();
        let agent: ureq::Agent = config.into();
        let started = std::time::Instant::now();
        let mut got = agent
            .post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .send_json(serde_json::json!({"a": 1}))
            .unwrap();
        let mut s = String::new();
        let r = got.body_mut().as_reader().read_to_string(&mut s);
        let elapsed = started.elapsed();
        r.unwrap_or_else(|e| {
            panic!("a 3s bound killed an 8s trickle after {elapsed:?}: {e} — `recv_body` is a total bound, and `SOCKET_IDLE` is now unsafe")
        });
        assert!(
            elapsed >= std::time::Duration::from_secs(7),
            "the trickle should have been read to its end, not cut short: {elapsed:?}"
        );
    }

    /// **`timeout_recv_response` caps the whole response**, body
    /// included — it does not bound time-to-headers, whatever its name
    /// says. The production pair is chosen on this fact
    /// (`COMPLETION_CEILING`), so it is pinned rather than remembered:
    /// a dependency upgrade that changed it would silently reinstate a
    /// hard cap on every completion.
    #[test]
    fn recv_response_caps_the_whole_response() {
        for (resp, body, expect) in [
            (3u64, 10u64, "receive response"),
            (10, 3, "receive body"),
            (3, 3, "receive response"),
        ] {
            let url = serve_then_go_silent(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"
                    .to_owned(),
            );
            let config = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(std::time::Duration::from_secs(5)))
                .timeout_recv_response(Some(std::time::Duration::from_secs(resp)))
                .timeout_recv_body(Some(std::time::Duration::from_secs(body)))
                .build();
            let agent: ureq::Agent = config.into();
            let started = std::time::Instant::now();
            let out = (|| -> Result<String, String> {
                let mut got = agent
                    .post(&format!("{url}/chat/completions"))
                    .send_json(serde_json::json!({"a": 1}))
                    .map_err(|e| format!("send: {e}"))?;
                let mut s = String::new();
                use std::io::Read;
                got.body_mut()
                    .as_reader()
                    .read_to_string(&mut s)
                    .map_err(|e| format!("read: {e}"))?;
                Ok(s)
            })();
            let elapsed = started.elapsed();
            let err = out.expect_err("the peer went silent");
            assert!(
                err.contains(expect),
                "resp={resp}s body={body}s named {err:?}, not {expect:?}"
            );
            assert!(
                elapsed < std::time::Duration::from_secs(resp.min(body) + 3),
                "resp={resp}s body={body}s took {elapsed:?}"
            );
        }
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

    /// **The model's history shows it acting the way it is told to.**
    ///
    /// Rendered the ordinary way, a past turn comes back as an
    /// assistant message holding a ```js block — so under the call
    /// transport the card says "code written in the reply does nothing"
    /// while the model looks at its own replies, full of code, that
    /// evidently ran. The first live run of this arm did exactly that,
    /// and in this codebase the example beats the rule every time it
    /// has been measured.
    ///
    /// The rewrite is also what lets the **exemplars** stay. They are
    /// authored once, in fences, and rendered in whichever shape the
    /// session is using — so neither arm has to give up the worked
    /// examples to be measured.
    #[test]
    fn a_past_program_goes_back_as_the_call_it_was() {
        let doc = doc(vec![
            msg(ChatRole::System, "card"),
            msg(ChatRole::User, "do it"),
            msg(ChatRole::Assistant, "Reading it.\n\n```js\nreturn 1;\n```\n"),
            msg(ChatRole::User, "## program completed"),
        ])
        .into_tool_calls();

        let turn = &doc.messages[2];
        assert_eq!(turn.content, "Reading it.", "the prose is kept, the fence is not");
        let (id, source) = turn.call.as_ref().expect("the turn became a call");
        // The cell keeps its trailing newline; the program is the
        // bytes between the fences, not a trimmed version of them.
        assert_eq!(source, "return 1;\n");
        assert_eq!(
            doc.messages[3].result_for.as_deref(),
            Some(id.as_str()),
            "the report that follows is that call's result"
        );

        let body = message_json(turn);
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["tool_calls"][0]["function"]["name"], "run_program");
        let args: serde_json::Value =
            serde_json::from_str(body["tool_calls"][0]["function"]["arguments"].as_str().unwrap())
                .unwrap();
        assert_eq!(args["source"], "return 1;\n");
        assert!(
            !body.to_string().contains("```js"),
            "a fence still reaches the model: {body}"
        );

        let report = message_json(&doc.messages[3]);
        assert_eq!(report["role"], "tool", "{report}");
        assert_eq!(report["tool_call_id"], *id);
    }

    /// **OpenAI takes a different shape for two fields.**
    ///
    /// It errors on the `thinking` object every other endpoint here
    /// wants, and it refuses `max_tokens` on its reasoning models. Both
    /// are facts about where the request is going, so both follow the
    /// base URL rather than a flag anyone has to remember.
    #[test]
    fn the_openai_endpoint_gets_the_shape_it_accepts() {
        let request = doc(vec![msg(ChatRole::System, "card")]);

        let ds = request_body(&request, "m", true, Some("high"), Some(4096), false, false);
        assert_eq!(ds["thinking"]["type"], "enabled");
        assert_eq!(ds["max_tokens"], 4096);
        assert!(ds.get("max_completion_tokens").is_none());

        let oa = request_body(&request, "gpt-5", true, Some("high"), Some(4096), false, true);
        assert!(oa.get("thinking").is_none(), "OpenAI rejects it: {oa}");
        assert_eq!(oa["max_completion_tokens"], 4096);
        assert!(oa.get("max_tokens").is_none());
        assert_eq!(oa["reasoning_effort"], "high");

        // Thinking off means no effort either, not "disabled".
        let off = request_body(&request, "gpt-5", false, Some("high"), None, false, true);
        assert!(off.get("thinking").is_none(), "{off}");
        assert!(off.get("reasoning_effort").is_none(), "{off}");
    }

    /// And the flag is derived, not configured.
    #[test]
    fn the_endpoint_decides_its_own_shape() {
        let oa = DeepSeekClient::new(
            "k".into(),
            "gpt-5".into(),
            "https://api.openai.com/v1".into(),
            true,
        );
        assert!(oa.openai_shape);
        let ds = DeepSeekClient::new(
            "k".into(),
            "deepseek-v4-flash".into(),
            "https://opencode.ai/zen/go/v1".into(),
            true,
        );
        assert!(!ds.openai_shape);
    }

    /// **Two calls in one turn are two cells, not one corrupt one.**
    ///
    /// The delta carries an `index`; reading `tool_calls[0]` of each
    /// chunk and ignoring it appends the second call's fragments to the
    /// first call's buffer. The result is one string of invalid JSON,
    /// and the turn dies complaining about JSON — which says nothing
    /// about the two programs that were lost.
    #[test]
    fn two_calls_in_a_turn_become_two_cells() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"source\":\"const a = 1;\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"source\":\"const b = a + 1;\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let mut streamed = String::new();
        let turn = parse_sse(
            stream.as_bytes(),
            &Cancel::new(),
            &mut |c| {
                if let LlmChunk::Text(t) = c {
                    streamed.push_str(&t);
                }
            },
            true,
        )
        .unwrap();
        let cells = crate::notebook::split_cells(&turn.source);
        assert_eq!(cells.len(), 2, "{:?}", turn.source);
        assert_eq!(cells[0].slice(&turn.source).trim(), "const a = 1;");
        assert_eq!(cells[1].slice(&turn.source).trim(), "const b = a + 1;");
        assert_eq!(streamed, turn.source, "the session would see something else");
    }

    /// **Reasoning goes back only when asked for, and the default is
    /// that it does not.**
    ///
    /// Thinking staying out of the document is why a block drafted
    /// while reasoning cannot reach the next turn, and therefore why
    /// drafting is measurable per reply at all. The knob exists because
    /// this provider refuses an assistant turn carrying `tool_calls`
    /// without it — one run in five died on that 400 — so turning it on
    /// buys a working call transport and opens a route this week's
    /// measurements all assumed was shut.
    #[test]
    fn reasoning_rides_back_only_when_the_document_carries_it() {
        let mut m = msg(ChatRole::Assistant, "prose");
        m.call = Some(("call_1".into(), "return 1;".into()));
        // A call always carries the field, empty when there is nothing
        // — the exemplars are exactly that case and the provider 400s
        // without it.
        let without = message_json(&m);
        assert_eq!(without["reasoning_content"], "", "{without}");

        m.thinking = Some("weighing it up".into());
        let with = message_json(&m);
        assert_eq!(with["reasoning_content"], "weighing it up");
        assert_eq!(with["tool_calls"][0]["function"]["name"], "run_program");

        // And on a plain turn too, since the notebook arm may want it.
        let mut plain = msg(ChatRole::Assistant, "prose");
        plain.thinking = Some("weighing it up".into());
        assert_eq!(message_json(&plain)["reasoning_content"], "weighing it up");
        assert!(message_json(&msg(ChatRole::Assistant, "prose"))
            .get("reasoning_content")
            .is_none());
    }

    /// **What is streamed must reconstruct what is returned.**
    ///
    /// The session never reads `LlmTurn.source`: it feeds the notebook
    /// from `chunk` as the deltas arrive and finalises a reply that is
    /// already on the log. So a transport that assembles its program
    /// only in the return value delivers a prose-only reply, the
    /// program runs nowhere, and the branch rests — which cost three
    /// A/B runs and looked each time like the model refusing to call.
    ///
    /// Asserted for both transports, because the rule is about the
    /// contract and not about the arm: whatever `source` says, the
    /// chunks said first.
    #[test]
    fn the_chunks_add_up_to_the_turn() {
        for (run_program, stream) in [
            (
                true,
                sse(&[
                    r#"{"choices":[{"delta":{"content":"Reading them."}}]}"#,
                    r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{\"source\":\"return 1;\"}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]),
            ),
            (
                false,
                sse(&[
                    r#"{"choices":[{"delta":{"content":"```js\nreturn 1;\n```"}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                ]),
            ),
        ] {
            let mut streamed = String::new();
            let turn = parse_sse(
                stream.as_bytes(),
                &Cancel::new(),
                &mut |c| {
                    if let LlmChunk::Text(t) = c {
                        streamed.push_str(&t);
                    }
                },
                run_program,
            )
            .unwrap();
            assert_eq!(
                streamed, turn.source,
                "run_program={run_program}: the session would see something else"
            );
            assert!(
                !crate::notebook::split_cells(&streamed).is_empty(),
                "run_program={run_program}: nothing to run in the streamed reply"
            );
        }
    }

    /// A turn with no cell is left alone — a resting reply is prose and
    /// nothing else, and inventing an empty call for it would tell the
    /// model it had run something.
    #[test]
    fn a_reply_that_ran_nothing_stays_a_plain_turn() {
        let doc = doc(vec![
            msg(ChatRole::System, "card"),
            msg(ChatRole::User, "what is it"),
            msg(ChatRole::Assistant, "Four files, all live."),
        ])
        .into_tool_calls();
        assert!(doc.messages[2].call.is_none());
        assert_eq!(message_json(&doc.messages[2])["role"], "assistant");
    }

    /// **The whole of the `run_program` transport, asserted.**
    ///
    /// The program rides in the call's `source` argument and comes out
    /// as a markdown reply with one ```js cell, because that is the
    /// shape every path below this file already reads. If this holds,
    /// the notebook splitter, the runner, the document and the log need
    /// no knowledge of the transport at all — which is the entire
    /// reason it is 40 lines and not a second implementation.
    #[test]
    fn a_run_program_call_arrives_as_a_notebook_reply() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"content":"Reading the four files."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"run_program","arguments":"{\"source\":\"const f = "}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"await tools.read_file(\\\"a.py\\\");\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}, true).unwrap();
        assert_eq!(
            turn.source,
            "Reading the four files.\n\n```js\nconst f = await tools.read_file(\"a.py\");\n```\n",
            "the call did not come back as prose plus one cell"
        );
        assert!(!turn.truncated);
        // And the cell splitter agrees, which is the claim that matters.
        let cells = crate::notebook::split_cells(&turn.source);
        assert_eq!(cells.len(), 1, "one cell: {:?}", turn.source);
    }

    /// A call with no prose beside it is a reply that is only a cell —
    /// no leading blank lines, which would otherwise show up as an
    /// empty prose part on the log.
    #[test]
    fn a_run_program_call_without_prose_is_only_the_cell() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{\"source\":\"finish();\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}, true).unwrap();
        assert_eq!(turn.source, "```js\nfinish();\n```\n");
    }

    /// **A truncated call is reported as truncated, not as bad JSON.**
    ///
    /// Cut-off arguments will not parse, and the tempting error message
    /// is about the JSON. `Cause::Truncated` is the one that must
    /// survive: `types.rs` says a truncated completion must never reach
    /// the compiler, and a parse error here would hide why.
    #[test]
    fn a_truncated_run_program_call_stays_truncated() {
        let stream = sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{\"source\":\"const x = "}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}, true).unwrap();
        assert!(turn.truncated, "the length stop was lost");
    }

    /// Off by default: no `tools` key at all, which is what keeps the
    /// notebook request byte-identical to what it was.
    #[test]
    fn the_tool_is_offered_only_when_asked_for() {
        let request = doc(vec![msg(ChatRole::System, "card")]);
        let off = request_body(&request, "m", true, None, None, false, false);
        assert!(off.get("tools").is_none(), "{off}");
        let on = request_body(&request, "m", true, None, None, true, false);
        assert_eq!(on["tools"][0]["function"]["name"], "run_program");
        assert_eq!(
            on["tools"][0]["function"]["parameters"]["required"][0], "source",
            "{on}"
        );
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
        }, false)
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
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}, false).unwrap();
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
        let turn = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}, false).unwrap();
        assert!(!turn.truncated);
    }

    #[test]
    fn parse_sse_surfaces_stream_errors() {
        let stream = "data: {\"error\":{\"message\":\"rate limited\"}}\n\n";
        let err = parse_sse(stream.as_bytes(), &Cancel::new(), &mut |_| {}, false).unwrap_err();
        assert!(err.contains("rate limited"), "{err}");
    }

    /// **A key is required of anything that could charge for the
    /// answer, and of nothing else.**
    ///
    /// The default endpoint costs nothing now, so demanding a
    /// credential for it would only teach people to export a
    /// placeholder — and the habit of exporting a placeholder is what
    /// made the old paid default silent: `DEEPSEEK_API_KEY=x` and a
    /// fourteen-run arm went to a billed API with no word anywhere.
    #[test]
    fn only_an_endpoint_that_can_bill_demands_a_key() {
        for local in [
            "http://192.168.1.216:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://localhost:11434/v1",
            "http://10.0.0.4/v1",
            "http://172.16.3.1:8080/v1",
            "http://[::1]:8080/v1",
            DEFAULT_BASE_URL,
        ] {
            assert!(is_local(local), "{local}");
        }
        for remote in [
            "https://opencode.ai/zen/go/v1",
            "https://api.deepseek.com/v1",
            // A name anybody can register, pointing anywhere. The
            // first version of `is_local` matched it on a prefix.
            "https://192.168.1.216.example.com/v1",
            "https://10.example.com/v1",
            "https://localhost.example.com/v1",
            "https://172.32.0.1/v1",
        ] {
            assert!(!is_local(remote), "{remote}");
        }
    }

    /// And the refusal says which endpoint it was unwilling to reach
    /// without one, because "DEEPSEEK_API_KEY is not set" alone does
    /// not tell you whether you meant to be spending money.
    #[test]
    fn the_refusal_names_the_endpoint_it_would_have_billed() {
        // Nothing here reads the environment: `from_env` is exercised
        // through the same decision it makes, spelled out, so the test
        // does not race another test's `set_var`.
        let base = "https://opencode.ai/zen/go/v1";
        assert!(!is_local(base));
        let err = format!("DEEPSEEK_API_KEY is not set, and {base} is not on this machine");
        assert!(err.contains("DEEPSEEK_API_KEY"), "{err}");
        assert!(err.contains("opencode.ai"), "{err}");
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

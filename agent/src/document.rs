//! The document renderer and the completion-extraction rule (phase 20
//! doc, Part A "The request path" / Part B "The document"; folded in
//! from the deleted `fence.rs`, phase 20 doc Part A "The transport").
//!
//! [`render`] turns one branch's slice of the event log (its own
//! snapshotted system prompt, `Spine::context`, plus its path) into a
//! role-delimited chat [`Document`] — the transport-agnostic shape
//! `host/mod.rs` hands to `spawn_llm`, which now takes a `Document`
//! directly rather than `machine::LlmRequest`. [`extract_program`] is
//! the other direction: turning a raw completion back into program
//! source before it is compiled and logged as a `Turn`. Both belong
//! here because both are "the document" in the broad sense — what goes
//! out, and what comes back — and neither talks to a model or a network
//! on its own.
//!
//! **This is a request builder over `&Tree`, not a stored log of its
//! own.** The POC's `document.rs` rendered a private row vector — a
//! second, parallel event log with its own id space. That log is gone
//! (its module deleted; doc 22, "one vocabulary"): `agent/src/types.rs`'s
//! `Event`/`EventId` are the only event vocabulary this crate has, and
//! [`render`] reads them directly via [`Tree::path_events`]. What used
//! to be one of that log's own stored effects rows is now a fold over
//! `Call`/`Result` events — one already exists, and lives in
//! `report.rs` (`derive_report`, memoised per outcome id), so this
//! module calls into it rather than re-deriving effects a second,
//! incompatible way.

use std::collections::HashMap;

use crate::tree::{CompactedView, depth_after};
use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    /// `Transport::RunProgram` only: the harness's answer to a
    /// `run_program` tool call, in the role the wire format requires
    /// immediately after a tool-calling assistant turn. `render` never
    /// produces this role under `Transport::Program` — see
    /// `flush_pending`.
    Tool,
}

/// A `run_program` call, as carried on an assistant [`ChatMessage`]
/// under `Transport::RunProgram`. One call per turn: code mode never
/// offers a menu of functions to choose from (`host/deepseek.rs`'s own
/// doc comment), so there is nothing here to disambiguate by name —
/// `id` exists only to pair this call with the `Tool`-role message that
/// answers it, the way the wire format requires.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// `Transport::RunProgram` only: the call this assistant turn makes
    /// instead of sending the program as bare `content`. Always `None`
    /// under `Transport::Program` and on every non-`Assistant` message,
    /// so `host/deepseek.rs`'s `message_json` omits the wire field
    /// entirely and a `Transport::Program` request body is unchanged
    /// byte-for-byte from before this type grew the field.
    pub tool_calls: Option<Vec<ToolCall>>,
    /// `Transport::RunProgram` only: on a `Tool`-role message, the id of
    /// the call (`tool_calls` above, on the preceding assistant turn)
    /// this is the result of. `None` everywhere else.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// A plain message with neither tool field set — every message
    /// `Transport::Program` ever produces, and most of what
    /// `Transport::RunProgram` produces too (its `System`/`User`
    /// messages are identical to `Program`'s; only its assistant turns
    /// and their tool answers carry the two fields above).
    fn text(role: ChatRole, content: impl Into<String>) -> Self {
        ChatMessage {
            role,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

/// Which container carries the model's program on the wire.
/// [`Transport::Program`] (default) sends the model's entire response
/// text as the program itself — no `tools` array, no function-calling
/// wrapper, per `host/deepseek.rs`'s own doc comment on why code mode
/// has neither. [`Transport::RunProgram`] instead advertises a single
/// `run_program(source)` tool and reads the program back out of the
/// resulting call, so the same wire round-trip looks like an ordinary
/// tool-calling completion to anything watching the transport (a proxy,
/// a provider's own logging) that only understands that shape.
///
/// Selected by `AGENT2_TRANSPORT` (`program`/`run_program`), read fresh
/// on every call rather than cached — the same `AGENT2_*` idiom as
/// `host/mod.rs`'s `document_budget`/`compaction_headroom`, so a session
/// can be pointed at either container without a rebuild. The event log
/// this produces is identical in shape either way: same `Turn`/`Call`/
/// `Result`/`Condition` payloads, same `Message::Turn.source` — only
/// this module's rendering and `host/deepseek.rs`'s wire-facing code
/// branch on it at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Program,
    RunProgram,
}

pub const DEFAULT_TRANSPORT: Transport = Transport::Program;

/// An unset or unrecognized value falls back to [`DEFAULT_TRANSPORT`],
/// the same "garbage in, quiet default" rule the other `AGENT2_*`
/// readers use (`llm_concurrency`'s `filter(|n| *n >= 1)`,
/// `compaction_headroom`'s open-interval filter) rather than a run
/// failing to start over a typo'd env var.
pub(crate) fn transport() -> Transport {
    match std::env::var("AGENT2_TRANSPORT").as_deref() {
        Ok("run_program") => Transport::RunProgram,
        _ => DEFAULT_TRANSPORT,
    }
}

/// A rendered request, transport-agnostic (Part A: "the document is
/// the interface"). `messages[0]` is always the card, in `System`
/// (Step A1: "the card goes in `system`").
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Document {
    pub messages: Vec<ChatMessage>,
}

impl Document {
    /// The conversation proper — everything after the preamble (the
    /// system message and the worked examples). What a caller reasoning
    /// about *this branch's* history wants, as opposed to what is sent
    /// on the wire.
    ///
    /// The preamble is a fixed-size prefix that grows when an exemplar
    /// is added, so positional indexing into `messages` is a latent
    /// break in anything that means "the first real turn."
    pub fn conversation(&self) -> &[ChatMessage] {
        &self.messages[1 + worked_examples().len()..]
    }

    /// Append ephemeral, one-request-only content to the open turn
    /// (Step B1c: the tail — a condition report, a `vm` pointer). It
    /// is never part of the log and never returned by [`render`] on its
    /// own: callers apply it to the rendered document, so it can never
    /// leak into what gets stored.
    ///
    /// Extends the trailing `User` message's content when there is
    /// one (the common case: the tail rides on the turn that already
    /// triggered this completion — a user post, or the outcome of a
    /// now-suspended program). Starts a fresh `User` message only when
    /// the record ends on an `Assistant` turn (or holds just the card)
    /// — a shape [`render`] only produces for a branch with nothing
    /// open yet, which a real completion request is never built from,
    /// but a defensive fallback costs nothing.
    pub fn with_tail(mut self, tail: &str) -> Self {
        if tail.is_empty() {
            return self;
        }
        match self.messages.last_mut() {
            Some(m) if m.role == ChatRole::User => {
                m.content.push('\n');
                m.content.push_str(tail);
            }
            _ => self.messages.push(ChatMessage::text(ChatRole::User, tail)),
        }
        self
    }
}

/// Harness lines have a fixed generated shape — `^\[\d+\]` — that the
/// harness never emits inside quoted material (Step B1). Escaping a
/// line of untrusted content that happens to start the same way is a
/// mechanical, unconditional rule (not a heuristic about what the line
/// "means"): prefix it with a backslash, the same way a literal
/// metacharacter is escaped. Cheap in the overwhelmingly common case
/// (no line of ordinary text starts `[7]`), and it is the whole
/// defence — the card states the rule once and it never needs to
/// change per caller.
fn escape_untrusted(text: &str) -> String {
    // Specifically `[<digits>]`, matching the real event-id shape — not
    // any bracketed text. `[TODO] fix this` is ordinary content and
    // must not pay an escaping cost that only real ids need.
    let looks_like_harness_line = |line: &str| -> bool {
        let Some(rest) = line.strip_prefix('[') else {
            return false;
        };
        match rest.find(']') {
            Some(i) => i > 0 && rest[..i].bytes().all(|b| b.is_ascii_digit()),
            None => false,
        }
    };
    if !text.lines().any(looks_like_harness_line) {
        // Fast, allocation-free path for the overwhelmingly common
        // case: no line looks like a harness statement.
        return text.to_owned();
    }
    text.lines()
        .map(|line| {
            if looks_like_harness_line(line) {
                format!("\\{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `Post`'s sender, for the `[id] from <label>: text` line. The user
/// has no branch of their own (`Author` doc: "they speak *inside*
/// branches"), so their label is fixed; an agent's is its branch name
/// when it has one, else a plain fallback that still names the id —
/// never silently blank.
fn author_label(tree: &Tree, from: Author) -> String {
    match from {
        Author::User => "user".to_owned(),
        Author::Harness => "harness".to_owned(),
        Author::Agent(id) => tree
            .branch_name(id)
            .unwrap_or_else(|| format!("agent {}", id.as_u64())),
    }
}

/// The short, stable checksum a compaction op's `label` is checked
/// against (`compaction.rs`'s `CompactionOp::label`) — the event's own
/// *kind*, not anything derived from its content. Kept here, beside the
/// rendering that consults the same rows, rather than duplicated in
/// `compaction.rs`.
pub(crate) fn label_of(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::Message(Message::Turn { .. }) => "turn",
        EventPayload::Message(Message::Post { .. }) => "post",
        EventPayload::Note { .. } => "note",
        EventPayload::Fork { .. } => "fork",
        EventPayload::Return { .. } => "return",
        EventPayload::Condition { .. } => "condition",
        _ => "event",
    }
}

/// A compacted row's rendered line: `[id] label: text`, `text` falling
/// back to [`crate::compaction::REMOVED_MARKER`] when the op was a bare
/// removal. The same shape an ordinary `Note`/`Post` line has — a
/// compacted row is lossy, not distinguished-looking, which is the
/// point: nothing about its rendering tells the model it is missing
/// anything it is entitled to ask for by id.
fn compacted_line(id: EventId, shadow: &CompactedView) -> String {
    format!(
        "[{}] {}: {}",
        id.as_u64(),
        shadow.label,
        shadow
            .text
            .as_deref()
            .unwrap_or(crate::compaction::REMOVED_MARKER)
    )
}

/// A compacted **program**'s rendered turn: still valid JavaScript,
/// still carrying its own id, saying how to fetch the original. Never a
/// non-assistant stub — that is what keeps role alternation intact
/// under compaction with no special case (doc 22, `Compacted`'s own
/// `types.rs` doc comment): whatever occupies the assistant's slot in
/// the rendered transcript is still an assistant turn, just one whose
/// entire body is a comment.
fn compacted_program_comment(id: EventId, shadow: &CompactedView) -> String {
    match &shadow.text {
        None => format!(
            "//: [{}] {} — compacted; fetch the original via artifact({})",
            id.as_u64(),
            shadow.label,
            id.as_u64()
        ),
        Some(text) => format!(
            "//: [{}] {}: {} — fetch the original via artifact({})",
            id.as_u64(),
            shadow.label,
            text,
            id.as_u64()
        ),
    }
}

/// One event's line in whatever user turn it lands in, for everything
/// *except* a `Turn` (an assistant message of its own, handled directly
/// by [`render`]) and a `Return`/`Condition` (whose chat-visible form is
/// the completion report `render` inserts via `report::derive_report`,
/// not a line of this shape). Consults `compacted` first, for any event
/// kind: a compacted row renders as its shadow regardless of what it
/// originally was.
fn pending_line(
    tree: &Tree,
    leaf: EventId,
    event: &Event,
    compacted: &HashMap<EventId, CompactedView>,
) -> Option<String> {
    if let Some(shadow) = compacted.get(&event.id) {
        return Some(compacted_line(event.id, shadow));
    }
    match &event.payload {
        EventPayload::Message(msg @ Message::Post { from, .. }) => {
            let resolved = tree.resolve(msg);
            let Message::Post { origin, .. } = &resolved else {
                unreachable!("resolve() never changes a Post's variant")
            };
            let text = origin.direct().map(|(t, _, _)| t).unwrap_or("");
            // `post` first, the author in parentheses after it. The
            // word straight after the id is the row's **label**, which
            // `remove_history`/`rewrite_history` check as a checksum —
            // and `[62] from agent 1: …` made the label read as "from
            // agent 1". A live compaction program on 2026-09-16 did
            // exactly that and was rejected: "#62 is a `post`, not a
            // `from agent 1`". It had picked the right row and read the
            // label off the row, which is the only place it could.
            Some(format!(
                "[{}] post ({}): {}",
                event.id.as_u64(),
                author_label(tree, *from),
                escape_untrusted(text)
            ))
        }
        EventPayload::Note { text } => Some(format!(
            "[{}] note: {}",
            event.id.as_u64(),
            escape_untrusted(text)
        )),
        // Renders to chat as a harness line (`types.rs`'s own doc
        // comment on `Fork`) — `report::render_fork` already carries the
        // real logic (settled vs. mid-program fork point, which branch
        // the pre-fork questions stayed with), so this calls into it
        // rather than re-deriving a second, thinner rendering.
        EventPayload::Fork { .. } => Some(crate::report::render_fork(tree, leaf, event.id)),
        // Everything else — `Call`, `Result`, `Console`, `Answer`,
        // `Rename`, `Agent`, and a `Compacted` event encountered at its
        // *own* log position (it shadows its target's row, not a row of
        // its own) — renders to chat: no.
        _ => None,
    }
}

/// Render one branch's document: its system prompt, then its path
/// folded into role-alternating turns (Step B1). This is the whole
/// grouping fold (Step B1b): walk the branch's own segment of
/// `spine.leaf_id`'s path in order, accumulate lines for the open user
/// turn, and flush it into an assistant turn every time a depth-0
/// `Turn` is reached.
///
/// **The system prompt comes from `spine.context().system`, never a
/// caller-supplied string.** `EventPayload::Agent.system` is snapshotted
/// once, at the branch's root, precisely so a later card edit or
/// registry change cannot alter an *existing* conversation's cached
/// prefix (`types.rs`'s own doc comment on `Agent`: "the deliberate
/// exception to 'nothing regenerable is stored'"). Taking a `card: &str`
/// parameter here instead would reopen exactly that hazard by letting a
/// caller pass today's card into a request for a branch rooted on
/// yesterday's — this function has no way to tell the difference, so it
/// does not accept the possibility at all.
///
/// **Depth-derived filtering, not a stored flag.** A `Turn` at handler
/// depth > 0 — a deliberation, still being decided — contributes
/// nothing here at all (doc 22: "a deliberating handler never enters
/// the document"); neither does anything it does while running. Only a
/// `Condition{disposition: Handover}` or a `Return` reached while depth
/// is already 0 is this branch's own program truly handing back, which
/// is when its completion report (`report::derive_report`) is inserted
/// — a `Pushed` condition at depth 0 means a nested handler is about to
/// run and produces no report yet, because nothing chat-visible has
/// happened on *this* branch until that handler resolves. `depth_after`
/// is the single fold this filter shares with `tree::programs_for`'s
/// own depth field, by construction rather than by convention.
///
/// `budget` is threaded straight to `report::derive_report` for the
/// completion-report sections it renders (the answer-into-context
/// clip) — it is **not** on `Context`, because it is per-agent
/// configurable (`agent({ budget })`, `machine.rs`) rather than part of
/// the committed chat-history shape `types.rs` defines, so it has to
/// arrive as a parameter from whichever caller already tracks it
/// (`host/mod.rs`'s session state) rather than be smuggled onto a type
/// that has no field for it.
///
/// Ephemeral, one-request-only content (a parse-repair diagnostic, a
/// "someone is attached" presence line) is **not** a parameter here —
/// it is never part of the log, so baking it into `render` would make
/// this function's output depend on something the log can't reproduce.
/// Apply it after, via [`Document::with_tail`].
///
/// Infallible: the two defensive errors the POC's `document.rs` used to
/// return (`AdjacentPrograms`, `ProgramWithNothingBefore`) described a
/// caller-assembled row vector that could be built wrong. Reading
/// straight from the log removes that possibility rather than checking
/// for it — every depth-0 program's own outcome auto-populates the
/// pending turn before the next `Turn` can appear, by construction of
/// this fold, so "two programs adjacent" is no longer representable.
pub fn render(tree: &Tree, spine: &Spine, budget: usize) -> Document {
    let leaf = spine.leaf_id;
    let agent = tree
        .enclosing_agent(leaf)
        .expect("a spine's leaf always has an enclosing Agent — spine_at() built it from one");
    let card = &spine.context().system;
    render_with_lookup(
        tree,
        agent,
        leaf,
        card,
        budget,
        &tree.compacted_lookup(leaf),
    )
}

/// [`render`], but against an explicit compaction lookup instead of one
/// derived from `tree` — the hook `compaction.rs` needs to answer "how
/// big would the document be if this proposed batch were already
/// applied", without appending anything to the log to find out.
/// `render` is the common case (a real request, against what is
/// actually logged) and stays the public entry point; this is the one
/// fold underneath both of them, so a compaction dry-run and a real
/// request can never silently diverge on how a row renders.
pub(crate) fn render_with_lookup(
    tree: &Tree,
    agent: EventId,
    leaf: EventId,
    card: &str,
    budget: usize,
    compacted: &HashMap<EventId, CompactedView>,
) -> Document {
    let transport = transport();
    let mut messages = vec![ChatMessage::text(ChatRole::System, card.to_owned())];
    messages.extend(worked_examples());
    let mut pending: Vec<String> = Vec::new();
    let mut cur_agent: Option<EventId> = None;
    let mut depth: usize = 0;
    // `Transport::RunProgram` only: the id of the most recent turn's
    // `run_program` call, still unanswered. `flush_pending` consumes it
    // whenever the next block closes — the harness's report on what
    // that turn did becomes the `Tool`-role answer to *this* call, never
    // an ordinary `User` message, because the wire format requires a
    // tool-calling assistant turn to be answered before anything else
    // may follow it. `None` before the first turn (so the very first
    // block, whatever led up to it, still renders as `User` in both
    // modes) and again under `Transport::Program`, which never opens a
    // call at all.
    let mut open_call: Option<String> = None;

    for ev in tree.path_events(leaf) {
        if let EventPayload::Agent { .. } = ev.payload {
            cur_agent = Some(ev.id);
            continue;
        }
        if cur_agent != Some(agent) {
            continue;
        }
        if depth == 0 {
            match &ev.payload {
                EventPayload::Message(Message::Turn { source, .. }) => {
                    let content = match compacted.get(&ev.id) {
                        None => source.clone(),
                        Some(shadow) => compacted_program_comment(ev.id, shadow),
                    };
                    messages.push(flush_pending(&mut pending, transport, &mut open_call));
                    let (assistant, call_id) = assistant_turn(transport, ev.id, content);
                    messages.push(assistant);
                    open_call = call_id;
                }
                EventPayload::Return { .. } => {
                    pending.push(crate::report::derive_report(tree, leaf, ev.id, budget));
                }
                EventPayload::Condition { disposition, .. } => {
                    if *disposition == Disposition::Handover {
                        pending.push(crate::report::derive_report(tree, leaf, ev.id, budget));
                    }
                }
                _ => {
                    if let Some(line) = pending_line(tree, leaf, ev, compacted) {
                        pending.push(line);
                    }
                }
            }
        }
        depth = depth_after(depth, &ev.payload);
    }

    if !pending.is_empty() {
        messages.push(flush_pending(&mut pending, transport, &mut open_call));
    }

    Document { messages }
}

/// The assistant's own turn, in whichever shape `transport` wants.
/// `Transport::Program` sends bare `content` — the model's whole
/// response *is* the program, `host/deepseek.rs`'s own substitution
/// table. `Transport::RunProgram` wraps the same program in a
/// `run_program` call instead, with empty prose `content`: nothing of
/// what the model said *alongside* the call was ever stored
/// (`Message::Turn` has a `source` field and no other), so there is
/// nothing truthful to replay there.
///
/// Returns the id of the call the next pending block should answer —
/// `Some` only for `RunProgram`, threaded back into `open_call` by the
/// caller so [`flush_pending`] knows what it is closing.
fn assistant_turn(
    transport: Transport,
    id: EventId,
    source: String,
) -> (ChatMessage, Option<String>) {
    match transport {
        Transport::Program => (ChatMessage::text(ChatRole::Assistant, source), None),
        Transport::RunProgram => {
            let call_id = format!("call_{}", id.as_u64());
            let message = ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: call_id.clone(),
                    source,
                }]),
                tool_call_id: None,
            };
            (message, Some(call_id))
        }
    }
}

/// Close out `pending` into the one message that answers whatever
/// precedes it, and clear both `pending` and `open_call` for the next
/// block. The first block ever (before any turn — `open_call` still
/// `None`) and every block under `Transport::Program` render as an
/// ordinary `User` message, exactly [`render`]'s pre-`Transport` shape.
/// A later block under `Transport::RunProgram` instead answers the
/// still-open call as a `Tool`-role message — see `assistant_turn` and
/// `open_call`'s own doc comment above.
fn flush_pending(
    pending: &mut Vec<String>,
    transport: Transport,
    open_call: &mut Option<String>,
) -> ChatMessage {
    let content = pending.join("\n");
    pending.clear();
    match (transport, open_call.take()) {
        (Transport::RunProgram, Some(id)) => ChatMessage {
            role: ChatRole::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(id),
        },
        _ => ChatMessage::text(ChatRole::User, content),
    }
}

/// The card's worked examples, as **real alternating turns** ahead of
/// the conversation — a request in the user role, the program that
/// answered it in the assistant role.
///
/// They were briefly rendered as prose quoted inside the system
/// message, on the reasoning that a synthetic turn is a document row
/// with no event behind it and `22_ONE_VOCABULARY.md` says every row is
/// exactly one event. That was clean and measurably wrong: the first
/// live runs of the real session lost both behaviours the examples
/// exist to induce — programs went back to reading a file and stopping
/// without acting, and `ask`/`raise` were not reached for once across
/// ten task runs. A worked example in the assistant role is a far
/// stronger signal than the same bytes quoted in a system prompt.
///
/// The invariant is not violated, because these are not conversation
/// rows at all: they are **preamble**, in the same class as the system
/// message, which is likewise not an event and carries no id. Seeding
/// them as real events was considered and rejected — `programs_for`
/// would list them as programs that ran, compaction could delete them,
/// and the eval's own `round_trips` fold would count them as turns the
/// model took. None of that is true of a prompt.
///
/// Each request is marked inline so the model can tell an example from
/// its own history, and the pairs alternate strictly, so the
/// conversation's own first user turn continues the alternation with
/// no special case.
///
/// **`Transport::RunProgram` renders three rows per exemplar, not two.**
/// The wire format requires a tool-calling assistant message to be
/// followed by a `Tool`-role answer before anything else — the same
/// rule `flush_pending` enforces for the real conversation — so an
/// exemplar's `run_program` call needs a stand-in result behind it or
/// the preamble itself would be malformed under this transport. The
/// stand-in is a fixed placeholder, never a real report: nothing ran, so
/// there is nothing truthful to report. `card::seed_exemplars()` itself
/// — the FILES this reads from — is untouched by which transport is
/// active; only this rendering is.
fn worked_examples() -> Vec<ChatMessage> {
    let transport = transport();
    crate::card::seed_exemplars()
        .iter()
        .enumerate()
        .flat_map(|(i, ex)| {
            let request = ChatMessage::text(
                ChatRole::User,
                format!("[worked example, not this conversation] {}", ex.user),
            );
            // Marked on **both** sides (both the request above and the
            // program below, whichever wire shape carries it). Only the
            // request used to carry the marker, which meant that once a
            // provider's chat template flattened these into one token
            // stream, the model saw N assistant turns indistinguishable
            // from its own prior output — an apparent history of
            // programs it had already written, every one of them short
            // and single-purpose. That is a demonstration of the wrong
            // thing, delivered in the most persuasive position
            // available: its own mouth.
            let program = format!("// [worked example]\n{}", ex.assistant);
            match transport {
                Transport::Program => {
                    vec![request, ChatMessage::text(ChatRole::Assistant, program)]
                }
                Transport::RunProgram => {
                    let call_id = format!("call_example_{i}");
                    let assistant = ChatMessage {
                        role: ChatRole::Assistant,
                        content: String::new(),
                        tool_calls: Some(vec![ToolCall {
                            id: call_id.clone(),
                            source: program,
                        }]),
                        tool_call_id: None,
                    };
                    let result = ChatMessage {
                        role: ChatRole::Tool,
                        content: "[worked example] ok".to_owned(),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                    };
                    vec![request, assistant, result]
                }
            }
        })
        .collect()
}

/// Strip a single leading/trailing code fence if the **whole** trimmed
/// completion is wrapped in one — never a fence appearing mid-text,
/// which is left alone per phase 20 doc Step A1: "reserve the
/// parse-failure condition for genuine syntax errors."
///
/// The card states absolutely that a completion is only ever valid
/// JavaScript — no fence, no surrounding prose. That is self-enforcing
/// (a completion that doesn't parse is already a condition with a
/// handler), but a model habitually wraps its answer in a ```javascript
/// fence anyway. `extract_program` tolerates exactly that one habit,
/// silently, and nothing else: it does not hunt for prose, does not try
/// to salvage a program buried in an explanation, and does not
/// advertise the leniency anywhere the model can see it.
///
/// Recognizes an optional language tag on the opening fence
/// (```javascript, ```js, or bare ```) and requires a matching closing
/// ``` as the last non-blank line, so a program that legitimately
/// contains a ``` in a string or comment is not mis-stripped.
pub fn extract_program(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return raw.to_owned();
    };
    // The rest of the opening fence line is a language tag (or
    // nothing) — skip to the first newline.
    let Some(nl) = after_open.find('\n') else {
        return raw.to_owned();
    };
    let tag = after_open[..nl].trim();
    if !(tag.is_empty() || tag.eq_ignore_ascii_case("javascript") || tag.eq_ignore_ascii_case("js"))
    {
        return raw.to_owned();
    }
    let body = &after_open[nl + 1..];
    let Some(body) = body.strip_suffix("```") else {
        return raw.to_owned();
    };
    body.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- extract_program (folded in from the deleted fence.rs) ---

    #[test]
    fn no_fence_is_returned_unchanged() {
        assert_eq!(extract_program("const x = 1;"), "const x = 1;");
    }

    #[test]
    fn a_stray_javascript_fence_is_stripped_silently() {
        assert_eq!(
            extract_program("```javascript\nconst x = 1;\n```"),
            "const x = 1;"
        );
    }

    #[test]
    fn a_bare_fence_with_no_language_tag_is_stripped() {
        assert_eq!(extract_program("```\nconst x = 1;\n```"), "const x = 1;");
    }

    #[test]
    fn js_tag_is_also_recognized() {
        assert_eq!(extract_program("```js\nconst x = 1;\n```"), "const x = 1;");
    }

    #[test]
    fn a_fence_appearing_mid_text_is_left_alone() {
        // Not wrapped end-to-end — this is a genuine syntax error to
        // report as a trap, not something to salvage.
        let raw = "const s = \"```\";\ntell(s);";
        assert_eq!(extract_program(raw), raw);
    }

    #[test]
    fn an_unmatched_opening_fence_is_left_alone() {
        let raw = "```javascript\nconst x = 1;";
        assert_eq!(extract_program(raw), raw);
    }

    #[test]
    fn a_fence_with_an_unrecognized_tag_is_left_alone() {
        let raw = "```python\nx = 1\n```";
        assert_eq!(extract_program(raw), raw);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            extract_program("  \n```javascript\nconst x = 1;\n```\n  "),
            "const x = 1;"
        );
    }

    // --- render, over a real Tree ---

    fn turn(source: &str) -> EventPayload {
        EventPayload::Message(Message::Turn {
            author: Author::Agent(EventId::new(1)),
            source: source.to_owned(),
            thinking: None,
            usage: None,
        })
    }

    fn user_post(text: &str) -> EventPayload {
        EventPayload::Message(Message::Post {
            from: Author::User,
            origin: Origin::Direct {
                text: text.to_owned(),
                input: serde_json::Value::Null,
                expects_reply: true,
            },
        })
    }

    /// The card, one user post, one completed program: card / user /
    /// assistant / user (the completion report), in that order, and the
    /// program's own source rendered bare — never wrapped in a
    /// function, since the model's own prior turns are its few-shot
    /// evidence for what to write next.
    #[test]
    fn a_completed_program_renders_card_user_assistant_user() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
        tree.append(&mut spine, user_post("hello")).unwrap();
        tree.append(&mut spine, turn("tell('hi'); return 1;"))
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!(1),
            },
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024);
        assert_eq!(doc.messages[0].role, ChatRole::System);
        assert_eq!(doc.messages[0].content, "CARD");
        let conv = doc.conversation();
        assert_eq!(conv.len(), 3);
        assert_eq!(conv[0].role, ChatRole::User);
        assert!(conv[0].content.contains("hello"));
        assert_eq!(conv[1].role, ChatRole::Assistant);
        assert_eq!(conv[1].content, "tell('hi'); return 1;");
        assert_eq!(conv[2].role, ChatRole::User);
    }

    /// A `Turn` at handler depth > 0 — a deliberation still being
    /// decided — contributes nothing to the document: not the turn
    /// itself, not anything it does while running. Only once the
    /// raising program's own outcome lands (here, its `Return` after
    /// the nested handler resumed it) does a report appear.
    #[test]
    fn a_pushed_deliberation_never_enters_the_document() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        tree.append(&mut spine, turn("raise('x');")).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Condition {
                cause: Cause::Raised {
                    name: "x".into(),
                    payload: None,
                },
                site: 0,
                stack: Vec::new(),
                disposition: Disposition::Pushed,
            },
        )
        .unwrap();
        // The handler: its own Turn and Return, both at depth 1.
        tree.append(&mut spine, turn("return resume(1);")).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!({}),
            },
        )
        .unwrap();
        // Back at depth 0: the raising program resumes and returns.
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!(2),
            },
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024);
        // card, user("go"), assistant("raise('x');"), user(report) —
        // never the handler's own turn.
        assert_eq!(doc.conversation().len(), 3);
        assert_eq!(doc.conversation()[1].content, "raise('x');");
        assert!(
            !doc.messages.iter().any(|m| m.content.contains("resume(1)")),
            "the handler's own deliberation must never reach this document: {doc:?}"
        );
    }

    /// A compacted program renders as a comment-only assistant turn —
    /// still valid JavaScript, still carrying its own id — never as a
    /// non-assistant stub, which is what keeps role alternation intact
    /// under compaction with no special case.
    #[test]
    fn a_compacted_program_renders_as_a_comment_only_assistant_turn() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        let program = tree.append(&mut spine, turn("1 + 1;")).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!(2),
            },
        )
        .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Compacted {
                of: program,
                label: "turn".into(),
                text: None,
            },
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024);
        let compacted_turn = &doc.conversation()[1];
        assert_eq!(compacted_turn.role, ChatRole::Assistant);
        assert!(compacted_turn.content.starts_with("//:"));
        assert!(
            compacted_turn
                .content
                .contains(&program.as_u64().to_string())
        );
        interp::compile(&compacted_turn.content)
            .expect("a compacted program's turn is still valid JavaScript");
    }
    /// **A row's rendered label is the one its compaction checksum
    /// expects.** `remove_history(id, label)` and
    /// `rewrite_history(id, label, value)` check the label against the
    /// row, and the only place a program can read a label is the row as
    /// rendered here — so if the two disagree, correct work is rejected
    /// and the rejection blames the program.
    ///
    /// That is not hypothetical: on 2026-09-16 a post rendered as
    /// `[62] from agent 1: …`, a live compaction program duly called
    /// `remove_history(62, "from agent 1")`, and the checksum answered
    /// "#62 is a `post`, not a `from agent 1`". Every test passed; none
    /// of them compared the two strings.
    #[test]
    fn a_rendered_row_carries_the_label_its_checksum_expects() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "go".into(),
                    input: serde_json::Value::Null,
                    expects_reply: false,
                },
            }),
        )
        .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Note {
                text: "remembered".into(),
            },
        )
        .unwrap();

        let compacted = tree.compacted_lookup(spine.leaf_id);
        let mut checked = 0;
        for ev in tree.path_events(spine.leaf_id) {
            let Some(line) = pending_line(&tree, spine.leaf_id, ev, &compacted) else {
                continue;
            };
            let Some(after_id) = line.split_once("] ") else {
                continue;
            };
            let rendered = after_id.1.split([':', ' ']).next().unwrap_or("");
            assert_eq!(
                rendered,
                label_of(&ev.payload),
                "row renders as {rendered:?} but its checksum wants {:?}: {line}",
                label_of(&ev.payload)
            );
            checked += 1;
        }
        assert!(checked >= 2, "exercised {checked} rows");
    }

    // --- Transport switch ---

    /// Runs `f` with `AGENT2_TRANSPORT` set to `value`, restoring
    /// whatever was there before (or its absence) once `f` returns — so
    /// one test flipping the switch can never leak into a sibling's run.
    /// Relies on this crate's own documented gate
    /// (`cargo test -p agent -- --test-threads=1`) running the binary
    /// single-threaded: a per-process env var has no other safe way to
    /// be scoped to one test.
    fn with_transport<T>(value: &str, f: impl FnOnce() -> T) -> T {
        let key = "AGENT2_TRANSPORT";
        let prev = std::env::var(key).ok();
        // SAFETY: single-threaded test binary — see doc comment above.
        unsafe { std::env::set_var(key, value) };
        let result = f();
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        result
    }

    fn sample_document() -> Document {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
        tree.append(&mut spine, user_post("hello")).unwrap();
        tree.append(&mut spine, turn("tell('hi'); return 1;"))
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!(1),
            },
        )
        .unwrap();
        render(&tree, &spine, 64 * 1024)
    }

    /// Pins today's shape: the model's whole response rides bare in
    /// `content`, no tool wrapper, and the report that follows it is an
    /// ordinary `User` message — [`render`]'s behaviour before this
    /// transport switch existed, and what `Transport::Program` must
    /// still produce byte-for-byte now that a second mode exists beside
    /// it.
    #[test]
    fn program_mode_renders_an_assistant_turn_as_plain_text() {
        // `doc.conversation()` recomputes the preamble length from
        // `worked_examples()` — which itself reads `transport()` fresh
        // (its own doc comment) — so it has to run inside the same
        // `with_transport` scope that built `doc`, not after: reading it
        // back once the guard has restored the env would slice the
        // preamble at the *other* mode's length.
        with_transport("program", || {
            let doc = sample_document();
            let conv = doc.conversation();
            assert_eq!(conv[1].role, ChatRole::Assistant);
            assert_eq!(conv[1].content, "tell('hi'); return 1;");
            assert!(
                conv[1].tool_calls.is_none(),
                "Transport::Program never wraps a turn in a tool call: {conv:?}"
            );
            assert_eq!(
                conv[2].role,
                ChatRole::User,
                "the report stays a plain User message under Transport::Program"
            );
        });
    }

    /// `Transport::RunProgram`'s whole point: the same turn now arrives
    /// as a `run_program` call (its `source` the program, unchanged),
    /// and the report that follows answers that call in the `Tool` role
    /// — never a `User` message, per the wire format's own rule that a
    /// tool-calling assistant turn must be answered before anything else
    /// follows it.
    #[test]
    fn run_program_mode_renders_an_assistant_turn_as_a_tool_call() {
        with_transport("run_program", || {
            let doc = sample_document();
            let conv = doc.conversation();
            assert_eq!(conv[1].role, ChatRole::Assistant);
            let calls = conv[1]
                .tool_calls
                .as_ref()
                .expect("Transport::RunProgram wraps the turn in a run_program call");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].source, "tell('hi'); return 1;");
            assert_eq!(
                conv[2].role,
                ChatRole::Tool,
                "the report answers the call under Transport::RunProgram: {conv:?}"
            );
            assert_eq!(conv[2].tool_call_id.as_deref(), Some(calls[0].id.as_str()));
        });
    }

    /// **The point of the exercise.** Two transports, one branch: the
    /// actual program and report text a model would see must be
    /// identical whichever container carries it, so a later comparison
    /// between the two measures the container and nothing else. Strict
    /// on purpose — this reads the payload back out of whichever field
    /// each transport put it in (`content` under `Program`, a lone
    /// `tool_calls[0].source` under `RunProgram`) and demands the two
    /// sequences match exactly, row for row.
    #[test]
    fn both_modes_carry_the_same_content() {
        fn build() -> (Tree, Spine) {
            let mut tree = Tree::new(None);
            let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
            tree.append(&mut spine, user_post("hello")).unwrap();
            tree.append(&mut spine, turn("tell('hi'); return 1;"))
                .unwrap();
            tree.append(
                &mut spine,
                EventPayload::Return {
                    value: serde_json::json!(1),
                },
            )
            .unwrap();
            tree.append(&mut spine, user_post("again")).unwrap();
            tree.append(&mut spine, turn("return 2;")).unwrap();
            tree.append(
                &mut spine,
                EventPayload::Return {
                    value: serde_json::json!(2),
                },
            )
            .unwrap();
            (tree, spine)
        }

        fn payload(m: &ChatMessage) -> String {
            match &m.tool_calls {
                Some(calls) => {
                    assert_eq!(calls.len(), 1, "run_program is the only tool on offer");
                    calls[0].source.clone()
                }
                None => m.content.clone(),
            }
        }

        let (program_tree, program_spine) = build();
        let (rp_tree, rp_spine) = build();
        // `.conversation()` has to run inside each transport's own
        // scope — see `program_mode_renders_an_assistant_turn_as_plain_text`'s
        // doc comment — so each branch collects its payload before the
        // guard restores the env.
        let program_content: Vec<String> = with_transport("program", || {
            render(&program_tree, &program_spine, 64 * 1024)
                .conversation()
                .iter()
                .map(payload)
                .collect()
        });
        let rp_content: Vec<String> = with_transport("run_program", || {
            render(&rp_tree, &rp_spine, 64 * 1024)
                .conversation()
                .iter()
                .map(payload)
                .collect()
        });

        assert_eq!(
            program_content.len(),
            rp_content.len(),
            "RunProgram must not merge or drop a row to make the text line up by accident"
        );
        assert_eq!(
            program_content, rp_content,
            "the two transports must carry identical content — only the container differs"
        );
    }
}

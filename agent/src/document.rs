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

use std::collections::{HashMap, HashSet};

use crate::tree::CompactedView;
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
/// Chosen once per process from `AGENT2_TRANSPORT`
/// ([`configured_transport`]) and then **passed as a value** — into
/// [`render`], recorded on the [`Document`] it produces, and read back
/// off that document by `host/deepseek.rs`. It is not re-read from the
/// environment anywhere downstream, which is what makes it impossible
/// for one request to be *rendered* under one container and *sent*
/// under the other.
///
/// It used to be an ambient `AGENT2_*` lookup on every call, like
/// `host/mod.rs`'s `document_budget`/`compaction_headroom`. That is a
/// safe idiom for a value only ever *read*; this one had to vary per
/// test, and the only way to vary a process-global from a test is to
/// write the environment variable, which Rust 2024 makes `unsafe`
/// precisely because it is undefined behaviour once any other thread is
/// running. `cargo test` runs one thread per core, so those writes
/// raced every concurrent test that rendered a document — most of the
/// suite. The visible symptom was
/// `program_mode_renders_an_assistant_turn_as_plain_text` asserting an
/// *empty* assistant message, because the other transport had moved the
/// program into a tool call and left `content` blank.
///
/// Threading the value fixes that at the source rather than by
/// serialising the suite: a test names the transport it means, in an
/// argument, and nothing global moves.
///
/// The event log is identical in shape either way: same `Turn`/`Call`/
/// `Result`/`Condition` payloads, same `Message::Turn.source` — only
/// this module's rendering and `host/deepseek.rs`'s wire-facing code
/// branch on it at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Program,
    RunProgram,
    /// Phase 25: the completion is **markdown containing executable code
    /// blocks**. Prose is prose and reaches the person as it streams; the
    /// ```js cells are one compilation that pauses at each block, sharing a
    /// frame and a scope (`docs/25_NOTEBOOK.md`).
    Notebook,
}

pub const DEFAULT_TRANSPORT: Transport = Transport::Program;

/// The process's transport, read from `AGENT2_TRANSPORT` **once** and
/// cached. This is the process entry point for the setting: real
/// callers (`main.rs`, the session loop, `score.rs`) ask here and then
/// pass the answer down as a value.
///
/// Once, not per call, and that is the whole point. A per-call read is
/// a process-global that anything can observe mid-render, so varying it
/// in a test meant writing the environment variable underneath every
/// other running test — UB under threads, and the cause of a real race
/// across this suite (see [`Transport`]).
/// A `OnceLock` makes the environment a *start-up* input: it is read
/// before any document exists, and no later read can disagree with an
/// earlier one. Tests never come here at all; they name a [`Transport`]
/// directly.
///
/// An unset or unrecognized value falls back to [`DEFAULT_TRANSPORT`],
/// the same "garbage in, quiet default" rule the other `AGENT2_*`
/// readers use (`llm_concurrency`'s `filter(|n| *n >= 1)`,
/// `compaction_headroom`'s open-interval filter) rather than a run
/// failing to start over a typo'd env var.
pub fn configured_transport() -> Transport {
    static CONFIGURED: std::sync::OnceLock<Transport> = std::sync::OnceLock::new();
    *CONFIGURED.get_or_init(|| match std::env::var("AGENT2_TRANSPORT").as_deref() {
        Ok("run_program") => Transport::RunProgram,
        Ok("notebook") => Transport::Notebook,
        _ => DEFAULT_TRANSPORT,
    })
}

/// A rendered request, transport-agnostic (Part A: "the document is
/// the interface"). `messages[0]` is always the card, in `System`
/// (Step A1: "the card goes in `system`").
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Document {
    pub messages: Vec<ChatMessage>,
    /// How many leading messages are preamble — the system message plus
    /// the worked-example turns. Recorded when the document is built,
    /// because it is a fact about *this* document and nothing else can
    /// recover it: it depends on the agent's snapshotted exemplars and
    /// on the transport, and [`conversation`] used to re-derive it by
    /// calling `worked_examples()` against today's card and whatever
    /// transport happened to be configured when it was asked. Slicing
    /// with a number computed from the wrong card — or the wrong
    /// container — is how a caller silently reads the tail of the
    /// preamble as the first real turn.
    ///
    /// [`conversation`]: Document::conversation
    pub preamble: usize,
    /// Which container this document was rendered for. Recorded for the
    /// same reason as `preamble`, and in fact it is *why* `preamble`
    /// varies: a `RunProgram` exemplar renders three rows where a
    /// `Program` one renders two.
    ///
    /// `host/deepseek.rs` reads it here rather than asking the
    /// environment again on its way to the wire. Those were once two
    /// independent reads of one global, held in agreement only by a
    /// comment promising they "can never drift onto different values
    /// mid-session" — a promise nothing enforced. Carrying the value on
    /// the document makes the request and the wire format it is sent
    /// under the same fact, so there is nothing left to keep in sync.
    pub transport: Transport,
}

impl Document {
    /// The conversation proper — everything after the preamble (the
    /// system message and the worked examples). What a caller reasoning
    /// about *this branch's* history wants, as opposed to what is sent
    /// on the wire.
    ///
    /// The preamble is a prefix whose length depends on the agent's own
    /// snapshotted exemplars and on the transport, so it is *recorded*
    /// rather than recomputed — positional indexing into `messages` is
    /// a latent break in anything that means "the first real turn."
    pub fn conversation(&self) -> &[ChatMessage] {
        &self.messages[self.preamble.min(self.messages.len())..]
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
pub(crate) fn escape_untrusted(text: &str) -> String {
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

/// Where each program's literal `tell`/`ask` calls sit in its own
/// source, keyed by the turn that wrote them.
///
/// Built in one pass with the same agent filter the fold uses, because
/// a span is only meaningful against the program it was compiled from:
/// scanning every event on the path matched one agent's call site
/// against another agent's source, which is how a broadcast test found
/// a 239-byte offset into a 238-byte program.
fn told_literal_cuts(tree: &Tree, agent: EventId, leaf: EventId) -> HashMap<EventId, Vec<Cut>> {
    let mut out: HashMap<EventId, Vec<Cut>> = HashMap::new();
    let mut cur_agent: Option<EventId> = None;
    let mut turn: Option<(EventId, String)> = None;
    for ev in tree.path_events(leaf) {
        if let EventPayload::Agent { .. } = ev.payload {
            cur_agent = Some(ev.id);
            continue;
        }
        if cur_agent != Some(agent) {
            continue;
        }
        match &ev.payload {
            EventPayload::Message(Message::Turn { source, .. }) => {
                turn = Some((ev.id, source.clone()));
            }
            EventPayload::Note {
                text,
                site,
                site_end,
            } => {
                let Some((id, src)) = &turn else { continue };
                push_cut(&mut out, *id, src, *site, *site_end, text, ev.id.as_u64());
            }
            EventPayload::Call(Call::Send {
                text,
                site,
                site_end,
                expects_reply,
                ..
            }) => {
                let _ = expects_reply;
                let Some((id, src)) = &turn else { continue };
                push_cut(&mut out, *id, src, *site, *site_end, text, ev.id.as_u64());
            }
            _ => {}
        }
    }
    out
}

/// A reply reconstructed for rendering: the completion verbatim, plus the
/// cuts of all its cells shifted into that text's coordinates.
struct ReplyRender {
    text: String,
    cuts: Vec<Cut>,
}

/// **Group a notebook run's `Turn`s into the replies they came from**, and
/// pair each reply with the completion text that produced it.
///
/// A reply is N cells and one completion (D7, D15). Its cells reach the log
/// as `Turn`s holding bare JavaScript and its prose as `Send`s, so rendering
/// the turn back from those pieces showed the model a series of bare
/// programs — its own context teaching it the opposite of the card that had
/// just told it to write markdown. `EventPayload::Completion` carries the
/// bytes; this puts them back where the reply was.
///
/// Returns the render keyed by the reply's **first** `Turn` (where the
/// assistant message goes) and the set of later `Turn`s it already covers
/// (which render nothing of their own).
///
/// Three cases are deliberately left to the per-`Turn` path instead:
///
/// - **A reply with no `Completion`.** Truncated mid-stream, killed, or a
///   log written before the text was stored. The cells are still there and
///   still render; nothing is lost, and the fallback is the behaviour this
///   whole function replaces.
/// - **A reply any of whose cells is compacted.** Compaction shortens a
///   `Turn`'s row, and replaying the full reply text over it would undo
///   exactly what it was for.
/// - **Cells that ran after their reply's `Completion`** — the tail of a
///   reply resumed from a `raise`. They arrive in a later group, so they
///   render as themselves rather than being folded into a reply already
///   drawn above them.
#[allow(clippy::type_complexity)]
fn notebook_replies(
    tree: &Tree,
    leaf: EventId,
    agent: EventId,
    cuts: &HashMap<EventId, Vec<Cut>>,
    compacted: &HashMap<EventId, CompactedView>,
) -> (HashMap<EventId, ReplyRender>, HashSet<EventId>) {
    let mut replies: HashMap<EventId, ReplyRender> = HashMap::new();
    let mut covered: HashSet<EventId> = HashSet::new();
    let mut cur_agent: Option<EventId> = None;
    // The reply being assembled: its cell `Turn`s in order, and its text
    // once seen.
    //
    // **The two are not in a fixed order.** A streamed reply logs its
    // cells as their fences close and its `Completion` at the end; a
    // batched one knows the whole text before a single cell runs and
    // logs it first. Both are the same reply, so this claims `Turn`s
    // either side of the text rather than assuming one arrangement.
    let mut group: Vec<EventId> = Vec::new();
    let mut group_text: Option<String> = None;

    // A reply is finished when its text and its cells have both been
    // seen; that is where it is recorded.
    macro_rules! flush {
        () => {
            if let Some(text) = group_text.take() {
                let turns = std::mem::take(&mut group);
                record_reply(&mut replies, &mut covered, &turns, &text, cuts, compacted);
            }
        };
    }

    for ev in tree.path_events(leaf) {
        if let EventPayload::Agent { .. } = ev.payload {
            cur_agent = Some(ev.id);
            continue;
        }
        if cur_agent != Some(agent) {
            continue;
        }
        match &ev.payload {
            EventPayload::Message(Message::Turn { .. }) => group.push(ev.id),
            // The run's terminal ends a reply — but only once its text
            // has arrived. A `raise` suspends mid-reply and logs its
            // `Condition` *before* the completion ends, so a group with
            // no text yet keeps waiting rather than falling back.
            EventPayload::Return { .. } | EventPayload::Condition { .. } => flush!(),
            EventPayload::Completion { text, .. } => {
                if text.is_empty() {
                    continue;
                }
                group_text = Some(text.clone());
                // Cells already logged: this completion is theirs.
                // Cells still to come (a batched reply): wait for the
                // terminal.
                if !group.is_empty() {
                    flush!();
                }
            }
            _ => {}
        }
    }
    flush!();
    (replies, covered)
}

/// Record one reply: its text keyed by its first cell's `Turn`, and its
/// later cells marked as already drawn.
fn record_reply(
    replies: &mut HashMap<EventId, ReplyRender>,
    covered: &mut HashSet<EventId>,
    turns: &[EventId],
    text: &str,
    cuts: &HashMap<EventId, Vec<Cut>>,
    compacted: &HashMap<EventId, CompactedView>,
) {
    if turns.is_empty() {
        return;
    }
    if turns.iter().any(|id| compacted.contains_key(id)) {
        return;
    }
    // The cells of this reply, in the same order the driver
    // fed them — the same splitter, on the same bytes, so
    // the k-th `Turn` is the k-th cell.
    let cells = crate::notebook::split_cells(text);
    let mut shifted: Vec<Cut> = Vec::new();
    for (k, id) in turns.iter().enumerate() {
        let Some(cell) = cells.get(k) else { break };
        // **A `Call::site` is cell-local** (D1): the cell's
        // offset is subtracted at log time, so a site
        // indexes its own `Turn.source`. Rendering the whole
        // reply means those offsets no longer index what is
        // being shown, and a snip would cut at the wrong
        // bytes — so the offset is added back here. It is
        // derived rather than stored: the reply's text is on
        // the log and the splitter is deterministic, so
        // where cell k starts is a fact about the bytes.
        if let Some(cell_cuts) = cuts.get(id) {
            shifted.extend(cell_cuts.iter().map(|c| Cut {
                start: c.start + cell.start,
                end: c.end + cell.start,
                row: c.row,
                literal: c.literal,
            }));
        }
    }
    replies.insert(
        turns[0],
        ReplyRender {
            text: text.to_owned(),
            cuts: shifted,
        },
    );
    covered.extend(turns.iter().skip(1).copied());
}
/// One call's span recorded against the turn that wrote it, if the span
/// is usable at all. Overlapping spans are dropped: an `ask` nested
/// inside a `tell` is one call's range inside another's, and editing
/// the outer leaves the inner pointing past the end of a string that
/// just got shorter.
fn push_cut(
    out: &mut HashMap<EventId, Vec<Cut>>,
    turn: EventId,
    src: &str,
    site: u32,
    site_end: u32,
    text: &str,
    row: u64,
) {
    let (a, b) = (site as usize, site_end as usize);
    if b <= a || b > src.len() || !src.is_char_boundary(a) || !src.is_char_boundary(b) {
        return;
    }
    let cuts = out.entry(turn).or_default();
    if cuts.iter().any(|c| a < c.end && c.start < b) {
        return;
    }
    cuts.push(Cut {
        start: a,
        end: b,
        row,
        // Only a literal can be replaced: the row already holds those
        // bytes. A computed argument is not duplication — the row has
        // the text and the source has how it was built — so it keeps
        // its construction and takes the reference alongside.
        literal: src[a..b].contains(text),
    });
}

/// A call in a program's source and the history row it produced.
#[derive(Clone)]
struct Cut {
    start: usize,
    end: usize,
    row: u64,
    literal: bool,
}

/// A program's source, cross-referenced to the history rows its calls
/// produced — and with a literal argument replaced by that reference
/// when doing so is shorter than keeping it.
///
/// **Every `tell`, `ask` and `history.append` is annotated**, whether
/// or not its text is duplicated. The row says `[40] you told user: …`
/// and the call says `/* history[40] */`, and neither on its own says
/// that *this* call produced *that* row. With several computed calls
/// in one program the link is otherwise only inferable from order.
///
/// **Replaced only when the argument is a literal and the reference is
/// shorter.** A literal is in the document twice — once as the row,
/// once inside the call — and across every run kept on 2026-09-17 that
/// was 1011 of 3344 tells and 121 of 196 asks. A computed
/// `tell("--- " + f.content)` is not duplication: the row has the
/// bytes, the source has the construction, and 87% of tells are
/// computed. And `tell("ok")` is shorter than any reference to it, so
/// it stays as written — 6% of literal calls were.
///
/// An `answer` is not in this: it renders whole as a row, but
/// `EventPayload::Answer` records no span to anchor a reference to.
///
/// Spans are the parser's own (`interp::Span`), not brackets matched
/// here — a scan would have to get string literals right and would get
/// them wrong on the first `tell(")")`. Rows written before those
/// fields existed carry zero and are left exactly as they were.
fn annotate_history_calls(source: &str, cuts: Option<&Vec<Cut>>) -> String {
    let Some(cuts) = cuts else {
        return source.to_owned();
    };
    let mut cuts = cuts.clone();
    // Right to left, so no earlier offset goes stale.
    cuts.sort_by_key(|c| std::cmp::Reverse(c.start));
    let mut out = source.to_owned();
    for c in cuts {
        let snipped = format!("/* snipped - history[{}] */", c.row);
        let marked = format!(" /* history[{}] */", c.row);
        if c.literal && snipped.len() < c.end - c.start {
            // Keep the callee, so the call still reads as a call:
            // `const a = await ask(/* snipped - history[7] */)` has a
            // shape that a bare comment would not.
            let callee = out[c.start..c.end]
                .find('(')
                .map(|i| &out[c.start..c.start + i])
                .unwrap_or("")
                .to_owned();
            out.replace_range(c.start..c.end, &format!("{callee}({snipped})"));
        } else {
            out.insert_str(c.end, &marked);
        }
    }
    out
}

/// A replaced entry's line, `[id] … text`. `None` for a removed one,
/// which renders nothing at all.
///
/// **The `…` says this entry stands in for something longer**, and it
/// is the whole reason the marker exists: without it a replacement is
/// presented exactly like a short original, so a later program cannot
/// tell that shortening it again means summarising a summary. On the
/// skipped-tests run of 2026-09-17 one entry was replaced four times,
/// each pass rewriting the pass before it, and the constraint that
/// mattered — that the project runs `unittest`, not `pytest` — was
/// three generations gone by the time a program needed it. The run
/// failed on a regex written for the wrong test framework.
///
/// One character, and only on replacements: the run's 150 ops were 143
/// removals and 7 replacements, so this is charged against the few
/// entries that already carry text someone chose to spend words on.
/// What to *do* about it — fetch the original and summarise from that
/// — is stated once, in the compaction directive, rather than repeated
/// on every line that carries the mark.
fn compacted_line(id: EventId, shadow: &CompactedView) -> Option<String> {
    shadow
        .text
        .as_deref()
        .map(|text| format!("`[{}]` … {}", id.as_u64(), text))
}

/// A compacted **program**'s rendered turn: still valid JavaScript,
/// still carrying its own id, saying how to fetch the original. Never a
/// non-assistant stub — that is what keeps role alternation intact
/// under compaction with no special case (doc 22, `Compacted`'s own
/// `types.rs` doc comment): whatever occupies the assistant's slot in
/// the rendered transcript is still an assistant turn, just one whose
/// entire body is a comment.
fn compacted_program_comment(id: EventId, shadow: &CompactedView) -> Option<String> {
    shadow
        .text
        .as_deref()
        .map(|text| format!("//: [{}] … {}", id.as_u64(), text))
}

/// A completion report's line — the rendering of a `Return` or a
/// handing-back `Condition` — with its compacted shadow taking
/// precedence, exactly as [`pending_line`] does for the rows it
/// handles.
///
/// **The report is a row.** It did not used to be: this arm called
/// `derive_report` unconditionally, so the return preview, the console
/// and the artifact menu — measured on 2026-09-17 as the largest thing
/// in a document after the card — were the one part of a conversation
/// compaction could not reach. `label_of` said `"return"` for the
/// event, so `remove_history(#19, "return")` passed the checksum and
/// was then applied to a rendering that ignored it: the dry run came
/// back the same size and the batch was refused for freeing nothing.
/// Live compaction programs hit exactly that, twice, and were told
/// "compact more of it and return again" for work that was correct.
///
/// This is also why `Call` and `Result` keep [`label_of`]'s `"event"`
/// fallback and stay [`crate::compaction::CompactionError::NotARow`]:
/// they have no line of their own to remove: they are *inside* this
/// one, and go when it goes.
fn report_line(
    tree: &Tree,
    leaf: EventId,
    id: EventId,
    budget: usize,
    compacted: &HashMap<EventId, CompactedView>,
) -> Option<String> {
    match compacted.get(&id) {
        None => Some(crate::report::derive_report(tree, leaf, id, budget)),
        Some(shadow) => compacted_line(id, shadow),
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
        return compacted_line(event.id, shadow);
    }
    match &event.payload {
        // **Symmetric with the outgoing rows.** `[2] user told you: …`
        // against `[4] you told user: …`, and the same for asking, so a
        // reader never has to work out which way a row points from the
        // punctuation around an author's name. It used to render as
        // `post (user): …`, with `post` leading because that word was
        // the row's label and compaction checked it as a checksum. The
        // checksum is gone — an op names an id and nothing else — so
        // the word is free to say what happened instead of what kind of
        // event it was.
        EventPayload::Message(msg @ Message::Post { from, .. }) => {
            let resolved = tree.resolve(msg);
            let Message::Post { origin, .. } = &resolved else {
                unreachable!("resolve() never changes a Post's variant")
            };
            let (text, wants_reply) = origin
                .direct()
                .map(|(t, _, r)| (t, r))
                .unwrap_or(("", false));
            Some(format!(
                "`[{}]` {} {} you: {}",
                event.id.as_u64(),
                author_label(tree, *from),
                if wants_reply { "asked" } else { "told" },
                escape_untrusted(text)
            ))
        }
        // The answer to a question this branch asked. Before this it
        // rendered as a menu row's `→ ok, 14 bytes` and, once `ask`
        // left the menu, as nothing at all — so a value the program had
        // suspended itself to obtain was invisible to the program after
        // it. The asker is named rather than assumed: it is whoever the
        // `ask` was addressed to.
        EventPayload::Result { call, outcome } => {
            let Some(Event {
                payload:
                    EventPayload::Call(Call::Send {
                        to,
                        expects_reply: true,
                        ..
                    }),
                ..
            }) = tree.events.get(call)
            else {
                return None;
            };
            let Outcome::Delivered(value) = outcome else {
                return None;
            };
            Some(format!(
                "`[{}]` {} answered `[{}]`: {}",
                event.id.as_u64(),
                crate::machine::address_label(to),
                call.as_u64(),
                escape_untrusted(&value.to_string())
            ))
        }
        // **A note, a `tell`, an `ask` and an `answer` are not
        // rendered here.** They are rows the *run* added, so they
        // belong in the run's own list of what it did
        // (`machine::menu_rows`, under `report.rs`'s `### rows it
        // added`) rather than as loose lines above the report. Until
        // 2026-09-19 they were here and the calls were there, so one
        // program's doings arrived as two lists in two places with the
        // outcome wedged between them.
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
/// **The system prompt and the worked examples both come from
/// `spine.context()`, never from a caller or from today's card.** They
/// are one prefix, and until 27 only half of it was snapshotted: the
/// exemplars were re-read from `card::seed_exemplars()` on every
/// render, so a conversation begun under `--card X` came back with X's
/// prose in front of the embedded examples in any process that had not
/// been given `--card X` again.
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
/// **Every program on this branch renders, and so does every
/// condition.** It did not used to: a `Turn` at handler depth > 0 was
/// filtered out entirely (doc 22: "a deliberating handler never enters
/// the document"), and only a `Condition{disposition: Handover}` or a
/// `Return` at depth 0 produced a completion report. That was written
/// for deliberation the model never saw — a handler decided and popped
/// between two of its turns. Under automatic continuation (27.1) the
/// model writes the recovery program itself, on this branch, as an
/// ordinary next turn, and `machine.rs` stamps `Pushed` on a *trap*
/// as well as a raise. So an unhandled trap pushed the derived depth
/// to 1 and every program written before something unwound it
/// vanished: measured on 2026-09-17, a `dead-code-sweep` run lost two
/// programs including the one that computed the edit it then believed
/// it had never made ("But I haven't actually written the files"),
/// while the trap that started it rendered as a **0-byte user turn**
/// — `flush_pending` on an empty `pending`, because the condition
/// that suspended the program produced no line.
///
/// A condition's report is therefore a row here rather than the
/// one-shot ephemeral tail `host::prompt_suspended` used to attach:
/// the model was prompted with it once, and a document that drops it
/// afterwards is a document in which the program died of nothing. The
/// same trap twice in one run — seen in these same logs — is what that
/// costs.
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
pub fn render(tree: &Tree, spine: &Spine, budget: usize, transport: Transport) -> Document {
    let leaf = spine.leaf_id;
    let agent = tree
        .enclosing_agent(leaf)
        .expect("a spine's leaf always has an enclosing Agent — spine_at() built it from one");
    let context = spine.context();
    render_with_lookup(
        tree,
        agent,
        leaf,
        context,
        budget,
        &tree.compacted_lookup(leaf),
        transport,
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
///
/// `context` arrives whole rather than as its `system` and `exemplars`
/// separately: both callers already hold one and pull the pair out of
/// it, and passing the halves let a caller combine a card from one
/// agent's snapshot with another's exemplars — a preamble no agent ever
/// had. `transport` is a parameter for the reason [`Transport`] gives:
/// it is the one input here that a test must vary, and it was a
/// process-global read mid-fold until that turned out to be a data
/// race.
pub(crate) fn render_with_lookup(
    tree: &Tree,
    agent: EventId,
    leaf: EventId,
    context: &Context,
    budget: usize,
    compacted: &HashMap<EventId, CompactedView>,
    transport: Transport,
) -> Document {
    let mut messages = vec![ChatMessage::text(ChatRole::System, context.system.clone())];
    messages.extend(worked_examples(&context.exemplars, transport));
    let preamble = messages.len();
    let cuts = told_literal_cuts(tree, agent, leaf);
    // Under `Transport::Notebook` a reply is N cell `Turn`s and one
    // completion; this pairs each reply with the bytes the model
    // actually generated, so the assistant turn replays them rather
    // than being rebuilt out of its pieces. Empty on every other
    // transport, where a `Turn` already *is* the completion.
    let (replies, covered) = if transport == Transport::Notebook {
        notebook_replies(tree, leaf, agent, &cuts, compacted)
    } else {
        (HashMap::new(), HashSet::new())
    };
    let mut pending: Vec<String> = Vec::new();
    let mut cur_agent: Option<EventId> = None;
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
    // Whether the open block has already reported a run, so an arrival
    // after it gets a heading rather than trailing off the run's last
    // section. Cleared by that heading and by every flush.
    let mut ran = false;

    for ev in tree.path_events(leaf) {
        if let EventPayload::Agent { .. } = ev.payload {
            cur_agent = Some(ev.id);
            continue;
        }
        if cur_agent != Some(agent) {
            continue;
        }
        match &ev.payload {
            EventPayload::Message(Message::Turn { source, .. }) => {
                // A later cell of a reply already drawn above: its
                // JavaScript is inside that reply's text.
                if covered.contains(&ev.id) {
                    continue;
                }
                let content = match (replies.get(&ev.id), compacted.get(&ev.id)) {
                    // **The reply, verbatim.** Only the documented
                    // annotate-and-snip pass is applied on top; no
                    // re-fencing, no re-assembly, no normalisation. What
                    // the model is shown as its own turn is what it
                    // wrote, because that is what it imitates.
                    (Some(reply), _) => Some(annotate_history_calls(
                        &reply.text,
                        Some(&reply.cuts).filter(|c| !c.is_empty()),
                    )),
                    (None, None) => Some(annotate_history_calls(source, cuts.get(&ev.id))),
                    (None, Some(shadow)) => compacted_program_comment(ev.id, shadow),
                };
                // A removed program occupies no slot at all. Because a
                // flush only happens here, the pending lines from either
                // side of it merge into one user message — no empty
                // message, and never two assistant turns in a row.
                if let Some(content) = content {
                    ran = false;
                    messages.push(flush_pending(&mut pending, transport, &mut open_call));
                    let (assistant, call_id) = assistant_turn(transport, ev.id, content);
                    messages.push(assistant);
                    open_call = call_id;
                }
            }
            // A compaction directive is the one condition that does not
            // belong in the document: it instructs, it does not report.
            // It rides the ephemeral tail instead (`request_tail`), so
            // it is the last thing read before the compaction program is
            // written and is gone by the next request. Left as a row it
            // was read as a standing instruction — in the run of
            // 2026-09-17 two expired "STOP — write a compaction program,
            // nothing else" directives sat in the history, 3,788 bytes
            // of a document that had just been compacted for being too
            // large, and the model wrote a third compaction program
            // nothing had asked for. What survives an episode is its
            // `Compacted` events and the shortened rows they produce,
            // which is the trace worth keeping.
            EventPayload::Condition {
                cause: Cause::Compaction { .. },
                ..
            } => {}
            EventPayload::Return { .. } | EventPayload::Condition { .. } => {
                if let Some(line) = report_line(tree, leaf, ev.id, budget, compacted) {
                    pending.push(line);
                    ran = true;
                }
            }
            _ => {
                if let Some(line) = pending_line(tree, leaf, ev, compacted) {
                    // **An arrival after a run needs a heading of its
                    // own.** Without one it sits under whichever `###`
                    // the run block ended on — `it printed`, usually —
                    // and reads as more of that section's output. Only
                    // after a run: an arrival that opens the block is
                    // the block's subject, and a heading over a single
                    // line saying what the line already says is the
                    // scaffolding this format exists to remove.
                    if ran {
                        pending.push(ARRIVAL_HEADING.to_owned());
                        ran = false;
                    }
                    pending.push(line);
                }
            }
        }
    }

    if !pending.is_empty() {
        messages.push(flush_pending(&mut pending, transport, &mut open_call));
    }

    Document {
        messages,
        preamble,
        transport,
    }
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
        // Notebook's completion is the model's whole response text, as
        // Program's is — markdown rather than bare JS, but the same plain
        // assistant message either way.
        Transport::Program | Transport::Notebook => {
            (ChatMessage::text(ChatRole::Assistant, source), None)
        }
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

/// **The user turn's own heading.** A message in the user role holds
/// whatever the log gained since the last reply: a person's words, the
/// model's own `tell`s, its notes, its program's report. Read without a
/// label, a kilobyte of the model's own prose arriving in the user role
/// looks like somebody saying it.
///
/// Markdown, and shouted, for the same reason the reply itself is
/// markdown: the document has one syntax now, and this is its outermost
/// heading. `report.rs`'s `## RAN YOUR PROGRAM` nests under it and the
/// `###` sections nest under that, so the structure of a turn is
/// readable from the markup rather than from having learned which
/// headings group with which.
pub const TURN_HEADING: &str = "# NEW EVENTS";

/// What arrivals render under once a run has already been reported in
/// the same turn — see the call site for why only then.
pub const ARRIVAL_HEADING: &str = "## MESSAGES";

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
    let content = if pending.is_empty() {
        String::new()
    } else {
        format!("{TURN_HEADING}\n\n{}", pending.join("\n\n"))
    };
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
fn worked_examples(exemplars: &[Exemplar], transport: Transport) -> Vec<ChatMessage> {
    exemplars
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
                Transport::Program | Transport::Notebook => {
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
/// A provider's own control tokens, arriving as *text* in the
/// completion and then compiled as if the model had written them.
///
/// Observed 2026-09-17: a run died on `compile error: 1:1: Unexpected
/// token` with `<｜｜DSML｜｜ calls>` as the offending source — DeepSeek's
/// tool-call delimiter, leaked into the content stream. The model did
/// not write it, the program was otherwise fine, and the whole run was
/// lost to a trap it could not have avoided or understood.
///
/// Stripped rather than handled further up because this is the one
/// place that decides what counts as the program's source, and because
/// the alternative — teaching the card about a provider's framing — is
/// exactly the sort of thing the model should never have to know.
///
/// Deliberately narrow: only tokens delimited by the full-width bars
/// `｜` (U+FF5C), which no ordinary program contains and which are how
/// this family of tokens is spelled. A broad "strip anything in angle
/// brackets" rule would eat `a < b && c > d`.
fn strip_control_tokens(raw: &str) -> String {
    if !raw.contains('\u{ff5c}') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        let Some(end_rel) = rest[start..].find('>') else {
            break;
        };
        let end = start + end_rel + 1;
        if rest[start..end].contains('\u{ff5c}') {
            out.push_str(&rest[..start]);
            rest = &rest[end..];
        } else {
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    out.push_str(rest);
    out
}

pub fn extract_program(raw: &str) -> String {
    let raw = strip_control_tokens(raw);
    let raw = raw.as_str();
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

    /// **The whole prefix belongs to the conversation, not to today's
    /// card.** `Agent.system` was snapshotted for exactly this reason
    /// and the exemplars beside it were not: they came from
    /// `card::seed_exemplars()`, the running process's active card. So
    /// a conversation begun under `--card X` rendered X's prose in
    /// front of the *embedded* examples in any process that had not
    /// been handed `--card X` again — `agent document`, `agent score`
    /// (which rebuilds `prompt_bytes` by re-rendering, so every
    /// variant's measured prompt size was wrong), and any resume.
    ///
    /// This is the shape of that: a tree whose agent snapshotted one
    /// exemplar, rendered while the *active* card has none.
    #[test]
    fn the_exemplars_come_from_the_agents_snapshot_not_todays_card() {
        let mut tree = Tree::new(None);
        let snapshotted = vec![Exemplar {
            user: "the user turn this branch was born with".into(),
            assistant: "done();".into(),
        }];
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD OF THE DAY", snapshotted)
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "go".into(),
                    input: serde_json::Value::Null,
                    options: Vec::new(),
                    expects_reply: true,
                },
            }),
        )
        .unwrap();

        let doc = render(&tree, &spine, 4096, Transport::Program);
        let text: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(
            text.contains("the user turn this branch was born with"),
            "the branch's own exemplar is missing from its prefix"
        );
        // `card::active()` here is the embedded card, with ten of its
        // own — none of which belong to this conversation.
        assert_eq!(doc.preamble, 1 + 2, "system + one exemplar's two turns");
        assert_eq!(
            doc.conversation().len(),
            doc.messages.len() - doc.preamble,
            "and the conversation starts exactly after it"
        );
    }

    // --- extract_program (folded in from the deleted fence.rs) ---

    /// **A provider's control token is not the model's program.** A run
    /// on 2026-09-17 was lost to `compile error: 1:1: Unexpected token`
    /// whose source was `<｜｜DSML｜｜ calls>` — DeepSeek's tool-call
    /// delimiter arriving as content. Nothing the model could have
    /// avoided, and nothing it should have to know about.
    #[test]
    fn a_leaked_control_token_is_not_compiled_as_source() {
        assert_eq!(
            extract_program("<｜｜DSML｜｜ calls>tell(\"hi\");"),
            "tell(\"hi\");"
        );
        assert_eq!(
            extract_program("tell(\"hi\");<｜tool▁calls▁end｜>"),
            "tell(\"hi\");"
        );
    }

    /// Narrow on purpose: a comparison is not a control token.
    #[test]
    fn ordinary_angle_brackets_survive() {
        let src = "if (a < b && c > d) { tell(\"x\"); }";
        assert_eq!(extract_program(src), src);
        let generic = "const xs = [1, 2]; if (xs.length < 3) done();";
        assert_eq!(extract_program(generic), generic);
    }

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
                options: Vec::new(),
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
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
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

        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
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

    /// A handler's deliberation renders like any other program,
    /// because under automatic continuation it *is* one: the model was
    /// prompted with the condition report and wrote `return resume(1)`
    /// as its next turn. This test used to assert the opposite — that
    /// a `Turn` at handler depth > 0 contributed nothing — and the
    /// filter it pinned had a second, unintended customer: `machine.rs`
    /// stamps `Disposition::Pushed` on a **trap** too, so an unhandled
    /// trap raised the derived depth and silently swallowed every
    /// program until something unwound it. See [`render_with_lookup`]'s
    /// own doc for the run that cost.
    #[test]
    fn a_deliberation_renders_like_any_other_program() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
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

        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let conv = doc.conversation();
        // user("go") / assistant(raise) / user(the raise's own report,
        // which is what the model was prompted with) / assistant(the
        // decision it wrote back) / user(both returns).
        assert_eq!(conv.len(), 5, "{doc:?}");
        assert_eq!(conv[1].content, "raise('x');");
        assert_eq!(conv[3].content, "return resume(1);");
        assert!(
            !conv[2].content.is_empty(),
            "a condition that suspended the program renders its report, \
             never an empty turn the model has to infer from: {doc:?}"
        );
    }

    /// The bug the depth filter actually had. `machine.rs` stamps
    /// `Disposition::Pushed` on a **trap**, not just a raise, and
    /// nothing handles a trap — under automatic continuation the model
    /// is prompted and writes an ordinary next program. So the filter
    /// read "a handler is deliberating" off a program that had simply
    /// died, and hid everything written until something unwound it.
    ///
    /// Taken from `keepOff2/dead-code-sweep-151849` (2026-09-17): a
    /// `ReferenceError` on a variable from the previous program, then
    /// two programs the document dropped — the second of which computed
    /// the edit and returned it. The program after that wrote "But I
    /// haven't actually written the files", correctly, from what it
    /// could see. The run finished having changed nothing.
    #[test]
    fn a_trap_hides_neither_itself_nor_the_programs_after_it() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        tree.append(&mut spine, turn("Edit.applyEdits(fmt.content, []);"))
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Condition {
                cause: Cause::Trapped {
                    kind: "ReferenceError".into(),
                    message: "fmt is not defined".into(),
                    resumable: true,
                },
                site: 0,
                stack: vec!["<root>".into()],
                disposition: Disposition::Pushed,
            },
        )
        .unwrap();
        tree.append(&mut spine, turn("return recompute();"))
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!("the edit"),
            },
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let conv = doc.conversation();
        assert_eq!(
            conv.iter()
                .filter(|m| m.role == ChatRole::Assistant)
                .count(),
            2,
            "both programs render; the trap is not a handler push: {doc:?}"
        );
        assert!(
            conv[2].content.contains("fmt is not defined"),
            "the trap says what happened, rather than rendering as a blank \
             turn the next program has to guess from: {doc:?}"
        );
        assert!(
            doc.messages.iter().any(|m| m.content.contains("the edit")),
            "the recovery program's own return survives too: {doc:?}"
        );
    }

    /// A literal `tell` is in the document twice — as its own row, and
    /// inside the call that produced it — so the call becomes a
    /// reference to the row. A computed one is not duplication and is
    /// left alone: the row has the bytes, the source has how they were
    /// built.
    #[test]
    fn a_literal_tell_becomes_a_reference_and_a_computed_one_does_not() {
        const LONG_TELL: &str = "checked every file and the build is green after the rename";
        let src = "tell(\"checked every file and the build is green after the rename\");\ntell(\"x \" + y);\n";
        let lit = src
            .find("tell(\"checked every file and the build is green after the rename\")")
            .unwrap();
        let comp = src.find("tell(\"x \" + y)").unwrap();
        let send = |text: &str, a: usize, b: usize| {
            EventPayload::Call(Call::Send {
                prose: false,
                to: Address::User,
                text: text.into(),
                input: serde_json::Value::Null,
                options: Vec::new(),
                expects_reply: false,
                site: a as u32,
                site_end: b as u32,
            })
        };

        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        tree.append(&mut spine, turn(src)).unwrap();
        let a = tree
            .append(
                &mut spine,
                send(
                    "checked every file and the build is green after the rename",
                    lit,
                    lit + "tell(\"checked every file and the build is green after the rename\")"
                        .len(),
                ),
            )
            .unwrap();
        // Computed: the text never appears inside its own call.
        tree.append(
            &mut spine,
            send("x 1", comp, comp + "tell(\"x \" + y)".len()),
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let program = doc
            .conversation()
            .iter()
            .find(|m| m.role == ChatRole::Assistant)
            .expect("a program")
            .content
            .clone();
        assert!(
            program.contains(&format!("tell(/* snipped - history[{}] */)", a.as_u64())),
            "the long literal is replaced by its row: {program}"
        );
        assert!(
            program.contains("tell(\"x \" + y) /* history["),
            "the computed one keeps its construction and takes a reference: {program}"
        );
        assert!(
            !program.contains(LONG_TELL),
            "and the duplicated bytes are gone: {program}"
        );
    }

    /// A call shorter than the reference keeps its text — de-duplicating
    /// is not worth spending more bytes than it saves — but still takes
    /// the reference, because the link from call to row is the point.
    #[test]
    fn a_short_call_keeps_its_text_and_still_takes_the_reference() {
        let src = "tell(\"ok\");\n";
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        tree.append(&mut spine, turn(src)).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Call(Call::Send {
                prose: false,
                to: Address::User,
                text: "ok".into(),
                input: serde_json::Value::Null,
                options: Vec::new(),
                expects_reply: false,
                site: 0,
                site_end: "tell(\"ok\")".len() as u32,
            }),
        )
        .unwrap();
        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let program = doc
            .conversation()
            .iter()
            .find(|m| m.role == ChatRole::Assistant)
            .expect("a program")
            .content
            .clone();
        assert!(
            program.starts_with("tell(\"ok\") /* history["),
            "text kept, reference added: {program}"
        );
    }

    /// Both directions read the same way. A row never makes the reader
    /// work out which way it points from the punctuation around a name:
    /// `user told you` against `you told user`, `user asked you`
    /// against `you asked user`, and the answer to a question this
    /// branch asked named by the question it answers.
    #[test]
    fn incoming_and_outgoing_rows_are_symmetric() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
        // Incoming, expecting a reply.
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "which one?".into(),
                    input: serde_json::Value::Null,
                    options: Vec::new(),
                    expects_reply: true,
                },
            }),
        )
        .unwrap();
        tree.append(&mut spine, turn("1;")).unwrap();
        // Outgoing question, and the answer that settles it.
        let q = tree
            .append(
                &mut spine,
                EventPayload::Call(Call::Send {
                    prose: false,
                    to: Address::User,
                    text: "30 or 240?".into(),
                    input: serde_json::Value::Null,
                    options: Vec::new(),
                    expects_reply: true,
                    site: 0,
                    site_end: 0,
                }),
            )
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Result {
                call: q,
                outcome: Outcome::Delivered(serde_json::json!("30")),
            },
        )
        .unwrap();
        // The run has to end for its rows to be reported: they are the
        // run's own list now, not loose lines beside it.
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::Value::Null,
            },
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let all: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(all.contains("user asked you: which one?"), "{all}");
        assert!(all.contains("you asked user: 30 or 240?"), "{all}");
        assert!(
            all.contains(&format!("user answered `[{}]`: \"30\"", q.as_u64())),
            "the answer names the question it settles: {all}"
        );
    }

    /// An `ask` is snipped on the same terms, and keeps its verb so the
    /// call still reads as a call — `await ask(/* [7] above */)` has a
    /// shape where a bare comment would not.
    #[test]
    fn a_literal_ask_is_snipped_and_keeps_its_verb() {
        let src = "const a = await ask(\"user\", \"is 240 still right for request_timeout_seconds, or did we settle on the old 30?\");\n";
        let at = src.find("ask(").unwrap();
        let end = src.find(");").unwrap() + 1;
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        tree.append(&mut spine, turn(src)).unwrap();
        let q = tree
            .append(
                &mut spine,
                EventPayload::Call(Call::Send {
                    prose: false,
                    to: Address::User,
                    text: "is 240 still right for request_timeout_seconds, or did we settle on the old 30?".into(),
                    input: serde_json::Value::Null,
                    options: Vec::new(),
                    expects_reply: true,
                    site: at as u32,
                    site_end: end as u32,
                }),
            )
            .unwrap();
        // The run has to end for its rows to be reported: they are the
        // run's own list now, not loose lines beside it.
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::Value::Null,
            },
        )
        .unwrap();

        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let all: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(
            all.contains(&format!(
                "const a = await ask(/* snipped - history[{}] */)",
                q.as_u64()
            )),
            "{all}"
        );
        assert!(
            all.contains(&format!("`[{}]` you asked user: is 240 still right for request_timeout_seconds, or did we settle on the old 30?", q.as_u64())),
            "and the question itself renders whole, as its own row: {all}"
        );
    }

    /// A replacement is marked; an original is not. Without the mark a
    /// replacement is presented exactly like a short original, and a
    /// later program cannot tell that shortening it again means
    /// summarising a summary — which is how one entry got rewritten
    /// four times on 2026-09-17, losing the constraint that mattered.
    #[test]
    fn a_replaced_entry_is_marked_as_standing_in_for_more() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
        tree.append(&mut spine, user_post("go")).unwrap();
        let note = tree
            .append(
                &mut spine,
                EventPayload::Note {
                    text: "a long finding worth several lines".into(),
                    site: 0,
                    site_end: 0,
                },
            )
            .unwrap();
        tree.append(&mut spine, turn("1;")).unwrap();
        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let before: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(
            !before.contains('…'),
            "an original carries no mark: {before}"
        );

        tree.append(
            &mut spine,
            EventPayload::Compacted {
                of: note,
                text: Some("the finding, in one line".into()),
            },
        )
        .unwrap();
        let doc = render(&tree, &spine, 64 * 1024, Transport::Program);
        let after: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(
            after.contains(&format!("`[{}]` … the finding", note.as_u64())),
            "a replacement says it stands in for more: {after}"
        );
    }

    /// A **replaced** program still occupies the assistant's slot, as a
    /// comment-only turn — valid JavaScript, carrying its own id — so
    /// role alternation survives with no special case.
    ///
    /// A **removed** one occupies no slot at all, and that also needs no
    /// special case: `render` flushes the pending user lines only when
    /// it meets a turn, so a turn that renders nothing lets the lines on
    /// either side of it merge into one user message. No empty message,
    /// and never two assistant turns in a row — which is what the stub
    /// used to be for, at 62 bytes apiece of permanent floor.
    #[test]
    fn a_replaced_program_keeps_the_assistant_slot_and_a_removed_one_vacates_it() {
        let build = |text: Option<String>| {
            let mut tree = Tree::new(None);
            let mut spine = tree
                .start_agent(None, None, "root", None, "CARD", Vec::new())
                .unwrap();
            tree.append(&mut spine, user_post("go")).unwrap();
            let program = tree.append(&mut spine, turn("1 + 1;")).unwrap();
            tree.append(
                &mut spine,
                EventPayload::Return {
                    value: serde_json::json!(2),
                },
            )
            .unwrap();
            tree.append(&mut spine, EventPayload::Compacted { of: program, text })
                .unwrap();
            render(&tree, &spine, 64 * 1024, Transport::Program)
        };

        let replaced = build(Some("did the arithmetic".into()));
        let conv = replaced.conversation();
        let assistant: Vec<&ChatMessage> = conv
            .iter()
            .filter(|m| m.role == ChatRole::Assistant)
            .collect();
        assert_eq!(assistant.len(), 1, "{conv:?}");
        assert!(
            assistant[0].content.starts_with("//:"),
            "still a program, and still a comment: {:?}",
            assistant[0].content
        );
        assert!(
            !assistant[0].content.contains("1 + 1"),
            "the original is gone: {:?}",
            assistant[0].content
        );

        let removed = build(None);
        let conv = removed.conversation();
        assert!(
            !conv.iter().any(|m| m.role == ChatRole::Assistant),
            "a removed program occupies no slot: {conv:?}"
        );
        assert!(
            !conv.iter().any(|m| m.content.is_empty()),
            "and leaves no empty message behind: {conv:?}"
        );
        assert!(
            conv.windows(2).all(|w| w[0].role != w[1].role),
            "roles still alternate: {conv:?}"
        );
    }

    // --- Transport switch ---

    /// The same small branch rendered under whichever container the
    /// caller names. There is no ambient switch to flip and nothing to
    /// restore: the transport is an argument, so two of these tests can
    /// run side by side on different threads and neither can see the
    /// other's choice.
    fn sample_document(transport: Transport) -> Document {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
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
        render(&tree, &spine, 64 * 1024, transport)
    }

    /// Pins today's shape: the model's whole response rides bare in
    /// `content`, no tool wrapper, and the report that follows it is an
    /// ordinary `User` message — [`render`]'s behaviour before this
    /// transport switch existed, and what `Transport::Program` must
    /// still produce byte-for-byte now that a second mode exists beside
    /// it.
    #[test]
    fn program_mode_renders_an_assistant_turn_as_plain_text() {
        // `conversation()` slices at the preamble length this document
        // recorded when it was built, and that length is transport-
        // dependent (a `RunProgram` exemplar renders three rows, a
        // `Program` one two). It is read off `doc` itself, so there is
        // no window in which it could be sliced at the other mode's
        // length — which is exactly what an ambient transport used to
        // make possible.
        let doc = sample_document(Transport::Program);
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
    }

    /// `Transport::RunProgram`'s whole point: the same turn now arrives
    /// as a `run_program` call (its `source` the program, unchanged),
    /// and the report that follows answers that call in the `Tool` role
    /// — never a `User` message, per the wire format's own rule that a
    /// tool-calling assistant turn must be answered before anything else
    /// follows it.
    #[test]
    fn run_program_mode_renders_an_assistant_turn_as_a_tool_call() {
        let doc = sample_document(Transport::RunProgram);
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
            let mut spine = tree
                .start_agent(None, None, "root", None, "CARD", Vec::new())
                .unwrap();
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
        // Each document carries the transport it was rendered under, so
        // `.conversation()` slices each at its own preamble length and
        // the two can simply be built one after the other.
        let program_content: Vec<String> =
            render(&program_tree, &program_spine, 64 * 1024, Transport::Program)
                .conversation()
                .iter()
                .map(payload)
                .collect();
        let rp_content: Vec<String> = render(&rp_tree, &rp_spine, 64 * 1024, Transport::RunProgram)
            .conversation()
            .iter()
            .map(payload)
            .collect();

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

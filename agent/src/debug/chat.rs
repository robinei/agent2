//! Chat transcript state (9_TUI Step 4 · 11_INTROSPECT Step 3 ·
//! 17_BRANCHES Step D1), driven **exclusively** by `SessionEvent`s — the
//! serializable boundary a remote client would consume. Enforced
//! structurally, not by discipline: `ChatState`'s fields are private to
//! this module and its only mutator is `apply(&SessionEvent)`, so nothing
//! privileged (VMs, tree, session) can leak into what this pane shows. Do
//! not add imports from `crate::host` beyond the protocol types, and none
//! from `interp`.
//!
//! A `run_program` execution renders as one **block** (decision 2): a
//! `run_program: <status>` header (status tracked live from
//! `ProgramStatus`) with the program's inner `Invoke`s listed beneath as
//! `⚙` lines; a `resume` folds into the same block. The completion/
//! condition report body is *not* inlined — it lives in the right
//! console/result pane. The transcript is **per-branch** (17_BRANCHES):
//! `rows(branch)` renders that branch's own slice plus — for a forked
//! branch — the shared prefix it inherited, reconstructed from the event
//! stream alone (`fork_parent`, below), since a fork carries *history,
//! not obligations* and this pane never reaches past the protocol into
//! the `Tree` to get it.

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

use crate::host::{AgentId, BranchId, ProgramStatus, SessionEvent};
use crate::types::{Address, Call, Event, EventId, EventPayload, Message, Outcome};

/// What a transcript row is, for styling by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatKind {
    /// The agent's stored system prompt (a collapsible header).
    System,
    User,
    Assistant,
    /// In-flight streamed text (replaced by the logged message).
    Streaming,
    /// Reasoning content — live-streamed or from a logged `Turn`'s
    /// `thinking`. Always present in `rows()`; hidden by default is a
    /// rendering choice the attached UI makes (`show_thinking`), not
    /// something this transcript model decides.
    Thinking,
    /// A `run_program` block header or one of its `⚙` inner-call lines.
    ToolCall,
    /// An attachment line within a program block.
    Attachment,
    /// Context lifecycle markers.
    Marker,
    Error,
    /// A markdown `#`..`######` line, in `Assistant`/`Streaming` prose
    /// only — see `classify_markdown_lines`.
    Heading,
    /// A markdown `>` line, in `Assistant`/`Streaming` prose only.
    Blockquote,
    /// A line inside a fenced ` ``` ` block, in `Assistant`/`Streaming`
    /// prose only — literal, never reinterpreted for further markdown.
    Code,
    /// A markdown pipe-table's header row, column-aligned and already
    /// formatted to fixed width — see `classify_markdown_lines`.
    TableHeader,
    /// A markdown pipe-table's body row, same formatting as its header.
    TableRow,
    /// A table's box-drawing border (`┌─┬─┐`/`├─┼─┤`/`└─┴─┘`) — matches
    /// the predecessor's own table rendering (`agent-cli/src/markdown.rs`
    /// `render_border_row`), the fidelity found lost in the port.
    TableBorder,
    /// A markdown horizontal rule (`---`/`***`/`___`, 3+ of the same
    /// char alone on a line) — rendered full-pane-width, matching the
    /// predecessor's own `"─".repeat(term_width)` rule.
    Hr,
}

/// Click-hit metadata for a transcript row.
#[derive(Debug, Clone, PartialEq)]
pub enum RowDetail {
    /// No special click target.
    None,
    /// A program block header row — clicking selects the program.
    Program(EventId),
    /// An attachment row — clicking selects the program + that attachment.
    Attachment(EventId, String),
    /// An invoke row — clicking selects the program + that invoke.
    Invoke(EventId, usize),
}

/// One transcript entry. A `Header` computes its text live from the
/// program's `ProgramStatus`; every other entry is a fixed line.
enum Entry {
    Line {
        /// The branch this entry's event landed on — the render key.
        branch: BranchId,
        /// This entry's own event id, so a forked branch's rendering can
        /// tell "before the fork" from "after" (`rows`, below).
        id: EventId,
        kind: ChatKind,
        text: String,
        /// The program this row belongs to (for click hit-testing): the
        /// `run_program` event id for block rows, the `System` event id
        /// for the system header, else `None`.
        program: Option<EventId>,
    },
    /// A `run_program` block header, keyed by the program's event id.
    Header {
        branch: BranchId,
        program: EventId,
        /// Attachment names in definition order.
        attachments: Vec<String>,
    },
}

/// One `Entry::Line`'s memoized `classify_entry_lines` output, alongside
/// the width it was computed for — a table's layout depends on the pane
/// width, so a cached entry is only valid for the width it was laid out
/// against; a mismatch is treated as a plain cache miss (recompute and
/// overwrite), the same "keyed by width" strategy the predecessor's own
/// render cache uses. `None` until first read.
type LineCache = std::cell::RefCell<Vec<Option<(usize, Vec<(ChatKind, String)>)>>>;

#[derive(Default)]
pub struct ChatState {
    entries: Vec<Entry>,
    /// Accumulating streamed text per branch, shown until the logged
    /// assistant message replaces it.
    streaming: Vec<(BranchId, String)>,
    /// Same shape as `streaming`, for the `thinking`-flagged half of the
    /// chunk stream — kept separate since a turn logs the two as
    /// distinct fields (`text`, `thinking`), not one buffer.
    thinking_streaming: Vec<(BranchId, String)>,
    /// The first branch seen — the default transcript when none is
    /// selected.
    main_branch: Option<BranchId>,
    /// The root agent, for the "is this a subagent" check on `Answer`
    /// markers — an agent-level fact, not a branch-level one, so it stays
    /// separate from `main_branch`.
    main_agent: Option<AgentId>,
    /// Transcript row index of each logged `Call`, so its `Result` can
    /// complete the row in place rather than pushing a second line.
    call_rows: HashMap<EventId, usize>,
    /// The open `run_program` block per branch: its inner calls and a
    /// folding `resume` attach here.
    current_program: HashMap<BranchId, EventId>,
    /// Live status per program block, titling its header.
    program_status: HashMap<EventId, ProgramStatus>,
    /// Attachment content per program: program_id → (name → content).
    pub attachment_content: HashMap<EventId, HashMap<String, String>>,
    /// Every event's own branch, by id — including events that never
    /// become a chat row (`Call`, `Result`, `Console`, …). What lets a
    /// `Fork`'s parent branch be resolved from `event.parent_id` alone.
    event_branch: HashMap<EventId, BranchId>,
    /// A forked branch → (the branch it forked from, the fork-point event
    /// id on that branch). `rows` walks this to reconstruct the inherited
    /// prefix without ever touching the `Tree`.
    fork_parent: HashMap<BranchId, (BranchId, EventId)>,
    /// `classify_entry_lines`'s output for each `entries[i]`, memoized —
    /// without it, `rows`/`rows_raw` re-run markdown classification
    /// (fence-tracking, table lookahead) for the *entire* history on
    /// every call, which in the attached TUI means every redraw tick,
    /// forever, whether or not anything changed. Same "quadratic without
    /// a memo, amortised O(1) with one" shape as `ReportMemo`
    /// (`types.rs`), one layer up. Two vectors since `render_markdown`
    /// changes an `Assistant`/`Streaming` entry's classification.
    /// `RefCell`: `rows`/`rows_raw` — this pane's entire read contract —
    /// only ever take `&self`. `push_entry` keeps both exactly as long
    /// as `entries`, so an index is always valid to look up.
    classified_line_cache: LineCache,
    raw_line_cache: LineCache,
    /// How many times `classify_entry_lines` has actually run (cache
    /// misses) — mirrors `ReportMemo::derivations` (`types.rs`), a
    /// counter for exactly this purpose: a test asserting the memo, not
    /// just the output, is doing its job.
    pub line_derivations: std::cell::Cell<u64>,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The one choke point every `self.entries.push` goes through
    /// instead — keeps both line caches exactly as long as `entries`, so
    /// an index is always valid to look up and `apply` never has to know
    /// the cache exists.
    fn push_entry(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.classified_line_cache.get_mut().push(None);
        self.raw_line_cache.get_mut().push(None);
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Chunk {
                branch,
                thinking,
                text,
                ..
            } => {
                let buffer = if *thinking {
                    &mut self.thinking_streaming
                } else {
                    &mut self.streaming
                };
                match buffer.iter_mut().find(|(b, _)| b == branch) {
                    Some((_, buf)) => buf.push_str(text),
                    None => buffer.push((*branch, text.clone())),
                }
            }
            SessionEvent::Error { branch, message } => {
                if let Some(b) = branch.or(self.main_branch) {
                    // Not tied to a log position — a live notification,
                    // not history — so it is visible only on an exact
                    // branch match, never inherited by a descendant fork.
                    self.push_entry(Entry::Line {
                        branch: b,
                        id: EventId::new(u64::MAX),
                        kind: ChatKind::Error,
                        text: format!("error: {message}"),
                        program: None,
                    });
                }
            }
            // The answer to a user's question is already this branch's
            // `Turn` in the transcript — the event only says it landed.
            SessionEvent::Answered { .. } => {}
            SessionEvent::Event {
                agent,
                branch,
                event,
            } => {
                self.event_branch.insert(event.id, *branch);
                self.apply_payload(*agent, *branch, event);
            }
            // Live program-block status titles the matching header.
            SessionEvent::ProgramStatus {
                program, status, ..
            } => {
                self.program_status.insert(*program, *status);
            }
            // Leaf/branch lists are for the navigator, not the chat pane;
            // a branch opening is the navigator's news too.
            SessionEvent::Leaves(_)
            | SessionEvent::Branches(_)
            | SessionEvent::BranchOpened { .. } => {}
        }
    }

    fn apply_payload(&mut self, agent: AgentId, branch: BranchId, event: &Event) {
        let id = event.id;
        match &event.payload {
            EventPayload::Agent { system, .. } => {
                // The system prompt is a snapshot on the root, not a
                // message: render it as this branch's leading block.
                self.main_branch.get_or_insert(branch);
                self.main_agent.get_or_insert(agent);
                self.push_entry(Entry::Line {
                    branch,
                    id,
                    kind: ChatKind::System,
                    text: system.clone(),
                    program: Some(id),
                });
            }
            // A subagent answering is a marker, not an ending: agents
            // never close, so the branch stays addressable after it.
            EventPayload::Answer { value, .. } => {
                if Some(agent) != self.main_agent {
                    self.push_entry(Entry::Line {
                        branch,
                        id,
                        kind: ChatKind::Marker,
                        text: format!("subagent answered: {}", short(value)),
                        program: None,
                    });
                }
            }
            EventPayload::Fork { name } => {
                // This branch's own root *is* the Fork event, so its id
                // is `id`/`branch` alike; `event.parent_id` is the fork
                // point on the branch it diverged from.
                if let Some(parent_point) = event.parent_id
                    && let Some(&parent_branch) = self.event_branch.get(&parent_point)
                {
                    self.fork_parent
                        .insert(branch, (parent_branch, parent_point));
                }
                self.push_entry(Entry::Line {
                    branch,
                    id,
                    kind: ChatKind::Marker,
                    text: format!(
                        "forked{}",
                        match name {
                            Some(n) => format!(" as «{n}»"),
                            None => String::new(),
                        }
                    ),
                    program: None,
                });
            }
            EventPayload::Message(Message::Post { from, origin }) => {
                self.push_entry(Entry::Line {
                    branch,
                    id,
                    kind: ChatKind::User,
                    text: crate::report::render_post(id, *from, origin),
                    program: None,
                });
            }
            EventPayload::Message(Message::Turn {
                author,
                text,
                thinking,
                tool_calls,
            }) => {
                self.streaming.retain(|(b, _)| *b != branch);
                self.thinking_streaming.retain(|(b, _)| *b != branch);
                if let Some(thinking) = thinking
                    && !thinking.is_empty()
                {
                    self.push_entry(Entry::Line {
                        branch,
                        id,
                        kind: ChatKind::Thinking,
                        text: thinking.clone(),
                        program: None,
                    });
                }
                // A `Turn { author: User }` is the user taking this
                // branch's turn (`Restart`). It renders as an assistant
                // message to the API — the *branch* acted — but the
                // person driving needs to see whose hand it was.
                let by_user = matches!(author, crate::types::Author::User);
                if !text.is_empty() || by_user {
                    let calls: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                    self.push_entry(Entry::Line {
                        branch,
                        id,
                        kind: if by_user {
                            ChatKind::Marker
                        } else {
                            ChatKind::Assistant
                        },
                        text: if by_user {
                            format!("you took this branch's turn: {}", calls.join(", "))
                        } else {
                            text.clone()
                        },
                        program: None,
                    });
                }
                // A `run_program` opens a new block keyed by this event;
                // a `resume` folds into the open one (decision 2).
                if let Some(call) = tool_calls.first()
                    && call.name == crate::machine::TOOL_RUN_PROGRAM
                {
                    let attachment_names: Vec<String> = call
                        .arguments
                        .get("attachments")
                        .and_then(|v| v.as_object())
                        .map(|obj| obj.keys().cloned().collect())
                        .unwrap_or_default();
                    let attachments: HashMap<String, String> = call
                        .arguments
                        .get("attachments")
                        .and_then(|v| v.as_object())
                        .map(|obj| {
                            obj.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    self.push_entry(Entry::Header {
                        branch,
                        program: id,
                        attachments: attachment_names,
                    });
                    self.current_program.insert(branch, id);
                    if !attachments.is_empty() {
                        self.attachment_content.insert(id, attachments);
                    }
                }
            }
            // A call is logged at dispatch, so its row appears the moment
            // it is issued; the `Result` completes the same row in place.
            // `wait_until` is exempt: a polling loop calls it repeatedly for
            // no reason a human watching needs to see, and it never has
            // interesting output — the log itself is untouched, only this
            // derived transcript view.
            EventPayload::Call(call) => {
                // A message to the user is a message, not a tool call — it
                // renders like the agent just spoke, same as a plain-text
                // Turn, with no `⚙ .../→ ...` tool-call framing. This is the
                // only place that text could ever be read; a Send to
                // another branch stays a compact tool-call summary, since
                // it's still readable in full there as a real Post.
                if let Call::Send {
                    to: Address::User,
                    text,
                    ..
                } = call
                {
                    self.push_entry(Entry::Line {
                        branch,
                        id,
                        kind: ChatKind::Assistant,
                        text: text.clone(),
                        program: None,
                    });
                } else {
                    let is_wait_until =
                        matches!(call, Call::Invoke { name, .. } if name.as_str() == "wait_until");
                    if !is_wait_until && let Some(&program) = self.current_program.get(&branch) {
                        let name = match call {
                            Call::Invoke { name, .. } => name.clone(),
                            Call::Send {
                                to: Address::Branch(id),
                                text,
                                expects_reply,
                                ..
                            } => {
                                let verb = if *expects_reply { "ask" } else { "tell" };
                                format!("{verb} #{}: {text}", id.as_u64())
                            }
                            Call::Send {
                                to: Address::User, ..
                            } => unreachable!("handled above"),
                            Call::Spawn { .. } => "spawn".to_owned(),
                        };
                        self.call_rows.insert(id, self.entries.len());
                        self.push_entry(Entry::Line {
                            branch,
                            id,
                            kind: ChatKind::ToolCall,
                            text: format!("⚙ {name} → …"),
                            program: Some(program),
                        });
                    }
                }
            }
            EventPayload::Result { call, outcome } => {
                if let Some(&row) = self.call_rows.get(call)
                    && let Some(Entry::Line { text, .. }) = self.entries.get_mut(row)
                {
                    let head = text.rsplit_once(" → ").map(|(h, _)| h.to_owned());
                    if let Some(head) = head {
                        *text = match outcome {
                            Outcome::Delivered(v) => format!("{head} → {}", short(v)),
                            Outcome::Failed(msg) => format!("{head} → failed: {msg}"),
                        };
                        // The row's own text just changed under it — the
                        // memo must forget it, or the "→ …" pending line
                        // never updates past its first render.
                        self.classified_line_cache.get_mut()[row] = None;
                        self.raw_line_cache.get_mut()[row] = None;
                    }
                }
            }
            // Execution/marker events are debug-pane data, never transcript.
            // The report body lives in the right console/result pane, not
            // the transcript (decision 2); it is derived from these.
            EventPayload::Return { .. }
            | EventPayload::Condition { .. }
            | EventPayload::Console { .. } => {}
            // A rename is a record: it changes the navigator, never the
            // transcript, and never wakes the branch.
            EventPayload::Rename { .. } => {}
        }
    }

    /// The ancestor chain from `target` back to its root-most branch,
    /// with the cutoff event id each non-final ancestor's own entries are
    /// bounded by — the event at which the *next* branch in the chain
    /// diverged from it. Reconstructs "history crosses a fork,
    /// obligations do not" from the event stream alone.
    fn ancestry(&self, target: BranchId) -> HashMap<BranchId, Option<EventId>> {
        let mut chain = HashMap::new();
        chain.insert(target, None);
        let mut cur = target;
        while let Some(&(parent, fork_point)) = self.fork_parent.get(&cur) {
            chain.insert(parent, Some(fork_point));
            cur = parent;
        }
        chain
    }

    /// Transcript rows for `branch` (or the main branch when `None`): one
    /// `(kind, line, detail, id)` per visual line — this branch's own
    /// events plus, for a fork, the shared prefix it inherited. `detail`
    /// carries click-hit metadata: which program, attachment, or invoke a
    /// row targets. `id` is the row's own underlying event — what a
    /// "fork at this point" gesture forks from (D2); a still-streaming
    /// row carries no logged id yet, so it gets a sentinel no branch root
    /// can ever equal. Multi-line items split; the system prompt
    /// collapses to a single header row.
    /// `width` is the pane's own content width — needed now that a table
    /// lays its columns out to fit it, not just its natural cell content:
    /// a table wider than the pane used to fall through to the generic
    /// word-wrapper, which has no notion of box-drawing structure and
    /// shredded the frame.
    pub fn rows(
        &self,
        branch: Option<BranchId>,
        width: usize,
    ) -> Vec<(ChatKind, String, RowDetail, EventId)> {
        self.rows_impl(branch, true, width)
    }

    /// Same rows, but with markdown block/inline classification skipped
    /// entirely — the stored `Entry::Line.text` is always the untouched
    /// original, so this is a re-derivation, never a lossy fallback: what
    /// settles "did the model actually fence/indent that?" when its own
    /// self-report (it never sees its own rendered output) can't be
    /// trusted.
    pub fn rows_raw(
        &self,
        branch: Option<BranchId>,
        width: usize,
    ) -> Vec<(ChatKind, String, RowDetail, EventId)> {
        self.rows_impl(branch, false, width)
    }

    fn rows_impl(
        &self,
        branch: Option<BranchId>,
        render_markdown: bool,
        width: usize,
    ) -> Vec<(ChatKind, String, RowDetail, EventId)> {
        let Some(target) = branch.or(self.main_branch) else {
            return Vec::new();
        };
        let chain = self.ancestry(target);
        let visible = |branch: BranchId, id: EventId| match chain.get(&branch) {
            None => false,
            Some(None) => true,
            Some(Some(cutoff)) => id.as_u64() <= cutoff.as_u64(),
        };
        let mut out = Vec::new();
        let mut invoke_index: HashMap<EventId, usize> = HashMap::new();
        let cache = if render_markdown {
            &self.classified_line_cache
        } else {
            &self.raw_line_cache
        };
        for (entry_index, entry) in self.entries.iter().enumerate() {
            match entry {
                Entry::Header {
                    branch,
                    program,
                    attachments,
                } if visible(*branch, *program) => {
                    let status = self
                        .program_status
                        .get(program)
                        .map(|s| status_label(*s))
                        .unwrap_or("running");
                    out.push((
                        ChatKind::ToolCall,
                        format!("run_program: {status}"),
                        RowDetail::Program(*program),
                        *program,
                    ));
                    for name in attachments {
                        out.push((
                            ChatKind::Attachment,
                            format!("⬡ attachment: {name}"),
                            RowDetail::Attachment(*program, name.clone()),
                            *program,
                        ));
                    }
                }
                Entry::Line {
                    branch,
                    id,
                    kind,
                    text,
                    program,
                } if visible(*branch, *id) => {
                    if *kind == ChatKind::System {
                        out.push((ChatKind::System, "system".into(), RowDetail::None, *id));
                        continue;
                    }
                    let detail = if *kind == ChatKind::ToolCall {
                        if let Some(pid) = program {
                            let idx = invoke_index.entry(*pid).or_insert(0);
                            let d = RowDetail::Invoke(*pid, *idx);
                            *idx += 1;
                            d
                        } else {
                            RowDetail::None
                        }
                    } else {
                        RowDetail::None
                    };
                    {
                        let mut cache_mut = cache.borrow_mut();
                        let stale = !matches!(&cache_mut[entry_index], Some((w, _)) if *w == width);
                        if stale {
                            self.line_derivations.set(self.line_derivations.get() + 1);
                            cache_mut[entry_index] = Some((
                                width,
                                classify_entry_lines(*kind, text, render_markdown, width),
                            ));
                        }
                    }
                    let cache_ref = cache.borrow();
                    for (k, line) in &cache_ref[entry_index].as_ref().unwrap().1 {
                        out.push((*k, line.clone(), detail.clone(), *id));
                    }
                }
                _ => {}
            }
        }
        for (b, buf) in &self.thinking_streaming {
            if *b != target {
                continue;
            }
            for line in buf.lines() {
                out.push((
                    ChatKind::Thinking,
                    line.to_owned(),
                    RowDetail::None,
                    EventId::new(u64::MAX),
                ));
            }
        }
        for (b, buf) in &self.streaming {
            if *b != target {
                continue;
            }
            if render_markdown {
                for (override_kind, line) in classify_markdown_lines(buf, width) {
                    out.push((
                        override_kind.unwrap_or(ChatKind::Streaming),
                        line,
                        RowDetail::None,
                        EventId::new(u64::MAX),
                    ));
                }
            } else {
                for line in buf.lines() {
                    out.push((
                        ChatKind::Streaming,
                        line.to_owned(),
                        RowDetail::None,
                        EventId::new(u64::MAX),
                    ));
                }
            }
        }
        out
    }
}

/// Classify one already-split-by-line piece of LLM prose into a markdown
/// block kind — `None` means "plain text, use whatever kind the caller
/// was already going to use." Must run *before* `push_wrapped` glues a
/// `"agent ❯ "`/`"you ❯ "` label onto the line, or a leading `#`/`>`/`|`
/// would never match; and it must see the *whole message*, not one row
/// at a time, since a fence's "am I inside one" state — and a table's
/// full row set, needed before any column width is known — both span
/// lines.
///
/// A ` ``` ` line toggles the fence and is dropped — it is punctuation,
/// never content. Inside a fence nothing is reinterpreted (no heading/
/// quote/table detection on code content); an unterminated fence at
/// end-of-input just leaves every remaining line `Code`, which is fine —
/// there is no separate "recovery" step the way `markdown.rs`'s inline
/// parser needs one, since a dangling fence has no dangling delimiter to
/// clean up. A table run consumes as many further pipe rows as follow
/// its delimiter row *right now*: called again once the streaming buffer
/// has grown, a still-arriving table just gets re-classified from
/// scratch with whatever rows have landed since — this can widen a
/// column as a later, longer cell arrives, but never panics or garbles
/// what's already there.
fn classify_markdown_lines(text: &str, width: usize) -> Vec<(Option<ChatKind>, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            out.push((Some(ChatKind::Code), line.to_owned()));
            i += 1;
            continue;
        }
        if line.trim().starts_with('|')
            && i + 1 < lines.len()
            && is_table_delimiter_row(lines[i + 1])
        {
            let header = table_cells(line);
            let ncols = header.len();
            let mut rows = vec![header];
            let mut j = i + 2;
            while j < lines.len() && lines[j].trim().starts_with('|') {
                rows.push(table_cells(lines[j]));
                j += 1;
            }
            let natural = column_widths(&rows, ncols);
            let typical = column_typical_widths(&rows[1..], ncols);
            // "│  " + cell + "  │  " between cells + "  │" to close —
            // the same total `format_table_row_lines`/
            // `format_table_border` actually spend per column (two
            // padding spaces either side, not the predecessor's one).
            let overhead = 5 * ncols + 1;
            let available = width.saturating_sub(overhead);
            let widths = distribute_widths(&natural, &typical, available);
            let mut rows = rows.into_iter().peekable();
            let header_row = rows.next().unwrap();
            out.push((
                Some(ChatKind::TableBorder),
                format_table_border(&widths, '┌', '┬', '┐'),
            ));
            for line in format_table_row_lines(&header_row, &widths, ncols) {
                out.push((Some(ChatKind::TableHeader), line));
            }
            out.push((
                Some(ChatKind::TableBorder),
                format_table_border(&widths, '├', '┼', '┤'),
            ));
            while let Some(row) = rows.next() {
                for line in format_table_row_lines(&row, &widths, ncols) {
                    out.push((Some(ChatKind::TableRow), line));
                }
                if rows.peek().is_some() {
                    out.push((
                        Some(ChatKind::TableBorder),
                        format_table_border(&widths, '├', '┼', '┤'),
                    ));
                }
            }
            out.push((
                Some(ChatKind::TableBorder),
                format_table_border(&widths, '└', '┴', '┘'),
            ));
            i = j;
            continue;
        }
        if is_horizontal_rule(line) {
            out.push((Some(ChatKind::Hr), "─".repeat(width)));
        } else if let Some(rest) = heading_text(line) {
            out.push((Some(ChatKind::Heading), rest.to_owned()));
        } else if let Some(rest) = line.strip_prefix('>') {
            out.push((
                Some(ChatKind::Blockquote),
                format!("▏ {}", rest.strip_prefix(' ').unwrap_or(rest)),
            ));
        } else {
            out.push((None, line.to_owned()));
        }
        i += 1;
    }
    out
}

/// A pipe row's cells, trimmed — the one leading/trailing `|` GFM makes
/// optional but every realistic LLM-generated table includes is
/// stripped first, so it never becomes a leading/trailing empty cell.
/// No `\|`-escaping support: not worth it for this renderer.
fn table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed.split('|').map(|c| c.trim().to_owned()).collect()
}

/// A table's delimiter row: every cell only `-`, optionally `:`-flanked
/// for GFM alignment — ignored here, everything left-justifies in a
/// monospace pane, so alignment markers are stripped and discarded.
fn is_table_delimiter_row(line: &str) -> bool {
    if !line.trim().starts_with('|') {
        return false;
    }
    table_cells(line).iter().all(|cell| {
        let dashes = cell.trim_start_matches(':').trim_end_matches(':');
        !dashes.is_empty() && dashes.chars().all(|c| c == '-')
    })
}

/// Each column's width: the widest cell across every row, header
/// included — `ncols` (the header's own cell count) is what every row
/// is measured/rendered against, so a ragged body row (real-world
/// tables routinely mismatch the delimiter's dash count or a row's cell
/// count) can never panic or desync a later column. A floor of 3 keeps
/// an empty column from collapsing to nothing.
///
/// Measured in display columns (`UnicodeWidthStr::width`), not
/// `.chars().count()` — a wide glyph like `✅` is one `char` but renders
/// two columns wide, and undercounting it here throws off every column
/// after it on that one row (the border keeps its intended width, but
/// that row's own padding falls one column short, misaligning it against
/// every other row). The predecessor's own `cell_char_len`
/// (`agent-cli/src/markdown.rs`) does the same width-aware measurement.
fn column_widths(rows: &[Vec<String>], ncols: usize) -> Vec<usize> {
    let mut widths = vec![3usize; ncols];
    for row in rows {
        for (col, width) in widths.iter_mut().enumerate() {
            let cell_width = row.get(col).map_or(0, |c| c.width());
            *width = (*width).max(cell_width);
        }
    }
    widths
}

/// Each column's *median* cell width across `body_rows` (the header
/// excluded — it's a deliberately chosen label, not a data sample) —
/// what a column would want to be if only the typical row mattered.
/// `column_widths`' natural width is a strict max, which lets a single
/// rare outlier cell (one long path next to nine short ones, say)
/// dictate the whole column's baseline size. `distribute_widths` uses
/// this as each column's own resistance point: a column can shrink most
/// of the way down to its typical width under pressure — the rare
/// outlier just wraps across extra lines — without that shrinkage ever
/// touching a column whose values are *uniformly* wide, where the
/// typical width is close to the max anyway and there's really nothing
/// to gain by calling it "just an outlier."
fn column_typical_widths(body_rows: &[Vec<String>], ncols: usize) -> Vec<usize> {
    (0..ncols)
        .map(|col| {
            let mut widths: Vec<usize> = body_rows
                .iter()
                .map(|row| row.get(col).map_or(0, |c| c.width()))
                .collect();
            if widths.is_empty() {
                return 0;
            }
            widths.sort_unstable();
            widths[widths.len() / 2]
        })
        .collect()
}

/// A column is never squeezed below this while another column still has
/// more than its share to give — enough room for a few wrapped words to
/// stay legible; below it a column stops reflowing and starts shredding
/// individual words letter by letter.
const MIN_COLUMN_WIDTH: usize = 8;

/// Shrink `natural` column widths to fit `available`, when they don't
/// already fit. Unchanged if they already fit.
///
/// This project first ported the predecessor's own `distribute_widths`
/// (`agent-cli/src/markdown.rs`) — scale every column down proportional
/// to its share of the *raw total* — but that's exactly wrong when one
/// column (a free-text description, say) is wildly larger than the
/// rest: its sheer size dominates the total, so the very first
/// proportional pass rounds every other column down to nothing before
/// any fairness logic ever runs. The next attempt overcorrected the
/// other way — shrink only whichever single column is currently
/// widest — which protects small columns completely, but *too*
/// completely: a handful of medium columns (each comfortably above the
/// floor, but none of them ever the single largest) never give up
/// anything at all, no matter how much room the oversized column
/// could actually use if they each gave a little.
///
/// The fix is what the predecessor was reaching for in the first place,
/// corrected: scale proportional to each column's *slack* — how much
/// natural width it has above `MIN_COLUMN_WIDTH` — not to its raw
/// natural width. A column already near the floor has almost no slack
/// and so gives up almost nothing, whatever its neighbor's size;
/// several medium columns each contribute their fair share; and a truly
/// oversized column, whose slack dwarfs everyone else's, still absorbs
/// nearly all of the reduction, exactly as it should.
fn distribute_widths(natural: &[usize], typical: &[usize], available: usize) -> Vec<usize> {
    // Tier 1: shrink toward each column's own *typical* width first — a
    // uniformly-wide column has little slack here and barely moves; a
    // mostly-short column with a rare outlier has lots and gives most
    // of it up (see `column_typical_widths`).
    let typical_floor: Vec<usize> = typical.iter().map(|&t| t.max(MIN_COLUMN_WIDTH)).collect();
    let widths = shrink_toward(natural, &typical_floor, available);
    if widths.iter().sum::<usize>() <= available {
        return widths;
    }
    // Tier 1 wasn't enough on its own — there's genuinely more content
    // than fits even at every column's typical size (several
    // legitimately wide columns, say), not just one column hoarding
    // width relative to its neighbors. Shrinking no further here would
    // silently let the table overflow the pane, exactly the bug this
    // whole file exists to avoid — so drop the typical protection and
    // shrink again, proportional to whatever's left above the hard
    // floor, sharing that pain too instead of giving up early.
    let hard_floor = vec![MIN_COLUMN_WIDTH; natural.len()];
    shrink_toward(&widths, &hard_floor, available)
}

/// Shrink `current` widths toward `floor` (never below it) to fit
/// `available`, proportional to each column's slack above its own
/// floor — unchanged if it already fits, and allowed to still exceed
/// `available` only once every column has hit its floor and there is
/// truly nothing left to give (the caller decides what floor to try
/// next, or to accept the overflow).
fn shrink_toward(current: &[usize], floor: &[usize], available: usize) -> Vec<usize> {
    let total: usize = current.iter().sum();
    if total <= available {
        return current.to_vec();
    }
    let slack: Vec<usize> = current
        .iter()
        .zip(floor)
        .map(|(&w, &f)| w.saturating_sub(f))
        .collect();
    let total_slack: usize = slack.iter().sum();
    let excess = total - available;
    if total_slack == 0 || excess >= total_slack {
        // Every column giving up its *entire* slack still isn't enough
        // (or nothing has any slack to give) — floor everyone that can
        // and let the caller decide whether that's the final answer.
        return current.iter().zip(&slack).map(|(&w, &s)| w - s).collect();
    }
    let mut widths: Vec<usize> = current
        .iter()
        .zip(&slack)
        .map(|(&w, &s)| w - (excess * s / total_slack))
        .collect();
    // Integer division leaves the reduction a little short of `excess`
    // — trim the remainder one column at a time from whoever currently
    // has the most slack left, same floor guarantee, until it actually
    // fits.
    loop {
        let sum: usize = widths.iter().sum();
        if sum <= available {
            break;
        }
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|&(i, &w)| w > floor[i])
            .max_by_key(|&(_, &w)| w)
        else {
            break;
        };
        widths[idx] -= 1;
    }
    widths
}

/// One cell's content, word-wrapped to `max_width` display columns —
/// ported from the predecessor's `wrap_cell`, plain strings only (a
/// table cell is never markdown-reinterpreted, so there are no styled
/// spans to carry through). Existing `\n`s split first, since a cell can
/// already be multi-line; `textwrap` (already a dependency, used the
/// same way in `attach.rs`) handles the actual word-wrap per hard line —
/// never split a word, an overlong single word gets its own overflowing
/// line regardless, same contract as everywhere else this project wraps.
fn wrap_table_cell(text: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
        } else if line.width() <= max_width {
            out.push(line.to_owned());
        } else {
            out.extend(
                textwrap::wrap(line, max_width)
                    .into_iter()
                    .map(String::from),
            );
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// `row`'s cells wrapped to `widths` and re-assembled into however many
/// output lines the tallest wrapped cell needs — a short cell pads with
/// blank lines so every column stays aligned row-by-row, matching the
/// predecessor's `render_data_row`. `│`, not `|` — box-drawing to match
/// the predecessor's own table rendering, the fidelity this was found to
/// have lost in the port. Ragged-input tolerant: a short row's missing
/// cells become empty ones, anything past `ncols` is ignored, the same
/// as `column_widths` already measures against.
///
/// Pads by display width, not `format!("{:<width$}")` — that pads by
/// `.chars().count()`, which is exactly the same undercount
/// `column_widths` avoids, just at render time instead of measurement
/// time; using one and not the other would just move the misalignment
/// rather than fix it.
///
/// Two spaces of padding either side of every cell, not one — a single
/// space read as asymmetric once a block's zero left margin (see
/// `classify_entry_lines`) put the table's own `┌`/`│` flush against the
/// pane's left border with nothing to balance the space already sitting
/// between a short row's content and the pane's right border.
fn format_table_row_lines(row: &[String], widths: &[usize], ncols: usize) -> Vec<String> {
    let wrapped: Vec<Vec<String>> = (0..ncols)
        .map(|col| {
            let cell = row.get(col).map_or("", String::as_str);
            wrap_table_cell(cell, widths[col])
        })
        .collect();
    let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (0..row_height)
        .map(|line_idx| {
            let cells: Vec<String> = (0..ncols)
                .map(|col| {
                    let line = wrapped[col].get(line_idx).map_or("", String::as_str);
                    let pad = widths[col].saturating_sub(line.width());
                    format!("{line}{}", " ".repeat(pad))
                })
                .collect();
            format!("│  {}  │", cells.join("  │  "))
        })
        .collect()
}

/// A border row — `┌───┬───┐` / `├───┼───┤` / `└───┴───┘` depending on
/// which three chars the caller passes — one call site per position
/// (`render_border_row`'s equivalent). `w + 4`: two padding spaces
/// either side of a cell's content, matching `format_table_row_lines`'s
/// `"│  {cell}  │"`.
fn format_table_border(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut line = String::new();
    line.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push(mid);
        }
        for _ in 0..w + 4 {
            line.push('─');
        }
    }
    line.push(right);
    line
}

/// A line that is nothing but 3+ of the same rule character (`-`, `*`,
/// or `_`), optionally spaced out (CommonMark allows `- - -`) — GFM's
/// thematic break. Checked before `heading_text`: a bare `---` must not
/// fall through to "not a heading, so plain text."
fn is_horizontal_rule(line: &str) -> bool {
    let stripped: String = line.trim().chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

/// `line` past its `#`..`######` marker, iff followed by whitespace (a
/// bare `#comment`-style line is not a heading) — `None` otherwise.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    rest.strip_prefix(' ').or(rest.strip_prefix('\t'))
}

/// One entry's visual lines — labelling user/assistant prose and
/// indenting continuation lines under the label. `Assistant`/`Streaming`
/// text additionally gets block-markdown classification per line — every
/// other kind (`User`, `Error`, `ToolCall`, ...) goes out unclassified,
/// the same scoping `render_chat` already applies to inline markdown.
///
/// Pure in `(kind, text, render_markdown)` — no `detail`/`id`, which are
/// the same for every line one entry expands to and stay the caller's
/// job to attach — which is exactly what makes this safe for
/// `ChatState::classified_line_cache`/`raw_line_cache` to memoize per
/// entry with no invalidation beyond the one place that needs it
/// (`EventPayload::Result`, below): an `Entry::Line`'s `text` is never
/// mutated after being pushed anywhere else.
fn classify_entry_lines(
    kind: ChatKind,
    text: &str,
    render_markdown: bool,
    width: usize,
) -> Vec<(ChatKind, String)> {
    let label = match kind {
        ChatKind::User => "you ❯ ",
        ChatKind::Assistant => "agent ❯ ",
        _ => "",
    };
    let classified: Vec<(Option<ChatKind>, String)> =
        if render_markdown && matches!(kind, ChatKind::Assistant | ChatKind::Streaming) {
            classify_markdown_lines(text, width)
        } else {
            text.lines().map(|l| (None, l.to_owned())).collect()
        };
    let mut out = Vec::new();
    let mut any = false;
    for (i, (override_kind, line)) in classified.into_iter().enumerate() {
        any = true;
        // A block construct (heading/blockquote/code/table/rule) never
        // shares its line with the label, even as its very first line —
        // reserving that margin for it would mean every one of its
        // lines (a table's column layout is uniform top to bottom) pays
        // for a label it may not even carry, right when it needs every
        // column it can get. Only plain prose keeps the "agent ❯ "/
        // "you ❯ " label and its continuation-line indent.
        let head = if override_kind.is_some() {
            String::new()
        } else if i == 0 {
            label.to_owned()
        } else {
            " ".repeat(label.chars().count())
        };
        out.push((override_kind.unwrap_or(kind), format!("{head}{line}")));
    }
    if !any {
        out.push((kind, label.to_owned()));
    }
    out
}

fn status_label(status: ProgramStatus) -> &'static str {
    match status {
        ProgramStatus::Running => "running",
        ProgramStatus::Suspended => "suspended",
        ProgramStatus::Completed => "completed",
        ProgramStatus::Failed => "failed",
    }
}

/// A compact one-line preview of an inner call's result.
fn short(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() <= 40 {
        return s;
    }
    let mut end = 40;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Author, Origin, ToolCall};
    use jiff::Timestamp;

    fn ev(id: u64, payload: EventPayload) -> SessionEvent {
        ev_on(1, id, None, payload)
    }

    fn ev_on(branch: u64, id: u64, parent: Option<u64>, payload: EventPayload) -> SessionEvent {
        SessionEvent::Event {
            agent: EventId::new(1),
            branch: EventId::new(branch),
            event: Event {
                id: EventId::new(id),
                parent_id: parent.map(EventId::new),
                timestamp: Timestamp::now(),
                payload,
            },
        }
    }

    fn run_program(id: u64) -> SessionEvent {
        ev(
            id,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "run_program".into(),
                    arguments: serde_json::json!({}),
                }],
            }),
        )
    }

    fn invoke(id: u64, name: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::Call(Call::Invoke {
                name: name.into(),
                args: serde_json::json!([]),
                site: 0,
            }),
        )
    }

    fn settled(id: u64, call: u64, result: serde_json::Value) -> SessionEvent {
        ev(
            id,
            EventPayload::Result {
                call: EventId::new(call),
                outcome: Outcome::Delivered(result),
            },
        )
    }

    fn post(id: u64, text: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: text.into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        )
    }

    #[test]
    fn transcript_builds_from_session_events_only() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "be helpful".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&post(2, "hi"));
        chat.apply(&SessionEvent::Chunk {
            branch: EventId::new(1),
            agent: EventId::new(1),
            thinking: false,
            text: "thinki".into(),
        });
        let rows = chat.rows(None, 80);
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::User && t.contains("hi"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Streaming && t.contains("thinki"))
        );

        // The logged assistant message replaces the stream.
        chat.apply(&run_program(3));
        let rows = chat.rows(None, 80);
        assert!(!rows.iter().any(|(k, _, _, _)| *k == ChatKind::Streaming));
        // A run_program renders as a status-titled block header.
        assert!(rows.iter().any(|(k, t, p, _)| *k == ChatKind::ToolCall
            && t == "run_program: running"
            && *p == RowDetail::Program(EventId::new(3))));

        // Return/Rename never reach the transcript.
        let before = chat.rows(None, 80).len();
        chat.apply(&ev(
            5,
            EventPayload::Return {
                value: serde_json::json!("done"),
            },
        ));
        chat.apply(&ev(
            6,
            EventPayload::Rename {
                name: "note".into(),
            },
        ));
        assert_eq!(chat.rows(None, 80).len(), before);
    }

    /// A run_program with two inner calls renders as one block: a
    /// status-titled header + two `⚙` lines, the header tracking
    /// `ProgramStatus`. No report body in the rows.
    #[test]
    fn run_program_block_lists_inner_calls_and_tracks_status() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&run_program(2));
        chat.apply(&invoke(3, "fetch"));
        chat.apply(&invoke(4, "store"));
        // The `Result`s complete the rows already pushed at dispatch —
        // two calls stay two lines, not four.
        chat.apply(&settled(5, 3, serde_json::json!("A")));
        chat.apply(&settled(6, 4, serde_json::json!(true)));

        let rows = chat.rows(None, 80);
        let glyphs: Vec<&(ChatKind, String, RowDetail, EventId)> = rows
            .iter()
            .filter(|(k, t, _, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
            .collect();
        assert_eq!(glyphs.len(), 2, "two inner-call lines");
        // Each ⚙ line carries the program id + invoke index for hit-testing.
        assert!(
            glyphs
                .iter()
                .all(|(_, _, p, _)| matches!(p, RowDetail::Invoke(_, _)))
        );
        assert!(
            glyphs
                .iter()
                .any(|(_, _, p, _)| *p == RowDetail::Invoke(EventId::new(2), 0))
        );
        assert!(
            glyphs
                .iter()
                .any(|(_, _, p, _)| *p == RowDetail::Invoke(EventId::new(2), 1))
        );

        // Header starts at running…
        assert!(
            chat.rows(None, 80)
                .iter()
                .any(|(_, t, _, _)| t == "run_program: running")
        );
        // …and tracks ProgramStatus to completed.
        chat.apply(&SessionEvent::ProgramStatus {
            branch: EventId::new(1),
            agent: EventId::new(1),
            program: EventId::new(2),
            status: ProgramStatus::Completed,
        });
        assert!(
            chat.rows(None, 80)
                .iter()
                .any(|(_, t, _, _)| t == "run_program: completed")
        );

        // The report body is never in the transcript — and now it is
        // never even an event: it is derived from the outcome.
        assert!(
            !chat
                .rows(None, 80)
                .iter()
                .any(|(_, t, _, _)| t.contains("program completed"))
        );
    }

    /// `rows()` re-scans every entry on every call (branch visibility is
    /// call-dependent, so that part can't be memoized) — but the
    /// expensive part, `classify_entry_lines`'s markdown parsing, must
    /// not redo work for entries nothing changed about. This is the
    /// direct check on the memo itself, not just on its output.
    #[test]
    fn repeated_rows_calls_do_not_re_derive_unchanged_entries() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "one **bold** line"));
        chat.apply(&assistant_turn(3, "another line"));

        chat.rows(None, 80);
        let after_first = chat.line_derivations.get();
        assert!(after_first > 0, "the first call must actually derive");

        chat.rows(None, 80);
        chat.rows(None, 80);
        assert_eq!(
            chat.line_derivations.get(),
            after_first,
            "a later call with nothing new must not re-derive any entry"
        );

        // A genuinely new entry must derive exactly once more — not the
        // whole history again.
        chat.apply(&assistant_turn(4, "a third line"));
        chat.rows(None, 80);
        assert_eq!(chat.line_derivations.get(), after_first + 1);
    }

    /// `classified_line_cache` and `raw_line_cache` are independent —
    /// switching modes must not read the other mode's memoized text
    /// (which would show classified content in raw view or vice versa).
    #[test]
    fn classified_and_raw_caches_do_not_cross_contaminate() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "# Heading"));

        chat.rows(None, 80); // primes the classified cache
        chat.rows_raw(None, 80); // primes the raw cache

        let classified = chat.rows(None, 80);
        assert!(
            classified
                .iter()
                .any(|(k, _, _, _)| *k == ChatKind::Heading)
        );
        let raw = chat.rows_raw(None, 80);
        assert!(
            raw.iter()
                .all(|(k, _, _, _)| matches!(k, ChatKind::System | ChatKind::Assistant)),
            "raw must still be unclassified even though the classified cache is warm: {raw:?}"
        );
    }

    /// A `Result` completing a `Call` row mutates that entry's `text`
    /// after it was already cached — without invalidating that one slot,
    /// the "⚙ name → …" pending line would never update to show the
    /// actual outcome. Everything else's memo must survive untouched.
    #[test]
    fn a_completed_call_invalidates_only_its_own_cached_row() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&run_program(2));
        chat.apply(&invoke(3, "fetch"));

        assert!(
            chat.rows(None, 80)
                .iter()
                .any(|(_, t, _, _)| t.contains("fetch → …")),
            "pending before the result lands"
        );
        let before = chat.line_derivations.get();

        chat.apply(&settled(4, 3, serde_json::json!("A")));
        assert!(
            chat.rows(None, 80)
                .iter()
                .any(|(_, t, _, _)| t.contains("fetch → \"A\"")),
            "the row must reflect the outcome, not the stale cached pending text"
        );
        assert_eq!(
            chat.line_derivations.get(),
            before + 1,
            "only the one row that actually changed re-derives"
        );
    }

    /// `wait_until` never becomes a row — a polling loop calling it
    /// repeatedly would otherwise spam the transcript with lines no one
    /// watching needs to see. Its `Result` is likewise silent (no row was
    /// ever inserted for `call_rows` to find). A sibling call is unaffected.
    #[test]
    fn wait_until_calls_are_never_transcript_rows() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&run_program(2));
        chat.apply(&invoke(3, "wait_until"));
        chat.apply(&invoke(4, "fetch"));
        chat.apply(&settled(5, 3, serde_json::json!(null)));
        chat.apply(&settled(6, 4, serde_json::json!("A")));

        let glyphs: Vec<String> = chat
            .rows(None, 80)
            .into_iter()
            .filter(|(k, t, _, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
            .map(|(_, t, _, _)| t)
            .collect();
        assert_eq!(glyphs, vec!["⚙ fetch → \"A\""]);
    }

    /// A `tell`/`ask` to the user must show its actual text — it is the
    /// only place that message could ever be read (unlike a Send to
    /// another branch, visible there too as a real `Post`).
    #[test]
    fn tell_and_ask_to_the_user_render_as_plain_assistant_messages() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&run_program(2));
        chat.apply(&ev(
            3,
            EventPayload::Call(Call::Send {
                to: Address::User,
                text: "hello 👋".into(),
                input: serde_json::Value::Null,
                expects_reply: false,
                site: 0,
            }),
        ));
        chat.apply(&ev(
            4,
            EventPayload::Call(Call::Send {
                to: Address::User,
                text: "what's your name?".into(),
                input: serde_json::Value::Null,
                expects_reply: true,
                site: 0,
            }),
        ));

        // Neither is a `⚙` tool-call row at all — both are plain
        // `Assistant`-kind messages, exactly like the agent's own text.
        assert!(
            !chat
                .rows(None, 80)
                .iter()
                .any(|(k, t, _, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
        );
        let messages: Vec<String> = chat
            .rows(None, 80)
            .into_iter()
            .filter(|(k, _, _, _)| *k == ChatKind::Assistant)
            .map(|(_, t, _, _)| t)
            .collect();
        assert_eq!(
            messages,
            vec!["agent ❯ hello 👋", "agent ❯ what's your name?"]
        );
    }

    /// `Agent.system` — the snapshot on the branch root — renders as the
    /// leading `system` row of its branch, and selecting another agent's
    /// branch shows that branch's slice (its own system block), not the
    /// root's.
    #[test]
    fn system_block_is_leading_and_per_branch() {
        let mut chat = ChatState::new();
        // Root branch #1.
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "root".into(),
                tools: None,
                system: "ROOT SYSTEM PROMPT".into(),
            },
        ));
        chat.apply(&post(3, "root q"));
        // Subagent branch #4 with its own system prompt.
        let child = EventId::new(4);
        chat.apply(&ev_on(
            4,
            4,
            None,
            EventPayload::Agent {
                name: None,
                charter: "child".into(),
                tools: None,
                system: "CHILD SYSTEM PROMPT".into(),
            },
        ));

        // Root's slice: leading system row, then the user message.
        let root_rows = chat.rows(Some(EventId::new(1)), 80);
        assert_eq!(root_rows[0].0, ChatKind::System);
        assert_eq!(root_rows[0].2, RowDetail::None);
        assert!(
            root_rows
                .iter()
                .any(|(k, t, _, _)| *k == ChatKind::User && t.contains("root q"))
        );
        // The child's content is not in the root's slice.
        assert!(!root_rows.iter().any(|(_, t, _, _)| t.contains("CHILD")));

        // The child's slice leads with its own system header.
        let child_rows = chat.rows(Some(child), 80);
        assert_eq!(child_rows[0].0, ChatKind::System);
        assert_eq!(child_rows[0].2, RowDetail::None);
        assert!(!child_rows.iter().any(|(_, t, _, _)| t.contains("root q")));
    }

    /// A fork's own rows include the shared prefix up to (and including)
    /// its fork point, but nothing the original branch does afterward —
    /// "history crosses a fork, obligations do not" (17_BRANCHES),
    /// reconstructed here from the event stream alone.
    #[test]
    fn fork_inherits_prefix_not_the_original_s_future() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "root".into(),
                tools: None,
                system: "SYS".into(),
            },
        ));
        chat.apply(&post(2, "shared question"));
        // The fork point: an assistant turn on the original branch.
        chat.apply(&ev_on(
            1,
            3,
            Some(2),
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: "shared answer".into(),
                thinking: None,
                tool_calls: vec![],
            }),
        ));
        // Fork at #3: branch id 10, rooted with parent_id = 3.
        chat.apply(&ev_on(
            10,
            10,
            Some(3),
            EventPayload::Fork {
                name: Some("try again".into()),
            },
        ));
        // After the fork: the original keeps going...
        chat.apply(&post(4, "original continues"));
        // ...and the fork has its own new activity.
        chat.apply(&ev_on(
            10,
            11,
            Some(10),
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "fork continues".into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        ));

        let fork_rows = chat.rows(Some(EventId::new(10)), 80);
        assert!(
            fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("shared question"))
        );
        assert!(
            fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("shared answer"))
        );
        assert!(
            fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("fork continues"))
        );
        assert!(
            !fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("original continues")),
            "a fork owes nothing of what the original does afterward"
        );

        let original_rows = chat.rows(Some(EventId::new(1)), 80);
        assert!(
            original_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("original continues"))
        );
        assert!(
            !original_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("fork continues")),
            "the original does not see the fork's own history"
        );
    }

    /// A logged `Turn`'s `thinking` becomes its own row, ahead of the
    /// assistant text it reasoned its way to — always in `rows()`,
    /// regardless of whether the attached UI chooses to show it.
    #[test]
    fn logged_thinking_renders_before_the_assistant_text() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&ev(
            2,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: "42".into(),
                thinking: Some("let me compute 6*7".into()),
                tool_calls: vec![],
            }),
        ));

        let rows = chat.rows(None, 80);
        let thinking_idx = rows
            .iter()
            .position(|(k, t, _, _)| *k == ChatKind::Thinking && t.contains("6*7"))
            .expect("the thinking row is present");
        let text_idx = rows
            .iter()
            .position(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("42"))
            .expect("the assistant row is present");
        assert!(
            thinking_idx < text_idx,
            "reasoning renders before the answer it led to"
        );
    }

    /// A live `thinking`-flagged chunk stream renders as its own kind,
    /// independent of the plain text stream, and is replaced by the
    /// logged row the same way the text stream is.
    #[test]
    fn live_thinking_chunks_stream_then_yield_to_the_logged_turn() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&SessionEvent::Chunk {
            branch: EventId::new(1),
            agent: EventId::new(1),
            thinking: true,
            text: "reasoning".into(),
        });
        chat.apply(&SessionEvent::Chunk {
            branch: EventId::new(1),
            agent: EventId::new(1),
            thinking: false,
            text: "partial answer".into(),
        });
        let rows = chat.rows(None, 80);
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Thinking && t.contains("reasoning"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Streaming && t.contains("partial answer"))
        );

        chat.apply(&ev(
            2,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: "final answer".into(),
                thinking: Some("done reasoning".into()),
                tool_calls: vec![],
            }),
        ));
        let rows = chat.rows(None, 80);
        assert!(
            !rows.iter().any(|(_, t, _, _)| t.contains("partial answer")),
            "the live text stream is replaced by the logged turn"
        );
        assert!(
            !rows.iter().any(|(k, t, _, _)| *k == ChatKind::Thinking
                && t.contains("reasoning")
                && !t.contains("done")),
            "the live thinking stream is replaced by the logged turn's own thinking"
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Thinking && t.contains("done reasoning"))
        );
    }

    fn agent_event() -> SessionEvent {
        ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        )
    }

    fn assistant_turn(id: u64, text: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: text.into(),
                thinking: None,
                tool_calls: vec![],
            }),
        )
    }

    /// `rows_raw` skips classification entirely: a heading, a blockquote,
    /// and a fence all come back exactly as written — the same stored
    /// `Entry::Line.text` `rows()` itself reads, just unclassified.
    #[test]
    fn rows_raw_skips_block_classification_entirely() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "# Heading\n> quoted\n```\ncode line\n```\nplain",
        ));

        let raw = chat.rows_raw(None, 80);
        assert!(
            raw.iter()
                .all(|(k, _, _, _)| matches!(k, ChatKind::System | ChatKind::Assistant)),
            "no Heading/Blockquote/Code row when markdown is off: {raw:?}"
        );
        let joined: Vec<&str> = raw.iter().map(|(_, t, _, _)| t.as_str()).collect();
        assert!(joined.iter().any(|t| t.contains("# Heading")));
        assert!(joined.iter().any(|t| t.contains("> quoted")));
        assert!(
            joined.iter().any(|t| t.contains("```")),
            "fence delimiters are not hidden in raw mode: {joined:?}"
        );

        // The classified version is unaffected — this is a read-time
        // choice, not a destructive one.
        let classified = chat.rows(None, 80);
        assert!(
            classified
                .iter()
                .any(|(k, _, _, _)| *k == ChatKind::Heading)
        );
    }

    /// A `# Heading` line becomes its own `Heading` row, stripped of its
    /// `---`/`***`/`___` alone on a line is a thematic break, rendered
    /// as a full-pane-width rule — not left as literal dashes, and not
    /// mistaken for a table delimiter (no leading `|`) or misdetected
    /// A block construct never pays for the "agent ❯ " label's margin,
    /// even directly after a prose line that does carry it: a table
    /// preceded by prose must be laid out identically to the same table
    /// with no prose at all — the label's width must never leak into
    /// its column math. A wide single-cell word (`x` × 30) makes this
    /// table wide enough that the old `width - label.len()` bug would
    /// A `distribute_widths` unit test isolated from the rest of the
    /// table pipeline: the user's exact reported shape — one wildly
    /// oversized free-text column next to several short label columns —
    /// must not crush the short columns down to the floor. A naive
    /// proportional scale does exactly that, since the oversized
    /// column's sheer size dominates the total and rounds every other
    /// column's *share* of it down to nothing before any fairness logic
    /// runs.
    #[test]
    fn oversized_column_absorbs_shrinkage_alone() {
        let natural = vec![7, 4, 10, 800]; // Entry, Type, Size/detail, free text
        let widths = distribute_widths(&natural, &[0; 4], 100);
        // Two of the three short columns have no slack above the floor
        // at all (7 and 4 are already under MIN_COLUMN_WIDTH) and so
        // are untouched; the third (10) gives up at most a sliver of
        // its one column of slack — nothing close to what the huge
        // column gives up.
        assert_eq!(
            &widths[..2],
            &natural[..2],
            "no-slack columns untouched: {widths:?}"
        );
        assert!(
            widths[2] + 2 >= natural[2],
            "a column with only 1-2 slack barely shrinks: {widths:?}"
        );
        assert!(
            widths[3] < natural[3] / 2,
            "the oversized column absorbs the overwhelming majority of the shrinkage: {widths:?}"
        );
        assert_eq!(
            widths.iter().sum::<usize>(),
            100,
            "the widths still add up to exactly the available budget"
        );
    }

    /// Every column keeps at least `MIN_COLUMN_WIDTH` even when the
    /// *sum* of natural widths still can't fit after every column but
    /// one has already hit the floor — there's nothing left to give, so
    /// the result is allowed to overflow `available` rather than being
    /// The user's second reported shape: several *medium* columns (each
    /// comfortably above the floor, but none the single largest) next
    /// to one huge free-text column. Every medium column must give up
    /// *something* — proportional to its own slack — rather than being
    /// fully exempt just because it never happens to be the current
    /// A column with nine short values and one rare long one has a
    /// *typical* (median) width close to the short case, not the
    /// outlier — `column_widths`' own natural width, a strict max,
    /// would report the outlier's width instead.
    #[test]
    fn column_typical_width_ignores_a_rare_outlier() {
        let mut rows: Vec<Vec<String>> = (0..9).map(|_| vec!["dir".to_owned()]).collect();
        rows.push(vec!["a very long one-off value".to_owned()]);
        let typical = column_typical_widths(&rows, 1);
        assert_eq!(
            typical[0], 3,
            "the median of nine 3-char values and one outlier is 3"
        );
    }

    /// A column where *every* value is uniformly long has a typical
    /// width close to its natural (max) one — there's no rare outlier
    /// to discount, so this floor must not differ meaningfully from a
    /// flat one.
    #[test]
    fn column_typical_width_tracks_natural_when_uniformly_wide() {
        let rows: Vec<Vec<String>> = (0..5)
            .map(|_| vec!["a fairly long and uniform value".to_owned()])
            .collect();
        let natural = column_widths(&rows, 1);
        let typical = column_typical_widths(&rows, 1);
        assert_eq!(typical[0], natural[0]);
    }

    /// The user's exact recollection: a column mostly full of short
    /// values, with one rare long outlier, should shrink most of the
    /// way to its *typical* size under pressure — leaving the outlier
    /// to wrap across extra lines — rather than keeping the outlier's
    /// full width just because a flat floor never got in its way. A
    /// uniformly-wide neighbor column, by contrast, must not be shrunk
    /// away just because it happens to look similar in `natural` width.
    #[test]
    fn mostly_short_column_shrinks_toward_its_typical_width_not_its_outlier() {
        // Column 0: nine short + one 40-char outlier (natural 40, typical ~3).
        // Column 1: uniformly ~40 chars every row (natural 40, typical ~40).
        let natural = vec![40, 40];
        let typical = vec![3, 40];
        let widths = distribute_widths(&natural, &typical, 50);
        assert_eq!(widths.iter().sum::<usize>(), 50);
        assert!(
            widths[0] < widths[1],
            "the mostly-short column must shrink well below the uniformly-wide one: {widths:?}"
        );
        assert!(
            widths[0] <= MIN_COLUMN_WIDTH.max(3) + 2,
            "it should land close to its own typical width, not just any floor: {widths:?}"
        );
    }

    /// max; the huge column still gives up the most by far.
    #[test]
    fn medium_columns_each_share_the_burden_proportional_to_their_own_slack() {
        let natural = vec![18, 11, 9, 8, 700]; // Name, Type, Size, Modified, description
        let widths = distribute_widths(&natural, &[0; 5], 100);
        assert_eq!(widths.iter().sum::<usize>(), 100);
        assert_eq!(
            widths[3], 8,
            "Modified had zero slack (already at the floor): untouched"
        );
        assert!(
            widths[0] < natural[0],
            "Name (10 slack) must give up something: {widths:?}"
        );
        assert!(
            widths[1] < natural[1],
            "Type (3 slack) must give up something: {widths:?}"
        );
        assert!(
            widths[4] < natural[4] / 8,
            "the description column still gives up the overwhelming majority: {widths:?}"
        );
        // Sharing the burden must leave the huge column with *more*
        // room than the old "only the single max ever shrinks"
        // approach would (it would have left every medium column at
        // its full natural size, giving description only
        // 100 - (18+11+9+8) = 54).
        assert!(
            widths[4] > 54,
            "spreading the reduction across every column with slack must leave the \
             oversized column with more room than shrinking it alone would: {widths:?}"
        );
    }

    /// crushed into unreadable single-character columns.
    #[test]
    fn distribute_widths_gives_up_at_the_floor_rather_than_overflowing_columns_further() {
        let natural = vec![50, 50, 50];
        let widths = distribute_widths(&natural, &[0; 3], 10);
        assert!(
            widths.iter().all(|&w| w == MIN_COLUMN_WIDTH),
            "every column floors out rather than going below it: {widths:?}"
        );
    }

    #[test]
    fn block_content_never_pays_for_the_label_margin() {
        let table = "| Path | What it is |\n|---|---|\n| p | xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx |";

        let mut with_prose = ChatState::new();
        with_prose.apply(&agent_event());
        with_prose.apply(&assistant_turn(
            2,
            &format!("Here's the layout:\n\n{table}"),
        ));
        let prose_rows = with_prose.rows(None, 40);
        let (_, prose, _, _) = prose_rows
            .iter()
            .find(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("layout"))
            .unwrap();
        assert!(prose.starts_with("agent ❯ "));

        let mut table_only = ChatState::new();
        table_only.apply(&agent_event());
        table_only.apply(&assistant_turn(2, table));
        let bare_rows = table_only.rows(None, 40);

        let border = |rows: &[(ChatKind, String, RowDetail, EventId)]| {
            rows.iter()
                .find(|(k, ..)| *k == ChatKind::TableBorder)
                .unwrap()
                .1
                .clone()
        };
        let (with_prose_border, bare_border) = (border(&prose_rows), border(&bare_rows));
        assert!(
            with_prose_border.starts_with('┌'),
            "no label margin before the table's own border: {with_prose_border:?}"
        );
        assert_eq!(
            with_prose_border, bare_border,
            "a preceding labeled prose line must not shrink the table that follows it"
        );
    }

    /// from a spaced-out `- - -` variant.
    #[test]
    fn horizontal_rule_renders_full_width() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "before\n---\nafter"));

        let rows = chat.rows(None, 20);
        let (kind, text, _, _) = rows
            .iter()
            .find(|(k, ..)| *k == ChatKind::Hr)
            .expect("an Hr row is present");
        assert_eq!(*kind, ChatKind::Hr);
        assert_eq!(text.width(), 20, "rendered at the full pane width");
        assert!(
            !text.contains('-'),
            "literal dashes replaced with the rule glyph"
        );

        for variant in ["***", "___", "- - -"] {
            let mut chat = ChatState::new();
            chat.apply(&agent_event());
            chat.apply(&assistant_turn(2, variant));
            assert!(
                chat.rows(None, 10).iter().any(|(k, ..)| *k == ChatKind::Hr),
                "{variant:?} must be recognized as a rule"
            );
        }

        // Two dashes is just a dash, not a rule; and a real table
        // delimiter must still win over a bare-dashes misread.
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "--\n| a | b |\n|---|---|\n| 1 | 2 |"));
        let rows = chat.rows(None, 20);
        assert!(!rows.iter().any(|(k, ..)| *k == ChatKind::Hr));
        assert!(rows.iter().any(|(k, ..)| *k == ChatKind::TableHeader));
    }

    /// marker, ahead of the plain-text row that follows it.
    #[test]
    fn heading_line_becomes_its_own_row_ahead_of_plain_text() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "# Heading\ntext after"));

        let rows = chat.rows(None, 80);
        let heading_idx = rows
            .iter()
            .position(|(k, t, _, _)| *k == ChatKind::Heading && t.contains("Heading"))
            .expect("the heading row is present");
        assert!(
            !rows[heading_idx].1.contains('#'),
            "the marker is stripped: {}",
            rows[heading_idx].1
        );
        let text_idx = rows
            .iter()
            .position(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("text after"))
            .expect("the plain-text row is present");
        assert!(heading_idx < text_idx);
    }

    /// A `> quoted` line becomes a `Blockquote` row with a gutter prefix,
    /// not a literal `>`.
    #[test]
    fn blockquote_line_gets_a_gutter_prefix() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "> quoted\nplain"));

        let rows = chat.rows(None, 80);
        let (_, text, _, _) = rows
            .iter()
            .find(|(k, t, _, _)| *k == ChatKind::Blockquote && t.contains("quoted"))
            .expect("the blockquote row is present");
        assert!(
            text.trim_start_matches("agent ❯ ").starts_with('▏'),
            "gutter prefix expected: {text}"
        );
    }

    /// A fenced ` ``` ` block's delimiter lines never become rows at
    /// all, its content lines are `Code`-kinded verbatim, and the prose
    /// before/after the fence still renders as plain `Assistant` text —
    /// the direct regression case for the reported corruption bug.
    #[test]
    fn fenced_code_block_hides_delimiters_and_keeps_content_literal() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "before\n```\ncode line\n```\nafter"));

        let rows = chat.rows(None, 80);
        assert!(
            !rows.iter().any(|(_, t, _, _)| t.contains("```")),
            "delimiter lines must never appear as rows"
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Code && t.contains("code line"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("before"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("after"))
        );
    }

    /// A heading marker *inside* a fence is never reinterpreted as a
    /// heading — code content is literal, hashes and all. This is the
    /// user's exact original repro: an LLM showing markdown source
    /// (headers included) inside a ` ```markdown ` fence.
    #[test]
    fn heading_marker_inside_a_fence_stays_literal_code() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "```markdown\n# Project Plan\n```"));

        let rows = chat.rows(None, 80);
        assert!(!rows.iter().any(|(_, t, _, _)| t.contains("```")));
        let (kind, text, _, _) = rows
            .iter()
            .find(|(_, t, _, _)| t.contains("Project Plan"))
            .expect("the fenced line is present");
        assert_eq!(*kind, ChatKind::Code, "not reinterpreted as a heading");
        assert!(text.contains('#'), "the hash is kept literally: {text}");
    }

    /// The classifier runs on the live streaming buffer too, not only a
    /// logged turn — an in-progress fence already renders `Code`-kinded
    /// before the turn ever completes.
    #[test]
    fn streaming_fence_content_is_code_kinded_before_the_turn_logs() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&SessionEvent::Chunk {
            branch: EventId::new(1),
            agent: EventId::new(1),
            thinking: false,
            text: "```\nin progress\n".into(),
        });

        let rows = chat.rows(None, 80);
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Code && t.contains("in progress")),
            "the still-open fence already streams as Code"
        );
        assert!(!rows.iter().any(|(_, t, _, _)| t.contains("```")));
    }

    /// Block classification is scoped to `Assistant`/`Streaming` prose —
    /// a `#`-prefixed line the *user* typed stays a plain `User` row,
    /// unstyled, matching the existing inline-markdown scoping decision.
    #[test]
    fn user_text_is_never_block_classified() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&post(2, "# not a heading"));

        let rows = chat.rows(None, 80);
        let (kind, text, _, _) = rows
            .iter()
            .find(|(_, t, _, _)| t.contains("not a heading"))
            .expect("the row is present");
        assert_eq!(*kind, ChatKind::User);
        assert!(text.contains('#'), "marker kept literally: {text}");
    }

    /// Two spaces of padding on both sides of every cell, symmetric —
    /// not the one-space form that read as lopsided once a table's own
    /// left edge sat flush against the pane border with nothing to
    /// balance the padding already on its right.
    #[test]
    fn table_cell_padding_is_two_spaces_both_sides() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        // 3-char cells match `column_widths`' own floor exactly, so no
        // extra fill-to-floor padding muddies the assertion below.
        chat.apply(&assistant_turn(
            2,
            "| AAA | BBB |\n|---|---|\n| 111 | 222 |",
        ));

        let rows = chat.rows(None, 80);
        let header = &rows
            .iter()
            .find(|(k, ..)| *k == ChatKind::TableHeader)
            .unwrap()
            .1;
        assert!(
            header.starts_with("│  AAA"),
            "two spaces after the opening │: {header:?}"
        );
        assert!(
            header.contains("AAA  │  BBB"),
            "two spaces on both sides of the inter-column │: {header:?}"
        );
        assert!(
            header.ends_with("BBB  │"),
            "two spaces before the closing │: {header:?}"
        );
    }

    /// A well-formed table becomes one `TableHeader` row and one
    /// `TableRow` per body row, the delimiter is dropped, and every row
    /// renders to the same length — the column-alignment `chat_style`'s
    /// bold/plain distinction depends on.
    #[test]
    fn well_formed_table_aligns_header_and_body_rows() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "| Name | Role |\n|------|------|\n| Ada | Engineer |\n| Grace | Admiral |",
        ));

        let rows = chat.rows(None, 80);
        assert!(!rows.iter().any(|(_, t, _, _)| t.contains('-')));
        let header = rows
            .iter()
            .find(|(k, _, _, _)| *k == ChatKind::TableHeader)
            .expect("header row present");
        let body: Vec<_> = rows
            .iter()
            .filter(|(k, _, _, _)| *k == ChatKind::TableRow)
            .collect();
        assert_eq!(body.len(), 2);
        assert!(header.1.contains("Name") && header.1.contains("Role"));
        assert!(body[0].1.contains("Ada") && body[0].1.contains("Engineer"));
        assert!(body[1].1.contains("Grace") && body[1].1.contains("Admiral"));
        let label_width = "agent ❯ ".chars().count();
        let lens: Vec<usize> = std::iter::once(header)
            .chain(body)
            .map(|(_, t, _, _)| t.chars().count() - label_width)
            .collect();
        assert!(
            lens.windows(2).all(|w| w[0] == w[1]),
            "every row renders to the same width: {lens:?}"
        );
    }

    /// The user's exact reported example: a `✅` cell (one `char`, two
    /// display columns) must not throw off every row after it. Every
    /// row — including the header, which has no wide glyph — must come
    /// out to the identical rendered width; a `.chars().count()`-based
    /// measurement would leave the `✅` rows one column narrower than
    /// The user's exact reported bug: a short column next to one whose
    /// natural content is wider than a realistic pane — previously this
    /// fell through to the generic word-wrapper, which has no notion of
    /// box-drawing and shredded the frame into garbled fragments. Every
    /// row (border and content alike) must now fit the pane, agree on
    /// one total width, and the long column must actually wrap rather
    /// than overflow.
    #[test]
    fn wide_table_column_wraps_to_fit_the_pane_instead_of_overflowing() {
        // The regression this reproduces: several columns that are each
        // individually wide with little slack relative to their OWN
        // typical width (every "Purpose" cell is a full sentence, every
        // "Name" a real filename) — so tier 1 (shrink toward typical)
        // isn't enough on its own, and without tier 2 falling back to
        // the hard floor, this used to overflow the pane outright.
        let mut probe = ChatState::new();
        probe.apply(&agent_event());
        probe.apply(&assistant_turn(
            2,
            "| Name | Kind | Size | Modified | Purpose |\n\
             |---|---|---|---|---|\n\
             | `Cargo.toml` | file | 72 B | Jun 18 | Workspace manifest; declares members `agent`, `interp`, `conformance` |\n\
             | `Cargo.lock` | file | 76 KB | Sep 7 | Locked dependency versions for the whole workspace |\n\
             | `agent/` | dir | — | Sep 7 | **Harness crate** — the product: branch-step machine, host loop, TUI, subagent tree |",
        ));
        for width in [60, 80, 100] {
            let rows = probe.rows(None, width);
            for (kind, t, _, _) in &rows {
                if matches!(
                    kind,
                    ChatKind::TableHeader | ChatKind::TableRow | ChatKind::TableBorder
                ) {
                    assert!(
                        t.width() <= width,
                        "row exceeds pane width {width}: {} cols: {t:?}",
                        t.width()
                    );
                }
            }
        }

        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "| Path | What it is |\n\
             |---|---|\n\
             | `agent/` | The agent crate (`src/`, `samples`) |\n\
             | `interp/` | The JS interpreter/VM crate (`src/`, `docs/`) |\n\
             | `conformance/` | Conformance suite: `tests/`, `expectations.json`, custom `harness/` |",
        ));

        let width = 80;
        let rows = chat.rows(None, width);
        let table_rows: Vec<&(ChatKind, String, RowDetail, EventId)> = rows
            .iter()
            .filter(|(k, ..)| {
                matches!(
                    k,
                    ChatKind::TableHeader | ChatKind::TableRow | ChatKind::TableBorder
                )
            })
            .collect();
        assert!(!table_rows.is_empty(), "the table must still be detected");

        for (_, t, _, _) in &table_rows {
            assert!(
                t.width() <= width,
                "row exceeds the pane width {width}: {} cols: {t:?}",
                t.width()
            );
        }
        let widths: Vec<usize> = table_rows.iter().map(|(_, t, _, _)| t.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every border and content row must share one rectangular frame width: {widths:?}"
        );
        let data_row_lines = table_rows
            .iter()
            .filter(|(k, ..)| *k == ChatKind::TableRow)
            .count();
        assert!(
            data_row_lines > 3,
            "the long column should have wrapped across extra lines for at least one of \
             the 3 data rows, not stayed one line each: {data_row_lines}"
        );
    }

    /// the rest, misaligning every border below them.
    #[test]
    fn wide_glyphs_do_not_misalign_later_rows() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "| Feature | Syntax | Renders? |\n\
             |---|---|---|\n\
             | Bold | `**text**` | ✅ |\n\
             | Table | pipes & dashes | ✅ |\n\
             | Quote | `> text` | ✅ |",
        ));

        let rows = chat.rows(None, 80);
        let label_width = "agent ❯ ".chars().count();
        let widths: Vec<usize> = rows
            .iter()
            .filter(|(k, ..)| {
                matches!(
                    k,
                    ChatKind::TableHeader | ChatKind::TableRow | ChatKind::TableBorder
                )
            })
            .map(|(_, t, _, _)| t.width() - label_width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every table row/border must render to the same display width: {widths:?}"
        );
    }

    /// The predecessor rendered tables with a full box-drawing frame
    /// (`┌─┬─┐`/`├─┼─┤`/`└─┴─┘`, `│` columns) — this checks that fidelity
    /// actually made it into the port: a top border, the header, a
    /// separator, each body row (with a separator between them, not just
    /// after the header), and a bottom border, in that exact order.
    #[test]
    fn table_renders_a_full_box_drawing_frame() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "| Name | Role |\n|------|------|\n| Ada | Engineer |\n| Grace | Admiral |",
        ));

        let rows = chat.rows(None, 80);
        let kinds: Vec<ChatKind> = rows.iter().map(|(k, ..)| *k).collect();
        assert_eq!(
            kinds,
            vec![
                ChatKind::System,
                ChatKind::TableBorder,
                ChatKind::TableHeader,
                ChatKind::TableBorder,
                ChatKind::TableRow,
                ChatKind::TableBorder,
                ChatKind::TableRow,
                ChatKind::TableBorder,
            ],
            "border/header/border/row/border/row/border, no bare pipe rows"
        );
        // Table rows carry no label/indent at all, even the very first
        // one — a block construct never shares its line with "agent ❯ "
        // (chat.rs, `classify_entry_lines`), so it can use the pane's
        // full width instead of paying for a label it doesn't show.
        let text = |k: ChatKind| rows.iter().find(|(rk, ..)| *rk == k).unwrap().1.clone();
        assert!(text(ChatKind::TableHeader).starts_with('│'));
        let mut borders = rows.iter().filter(|(k, ..)| *k == ChatKind::TableBorder);
        assert!(borders.next().unwrap().1.starts_with('┌'));
        let mid = &borders.next().unwrap().1;
        assert!(mid.starts_with('├') && mid.contains('┼') && mid.ends_with('┤'));
        assert!(
            borders.next_back().unwrap().1.ends_with('┘'),
            "the very last row is the bottom border"
        );
    }

    /// A pipe-row block with NO delimiter row at all (a model that wrote
    /// a table by hand, forgot the `|---|---|` line, and later insisted
    /// it was there) must never be recognized as a table and must never
    /// lose a row — every pipe line passes through byte-for-byte, in
    /// both classified and raw rendering, since there is nothing here
    /// for `classify_markdown_lines` to legitimately consume. This is
    /// the direct check for "is something eating the delimiter/a row"
    /// when the real answer is "the model never sent one."
    #[test]
    fn table_without_a_delimiter_row_is_never_touched() {
        let text = "| Feature | Renders as | Notes |\n\
                     | # headings | Large bold titles | up to 6 levels |\n\
                     | **bold** | bold | two asterisks |";
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, text));

        for (label, rows) in [
            ("classified", chat.rows(None, 80)),
            ("raw", chat.rows_raw(None, 80)),
        ] {
            assert!(
                !rows
                    .iter()
                    .any(|(k, _, _, _)| matches!(k, ChatKind::TableHeader | ChatKind::TableRow)),
                "{label}: no delimiter row means no table, ever"
            );
            for line in text.lines() {
                assert!(
                    rows.iter().any(|(_, t, _, _)| t.contains(line)),
                    "{label}: line {line:?} must survive verbatim; got {rows:?}"
                );
            }
        }
    }

    /// The user's exact reported example: mismatched delimiter dash
    /// counts, a body row missing a space before its closing `|`, and
    /// (in this test) a short row with fewer cells than the header —
    /// none of it may panic, and every row still comes out aligned.
    #[test]
    fn ragged_table_does_not_panic_and_still_aligns() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "| Name | Role | City |\n\
             |-------|----------|----------|\n\
             | Ada | Engineer | London |\n\
             | Grace | Admiral | Arlington|\n\
             | Short |",
        ));

        let rows = chat.rows(None, 80);
        assert!(!rows.iter().any(|(_, t, _, _)| t.contains("---")));
        let table_rows: Vec<_> = rows
            .iter()
            .filter(|(k, _, _, _)| matches!(k, ChatKind::TableHeader | ChatKind::TableRow))
            .collect();
        assert_eq!(table_rows.len(), 4, "header + 3 body rows, none dropped");
        let label_width = "agent ❯ ".chars().count();
        let lens: Vec<usize> = table_rows
            .iter()
            .map(|(_, t, _, _)| t.chars().count() - label_width)
            .collect();
        assert!(
            lens.windows(2).all(|w| w[0] == w[1]),
            "a ragged row still pads out to the common width: {lens:?}"
        );
        assert!(
            table_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("Grace") && t.contains("Arlington"))
        );
        assert!(table_rows.iter().any(|(_, t, _, _)| t.contains("Short")));
    }

    /// Prose surrounding a table is untouched by it.
    #[test]
    fn prose_around_a_table_stays_plain_assistant_text() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(
            2,
            "before\n| A | B |\n|---|---|\n| 1 | 2 |\nafter",
        ));

        let rows = chat.rows(None, 80);
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("before"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Assistant && t.contains("after"))
        );
    }

    /// A pipe table *inside* a fence is never reinterpreted as a table —
    /// the fence wins, same rule already established for headers.
    #[test]
    fn table_inside_a_fence_stays_literal_code() {
        let mut chat = ChatState::new();
        chat.apply(&agent_event());
        chat.apply(&assistant_turn(2, "```\n| a | b |\n|---|---|\n```"));

        let rows = chat.rows(None, 80);
        assert!(!rows.iter().any(|(_, t, _, _)| t.contains("```")));
        assert!(
            !rows
                .iter()
                .any(|(k, _, _, _)| matches!(k, ChatKind::TableHeader | ChatKind::TableRow)),
            "no row was reinterpreted as a table"
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Code && t.contains("| a | b |"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Code && t.contains("|---|---|"))
        );
    }
}

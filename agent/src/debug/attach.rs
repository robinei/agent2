//! Attached mode (9_TUI Step 4): the same debug panes over a live
//! harness session — this *is* the harness TUI (decision 6). The chat
//! pane consumes `SessionEvent`s only (`chat.rs`); user input goes
//! through `SessionCommand`; the debug panes borrow the selected
//! agent's VM and the tree directly because rendering happens on the
//! loop thread (decision 4): crossterm input arrives as inbox messages
//! via a cloned `SessionHandle`, and we render after draining.
//!
//! Layout state machine (pure UI state — nothing in the host changes):
//! - **Chat** (default): full-width chat.
//! - **Running**: auto-popped when the *selected* agent starts a new
//!   program (every `Turn` is one now, 22_ONE_VOCABULARY) — source +
//!   console as a right column, sticky after completion for post-mortem
//!   reading; `c` collapses back, `1`–`4` override the auto-pop set.
//! - **FullDebug** (`d`): the standalone layout — console/result left,
//!   full debug pane stack right, chat hidden; `1`–`9` switch agents.
//!
//! Keys are focus-modal so chat typing stays free: printable keys go
//! to the input line; `Esc` swaps to debug-control focus (and back).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::PaneInfo;
use super::chat::{ChatKind, ChatState, RowDetail};
use super::input::InputBuffer;
use super::markdown;
use super::ui;
use crate::host::{BranchId, BranchInfo, Session, SessionCommand, SessionEvent};
use crate::report::derived_branch_label;
use crate::tree::ProgramView;
use crate::types::{Call, EventId, EventPayload, Outcome};

/// Cap for one step-line key, so a hot loop on one source line cannot
/// wedge the UI (mirrors the standalone runner).
const LINE_STEP_CAP: u64 = 50_000;

/// The input box's height caps at this fraction of the chat column's
/// height (19_UX Step A2), so a long prefilled program still leaves
/// the chat pane standing rather than filling the whole screen.
const INPUT_MAX_HEIGHT_FRACTION: u16 = 4;

/// The navigator's height caps at this fraction of the right column's
/// height (19_UX Step E0), so a deep fork/spawn tree still leaves room
/// for chat/source/console instead of consuming the whole screen.
const NAVIGATOR_HEIGHT_FRACTION: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum View {
    Chat,
    Running,
    FullDebug,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    /// Printable keys type into the chat input line.
    Input,
    /// Bare keys are debugger controls.
    Debug,
}

/// What is selected inside a program block — the thing the detail pane
/// shows. Two shapes because they are addressed differently: a call by
/// its position in `ProgramView::invokes`, an append by its own event
/// id (it is not an invoke, and numbering them together would shift
/// every call after the first append).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    Invoke(usize),
    Append(EventId),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pane {
    Chat,
    Navigator,
    Source,
    Disasm,
    Stack,
    Promises,
    Console,
    /// **What you selected**, shown in full — a call's arguments and
    /// result, or an append's value. It replaced `Source` on the right
    /// of the chat views: the source is already on the screen (the
    /// block shows its own cells) and what a reader wants beside it is
    /// the thing they just clicked. `Source` stays in `FullDebug`,
    /// where stepping is the point.
    Detail,
    /// The message/rewrite box (19_UX Step C3) — its own hit-testable
    /// rect, distinct from `Chat`'s transcript, so a click there is
    /// routed correctly instead of misread as a chat-transcript row.
    Input,
}

/// What the current view state renders — the headless output of the
/// layout state machine.
#[derive(Debug, PartialEq)]
pub struct PaneSet {
    pub chat: bool,
    /// Console/result fills the left column (full debugger mode).
    pub console_left: bool,
    pub right: Vec<Pane>,
}

/// Session-affecting result of a keypress; everything layout-local is
/// handled inside `on_key`.
#[derive(Debug, PartialEq)]
pub enum KeyAction {
    None,
    /// The input line's default behaviour: `UserTurn` (`expects_reply`)
    /// or `Reply` — the caller resolves which by whether the selected
    /// branch has a pending ask (17_BRANCHES: "Reply when the branch has
    /// a pending ask to you").
    Submit {
        text: String,
        expects_reply: bool,
    },
    /// One of the explicit input modes' submissions (rename, resume with
    /// a value, paste a rewrite, spawn's charter).
    SubmitMode(ExplicitMode, String),
    TogglePause,
    StepInstr,
    StepLine,
    /// Fork the selected branch at its current leaf — "ask without
    /// pausing it," no separate gesture from forking mid-program.
    Fork,
    /// Fork the selected branch at a specific logged event — the last
    /// chat row clicked.
    ForkAt(EventId),
    /// Cancel the selected branch's in-flight generation, or pause its
    /// program at the next slice.
    Interrupt,
    /// Select the next branch (cyclically) with a pending ask-to-user.
    JumpToWaiting,
    /// Jump to the timeline's currently highlighted branch and close it.
    JumpTimeline,
    /// The rewrite gesture was armed (`r` in `FullDebug`) — the driving
    /// loop resolves the current program's source and prefills the
    /// input buffer with it (needs `Session`, which `on_key` doesn't
    /// have). App-local state (view, focus, `explicit_mode`) is already
    /// set by the time this is returned.
    ArmRewrite,
}

/// An explicit input-line sub-mode (D2's restart/rename/spawn keys):
/// what the next Enter submits, instead of the default ask/tell/reply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExplicitMode {
    /// `r` — `Rename { branch, name }`.
    Rename,
    /// `v` — `Restart { branch, source: "history.note(resume(<value>));" }`; the
    /// text is parsed as JSON, falling back to a bare string, and
    /// spliced into the synthesized program — what's shown in the pane
    /// is exactly what runs (22_ONE_VOCABULARY's `UserCall` collapse).
    ResumeWithValue,
    /// `e` — `Restart { branch, source }`, the pasted text verbatim: a
    /// full rewrite, unchanged in spirit from before `UserCall` existed.
    Rewrite,
    /// `p` — `Spawn { parent: branch, charter, name: None, text: None }`.
    SpawnCharter,
}

pub struct AttachedApp {
    pub chat: ChatState,
    pub view: View,
    prev_view: View,
    pub focus: Focus,
    pub selected: Option<BranchId>,
    /// Which program the right-hand panes show (decision 1). `None` ⇒ the
    /// selected branch's most-recent program (the default); a click on an
    /// older chat block pins a specific one by its own `Turn` event id.
    pub selected_program: Option<EventId>,
    /// The effect selected within the selected program — what the
    /// detail pane shows. An invoke is addressed by its index into
    /// `ProgramView.invokes`), shown in the right panel instead of the
    /// program console.
    pub selected_effect: Option<Effect>,
    /// Branches whose `System` block is folded to its header (decision 7).
    collapsed: HashSet<BranchId>,
    pub input: InputBuffer,
    pub quit: bool,
    /// A bare `Ctrl-C` with nothing to clear and nothing to interrupt
    /// arms this instead of quitting; the next one quits. Any other key
    /// disarms it, so the confirmation is about *this* keystroke rather
    /// than a mode you can end up in without noticing.
    pub quit_armed: bool,
    pub show_source: bool,
    pub show_disasm: bool,
    pub show_stack: bool,
    pub show_promises: bool,
    /// Ctrl-T: reasoning content is logged and always in `chat.rows()`,
    /// hidden by default — most of it is noise once the final answer is
    /// in, so showing it is an explicit ask, not the default.
    pub show_thinking: bool,
    /// `m`: markdown styling on by default; toggled off shows exactly
    /// what the model emitted — no block classification (headings,
    /// blockquotes, fences, tables), no inline emphasis. `chat.rows()`
    /// always has the raw text to fall back to (`rows_raw`), so this
    /// never loses anything, only re-derives from source on demand.
    pub show_markdown: bool,
    /// **What the selected branch is doing, while it is doing it** —
    /// the word from `branch_infos()` (`thinking`, `running`,
    /// `queued`), and when it started.
    ///
    /// Set each tick from live session state rather than from
    /// `SessionEvent`s, because it is not a fact about the transcript:
    /// `ChatState` is fed only by the protocol and nothing live leaks
    /// into it (that module's own doc), and a spinner is live by
    /// definition.
    ///
    /// **Why it exists.** A model that spends its whole budget on
    /// reasoning writes nothing to the pane — reasoning rows are hidden
    /// unless Ctrl-T is on, and no `Reply` is logged until the first
    /// *text* chunk — so a completion that is working hard and a dead
    /// socket looked exactly alike: a static pane. `try23.jsonl` has
    /// twelve minutes of that, ended by the person quitting, and the
    /// completion it was waiting on reported 11,318 reasoning tokens.
    pub busy: Option<(String, Instant)>,
    pub chat_scroll: Option<usize>,
    pub console_scroll: Option<usize>,
    pub source_scroll: Option<usize>,
    pub detail_scroll: Option<usize>,
    pub disasm_scroll: Option<usize>,
    pub stack_scroll: Option<usize>,
    pub promises_scroll: Option<usize>,
    /// `None` auto-follows the selected branch's row; `Some(n)` is a
    /// manual wheel-scroll, reset back to auto-follow by `select_branch`
    /// (19_UX Step E0) — needed now that the navigator's height is
    /// capped instead of always growing to fit the whole tree.
    pub navigator_scroll: Option<usize>,
    pub pane_rects: Vec<(Pane, PaneInfo)>,
    /// Wrapped chat line → logical row index, rebuilt by `render_chat`
    /// every frame. A chat row wraps to one-or-more terminal lines
    /// (`push_wrapped_width`), so `PaneInfo.scroll_top` and a click's
    /// in-pane offset are both wrapped-line coordinates — indexing
    /// `chat.rows()` with them directly (a row-space list) drifts by
    /// however many extra lines any wrapped row above the click added,
    /// landing clicks on the wrong row once anything has wrapped. This
    /// is the one source of truth translating one space to the other.
    pub chat_line_rows: Vec<usize>,
    last_chat_lines: usize,
    /// The currently selected chat row, toggled by clicking it — what
    /// `f` forks from when set (19_UX Step C2); with nothing selected,
    /// `f` forks from the branch's current leaf instead.
    pub last_clicked_event: Option<EventId>,
    /// What the next Enter submits, when it isn't the default ask/tell/
    /// reply — set by the rename/resume/rewrite/spawn keys, cleared on
    /// submit or `Esc`.
    pub explicit_mode: Option<ExplicitMode>,
    /// The next Enter submits as an **ask** (`expects_reply: true`)
    /// instead of the default tell — armed by a dedicated key
    /// (`arm_ask`), cleared on submit or `Esc`. A dedicated key rather
    /// than a modifier on Enter: Alt+Enter is not reliably delivered —
    /// many terminals and window managers claim it for their own
    /// fullscreen toggle before it ever reaches the app, and
    /// modifier+Enter chords are ambiguous in general without an
    /// enhanced keyboard protocol, since Enter's own control code
    /// already occupies the byte a modifier would need to alter.
    pub ask_armed: bool,
    /// The timeline: every post of yours across branches, a filter you
    /// open rather than a place you live (17_BRANCHES Part D).
    pub timeline: bool,
    pub timeline_cursor: usize,
    /// Every message this attached session has submitted from the input
    /// box, oldest first — the up-arrow history. In-memory only (not
    /// read back from the log), scoped to one attach.
    history: Vec<String>,
    /// `Some(i)` while browsing `history` backward from the live draft
    /// (`i` indexes `history`); `None` when `input` holds the live draft
    /// rather than a recalled entry.
    history_cursor: Option<usize>,
    /// The draft `input` held before the first `Up` started browsing —
    /// restored verbatim (cursor and all) when `Down` walks back past
    /// the newest entry.
    history_draft: Option<InputBuffer>,
}

impl AttachedApp {
    pub fn new(root: BranchId) -> Self {
        let mut chat = ChatState::new();
        // A `Turn` is a cell of a reply, shown inline in the transcript
        // (D13).
        chat.set_show_cells(true);
        AttachedApp {
            chat,
            view: View::Chat,
            prev_view: View::Chat,
            focus: Focus::Input,
            selected: Some(root),
            selected_program: None,
            selected_effect: None,
            collapsed: HashSet::new(),
            input: InputBuffer::new(),
            quit: false,
            quit_armed: false,
            show_source: false,
            show_disasm: false,
            show_stack: false,
            show_promises: false,
            show_thinking: false,
            busy: None,
            show_markdown: true,
            chat_scroll: None,
            console_scroll: None,
            source_scroll: None,
            detail_scroll: None,
            disasm_scroll: None,
            stack_scroll: None,
            promises_scroll: None,
            navigator_scroll: None,
            pane_rects: Vec::new(),
            chat_line_rows: Vec::new(),
            last_chat_lines: 0,
            last_clicked_event: None,
            explicit_mode: None,
            ask_armed: false,
            timeline: false,
            timeline_cursor: 0,
            history: Vec::new(),
            history_cursor: None,
            history_draft: None,
        }
    }

    /// `Up` at the top row: recall the previous history entry, stashing
    /// the live draft on the way in. No-op with no (further) history.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.history_draft = Some(self.input.clone());
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        self.input = InputBuffer::prefilled(&self.history[next]);
    }

    /// `Down` at the bottom row: recall the next (newer) history entry,
    /// or the stashed live draft once the newest entry is passed. No-op
    /// when not currently browsing history.
    fn history_down(&mut self) {
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 == self.history.len() {
            self.history_cursor = None;
            self.input = self.history_draft.take().unwrap_or_default();
        } else {
            self.history_cursor = Some(i + 1);
            self.input = InputBuffer::prefilled(&self.history[i + 1]);
            self.input.bottom();
        }
    }

    /// Feed one `SessionEvent`: updates the transcript and drives the
    /// auto-pop — the selected branch starting a new program (every
    /// `Turn` is one now) pops the source + console column (decision 6).
    pub fn apply(&mut self, event: &SessionEvent) {
        if let SessionEvent::Event { branch, event, .. } = event
            && Some(*branch) == self.selected
            && matches!(&event.payload, EventPayload::Reply)
        {
            // Follow the live program: every `Turn` is one now
            // (22_ONE_VOCABULARY's "a turn is a program"), so a fresh one
            // on the selected agent drops any pinned older program, the
            // same way a `run_program` tool call used to trigger this.
            self.selected_program = None;
            if self.view == View::Chat {
                self.view = View::Running;
                // A fresh pop resets to the auto-pop set; later manual
                // toggles override it until the next pop. The source is
                // not in that set any more — `Pane::Detail` has its
                // slot, and `1` brings the source back for anyone who
                // wants it.
                self.show_source = false;
                self.show_disasm = false;
                self.show_stack = false;
                self.show_promises = false;
            }
        }
        self.chat.apply(event);
    }

    pub fn reset_scrolls(&mut self) {
        self.chat_scroll = None;
        self.console_scroll = None;
        self.source_scroll = None;
        self.disasm_scroll = None;
        self.stack_scroll = None;
        self.promises_scroll = None;
    }

    pub fn auto_reset_chat_scroll(&mut self) {
        let current = self
            .chat
            .rows(self.selected, self.chat_wrap_width(), self.selected_program)
            .len();
        if current != self.last_chat_lines {
            self.chat_scroll = None;
            self.last_chat_lines = current;
        }
    }

    pub fn on_mouse(&mut self, column: u16, row: u16, kind: MouseEventKind, branches: &[BranchId]) {
        if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
            self.on_click(column, row, branches);
            return;
        }
        let delta: i64 = match kind {
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::ScrollUp => -3,
            _ => return,
        };
        let Some((pane, info)) = self.pane_at(column, row) else {
            return;
        };
        let new = (info.scroll_top as i64 + delta).max(0) as usize;
        match pane {
            Pane::Chat => self.chat_scroll = Some(new),
            Pane::Console => self.console_scroll = Some(new),
            Pane::Source => self.source_scroll = Some(new),
            Pane::Detail => self.detail_scroll = Some(new),
            Pane::Disasm => self.disasm_scroll = Some(new),
            Pane::Stack => self.stack_scroll = Some(new),
            Pane::Promises => self.promises_scroll = Some(new),
            Pane::Navigator => self.navigator_scroll = Some(new),
            Pane::Input => {}
        }
    }

    /// The width `render_chat` last wrapped (and now lays tables out)
    /// against — table layout needs to be measured at the *same* width
    /// `rows()` was called with there, or `chat_line_rows`'s wrapped-line
    /// offsets (built off that render) desync from a `rows()` called here
    /// at a different width. Before the first render there is no
    /// geometry yet; 80 is a plain, unsurprising placeholder no real
    /// terminal is likely to be narrower than.
    fn chat_wrap_width(&self) -> usize {
        self.pane_rects
            .iter()
            .find(|(p, _)| *p == Pane::Chat)
            .map(|(_, info)| info.area.width.saturating_sub(2 + 2 * CHAT_MARGIN).max(1) as usize)
            .unwrap_or(80)
    }

    /// The pane (and its last-rendered geometry) under a cell, if any.
    fn pane_at(&self, column: u16, row: u16) -> Option<(Pane, PaneInfo)> {
        self.pane_rects
            .iter()
            .find(|(_, info)| {
                column >= info.area.x
                    && column < info.area.right()
                    && row >= info.area.y
                    && row < info.area.bottom()
            })
            .copied()
    }

    /// Left-click hit-testing (decision 7): a navigator row retargets
    /// the branch; a chat-block row pins the program; a `system` header
    /// toggles its branch's fold.
    fn on_click(&mut self, column: u16, row: u16, branches: &[BranchId]) {
        let Some((pane, info)) = self.pane_at(column, row) else {
            return;
        };
        // Row within the bordered pane body (the top border is row 0).
        let body = (row as usize).checked_sub(info.area.y as usize + 1);
        match pane {
            Pane::Navigator => {
                if let Some(idx) = body
                    && let Some(&bid) = branches.get(idx)
                {
                    self.select_branch(bid);
                }
            }
            Pane::Chat => {
                let Some(body) = body else { return };
                let line = info.scroll_top + body;
                let rows =
                    self.chat
                        .rows(self.selected, self.chat_wrap_width(), self.selected_program);
                // `line` is a wrapped-line offset; translate it back to
                // the logical row it belongs to before indexing `rows`.
                let Some(&row_idx) = self.chat_line_rows.get(line) else {
                    return;
                };
                if let Some((kind, text, detail, id)) = rows.get(row_idx) {
                    // "Fork at this point" (D2, `f`) forks from whatever
                    // row is selected — a real logged event, never the
                    // streaming sentinel. Clicking the already-selected
                    // row deselects it (19_UX Step C2), same as any
                    // other toggle in this file.
                    if id.as_u64() != u64::MAX {
                        self.last_clicked_event = if self.last_clicked_event == Some(*id) {
                            None
                        } else {
                            Some(*id)
                        };
                    }
                    if *kind == ChatKind::System {
                        if let Some(branch) = self.selected
                            && !self.collapsed.remove(&branch)
                        {
                            self.collapsed.insert(branch);
                        }
                        return;
                    }
                    match detail {
                        // A plain prose line mentioning "agent N" is
                        // clickable — the orchestrator can say "see the
                        // researcher" and clicking it is being there
                        // (17_BRANCHES Part D). Scoped to prose rather
                        // than the tool-call rows, which already have
                        // their own click behaviour (inspect the call).
                        // `Prose` is here too: it is the same plain
                        // prose row, carrying its completion only so
                        // the pane can tell one response from the next.
                        // Left out of this arm, clicking "agent 3" in a
                        // sentence stopped working the moment prose
                        // learned which program wrote it.
                        RowDetail::None | RowDetail::Prose(_) => {
                            // **Only if it names a branch that exists.**
                            // `agent_reference_in` matches the word
                            // "agent" followed by digits, and prose is
                            // full of those — this project is *called*
                            // agent2, so a sentence mentioning it
                            // yielded `2`, and `select_branch` took the
                            // id on trust. Event #2 is usually the
                            // first user post, so selecting it pointed
                            // the whole UI at something that is not a
                            // branch: the navigator highlighted
                            // nothing and the transcript emptied, which
                            // reads as the branch being deselected.
                            if let Some(n) = agent_reference_in(text)
                                && n != 0
                                && branches.contains(&EventId::new(n))
                            {
                                self.select_branch(EventId::new(n));
                            }
                        }
                        RowDetail::Program(pid) => {
                            self.selected_program = Some(*pid);
                            self.selected_effect = None;
                            self.reset_program_scrolls();
                        }
                        RowDetail::Invoke(pid, idx) => {
                            let want = Effect::Invoke(*idx);
                            let toggle_off = self.selected_program == Some(*pid)
                                && self.selected_effect == Some(want);
                            self.selected_program = Some(*pid);
                            self.selected_effect = if toggle_off { None } else { Some(want) };
                            self.reset_program_scrolls();
                        }
                        RowDetail::Note(pid, note) => {
                            let want = Effect::Append(*note);
                            let toggle_off = self.selected_program == Some(*pid)
                                && self.selected_effect == Some(want);
                            self.selected_program = Some(*pid);
                            self.selected_effect = if toggle_off { None } else { Some(want) };
                            self.reset_program_scrolls();
                        }
                    }
                }
            }
            // Clicking in to keep typing shouldn't cost a draft — only
            // whatever mode or selection was active before (19_UX Step
            // C3, same reasoning `disarm` was built for in C1).
            Pane::Input => {
                self.focus = Focus::Input;
                self.disarm();
            }
            _ => {}
        }
    }

    /// Point both selection axes at `branch`: it becomes the chat focus
    /// and the panes fall back to its most-recent program (decision 1).
    fn select_branch(&mut self, branch: BranchId) {
        self.selected = Some(branch);
        self.selected_program = None;
        self.disarm();
        self.reset_program_scrolls();
        self.navigator_scroll = None;
    }

    /// Back to the neutral state: clears `explicit_mode`, `ask_armed`,
    /// and `last_clicked_event` (the fork-from-here target) together —
    /// everything that would otherwise silently fire against, or
    /// target, something other than what armed it (19_UX Step C1, the
    /// same shape as the `e` bug this whole file started from).
    /// Deliberately does **not** touch `self.input` or `self.focus` — a
    /// typed draft surviving a context change is normal chat-app
    /// behavior; only the *armed* state is the danger.
    fn disarm(&mut self) {
        self.explicit_mode = None;
        self.ask_armed = false;
        self.last_clicked_event = None;
    }

    /// Reset scrolls for the right-hand panes (program-specific content
    /// that should re-anchor when the visible program changes).
    fn reset_program_scrolls(&mut self) {
        self.source_scroll = None;
        self.detail_scroll = None;
        self.console_scroll = None;
        self.disasm_scroll = None;
        self.stack_scroll = None;
        self.promises_scroll = None;
    }

    /// The layout state machine's output: view state in, pane set out.
    pub fn pane_set(&self) -> PaneSet {
        match self.view {
            // The navigator is persistent top-right in every view
            // (decision 7); Step 5 fills the rest of the column from
            // `selected_program`.
            View::Chat => PaneSet {
                chat: true,
                console_left: false,
                right: vec![Pane::Navigator],
            },
            View::Running => {
                let mut right = vec![Pane::Navigator];
                // Off by default now — `1` still brings it back for
                // anyone who wants it. What sits here instead is the
                // detail of whatever is selected.
                if self.show_source {
                    right.push(Pane::Source);
                }
                right.push(Pane::Detail);
                // No separate console pane: `Detail` is showing it
                // until something is selected, and two of them in a
                // two-fifths column left neither enough room.
                if self.show_disasm {
                    right.push(Pane::Disasm);
                }
                if self.show_stack {
                    right.push(Pane::Stack);
                }
                if self.show_promises {
                    right.push(Pane::Promises);
                }
                PaneSet {
                    chat: true,
                    console_left: false,
                    right,
                }
            }
            View::FullDebug => PaneSet {
                chat: false,
                console_left: true,
                right: vec![
                    Pane::Navigator,
                    Pane::Source,
                    Pane::Disasm,
                    Pane::Stack,
                    Pane::Promises,
                ],
            },
        }
    }

    /// `selected_status` is the selected branch's `BranchInfo.status`
    /// (`None` when nothing is selected) — borrowed from the `infos` the
    /// driving loop already computes each tick, not a new query. It
    /// exists so `v`/`x` (19_UX Step F0) can tell whether they'd do
    /// anything before arming/firing.
    pub fn on_key(
        &mut self,
        key: KeyEvent,
        branches: &[BranchId],
        selected_status: Option<&str>,
    ) -> KeyAction {
        // The timeline is a filter you open, not a place you live: while
        // it's open it owns every key, and closes on its own terms.
        if self.timeline {
            return self.on_timeline_key(key.code);
        }
        // Context switching works everywhere.
        if key.code == KeyCode::Tab {
            self.cycle_branch(branches);
            return KeyAction::None;
        }
        // Ctrl-C: the universal "get me out of this", as a ladder from
        // the most local escape to the most final one. A half-typed line
        // is a change of mind mid-message; a running program is the next
        // thing you would want out of; only with neither to escape does
        // it mean leave, and then only on the second press.
        //
        // Quitting used to be the *first* rung whenever the input was
        // empty, which made the commonest gesture for "stop what you are
        // doing" close the session instead — and left interrupt reachable
        // only through `x`, which nobody reaches for by reflex. `x` stays
        // as the explicit, branch-targeted spelling.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            let was_armed = std::mem::replace(&mut self.quit_armed, false);
            if !self.input.is_empty() {
                self.input.clear();
                return KeyAction::None;
            }
            if matches!(selected_status, Some("running" | "thinking" | "suspended")) {
                return KeyAction::Interrupt;
            }
            if was_armed {
                self.quit = true;
            } else {
                self.quit_armed = true;
            }
            return KeyAction::None;
        }
        // Any other key disarms a pending quit — the confirmation is
        // about the very next keystroke, never a lingering state.
        self.quit_armed = false;
        // Ctrl-T: show/hide reasoning content, everywhere — it's a
        // transcript display toggle, not something either focus owns.
        if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_thinking = !self.show_thinking;
            return KeyAction::None;
        }
        match self.view {
            View::FullDebug => self.on_debug_key(key.code, branches, selected_status),
            View::Chat | View::Running => match self.focus {
                Focus::Input => self.on_input_key(key),
                Focus::Debug => self.on_debug_key(key.code, branches, selected_status),
            },
        }
    }

    fn on_timeline_key(&mut self, code: KeyCode) -> KeyAction {
        match code {
            KeyCode::Char('t') | KeyCode::Esc | KeyCode::Char('q') => {
                self.timeline = false;
                KeyAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.timeline_cursor = self.timeline_cursor.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.timeline_cursor += 1;
                KeyAction::None
            }
            KeyCode::Enter => KeyAction::JumpTimeline,
            _ => KeyAction::None,
        }
    }

    fn on_input_key(&mut self, key: KeyEvent) -> KeyAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if !self.input.is_empty() => {
                let text = std::mem::take(&mut self.input).to_string();
                self.history.push(text.clone());
                self.history_cursor = None;
                self.history_draft = None;
                let ask = std::mem::take(&mut self.ask_armed);
                match self.explicit_mode.take() {
                    Some(mode) => KeyAction::SubmitMode(mode, text),
                    // The default is a tell; `ask_armed` (set by the `a`
                    // key, `arm_ask`) is the deliberate exception.
                    None => KeyAction::Submit {
                        text,
                        expects_reply: ask,
                    },
                }
            }
            // The physical keys.
            KeyCode::Backspace => {
                self.input.backspace();
                KeyAction::None
            }
            KeyCode::Delete => {
                self.input.delete_forward();
                KeyAction::None
            }
            KeyCode::Left => {
                self.input.left();
                KeyAction::None
            }
            KeyCode::Right => {
                self.input.right();
                KeyAction::None
            }
            // At the top/bottom row, Up/Down walk message history instead
            // of moving the cursor — there is nowhere for it to go
            // vertically anyway. Mid-buffer, they move the cursor as
            // usual.
            KeyCode::Up => {
                if self.input.cursor().0 == 0 {
                    self.history_up();
                } else {
                    self.input.up();
                }
                KeyAction::None
            }
            KeyCode::Down => {
                if self.input.cursor().0 + 1 == self.input.line_count() {
                    self.history_down();
                } else {
                    self.input.down();
                }
                KeyAction::None
            }
            KeyCode::Home => {
                self.input.home();
                KeyAction::None
            }
            KeyCode::End => {
                self.input.end();
                KeyAction::None
            }
            // The readline/emacs subset (18_TARGETING Step A1's
            // superseded note: Ctrl/Alt+letter is a reliable gesture
            // everywhere modifier+Enter wasn't). One arm per table row;
            // checked before the plain-character arm below, since
            // crossterm reports these as `Char` too, with the modifier
            // riding in `key.modifiers`.
            KeyCode::Char('a') if ctrl => {
                self.input.home();
                KeyAction::None
            }
            KeyCode::Char('e') if ctrl => {
                self.input.end();
                KeyAction::None
            }
            KeyCode::Char('b') if ctrl => {
                self.input.left();
                KeyAction::None
            }
            KeyCode::Char('f') if ctrl => {
                self.input.right();
                KeyAction::None
            }
            KeyCode::Char('p') if ctrl => {
                if self.input.cursor().0 == 0 {
                    self.history_up();
                } else {
                    self.input.up();
                }
                KeyAction::None
            }
            KeyCode::Char('n') if ctrl => {
                if self.input.cursor().0 + 1 == self.input.line_count() {
                    self.history_down();
                } else {
                    self.input.down();
                }
                KeyAction::None
            }
            // Ctrl-D deliberately does not carry readline's "EOF on an
            // empty line" meaning — there is no exit gesture on this
            // key here, only forward-delete, so it can't be hit by
            // accident while editing.
            KeyCode::Char('d') if ctrl => {
                self.input.delete_forward();
                KeyAction::None
            }
            // Some terminals send this in place of `KeyCode::Backspace`
            // for the physical Backspace key — an alias, not a new
            // gesture.
            KeyCode::Char('h') if ctrl => {
                self.input.backspace();
                KeyAction::None
            }
            KeyCode::Char('k') if ctrl => {
                self.input.kill_to_end();
                KeyAction::None
            }
            KeyCode::Char('u') if ctrl => {
                self.input.kill_to_start();
                KeyAction::None
            }
            KeyCode::Char('w') if ctrl => {
                self.input.delete_word_backward();
                KeyAction::None
            }
            KeyCode::Char('o') if ctrl => {
                self.input.insert_newline();
                KeyAction::None
            }
            KeyCode::Char('b') if alt => {
                self.input.word_left();
                KeyAction::None
            }
            KeyCode::Char('f') if alt => {
                self.input.word_right();
                KeyAction::None
            }
            KeyCode::Char('d') if alt => {
                self.input.delete_word_forward();
                KeyAction::None
            }
            KeyCode::Esc => {
                if self.input.is_empty() {
                    self.explicit_mode = None;
                    self.ask_armed = false;
                    self.focus = Focus::Debug;
                } else {
                    self.input.clear();
                }
                KeyAction::None
            }
            KeyCode::Char(c) => {
                self.input.insert_char(c);
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    /// Arm an explicit input mode: focus the input line, ready for the
    /// next Enter to submit as `mode` instead of the default ask/tell.
    fn arm(&mut self, mode: ExplicitMode) -> KeyAction {
        self.explicit_mode = Some(mode);
        self.focus = Focus::Input;
        KeyAction::None
    }

    /// Arm the ask gesture: focus the input line, ready for the next
    /// Enter to submit as an ask (`expects_reply: true`) instead of the
    /// default tell. See `ask_armed`'s doc for why this is a dedicated
    /// key rather than a modifier on Enter.
    fn arm_ask(&mut self) -> KeyAction {
        self.ask_armed = true;
        self.focus = Focus::Input;
        KeyAction::None
    }

    /// Arm the rewrite gesture: leave `FullDebug` for whichever view
    /// was live before it — matching what `Esc` already does leaving
    /// `FullDebug` — since `FullDebug` has no chat/input pane at all to
    /// type the rewrite into. Focuses the input line and returns
    /// `KeyAction::ArmRewrite` so the driving loop can prefill it with
    /// the current program's source, which needs `Session` and so
    /// can't happen here.
    fn arm_rewrite(&mut self) -> KeyAction {
        self.view = self.prev_view;
        self.explicit_mode = Some(ExplicitMode::Rewrite);
        self.focus = Focus::Input;
        KeyAction::ArmRewrite
    }

    fn on_debug_key(
        &mut self,
        code: KeyCode,
        branches: &[BranchId],
        selected_status: Option<&str>,
    ) -> KeyAction {
        match code {
            KeyCode::Char('q') => {
                self.quit = true;
                KeyAction::None
            }
            KeyCode::Char('d') => {
                if self.view == View::FullDebug {
                    self.view = self.prev_view;
                    self.focus = Focus::Input;
                } else {
                    self.prev_view = self.view;
                    self.view = View::FullDebug;
                }
                KeyAction::None
            }
            KeyCode::Esc | KeyCode::Char('i') if self.view != View::FullDebug => {
                self.focus = Focus::Input;
                KeyAction::None
            }
            KeyCode::Esc if self.view == View::FullDebug => {
                self.view = self.prev_view;
                self.focus = Focus::Input;
                KeyAction::None
            }
            // The collapse key, both ways: `Running`'s source/console
            // panes fold back to full-width chat, and — since this was
            // otherwise a one-way door, reversible only by a fresh
            // program re-triggering the auto-pop in `apply` — pressing it
            // again from `Chat` reopens them.
            KeyCode::Char('c') if self.view == View::Running => {
                self.view = View::Chat;
                self.focus = Focus::Input;
                KeyAction::None
            }
            KeyCode::Char('c') if self.view == View::Chat => {
                self.view = View::Running;
                self.focus = Focus::Input;
                KeyAction::None
            }
            KeyCode::Char(' ') => KeyAction::TogglePause,
            KeyCode::Char('s') => KeyAction::StepInstr,
            KeyCode::Char('n') => KeyAction::StepLine,
            // The dancing gestures (D2) — everywhere but FullDebug, which
            // keeps its own single-purpose letters (space/s/n/1-9) for
            // real instruction stepping.
            // A selected message forks from it; none selected forks
            // from the branch's leaf (19_UX Step C2 — one key, not a
            // Shift-cased pair only one half of which read the click).
            KeyCode::Char('f') if self.view != View::FullDebug => match self.last_clicked_event {
                Some(id) => KeyAction::ForkAt(id),
                None => KeyAction::Fork,
            },
            // Only three statuses are actually interruptible — `Idle`
            // (`Runner::interrupt`, `machine.rs`) and `dormant` (no live
            // `Runner`, `cmd_interrupt` short-circuits) are both inert,
            // spelled out rather than `!= "idle"` since `dormant` would
            // otherwise wrongly count as live (19_UX Step F2).
            KeyCode::Char('x')
                if self.view != View::FullDebug
                    && matches!(selected_status, Some("running" | "thinking" | "suspended")) =>
            {
                KeyAction::Interrupt
            }
            KeyCode::Char('w') if self.view != View::FullDebug => KeyAction::JumpToWaiting,
            KeyCode::Char('t') if self.view != View::FullDebug => {
                self.timeline = true;
                self.timeline_cursor = 0;
                KeyAction::None
            }
            // Raw view: what the model actually emitted, unclassified
            // and unstyled — for settling exactly the kind of "did I
            // really fence/indent that?" dispute the model itself can't
            // reliably answer (it never sees its own rendered output).
            KeyCode::Char('m') if self.view != View::FullDebug => {
                self.show_markdown = !self.show_markdown;
                KeyAction::None
            }
            KeyCode::Char('a') if self.view != View::FullDebug => self.arm_ask(),
            KeyCode::Char('r') if self.view != View::FullDebug => self.arm(ExplicitMode::Rename),
            // Only a `Phase::Suspended` branch actually has a `resume`
            // bound to hand a value to — everywhere else `resume` is just
            // an unbound name, so the synthesized program would trap
            // (22_ONE_VOCABULARY: "`resume` is unbound → a trap → a
            // handler, like any other error") rather than doing anything
            // useful. Gating the gesture here avoids that wasted round
            // trip, not a harmless no-op like `w` on an empty wait-list
            // (19_UX Step F1).
            KeyCode::Char('v')
                if self.view != View::FullDebug && selected_status == Some("suspended") =>
            {
                self.arm(ExplicitMode::ResumeWithValue)
            }
            KeyCode::Char('p') if self.view != View::FullDebug => {
                self.arm(ExplicitMode::SpawnCharter)
            }
            // Rewrite is a debugging/recovery gesture — replace a
            // suspended or crashed program by hand — not something
            // that belongs beside ordinary chat, so it lives only in
            // `FullDebug` (19_UX Part B), on `r` where `Chat`/
            // `Running`'s Rename sits — the two never overlap, since
            // this arm is only reachable when the other isn't.
            KeyCode::Char('r') if self.view == View::FullDebug => self.arm_rewrite(),
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as u8 - b'1') as usize;
                if self.view == View::FullDebug {
                    // 1–9 switch which branch the panes borrow.
                    if let Some(id) = branches.get(idx) {
                        self.select_branch(*id);
                    }
                } else if self.view == View::Running {
                    // 1–4 override the auto-pop set.
                    match c {
                        '1' => self.show_source = !self.show_source,
                        '2' => self.show_disasm = !self.show_disasm,
                        '3' => self.show_stack = !self.show_stack,
                        '4' => self.show_promises = !self.show_promises,
                        _ => {}
                    }
                }
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn cycle_branch(&mut self, branches: &[BranchId]) {
        // With nothing else to cycle *to*, this would still call
        // `select_branch` on the branch already selected — which resets
        // the program/subitem selection and every pane's scroll
        // position. Not a pure no-op like an empty list, so it needs
        // its own guard rather than falling out of the modulo below.
        if branches.len() <= 1 {
            return;
        }
        let next = match self
            .selected
            .and_then(|s| branches.iter().position(|f| *f == s))
        {
            Some(i) => (i + 1) % branches.len(),
            None => 0,
        };
        self.select_branch(branches[next]);
    }
}

/// The first `agent N` mention in `text` (case-insensitive on "agent"),
/// as the id it names — what makes chat prose referencing a branch
/// clickable (17_BRANCHES Part D: "the orchestrator can say 'see the
/// researcher' and you are there"). An agent's own id is also its first
/// branch's id, so this needs no lookup — the reference *is* the
/// address.
fn agent_reference_in(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let mut search = lower.as_str();
    while let Some(at) = search.find("agent") {
        let rest = &search[at + "agent".len()..];
        let digits_start = rest.find(|c: char| !c.is_whitespace() && c != '#');
        if let Some(ds) = digits_start {
            let digits: String = rest[ds..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
        search = &search[at + "agent".len()..];
    }
    None
}

/// The input line's default Enter, resolved against live status: a
/// `Reply` when `branch` has a pending ask-to-user, else a `UserTurn`
/// carrying the ask/tell modifier (17_BRANCHES: "Reply when the branch
/// has a pending ask to you"). Pure — takes the navigator's own
/// `branch_infos` snapshot rather than a session, so it is directly
/// testable.
/// **A name from the first words of what was said.** A branch nobody
/// named reads as `branch #42` in the navigator, which is an address
/// rather than a reminder. The message that opened it is the best
/// short description anyone has, and it costs nothing to take.
///
/// Cut on a word boundary where there is one, so a name ends in a word
/// rather than mid-syllable, and cleaned of the whitespace a pasted
/// message carries.
pub fn name_from_message(text: &str) -> Option<String> {
    const MAX: usize = 32;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    if flat.len() <= MAX {
        return Some(flat);
    }
    let cut = flat[..MAX].rfind(' ').unwrap_or(MAX);
    Some(format!("{}…", flat[..cut].trim_end()))
}

/// `/fork <message>` and `/spawn <message>` — hand work out in the one
/// gesture that makes the helper and tells it what it is for.
///
/// **Why they exist even though the model can do this itself.** It
/// often does not: on a thread worth protecting, asked something whose
/// findings it would have to hold, it hands the work out in 0 of 20
/// samples without a worked example to imitate
/// (`docs/evidence/`). The person watching the thread
/// fill up should not have to negotiate about it.
///
/// `/fork` is for work that needs what this thread already knows;
/// `/spawn` for work that needs none of it. Anything else is a message.
fn slash_command(branch: BranchId, leaf: Option<EventId>, text: &str) -> Option<SessionCommand> {
    let (verb, rest) = text.split_once(char::is_whitespace)?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let name = name_from_message(rest);
    match verb {
        "/fork" => Some(SessionCommand::Fork {
            from: leaf?,
            name,
            text: Some(rest.to_owned()),
        }),
        "/spawn" => Some(SessionCommand::Spawn {
            parent: branch,
            name,
            charter: rest.to_owned(),
            text: None,
        }),
        _ => None,
    }
}

fn resolve_submit(
    infos: &[BranchInfo],
    branch: BranchId,
    text: String,
    expects_reply: bool,
) -> SessionCommand {
    // **A slash command is not an answer.** Checked before the
    // asking-user branch below: a person typing `/fork …` while a
    // question is open means to hand that work out, not to answer with
    // the literal text "/fork …".
    let leaf = infos.iter().find(|b| b.branch == branch).map(|b| b.leaf);
    if let Some(cmd) = slash_command(branch, leaf, &text) {
        return cmd;
    }
    let asking = infos
        .iter()
        .find(|b| b.branch == branch)
        .and_then(|b| b.asking_user);
    match asking {
        Some(call) => SessionCommand::Reply {
            branch,
            call,
            value: serde_json::Value::String(text),
        },
        None => SessionCommand::UserTurn {
            branch,
            text,
            expects_reply,
        },
    }
}

/// One explicit input mode's submission, resolved into the command it
/// stands for (17_BRANCHES Part D's rename/restart/spawn keys). Pure —
/// no session needed, so it is directly testable.
fn resolve_submit_mode(mode: ExplicitMode, branch: BranchId, text: String) -> SessionCommand {
    match mode {
        ExplicitMode::Rename => SessionCommand::Rename { branch, name: text },
        // `v` keeps the "type a JSON value" UX, but synthesizes the
        // program that actually runs it, per 22_ONE_VOCABULARY's
        // `UserCall` collapse: what's shown in the pane is exactly what
        // ran, rather than a value tucked inside a tool-call struct.
        ExplicitMode::ResumeWithValue => {
            let value: serde_json::Value =
                serde_json::from_str(&text).unwrap_or_else(|_| serde_json::Value::String(text));
            SessionCommand::Restart {
                branch,
                source: format!("history.note(resume({value}));"),
            }
        }
        // `e` is unchanged in spirit: the pasted text *is* the program.
        ExplicitMode::Rewrite => SessionCommand::Restart {
            branch,
            source: text,
        },
        ExplicitMode::SpawnCharter => SessionCommand::Spawn {
            parent: branch,
            name: None,
            charter: text,
            text: None,
        },
    }
}

/// What `KeyAction::ArmRewrite` prefills the input buffer with: the
/// selected branch's current program source if one has ever run,
/// empty otherwise — today's blank-line start. Pure given `app` and
/// `session`, so it is directly testable the same way
/// `resolve_submit`/`resolve_submit_mode` are.
fn resolve_rewrite_prefill(app: &AttachedApp, session: &Session) -> InputBuffer {
    let (_, pv) = resolve_program(app, session);
    match pv {
        Some(pv) => InputBuffer::prefilled(&pv.source),
        None => InputBuffer::new(),
    }
}

/// The next branch waiting on you, cyclically after `current` — what `w`
/// jumps to. `ordered` is the navigator's own row order, so repeated
/// presses walk the tree the same way the eye does.
fn next_waiting(ordered: &[BranchInfo], current: BranchId) -> Option<BranchId> {
    let start = ordered
        .iter()
        .position(|b| b.branch == current)
        .unwrap_or(0);
    let n = ordered.len();
    (1..=n)
        .map(|offset| &ordered[(start + offset) % n])
        .find(|b| b.asking_user.is_some())
        .map(|b| b.branch)
}

/// Every post of yours across the whole tree, oldest first — the
/// timeline (17_BRANCHES Part D): "a filter you can open, not a place
/// you live." One row per `Post { from: User }`, whichever branch it
/// landed on.
fn timeline_rows(session: &Session) -> Vec<(EventId, BranchId, String)> {
    let tree = session.tree();
    let mut rows: Vec<(EventId, BranchId, String)> = tree
        .events
        .values()
        .filter_map(|e| {
            let EventPayload::Post {
                from: crate::types::Author::User,
                origin,
            } = &e.payload
            else {
                return None;
            };
            let branch = tree.branch_of(e.id)?;
            Some((
                e.id,
                branch,
                crate::report::render_post(e.id, crate::types::Author::User, origin),
            ))
        })
        .collect();
    rows.sort_by_key(|(id, ..)| id.as_u64());
    rows
}

/// The current spine leaf for `branch` — from the live state if
/// available (resume-friendly session), or the leaf the tree projection
/// records for it (log-only, decision 8; a dormant branch has no
/// `Runner`, C1's `dormant`).
fn find_leaf(session: &Session, branch: BranchId) -> Option<EventId> {
    if let Some(state) = session.state(branch) {
        return Some(state.spine.leaf_id);
    }
    session
        .tree()
        .branches()
        .into_iter()
        .find_map(|(root, leaf)| (root == branch).then_some(leaf))
}

/// If `program` is the current (or most-recently-completed) program on
/// `branch`, returns the VM for rich introspection — otherwise `None` (it
/// is an older program rendered from the log projection, Step 5).
fn vm_for_program(session: &Session, branch: BranchId, program: EventId) -> Option<&interp::VM> {
    let state = session.state(branch)?;
    let vm = state.vm()?;
    let leaf = state.spine.leaf_id;
    let agent = session.tree().enclosing_agent(branch)?;
    let programs = session.tree().programs_for(agent, leaf);
    if programs.last().map(|p| p.id) == Some(program) {
        Some(vm)
    } else {
        None
    }
}

/// Resolve the selected program for `app` into its VM (for the running/
/// just-completed program) and its `ProgramView` (for old programs).
/// `vm` is `Some` only for the current program; `pv` is `Some` for any
/// program (current or old) that exists in the log projection.
fn resolve_program<'a>(
    app: &AttachedApp,
    session: &'a Session,
) -> (Option<&'a interp::VM>, Option<ProgramView>) {
    let Some(branch) = app.selected else {
        return (None, None);
    };
    let Some(leaf) = find_leaf(session, branch) else {
        return (None, None);
    };
    let Some(agent) = session.tree().enclosing_agent(branch) else {
        return (None, None);
    };
    let programs = session.tree().programs_for(agent, leaf);
    let effective = app
        .selected_program
        .or_else(|| programs.last().map(|p| p.id));
    let pv = effective.and_then(|id| programs.into_iter().find(|p| p.id == id));
    let vm = effective.and_then(|prog_id| vm_for_program(session, branch, prog_id));
    (vm, pv)
}

/// Run the attached TUI over `session`. The session loop *is* this
/// thread: we pump the inbox, drain `SessionEvent`s into the app, and
/// render — input arrives through the inbox from a reader thread.
pub fn run_attached(mut session: Session, events_rx: Receiver<SessionEvent>) -> Result<(), String> {
    let handle = session.handle();
    {
        let input_handle = handle.clone();
        thread::spawn(move || {
            while let Ok(event) = ratatui::crossterm::event::read() {
                if !input_handle.send_input(event) {
                    return;
                }
            }
        });
    }

    let mut app = AttachedApp::new(session.conversation_branch());
    let mut terminal = ratatui::init();
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    )
    .map_err(|e| e.to_string())?;
    let result = loop {
        let mut inputs = Vec::new();
        session.pump_until(Instant::now() + super::REDRAW_EVERY, &mut inputs);
        for event in events_rx.try_iter() {
            app.apply(&event);
        }
        app.auto_reset_chat_scroll();
        // Every branch, nested exactly as the log nests them (Part D):
        // the navigator's row order, and what Tab/1–9/clicks index into.
        // From `branch_infos` (identity + shape from the log,
        // status/thinking from live session state) so it survives resume
        // (decision 8), not just the live `states`. Kept around this tick
        // for the input line's ask-vs-reply decision and the `w` jump.
        let infos = session.branch_infos();
        // **The spinner's clock starts when the work does, not when the
        // pane notices.** Kept across ticks while the word is unchanged
        // so the elapsed count is the age of *this* wait; cleared the
        // moment the branch is anything else, so it never outlives what
        // it describes. Keyed on the selected branch, which is the one
        // the pane is showing.
        let busy_now = app
            .selected
            .and_then(|b| infos.iter().find(|i| i.branch == b))
            .map(|i| i.status.clone())
            .filter(|s| matches!(s.as_str(), "thinking" | "running" | "queued"));
        app.busy = match (busy_now, app.busy.take()) {
            (Some(now), Some((was, since))) if was == now => Some((was, since)),
            (Some(now), _) => Some((now, Instant::now())),
            (None, _) => None,
        };
        let ordered = ordered_branches(infos.clone());
        let branches: Vec<BranchId> = ordered.iter().map(|b| b.branch).collect();
        // Render *before* resolving this tick's inputs, not after: a click
        // is hit-tested against `pane_rects`/`chat_line_rows`, which this
        // call is what refreshes. Rendering at the end of the loop (the
        // old order) meant a click was always resolved against the
        // *previous* tick's layout — stale by exactly the events just
        // applied above. Usually harmless (nothing shifted), but a new
        // chat row landing in that same window shifts every wrapped-line
        // offset after it, so a click on what just became the newest row
        // was the single likeliest way to hit this: the fresh content is
        // in `chat.rows()` already (applied above) but not yet in the
        // layout a click is about to be resolved against. Moving render
        // here costs one tick (~30ms) of visual lag on a click's own
        // on-screen effect — imperceptible against this loop's own cadence.
        if let Err(e) = terminal.draw(|frame| render(frame, &mut app, &session)) {
            break Err(e.to_string());
        }
        for input in inputs {
            match input {
                CtEvent::Key(key) if key.is_press() => {
                    let selected_status = app
                        .selected
                        .and_then(|s| infos.iter().find(|i| i.branch == s))
                        .map(|i| i.status.as_str());
                    let action = app.on_key(key, &branches, selected_status);
                    let Some(selected) = app.selected else {
                        continue;
                    };
                    match action {
                        KeyAction::None => {}
                        // The input line always sends to the selected
                        // branch: the user speaks *inside* branches.
                        // Reply, not UserTurn, when this branch is
                        // waiting on an answer from you.
                        KeyAction::Submit {
                            text,
                            expects_reply,
                        } => {
                            handle.send(resolve_submit(&infos, selected, text, expects_reply));
                            app.reset_scrolls();
                        }
                        KeyAction::SubmitMode(mode, text) => {
                            handle.send(resolve_submit_mode(mode, selected, text));
                            app.reset_scrolls();
                        }
                        KeyAction::TogglePause => {
                            let paused = session.is_paused(selected);
                            session.set_paused(selected, !paused);
                            app.reset_scrolls();
                        }
                        KeyAction::StepInstr => {
                            session.set_paused(selected, true);
                            session.step_paused(selected, 1);
                            app.reset_scrolls();
                        }
                        KeyAction::StepLine => {
                            step_line(&mut session, selected);
                            app.reset_scrolls();
                        }
                        // "Ask a running agent something without pausing
                        // it" is fork-at-current-leaf; no separate
                        // gesture (17_BRANCHES).
                        KeyAction::Fork => {
                            if let Some(at) = find_leaf(&session, selected) {
                                handle.send(SessionCommand::Fork {
                                    from: at,
                                    name: None,
                                    text: None,
                                });
                            }
                        }
                        KeyAction::ForkAt(at) => {
                            handle.send(SessionCommand::Fork {
                                from: at,
                                name: None,
                                text: None,
                            });
                        }
                        KeyAction::Interrupt => {
                            handle.send(SessionCommand::Interrupt { branch: selected });
                        }
                        KeyAction::JumpToWaiting => {
                            if let Some(next) = next_waiting(&ordered, selected) {
                                app.select_branch(next);
                            }
                        }
                        KeyAction::JumpTimeline => {
                            let rows = timeline_rows(&session);
                            if !rows.is_empty() {
                                let idx = app.timeline_cursor.min(rows.len() - 1);
                                app.select_branch(rows[idx].1);
                            }
                            app.timeline = false;
                        }
                        // View/focus/explicit_mode are already set
                        // (arm_rewrite) — this only needs Session,
                        // which on_key doesn't have: the current
                        // program's source, if there is one.
                        KeyAction::ArmRewrite => {
                            app.input = resolve_rewrite_prefill(&app, &session);
                        }
                    }
                }
                CtEvent::Mouse(mouse) => {
                    app.on_mouse(mouse.column, mouse.row, mouse.kind, &branches)
                }
                _ => {}
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    )
    .map_err(|e| e.to_string())?;
    ratatui::restore();
    result
}

/// Step the selected branch's VM until its source line changes (or the
/// program yields/finishes, or the cap is hit).
fn step_line(session: &mut Session, branch: BranchId) {
    session.set_paused(branch, true);
    let line_of = |session: &Session| {
        session
            .state(branch)
            .and_then(|s| s.vm())
            .and_then(super::panes::current_line)
    };
    let start = line_of(session);
    for _ in 0..LINE_STEP_CAP {
        session.step_paused(branch, 1);
        let state = session.state(branch);
        if !state.map(|s| s.status() == "running").unwrap_or(false) {
            return; // blocked on the host, suspended, or finished
        }
        if start.is_none() || line_of(session) != start {
            return;
        }
    }
}

// ── rendering ───────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &mut AttachedApp, session: &Session) {
    app.pane_rects.clear();

    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    if app.timeline {
        render_timeline(frame, app, session, main);
        frame.render_widget(
            Paragraph::new(" j/k move · enter jump to branch · t/esc close ")
                .style(Style::default().add_modifier(Modifier::REVERSED)),
            footer,
        );
        return;
    }

    let panes = app.pane_set();
    let (left, right) = if panes.right.is_empty() {
        (main, None)
    } else {
        // Two fifths. The transcript is what is being read; the right
        // column is what is being referred to.
        let [l, r] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .areas(main);
        (l, Some(r))
    };

    if panes.chat {
        // The question sits above the input, and the input line switches
        // to reply mode, exactly when this branch is waiting on you
        // (17_BRANCHES Part D).
        let asking_text = app.selected.and_then(|b| asking_question_text(session, b));
        let (top, transcript_area, input_area, row_at_line) =
            render_chat(frame, app, left, app.chat_scroll, asking_text.as_deref());
        app.chat_line_rows = row_at_line;
        app.pane_rects.push((
            Pane::Chat,
            PaneInfo {
                area: transcript_area,
                scroll_top: top,
            },
        ));
        app.pane_rects.push((
            Pane::Input,
            PaneInfo {
                area: input_area,
                scroll_top: 0,
            },
        ));
    } else if panes.console_left {
        let (top, area) = render_attached_console(frame, app, session, left, app.console_scroll);
        app.pane_rects.push((
            Pane::Console,
            PaneInfo {
                area,
                scroll_top: top,
            },
        ));
    }

    if let Some(right) = right {
        let (vm, pv) = resolve_program(app, session);
        // Old programs (no live VM, from the log) strip the VM-only
        // panes — there is no VM to disassemble or walk. What is left
        // is the navigator and the detail of whatever is selected;
        // this used to force the *source* pane in here, which is how
        // it kept coming back after being taken out of the auto-pop
        // set: a completed program has no live VM, so every block you
        // finished reading re-opened it.
        let mut right_panes = panes.right.clone();
        if vm.is_none() && pv.is_some() {
            right_panes.retain(|p| matches!(p, Pane::Navigator | Pane::Source | Pane::Detail));
            if !right_panes.contains(&Pane::Source) && !right_panes.contains(&Pane::Detail) {
                right_panes.insert(1, Pane::Detail);
            }
        }
        // Uncapped, a deep fork/spawn tree would grow the navigator to
        // consume the whole right column, squeezing everything else off
        // (19_UX Step E0) — capped at a third of it instead, same
        // fraction-of-the-area shape as the input box's own height cap
        // (`INPUT_MAX_HEIGHT_FRACTION`).
        let navigator_cap = (right.height / NAVIGATOR_HEIGHT_FRACTION).max(3);
        let slots = Layout::vertical(right_panes.iter().map(|p| match p {
            Pane::Navigator => {
                Constraint::Length((session.tree().branches().len() as u16 + 2).min(navigator_cap))
            }
            _ => Constraint::Fill(1),
        }))
        .split(right);
        for (pane, slot) in right_panes.iter().zip(slots.iter()) {
            match pane {
                Pane::Navigator => {
                    let top = render_navigator(frame, app, session, *slot);
                    app.pane_rects.push((
                        Pane::Navigator,
                        PaneInfo {
                            area: *slot,
                            scroll_top: top,
                        },
                    ));
                }
                Pane::Detail => {
                    // **Console until you pick something.** The source
                    // is already in the transcript — a block shows its
                    // own cells — so what belongs beside it is the
                    // program's output, and then the detail of whatever
                    // you click instead.
                    let (top, area) = if app.selected_effect.is_some() {
                        (
                            render_detail(frame, app, session, pv.as_ref(), *slot),
                            *slot,
                        )
                    } else if let Some(ref pv) = pv {
                        render_console_from_pv(frame, pv, *slot, app.detail_scroll)
                    } else {
                        render_attached_console(frame, app, session, *slot, app.detail_scroll)
                    };
                    app.pane_rects.push((
                        Pane::Detail,
                        PaneInfo {
                            area,
                            scroll_top: top,
                        },
                    ));
                }
                Pane::Console => {
                    let (top, area) = if let Some(ref pv) = pv {
                        render_console_from_pv(frame, pv, *slot, app.console_scroll)
                    } else {
                        render_attached_console(frame, app, session, *slot, app.console_scroll)
                    };
                    app.pane_rects.push((
                        Pane::Console,
                        PaneInfo {
                            area,
                            scroll_top: top,
                        },
                    ));
                }
                Pane::Source => {
                    // Outside FullDebug, "how far along" is the question
                    // — the same annotated source the model's own report
                    // renders (`report::annotate_program`, over
                    // `programs_for`, never the VM: 17_BRANCHES Part D —
                    // a pane the model also sees must derive it the same
                    // way the model's copy is derived), so it agrees with
                    // the report even while the program is still running.
                    // FullDebug keeps the raw IP/line-highlighted view —
                    // real instruction stepping wants the VM, not a call
                    // menu.
                    let top = if app.view != View::FullDebug
                        && let Some(ref pv) = pv
                    {
                        ui::render_source_str(
                            frame,
                            &crate::report::annotate_program(pv),
                            *slot,
                            app.source_scroll,
                        )
                    } else if let Some(vm) = vm {
                        ui::render_source(frame, vm, *slot, app.source_scroll)
                    } else if let Some(ref pv) = pv {
                        ui::render_source_str(frame, &pv.source, *slot, app.source_scroll)
                    } else {
                        render_placeholder(frame, Pane::Source, *slot);
                        0
                    };
                    app.pane_rects.push((
                        Pane::Source,
                        PaneInfo {
                            area: *slot,
                            scroll_top: top,
                        },
                    ));
                }
                Pane::Disasm => {
                    if let Some(vm) = vm {
                        let top = ui::render_disasm(frame, vm, *slot, app.disasm_scroll);
                        app.pane_rects.push((
                            Pane::Disasm,
                            PaneInfo {
                                area: *slot,
                                scroll_top: top,
                            },
                        ));
                    } else {
                        render_placeholder(frame, Pane::Disasm, *slot);
                    }
                }
                Pane::Stack => {
                    if let Some(vm) = vm {
                        let top = ui::render_stack(frame, vm, *slot, app.stack_scroll);
                        app.pane_rects.push((
                            Pane::Stack,
                            PaneInfo {
                                area: *slot,
                                scroll_top: top,
                            },
                        ));
                    } else {
                        render_placeholder(frame, Pane::Stack, *slot);
                    }
                }
                Pane::Promises => {
                    if let Some(vm) = vm {
                        let top = ui::render_promises(frame, vm, None, *slot, app.promises_scroll);
                        app.pane_rects.push((
                            Pane::Promises,
                            PaneInfo {
                                area: *slot,
                                scroll_top: top,
                            },
                        ));
                    } else {
                        render_placeholder(frame, Pane::Promises, *slot);
                    }
                }
                Pane::Chat => render_placeholder(frame, Pane::Chat, *slot),
                // Never a member of `panes.right` — the input box is
                // registered separately by `render_chat`'s own caller.
                Pane::Input => unreachable!("Pane::Input is not a debug pane slot"),
            }
        }
    }

    // `w waiting` only means something when a branch actually owes you a
    // reply — otherwise it's a no-op key with a hint that just adds
    // noise, so it's only advertised while it would do something.
    let infos = session.branch_infos();
    let waiting = branch_counts(&infos).0 > 0;
    // `v resume` only means something on a suspended branch (19_UX Step
    // F1) — same "only advertised while it would do something" shape.
    let resumable = app
        .selected
        .and_then(|s| infos.iter().find(|i| i.branch == s))
        .is_some_and(|i| i.status == "suspended");
    // `x interrupt` only means something on a live, non-idle branch
    // (19_UX Step F2) — same shape again.
    let interruptible = app
        .selected
        .and_then(|s| infos.iter().find(|i| i.branch == s))
        .is_some_and(|i| matches!(i.status.as_str(), "running" | "thinking" | "suspended"));
    // `tab`/`1-9` cycle or jump between branches — with only one, that
    // targets the branch already selected, and `cycle_branch` guards
    // against the real cost of that (it would otherwise silently reset
    // the program/subitem selection and every pane's scroll position).
    // Advertised the same way `waiting` is: only when it would move.
    let multi_branch = session.tree().branches().len() > 1;
    let help = if app.quit_armed {
        // The armed quit replaces the hint line outright rather than
        // appending to it: it is a question waiting on the very next
        // keystroke, and a reader who has to find it among twelve other
        // bindings has not been told anything.
        " ctrl-c again to quit ".to_owned()
    } else {
        footer_hint(
            app.view,
            app.focus,
            app.ask_armed,
            waiting,
            resumable,
            interruptible,
            multi_branch,
        )
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().add_modifier(Modifier::REVERSED)),
        footer,
    );
}

/// The footer's key-hint line: a shortcut is only advertised while it
/// would actually do something (`waiting`/`resumable`/`multi_branch`
/// each gate their own `·` clause the same way, 19_UX Steps A.. /F1).
/// Pure and ratatui-free so it can be tested directly, same reasoning
/// as `wrap_input`.
fn footer_hint(
    view: View,
    focus: Focus,
    ask_armed: bool,
    waiting: bool,
    resumable: bool,
    interruptible: bool,
    multi_branch: bool,
) -> String {
    match (view, focus) {
        (View::FullDebug, _) => format!(
            " d/esc chat · r rewrite{} · space run/pause · s step · n step line · q quit ",
            if multi_branch {
                " · tab/1-9 agent"
            } else {
                ""
            }
        ),
        (_, Focus::Input) if ask_armed => format!(
            " type to ask · enter send · esc clear/cancel{} ",
            if multi_branch { " · tab agent" } else { "" }
        ),
        (_, Focus::Input) => format!(
            " type to chat · enter send{} · esc debug keys ",
            if multi_branch { " · tab agent" } else { "" }
        ),
        (View::Running, Focus::Debug) => format!(
            " esc/i type · c collapse · d debugger · 1-4 panes · f fork · p spawn · a ask \
             · r rename{}{}{} · t timeline · m markdown · ctrl-t thinking{} · q quit ",
            if interruptible {
                " · ctrl-c/x interrupt"
            } else {
                ""
            },
            if resumable { " · v resume" } else { "" },
            if waiting { " · w waiting" } else { "" },
            if multi_branch { " · tab agent" } else { "" }
        ),
        (_, Focus::Debug) => format!(
            " esc/i type · c expand · d debugger · f fork · p spawn · a ask \
             · r rename{}{}{} · t timeline · m markdown · ctrl-t thinking{} · q quit ",
            if interruptible {
                " · ctrl-c/x interrupt"
            } else {
                ""
            },
            if resumable { " · v resume" } else { "" },
            if waiting { " · w waiting" } else { "" },
            if multi_branch { " · tab agent" } else { "" }
        ),
    }
}

/// One line of JavaScript as colored spans, over `base`.
///
/// Reuses `debug/highlight.rs` — already a hand-rolled JS highlighter, and
/// already what `ui.rs` drives for the source pane — so a cell and the
/// source pane cannot drift in how they color the same token.
fn js_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut pos = 0usize;
    for tok in super::highlight::tokenize(text) {
        if tok.start > pos {
            out.push(Span::styled(text[pos..tok.start].to_string(), base));
        }
        let style = match token_color(tok.kind) {
            Some(color) => base.fg(color),
            None => base,
        };
        out.push(Span::styled(text[tok.start..tok.end].to_string(), style));
        pos = tok.end;
    }
    if pos < text.len() {
        out.push(Span::styled(text[pos..].to_string(), base));
    }
    if out.is_empty() {
        out.push(Span::styled(text.to_string(), base));
    }
    out
}

/// The transcript's colors for a highlighted token. Deliberately muted
/// against the source pane's: a cell sits inside a conversation, and the
/// prose around it must stay the thing the eye lands on first.
fn token_color(kind: super::highlight::Kind) -> Option<Color> {
    use super::highlight::Kind;
    match kind {
        Kind::Keyword => Some(Color::Rgb(197, 134, 192)),
        Kind::Str => Some(Color::Rgb(206, 145, 120)),
        Kind::Number => Some(Color::Rgb(181, 206, 168)),
        Kind::Comment => Some(Color::Rgb(106, 153, 85)),
        Kind::Ident | Kind::Punct => None,
    }
}

/// **A column of air down each side of the chat pane**, between the
/// border and anything a row paints. The left one doubles as the
/// selection gutter, which is why it is reserved rather than claimed
/// when needed: a mark that has to be made room for moves the text it
/// is marking.
const CHAT_MARGIN: u16 = 1;

/// **The three backgrounds a program block is made of.** A block is one
/// shape on the screen: a lid saying what it is and how it went, the
/// source under it, and the calls it made attached below. They share a
/// family so the eye reads them as one panel, and differ enough to say
/// which part is which.
const SLAB_HEADER: Color = Color::Rgb(54, 54, 54);
const SLAB_CODE: Color = Color::Rgb(40, 40, 40);
const SLAB_ATTACHED: Color = Color::Rgb(30, 32, 36);

/// The background a row belongs to, or `None` for one that sits on the
/// pane's own ground.
fn slab_of(kind: ChatKind, detail: &RowDetail) -> Option<Color> {
    match (kind, detail) {
        (ChatKind::Program, RowDetail::Program(_)) => Some(SLAB_HEADER),
        // Any code, a block's own source or a fenced quote in prose:
        // both are code and both read as one slab. Only the *lid* is
        // particular to a block.
        (ChatKind::Code, _) => Some(SLAB_CODE),
        // The calls a block made and the values it appended are the
        // same thing to a reader — effects, attached below the source —
        // so they share the attached ground. An append that sat on the
        // pane's own background read as a stray line beside the block
        // rather than part of it.
        (_, RowDetail::Invoke(..) | RowDetail::Note(..)) => Some(SLAB_ATTACHED),
        _ => None,
    }
}

/// Which voice a row is in — what the blank line between turns is
/// counted against. A program block and the prose around it are the
/// same voice: the agent's.
fn speaker_of(kind: ChatKind) -> u8 {
    match kind {
        ChatKind::User => 1,
        ChatKind::System | ChatKind::Marker | ChatKind::Error => 2,
        _ => 0,
    }
}

/// **One colour per kind, not two.** Rows used to alternate brightness
/// by parity, which was the only grouping signal the pane had. It is
/// not any more — turns are separated by whitespace and a program's
/// parts share a background — and two signals for "these lines belong
/// together" disagree more often than they agree.
fn chat_style(kind: ChatKind) -> Style {
    match kind {
        ChatKind::System => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::DIM),
        ChatKind::User => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ChatKind::Assistant => Style::default().fg(Color::Rgb(220, 220, 220)),
        ChatKind::Heading => Style::default()
            .fg(Color::Rgb(220, 220, 220))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ChatKind::Blockquote => Style::default()
            .fg(Color::Rgb(130, 130, 130))
            .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ChatKind::Code => Style::default().fg(Color::Rgb(205, 205, 205)),
        ChatKind::TableHeader => Style::default()
            .fg(Color::Rgb(220, 220, 220))
            .add_modifier(Modifier::BOLD),
        ChatKind::TableRow => Style::default().fg(Color::Rgb(220, 220, 220)),
        // A table's frame reads as one fixed structure.
        ChatKind::TableBorder => Style::default().fg(Color::DarkGray),
        // Already a full line of `─` by the time it's here (`classify_
        // markdown_lines` renders it at the pane's own width) — nothing
        // left to style but the color.
        ChatKind::Hr => Style::default().fg(Color::DarkGray),
        ChatKind::Thinking => Style::default()
            .fg(Color::Rgb(130, 130, 130))
            .add_modifier(Modifier::DIM),
        ChatKind::Streaming => Style::default()
            .fg(Color::Rgb(140, 140, 140))
            .add_modifier(Modifier::ITALIC),
        ChatKind::Program => Style::default().fg(Color::Rgb(200, 190, 120)),
        ChatKind::Marker => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        ChatKind::Error => Style::default().fg(Color::Red),
    }
}

/// A program block header's severity color, read off its own status
/// word — reusing the palette a call's outcome already uses elsewhere
/// (a `⇒ result` line is green, an error line is red) instead of
/// inventing a fourth scheme. `None` for `running`/`suspended`: nothing
/// has gone right or wrong yet, so the header keeps its plain
/// `ChatKind::Program` color.
fn program_header_severity(text: &str) -> Option<Color> {
    if text.ends_with("failed") {
        Some(Color::Red)
    } else if text.ends_with("completed") {
        Some(Color::Green)
    } else {
        None
    }
}

/// Split a formatted table row (`"│ cell │ cell │"`, `chat.rs`'s
/// `format_table_row`) into spans so every `│` gets the *same* color as
/// the table's own border rows regardless of which row it's in — a
/// header row's `│` must not turn bold along with "Name", nor a body
/// row's `│` take on that row's plain color. `cell_style` is what
/// `chat_style` already computed for this row's `ChatKind` (bold for a
/// header, plain for a body); only the `│` characters override it.
fn table_row_spans(text: &str, cell_style: Style) -> Vec<Span<'static>> {
    let border_style = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    let mut cell = String::new();
    for c in text.chars() {
        if c == '│' {
            if !cell.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut cell), cell_style));
            }
            spans.push(Span::styled("│", border_style));
        } else {
            cell.push(c);
        }
    }
    if !cell.is_empty() {
        spans.push(Span::styled(cell, cell_style));
    }
    spans
}

/// The text of `branch`'s pending ask-to-user, if it has one — what
/// renders above the input line and switches it to reply mode.
fn asking_question_text(session: &Session, branch: BranchId) -> Option<String> {
    let call = session
        .branch_infos()
        .into_iter()
        .find(|info| info.branch == branch)?
        .asking_user?;
    match session.tree().events.get(&call).map(|e| &e.payload) {
        // A `choose` is not answerable without seeing what is on offer:
        // the options are numbered because typing `2` is what a person
        // does when reading a numbered list, and `pick_option` takes the
        // ordinal for exactly that reason. Anything else typed here is
        // still accepted — it reaches the agent as a condition to think
        // about rather than being refused at the input line.
        Some(EventPayload::Call(Call::Send { text, options, .. })) if !options.is_empty() => {
            let listed = options
                .iter()
                .enumerate()
                .map(|(i, o)| format!("  {}. {o}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!("{text}\n{listed}"))
        }
        Some(EventPayload::Call(Call::Send { text, .. })) => Some(text.clone()),
        _ => None,
    }
}

/// Word-wrap `text` to `width` columns and push one styled `Line` per
/// wrapped row (a blank line still pushes one empty row, so intentional
/// spacing in the transcript survives).
fn push_wrapped_width(lines: &mut Vec<Line<'static>>, text: &str, style: Style, width: usize) {
    let wrapped = textwrap::wrap(text, width);
    if wrapped.is_empty() {
        lines.push(Line::from(String::new()).style(style));
        return;
    }
    for row in wrapped {
        lines.push(Line::from(row.into_owned()).style(style));
    }
}

/// The input box's title while `mode` is armed — every explicit mode
/// gets one (19_UX Step C0), so arming any of them is as visible as
/// arming an ask already was.
fn explicit_mode_title(mode: ExplicitMode) -> &'static str {
    match mode {
        ExplicitMode::Rename => " rename ",
        ExplicitMode::ResumeWithValue => " resume ",
        ExplicitMode::Rewrite => " rewrite ",
        ExplicitMode::SpawnCharter => " spawn ",
    }
}

/// Word-wraps an `InputBuffer`'s every line to `width` columns (no
/// horizontal scrolling — a long line wraps, same as the transcript)
/// and returns the rendered rows plus which one holds the cursor.
/// "❯ " marks the first rendered row; continuation rows align under it
/// with two spaces instead.
///
/// When `show_cursor`, the cursor glyph (`▏`) is spliced into the
/// buffer's own text *before* wrapping, so it lands exactly where
/// wrapping would place a real character there — simpler and more
/// accurate than computing its wrapped position separately afterward.
/// Pure and `ratatui`-free so the cursor math is unit-testable without
/// a rendered `Frame`.
fn wrap_input(input: &InputBuffer, show_cursor: bool, width: usize) -> (Vec<String>, usize) {
    let (cursor_row, cursor_col) = input.cursor();
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_visual_row = 0usize;
    for row in 0..input.line_count() {
        let mut chars: Vec<char> = input.line(row).to_vec();
        if row == cursor_row && show_cursor {
            chars.insert(cursor_col.min(chars.len()), '▏');
        }
        let text: String = chars.into_iter().collect();
        let wrapped = textwrap::wrap(&text, width);
        let wrapped_rows: Vec<String> = if wrapped.is_empty() {
            vec![String::new()]
        } else {
            wrapped.into_iter().map(|s| s.into_owned()).collect()
        };
        for r in wrapped_rows {
            if r.contains('▏') {
                cursor_visual_row = rows.len();
            }
            let prefix = if rows.is_empty() { "❯ " } else { "  " };
            rows.push(format!("{prefix}{r}"));
        }
    }
    (rows, cursor_visual_row)
}

/// The spinner row, when the branch is doing something.
///
/// Braille frames at ~12/s off the elapsed time rather than a counter,
/// so it does not depend on being called at any particular rate — the
/// loop redraws every 30ms, and a missed tick just skips a frame.
fn spinner_line(app: &AttachedApp) -> Option<Line<'static>> {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let (what, since) = app.busy.as_ref()?;
    // What the word means, said plainly: the status vocabulary is for
    // `list_agents`, not for a person watching a pane.
    let label = match what.as_str() {
        "thinking" => "waiting for the model",
        "running" => "running a program",
        "queued" => "starting",
        other => other,
    };
    let elapsed = since.elapsed();
    let frame = FRAMES[(elapsed.as_millis() / 80) as usize % FRAMES.len()];
    let secs = elapsed.as_secs();
    // Under a second there is no number worth printing, and a reply
    // that lands promptly should not flash one.
    let time = if secs == 0 {
        String::new()
    } else if secs < 60 {
        format!(" · {secs}s")
    } else {
        format!(" · {}m{:02}s", secs / 60, secs % 60)
    };
    let style = Style::default()
        .fg(Color::Rgb(130, 130, 130))
        .add_modifier(Modifier::ITALIC);
    Some(Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{frame} {label}{time}"), style),
    ]))
}

fn render_chat(
    frame: &mut Frame,
    app: &AttachedApp,
    area: Rect,
    scroll: Option<usize>,
    asking: Option<&str>,
) -> (usize, Rect, Rect, Vec<usize>) {
    // The input box grows to fit a prefilled/multi-line buffer (Part
    // B's rewrite gesture, or Ctrl-O), capped so a long one still
    // leaves the chat pane standing.
    let max_input_height = (area.height / INPUT_MAX_HEIGHT_FRACTION).max(3);
    let input_height = (app.input.line_count() as u16 + 2).clamp(3, max_input_height);
    let [transcript_area, question_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if asking.is_some() { 3 } else { 0 }),
        Constraint::Length(input_height),
    ])
    .areas(area);

    // Ratatui's `Paragraph` clips an overlong line rather than wrapping it
    // unless told to, and its own `.wrap()` would break the manual
    // top/end slicing below (one row in `lines` must stay one scrollable
    // unit). So wrap here, to the pane's inner width, before scrolling
    // math ever sees the line count — a long error message (or any long
    // single-line text with no `\n` of its own) gets to span rows instead
    // of losing everything past the border.
    // Less the borders, and less **a column of margin down each side**.
    // The left one is where a selection's gutter mark goes, so selecting
    // a row paints that column rather than pushing the row's text over
    // by one — a block used to shuffle sideways as you moved through it.
    // The right one just keeps a slab off the border.
    let wrap_width = transcript_area
        .width
        .saturating_sub(2 + 2 * CHAT_MARGIN)
        .max(1) as usize;
    let rows = if app.show_markdown {
        app.chat
            .rows(app.selected, wrap_width, app.selected_program)
    } else {
        app.chat
            .rows_raw(app.selected, wrap_width, app.selected_program)
    };
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
    // One entry per wrapped line pushed below, naming which `rows` index
    // it came from — the inverse of the one-to-many row→lines expansion
    // `push_wrapped_width` does, and what lets a click (a wrapped-line
    // coordinate) find its way back to a logical row.
    let mut row_at_line: Vec<usize> = Vec::with_capacity(rows.len());
    let mut prev_speaker: Option<u8> = None;
    let mut prev_turn: Option<EventId> = None;
    for (row_idx, (kind, text, detail, id)) in rows.iter().enumerate() {
        if *kind == ChatKind::Thinking && !app.show_thinking {
            continue;
        }
        // **A blank line where one turn ends and the next begins**, and
        // nowhere else — a turn being one person's message, then one
        // model response to it.
        //
        // This used to break on any change of *block*, which is not the
        // same thing. One completion's rows alternate between its block
        // (header, source, `⚙` lines) and its prose, so a reply that
        // spoke, ran a cell, then spoke again was drawn as three pieces
        // with air between them — indistinguishable from three separate
        // responses, which is exactly what the blank line is supposed
        // to be telling you apart.
        //
        // Keyed on the completion now. Every row of one model response
        // carries that response's program id (prose included), so they
        // are one unbroken run; the blank falls between completions and
        // between the person's turn and the answer to it. A row that
        // belongs to no completion — a user post, the system header, a
        // marker — is its own turn, keyed by its own event id.
        let speaker = speaker_of(*kind);
        let block = match detail {
            RowDetail::Program(pid)
            | RowDetail::Invoke(pid, _)
            | RowDetail::Note(pid, _)
            | RowDetail::Prose(pid) => Some(*pid),
            RowDetail::None => None,
        };
        let turn = block.unwrap_or(*id);
        if prev_turn.is_some_and(|p| p != turn) || prev_speaker.is_some_and(|p| p != speaker) {
            lines.push(Line::from(""));
            row_at_line.resize(lines.len(), row_idx);
        }
        prev_turn = Some(turn);
        prev_speaker = Some(speaker);
        let mut style = chat_style(*kind);
        if matches!(detail, RowDetail::Program(_))
            && let Some(color) = program_header_severity(text)
        {
            style = style.fg(color);
        }
        // **Compaction fades a row; it does not take it away.** What
        // the next request carries and what the person has read are two
        // different things, and this pane is the second one. A
        // compacted row used to have its text replaced by the word
        // `[removed]`, so one pass over a long conversation left fifty
        // of those in the scrollback and nothing else.
        if app.chat.is_compacted(*id) {
            style = style.add_modifier(Modifier::DIM);
        }
        // **Selection is a mark in the gutter, not an inversion.**
        // `REVERSED` meant three different things at once — the
        // selected sub-item, the fork-from-here target, and the help
        // bar — so two of them looked identical, and on a code row it
        // swapped the slab for the foreground, which read as damage
        // rather than as a cursor. A gutter mark composes with a
        // background instead of fighting it.
        //
        // **The whole selected block is marked**, not just a row of it.
        // The block being open is the other half of the same fact — its
        // source is showing because it is the one you are looking at —
        // and a bar down its edge is what says so. Narrowed to a single
        // row once a sub-item is picked out, which is then the finer
        // thing being pointed at.
        //
        // **Prose is beside the program, not in it.** It belongs to the
        // same completion — which is why it sits in the same unbroken
        // run and shares the turn key above — but selecting a *program*
        // points at the program: its lid, its source, the calls it
        // made. Marking the answer it spoke as well drew the bar down
        // an entire response, which says "this is the thing you picked"
        // about four different things at once.
        let in_block = block.is_some()
            && block == app.selected_program
            && !matches!(detail, RowDetail::Prose(_));
        let selected = match app.selected_effect {
            Some(Effect::Invoke(sel)) => matches!(detail, RowDetail::Invoke(pid, idx)
                if app.selected_program == Some(*pid) && *idx == sel),
            Some(Effect::Append(note)) => matches!(detail, RowDetail::Note(pid, nid)
                if app.selected_program == Some(*pid) && *nid == note),
            None => in_block,
        } || app.last_clicked_event == Some(*id);
        let before = lines.len();
        if app.show_markdown
            && matches!(
                kind,
                ChatKind::Assistant
                    | ChatKind::Streaming
                    | ChatKind::Heading
                    | ChatKind::Blockquote
            )
        {
            let spans = markdown::inline_spans(text, style);
            lines.extend(markdown::wrap_spans(&spans, style, wrap_width));
        } else if *kind == ChatKind::Code
            && matches!(detail, RowDetail::Program(_))
            && text.chars().count() <= wrap_width
        {
            // **A cell's source is syntax-highlighted** (D13), through the
            // highlighter the source pane already drives — the work here is
            // routing, not a second tokenizer. Scoped to a cell's own rows
            // (`RowDetail::Program`), so a fenced block quoted in *prose*
            // stays literal, which is what a quote is for.
            //
            // Only when it fits unwrapped: a wrapped line would have to
            // re-tokenize per fragment to keep the colors right, and an
            // overlong line falls back to the plain wrap below rather than
            // to mis-colored code.
            lines.push(Line::from(js_spans(text, style)));
        } else if matches!(kind, ChatKind::TableHeader | ChatKind::TableRow)
            && text.chars().count() <= wrap_width
        {
            // Only when it fits unwrapped: reflowing a formatted table
            // row word-by-word would misalign its columns regardless of
            // color, so an overlong one falls back to the plain (single
            // color) wrap below rather than to a broken table.
            lines.push(Line::from(table_row_spans(text, style)));
        } else {
            push_wrapped_width(&mut lines, text, style, wrap_width);
        }
        debug_assert!(lines.len() > before, "every row pushes at least one line");
        // **A slab reaches the edge, and a selection shows in the
        // gutter.** ratatui paints a `Line`'s style only where the line
        // has cells, so a background stopped at the last character and
        // a block read as ragged highlighting rather than a panel. Both
        // are applied here, after the row's lines exist, because a row
        // can become several of them and every one is part of the same
        // shape.
        let slab = slab_of(*kind, detail);
        for line in &mut lines[before..] {
            let width: usize = line
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>();
            if let Some(bg) = slab {
                if width < wrap_width {
                    line.spans
                        .push(Span::styled(" ".repeat(wrap_width - width), style));
                }
                for span in &mut line.spans {
                    span.style = span.style.bg(bg);
                }
            }
            // The margins, outside whatever the slab painted: the mark
            // occupies the left one when this row is selected, and the
            // row is laid out as if it always did.
            line.spans.insert(
                0,
                if selected {
                    Span::styled("▌", Style::default().fg(Color::Cyan))
                } else {
                    Span::raw(" ")
                },
            );
            line.spans.push(Span::raw(" "));
        }
        row_at_line.resize(lines.len(), row_idx);
    }
    // **Something is happening, and here is where it will land.** Last
    // of all, on the line the next message will occupy, for exactly as
    // long as the branch is busy.
    //
    // Reasoning is hidden unless Ctrl-T is on and no `Reply` is logged
    // until the first *text* chunk, so a model thinking hard wrote
    // nothing here at all — indistinguishable from a dead socket, which
    // is a distinction the person cannot make any other way and the
    // one they most need. The elapsed count is the point: a spinner
    // alone says "busy", a spinner and `47s` says which kind.
    if let Some(line) = spinner_line(app) {
        lines.push(line);
        row_at_line.resize(lines.len(), rows.len().saturating_sub(1));
    }
    let visible = transcript_area.height.saturating_sub(2) as usize;
    let default_top = lines.len().saturating_sub(visible);
    let top = scroll.unwrap_or(default_top).min(default_top);
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec())
            .block(Block::default().borders(Borders::ALL).title(" chat ")),
        transcript_area,
    );

    if let Some(question) = asking {
        frame.render_widget(
            Paragraph::new(question)
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(" waiting on your answer "),
                ),
            question_area,
        );
    }
    let (border, cursor, title) = match (app.focus, asking) {
        // `on_input_key`'s Enter checks `explicit_mode` before anything
        // else, unconditionally — an armed mode fires on Enter even
        // when this branch also owes a reply (`resolve_submit_mode`
        // never looks at `asking_user`). So an armed mode's title wins
        // here too, to say what Enter will actually do; "reply" only
        // shows when nothing is armed to preempt it. (That an explicit
        // mode can silently eat an owed reply this way at all is a
        // sharper edge than this step means to fix — flagged, not
        // addressed, here.) Armed is armed, regardless of which mode —
        // same color, same style for all four.
        (Focus::Input, _) if app.explicit_mode.is_some() => (
            Style::default().fg(Color::Yellow),
            "▏",
            explicit_mode_title(app.explicit_mode.unwrap()),
        ),
        (Focus::Input, Some(_)) => (Style::default().fg(Color::Yellow), "▏", " reply "),
        (Focus::Input, None) if app.ask_armed => (Style::default().fg(Color::Yellow), "▏", " ask "),
        (Focus::Input, None) => (Style::default().fg(Color::Cyan), "▏", " message "),
        (Focus::Debug, _) => (Style::default().fg(Color::DarkGray), "", " message "),
    };
    // No horizontal scrolling — a line wider than the box wraps, same
    // as the transcript above.
    let input_wrap_width = input_area.width.saturating_sub(2 + 2).max(1) as usize;
    let (input_rows, cursor_visual_row) =
        wrap_input(&app.input, !cursor.is_empty(), input_wrap_width);
    let input_lines: Vec<Line<'static>> = input_rows.into_iter().map(Line::from).collect();
    let input_visible = input_area.height.saturating_sub(2) as usize;
    let input_max_top = input_lines.len().saturating_sub(input_visible);
    let input_top = cursor_visual_row
        .saturating_sub(input_visible.saturating_sub(1))
        .min(input_max_top);
    let input_end = (input_top + input_visible).min(input_lines.len());
    frame.render_widget(
        Paragraph::new(input_lines[input_top..input_end].to_vec()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        ),
        input_area,
    );
    (top, transcript_area, input_area, row_at_line)
}

/// Every branch, nested exactly as the log nests them (`parent_branch`):
/// a fork under the branch it diverged from, a spawned agent's first
/// branch under the branch that spawned it. Root-first, children grouped
/// under their parent and sorted by id — the navigator's row order and
/// what Tab/`1`–`9`/clicks index into.
fn ordered_branches(mut infos: Vec<BranchInfo>) -> Vec<BranchInfo> {
    infos.sort_by_key(|b| b.branch.as_u64());
    let mut children: HashMap<Option<BranchId>, Vec<BranchInfo>> = HashMap::new();
    for info in infos {
        children.entry(info.parent_branch).or_default().push(info);
    }
    let mut result = Vec::new();
    fn dfs(
        parent: Option<BranchId>,
        children: &mut HashMap<Option<BranchId>, Vec<BranchInfo>>,
        result: &mut Vec<BranchInfo>,
    ) {
        let Some(kids) = children.remove(&parent) else {
            return;
        };
        for kid in kids {
            let id = kid.branch;
            result.push(kid);
            dfs(Some(id), children, result);
        }
    }
    dfs(None, &mut children, &mut result);
    result
}

/// `ordered_branches` plus the box-drawing prefix and edge kind for each
/// row: `is_fork` distinguishes a fork (the same context diverged) from
/// a spawn (a new clean-room context) — the difference `context()` turns
/// on, so the navigator marks it (17_BRANCHES Part D box 2).
fn navigator_rows(infos: Vec<BranchInfo>) -> Vec<(BranchInfo, String, bool)> {
    let ordered = ordered_branches(infos);
    let mut children_count: HashMap<Option<BranchId>, usize> = HashMap::new();
    for info in &ordered {
        *children_count.entry(info.parent_branch).or_insert(0) += 1;
    }
    // Recompute is_last per row using each parent's remaining sibling
    // count as we walk in DFS order (already the walk order `dfs` above
    // produced), so no second tree pass is needed.
    let mut seen: HashMap<Option<BranchId>, usize> = HashMap::new();
    let mut depth_last: HashMap<BranchId, Vec<bool>> = HashMap::new();
    let mut result = Vec::with_capacity(ordered.len());
    for info in ordered {
        let siblings = *children_count.get(&info.parent_branch).unwrap_or(&1);
        let idx = seen.entry(info.parent_branch).or_insert(0);
        *idx += 1;
        let is_last = *idx == siblings;
        let ancestors_last = info
            .parent_branch
            .and_then(|p| depth_last.get(&p).cloned())
            .unwrap_or_default();
        let mut prefix = String::new();
        for &ancestor_last in &ancestors_last {
            prefix.push_str(if ancestor_last { "    " } else { "│   " });
        }
        if info.parent_branch.is_some() {
            prefix.push_str(if is_last { "└── " } else { "├── " });
        }
        let mut own_last = ancestors_last;
        own_last.push(is_last);
        depth_last.insert(info.branch, own_last);
        let is_fork = info.agent != info.branch;
        result.push((info, prefix, is_fork));
    }
    result
}

/// (branches asking you, branches thinking) — the header's two counts.
fn branch_counts(infos: &[BranchInfo]) -> (usize, usize) {
    let waiting = infos.iter().filter(|b| b.asking_user.is_some()).count();
    let thinking = infos.iter().filter(|b| b.thinking).count();
    (waiting, thinking)
}

/// Every post of yours across the whole tree, each row jumping to its
/// branch on Enter — "a filter you can open, not a place you live"
/// (17_BRANCHES Part D).
fn render_timeline(frame: &mut Frame, app: &AttachedApp, session: &Session, area: Rect) {
    let rows = timeline_rows(session);
    let cursor = app.timeline_cursor.min(rows.len().saturating_sub(1));
    let lines: Vec<Line> = if rows.is_empty() {
        vec![Line::from("(no posts of yours yet)").style(Style::default().fg(Color::DarkGray))]
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, (_, branch, text))| {
                let name = session
                    .tree()
                    .branch_name(*branch)
                    .or_else(|| {
                        find_leaf(session, *branch)
                            .and_then(|leaf| derived_branch_label(session.tree(), *branch, leaf))
                    })
                    .unwrap_or_else(|| format!("branch #{}", branch.as_u64()));
                let first_line = text.lines().next().unwrap_or("");
                let text = format!("{}: {}", name, first_line);
                let style = if i == cursor {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::from(text).style(style)
            })
            .collect()
    };
    // No independent scroll input here (only `j`/`k`, which move
    // `cursor`) — the viewport is a pure function of the cursor and the
    // area height, recomputed every frame, same shape as the input box's
    // own cursor-follow scrolling above.
    let visible = area.height.saturating_sub(2) as usize;
    let top = scroll_top_following(cursor, rows.len(), visible);
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" timeline — every post of yours "),
        ),
        area,
    );
}

/// A scroll offset that auto-follows one index: the smallest `top` that
/// keeps `index` inside `top..top+visible`, clamped so the window never
/// runs past the end of the rows. Shared by the timeline (follows
/// `timeline_cursor`, 19_UX Step D0) and the navigator (follows the
/// selected branch's row, Step E0) — pure and ratatui-free so it can be
/// tested directly, same reasoning as `wrap_input` (Step A2).
fn scroll_top_following(index: usize, rows_len: usize, visible: usize) -> usize {
    let max_top = rows_len.saturating_sub(visible);
    index.saturating_sub(visible.saturating_sub(1)).min(max_top)
}

fn render_navigator(frame: &mut Frame, app: &AttachedApp, session: &Session, area: Rect) -> usize {
    // `branch_infos` is the navigator projection: identity and shape from
    // the log (so it survives resume, decision 8), status/thinking from
    // live session state.
    let infos = session.branch_infos();
    let (waiting, thinking) = branch_counts(&infos);
    let rows = navigator_rows(infos);
    let selected_idx = rows
        .iter()
        .position(|(info, _, _)| Some(info.branch) == app.selected)
        .unwrap_or(0);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(info, prefix, is_fork)| {
            let selected = app.selected == Some(info.branch);
            let live = info.status != "dormant";
            let status = if info.asking_user.is_some() {
                "asking you".to_owned()
            } else {
                info.status.clone()
            };
            let paused = live && session.is_paused(info.branch);
            let busy = matches!(info.status.as_str(), "running" | "thinking");
            let name = info.name.clone().unwrap_or_else(|| {
                derived_branch_label(session.tree(), info.branch, info.leaf)
                    .unwrap_or_else(|| format!("branch #{}", info.branch.as_u64()))
            });
            let edge = if *is_fork { "⑂ " } else { "" };
            let open = if info.open > 0 {
                format!(" · {} open", info.open)
            } else {
                String::new()
            };
            let text = format!(
                "{} {}{}{} · {}{}{}",
                if selected { "▶" } else { " " },
                prefix,
                edge,
                name,
                status,
                open,
                if paused {
                    " ⏸"
                } else if busy {
                    " ●"
                } else {
                    ""
                },
            );
            let style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else if info.asking_user.is_some() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(text).style(style)
        })
        .collect();
    // "There is no home; the tree comes to you" (17_BRANCHES Part D): the
    // header counts what needs you, right where the tree already is.
    // `w jump` only means something when the count beside it is nonzero
    // — same reasoning as the footer hint's own `w waiting`.
    let jump = if waiting > 0 { " · w jump" } else { "" };
    let title =
        format!(" agents · {waiting} waiting on you · {thinking} thinking{jump} · t timeline ");
    let visible = area.height.saturating_sub(2) as usize;
    let top = app
        .navigator_scroll
        .unwrap_or_else(|| scroll_top_following(selected_idx, rows.len(), visible))
        .min(rows.len().saturating_sub(visible));
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec())
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
    top
}

/// Console + status for the selected agent's VM (live or post-mortem).
fn render_attached_console(
    frame: &mut Frame,
    app: &AttachedApp,
    session: &Session,
    area: Rect,
    scroll: Option<usize>,
) -> (usize, Rect) {
    let state = app.selected.and_then(|f| session.state(f));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(state) = state {
        if let Some(vm) = state.vm() {
            lines.extend(vm.console_lines.iter().map(|l| Line::from(l.as_str())));
        }
        let status = format!(
            "· {}{}",
            state.status(),
            if !state.vm_is_live() && state.vm().is_some() {
                " (final program state)"
            } else {
                ""
            }
        );
        lines.push(Line::from(status).style(Style::default().fg(Color::DarkGray)));
    }
    let visible = area.height.saturating_sub(2) as usize;
    let default_top = lines.len().saturating_sub(visible);
    let top = scroll.unwrap_or(default_top).min(default_top);
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec())
            .block(Block::default().borders(Borders::ALL).title(" console ")),
        area,
    );
    (top, area)
}

/// Console + result/condition footer from the log projection (Step 5
/// — no live VM, program view from the tree).
fn render_console_from_pv(
    frame: &mut Frame,
    pv: &ProgramView,
    area: Rect,
    scroll: Option<usize>,
) -> (usize, Rect) {
    let mut lines: Vec<Line> = pv.console.iter().map(|l| Line::from(l.as_str())).collect();
    if let Some(ref result) = pv.result {
        lines.push(Line::from(format!("⇒ {result}")).style(Style::default().fg(Color::Green)));
    } else if let Some(cause) = &pv.condition {
        lines.push(
            Line::from(format!("⚡ {}", condition_line(cause)))
                .style(Style::default().fg(Color::Yellow)),
        );
    }
    let visible = area.height.saturating_sub(2) as usize;
    let default_top = lines.len().saturating_sub(visible);
    let top = scroll.unwrap_or(default_top).min(default_top);
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec())
            .block(Block::default().borders(Borders::ALL).title(" console ")),
        area,
    );
    (top, area)
}

/// **The thing you selected, in full.** A call's arguments and result,
/// or an append's value — the detail behind a one-line effect row.
///
/// This is what sits on the right of the chat views, where the source
/// pane used to. The source is already on the screen (a block shows
/// its own cells) and re-showing it there answered a question nobody
/// was asking; what a reader wants beside the transcript is the thing
/// they just clicked. `Pane::Source` stays in `FullDebug`, where
/// stepping through instructions is the point.
fn render_detail(
    frame: &mut Frame,
    app: &AttachedApp,
    session: &Session,
    pv: Option<&ProgramView>,
    area: Rect,
) -> usize {
    let mut lines: Vec<Line> = Vec::new();
    let title = match app.selected_effect {
        // **What `history.fetch` would hand back.** The transcript row
        // is one line and says how big the value was; this is where the
        // value itself is, whole, for the person who asked to see it.
        Some(Effect::Append(note)) => {
            match session.tree().events.get(&note).map(|e| &e.payload) {
                Some(EventPayload::Note { value, .. }) => {
                    lines.push(
                        Line::from(format!("▸ appended  #{}", note.as_u64()))
                            .style(Style::default().fg(Color::Yellow)),
                    );
                    lines.push(Line::from(""));
                    let body = match value {
                        serde_json::Value::String(t) => t.clone(),
                        other => serde_json::to_string_pretty(other)
                            .unwrap_or_else(|_| other.to_string()),
                    };
                    for l in body.lines() {
                        lines.push(Line::from(l.to_owned()));
                    }
                }
                _ => lines.push(Line::from("(append not found)")),
            }
            " ▸ appended ".to_string()
        }
        Some(Effect::Invoke(idx)) => match pv.and_then(|pv| pv.invokes.get(idx)) {
            Some(invoke) => {
                lines.push(
                    Line::from(format!("⚙ {}", invoke.name))
                        .style(Style::default().fg(Color::Yellow)),
                );
                lines.push(Line::from(""));
                lines.push(Line::from("args:").style(Style::default().fg(Color::DarkGray)));
                let args_str = serde_json::to_string_pretty(&invoke.args)
                    .unwrap_or_else(|_| format!("{:?}", invoke.args));
                for l in args_str.lines() {
                    lines.push(Line::from(l.to_owned()));
                }
                lines.push(Line::from(""));
                let (head, body) = match &invoke.outcome {
                    Some(Outcome::Delivered(v)) => (
                        "result:",
                        serde_json::to_string_pretty(v).unwrap_or_else(|_| format!("{v:?}")),
                    ),
                    Some(Outcome::Failed(msg)) => ("failed:", msg.clone()),
                    None => ("result:", "(pending — no result recorded)".to_owned()),
                };
                lines.push(Line::from(head).style(Style::default().fg(Color::DarkGray)));
                for l in body.lines() {
                    lines.push(Line::from(l.to_owned()));
                }
                format!(" ⚙ {} ", invoke.name)
            }
            None => {
                lines.push(Line::from("(invoke not found)"));
                " detail ".to_string()
            }
        },
        // Says what to do rather than what is missing: the pane is
        // empty because nothing is picked, which is a normal state and
        // not a failure to render something.
        None => {
            lines.push(
                Line::from("click a ⚙ call or a ▸ append to see it here")
                    .style(Style::default().fg(Color::DarkGray)),
            );
            " detail ".to_string()
        }
    };
    let visible = area.height.saturating_sub(2) as usize;
    let default_top = lines.len().saturating_sub(visible);
    let top = app.detail_scroll.unwrap_or(default_top).min(default_top);
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec())
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
    top
}

fn render_placeholder(frame: &mut Frame, pane: Pane, area: Rect) {
    let title = match pane {
        Pane::Chat => " chat ",
        Pane::Source => " source [1] ",
        Pane::Disasm => " disassembly [2] ",
        Pane::Stack => " stack [3] ",
        Pane::Promises => " promises [4] ",
        Pane::Navigator => " agents ",
        Pane::Console => " console ",
        Pane::Detail => " detail ",
        Pane::Input => unreachable!("Pane::Input is never a placeholder target"),
    };
    frame.render_widget(
        Paragraph::new("(no program yet)")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// One-line summary of a logged condition for the console footer.
fn condition_line(cause: &crate::types::Handback) -> String {
    match cause {
        crate::types::Handback::Raised { name, .. } => format!("raised `{name}`"),
        crate::types::Handback::Trapped { message, .. } => message.clone(),
        crate::types::Handback::Posted { .. } => "a message arrived".to_owned(),
        crate::types::Handback::CellFailed { .. } => "compile error".to_owned(),
        crate::types::Handback::Completed { rested: true, .. } => "finished".to_owned(),
        crate::types::Handback::Completed { .. } => "completed".to_owned(),
        crate::types::Handback::Interrupted => "interrupted".to_owned(),
        // A handler decided `return abandon()`: the suspended run was
        // discarded, not continued.
        crate::types::Handback::Abandoned => "abandoned".to_owned(),
        crate::types::Handback::Superseded => "superseded".to_owned(),
    }
}

/// Render a pane and read back what actually landed on the terminal.
///
/// **Because this is the only pane whose correctness is how it looks.**
/// Every other test here asserts on the row *model* — the strings
/// `ChatState` produces — and the row model is not the thing a person
/// sees: a background that stops at the end of the text, a selection
/// drawn three different ways, a slab that does not reach the edge are
/// all invisible to it. The tests that did look scanned the buffer cell
/// by cell with nested loops, once per question asked.
///
/// So: render to a `TestBackend`, then ask the buffer questions in the
/// terms the change is about — where does this row's background start
/// and stop, what colour is that gutter, does this text appear. And
/// `dump` prints the whole thing with its backgrounds marked, which is
/// how someone who cannot see the terminal reviews a change to it.
#[cfg(test)]
pub(crate) struct Screen {
    buffer: ratatui::buffer::Buffer,
}

#[cfg(test)]
impl Screen {
    /// Draw the chat pane at `w`×`h` and keep the buffer.
    pub(crate) fn chat(app: &AttachedApp, w: u16, h: u16) -> Self {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("a test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, app, area, None, None);
            })
            .expect("draw");
        Self {
            buffer: terminal.backend().buffer().clone(),
        }
    }

    fn width(&self) -> u16 {
        self.buffer.area.width
    }

    fn height(&self) -> u16 {
        self.buffer.area.height
    }

    /// One row's text, trailing blanks kept off.
    pub(crate) fn row(&self, y: u16) -> String {
        (0..self.width())
            .filter_map(|x| self.buffer.cell((x, y)).map(|c| c.symbol().to_owned()))
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// One row with the pane's own border trimmed off — what the row
    /// *says*, as distinct from what the terminal holds. Every
    /// question about content wants this; `row` is for the ones about
    /// the frame.
    pub(crate) fn inner(&self, y: u16) -> String {
        self.row(y)
            .trim_start_matches('│')
            .trim_end_matches('│')
            .trim_end()
            .to_owned()
    }

    /// Every row, as one string — for `contains` questions.
    pub(crate) fn text(&self) -> String {
        (0..self.height())
            .map(|y| self.row(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The first row whose text contains `needle`.
    pub(crate) fn find(&self, needle: &str) -> Option<u16> {
        (0..self.height()).find(|y| self.row(*y).contains(needle))
    }

    /// The background colours across one row, run-length encoded — the
    /// question "does the slab reach the edge" in the form the buffer
    /// can answer it.
    pub(crate) fn backgrounds(&self, y: u16) -> Vec<(Color, u16)> {
        let mut runs: Vec<(Color, u16)> = Vec::new();
        for x in 0..self.width() {
            let bg = self.buffer.cell((x, y)).map_or(Color::Reset, |c| c.bg);
            match runs.last_mut() {
                Some((last, n)) if *last == bg => *n += 1,
                _ => runs.push((bg, 1)),
            }
        }
        runs
    }

    pub(crate) fn modifiers(&self, y: u16) -> Modifier {
        (0..self.width())
            .filter_map(|x| self.buffer.cell((x, y)).map(|c| c.modifier))
            .fold(Modifier::empty(), |a, b| a | b)
    }

    /// The whole pane with its backgrounds marked, for a reader who
    /// cannot see the terminal. `·` is the default background; any
    /// other gets a letter, and the key is printed underneath.
    pub(crate) fn dump(&self) -> String {
        let mut seen: Vec<Color> = Vec::new();
        let mut out = String::new();
        for y in 0..self.height() {
            let mut marks = String::new();
            for x in 0..self.width() {
                let bg = self.buffer.cell((x, y)).map_or(Color::Reset, |c| c.bg);
                if bg == Color::Reset {
                    marks.push('·');
                    continue;
                }
                let at = seen.iter().position(|c| *c == bg).unwrap_or_else(|| {
                    seen.push(bg);
                    seen.len() - 1
                });
                marks.push((b'a' + at as u8) as char);
            }
            out.push_str(&format!("{y:>3} |{marks}| {}\n", self.row(y)));
        }
        for (i, c) in seen.iter().enumerate() {
            out.push_str(&format!("      {} = {c:?}\n", (b'a' + i as u8) as char));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with one of everything the chat pane draws: a user
    /// turn, prose, a multi-line cell, calls under it, a `tell`, and a
    /// terminal. What every look-at-it test renders.
    fn a_session_with_one_of_everything() -> AttachedApp {
        let (tx, rx) = channel();
        let mut registry = ToolRegistry::new();
        registry.register(crate::host::ToolDef {
            name: "read_file".into(),
            description: String::new(),
            input_schema: serde_json::json!({ "type": "array" }),
            guidelines: Vec::new(),
            example: None,
            returns: None,
            handler: Box::new(|_| Ok(serde_json::json!({ "content": "x = 1\n", "version": "v1" }))),
            show_once: false,
        });
        let session = Session::new(
            Tree::new(None),
            "a test agent",
            registry,
            Box::new(ScriptedLlm::new([scripted_markdown(concat!(
                "Reading it first, then I will say what I found.\n\n",
                "```js\n",
                "const f = await tools.read_file(\"a.txt\");\n",
                "const n = f.content.trim().length;\n",
                "const doubled = n * 2;\n",
                "const label = `len ${n}`;\n",
                "console.log(label, doubled);\n",
                "tell(`a.txt is ${n} characters.`);\n",
                "```\n",
            ))])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "how long is a.txt?".into(),
            expects_reply: true,
        });
        let session = session.run();
        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }
        app
    }

    /// **A program block is one shape that reaches both edges.** A
    /// `Line`'s style paints only where the line has cells, so every
    /// background used to stop at the last character and a block read
    /// as ragged highlighting rather than a panel.
    #[test]
    fn a_program_block_is_a_full_width_slab_in_three_shades() {
        let app = a_session_with_one_of_everything();
        let screen = Screen::chat(&app, 64, 26);

        let lid = screen.find("program: completed").expect("the block's lid");
        let code = screen.find("const n =").expect("its source");
        let call = screen.find("⚙ read_file").expect("the call it made");

        for (y, what) in [(lid, "lid"), (code, "source"), (call, "call")] {
            let runs = screen.backgrounds(y);
            // The pane's border owns column 0 and the last column; the
            // slab is everything between, in one piece.
            let inner: Vec<_> = runs[1..runs.len() - 1].to_vec();
            assert_eq!(
                inner.len(),
                1,
                "the {what} row is not one slab: {runs:?}\n{}",
                screen.dump()
            );
        }
        // Three shades of one family: lid, source, and the calls
        // attached below it.
        let shade = |y| screen.backgrounds(y)[1].0;
        assert_ne!(shade(lid), shade(code));
        assert_ne!(shade(code), shade(call));
    }

    /// **The fences are gone and their tag is not.** A `Part::Cell`
    /// carries its own ` ```js `, which makes the parts concatenate
    /// back to the reply and said nothing to a reader — but the tag on
    /// it says which dialect ran, so it moves to the lid.
    #[test]
    fn a_block_shows_no_fences_and_names_its_dialect() {
        let app = a_session_with_one_of_everything();
        let screen = Screen::chat(&app, 64, 26);
        assert!(
            !screen.text().contains("```"),
            "a fence reached the screen:\n{}",
            screen.dump()
        );
        assert!(
            screen
                .inner(screen.find("program:").unwrap())
                .contains("· js"),
            "the lid names the dialect the fence did"
        );
    }

    /// **Two responses, two runs, one blank line between them.** The
    /// fixture above has a single completion, which cannot tell a rule
    /// that separates completions from one that separates nothing.
    #[test]
    fn consecutive_completions_are_separated_from_each_other() {
        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "a test agent",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([
                scripted_markdown("First answer.\n\n```js\ntell(\"one\");\n```\n"),
                scripted_markdown("Second answer.\n\n```js\ntell(\"two\");\n```\n"),
            ])),
            tx,
        )
        .unwrap();
        let branch = session.conversation_branch();
        session.handle().send(SessionCommand::UserTurn {
            branch,
            text: "first question".into(),
            expects_reply: true,
        });
        session.handle().send(SessionCommand::UserTurn {
            branch,
            text: "second question".into(),
            expects_reply: true,
        });
        let session = session.run();
        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let screen = Screen::chat(&app, 64, 30);
        let two = screen.find("Second answer").expect("the second response");
        let first_lid = screen.find("program:").expect("the first block");
        assert!(first_lid < two, "the responses are in order");
        // Somewhere between the end of the first response and the start
        // of the second there is exactly one blank row, and none inside
        // either run.
        let blanks: Vec<u16> = (first_lid..two)
            .filter(|r| screen.inner(*r).trim().is_empty())
            .collect();
        assert_eq!(
            blanks.len(),
            1,
            "one blank between two completions, got {blanks:?}:\n{}",
            screen.dump()
        );
    }

    /// **Selecting a program marks the program, not the whole
    /// response.** Prose shares the completion — same unbroken run,
    /// same turn key — but the gutter bar points at what was picked,
    /// and what was picked is a program: its lid, its source, its
    /// calls.
    #[test]
    fn selecting_a_program_does_not_mark_the_prose_beside_it() {
        let mut app = a_session_with_one_of_everything();
        app.selected_program = Some(EventId::new(3));
        let screen = Screen::chat(&app, 64, 26);

        let lid = screen.find("program:").expect("the lid");
        let call = screen.find("⚙ read_file").expect("the call");
        let prose = screen.find("Reading it first").expect("the prose");
        for (row, marked) in [(lid, true), (call, true), (prose, false)] {
            assert_eq!(
                screen.inner(row).starts_with('▌'),
                marked,
                "row {row} mark should be {marked}:\n{}",
                screen.dump()
            );
        }
    }

    /// **`/fork` and `/spawn` hand work out in one gesture.**
    ///
    /// The model often will not: on a thread worth keeping, asked
    /// something whose findings it would have to hold, it hands the
    /// work out in 0 of 20 samples unless a worked example shows it.
    /// The person watching their thread fill up should not have to
    /// negotiate about it.
    #[test]
    fn slash_fork_and_spawn_hand_work_out_and_name_themselves() {
        let branch = fid(1);
        let leaf = Some(EventId::new(9));

        let Some(SessionCommand::Fork { from, name, text }) =
            slash_command(branch, leaf, "/fork should we pull this patch?")
        else {
            panic!("/fork makes a fork");
        };
        assert_eq!(from, EventId::new(9), "forked from where the branch is");
        assert_eq!(name.as_deref(), Some("should we pull this patch?"));
        assert_eq!(
            text.as_deref(),
            Some("should we pull this patch?"),
            "and it is told what it is for in the same gesture"
        );

        let Some(SessionCommand::Spawn { charter, name, .. }) =
            slash_command(branch, leaf, "/spawn run the suite and say what fails")
        else {
            panic!("/spawn makes an agent");
        };
        assert_eq!(charter, "run the suite and say what fails");
        // Exactly at the bound, so it is kept whole.
        assert_eq!(name.as_deref(), Some("run the suite and say what fails"));

        // **Only these two, and only with something to say.** Anything
        // else is a message: a person writing about `/fork` in prose is
        // not issuing one.
        assert!(slash_command(branch, leaf, "/fork").is_none());
        assert!(slash_command(branch, leaf, "/fork   ").is_none());
        assert!(slash_command(branch, leaf, "/rename x").is_none());
        assert!(slash_command(branch, leaf, "what does /fork do?").is_none());
    }

    /// A name is a reminder, not an address: it comes from the first
    /// words of the message and stops on a word.
    #[test]
    fn a_derived_name_stops_on_a_word() {
        assert_eq!(name_from_message("short one"), Some("short one".into()));
        assert_eq!(
            name_from_message("  ragged\n  whitespace   collapses "),
            Some("ragged whitespace collapses".into())
        );
        let long = name_from_message("judge whether the retry refactor is wired in correctly")
            .expect("a name");
        assert!(long.len() <= 35, "bounded: {long:?}");
        assert!(long.ends_with('…'), "says it was cut: {long:?}");
        assert!(!long.contains("  "), "no ragged edge: {long:?}");
        assert_eq!(name_from_message("   "), None);
    }

    /// **Something is happening, and the pane says so.**
    ///
    /// The whole point is the case where nothing else moves: reasoning
    /// rows are hidden unless Ctrl-T is on and no `Reply` is logged
    /// until the first text chunk, so a model thinking for minutes drew
    /// an unchanging pane — identical to a dead socket.
    #[test]
    fn a_spinner_marks_the_wait_where_the_next_message_will_land() {
        let mut app = a_session_with_one_of_everything();
        assert!(
            Screen::chat(&app, 64, 26)
                .find("waiting for the model")
                .is_none(),
            "nothing is happening, so nothing is claimed"
        );

        app.busy = Some((
            "thinking".into(),
            Instant::now() - std::time::Duration::from_secs(47),
        ));
        let screen = Screen::chat(&app, 64, 26);
        let row = screen
            .find("waiting for the model")
            .expect("the wait is on the pane");
        assert!(
            screen.inner(row).contains("47s"),
            "and how long it has been waiting: {:?}",
            screen.inner(row)
        );

        // **Last of all**: it marks where the answer will appear, so
        // every row of the conversation is above it.
        let last = screen
            .find("a.txt is 5 characters")
            .expect("the final prose");
        assert!(row > last, "the spinner is below the transcript");

        // A running program says what it is, not the same word.
        app.busy = Some(("running".into(), Instant::now()));
        let screen = Screen::chat(&app, 64, 26);
        assert!(screen.find("running a program").is_some());
        // Under a second, no number: a prompt reply must not flash one.
        let row = screen.find("running a program").unwrap();
        assert!(!screen.inner(row).contains('s'), "{:?}", screen.inner(row));

        // And it goes when the work does.
        app.busy = None;
        assert!(
            Screen::chat(&app, 64, 26)
                .find("running a program")
                .is_none()
        );
    }

    /// **Air falls between turns, and never inside one.** A turn is the
    /// person's message and the one model response to it; that response
    /// is a block, the prose it spoke, and the calls it made.
    ///
    /// The rule used to break on any change of *block*, which put a
    /// blank line between a completion's own parts — so a reply that
    /// spoke, ran a cell, then spoke again was drawn as three pieces
    /// with air between them, indistinguishable from three separate
    /// responses. The slab background is what keeps the block legible
    /// against the prose around it; whitespace is reserved for saying
    /// where one completion ends.
    #[test]
    fn a_blank_line_separates_turns_and_never_splits_a_completion() {
        let app = a_session_with_one_of_everything();
        let screen = Screen::chat(&app, 64, 26);
        let user = screen.find("how long is a.txt").expect("the question");
        assert_eq!(screen.inner(user - 1), "", "air above the person's turn");
        assert_eq!(
            screen.inner(user + 1),
            "",
            "and below it, before the answer"
        );

        // One response, one unbroken run: lid, source, prose, call,
        // closing prose, with the call in the middle of it rather than
        // fenced off from the words either side.
        let lid = screen.find("program:").expect("the block's lid");
        let call = screen.find("⚙ read_file").expect("the block's call");
        let last = screen
            .find("a.txt is 5 characters")
            .expect("the response's closing prose");
        assert!(lid < call && call < last, "the run is in reading order");
        for row in lid..=last {
            assert_ne!(
                screen.inner(row).trim(),
                "",
                "row {row} splits one completion:\n{}",
                screen.dump()
            );
        }
    }

    /// **A `tell` is speech, not a call.** It reaches the pane as its
    /// own `Send` and renders as prose on the pane's own ground —
    /// never inside the block, whose background it would otherwise
    /// take.
    #[test]
    fn a_tell_renders_as_prose_outside_the_block() {
        let app = a_session_with_one_of_everything();
        let screen = Screen::chat(&app, 64, 26);
        let said = screen
            .find("a.txt is 5 characters")
            .expect("the tell reached the person");
        assert_eq!(
            screen.backgrounds(said)[0].0,
            Color::Reset,
            "it is on the pane's ground, not in a slab:\n{}",
            screen.dump()
        );
    }

    /// **Selection is a gutter mark.** `REVERSED` meant three things at
    /// once, so two of them looked identical — and on a code row it
    /// swapped the slab for the foreground, which reads as damage
    /// rather than as a cursor.
    #[test]
    fn selection_marks_the_gutter_and_leaves_the_slab_alone() {
        let mut app = a_session_with_one_of_everything();
        let plain = Screen::chat(&app, 64, 26);
        let call_row = plain.find("⚙ read_file").expect("a call to select");
        let before = plain.backgrounds(call_row)[1].0;

        app.selected_program = Some(EventId::new(3));
        app.selected_effect = Some(Effect::Invoke(0));
        let screen = Screen::chat(&app, 64, 26);
        let y = screen.find("⚙ read_file").expect("still there");
        assert!(
            screen.inner(y).starts_with("▌"),
            "the mark is in the gutter: {:?}",
            screen.inner(y)
        );
        assert!(
            !screen.modifiers(y).contains(Modifier::REVERSED),
            "and nothing is inverted"
        );
        assert_eq!(screen.backgrounds(y)[1].0, before, "the slab is untouched");
    }

    /// **The mark costs the row nothing.** The gutter is a reserved
    /// column, not one taken when there is something to put in it, so a
    /// row's text is at the same x selected or not — a block used to
    /// shuffle a column sideways as the selection passed through it.
    #[test]
    fn selecting_a_row_does_not_move_it() {
        let column_of = |screen: &Screen, needle: char| {
            let y = screen.find("⚙ read_file").expect("a call to look at");
            screen.inner(y).chars().position(|c| c == needle)
        };
        let mut app = a_session_with_one_of_everything();
        let before = column_of(&Screen::chat(&app, 64, 26), '⚙');

        app.selected_program = Some(EventId::new(3));
        app.selected_effect = Some(Effect::Invoke(0));
        let screen = Screen::chat(&app, 64, 26);
        assert!(
            column_of(&screen, '▌').is_some(),
            "the row is marked:\n{}",
            screen.dump()
        );
        assert_eq!(
            column_of(&screen, '⚙'),
            before,
            "and its text did not move:\n{}",
            screen.dump()
        );
    }

    /// **The open block is the selected one**, end to end: the source a
    /// collapsed block hides is on the screen once you are looking at
    /// it, and gone again when you look elsewhere.
    #[test]
    fn selecting_a_block_opens_its_source() {
        let mut app = a_session_with_one_of_everything();
        let shut = Screen::chat(&app, 64, 26);
        assert!(
            shut.find("more line").is_some(),
            "the fixture's cell is long enough to fold:\n{}",
            shut.dump()
        );

        app.selected_program = Some(EventId::new(3));
        let open = Screen::chat(&app, 64, 26);
        assert!(
            open.find("more line").is_none(),
            "nothing is hidden, so nothing says so:\n{}",
            open.dump()
        );

        // And looking at something else shuts it, with no gesture of
        // its own — which is the whole point of dropping the sticky bit.
        app.selected_program = Some(EventId::new(99));
        assert!(Screen::chat(&app, 64, 26).find("more line").is_some());
    }

    /// Not an assertion — a picture, for changing how this looks.
    /// `cargo test -p agent look_at_the_chat_pane -- --ignored --nocapture`
    #[test]
    #[ignore = "prints the pane; it asserts nothing"]
    fn look_at_the_chat_pane() {
        let mut app = a_session_with_one_of_everything();
        for (kind, text, detail, id) in app.chat.rows(app.selected, 60, app.selected_program) {
            println!("ROW {kind:?} {detail:?} #{} {text:?}", id.as_u64());
        }
        println!("{}", Screen::chat(&app, 64, 26).dump());
        // Selected too, because selecting is what opens a block now —
        // the two states are one gesture apart and worth seeing together.
        app.selected_program = Some(EventId::new(3));
        println!("--- with the block selected:");
        println!("{}", Screen::chat(&app, 64, 26).dump());
        app.selected_program = None;
        app.busy = Some((
            "thinking".into(),
            Instant::now() - std::time::Duration::from_secs(47),
        ));
        println!("--- waiting on the model:");
        println!("{}", Screen::chat(&app, 64, 26).dump());
    }
    use crate::host::{
        ScriptedLlm, ToolDef, ToolRegistry, run_demo, scripted_markdown, scripted_program,
        scripted_text,
    };
    use crate::types::{EventId, Tree};
    use serde_json::json;
    use std::sync::mpsc::channel;

    fn fid(n: u64) -> BranchId {
        EventId::new(n)
    }

    /// A minimal `BranchInfo` for the branch-keyed helpers, with no
    /// pending ask.
    fn info(branch: BranchId) -> BranchInfo {
        BranchInfo {
            branch,
            agent: branch,
            leaf: branch,
            name: None,
            parent_branch: None,
            status: "idle".into(),
            open: 0,
            asking_user: None,
            thinking: false,
        }
    }

    /// The input line's default behaviour (18_TARGETING Part A, revised
    /// to drop Alt+Enter for `a`/`arm_ask`): a plain Enter tells, an
    /// armed one asks — both `UserTurn` — unless the branch has a
    /// pending ask-to-user, in which case either one replies.
    #[test]
    fn reply_mode_wins_over_ask_or_tell_when_a_branch_is_waiting_on_you() {
        let b = fid(1);
        let idle = [info(b)];
        assert_eq!(
            resolve_submit(&idle, b, "hi".into(), true),
            SessionCommand::UserTurn {
                branch: b,
                text: "hi".into(),
                expects_reply: true,
            }
        );
        assert_eq!(
            resolve_submit(&idle, b, "fyi".into(), false),
            SessionCommand::UserTurn {
                branch: b,
                text: "fyi".into(),
                expects_reply: false,
            }
        );
        let mut asking = info(b);
        asking.asking_user = Some(fid(7));
        assert_eq!(
            resolve_submit(&[asking], b, "42".into(), true),
            SessionCommand::Reply {
                branch: b,
                call: fid(7),
                value: json!("42"),
            }
        );
    }

    /// The `a` key arms an ask for the next Enter; a plain Enter tells
    /// by default, and arming does not leak into the turn after —
    /// exactly one bare reply's worth (18_TARGETING revision: a
    /// dedicated key, not a modifier on Enter, because Alt+Enter is not
    /// reliably delivered — see `ask_armed`'s doc).
    #[test]
    fn the_ask_key_arms_exactly_one_reply() {
        let mut app = AttachedApp::new(fid(1));
        for c in "fyi".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_input_key(KeyEvent::from(KeyCode::Enter)),
            KeyAction::Submit {
                text: "fyi".into(),
                expects_reply: false,
            }
        );

        app.focus = Focus::Debug;
        assert_eq!(
            app.on_debug_key(KeyCode::Char('a'), &[], None),
            KeyAction::None
        );
        assert_eq!(app.focus, Focus::Input);
        assert!(app.ask_armed);
        for c in "hi".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_input_key(KeyEvent::from(KeyCode::Enter)),
            KeyAction::Submit {
                text: "hi".into(),
                expects_reply: true,
            }
        );
        assert!(!app.ask_armed, "one reply, not a standing mode");

        for c in "next".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_input_key(KeyEvent::from(KeyCode::Enter)),
            KeyAction::Submit {
                text: "next".into(),
                expects_reply: false,
            }
        );
    }

    /// `Esc` on an empty line disarms an ask the same way it disarms an
    /// explicit mode — a change of mind costs nothing.
    #[test]
    fn esc_disarms_the_ask_key() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        app.on_debug_key(KeyCode::Char('a'), &[], None);
        assert!(app.ask_armed);
        app.on_input_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.ask_armed);
        assert_eq!(app.focus, Focus::Debug);
    }

    /// The restart keys arm an explicit mode; typing and Enter submit it
    /// as the right command, and `Esc` on an empty line disarms it
    /// without submitting anything.
    #[test]
    fn restart_keys_arm_an_explicit_mode_and_submit_the_right_command() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_debug_key(KeyCode::Char('r'), &[], None),
            KeyAction::None
        );
        assert_eq!(app.explicit_mode, Some(ExplicitMode::Rename));
        assert_eq!(app.focus, Focus::Input);
        for c in "researcher".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_input_key(KeyEvent::from(KeyCode::Enter)),
            KeyAction::SubmitMode(ExplicitMode::Rename, "researcher".into())
        );
        assert_eq!(app.explicit_mode, None, "cleared on submit");

        app.focus = Focus::Debug;
        app.on_debug_key(KeyCode::Char('v'), &[], Some("suspended"));
        assert_eq!(app.explicit_mode, Some(ExplicitMode::ResumeWithValue));
        app.on_input_key(KeyEvent::from(KeyCode::Esc)); // Esc on empty input: disarm
        assert_eq!(app.explicit_mode, None);
        assert_eq!(app.focus, Focus::Debug);

        let branch = fid(1);
        assert_eq!(
            resolve_submit_mode(ExplicitMode::Rename, branch, "researcher".into()),
            SessionCommand::Rename {
                branch,
                name: "researcher".into(),
            }
        );
        assert_eq!(
            resolve_submit_mode(ExplicitMode::ResumeWithValue, branch, "5".into()),
            SessionCommand::Restart {
                branch,
                source: "history.note(resume(5));".into(),
            }
        );
        assert_eq!(
            resolve_submit_mode(ExplicitMode::ResumeWithValue, branch, "not json".into()),
            SessionCommand::Restart {
                branch,
                source: "history.note(resume(\"not json\"));".into(),
            },
            "a non-JSON value falls back to a bare string"
        );
        assert_eq!(
            resolve_submit_mode(ExplicitMode::Rewrite, branch, "return 1;".into()),
            SessionCommand::Restart {
                branch,
                source: "return 1;".into(),
            }
        );
        assert_eq!(
            resolve_submit_mode(ExplicitMode::SpawnCharter, branch, "read files".into()),
            SessionCommand::Spawn {
                parent: branch,
                name: None,
                charter: "read files".into(),
                text: None,
            }
        );
    }

    /// `v` only arms Resume when the selected branch is actually
    /// suspended — everywhere else it's a wasted round trip through
    /// `cmd_restart` that comes back refused (19_UX Step F1).
    #[test]
    fn v_only_arms_resume_on_a_suspended_branch() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_debug_key(KeyCode::Char('v'), &[], Some("suspended")),
            KeyAction::None
        );
        assert_eq!(app.explicit_mode, Some(ExplicitMode::ResumeWithValue));

        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_debug_key(KeyCode::Char('v'), &[], Some("idle")),
            KeyAction::None
        );
        assert_eq!(app.explicit_mode, None, "idle isn't resumable");

        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_debug_key(KeyCode::Char('v'), &[], None),
            KeyAction::None
        );
        assert_eq!(app.explicit_mode, None, "nothing selected isn't resumable");
    }

    /// `f`/`x`/`w` map to the right `KeyAction`, and `f` without a
    /// selected message falls back to fork-here (19_UX Step C2: one
    /// key carries both of the old `f`/`F` pair's meanings).
    #[test]
    fn fork_interrupt_and_jump_keys() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_debug_key(KeyCode::Char('f'), &[], None),
            KeyAction::Fork
        );
        app.last_clicked_event = Some(fid(42));
        assert_eq!(
            app.on_debug_key(KeyCode::Char('f'), &[], None),
            KeyAction::ForkAt(fid(42))
        );
        assert_eq!(
            app.on_debug_key(KeyCode::Char('x'), &[], Some("running")),
            KeyAction::Interrupt
        );
        assert_eq!(
            app.on_debug_key(KeyCode::Char('w'), &[], None),
            KeyAction::JumpToWaiting
        );
    }

    /// `x` only fires Interrupt on a live, non-idle branch — `Idle` is a
    /// genuine no-op in `Runner::interrupt`, and `dormant` short-circuits
    /// in `cmd_interrupt` before it even runs, so both stay silently
    /// inert rather than a wasted (if harmless) round trip (19_UX Step
    /// F2).
    #[test]
    fn x_only_interrupts_a_live_non_idle_branch() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        for status in ["running", "thinking", "suspended"] {
            assert_eq!(
                app.on_debug_key(KeyCode::Char('x'), &[], Some(status)),
                KeyAction::Interrupt,
                "{status} should be interruptible"
            );
        }
        for status in [Some("idle"), Some("dormant"), None] {
            assert_eq!(
                app.on_debug_key(KeyCode::Char('x'), &[], status),
                KeyAction::None,
                "{status:?} should not be interruptible"
            );
        }
    }

    /// A branch waiting on you renders its question above the input line
    /// and switches Enter to reply mode (17_BRANCHES Part D).
    #[test]
    fn a_pending_ask_to_user_shows_above_the_input_as_reply_mode() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "sys", Vec::new())
            .unwrap();
        let send = tree
            .append(
                &mut spine,
                EventPayload::Call(crate::types::Call::Send {
                    prose: false,
                    to: crate::types::Address::User,
                    text: "which file?".into(),
                    input: json!(null),
                    options: Vec::new(),
                    expects_reply: true,
                    site: 0,
                    site_end: 0,
                }),
            )
            .unwrap();
        let (tx, _rx) = channel();
        let session = Session::open_at(
            tree,
            spine.leaf_id,
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([])),
            tx,
        )
        .unwrap();

        let branch = session.conversation_branch();
        assert_eq!(
            asking_question_text(&session, branch),
            Some("which file?".into())
        );

        // And the resolved command is a Reply naming that Send, not a
        // fresh UserTurn — the input line's default behaviour.
        let infos = session.branch_infos();
        assert_eq!(
            resolve_submit(&infos, branch, "PLAN.md".into(), true),
            SessionCommand::Reply {
                branch,
                call: send,
                value: json!("PLAN.md"),
            }
        );
    }

    /// A chat reference like "see agent 7" or "Agent #12 is stuck" names
    /// the branch it points at; unrelated text names none.
    #[test]
    fn agent_references_in_prose_are_recognized() {
        assert_eq!(agent_reference_in("ask agent 7 about it"), Some(7));
        assert_eq!(agent_reference_in("Agent #12 is stuck"), Some(12));
        assert_eq!(agent_reference_in("no reference here"), None);
        assert_eq!(agent_reference_in("agent alone, no number"), None);
    }

    /// A line longer than the pane spans multiple rows instead of being
    /// clipped at the border — `ratatui::Paragraph` clips by default, and
    /// the chat pane's manual scroll math means its own `.wrap()` isn't
    /// safe to reach for (one entry in `lines` must stay one scrollable
    /// row), so `render_chat` wraps before that math ever runs.
    #[test]
    fn long_lines_wrap_instead_of_clipping() {
        let mut lines = Vec::new();
        let style = Style::default();
        let long = "error: the request to the remote server timed out after \
                    thirty seconds without a response from the host machine";
        push_wrapped_width(&mut lines, long, style, 20);
        assert!(lines.len() > 1, "a long line at width 20 must wrap");
        for line in &lines {
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(width <= 20, "row exceeds wrap width: {width} > 20");
        }
        // Concatenating the rows recovers every word — nothing was
        // dropped, only rebroken.
        let rejoined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            rejoined.split_whitespace().collect::<Vec<_>>(),
            long.split_whitespace().collect::<Vec<_>>()
        );

        // A blank line still pushes exactly one (empty) row, not zero —
        // intentional spacing in the transcript must survive.
        let mut blank = Vec::new();
        push_wrapped_width(&mut blank, "", style, 20);
        assert_eq!(blank.len(), 1);
    }

    /// Every explicit mode gets its own title — arming any of them is
    /// as visible as arming an ask already was (19_UX Step C0).
    #[test]
    fn every_explicit_mode_has_a_distinct_title() {
        let titles = [
            explicit_mode_title(ExplicitMode::Rename),
            explicit_mode_title(ExplicitMode::ResumeWithValue),
            explicit_mode_title(ExplicitMode::Rewrite),
            explicit_mode_title(ExplicitMode::SpawnCharter),
        ];
        for title in titles {
            assert!(title.starts_with(' ') && title.ends_with(' '), "{title}");
        }
        let unique: std::collections::HashSet<_> = titles.iter().collect();
        assert_eq!(unique.len(), titles.len(), "no two modes share a title");
    }

    #[test]
    fn footer_hint_shows_v_resume_only_when_resumable() {
        let with = footer_hint(View::Chat, Focus::Debug, false, false, true, false, false);
        assert!(with.contains("v resume"), "{with}");

        let without = footer_hint(View::Chat, Focus::Debug, false, false, false, false, false);
        assert!(!without.contains("v resume"), "{without}");

        let running_with = footer_hint(
            View::Running,
            Focus::Debug,
            false,
            false,
            true,
            false,
            false,
        );
        assert!(running_with.contains("v resume"), "{running_with}");

        let running_without = footer_hint(
            View::Running,
            Focus::Debug,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(!running_without.contains("v resume"), "{running_without}");
    }

    #[test]
    fn footer_hint_shows_x_interrupt_only_when_interruptible() {
        let with = footer_hint(View::Chat, Focus::Debug, false, false, false, true, false);
        assert!(with.contains("x interrupt"), "{with}");

        let without = footer_hint(View::Chat, Focus::Debug, false, false, false, false, false);
        assert!(!without.contains("x interrupt"), "{without}");

        let running_with = footer_hint(
            View::Running,
            Focus::Debug,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(running_with.contains("x interrupt"), "{running_with}");

        let running_without = footer_hint(
            View::Running,
            Focus::Debug,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(
            !running_without.contains("x interrupt"),
            "{running_without}"
        );
    }

    /// `wrap_input` places the cursor glyph exactly where wrapping
    /// would put a real character there, and reports which rendered
    /// row holds it — the math `render_chat` leans on for scrolling.
    #[test]
    fn wrap_input_places_the_cursor_row_correctly() {
        // Short, no wrap needed: one row, cursor at the end.
        let mut buf = InputBuffer::new();
        for c in "hi".chars() {
            buf.insert_char(c);
        }
        let (rows, cursor_row) = wrap_input(&buf, true, 20);
        assert_eq!(rows, vec!["❯ hi▏".to_owned()]);
        assert_eq!(cursor_row, 0);

        // Multi-line: continuation rows get the two-space alignment
        // prefix, and the cursor row tracks which buffer line it's on.
        let mut buf = InputBuffer::new();
        for c in "one".chars() {
            buf.insert_char(c);
        }
        buf.insert_newline();
        for c in "two".chars() {
            buf.insert_char(c);
        }
        let (rows, cursor_row) = wrap_input(&buf, true, 20);
        assert_eq!(rows, vec!["❯ one".to_owned(), "  two▏".to_owned()]);
        assert_eq!(cursor_row, 1);

        // `show_cursor: false` — no glyph anywhere, and no panic
        // finding a row for one.
        let (rows, cursor_row) = wrap_input(&buf, false, 20);
        assert_eq!(rows, vec!["❯ one".to_owned(), "  two".to_owned()]);
        assert_eq!(cursor_row, 0);

        // A line that actually wraps: inserting the glyph mid-word can
        // shift which words share a row (here "cccc dddd" fits one row
        // of width 9 on its own, but "cc▏cc dddd" no longer does), so
        // the cursor must land on whichever row it actually ends up
        // on, not just "the row the un-marked text would wrap to."
        let mut buf = InputBuffer::prefilled("aaaa bbbb cccc dddd");
        for _ in 0..12 {
            buf.right();
        }
        let (rows, cursor_row) = wrap_input(&buf, true, 9);
        assert_eq!(
            rows,
            vec![
                "❯ aaaa bbbb".to_owned(),
                "  cc▏cc".to_owned(),
                "  dddd".to_owned()
            ]
        );
        assert_eq!(cursor_row, 1);
    }

    #[test]
    fn scroll_top_following_follows_the_index_past_the_bottom() {
        // 10 rows, 4 visible: the cursor starts in view, so no scroll yet.
        assert_eq!(scroll_top_following(0, 10, 4), 0);
        assert_eq!(scroll_top_following(3, 10, 4), 0);

        // Moved past the bottom of the window: `top` advances just enough
        // to keep the cursor's row inside `top..top+visible`.
        assert_eq!(scroll_top_following(4, 10, 4), 1);
        assert_eq!(scroll_top_following(9, 10, 4), 6);

        // Never scrolls past the point where the window would run off
        // the end of the rows.
        assert_eq!(scroll_top_following(9, 10, 20), 0);
    }

    /// `next_waiting` cycles from the current branch, wraps around, and
    /// skips branches that owe you nothing.
    #[test]
    fn next_waiting_cycles_and_wraps() {
        let mut a = info(fid(1));
        let mut b = info(fid(2));
        let c = info(fid(3));
        a.asking_user = Some(fid(10));
        b.asking_user = Some(fid(11));
        let rows = vec![a, b, c];
        assert_eq!(next_waiting(&rows, fid(1)), Some(fid(2)));
        // From the last asker, wrap around past the non-asker back to the first.
        assert_eq!(next_waiting(&rows, fid(2)), Some(fid(1)));
        assert_eq!(next_waiting(&rows, fid(3)), Some(fid(1)));
    }

    /// Drive the layout with the real M0 scripted demo's events.
    #[test]
    fn m0_run_program_auto_pops_and_sticks() {
        let (tx, rx) = channel();
        let session = run_demo(Tree::new(None), tx).unwrap();
        let mut app = AttachedApp::new(session.conversation_branch());

        let mut popped_while_program_visible = false;
        for event in rx.try_iter() {
            app.apply(&event);
            if app.view == View::Running {
                popped_while_program_visible = true;
            }
        }
        assert!(popped_while_program_visible);
        // Sticky: the program completed and the agent finished, but the
        // panes remain for post-mortem reading.
        assert_eq!(app.view, View::Running);
        let panes = app.pane_set();
        assert!(panes.chat);
        // The auto-pop set is detail + console now; the source pane is
        // off unless `1` asks for it.
        assert!(panes.right.contains(&Pane::Detail));
        assert!(!panes.right.contains(&Pane::Console));
        assert!(!panes.right.contains(&Pane::Source));
        // The post-mortem VM is still borrowable for those panes.
        let state = session.state(session.conversation_branch()).unwrap();
        assert!(!state.vm_is_live());
        assert!(state.vm().is_some(), "final program state kept");

        // The collapse key restores full-width chat.
        app.on_key(KeyCode::Esc.into(), &[], None); // input → debug focus
        assert_eq!(app.focus, Focus::Debug);
        app.on_key(KeyCode::Char('c').into(), &[], None);
        assert_eq!(app.view, View::Chat);
        assert_eq!(
            app.pane_set(),
            PaneSet {
                chat: true,
                console_left: false,
                right: vec![Pane::Navigator]
            }
        );
        // It is not a one-way door: `c` again from `Chat` reopens the
        // detail pane without needing a fresh run_program to
        // re-trigger the auto-pop. Collapsing left focus on `Input`
        // (same as the original direction leaves it), so `Esc` back to
        // `Focus::Debug` first, same as above.
        app.on_key(KeyCode::Esc.into(), &[], None);
        assert_eq!(app.focus, Focus::Debug);
        app.on_key(KeyCode::Char('c').into(), &[], None);
        assert_eq!(app.view, View::Running);
        let panes = app.pane_set();
        assert!(panes.right.contains(&Pane::Detail));
    }

    #[test]
    fn full_debugger_mode_swaps_and_returns_without_session_actions() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running;
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_key(KeyCode::Char('d').into(), &[fid(1)], None),
            KeyAction::None
        );
        assert_eq!(app.view, View::FullDebug);
        let panes = app.pane_set();
        assert!(!panes.chat, "chat hidden in full debugger mode");
        assert!(panes.console_left);
        assert_eq!(
            panes.right,
            vec![
                Pane::Navigator,
                Pane::Source,
                Pane::Disasm,
                Pane::Stack,
                Pane::Promises
            ]
        );
        assert_eq!(
            app.on_key(KeyCode::Char('d').into(), &[fid(1)], None),
            KeyAction::None
        );
        assert_eq!(app.view, View::Running, "returns to the previous view");
    }

    /// Arming rewrite prefills the buffer with the current program's
    /// source — editing what's actually there, not a blank line
    /// (19_UX Step B1) — cursor at the top for reviewing from the
    /// start.
    #[test]
    fn arming_rewrite_prefills_from_the_current_program() {
        let (tx, _rx) = channel();
        let session = run_demo(Tree::new(None), tx).unwrap();
        // `resolve_program` reads the log/tree directly — `app` only
        // needs to know which branch, not to have replayed events.
        let app = AttachedApp::new(session.conversation_branch());
        let (_, pv) = resolve_program(&app, &session);
        let source = pv.expect("the demo ran a program").source;

        let buf = resolve_rewrite_prefill(&app, &session);
        assert_eq!(buf.to_string(), source);
        assert_eq!(buf.cursor(), (0, 0));
    }

    /// A branch that has never run a program has nothing to resolve —
    /// arms with an empty buffer, same as today's blank-line start.
    #[test]
    fn arming_rewrite_with_nothing_to_resolve_is_empty() {
        let (tx, _rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "idle agent",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([])),
            tx,
        )
        .unwrap();
        let app = AttachedApp::new(session.conversation_branch());
        let buf = resolve_rewrite_prefill(&app, &session);
        assert!(buf.is_empty());
    }

    /// **An append is on the same ground as the calls.** Both are
    /// effects attached below a block's source, and one of them sitting
    /// on the pane's own background read as a stray line beside the
    /// block rather than part of it.
    #[test]
    fn an_append_shares_the_attached_slab_with_calls() {
        let program = EventId::new(3);
        assert_eq!(
            slab_of(
                ChatKind::Program,
                &RowDetail::Note(program, EventId::new(9))
            ),
            slab_of(ChatKind::Program, &RowDetail::Invoke(program, 0)),
        );
        assert_eq!(
            slab_of(
                ChatKind::Program,
                &RowDetail::Note(program, EventId::new(9))
            ),
            Some(SLAB_ATTACHED),
        );
    }

    #[test]
    fn manual_toggles_override_auto_pop_set() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running;
        app.focus = Focus::Debug;
        // The source pane is off in the chat views now — `Detail` has
        // its slot — and `1` is what brings it back.
        assert!(!app.pane_set().right.contains(&Pane::Source));
        assert!(app.pane_set().right.contains(&Pane::Detail));
        app.on_key(KeyCode::Char('1').into(), &[], None);
        assert!(app.pane_set().right.contains(&Pane::Source));
        app.on_key(KeyCode::Char('1').into(), &[], None);
        assert!(!app.pane_set().right.contains(&Pane::Source));
        app.on_key(KeyCode::Char('3').into(), &[], None);
        assert!(app.pane_set().right.contains(&Pane::Stack));
    }

    #[test]
    fn typing_is_free_and_submit_goes_through_commands() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running; // digits must still type, not toggle
        for c in "d1 sq".chars() {
            assert_eq!(
                app.on_key(KeyCode::Char(c).into(), &[], None),
                KeyAction::None
            );
        }
        assert_eq!(app.input.to_string(), "d1 sq");
        assert_eq!(app.view, View::Running, "no debug keys fired while typing");
        assert!(!app.quit);
        assert_eq!(
            app.on_key(KeyCode::Enter.into(), &[], None),
            KeyAction::Submit {
                text: "d1 sq".into(),
                expects_reply: false,
            }
        );
        assert!(app.input.is_empty());
    }

    /// `Up` walks back through submitted messages, newest first,
    /// stashing whatever was being drafted; `Down` walks back forward
    /// and, past the newest entry, restores that stashed draft.
    #[test]
    fn arrow_keys_walk_message_history_at_the_buffer_edges() {
        let mut app = AttachedApp::new(fid(1));
        for text in ["first", "second"] {
            for c in text.chars() {
                app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
            }
            app.on_input_key(KeyEvent::from(KeyCode::Enter));
        }
        for c in "draft".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }

        app.on_input_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(
            app.input.to_string(),
            "second",
            "Up recalls the newest entry first"
        );
        app.on_input_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.input.to_string(), "first");
        app.on_input_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(
            app.input.to_string(),
            "first",
            "no further history: stays put"
        );

        app.on_input_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.input.to_string(), "second");
        app.on_input_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(
            app.input.to_string(),
            "draft",
            "Down past the newest entry restores the stashed draft"
        );
        app.on_input_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(
            app.input.to_string(),
            "draft",
            "nothing further to go forward to"
        );
    }

    /// Mid-buffer, Up/Down move the cursor within the current draft
    /// instead of touching history — even when history is non-empty.
    #[test]
    fn arrow_keys_move_the_cursor_before_touching_history_mid_buffer() {
        let mut app = AttachedApp::new(fid(1));
        for c in "old".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_input_key(KeyEvent::from(KeyCode::Enter));

        for c in "line one".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_input_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        for c in "line two".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.input.cursor(), (1, 8));

        app.on_input_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(
            app.input.cursor().0,
            0,
            "moved within the buffer, not into history"
        );
        assert_eq!(
            app.input.to_string(),
            "line one\nline two",
            "history untouched"
        );
    }

    /// `on_input_key` reaches the right `InputBuffer` method for a
    /// representative few of the readline/emacs bindings — the buffer's
    /// own logic is exhaustively covered in `debug::input::tests`, this
    /// only proves the wiring. `key.code == KeyCode::Char(_)` is how
    /// crossterm reports every one of these, with the modifier riding
    /// in `key.modifiers`, so the plain-character catch-all must not
    /// swallow them.
    #[test]
    fn readline_bindings_reach_the_buffer() {
        let mut app = AttachedApp::new(fid(1));
        for c in "hello".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_input_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.on_input_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty(), "Ctrl-A home, Ctrl-K kill-to-end");

        for c in "foo bar".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_input_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(
            app.input.to_string(),
            "foo ",
            "Ctrl-W deletes the word behind the cursor"
        );

        app.on_input_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        app.on_input_key(KeyEvent::from(KeyCode::Char('!')));
        assert_eq!(
            app.input.to_string(),
            "foo \n!",
            "Ctrl-O inserts a literal newline, not a submit"
        );
    }

    /// Ctrl-C: clears a non-empty input line first (a change of mind,
    /// not an exit) and only quits once the line is already empty. Fires
    /// through `on_key`, so it works regardless of focus/view — same
    /// "everywhere" category as Tab, checked right beside it.
    #[test]
    fn ctrl_c_clears_input_then_quits_on_empty() {
        let mut app = AttachedApp::new(fid(1));
        for c in "hello".chars() {
            app.on_input_key(KeyEvent::from(KeyCode::Char(c)));
        }
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c, &[], None), KeyAction::None);
        assert!(app.input.is_empty(), "first Ctrl-C clears, does not quit");
        assert!(!app.quit);

        // Nothing to clear and nothing running: arms, does not quit.
        assert_eq!(app.on_key(ctrl_c, &[], None), KeyAction::None);
        assert!(
            !app.quit,
            "an empty line arms the quit, it does not take it"
        );
        assert!(app.quit_armed);

        assert_eq!(app.on_key(ctrl_c, &[], None), KeyAction::None);
        assert!(app.quit, "the second Ctrl-C quits");
    }

    #[test]
    /// The middle rung, and the reason the ladder exists: Ctrl-C is what
    /// people press to stop what is happening, and it used to close the
    /// session instead. Interrupt was reachable only through `x`.
    fn ctrl_c_interrupts_a_live_branch_before_it_ever_quits() {
        let mut app = AttachedApp::new(fid(1));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for status in ["running", "thinking", "suspended"] {
            assert_eq!(
                app.on_key(ctrl_c, &[], Some(status)),
                KeyAction::Interrupt,
                "{status} is interruptible"
            );
            assert!(
                !app.quit,
                "{status}: never quits while there is a program to stop"
            );
            assert!(
                !app.quit_armed,
                "{status}: interrupting does not arm a quit"
            );
        }
    }

    #[test]
    /// A pending quit is about the very next keystroke. Anything else
    /// disarms it, so nobody ends up in a confirm state they cannot see.
    fn any_other_key_disarms_a_pending_quit() {
        let mut app = AttachedApp::new(fid(1));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.on_key(ctrl_c, &[], None);
        assert!(app.quit_armed);

        // Esc, not a printable: with input focus a character would be
        // typed, and the next Ctrl-C would then clear the line rather
        // than reaching the quit rung at all.
        app.on_key(KeyEvent::from(KeyCode::Esc), &[], None);
        assert!(!app.quit_armed, "another key disarms");

        app.on_key(ctrl_c, &[], None);
        assert!(
            !app.quit,
            "so the next Ctrl-C arms again rather than quitting"
        );
        assert!(app.quit_armed);
    }

    /// A `run_program` header colors by its own status word: red once
    /// failed, green once completed, unstyled (`None`) while still
    /// running or suspended — nothing to signal yet either way.
    #[test]
    fn program_header_severity_colors_failed_red_and_completed_green() {
        assert_eq!(
            program_header_severity("run_program: failed"),
            Some(Color::Red)
        );
        assert_eq!(
            program_header_severity("run_program: completed"),
            Some(Color::Green)
        );
        assert_eq!(program_header_severity("run_program: running"), None);
        assert_eq!(program_header_severity("run_program: suspended"), None);
    }

    /// Ctrl-T shows/hides reasoning content — a display toggle, hidden
    /// by default, that fires through `on_key` regardless of focus,
    /// same "everywhere" category as Ctrl-C right above.
    #[test]
    fn ctrl_t_toggles_thinking_visibility_from_either_focus() {
        let mut app = AttachedApp::new(fid(1));
        assert!(!app.show_thinking, "hidden by default");
        let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);

        app.focus = Focus::Input;
        assert_eq!(app.on_key(ctrl_t, &[], None), KeyAction::None);
        assert!(app.show_thinking);

        app.focus = Focus::Debug;
        assert_eq!(app.on_key(ctrl_t, &[], None), KeyAction::None);
        assert!(!app.show_thinking);
    }

    /// `m` toggles raw view, but only from `Focus::Debug` — unlike
    /// Ctrl-T's global "everywhere" gesture, a bare `m` is an ordinary
    /// character the input box must still be free to type.
    #[test]
    fn m_toggles_markdown_only_from_debug_focus() {
        let mut app = AttachedApp::new(fid(1));
        assert!(app.show_markdown, "on by default");
        let m = KeyEvent::from(KeyCode::Char('m'));

        app.focus = Focus::Input;
        app.on_key(m, &[], None);
        assert!(
            app.show_markdown,
            "typing in the input box must not toggle it"
        );
        assert_eq!(app.input.to_string(), "m", "and the letter is typed");

        app.input.clear();
        app.focus = Focus::Debug;
        app.on_key(m, &[], None);
        assert!(!app.show_markdown);
        app.on_key(m, &[], None);
        assert!(app.show_markdown);
    }

    /// With markdown off, `render_chat` reads `rows_raw` — a heading's
    /// A table's `|---|---|` delimiter row is deliberately dropped when
    /// markdown IS on (it's punctuation, not data) — but with raw view on,
    /// nothing in the pipeline may touch it: it must reach the real
    /// terminal buffer exactly as the model wrote it, dashes and all.
    #[test]
    fn raw_view_shows_the_table_delimiter_row_literally_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_markdown(
                "| Name | Role |\n|------|------|\n| Ada | Engineer |",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        // Sanity check on the data model first: with markdown ON, the
        // delimiter row is gone by design.
        let classified = app.chat.rows(app.selected, 80, app.selected_program);
        assert!(
            !classified.iter().any(|(_, t, _, _)| t.contains("------")),
            "the delimiter row is dropped when markdown rendering is on"
        );

        app.show_markdown = false;
        let raw = app.chat.rows_raw(app.selected, 80, app.selected_program);
        assert!(
            raw.iter().any(|(_, t, _, _)| t.contains("------")),
            "but must survive verbatim in the raw row data: {raw:?}"
        );

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut delimiter_on_screen = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let run: String = (0..6)
                    .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                    .collect();
                if run == "------" {
                    delimiter_on_screen = true;
                }
            }
        }
        assert!(
            delimiter_on_screen,
            "the delimiter row must reach the actual terminal cells in raw view"
        );
    }

    /// `#` marker must survive to the real terminal buffer, unstripped.
    #[test]
    fn raw_view_shows_the_heading_marker_literally_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_text("# Heading\nplain text")])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }
        app.show_markdown = false;

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut marker_kept = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let marker: String = (0..9)
                    .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                    .collect();
                if marker == "# Heading" {
                    marker_kept = true;
                }
            }
        }
        assert!(
            marker_kept,
            "raw view must show the literal \"# \" marker, unstripped"
        );
    }

    /// The markdown wiring in `render_chat` (as opposed to
    /// `markdown::inline_spans` in isolation, which `debug::markdown`'s
    /// own tests already cover) was never actually exercised end to
    /// end: every other `render_chat` test only checks its pure helpers
    /// (`push_wrapped_width`, `chat_style`, ...), never a real draw. Draw
    /// into a `TestBackend` and read the terminal cells back to prove an
    /// assistant reply's `**bold**` actually reaches the screen styled.
    #[test]
    fn assistant_markdown_renders_bold_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_text("plain **bold** word")])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut bold_found = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let word: String = (0..4)
                    .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                    .collect();
                if word == "bold" {
                    let cell = buffer.cell((x, y)).unwrap();
                    if cell.modifier.contains(Modifier::BOLD) {
                        bold_found = true;
                    }
                }
            }
        }
        assert!(
            bold_found,
            "\"bold\" must render with the BOLD modifier somewhere in the pane; \
             buffer:\n{}",
            (0..buffer.area.height)
                .map(|y| (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Same real-draw technique as `assistant_markdown_renders_bold_in_the_
    /// real_chat_pane`, for a `# Heading` line: it must reach the screen
    /// bold, not as a literal `#`.
    #[test]
    fn heading_renders_bold_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_markdown(
                "# Heading\nplain text",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut heading_bold = false;
        let mut marker_kept = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let word: String = (0..7)
                    .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                    .collect();
                if word == "Heading"
                    && buffer
                        .cell((x, y))
                        .unwrap()
                        .modifier
                        .contains(Modifier::BOLD)
                {
                    heading_bold = true;
                }
                let marker: String = (0..9)
                    .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                    .collect();
                if marker == "# Heading" {
                    marker_kept = true;
                }
            }
        }
        assert!(heading_bold, "\"Heading\" must render bold");
        assert!(
            !marker_kept,
            "the \"# \" marker must be stripped from the heading text"
        );
    }

    /// A fenced code line's background must differ from a plain row's —
    /// proof the flat `Code` style (not the even/odd-alternated prose
    /// style) actually reaches the screen.
    #[test]
    fn fenced_code_line_has_a_distinct_background_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_markdown(
                "plain line\n```\ncode line\n```",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let bg_at_word = |word: &str| {
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let found: String = (0..word.chars().count() as u16)
                        .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                        .collect();
                    if found == word {
                        return Some(buffer.cell((x, y)).unwrap().bg);
                    }
                }
            }
            None
        };
        let plain_bg = bg_at_word("plain").expect("the plain line renders");
        let code_bg = bg_at_word("code").expect("the code line renders");
        assert_ne!(
            plain_bg, code_bg,
            "a fenced code line must not share the plain row's background"
        );
    }

    /// A table header cell renders bold, a body cell does not — same
    /// real-draw technique as the heading/fence tests above.
    #[test]
    fn table_header_row_renders_bold_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_markdown(
                "| Name | Role |\n|------|------|\n| Ada | Engineer |",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let bold_at_word = |word: &str| {
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let found: String = (0..word.chars().count() as u16)
                        .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                        .collect();
                    if found == word {
                        return Some(
                            buffer
                                .cell((x, y))
                                .unwrap()
                                .modifier
                                .contains(Modifier::BOLD),
                        );
                    }
                }
            }
            None
        };
        assert_eq!(
            bold_at_word("Name"),
            Some(true),
            "the header cell must render bold"
        );
        assert_eq!(
            bold_at_word("Ada"),
            Some(false),
            "a body cell must not render bold"
        );
    }

    /// Every `│` in the pane — the frame borders and the ones inside a
    /// header/body row alike — must share one color, never picking up a
    /// header row's bold or a body row's own tint. Before `table_row_spans`
    /// existed, a row was one uniformly-styled string, so a header's `│`
    /// rendered bold right along with "Name".
    #[test]
    fn every_table_border_char_shares_one_color() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_text(
                "| Name | Role |\n|------|------|\n| Ada | Engineer |",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        // Scoped to rows the table itself occupies — the pane chrome
        // (`Block::bordered()`) uses these same glyphs at the terminal's
        // own edges, which would otherwise contaminate the sample.
        let row_of = |word: &str| -> u16 {
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let found: String = (0..word.chars().count() as u16)
                        .filter_map(|i| buffer.cell((x + i, y)).map(|c| c.symbol().to_owned()))
                        .collect();
                    if found == word {
                        return y;
                    }
                }
            }
            panic!("{word:?} not found in the rendered buffer");
        };
        // Excludes x=0 and the last column: the *pane's* own
        // `Block::bordered()` draws `│` at the transcript area's own
        // left/right edges on every row height, including these — a
        // second, unrelated source of the same glyph this test must not
        // sample from.
        let border_cells_in_row = |y: u16| -> Vec<(Color, Modifier)> {
            (1..buffer.area.width - 1)
                .filter_map(|x| {
                    let cell = buffer.cell((x, y))?;
                    (cell.symbol() == "│").then_some((cell.fg, cell.modifier))
                })
                .collect()
        };
        let mut border_cells = border_cells_in_row(row_of("Name"));
        border_cells.extend(border_cells_in_row(row_of("Ada")));
        assert!(
            border_cells.len() >= 4,
            "expected │ cells in both the header and body rows: {border_cells:?}"
        );
        let (first_fg, first_mod) = border_cells[0];
        assert!(
            border_cells
                .iter()
                .all(|(fg, m)| *fg == first_fg && *m == first_mod),
            "every │ must share one color/style, header row and body row alike: {border_cells:?}"
        );
        assert!(
            !first_mod.contains(Modifier::BOLD),
            "│ must not inherit the header row's bold"
        );
    }

    /// The user's exact reported bug, through the real render pipeline
    /// this time (not just the `chat.rs` data layer): a table whose
    /// natural width overflows a realistic pane used to fall through to
    /// `push_wrapped_width`, which has no notion of box-drawing and
    /// treats an unbroken border line as one giant unsplittable "word" —
    /// clipped mid-frame by `Paragraph`'s no-wrap rendering, corners
    /// never reaching the screen. Now that `rows()` pre-fits the table to
    /// `wrap_width` itself, every corner must actually render.
    #[test]
    fn wide_table_renders_a_complete_unclipped_frame_in_the_real_chat_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "say hi",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_text(
                "| Path | What it is |\n\
                 |---|---|\n\
                 | `agent/` | The agent crate (`src/`, `samples`) |\n\
                 | `interp/` | The JS interpreter/VM crate (`src/`, `docs/`) |\n\
                 | `conformance/` | Conformance suite: `tests/`, `expectations.json`, \
                 custom `harness/` |",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "go".into(),
            expects_reply: true,
        });
        let session = session.run();

        let mut app = AttachedApp::new(session.conversation_branch());
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat(frame, &app, area, None, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let contains = |glyph: &str| {
            (0..buffer.area.height).any(|y| {
                (0..buffer.area.width).any(|x| buffer.cell((x, y)).unwrap().symbol() == glyph)
            })
        };
        for glyph in ["┌", "┐", "├", "┤", "┼", "└", "┘"] {
            assert!(
                contains(glyph),
                "frame glyph {glyph:?} never reached the screen — the row that carries \
                 it was clipped instead of fitting the pane"
            );
        }
    }

    /// With nothing else to cycle to, Tab must not fall through to
    /// `select_branch` on the branch already selected — that would
    /// silently reset the program/subitem selection and every pane's
    /// scroll position for no reason.
    #[test]
    fn tab_with_one_branch_is_a_true_no_op() {
        let mut app = AttachedApp::new(fid(1));
        app.selected_program = Some(fid(7));
        app.source_scroll = Some(3);
        app.on_key(KeyCode::Tab.into(), &[fid(1)], None);
        assert_eq!(app.selected, Some(fid(1)));
        assert_eq!(app.selected_program, Some(fid(7)), "not reset");
        assert_eq!(app.source_scroll, Some(3), "not reset");
    }

    /// Switching branches disarms whatever mode/target was armed —
    /// firing it against a branch you didn't mean is the `e` bug's
    /// shape — but leaves a typed draft alone, since losing that on a
    /// context change would be worse than the risk it guards against.
    #[test]
    fn selecting_a_branch_disarms_but_keeps_the_draft() {
        let mut app = AttachedApp::new(fid(1));
        app.explicit_mode = Some(ExplicitMode::Rewrite);
        app.ask_armed = true;
        app.last_clicked_event = Some(fid(42));
        for c in "half-typed".chars() {
            app.input.insert_char(c);
        }
        app.select_branch(fid(2));
        assert_eq!(app.selected, Some(fid(2)));
        assert_eq!(app.explicit_mode, None);
        assert!(!app.ask_armed);
        assert_eq!(app.last_clicked_event, None);
        assert_eq!(app.input.to_string(), "half-typed", "draft survives");
    }

    /// **A sentence is not a branch reference.**
    /// `agent_reference_in` matches the word "agent" followed by
    /// digits, which ordinary prose is full of — this project is
    /// called agent2, so a line mentioning it yields `2`. Following
    /// that unchecked pointed `selected` at an event that is not a
    /// branch (#2 is usually the first user post), and the navigator
    /// highlighted nothing while the transcript emptied: the branch
    /// looked deselected.
    #[test]
    fn prose_naming_something_that_is_not_a_branch_leaves_the_selection_alone() {
        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "a test agent",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_markdown(
                "agent2 keeps the whole log, so nothing is lost.\n",
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "where does it all go?".into(),
            expects_reply: true,
        });
        let session = session.run();
        let branch = session.conversation_branch();
        let mut app = AttachedApp::new(branch);
        for event in rx.try_iter() {
            app.apply(&event);
        }

        let rows = app.chat.rows(Some(branch), 200, None);
        let prose = rows
            .iter()
            .position(|(_, t, d, _)| t.contains("agent2") && *d == RowDetail::None)
            .expect("the reply is on the transcript");
        assert_eq!(agent_reference_in(&rows[prose].1), Some(2));
        assert!(
            !session.tree().branches().iter().any(|(b, _)| *b == fid(2)),
            "#2 is on the log but is not a branch — which is the whole point"
        );

        app.pane_rects.push((
            Pane::Chat,
            PaneInfo {
                area: Rect {
                    x: 0,
                    y: 0,
                    width: 200,
                    height: 60,
                },
                scroll_top: 0,
            },
        ));
        app.chat_line_rows = (0..rows.len()).collect();
        app.on_click(0, prose as u16 + 1, &[branch]);
        assert_eq!(
            app.selected,
            Some(branch),
            "a prose click moved the selection off the branch"
        );
    }

    /// Clicking a chat row selects it, clicking the same row again
    /// deselects it, and clicking a different one replaces the
    /// selection outright (19_UX Step C2).
    #[test]
    fn clicking_a_chat_row_toggles_its_selection() {
        let (tx, rx) = channel();
        let session = run_demo(Tree::new(None), tx).unwrap();
        let branch = session.conversation_branch();
        let mut app = AttachedApp::new(branch);
        for event in rx.try_iter() {
            app.apply(&event);
        }
        let rows = app.chat.rows(Some(branch), 80, None);
        assert!(rows.len() >= 2, "the demo logs more than one row");
        let (first_id, second_id) = (rows[0].3, rows[1].3);
        app.pane_rects.push((
            Pane::Chat,
            PaneInfo {
                area: Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 50,
                },
                scroll_top: 0,
            },
        ));
        // Identity mapping — every row here is one line, no wrapping —
        // standing in for what `render_chat` would otherwise build.
        app.chat_line_rows = (0..rows.len()).collect();

        // Row 0 is at `row = 1` (the top border occupies row 0).
        app.on_click(0, 1, &[branch]);
        assert_eq!(app.last_clicked_event, Some(first_id));

        app.on_click(0, 1, &[branch]);
        assert_eq!(app.last_clicked_event, None, "clicking it again deselects");

        app.on_click(0, 2, &[branch]);
        assert_eq!(app.last_clicked_event, Some(second_id));
        app.on_click(0, 1, &[branch]);
        assert_eq!(
            app.last_clicked_event,
            Some(first_id),
            "a different row replaces the selection outright"
        );
    }

    /// The bug report this guards against: a wrapped row pushes more
    /// than one entry into `render_chat`'s wrapped-line `Paragraph`, so
    /// screen row 2 is *not* logical row 2 once anything above it
    /// wrapped — `chat_line_rows` is what translates back. Without it
    /// (indexing `rows` with the wrapped-line offset directly, as the
    /// code used to), this same click would have landed on `rows[2]`
    /// instead of `rows[1]` — a click on the row after a wrapped one
    /// hitting the wrong logical row, or landing past the end of a short
    /// transcript and silently doing nothing (which looks exactly like
    /// "the branch got deselected" once it happens to misfire onto a
    /// stray "agent N" mention elsewhere in the transcript).
    #[test]
    fn clicking_past_a_wrapped_row_resolves_the_correct_logical_row() {
        let (tx, rx) = channel();
        let session = run_demo(Tree::new(None), tx).unwrap();
        let branch = session.conversation_branch();
        let mut app = AttachedApp::new(branch);
        for event in rx.try_iter() {
            app.apply(&event);
        }
        let rows = app.chat.rows(Some(branch), 80, None);
        assert!(rows.len() >= 3, "the demo logs at least three rows");
        let (row1_id, row2_id) = (rows[1].3, rows[2].3);
        app.pane_rects.push((
            Pane::Chat,
            PaneInfo {
                area: Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 50,
                },
                scroll_top: 0,
            },
        ));
        // Row 0 wraps to two lines; every row after it shifts by one in
        // wrapped-line space.
        app.chat_line_rows = std::iter::once(0).chain(0..rows.len()).collect::<Vec<_>>();

        // Wrapped-line 2 is logical row 1's line (row 0 occupied wrapped
        // lines 0 and 1) — not logical row 2.
        app.on_click(0, 3, &[branch]); // row 0 is border; wrapped-line 2 is screen row 3
        assert_eq!(app.last_clicked_event, Some(row1_id));
        assert_ne!(app.last_clicked_event, Some(row2_id));
    }

    #[test]
    fn clicking_the_input_box_focuses_it_and_disarms() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        app.explicit_mode = Some(ExplicitMode::Rename);
        app.last_clicked_event = Some(EventId::new(3));
        app.input = InputBuffer::prefilled("draft text");
        app.pane_rects.push((
            Pane::Input,
            PaneInfo {
                area: Rect {
                    x: 0,
                    y: 40,
                    width: 80,
                    height: 5,
                },
                scroll_top: 0,
            },
        ));

        app.on_click(0, 42, &[fid(1)]);

        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.explicit_mode, None);
        assert_eq!(app.last_clicked_event, None);
        assert_eq!(app.input.to_string(), "draft text");
    }

    #[test]
    fn manual_navigator_scroll_resets_to_auto_follow_on_select() {
        let branches = [fid(1), fid(2), fid(3)];
        let mut app = AttachedApp::new(fid(1));
        app.pane_rects.push((
            Pane::Navigator,
            PaneInfo {
                area: Rect {
                    x: 0,
                    y: 0,
                    width: 40,
                    height: 5,
                },
                scroll_top: 0,
            },
        ));

        // A wheel-scroll pins a manual offset (Step E0).
        app.on_mouse(0, 2, MouseEventKind::ScrollDown, &branches);
        assert_eq!(app.navigator_scroll, Some(3));

        // Selecting a branch (row 1 in the pane = branches[1]) hands
        // auto-follow back — the same reset `chat_scroll` already gets.
        app.on_click(0, 2, &branches);
        assert_eq!(app.selected, Some(fid(2)));
        assert_eq!(
            app.navigator_scroll, None,
            "select_branch overrides the manual scroll back to auto-follow"
        );
    }

    #[test]
    fn tab_cycles_agents_and_digits_select_in_full_debug() {
        let agents = [fid(1), fid(5)];
        let mut app = AttachedApp::new(fid(1));
        app.on_key(KeyCode::Tab.into(), &agents, None);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Tab.into(), &agents, None);
        assert_eq!(app.selected, Some(fid(1)));

        app.focus = Focus::Debug;
        app.on_key(KeyCode::Char('d').into(), &agents, None);
        assert_eq!(app.view, View::FullDebug);
        app.on_key(KeyCode::Char('2').into(), &agents, None);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Char('1').into(), &agents, None);
        assert_eq!(app.selected, Some(fid(1)));
    }

    /// Two live agents (caller + in-flight subagent): both appear in
    /// the agent list, and switching retargets the VM the panes borrow.
    #[test]
    fn concurrent_agents_list_and_retarget() {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "slow".into(),
            description: String::new(),
            input_schema: json!({}),
            guidelines: Vec::new(),
            example: None,
            returns: None,
            handler: Box::new(|_| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(json!("done"))
            }),
            show_once: false,
        });
        // The parent's one program spawns a child and asks it something,
        // parking on the `await` — no second completion for the parent
        // itself (a yield, not a round trip: 22_ONE_VOCABULARY's "you
        // await an exchange, never a strand"). The child's own first
        // program starts the slow tool call without answering yet, which
        // is the concurrent-running window this test is probing; once
        // that settles, the child still owes the parent's `ask` and gets
        // a fresh turn to discharge it explicitly — `answer()` is the
        // only thing that does (a bare `return` answers nothing).
        // Event id 8 is the parent's `ask`-delivered `Post` on the
        // child's branch, deterministic from this exact call sequence
        // (Agent 1, Post 2, Turn 3, Spawn 4, Agent 5, Result 6, Send 7,
        // Post 8) — the same numbering `exchange_ids_form_a_closed_loop`
        // (`host/mod.rs`) documents for the identical spawn+ask shape.
        let script = vec![
            scripted_program(
                r#"const w = await spawn("child worker");
                   history.note(await ask(w.agent, "child task"));"#,
            ),
            scripted_program("history.note(await tools.slow());"),
            scripted_program(r#"history.note(answer(8, "child", "done"));"#),
        ];
        let (tx, _rx) = channel();
        let mut session = Session::new(
            Tree::new(None),
            "parent",
            registry,
            Box::new(ScriptedLlm::new(script)),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.conversation_branch(),
            text: "delegate".into(),
            expects_reply: true,
        });

        // Pump until both branches are live with running programs.
        let live = |session: &Session| -> Vec<BranchInfo> {
            session
                .branch_infos()
                .into_iter()
                .filter(|b| b.status != "dormant")
                .collect()
        };
        for _ in 0..200 {
            let branches = live(&session);
            if branches.len() == 2 && branches.iter().all(|b| b.status == "running") {
                break;
            }
            assert!(session.pump_one(), "session ended early");
        }
        let agents = live(&session);
        assert_eq!(agents.len(), 2, "{agents:?}");

        // Selecting each branch yields its own VM: different programs.
        let sources: Vec<String> = agents
            .iter()
            .map(|b| {
                session
                    .state(b.branch)
                    .unwrap()
                    .vm()
                    .unwrap()
                    .source
                    .to_string()
            })
            .collect();
        assert!(sources[0].contains("spawn("), "{sources:?}");
        assert!(sources[1].contains("tools.slow"), "{sources:?}");

        // And the whole thing still settles cleanly: nothing *ends* —
        // agents never close — so what it reaches is **quiet**, with the
        // root's final answer on its branch.
        while session.pump_one() {}
        assert!(session.quiet());
    }
}

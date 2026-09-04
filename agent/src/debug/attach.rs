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
//! - **Running**: auto-popped when the *selected* agent starts a
//!   `run_program` — source + console as a right column, sticky after
//!   completion for post-mortem reading; `c` collapses back, `1`–`4`
//!   override the auto-pop set.
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
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::PaneInfo;
use super::chat::{ChatKind, ChatState, RowDetail};
use super::input::InputBuffer;
use super::ui;
use crate::host::{BranchId, BranchInfo, Session, SessionCommand, SessionEvent, UserCall};
use crate::machine::TOOL_RUN_PROGRAM;
use crate::report::derived_branch_label;
use crate::tree::ProgramView;
use crate::types::{Call, Cause, EventId, EventPayload, Message, Outcome};

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pane {
    Chat,
    Navigator,
    Source,
    Disasm,
    Stack,
    Promises,
    Console,
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
    /// `v` — `Restart { branch, call: UserCall::Resume { value } }`; the
    /// text is parsed as JSON, falling back to a bare string.
    ResumeWithValue,
    /// `e` — `Restart { branch, call: UserCall::RunProgram { source } }`,
    /// pasting a full rewrite.
    Rewrite,
    /// `p` — `Spawn { parent: branch, charter, name: None, text: None }`.
    SpawnCharter,
}

/// A subitem selected within a program block.
#[derive(Debug, Clone, PartialEq)]
pub enum Subitem {
    Attachment(String),
    Invoke(usize),
}

pub struct AttachedApp {
    pub chat: ChatState,
    pub view: View,
    prev_view: View,
    pub focus: Focus,
    pub selected: Option<BranchId>,
    /// Which program the right-hand panes show (decision 1). `None` ⇒ the
    /// selected branch's most-recent program (the default); a click on an
    /// older chat block pins a specific one by its `run_program` event id.
    pub selected_program: Option<EventId>,
    /// A subitem within the selected program: an attachment or invoke to
    /// show in the right panel instead of the program console.
    pub selected_subitem: Option<Subitem>,
    /// Branches whose `System` block is folded to its header (decision 7).
    collapsed: HashSet<BranchId>,
    pub input: InputBuffer,
    pub quit: bool,
    pub show_source: bool,
    pub show_disasm: bool,
    pub show_stack: bool,
    pub show_promises: bool,
    pub chat_scroll: Option<usize>,
    pub console_scroll: Option<usize>,
    pub source_scroll: Option<usize>,
    pub disasm_scroll: Option<usize>,
    pub stack_scroll: Option<usize>,
    pub promises_scroll: Option<usize>,
    /// `None` auto-follows the selected branch's row; `Some(n)` is a
    /// manual wheel-scroll, reset back to auto-follow by `select_branch`
    /// (19_UX Step E0) — needed now that the navigator's height is
    /// capped instead of always growing to fit the whole tree.
    pub navigator_scroll: Option<usize>,
    pub pane_rects: Vec<(Pane, PaneInfo)>,
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
}

impl AttachedApp {
    pub fn new(root: BranchId) -> Self {
        AttachedApp {
            chat: ChatState::new(),
            view: View::Chat,
            prev_view: View::Chat,
            focus: Focus::Input,
            selected: Some(root),
            selected_program: None,
            selected_subitem: None,
            collapsed: HashSet::new(),
            input: InputBuffer::new(),
            quit: false,
            show_source: true,
            show_disasm: false,
            show_stack: false,
            show_promises: false,
            chat_scroll: None,
            console_scroll: None,
            source_scroll: None,
            disasm_scroll: None,
            stack_scroll: None,
            promises_scroll: None,
            navigator_scroll: None,
            pane_rects: Vec::new(),
            last_chat_lines: 0,
            last_clicked_event: None,
            explicit_mode: None,
            ask_armed: false,
            timeline: false,
            timeline_cursor: 0,
        }
    }

    /// Feed one `SessionEvent`: updates the transcript and drives the
    /// auto-pop — the selected branch starting a `run_program` pops the
    /// source + console column (decision 6).
    pub fn apply(&mut self, event: &SessionEvent) {
        if let SessionEvent::Event { branch, event, .. } = event
            && Some(*branch) == self.selected
            && matches!(
                &event.payload,
                EventPayload::Message(Message::Turn { tool_calls, .. })
                    if tool_calls.iter().any(|c| c.name == TOOL_RUN_PROGRAM)
            )
        {
            // Follow the live program: a fresh run on the selected agent
            // drops any pinned older program.
            self.selected_program = None;
            if self.view == View::Chat {
                self.view = View::Running;
                // A fresh pop resets to the auto-pop set; later manual
                // toggles override it until the next pop.
                self.show_source = true;
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
        let current = self.chat.rows(self.selected).len();
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
            Pane::Disasm => self.disasm_scroll = Some(new),
            Pane::Stack => self.stack_scroll = Some(new),
            Pane::Promises => self.promises_scroll = Some(new),
            Pane::Navigator => self.navigator_scroll = Some(new),
            Pane::Input => {}
        }
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
                let rows = self.chat.rows(self.selected);
                if let Some((kind, text, detail, id)) = rows.get(line) {
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
                        RowDetail::None => {
                            if let Some(n) = agent_reference_in(text)
                                && n != 0
                            {
                                self.select_branch(EventId::new(n));
                            }
                        }
                        RowDetail::Program(pid) => {
                            self.selected_program = Some(*pid);
                            self.selected_subitem = None;
                            self.reset_program_scrolls();
                        }
                        RowDetail::Attachment(pid, name) => {
                            let toggle_off = self.selected_program == Some(*pid)
                                && self.selected_subitem == Some(Subitem::Attachment(name.clone()));
                            self.selected_program = Some(*pid);
                            if toggle_off {
                                self.selected_subitem = None;
                            } else {
                                self.selected_subitem = Some(Subitem::Attachment(name.clone()));
                            }
                            self.reset_program_scrolls();
                        }
                        RowDetail::Invoke(pid, idx) => {
                            let toggle_off = self.selected_program == Some(*pid)
                                && self.selected_subitem == Some(Subitem::Invoke(*idx));
                            self.selected_program = Some(*pid);
                            if toggle_off {
                                self.selected_subitem = None;
                            } else {
                                self.selected_subitem = Some(Subitem::Invoke(*idx));
                            }
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
                if self.show_source {
                    right.push(Pane::Source);
                }
                if self.show_disasm {
                    right.push(Pane::Disasm);
                }
                if self.show_stack {
                    right.push(Pane::Stack);
                }
                if self.show_promises {
                    right.push(Pane::Promises);
                }
                right.push(Pane::Console);
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

    pub fn on_key(&mut self, key: KeyEvent, branches: &[BranchId]) -> KeyAction {
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
        match self.view {
            View::FullDebug => self.on_debug_key(key.code, branches),
            View::Chat | View::Running => match self.focus {
                Focus::Input => self.on_input_key(key),
                Focus::Debug => self.on_debug_key(key.code, branches),
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
            KeyCode::Up => {
                self.input.up();
                KeyAction::None
            }
            KeyCode::Down => {
                self.input.down();
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
                self.input.up();
                KeyAction::None
            }
            KeyCode::Char('n') if ctrl => {
                self.input.down();
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

    fn on_debug_key(&mut self, code: KeyCode, branches: &[BranchId]) -> KeyAction {
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
            // `run_program` re-triggering the auto-pop in `apply` —
            // pressing it again from `Chat` reopens them.
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
            KeyCode::Char('x') if self.view != View::FullDebug => KeyAction::Interrupt,
            KeyCode::Char('w') if self.view != View::FullDebug => KeyAction::JumpToWaiting,
            KeyCode::Char('t') if self.view != View::FullDebug => {
                self.timeline = true;
                self.timeline_cursor = 0;
                KeyAction::None
            }
            KeyCode::Char('a') if self.view != View::FullDebug => self.arm_ask(),
            KeyCode::Char('r') if self.view != View::FullDebug => self.arm(ExplicitMode::Rename),
            KeyCode::Char('v') if self.view != View::FullDebug => {
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
fn resolve_submit(
    infos: &[BranchInfo],
    branch: BranchId,
    text: String,
    expects_reply: bool,
) -> SessionCommand {
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
        ExplicitMode::ResumeWithValue => SessionCommand::Restart {
            branch,
            call: UserCall::Resume {
                value: Some(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))),
            },
        },
        ExplicitMode::Rewrite => SessionCommand::Restart {
            branch,
            call: UserCall::RunProgram { source: text },
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
            let EventPayload::Message(Message::Post {
                from: crate::types::Author::User,
                origin,
            }) = &e.payload
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
        let ordered = ordered_branches(infos.clone());
        let branches: Vec<BranchId> = ordered.iter().map(|b| b.branch).collect();
        for input in inputs {
            match input {
                CtEvent::Key(key) if key.is_press() => {
                    let action = app.on_key(key, &branches);
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
                                });
                            }
                        }
                        KeyAction::ForkAt(at) => {
                            handle.send(SessionCommand::Fork {
                                from: at,
                                name: None,
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
        if let Err(e) = terminal.draw(|frame| render(frame, &mut app, &session)) {
            break Err(e.to_string());
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
        let [l, r] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(main);
        (l, Some(r))
    };

    if panes.chat {
        // The question sits above the input, and the input line switches
        // to reply mode, exactly when this branch is waiting on you
        // (17_BRANCHES Part D).
        let asking_text = app.selected.and_then(|b| asking_question_text(session, b));
        let (top, transcript_area, input_area) =
            render_chat(frame, app, left, app.chat_scroll, asking_text.as_deref());
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
        // Old programs (no live VM, from the log) strip VM-only panes
        // and force source + console (plan Step 5).
        let mut right_panes = panes.right.clone();
        if vm.is_none() && pv.is_some() {
            right_panes.retain(|p| matches!(p, Pane::Navigator | Pane::Source | Pane::Console));
            if !right_panes.contains(&Pane::Source) {
                right_panes.insert(1, Pane::Source);
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
                Pane::Console => {
                    let (top, area) = if app.selected_subitem.is_some() {
                        if let Some(ref pv) = pv {
                            render_subitem(frame, app, pv, *slot, app.console_scroll)
                        } else {
                            render_placeholder(frame, Pane::Console, *slot);
                            (0, *slot)
                        }
                    } else if let Some(ref pv) = pv {
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
    let waiting = branch_counts(&session.branch_infos()).0 > 0;
    // `tab`/`1-9` cycle or jump between branches — with only one, that
    // targets the branch already selected, and `cycle_branch` guards
    // against the real cost of that (it would otherwise silently reset
    // the program/subitem selection and every pane's scroll position).
    // Advertised the same way `waiting` is: only when it would move.
    let multi_branch = session.tree().branches().len() > 1;
    let help = match (app.view, app.focus) {
        (View::FullDebug, _) => format!(
            " d/esc chat · r rewrite{} · space run/pause · s step · n step line · q quit ",
            if multi_branch {
                " · tab/1-9 agent"
            } else {
                ""
            }
        ),
        (_, Focus::Input) if app.ask_armed => format!(
            " type to ask · enter send · esc clear/cancel{} ",
            if multi_branch { " · tab agent" } else { "" }
        ),
        (_, Focus::Input) => format!(
            " type to chat · enter send{} · esc debug keys ",
            if multi_branch { " · tab agent" } else { "" }
        ),
        (View::Running, Focus::Debug) => format!(
            " esc/i type · c collapse · d debugger · 1-4 panes · f fork · p spawn · \
             x interrupt · a ask · v resume · r rename{} · t timeline{} · q quit ",
            if waiting { " · w waiting" } else { "" },
            if multi_branch { " · tab agent" } else { "" }
        ),
        (_, Focus::Debug) => format!(
            " esc/i type · c expand · d debugger · f fork · p spawn · x interrupt · a ask \
             · v resume · r rename{} · t timeline{} · q quit ",
            if waiting { " · w waiting" } else { "" },
            if multi_branch { " · tab agent" } else { "" }
        ),
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().add_modifier(Modifier::REVERSED)),
        footer,
    );
}

fn chat_style(kind: ChatKind, even: bool) -> Style {
    let dim = |r, g, b| Color::Rgb((r * 3 / 4) as u8, (g * 3 / 4) as u8, (b * 3 / 4) as u8);
    match kind {
        ChatKind::System => {
            let fg = if even {
                Color::Magenta
            } else {
                dim(255, 0, 255)
            };
            Style::default().fg(fg).add_modifier(Modifier::DIM)
        }
        ChatKind::User => {
            let fg = if even { Color::Cyan } else { dim(0, 220, 220) };
            Style::default().fg(fg).add_modifier(Modifier::BOLD)
        }
        ChatKind::Assistant => {
            let fg = if even {
                Color::Rgb(220, 220, 220)
            } else {
                Color::Rgb(155, 155, 155)
            };
            Style::default().fg(fg)
        }
        ChatKind::Streaming => {
            let fg = if even {
                Color::Rgb(140, 140, 140)
            } else {
                Color::Rgb(90, 90, 90)
            };
            Style::default().fg(fg).add_modifier(Modifier::ITALIC)
        }
        ChatKind::ToolCall => {
            let fg = if even {
                Color::Yellow
            } else {
                Color::Rgb(180, 170, 0)
            };
            Style::default().fg(fg)
        }
        ChatKind::Attachment => {
            let fg = if even {
                Color::Rgb(120, 200, 255)
            } else {
                Color::Rgb(80, 140, 200)
            };
            Style::default().fg(fg)
        }
        ChatKind::Marker => {
            let fg = if even {
                Color::DarkGray
            } else {
                Color::Rgb(70, 70, 70)
            };
            Style::default().fg(fg).add_modifier(Modifier::ITALIC)
        }
        ChatKind::Error => {
            let fg = if even { Color::Red } else { dim(240, 0, 0) };
            Style::default().fg(fg)
        }
    }
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

fn render_chat(
    frame: &mut Frame,
    app: &AttachedApp,
    area: Rect,
    scroll: Option<usize>,
    asking: Option<&str>,
) -> (usize, Rect, Rect) {
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
    let wrap_width = transcript_area.width.saturating_sub(2).max(1) as usize;
    let rows = app.chat.rows(app.selected);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
    let mut parity: HashMap<ChatKind, bool> = HashMap::new();
    let mut in_program: Option<EventId> = None;
    let mut prev_kind: Option<ChatKind> = None;
    for (kind, text, detail, id) in &rows {
        let even = match detail {
            RowDetail::Program(pid) | RowDetail::Attachment(pid, _) | RowDetail::Invoke(pid, _) => {
                if in_program != Some(*pid) {
                    in_program = Some(*pid);
                    let e = parity.entry(ChatKind::ToolCall).or_insert(true);
                    *e = !*e;
                }
                *parity.get(&ChatKind::ToolCall).unwrap_or(&true)
            }
            RowDetail::None => {
                in_program = None;
                if prev_kind != Some(*kind) {
                    let e = parity.entry(*kind).or_insert(true);
                    *e = !*e;
                }
                *parity.get(kind).unwrap_or(&true)
            }
        };
        prev_kind = Some(*kind);
        let mut style = chat_style(*kind, even);
        // Highlight the selected subitem line.
        if let Some(ref sel) = app.selected_subitem {
            let highlight = match (detail, sel) {
                (RowDetail::Attachment(pid, name), Subitem::Attachment(s))
                    if app.selected_program == Some(*pid) && name == s =>
                {
                    true
                }
                (RowDetail::Invoke(pid, idx), Subitem::Invoke(i))
                    if app.selected_program == Some(*pid) && idx == i =>
                {
                    true
                }
                _ => false,
            };
            if highlight {
                style = style.add_modifier(Modifier::REVERSED);
            }
        }
        // Highlight the selected message (the fork-from-here target,
        // 19_UX Step C2) the same way — one visual language for
        // "selected," not a second one just for this.
        if app.last_clicked_event == Some(*id) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        push_wrapped_width(&mut lines, text, style, wrap_width);
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
    (top, transcript_area, input_area)
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

fn render_subitem(
    frame: &mut Frame,
    app: &AttachedApp,
    pv: &ProgramView,
    area: Rect,
    scroll: Option<usize>,
) -> (usize, Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let title = match &app.selected_subitem {
        Some(Subitem::Attachment(name)) => {
            let content = pv
                .attachments
                .get(name)
                .map(|s| s.as_str())
                .unwrap_or("(attachment not found)");
            for l in content.lines() {
                lines.push(Line::from(l.to_owned()));
            }
            format!(" attachment: {name} ")
        }
        Some(Subitem::Invoke(idx)) => {
            if let Some(invoke) = pv.invokes.get(*idx) {
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
            } else {
                lines.push(Line::from("(invoke not found)"));
                " invoke ".to_string()
            }
        }
        None => {
            lines.push(Line::from("(no subitem)"));
            " subitem ".to_string()
        }
    };
    let visible = area.height.saturating_sub(2) as usize;
    let default_top = lines.len().saturating_sub(visible);
    let top = scroll.unwrap_or(default_top).min(default_top);
    let end = (top + visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[top..end].to_vec())
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
    (top, area)
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
fn condition_line(cause: &Cause) -> String {
    match cause {
        Cause::Raised { name, .. } => format!("raised `{name}`"),
        Cause::Trapped { message, .. } => message.clone(),
        Cause::Posted { .. } => "a message arrived".to_owned(),
        Cause::CompileFailed { .. } => "compile error".to_owned(),
        Cause::Refused { reason } => format!("refused: {reason}"),
        Cause::Interrupted => "interrupted".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        ScriptedLlm, ToolDef, ToolRegistry, run_demo, scripted_program, scripted_text,
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
        assert_eq!(app.on_debug_key(KeyCode::Char('a'), &[]), KeyAction::None);
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
        app.on_debug_key(KeyCode::Char('a'), &[]);
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
        assert_eq!(app.on_debug_key(KeyCode::Char('r'), &[]), KeyAction::None);
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
        app.on_debug_key(KeyCode::Char('v'), &[]);
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
                call: UserCall::Resume {
                    value: Some(json!(5))
                },
            }
        );
        assert_eq!(
            resolve_submit_mode(ExplicitMode::ResumeWithValue, branch, "not json".into()),
            SessionCommand::Restart {
                branch,
                call: UserCall::Resume {
                    value: Some(json!("not json")),
                },
            },
            "a non-JSON value falls back to a bare string"
        );
        assert_eq!(
            resolve_submit_mode(ExplicitMode::Rewrite, branch, "return 1;".into()),
            SessionCommand::Restart {
                branch,
                call: UserCall::RunProgram {
                    source: "return 1;".into(),
                },
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

    /// `f`/`x`/`w` map to the right `KeyAction`, and `f` without a
    /// selected message falls back to fork-here (19_UX Step C2: one
    /// key carries both of the old `f`/`F` pair's meanings).
    #[test]
    fn fork_interrupt_and_jump_keys() {
        let mut app = AttachedApp::new(fid(1));
        app.focus = Focus::Debug;
        assert_eq!(app.on_debug_key(KeyCode::Char('f'), &[]), KeyAction::Fork);
        app.last_clicked_event = Some(fid(42));
        assert_eq!(
            app.on_debug_key(KeyCode::Char('f'), &[]),
            KeyAction::ForkAt(fid(42))
        );
        assert_eq!(
            app.on_debug_key(KeyCode::Char('x'), &[]),
            KeyAction::Interrupt
        );
        assert_eq!(
            app.on_debug_key(KeyCode::Char('w'), &[]),
            KeyAction::JumpToWaiting
        );
    }

    /// A branch waiting on you renders its question above the input line
    /// and switches Enter to reply mode (17_BRANCHES Part D).
    #[test]
    fn a_pending_ask_to_user_shows_above_the_input_as_reply_mode() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "sys").unwrap();
        let send = tree
            .append(
                &mut spine,
                EventPayload::Call(crate::types::Call::Send {
                    to: crate::types::Address::User,
                    text: "which file?".into(),
                    input: json!(null),
                    expects_reply: true,
                    site: 0,
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
        assert!(panes.right.contains(&Pane::Source));
        assert!(panes.right.contains(&Pane::Console));
        // The post-mortem VM is still borrowable for those panes.
        let state = session.state(session.conversation_branch()).unwrap();
        assert!(!state.vm_is_live());
        assert!(state.vm().is_some(), "final program state kept");

        // The collapse key restores full-width chat.
        app.on_key(KeyCode::Esc.into(), &[]); // input → debug focus
        assert_eq!(app.focus, Focus::Debug);
        app.on_key(KeyCode::Char('c').into(), &[]);
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
        // source/console panes without needing a fresh run_program to
        // re-trigger the auto-pop. Collapsing left focus on `Input`
        // (same as the original direction leaves it), so `Esc` back to
        // `Focus::Debug` first, same as above.
        app.on_key(KeyCode::Esc.into(), &[]);
        assert_eq!(app.focus, Focus::Debug);
        app.on_key(KeyCode::Char('c').into(), &[]);
        assert_eq!(app.view, View::Running);
        let panes = app.pane_set();
        assert!(panes.right.contains(&Pane::Source));
        assert!(panes.right.contains(&Pane::Console));
    }

    #[test]
    fn full_debugger_mode_swaps_and_returns_without_session_actions() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running;
        app.focus = Focus::Debug;
        assert_eq!(
            app.on_key(KeyCode::Char('d').into(), &[fid(1)]),
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
            app.on_key(KeyCode::Char('d').into(), &[fid(1)]),
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

    #[test]
    fn manual_toggles_override_auto_pop_set() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running;
        app.focus = Focus::Debug;
        assert!(app.pane_set().right.contains(&Pane::Source));
        app.on_key(KeyCode::Char('1').into(), &[]);
        assert!(!app.pane_set().right.contains(&Pane::Source));
        app.on_key(KeyCode::Char('3').into(), &[]);
        assert!(app.pane_set().right.contains(&Pane::Stack));
    }

    #[test]
    fn typing_is_free_and_submit_goes_through_commands() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running; // digits must still type, not toggle
        for c in "d1 sq".chars() {
            assert_eq!(app.on_key(KeyCode::Char(c).into(), &[]), KeyAction::None);
        }
        assert_eq!(app.input.to_string(), "d1 sq");
        assert_eq!(app.view, View::Running, "no debug keys fired while typing");
        assert!(!app.quit);
        assert_eq!(
            app.on_key(KeyCode::Enter.into(), &[]),
            KeyAction::Submit {
                text: "d1 sq".into(),
                expects_reply: false,
            }
        );
        assert!(app.input.is_empty());
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

    /// With nothing else to cycle to, Tab must not fall through to
    /// `select_branch` on the branch already selected — that would
    /// silently reset the program/subitem selection and every pane's
    /// scroll position for no reason.
    #[test]
    fn tab_with_one_branch_is_a_true_no_op() {
        let mut app = AttachedApp::new(fid(1));
        app.selected_program = Some(fid(7));
        app.source_scroll = Some(3);
        app.on_key(KeyCode::Tab.into(), &[fid(1)]);
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
        let rows = app.chat.rows(Some(branch));
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
        app.on_key(KeyCode::Tab.into(), &agents);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Tab.into(), &agents);
        assert_eq!(app.selected, Some(fid(1)));

        app.focus = Focus::Debug;
        app.on_key(KeyCode::Char('d').into(), &agents);
        assert_eq!(app.view, View::FullDebug);
        app.on_key(KeyCode::Char('2').into(), &agents);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Char('1').into(), &agents);
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
            handler: Box::new(|_| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(json!("done"))
            }),
        });
        let script = vec![
            scripted_program(
                "c1",
                r#"return await tools.agent({ prompt: "child task", input: null });"#,
            ),
            scripted_program("c2", "return await tools.slow();"),
            scripted_text("child done"),
            scripted_text("parent done"),
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
        assert!(sources[0].contains("tools.agent"), "{sources:?}");
        assert!(sources[1].contains("tools.slow"), "{sources:?}");

        // And the whole thing still settles cleanly: nothing *ends* —
        // agents never close — so what it reaches is **quiet**, with the
        // root's final answer on its branch.
        while session.pump_one() {}
        assert!(session.quiet());
    }
}

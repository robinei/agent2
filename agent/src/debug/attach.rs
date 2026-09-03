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
use ratatui::crossterm::event::{Event as CtEvent, KeyCode, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::PaneInfo;
use super::chat::{ChatKind, ChatState, RowDetail};
use super::ui;
use crate::host::{AgentId, Session, SessionCommand, SessionEvent};
use crate::machine::TOOL_RUN_PROGRAM;
use crate::tree::{AgentView, ProgramView};
use crate::types::{Cause, EventId, EventPayload, Message, Outcome};

/// Cap for one step-line key, so a hot loop on one source line cannot
/// wedge the UI (mirrors the standalone runner).
const LINE_STEP_CAP: u64 = 50_000;

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
    Submit(String),
    TogglePause,
    StepInstr,
    StepLine,
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
    pub selected: Option<AgentId>,
    /// Which program the right-hand panes show (decision 1). `None` ⇒ the
    /// selected agent's most-recent program (the default); a click on an
    /// older chat block pins a specific one by its `run_program` event id.
    pub selected_program: Option<EventId>,
    /// A subitem within the selected program: an attachment or invoke to
    /// show in the right panel instead of the program console.
    pub selected_subitem: Option<Subitem>,
    /// Agents whose `System` block is folded to its header (decision 7).
    collapsed: HashSet<AgentId>,
    pub input: String,
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
    pub pane_rects: Vec<(Pane, PaneInfo)>,
    last_chat_lines: usize,
}

impl AttachedApp {
    pub fn new(root: AgentId) -> Self {
        AttachedApp {
            chat: ChatState::new(),
            view: View::Chat,
            prev_view: View::Chat,
            focus: Focus::Input,
            selected: Some(root),
            selected_program: None,
            selected_subitem: None,
            collapsed: HashSet::new(),
            input: String::new(),
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
            pane_rects: Vec::new(),
            last_chat_lines: 0,
        }
    }

    /// Feed one `SessionEvent`: updates the transcript and drives the
    /// auto-pop — the selected agent starting a `run_program` pops the
    /// source + console column (decision 6).
    pub fn apply(&mut self, event: &SessionEvent) {
        if let SessionEvent::Event { agent, event } = event
            && Some(*agent) == self.selected
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

    pub fn on_mouse(&mut self, column: u16, row: u16, kind: MouseEventKind, agents: &[AgentId]) {
        if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
            self.on_click(column, row, agents);
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
            Pane::Navigator => {}
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
    /// the agent; a chat-block row pins the program; a `system` header
    /// toggles its agent's fold.
    fn on_click(&mut self, column: u16, row: u16, agents: &[AgentId]) {
        let Some((pane, info)) = self.pane_at(column, row) else {
            return;
        };
        // Row within the bordered pane body (the top border is row 0).
        let body = (row as usize).checked_sub(info.area.y as usize + 1);
        match pane {
            Pane::Navigator => {
                if let Some(idx) = body
                    && let Some(&fid) = agents.get(idx)
                {
                    self.select_agent(fid);
                }
            }
            Pane::Chat => {
                let Some(body) = body else { return };
                let line = info.scroll_top + body;
                let rows = self.chat.rows(self.selected);
                if let Some((kind, _text, detail)) = rows.get(line) {
                    if *kind == ChatKind::System {
                        if let Some(agent) = self.selected
                            && !self.collapsed.remove(&agent)
                        {
                            self.collapsed.insert(agent);
                        }
                        return;
                    }
                    match detail {
                        RowDetail::None => {}
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
            _ => {}
        }
    }

    /// Point both selection axes at `agent`: it becomes the chat focus and
    /// the panes fall back to its most-recent program (decision 1).
    fn select_agent(&mut self, agent: AgentId) {
        self.selected = Some(agent);
        self.selected_program = None;
        self.reset_program_scrolls();
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

    pub fn on_key(&mut self, code: KeyCode, agents: &[AgentId]) -> KeyAction {
        // Context switching works everywhere.
        if code == KeyCode::Tab {
            self.cycle_agent(agents);
            return KeyAction::None;
        }
        match self.view {
            View::FullDebug => self.on_debug_key(code, agents),
            View::Chat | View::Running => match self.focus {
                Focus::Input => self.on_input_key(code),
                Focus::Debug => self.on_debug_key(code, agents),
            },
        }
    }

    fn on_input_key(&mut self, code: KeyCode) -> KeyAction {
        match code {
            KeyCode::Enter if !self.input.is_empty() => {
                KeyAction::Submit(std::mem::take(&mut self.input))
            }
            KeyCode::Backspace => {
                self.input.pop();
                KeyAction::None
            }
            KeyCode::Esc => {
                if self.input.is_empty() {
                    self.focus = Focus::Debug;
                } else {
                    self.input.clear();
                }
                KeyAction::None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn on_debug_key(&mut self, code: KeyCode, agents: &[AgentId]) -> KeyAction {
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
            // The collapse key: back to full-width chat.
            KeyCode::Char('c') if self.view == View::Running => {
                self.view = View::Chat;
                self.focus = Focus::Input;
                KeyAction::None
            }
            KeyCode::Char(' ') => KeyAction::TogglePause,
            KeyCode::Char('s') => KeyAction::StepInstr,
            KeyCode::Char('n') => KeyAction::StepLine,
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as u8 - b'1') as usize;
                if self.view == View::FullDebug {
                    // 1–9 switch which agent the panes borrow.
                    if let Some(id) = agents.get(idx) {
                        self.select_agent(*id);
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

    fn cycle_agent(&mut self, agents: &[AgentId]) {
        if agents.is_empty() {
            return;
        }
        let next = match self
            .selected
            .and_then(|s| agents.iter().position(|f| *f == s))
        {
            Some(i) => (i + 1) % agents.len(),
            None => 0,
        };
        self.select_agent(agents[next]);
    }
}

/// The current spine leaf for `agent` — from the live state if available
/// (resume-friendly session), or the first leaf in the agent's subtree
/// from the tree projection (log-only, decision 8).
fn find_leaf(session: &Session, agent: AgentId) -> Option<EventId> {
    if let Some(state) = session.state(agent) {
        return Some(state.spine.leaf_id);
    }
    session.tree().list_leaves().iter().find_map(|(id, _)| {
        session
            .tree()
            .enclosing_agent(*id)
            .filter(|ef| *ef == agent)?;
        Some(*id)
    })
}

/// If `program` is the current (or most-recently-completed) program in
/// `agent`, returns the VM for rich introspection — otherwise `None` (it
/// is an older program rendered from the log projection, Step 5).
fn vm_for_program(session: &Session, agent: AgentId, program: EventId) -> Option<&interp::VM> {
    let state = session.state(agent)?;
    let vm = state.vm()?;
    let leaf = state.spine.leaf_id;
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
    let Some(agent) = app.selected else {
        return (None, None);
    };
    let Some(leaf) = find_leaf(session, agent) else {
        return (None, None);
    };
    let programs = session.tree().programs_for(agent, leaf);
    let effective = app
        .selected_program
        .or_else(|| programs.last().map(|p| p.id));
    let pv = effective.and_then(|id| programs.into_iter().find(|p| p.id == id));
    let vm = effective.and_then(|prog_id| vm_for_program(session, agent, prog_id));
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

    let mut app = AttachedApp::new(session.root());
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
        // All agents root-first, from the log projection so the navigator
        // survives resume (decision 8), not just the live `states`.
        let agents: Vec<AgentId> = session.tree().agent_list().iter().map(|fv| fv.id).collect();
        for input in inputs {
            match input {
                CtEvent::Key(key) if key.is_press() => {
                    let action = app.on_key(key.code, &agents);
                    let Some(selected) = app.selected else {
                        continue;
                    };
                    match action {
                        KeyAction::None => {}
                        KeyAction::Submit(text) => {
                            handle.send(SessionCommand::UserTurn(text));
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
                    }
                }
                CtEvent::Mouse(mouse) => app.on_mouse(mouse.column, mouse.row, mouse.kind, &agents),
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

/// Step the selected agent's VM until its source line changes (or the
/// program yields/finishes, or the cap is hit).
fn step_line(session: &mut Session, agent: AgentId) {
    session.set_paused(agent, true);
    let line_of = |session: &Session| {
        session
            .state(agent)
            .and_then(|s| s.vm())
            .and_then(super::panes::current_line)
    };
    let start = line_of(session);
    for _ in 0..LINE_STEP_CAP {
        session.step_paused(agent, 1);
        let state = session.state(agent);
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

    let panes = app.pane_set();
    let (left, right) = if panes.right.is_empty() {
        (main, None)
    } else {
        let [l, r] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(main);
        (l, Some(r))
    };

    if panes.chat {
        let (top, chat_area) = render_chat(frame, app, left, app.chat_scroll);
        app.pane_rects.push((
            Pane::Chat,
            PaneInfo {
                area: chat_area,
                scroll_top: top,
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
        let slots = Layout::vertical(right_panes.iter().map(|p| match p {
            Pane::Navigator => Constraint::Length(session.tree().agent_list().len() as u16 + 2),
            _ => Constraint::Fill(1),
        }))
        .split(right);
        for (pane, slot) in right_panes.iter().zip(slots.iter()) {
            match pane {
                Pane::Navigator => {
                    render_navigator(frame, app, session, *slot);
                    app.pane_rects.push((
                        Pane::Navigator,
                        PaneInfo {
                            area: *slot,
                            scroll_top: 0,
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
                    let top = if let Some(vm) = vm {
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
            }
        }
    }

    let help = match (app.view, app.focus) {
        (View::FullDebug, _) => {
            " d/esc chat · tab/1-9 agent · space run/pause · s step · n step line · q quit "
        }
        (_, Focus::Input) => " type to chat · enter send · tab agent · esc debug keys ",
        (View::Running, Focus::Debug) => {
            " esc/i type · c collapse · d debugger · 1-4 panes · space/s/n vm · tab agent · q quit "
        }
        (_, Focus::Debug) => " esc/i type · d debugger · space/s/n vm · tab agent · q quit ",
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

fn render_chat(
    frame: &mut Frame,
    app: &AttachedApp,
    area: Rect,
    scroll: Option<usize>,
) -> (usize, Rect) {
    let [transcript_area, input_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(area);

    let rows = app.chat.rows(app.selected);
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    let mut parity: HashMap<ChatKind, bool> = HashMap::new();
    let mut in_program: Option<EventId> = None;
    let mut prev_kind: Option<ChatKind> = None;
    for (kind, text, detail) in &rows {
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
        lines.push(Line::from(text.as_str()).style(style));
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

    let (border, cursor) = match app.focus {
        Focus::Input => (Style::default().fg(Color::Cyan), "▏"),
        Focus::Debug => (Style::default().fg(Color::DarkGray), ""),
    };
    frame.render_widget(
        Paragraph::new(format!("❯ {}{}", app.input, cursor)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(" message "),
        ),
        input_area,
    );
    (top, area)
}

fn build_navigator_lines(agents: &[AgentView]) -> Vec<(AgentView, String)> {
    let mut children: std::collections::HashMap<Option<AgentId>, Vec<&AgentView>> =
        std::collections::HashMap::new();
    for fv in agents {
        children.entry(fv.parent).or_default().push(fv);
    }
    for list in children.values_mut() {
        list.sort_by_key(|fv| fv.id.as_u64());
    }

    let mut result = Vec::with_capacity(agents.len());
    fn dfs(
        parent: Option<AgentId>,
        children: &std::collections::HashMap<Option<AgentId>, Vec<&AgentView>>,
        ancestors_last: &mut Vec<bool>,
        result: &mut Vec<(AgentView, String)>,
    ) {
        let Some(kids) = children.get(&parent) else {
            return;
        };
        let len = kids.len();
        for (i, fv) in kids.iter().enumerate() {
            let is_last = i == len - 1;
            let mut prefix = String::new();
            for &ancestor_last in ancestors_last.iter() {
                if !ancestor_last {
                    prefix.push_str("│   ");
                } else {
                    prefix.push_str("    ");
                }
            }
            if is_last {
                prefix.push_str("└── ");
            } else {
                prefix.push_str("├── ");
            }
            result.push(((*fv).clone(), prefix));
            ancestors_last.push(is_last);
            dfs(Some(fv.id), children, ancestors_last, result);
            ancestors_last.pop();
        }
    }

    let mut ancestors_last = Vec::new();
    dfs(None, &children, &mut ancestors_last, &mut result);
    result
}

fn render_navigator(frame: &mut Frame, app: &AttachedApp, session: &Session, area: Rect) {
    // Agents from the log projection so the navigator survives resume
    // (decision 8), with live status overlayed from `session.agents()`.
    let live: std::collections::HashMap<AgentId, &'static str> =
        session.agents().into_iter().collect();
    let agent_views = session.tree().agent_list();
    let tree_lines = build_navigator_lines(&agent_views);
    let lines: Vec<Line> = tree_lines
        .iter()
        .map(|(fv, prefix)| {
            let selected = app.selected == Some(fv.id);
            let live_status = live.get(&fv.id).copied();
            let status = live_status.unwrap_or(if fv.complete { "done" } else { "idle" });
            let paused = live_status.is_some() && session.is_paused(fv.id);
            let busy = matches!(live_status, Some("running" | "awaiting llm"));
            let text = format!(
                "{} {}agent #{} · {}{}",
                if selected { "▶" } else { " " },
                prefix,
                fv.id.as_u64(),
                status,
                if paused {
                    " ⏸"
                } else if busy {
                    " ●"
                } else {
                    ""
                },
            );
            let style = if selected {
                if fv.complete {
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::DarkGray)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                }
            } else if fv.complete {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(text).style(style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" agents ")),
        area,
    );
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

    fn fid(n: u64) -> AgentId {
        EventId::new(n)
    }

    /// Drive the layout with the real M0 scripted demo's events.
    #[test]
    fn m0_run_program_auto_pops_and_sticks() {
        let (tx, rx) = channel();
        let session = run_demo(Tree::new(None), tx).unwrap();
        let mut app = AttachedApp::new(session.root());

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
        let state = session.state(session.root()).unwrap();
        assert!(!state.vm_is_live());
        assert!(state.vm().is_some(), "final program state kept");

        // The collapse key restores full-width chat.
        app.on_key(KeyCode::Esc, &[]); // input → debug focus
        assert_eq!(app.focus, Focus::Debug);
        app.on_key(KeyCode::Char('c'), &[]);
        assert_eq!(app.view, View::Chat);
        assert_eq!(
            app.pane_set(),
            PaneSet {
                chat: true,
                console_left: false,
                right: vec![Pane::Navigator]
            }
        );
    }

    #[test]
    fn full_debugger_mode_swaps_and_returns_without_session_actions() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running;
        app.focus = Focus::Debug;
        assert_eq!(app.on_key(KeyCode::Char('d'), &[fid(1)]), KeyAction::None);
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
        assert_eq!(app.on_key(KeyCode::Char('d'), &[fid(1)]), KeyAction::None);
        assert_eq!(app.view, View::Running, "returns to the previous view");
    }

    #[test]
    fn manual_toggles_override_auto_pop_set() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running;
        app.focus = Focus::Debug;
        assert!(app.pane_set().right.contains(&Pane::Source));
        app.on_key(KeyCode::Char('1'), &[]);
        assert!(!app.pane_set().right.contains(&Pane::Source));
        app.on_key(KeyCode::Char('3'), &[]);
        assert!(app.pane_set().right.contains(&Pane::Stack));
    }

    #[test]
    fn typing_is_free_and_submit_goes_through_commands() {
        let mut app = AttachedApp::new(fid(1));
        app.view = View::Running; // digits must still type, not toggle
        for c in "d1 sq".chars() {
            assert_eq!(app.on_key(KeyCode::Char(c), &[]), KeyAction::None);
        }
        assert_eq!(app.input, "d1 sq");
        assert_eq!(app.view, View::Running, "no debug keys fired while typing");
        assert!(!app.quit);
        assert_eq!(
            app.on_key(KeyCode::Enter, &[]),
            KeyAction::Submit("d1 sq".into())
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn tab_cycles_agents_and_digits_select_in_full_debug() {
        let agents = [fid(1), fid(5)];
        let mut app = AttachedApp::new(fid(1));
        app.on_key(KeyCode::Tab, &agents);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Tab, &agents);
        assert_eq!(app.selected, Some(fid(1)));

        app.focus = Focus::Debug;
        app.on_key(KeyCode::Char('d'), &agents);
        assert_eq!(app.view, View::FullDebug);
        app.on_key(KeyCode::Char('2'), &agents);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Char('1'), &agents);
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
        session
            .handle()
            .send(SessionCommand::UserTurn("delegate".into()));

        // Pump until both agents are live with running programs.
        for _ in 0..200 {
            let agents = session.agents();
            if agents.len() == 2 && agents.iter().all(|(_, s)| *s == "running") {
                break;
            }
            assert!(session.pump_one(), "session ended early");
        }
        let agents = session.agents();
        assert_eq!(agents.len(), 2, "{agents:?}");

        // Selecting each agent yields its own VM: different programs.
        let sources: Vec<String> = agents
            .iter()
            .map(|(id, _)| session.state(*id).unwrap().vm().unwrap().source.to_string())
            .collect();
        assert!(sources[0].contains("tools.agent"), "{sources:?}");
        assert!(sources[1].contains("tools.slow"), "{sources:?}");

        // And the whole thing still settles cleanly: the root yields its
        // final answer back to the user (the top conversation never ends).
        while session.pump_one() {}
        assert!(session.is_awaiting_user());
    }
}

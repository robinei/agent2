//! Attached mode (9_TUI Step 4): the same debug panes over a live
//! harness session — this *is* the harness TUI (decision 6). The chat
//! pane consumes `SessionEvent`s only (`chat.rs`); user input goes
//! through `SessionCommand`; the debug panes borrow the selected
//! frame's VM and the tree directly because rendering happens on the
//! loop thread (decision 4): crossterm input arrives as inbox messages
//! via a cloned `SessionHandle`, and we render after draining.
//!
//! Layout state machine (pure UI state — nothing in the host changes):
//! - **Chat** (default): full-width chat.
//! - **Running**: auto-popped when the *selected* frame starts a
//!   `run_program` — source + console as a right column, sticky after
//!   completion for post-mortem reading; `c` collapses back, `1`–`4`
//!   override the auto-pop set.
//! - **FullDebug** (`d`): the standalone layout — console/result left,
//!   full debug pane stack right, chat hidden; `1`–`9` switch frames.
//!
//! Keys are focus-modal so chat typing stays free: printable keys go
//! to the input line; `Esc` swaps to debug-control focus (and back).

use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{Event as CtEvent, KeyCode};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::chat::{ChatKind, ChatState};
use super::ui;
use crate::host::{FrameId, Session, SessionCommand, SessionEvent};
use crate::machine::TOOL_RUN_PROGRAM;
use crate::types::{EventPayload, Message};

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
    FrameList,
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

pub struct AttachedApp {
    pub chat: ChatState,
    pub view: View,
    /// Where `d` returns to from FullDebug.
    prev_view: View,
    pub focus: Focus,
    pub selected: Option<FrameId>,
    pub input: String,
    pub quit: bool,
    // Running-view pane toggles; console is part of the auto-pop set
    // and stays.
    pub show_source: bool,
    pub show_disasm: bool,
    pub show_stack: bool,
    pub show_promises: bool,
}

impl AttachedApp {
    pub fn new(root: FrameId) -> Self {
        AttachedApp {
            chat: ChatState::new(),
            view: View::Chat,
            prev_view: View::Chat,
            focus: Focus::Input,
            selected: Some(root),
            input: String::new(),
            quit: false,
            show_source: true,
            show_disasm: false,
            show_stack: false,
            show_promises: false,
        }
    }

    /// Feed one `SessionEvent`: updates the transcript and drives the
    /// auto-pop — the selected frame starting a `run_program` pops the
    /// source + console column (decision 6).
    pub fn apply(&mut self, event: &SessionEvent) {
        if let SessionEvent::Event { frame, event } = event
            && Some(*frame) == self.selected
            && matches!(
                &event.payload,
                EventPayload::Message(Message::Assistant { tool_calls, .. })
                    if tool_calls.iter().any(|c| c.name == TOOL_RUN_PROGRAM)
            )
            && self.view == View::Chat
        {
            self.view = View::Running;
            // A fresh pop resets to the auto-pop set; later manual
            // toggles override it until the next pop.
            self.show_source = true;
            self.show_disasm = false;
            self.show_stack = false;
            self.show_promises = false;
        }
        self.chat.apply(event);
    }

    /// The layout state machine's output: view state in, pane set out.
    pub fn pane_set(&self) -> PaneSet {
        match self.view {
            View::Chat => PaneSet {
                chat: true,
                console_left: false,
                right: Vec::new(),
            },
            View::Running => {
                let mut right = vec![Pane::FrameList];
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
                    Pane::FrameList,
                    Pane::Source,
                    Pane::Disasm,
                    Pane::Stack,
                    Pane::Promises,
                ],
            },
        }
    }

    pub fn on_key(&mut self, code: KeyCode, frames: &[FrameId]) -> KeyAction {
        // Frame switching works everywhere.
        if code == KeyCode::Tab {
            self.cycle_frame(frames);
            return KeyAction::None;
        }
        match self.view {
            View::FullDebug => self.on_debug_key(code, frames),
            View::Chat | View::Running => match self.focus {
                Focus::Input => self.on_input_key(code),
                Focus::Debug => self.on_debug_key(code, frames),
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

    fn on_debug_key(&mut self, code: KeyCode, frames: &[FrameId]) -> KeyAction {
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
                    // 1–9 switch which frame the panes borrow.
                    if let Some(id) = frames.get(idx) {
                        self.selected = Some(*id);
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

    fn cycle_frame(&mut self, frames: &[FrameId]) {
        if frames.is_empty() {
            return;
        }
        let next = match self
            .selected
            .and_then(|s| frames.iter().position(|f| *f == s))
        {
            Some(i) => (i + 1) % frames.len(),
            None => 0,
        };
        self.selected = Some(frames[next]);
    }
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
    let result = loop {
        let mut inputs = Vec::new();
        session.pump_until(Instant::now() + super::REDRAW_EVERY, &mut inputs);
        for event in events_rx.try_iter() {
            app.apply(&event);
        }
        let frames: Vec<FrameId> = session.frames().iter().map(|(id, _)| *id).collect();
        for input in inputs {
            let CtEvent::Key(key) = input else { continue };
            if !key.is_press() {
                continue;
            }
            let action = app.on_key(key.code, &frames);
            let Some(selected) = app.selected else {
                continue;
            };
            match action {
                KeyAction::None => {}
                KeyAction::Submit(text) => handle.send(SessionCommand::UserTurn(text)),
                KeyAction::TogglePause => {
                    let paused = session.is_paused(selected);
                    session.set_paused(selected, !paused);
                }
                KeyAction::StepInstr => {
                    session.set_paused(selected, true);
                    session.step_paused(selected, 1);
                }
                KeyAction::StepLine => step_line(&mut session, selected),
            }
        }
        if app.quit {
            break Ok(());
        }
        if let Err(e) = terminal.draw(|frame| render(frame, &app, &session)) {
            break Err(e.to_string());
        }
    };
    ratatui::restore();
    result
}

/// Step the selected frame's VM until its source line changes (or the
/// program yields/finishes, or the cap is hit).
fn step_line(session: &mut Session, frame: FrameId) {
    session.set_paused(frame, true);
    let line_of = |session: &Session| {
        session
            .state(frame)
            .and_then(|s| s.vm())
            .and_then(super::panes::current_line)
    };
    let start = line_of(session);
    for _ in 0..LINE_STEP_CAP {
        session.step_paused(frame, 1);
        let state = session.state(frame);
        if !state.map(|s| s.status() == "running").unwrap_or(false) {
            return; // blocked on the host, suspended, or finished
        }
        if start.is_none() || line_of(session) != start {
            return;
        }
    }
}

// ── rendering ───────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &AttachedApp, session: &Session) {
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
        render_chat(frame, app, left);
    } else if panes.console_left {
        render_attached_console(frame, app, session, left);
    }

    if let Some(right) = right {
        let vm = app
            .selected
            .and_then(|f| session.state(f))
            .and_then(|s| s.vm());
        let slots = Layout::vertical(panes.right.iter().map(|p| match p {
            Pane::FrameList => Constraint::Length(session.frames().len() as u16 + 2),
            _ => Constraint::Fill(1),
        }))
        .split(right);
        for (pane, slot) in panes.right.iter().zip(slots.iter()) {
            match (pane, vm) {
                (Pane::FrameList, _) => render_frame_list(frame, app, session, *slot),
                (Pane::Console, _) => render_attached_console(frame, app, session, *slot),
                (Pane::Source, Some(vm)) => ui::render_source(frame, vm, *slot),
                (Pane::Disasm, Some(vm)) => ui::render_disasm(frame, vm, *slot),
                (Pane::Stack, Some(vm)) => ui::render_stack(frame, vm, *slot),
                (Pane::Promises, Some(vm)) => ui::render_promises(frame, vm, None, *slot),
                (pane, None) => render_placeholder(frame, *pane, *slot),
            }
        }
    }

    let help = match (app.view, app.focus) {
        (View::FullDebug, _) => {
            " d/esc chat · tab/1-9 frame · space run/pause · s step · n step line · q quit "
        }
        (_, Focus::Input) => " type to chat · enter send · tab frame · esc debug keys ",
        (View::Running, Focus::Debug) => {
            " esc/i type · c collapse · d debugger · 1-4 panes · space/s/n vm · tab frame · q quit "
        }
        (_, Focus::Debug) => " esc/i type · d debugger · space/s/n vm · tab frame · q quit ",
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().add_modifier(Modifier::REVERSED)),
        footer,
    );
}

fn chat_style(kind: ChatKind) -> Style {
    match kind {
        ChatKind::User => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ChatKind::Assistant => Style::default(),
        ChatKind::Streaming => Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC),
        ChatKind::ToolCall => Style::default().fg(Color::Yellow),
        ChatKind::ToolResult => Style::default().fg(Color::DarkGray),
        ChatKind::Marker => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        ChatKind::Error => Style::default().fg(Color::Red),
    }
}

fn render_chat(frame: &mut Frame, app: &AttachedApp, area: Rect) {
    let [transcript_area, input_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(area);

    let lines: Vec<Line> = app
        .chat
        .rows()
        .into_iter()
        .map(|(kind, text)| Line::from(text).style(chat_style(kind)))
        .collect();
    let visible = transcript_area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(visible);
    frame.render_widget(
        Paragraph::new(lines[skip..].to_vec())
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
}

fn render_frame_list(frame: &mut Frame, app: &AttachedApp, session: &Session, area: Rect) {
    let lines: Vec<Line> = session
        .frames()
        .iter()
        .enumerate()
        .map(|(i, (id, status))| {
            let selected = app.selected == Some(*id);
            let busy = matches!(*status, "running" | "awaiting llm");
            let paused = session.is_paused(*id);
            let text = format!(
                "{} {} frame #{} · {}{}",
                if selected { "▶" } else { " " },
                i + 1,
                id.as_u64(),
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
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(text).style(style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" frames ")),
        area,
    );
}

/// Console + status for the selected frame's VM (live or post-mortem).
fn render_attached_console(frame: &mut Frame, app: &AttachedApp, session: &Session, area: Rect) {
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
    let skip = lines.len().saturating_sub(visible);
    frame.render_widget(
        Paragraph::new(lines[skip..].to_vec())
            .block(Block::default().borders(Borders::ALL).title(" console ")),
        area,
    );
}

fn render_placeholder(frame: &mut Frame, pane: Pane, area: Rect) {
    let title = match pane {
        Pane::Source => " source [1] ",
        Pane::Disasm => " disassembly [2] ",
        Pane::Stack => " stack [3] ",
        Pane::Promises => " promises [4] ",
        Pane::FrameList => " frames ",
        Pane::Console => " console ",
    };
    frame.render_widget(
        Paragraph::new("(no program yet)")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
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

    fn fid(n: u64) -> FrameId {
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
        // Sticky: the program completed and the frame finished, but the
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
                right: vec![]
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
                Pane::FrameList,
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
    fn tab_cycles_frames_and_digits_select_in_full_debug() {
        let frames = [fid(1), fid(5)];
        let mut app = AttachedApp::new(fid(1));
        app.on_key(KeyCode::Tab, &frames);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Tab, &frames);
        assert_eq!(app.selected, Some(fid(1)));

        app.focus = Focus::Debug;
        app.on_key(KeyCode::Char('d'), &frames);
        assert_eq!(app.view, View::FullDebug);
        app.on_key(KeyCode::Char('2'), &frames);
        assert_eq!(app.selected, Some(fid(5)));
        app.on_key(KeyCode::Char('1'), &frames);
        assert_eq!(app.selected, Some(fid(1)));
    }

    /// Two live frames (caller + in-flight subagent): both appear in
    /// the frame list, and switching retargets the VM the panes borrow.
    #[test]
    fn concurrent_frames_list_and_retarget() {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "slow".into(),
            description: String::new(),
            input_schema: json!({}),
            output_schema: json!({}),
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
            json!(null),
            registry,
            Box::new(ScriptedLlm::new(script)),
            tx,
        )
        .unwrap();
        session
            .handle()
            .send(SessionCommand::UserTurn("delegate".into()));

        // Pump until both frames are live with running programs.
        for _ in 0..200 {
            let frames = session.frames();
            if frames.len() == 2 && frames.iter().all(|(_, s)| *s == "running") {
                break;
            }
            assert!(session.pump_one(), "session ended early");
        }
        let frames = session.frames();
        assert_eq!(frames.len(), 2, "{frames:?}");

        // Selecting each frame yields its own VM: different programs.
        let sources: Vec<String> = frames
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

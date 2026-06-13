//! Debugger app state + key dispatch — terminal-free (unit-testable);
//! rendering lives in `ui.rs`.

use ratatui::crossterm::event::{KeyCode, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::runner::{RunState, Runner};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaneId {
    Console,
    Source,
    Disasm,
    Stack,
    Promises,
}

#[derive(Debug, Clone, Copy)]
pub struct PaneInfo {
    pub area: Rect,
    pub scroll_top: usize,
}

pub struct App {
    pub runner: Runner,
    pub path: String,
    pub quit: bool,
    pub show_source: bool,
    pub show_disasm: bool,
    pub show_stack: bool,
    pub show_promises: bool,
    pub console_scroll: Option<usize>,
    pub source_scroll: Option<usize>,
    pub disasm_scroll: Option<usize>,
    pub stack_scroll: Option<usize>,
    pub promises_scroll: Option<usize>,
    pub pane_rects: Vec<(PaneId, PaneInfo)>,
    last_console_len: usize,
}

impl App {
    pub fn new(runner: Runner, path: String) -> Self {
        let last_console_len = runner.vm.console_lines.len();
        App {
            runner,
            path,
            quit: false,
            show_source: true,
            show_disasm: true,
            show_stack: true,
            show_promises: false,
            console_scroll: None,
            source_scroll: None,
            disasm_scroll: None,
            stack_scroll: None,
            promises_scroll: None,
            pane_rects: Vec::new(),
            last_console_len,
        }
    }

    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char(' ') => {
                self.runner.toggle_run();
                self.reset_scrolls();
            }
            KeyCode::Char('s') => {
                self.runner.step_instr();
                self.reset_scrolls();
            }
            KeyCode::Char('n') => {
                self.runner.step_line();
                self.reset_scrolls();
            }
            KeyCode::Char('r') => {
                self.runner.resume_condition();
                self.reset_scrolls();
            }
            KeyCode::Char('1') => self.show_source = !self.show_source,
            KeyCode::Char('2') => self.show_disasm = !self.show_disasm,
            KeyCode::Char('3') => self.show_stack = !self.show_stack,
            KeyCode::Char('4') => self.show_promises = !self.show_promises,
            _ => {}
        }
    }

    pub fn reset_scrolls(&mut self) {
        self.console_scroll = None;
        self.source_scroll = None;
        self.disasm_scroll = None;
        self.stack_scroll = None;
        self.promises_scroll = None;
    }

    pub fn auto_reset_console_scroll(&mut self) {
        let current = self.runner.vm.console_lines.len();
        if current != self.last_console_len {
            self.console_scroll = None;
            self.last_console_len = current;
        }
    }

    pub fn on_mouse(&mut self, event: MouseEvent) {
        let delta: i64 = match event.kind {
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::ScrollUp => -3,
            _ => return,
        };
        for (id, info) in &self.pane_rects {
            if !info.area.contains(Position::new(event.column, event.row)) {
                continue;
            }
            let current = info.scroll_top as i64;
            let new = (current + delta).max(0) as usize;
            match id {
                PaneId::Console => self.console_scroll = Some(new),
                PaneId::Source => self.source_scroll = Some(new),
                PaneId::Disasm => self.disasm_scroll = Some(new),
                PaneId::Stack => self.stack_scroll = Some(new),
                PaneId::Promises => self.promises_scroll = Some(new),
            }
            return;
        }
    }

    pub fn status(&self) -> String {
        match &self.runner.state {
            RunState::Paused => "paused".to_string(),
            RunState::Running => "running".to_string(),
            RunState::Waiting => "waiting (pending tools)".to_string(),
            RunState::Condition { condition, .. } => format!("condition: {condition}"),
            RunState::Done { .. } => "done".to_string(),
            RunState::Failed { .. } => "failed".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(src: &str) -> App {
        let prog = interp::compile(src).expect("compiles");
        let runner = Runner::new(prog, serde_json::Value::Null).expect("vm");
        App::new(runner, "<test>".into())
    }

    #[test]
    fn step_key_advances_exactly_one_instruction() {
        let mut a = app("let x = 1;\nlet y = 2;\nreturn x + y;");
        let ip0 = a.runner.vm.ip;
        a.on_key(KeyCode::Char('s'));
        assert_eq!(a.runner.vm.ip, ip0 + 1);
        assert_eq!(a.runner.state, RunState::Paused);
    }

    #[test]
    fn space_toggles_run_pause_and_q_quits() {
        let mut a = app("return 1;");
        a.on_key(KeyCode::Char(' '));
        assert_eq!(a.runner.state, RunState::Running);
        a.on_key(KeyCode::Char(' '));
        assert_eq!(a.runner.state, RunState::Paused);
        a.on_key(KeyCode::Char('q'));
        assert!(a.quit);
    }
}

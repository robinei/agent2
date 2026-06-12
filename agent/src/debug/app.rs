//! Debugger app state + key dispatch — terminal-free (unit-testable);
//! rendering lives in `ui.rs`.

use ratatui::crossterm::event::KeyCode;

use super::runner::{RunState, Runner};

pub struct App {
    pub runner: Runner,
    /// Source path, shown in the title bar.
    pub path: String,
    pub quit: bool,
}

impl App {
    pub fn new(runner: Runner, path: String) -> Self {
        App {
            runner,
            path,
            quit: false,
        }
    }

    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char(' ') => self.runner.toggle_run(),
            KeyCode::Char('s') => self.runner.step_instr(),
            KeyCode::Char('n') => self.runner.step_line(),
            KeyCode::Char('r') => self.runner.resume_condition(),
            _ => {}
        }
    }

    /// One status line for the right pane / title.
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

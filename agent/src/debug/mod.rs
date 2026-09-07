//! `agent debug <file.js>` — the standalone debugger (9_TUI Step 2):
//! compile a program, run it under the fuel slicer with stub tools, and
//! drive it interactively. Renders on the main thread; input is polled
//! between slices, redraws are throttled (~30ms).

mod app;
mod attach;
mod chat;
mod highlight;
mod input;
mod markdown;
mod panes;
mod runner;
mod ui;

pub use attach::run_attached;

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event};

use app::App;
use runner::{RunState, Runner, now_ms};

const REDRAW_EVERY: Duration = Duration::from_millis(30);

pub fn run(path: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let prog = interp::compile(&src).map_err(|errs| {
        errs.iter()
            .map(|d| d.render(&src))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    // `input.now` stands in for the `Date.now()` this VM doesn't have yet,
    // so a program can build an absolute `wait_until` deadline.
    let runner = Runner::new(prog, serde_json::json!({ "now": now_ms() }))?;
    let mut app = App::new(runner, path.to_string());

    let mut terminal = ratatui::init();
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    )
    .map_err(|e| e.to_string())?;
    let result = event_loop(&mut terminal, &mut app);
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    )
    .map_err(|e| e.to_string())?;
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<(), String> {
    let mut last_draw = Instant::now() - REDRAW_EVERY;
    loop {
        if app.quit {
            return Ok(());
        }
        app.runner.poll_timers();
        app.runner.tick();

        app.auto_reset_console_scroll();

        if last_draw.elapsed() >= REDRAW_EVERY {
            terminal
                .draw(|agent| ui::render(agent, app))
                .map_err(|e| e.to_string())?;
            last_draw = Instant::now();
        }

        let timeout = if app.runner.state == RunState::Running {
            Duration::from_millis(1)
        } else {
            let heartbeat = Duration::from_millis(100);
            app.runner
                .next_timer()
                .map(|t| t.saturating_duration_since(Instant::now()).min(heartbeat))
                .unwrap_or(heartbeat)
        };
        if event::poll(timeout).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) if key.is_press() => app.on_key(key.code),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => {}
            }
        }
    }
}

//! `agent debug <file.js>` — the standalone debugger (9_TUI Step 2):
//! compile a program, run it under the fuel slicer with stub tools, and
//! drive it interactively. Renders on the main thread; input is polled
//! between slices, redraws are throttled (~30ms).

mod app;
mod highlight;
mod panes;
mod runner;
mod ui;

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event};

use app::App;
use runner::{RunState, Runner};

const REDRAW_EVERY: Duration = Duration::from_millis(30);

pub fn run(path: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let prog = interp::compile(&src).map_err(|errs| {
        errs.iter()
            .map(|d| d.render(&src))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let runner = Runner::new(prog, serde_json::Value::Null)?;
    let mut app = App::new(runner, path.to_string());

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
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

        if last_draw.elapsed() >= REDRAW_EVERY {
            terminal
                .draw(|frame| ui::render(frame, app))
                .map_err(|e| e.to_string())?;
            last_draw = Instant::now();
        }

        // While running, poll briefly so slices keep coming; while idle,
        // wait up to the next timer (or a UI heartbeat) for a key.
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
                _ => {}
            }
        }
    }
}

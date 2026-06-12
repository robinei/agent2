//! Rendering for the standalone debugger (Step 2 shell): console pane on
//! the left, a status pane on the right (the real debug panes are
//! Step 3), and a help footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::App;
use super::runner::RunState;

pub fn render(frame: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(main);

    render_console(frame, app, left);
    render_status(frame, app, right);

    let help = " q quit · space run/pause · s step instr · n step line · r resume condition ";
    frame.render_widget(
        Paragraph::new(help).style(Style::default().add_modifier(Modifier::REVERSED)),
        footer,
    );
}

fn render_console(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line> = app
        .runner
        .vm
        .console_lines
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();
    match &app.runner.state {
        RunState::Done { value } => lines.push(Line::from(format!("⇒ {value}"))),
        RunState::Failed { error } => {
            for l in error.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }
        RunState::Condition { condition, payload } => {
            lines.push(Line::from(format!("⚡ condition `{condition}` {payload}")));
            lines.push(Line::from("   r — resume with null"));
        }
        _ => {}
    }
    // Tail-scroll: keep the last lines visible.
    let visible = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(visible);
    let para = Paragraph::new(lines[skip..].to_vec()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" console — {} ", app.path)),
    );
    frame.render_widget(para, area);
}

fn render_status(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let vm = &app.runner.vm;
    let mut lines: Vec<Line> = vec![
        Line::from(format!("state  {}", app.status())),
        Line::from(format!(
            "ip     {}{}",
            vm.ip,
            app.runner
                .current_line()
                .map(|l| format!("  (line {l})"))
                .unwrap_or_default()
        )),
        Line::from(format!("instr  {}", vm.disasm_line(vm.ip))),
        Line::from(""),
        Line::from("frames (innermost last):"),
    ];
    for f in vm.frames() {
        lines.push(Line::from(format!(
            "  {}  [{} locals, {} temps]",
            f.name(),
            f.locals.len(),
            f.temps.len()
        )));
    }
    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" status "));
    frame.render_widget(para, area);
}

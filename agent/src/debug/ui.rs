//! Rendering for the debugger (9_TUI Step 3): console pane on the left;
//! a status bar and the toggleable debug panes (source, disassembly,
//! stack, promises) stacked on the right. Pane *content* comes from
//! `panes.rs` / `highlight.rs`; this module only styles it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::App;
use super::highlight::{self, Kind};
use super::panes;
use super::runner::RunState;

pub fn render(frame: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(main);

    render_console(frame, app, left);

    let [status, panes_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(right);
    render_status(frame, app, status);

    type PaneFn = fn(&mut Frame, &App, Rect);
    let enabled: Vec<PaneFn> = [
        app.show_source.then_some(render_source as PaneFn),
        app.show_disasm.then_some(render_disasm as PaneFn),
        app.show_stack.then_some(render_stack as PaneFn),
        app.show_promises.then_some(render_promises as PaneFn),
    ]
    .into_iter()
    .flatten()
    .collect();
    if enabled.is_empty() {
        frame.render_widget(
            Paragraph::new("all panes hidden — 1 source · 2 disasm · 3 stack · 4 promises"),
            panes_area,
        );
    } else {
        let slots = Layout::vertical(vec![Constraint::Fill(1); enabled.len()]).split(panes_area);
        for (pane, slot) in enabled.into_iter().zip(slots.iter()) {
            pane(frame, app, *slot);
        }
    }

    let help =
        " q quit · space run/pause · s step · n step line · r resume · 1 src 2 asm 3 stk 4 prom ";
    frame.render_widget(
        Paragraph::new(help).style(Style::default().add_modifier(Modifier::REVERSED)),
        footer,
    );
}

fn render_console(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = app
        .runner
        .vm
        .console_lines
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();
    match &app.runner.state {
        RunState::Done { value } => {
            lines.push(Line::from(format!("⇒ {value}")).style(Style::default().fg(Color::Green)))
        }
        RunState::Failed { error } => {
            for l in error.lines() {
                lines.push(Line::from(l.to_string()).style(Style::default().fg(Color::Red)));
            }
        }
        RunState::Condition { condition, payload } => {
            lines.push(
                Line::from(format!("⚡ condition `{condition}` {payload}"))
                    .style(Style::default().fg(Color::Yellow)),
            );
            lines.push(Line::from("   r — resume with null"));
        }
        _ => {}
    }
    let visible = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(visible);
    let para = Paragraph::new(lines[skip..].to_vec()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" console — {} ", app.path)),
    );
    frame.render_widget(para, area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let vm = &app.runner.vm;
    let line = format!(
        " {} · ip {}{} ",
        app.status(),
        vm.ip,
        app.runner
            .current_line()
            .map(|l| format!(" · line {l}"))
            .unwrap_or_default()
    );
    frame.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn token_style(kind: Kind) -> Style {
    match kind {
        Kind::Keyword => Style::default().fg(Color::Cyan),
        Kind::Str => Style::default().fg(Color::Green),
        Kind::Number => Style::default().fg(Color::Magenta),
        Kind::Comment => Style::default().fg(Color::DarkGray),
        Kind::Ident | Kind::Punct => Style::default(),
    }
}

/// Source split into syntax-styled lines (tokens may span lines — block
/// comments, template strings — so split on newlines while emitting).
fn styled_source(src: &str) -> Vec<Line<'static>> {
    fn emit(
        text: &str,
        style: Style,
        cur: &mut Vec<Span<'static>>,
        lines: &mut Vec<Line<'static>>,
    ) {
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::from(std::mem::take(cur)));
            }
            if !piece.is_empty() {
                cur.push(Span::styled(piece.to_string(), style));
            }
        }
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut pos = 0;
    for tok in highlight::tokenize(src) {
        if tok.start > pos {
            emit(&src[pos..tok.start], Style::default(), &mut cur, &mut lines);
        }
        emit(
            &src[tok.start..tok.end],
            token_style(tok.kind),
            &mut cur,
            &mut lines,
        );
        pos = tok.end;
    }
    if pos < src.len() {
        emit(&src[pos..], Style::default(), &mut cur, &mut lines);
    }
    lines.push(Line::from(cur));
    lines
}

fn render_source(frame: &mut Frame, app: &App, area: Rect) {
    let src: &str = &app.runner.vm.source;
    let cur_line = app.runner.current_line();
    let mut lines = styled_source(src);
    let num_style = Style::default().fg(Color::DarkGray);
    for (i, line) in lines.iter_mut().enumerate() {
        let n = i + 1;
        line.spans
            .insert(0, Span::styled(format!("{n:>4} "), num_style));
        if Some(n) == cur_line {
            *line = std::mem::take(line).style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
    let height = area.height.saturating_sub(2) as usize;
    let top = cur_line
        .unwrap_or(1)
        .saturating_sub(height / 2 + 1)
        .min(lines.len().saturating_sub(height));
    let end = (top + height).min(lines.len());
    let para = Paragraph::new(lines[top..end].to_vec())
        .block(Block::default().borders(Borders::ALL).title(" source [1] "));
    frame.render_widget(para, area);
}

fn op_style(op: &str) -> Style {
    match panes::op_kind(op) {
        panes::OpKind::Push => Style::default().fg(Color::Magenta),
        panes::OpKind::Control => Style::default().fg(Color::Cyan),
        panes::OpKind::Effect => Style::default().fg(Color::Yellow),
        panes::OpKind::Other => Style::default().fg(Color::White),
    }
}

fn render_disasm(frame: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let rows = panes::disasm_window(&app.runner.vm, height);
    let dim = Style::default().fg(Color::DarkGray);
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|r| match r {
            panes::AsmRow::Header(text) => Line::from(text).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            panes::AsmRow::Instr {
                ip,
                op,
                args,
                line,
                current,
            } => {
                let mut spans = vec![
                    Span::styled(format!("{ip:>5}  "), dim),
                    Span::styled(op.clone(), op_style(&op)),
                ];
                if !args.is_empty() {
                    spans.push(Span::raw("("));
                    spans.push(Span::styled(args, Style::default().fg(Color::Green)));
                    spans.push(Span::raw(")"));
                }
                if let Some(l) = line {
                    spans.push(Span::styled(format!("  @{l}"), dim));
                }
                let mut out = Line::from(spans);
                if current {
                    out = out.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                out
            }
        })
        .collect();
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" disassembly [2] "),
    );
    frame.render_widget(para, area);
}

fn render_stack(frame: &mut Frame, app: &App, area: Rect) {
    // Region backgrounds: the frame band (header + locals) on one shade,
    // the temporaries band on a lighter one. Rows are padded to the pane
    // width so the bands render solid.
    let frame_bg = Color::Indexed(235);
    let temp_bg = Color::Indexed(238);
    let width = area.width.saturating_sub(2) as usize;
    let lines: Vec<Line> = panes::stack_rows(&app.runner.vm)
        .into_iter()
        .map(|r| {
            let style = match r.kind {
                panes::StackRowKind::FrameHeader => Style::default()
                    .fg(Color::Yellow)
                    .bg(frame_bg)
                    .add_modifier(Modifier::BOLD),
                panes::StackRowKind::Local => Style::default().bg(frame_bg),
                panes::StackRowKind::Temp => Style::default().bg(temp_bg),
            };
            Line::from(format!("{:<width$}", r.text)).style(style)
        })
        .collect();
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" stack (innermost first · dark frame · light temps) [3] "),
    );
    frame.render_widget(para, area);
}

fn render_promises(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = panes::promise_rows(&app.runner.vm)
        .into_iter()
        .map(Line::from)
        .collect();
    if let Some(due) = app.runner.next_timer() {
        let ms = due
            .saturating_duration_since(std::time::Instant::now())
            .as_millis();
        lines.push(
            Line::from(format!("⏲ sleep resolves in {ms}ms"))
                .style(Style::default().fg(Color::DarkGray)),
        );
    }
    if lines.is_empty() {
        lines.push(Line::from("(no promises yet)").style(Style::default().fg(Color::DarkGray)));
    }
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" promises [4] "),
    );
    frame.render_widget(para, area);
}

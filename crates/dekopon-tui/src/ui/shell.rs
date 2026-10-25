//! Manual scripts and their results in one wrapped, pageable transcript.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use super::Theme;
use crate::{
    app::{App, Mode},
    redact::sanitize_line,
};

fn lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &app.shell_history {
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(Theme::READ_ONLY)),
            Span::raw(sanitize_line(&entry.input)),
        ]));
        if !entry.output.is_empty() {
            for line in entry.output.split('\n') {
                lines.push(Line::raw(sanitize_line(line)));
            }
        }
        if let Some(exit) = entry.exit_code {
            lines.push(Line::styled(
                format!("[exit code: {exit}]"),
                Style::default().fg(if exit == 0 {
                    Theme::READ_ONLY
                } else {
                    Theme::DENIED
                }),
            ));
            if entry.truncated {
                lines.push(Line::styled(
                    "[output truncated by interpreter limits]",
                    Style::default().fg(Theme::DENIED),
                ));
            }
        } else {
            lines.push(Line::styled(
                "[running · Esc requests stop]",
                Style::default().fg(Theme::LOCAL_WRITE),
            ));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default().fg(Theme::READ_ONLY)),
        Span::raw(sanitize_line(&app.composer)),
    ]));
    lines
}

fn paragraph(app: &App) -> Paragraph<'static> {
    Paragraph::new(lines(app)).wrap(Wrap { trim: false })
}

fn max_top(app: &App) -> usize {
    let (width, height) = app.shell_viewport;
    paragraph(app)
        .line_count(width.max(1))
        .saturating_sub(height as usize)
}

/// Pages by visible rows, including rows created by wrapping wide or Unicode content.
pub fn page(app: &mut App, direction: isize) {
    let end = max_top(app);
    let top = app.shell_top.unwrap_or(end).min(end);
    let distance = usize::from(app.shell_viewport.1.max(1));
    let next = if direction < 0 {
        top.saturating_sub(distance)
    } else {
        top.saturating_add(distance).min(end)
    };
    app.shell_top = (next != end).then_some(next);
}

/// Jumps to the first visual row.
pub fn home(app: &mut App) {
    app.shell_top = Some(0);
}

/// Draws the command, output, and editable trailing prompt in the same viewport.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let inner = Block::bordered().inner(area);
    let height = inner.height as usize;
    let count = paragraph(app).line_count(inner.width.max(1));
    let end = count.saturating_sub(height);
    let top = app.shell_top.unwrap_or(end).min(end);
    let title = match &app.session {
        Some(session) => format!(
            " shell as {} · each line is its own script; variables do not carry over ",
            session.agent
        ),
        None => " shell · hop into an agent first ".to_owned(),
    };
    let title = sanitize_line(&title);
    let hint = if app.mode == Mode::Composing {
        " type · Enter runs · Tab switches · Esc leaves input "
    } else {
        " i to type · ? for keys "
    };
    frame.render_widget(
        paragraph(app)
            .scroll((top.min(u16::MAX as usize) as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_bottom(hint),
            ),
        area,
    );
}

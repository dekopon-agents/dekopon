//! A prompt bound to the same capability seam the agent's own sessions dispatch through.
//!
//! Not a simulation of the agent's shell — literally it. The same [`dekopon_shell::Interpreter`],
//! the same granted set, the same broker leg. What is refused here is refused there, for the same
//! reason, and a script that works here is a script a model can run.
//!
//! What it shows is the interpreter's own combined output rather than the structured call tree the
//! turn pane draws, because that tree is scoped to a turn and a line typed here belongs to no turn.
//! An operator wanting a call's exact JSON runs it through a turn, or reads it back from the
//! broker's audit chain under this session's trace prefix.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::Theme;
use crate::{
    app::{App, Mode},
    redact::sanitize_line,
};

/// Draws the shell scrollback and its prompt.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [scrollback, prompt] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &app.shell_history {
        lines.push(Line::from(vec![
            Span::styled("$ ", Style::default().fg(Theme::READ_ONLY)),
            Span::raw(sanitize_line(&entry.input)),
        ]));
        for line in entry.output.lines() {
            lines.push(Line::raw(sanitize_line(line)));
        }
        if entry.exit_code != 0 {
            lines.push(Line::styled(
                format!("[exit code: {}]", entry.exit_code),
                Style::default().fg(Theme::DENIED),
            ));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "try: cap --list",
            Style::default().fg(Theme::FORGOTTEN),
        ));
    }

    let height = scrollback.height.saturating_sub(2) as usize;
    let offset = lines.len().saturating_sub(height);

    // Said in the frame rather than left to be discovered: the interpreter keeps no state between
    // runs, so a variable set on one line is gone on the next.
    let title = match &app.session {
        Some(session) => format!(
            " shell as {} · each line is its own script; variables do not carry over ",
            session.agent
        ),
        None => " shell · hop into an agent first ".to_owned(),
    };

    frame.render_widget(
        Paragraph::new(lines)
            .scroll((offset.min(u16::MAX as usize) as u16, 0))
            .block(Block::default().borders(Borders::ALL).title(title)),
        scrollback,
    );

    let (prompt_title, style) = if app.mode == Mode::Composing {
        (" enter runs · esc cancels ", Style::default())
    } else {
        (" i to type ", Style::default().fg(Theme::FORGOTTEN))
    };
    frame.render_widget(
        Paragraph::new(sanitize_line(&app.composer))
            .style(style)
            .block(Block::default().borders(Borders::ALL).title(prompt_title)),
        prompt,
    );
}

//! The tab bar, the status line, and the keybinding overlay.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use super::Theme;
use crate::app::{App, Pane};

/// Shows the represented identity separately from the authenticated console peer.
pub fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let agent = app
        .session
        .as_ref()
        .map_or("picker", |session| session.agent.as_str());
    // A single overflowing line used to clip off the live-effects warning at 80 columns.
    // Keep that warning on its own first row; identity and scope have bounded separate rows.
    let [warning, model, context, identity, scope] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(area);
    for (row, label) in [
        (warning, "LIVE PROVIDERS — effects are real".to_owned()),
        (
            model,
            format!("model ({}): {}", app.model_source.label(), app.model),
        ),
        (
            context,
            format!(
                "agent: {agent} · profile: {}",
                app.profile.as_deref().unwrap_or("subject-only")
            ),
        ),
        (
            identity,
            format!(
                "subject: {} · console: dekopon-console (uid {})",
                app.subject,
                rustix::process::geteuid().as_raw()
            ),
        ),
        (scope, format!("session: {}", app.scope_label)),
    ] {
        frame.render_widget(
            Paragraph::new(crate::redact::sanitize_line(&label)).wrap(Wrap { trim: true }),
            row,
        );
    }
}

/// Draws the pane tabs.
pub fn draw_tabs(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = Vec::with_capacity(Pane::ORDER.len() * 2);
    for pane in Pane::ORDER {
        let style = if pane == app.pane {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Theme::FORGOTTEN)
        };
        spans.push(Span::styled(format!(" {} ", pane.title()), style));
        spans.push(Span::raw(" "));
    }
    if app.busy {
        spans.push(Span::styled(
            "· running",
            Style::default().fg(Theme::LOCAL_WRITE),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Draws the status lines: the notice across the full width, then the facts that must never be a
/// guess. A notice that still does not fit is an error summary; `e` opens its full report.
pub fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [left, right] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let notice = app.notice.as_ref().map_or_else(
        || Span::styled("? for keys", Style::default().fg(Theme::FORGOTTEN)),
        |notice| {
            let style = if notice.is_refusal {
                Style::default().fg(Theme::DENIED)
            } else {
                Style::default()
            };
            Span::styled(crate::redact::sanitize_line(&notice.text), style)
        },
    );
    frame.render_widget(Paragraph::new(Line::from(notice)), left);

    // The subject and the credential file decide what a session may do and whose token it spends.
    // Both are resolved rather than typed, so both are on screen rather than in an operator's head.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "{} · {}",
                app.subject,
                credential_label(&app.credential_path)
            ),
            Style::default().fg(Theme::FORGOTTEN),
        )))
        .alignment(Alignment::Right),
        right,
    );
}

/// The credential file's own name, which is the part that says whose credential this is.
fn credential_label(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Draws the keybinding overlay.
pub fn draw_help(frame: &mut Frame<'_>) {
    let area = centered(frame.area(), 72, 20);
    frame.render_widget(Clear, area);

    let keys = [
        ("tab / shift-tab", "next / previous pane"),
        ("j k  ↑ ↓", "move agent selection"),
        ("enter", "hop into agent; shell opens ready to type"),
        ("i", "resume typing in shell"),
        ("enter (typing)", "run one script"),
        ("esc", "request stop if busy, else leave input"),
        (
            "page up / down",
            "page wrapped shell rows (also while typing)",
        ),
        (
            "home / end",
            "shell start / follow bottom (Fn arrows on Mac)",
        ),
        ("e", "show the last error in full"),
        ("?", "this"),
        ("q", "quit"),
    ];
    let mut lines: Vec<Line<'_>> = keys
        .iter()
        .map(|(key, meaning)| {
            Line::from(vec![
                Span::styled(
                    format!("{key:>18}  "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(*meaning),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  terminals may intercept Fn/Command keys",
        Style::default().fg(Theme::FORGOTTEN),
    ));
    lines.push(Line::styled(
        "  stop is cooperative: calls already sent still complete",
        Style::default().fg(Theme::FORGOTTEN),
    ));
    lines.push(Line::styled(
        "  errors open in full; the last one is printed again on quit",
        Style::default().fg(Theme::FORGOTTEN),
    ));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        area,
    );
}

/// The full report as sanitised lines, wrapped and never cut short.
fn error_paragraph(app: &App) -> Paragraph<'static> {
    let text = app
        .last_error
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    Paragraph::new(
        text.split('\n')
            .map(|line| Line::raw(crate::redact::sanitize_line(line)))
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false })
}

/// How far the error pane can scroll at the current terminal size.
#[must_use]
pub fn error_max_scroll(app: &App) -> u16 {
    let (width, height) = app.shell_viewport;
    // The pane spans the whole frame less its top and bottom rules.
    let rows = error_paragraph(app).line_count(width.saturating_add(2).max(1));
    let visible = usize::from(height.saturating_add(9));
    u16::try_from(rows.saturating_sub(visible)).unwrap_or(u16::MAX)
}

/// The last error, full-screen. No side borders, so a terminal selection copies clean text.
pub fn draw_error(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(Theme::DENIED))
        .title(" error · select to copy · also on stderr and printed again on quit ")
        .title_bottom(" j/k ↑↓ PgUp/PgDn scroll · Esc/Enter/q close · e reopens ");
    let inner = block.inner(area);
    let paragraph = error_paragraph(app);
    let end = paragraph
        .line_count(inner.width.max(1))
        .saturating_sub(usize::from(inner.height));
    let top = usize::from(app.error_scroll).min(end);
    frame.render_widget(
        paragraph
            .scroll((u16::try_from(top).unwrap_or(u16::MAX), 0))
            .block(block),
        area,
    );
}

/// Centres a fixed-size box, clamped to what the terminal actually has.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

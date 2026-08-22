//! The conversation, and the tree of what each turn actually did.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::Theme;
use crate::{
    app::{App, Mode},
    record::{CallOutcome, CapabilityCall},
    redact::{Redacted, redact, sanitize_line},
    transcript::{ScriptNode, Turn, TurnStatus},
};

/// Draws the turn scrollback and the composer.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [scrollback, composer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, turn) in app.transcript.turns().iter().enumerate() {
        render_turn(&mut lines, index, turn, app);
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "nothing asked yet — press i and type",
            Style::default().fg(Theme::FORGOTTEN),
        ));
    }

    // The tail is what an operator is watching, so the view is pinned to the bottom rather than
    // scrolled from the top: a churning turn that scrolled off screen would be a turn nobody sees.
    let height = scrollback.height.saturating_sub(2) as usize;
    let offset = lines.len().saturating_sub(height);

    frame.render_widget(
        Paragraph::new(lines)
            .scroll((offset.min(u16::MAX as usize) as u16, 0))
            .block(Block::default().borders(Borders::ALL).title(" turns ")),
        scrollback,
    );

    let (title, style) = if app.mode == Mode::Composing {
        (" compose · enter sends · esc cancels ", Style::default())
    } else {
        (" i to compose ", Style::default().fg(Theme::FORGOTTEN))
    };
    frame.render_widget(
        Paragraph::new(sanitize_line(&app.composer))
            .style(style)
            .block(Block::default().borders(Borders::ALL).title(title)),
        composer,
    );
}

fn render_turn(lines: &mut Vec<Line<'static>>, index: usize, turn: &Turn, app: &App) {
    // A turn the model can no longer see is dimmed, which is the answer to "why did it forget".
    let base = if turn.in_replay_window {
        Style::default()
    } else {
        Style::default().fg(Theme::FORGOTTEN)
    };
    if !turn.in_replay_window {
        lines.push(Line::styled(
            "── outside the model's replay window ──",
            Style::default().fg(Theme::FORGOTTEN),
        ));
    }

    lines.push(Line::from(vec![
        Span::styled(
            format!("▾ turn {}  ", turn.ordinal),
            base.add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary(turn), base.fg(Theme::FORGOTTEN)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  ▸ ", base),
        Span::styled(sanitize_line(&turn.prompt), base),
    ]));

    for (script_index, script) in turn.scripts.iter().enumerate() {
        render_script(lines, (index, script_index), script, app, base);
    }

    match &turn.status {
        TurnStatus::Running => lines.push(Line::styled(
            if turn.stop_requested {
                "  ◆ stopping at the next boundary…"
            } else {
                "  ◆ running…"
            },
            base.fg(Theme::LOCAL_WRITE),
        )),
        TurnStatus::Answered(answer) => {
            for line in answer.lines() {
                lines.push(Line::from(vec![
                    Span::styled("  ◆ ", base.fg(Theme::READ_ONLY)),
                    Span::styled(sanitize_line(line), base),
                ]));
            }
        }
        TurnStatus::Suppressed => lines.push(Line::styled(
            "  ◆ the model declined to reply",
            base.fg(Theme::FORGOTTEN),
        )),
        TurnStatus::Failed(error) => lines.push(Line::from(vec![
            Span::styled("  ◆ ", base.fg(Theme::FAILED)),
            Span::styled(sanitize_line(error), base.fg(Theme::FAILED)),
        ])),
    }
    lines.push(Line::raw(""));
}

fn summary(turn: &Turn) -> String {
    let mut summary = format!(
        "{} scripts · {} calls",
        turn.scripts.len(),
        turn.capability_calls()
    );
    let denied = turn.denied_calls();
    if denied > 0 {
        summary.push_str(&format!(" · {denied} denied"));
    }
    match (turn.tokens.input, turn.tokens.output) {
        (Some(input), Some(output)) => summary.push_str(&format!(" · {input}/{output} tok")),
        // Reported nothing is not reported zero, and saying "0 tok" would be a number the provider
        // never gave.
        _ if turn.tokens.unreported > 0 => summary.push_str(" · tokens unreported"),
        _ => {}
    }
    summary
}

fn render_script(
    lines: &mut Vec<Line<'static>>,
    (turn_index, script_index): (usize, usize),
    script: &ScriptNode,
    app: &App,
    base: Style,
) {
    let status = script.outcome.as_ref().map_or_else(
        || "running".to_owned(),
        |outcome| format!("exit {}", outcome.exit_code),
    );
    lines.push(Line::from(vec![
        Span::styled("  ▾ bash  ", base.add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{status} · {} B", script.script.len()),
            base.fg(Theme::FORGOTTEN),
        ),
    ]));
    for line in script.script.lines().take(12) {
        lines.push(Line::from(vec![
            Span::styled("    │ ", base.fg(Theme::FORGOTTEN)),
            Span::styled(sanitize_line(line), base),
        ]));
    }

    for (call_index, call) in script.calls.iter().enumerate() {
        render_call(
            lines,
            (turn_index, script_index, call_index),
            call,
            app,
            base,
        );
    }

    if script
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.truncated)
    {
        lines.push(Line::styled(
            "    │ [output truncated at the interpreter's ceiling]",
            base.fg(Theme::LOCAL_WRITE),
        ));
    }
}

fn render_call(
    lines: &mut Vec<Line<'static>>,
    target: (usize, usize, usize),
    call: &CapabilityCall,
    app: &App,
    base: Style,
) {
    let colour = match &call.outcome {
        CallOutcome::Succeeded(_) => Theme::READ_ONLY,
        CallOutcome::Denied(_) => Theme::DENIED,
        CallOutcome::Failed(_) => Theme::FAILED,
        CallOutcome::NotFound => Theme::FORGOTTEN,
    };
    let expanded = app.expanded_call == Some(target);
    lines.push(Line::from(vec![
        Span::styled(
            format!("    {} ", if expanded { "▾" } else { "▸" }),
            base.fg(colour),
        ),
        Span::styled(sanitize_line(&call.capability), base.fg(colour)),
        Span::styled(
            format!("  {}  {}ms", call.outcome.label(), call.elapsed.as_millis()),
            base.fg(Theme::FORGOTTEN),
        ),
    ]));

    let input = redact(&call.input);
    lines.push(payload_line("input ", &input, base, app));

    match &call.outcome {
        CallOutcome::Succeeded(output) => {
            let output = redact(output);
            lines.push(payload_line("output", &output, base, app));
            if expanded {
                for line in pretty(&output.value).lines().take(200) {
                    lines.push(Line::from(vec![
                        Span::styled("      │ ", base.fg(Theme::FORGOTTEN)),
                        Span::styled(sanitize_line(line), base),
                    ]));
                }
            }
        }
        CallOutcome::Denied(reason) => lines.push(Line::from(vec![
            Span::styled("      denied  ", base.fg(Theme::DENIED)),
            Span::styled(sanitize_line(reason), base.fg(Theme::DENIED)),
        ])),
        CallOutcome::Failed(error) => lines.push(Line::from(vec![
            Span::styled("      failed  ", base.fg(Theme::FAILED)),
            Span::styled(sanitize_line(error), base.fg(Theme::FAILED)),
        ])),
        CallOutcome::NotFound => lines.push(Line::styled(
            "      no leg of this session claims that capability",
            base.fg(Theme::FORGOTTEN),
        )),
    }
}

fn payload_line(label: &str, payload: &Redacted, base: Style, app: &App) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("      {label}  "), base.fg(Theme::FORGOTTEN)),
        Span::styled(sanitize_line(&compact(&payload.value)), base),
    ];
    if !payload.is_clean() {
        let hidden = payload
            .redactions
            .iter()
            .filter(|redaction| !app.is_revealed(&redaction.path))
            .count();
        if hidden > 0 {
            spans.push(Span::styled(
                format!("  [{hidden} redacted · r to reveal]"),
                base.fg(Theme::REDACTED),
            ));
        }
    }
    Line::from(spans)
}

/// One-line rendering, bounded so a large result cannot become a very wide frame.
fn compact(value: &serde_json::Value) -> String {
    let rendered = value.to_string();
    if rendered.chars().count() <= 160 {
        return rendered;
    }
    let head: String = rendered.chars().take(157).collect();
    format!("{head}…")
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

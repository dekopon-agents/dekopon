//! What policy actually grants the attested subject through the open agent. The catalog declares
//! no capabilities; the broker's answer is the whole surface.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
};

use super::Theme;
use crate::{app::App, redact::sanitize_line};

/// Draws the capability surfaces for the agent currently hopped into.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(session) = app.session.as_ref() else {
        frame.render_widget(
            Paragraph::new("no agent open — go to the agents pane and press enter").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" capabilities "),
            ),
            area,
        );
        return;
    };

    let [table_area, help_area, words_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(4),
    ])
    .areas(area);

    let mut granted: Vec<&dekopon_agent::meta::EffectiveCapabilityView> =
        session.effective.iter().collect();
    granted.sort_unstable_by(|left, right| left.id.cmp(&right.id));

    let rows = granted.into_iter().map(|view| {
        Row::new(vec![
            Cell::from(sanitize_line(&view.id)),
            Cell::from(view.effect.as_str().to_owned()),
            Cell::from(view.risk.as_str().to_owned()),
            Cell::from(sanitize_line(&view.description)),
        ])
        .style(Style::default().fg(Theme::effect(&view.effect)))
    });

    let title = if session.is_empty() {
        format!(
            " {} · policy grants this subject nothing here ",
            session.agent
        )
    } else {
        format!(" {} · {} granted ", session.agent, session.effective.len())
    };

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(34),
                Constraint::Length(15),
                Constraint::Length(7),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(vec!["CAPABILITY", "EFFECT", "RISK", "DESCRIPTION"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title)),
        table_area,
    );

    // Provider-authored help pages, exactly as the model's script tool receives them.
    let mut help = Vec::new();
    for (word, page) in &session.command_word_help {
        help.push(Line::styled(
            sanitize_line(word),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        help.extend(page.lines().map(|line| Line::raw(sanitize_line(line))));
    }
    if help.is_empty() {
        help.push(Line::styled(
            "the broker sent no help pages",
            Style::default().fg(Theme::FORGOTTEN),
        ));
    }
    frame.render_widget(
        Paragraph::new(help).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" command help "),
        ),
        help_area,
    );

    let words = if session.command_words.is_empty() {
        Span::styled("none", Style::default().fg(Theme::FORGOTTEN))
    } else {
        Span::raw(sanitize_line(&session.command_words.join("  ")))
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(words),
            Line::from(format!(
                "broker trace: {}",
                app.broker_trace.as_deref().unwrap_or("not opened")
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" command words the bash tool will accept "),
        ),
        words_area,
    );
}

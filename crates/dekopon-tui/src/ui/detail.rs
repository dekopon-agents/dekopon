//! Declared versus effective: what the catalog says an agent may propose, beside what policy
//! actually grants it.
//!
//! This is the pane worth having. A capability the catalog declares and the broker withholds is
//! the answer to "why did the agent say it couldn't do that", and nothing else in the system shows
//! the two lists side by side.

use std::collections::BTreeMap;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
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

    let [table_area, words_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(4)]).areas(area);

    let effective: BTreeMap<&str, &dekopon_agent::meta::EffectiveCapabilityView> = session
        .effective
        .iter()
        .map(|view| (view.id.as_str(), view))
        .collect();
    let declared: Vec<String> = app
        .agents
        .iter()
        .find(|agent| agent.metadata.name == session.agent.as_str())
        .map(|agent| {
            agent
                .spec
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Every identifier from either side, so a capability policy grants that the catalog never
    // declared is as visible as one the catalog declared and policy withheld.
    let mut identifiers: Vec<&str> = declared.iter().map(String::as_str).collect();
    identifiers.extend(effective.keys().copied());
    identifiers.sort_unstable();
    identifiers.dedup();

    let rows = identifiers.into_iter().map(|id| {
        let granted = effective.get(id);
        let is_declared = declared.iter().any(|declared| declared == id);
        let (mark, style) = match (granted, is_declared) {
            (Some(view), _) => ("granted", Style::default().fg(Theme::effect(&view.effect))),
            (None, true) => ("denied", Style::default().fg(Theme::DENIED)),
            (None, false) => ("-", Style::default().fg(Theme::FORGOTTEN)),
        };
        Row::new(vec![
            Cell::from(sanitize_line(id)),
            Cell::from(if is_declared { "yes" } else { "no" }),
            Cell::from(mark),
            Cell::from(granted.map_or("-", |view| view.effect.as_str()).to_owned()),
            Cell::from(granted.map_or("-", |view| view.risk.as_str()).to_owned()),
            Cell::from(granted.map_or_else(String::new, |view| sanitize_line(&view.description))),
        ])
        .style(style)
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
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(15),
                Constraint::Length(7),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(vec![
                "CAPABILITY",
                "DECLARED",
                "POLICY",
                "EFFECT",
                "RISK",
                "DESCRIPTION",
            ])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title)),
        table_area,
    );

    let words = if session.command_words.is_empty() {
        Span::styled("none", Style::default().fg(Theme::FORGOTTEN))
    } else {
        Span::raw(sanitize_line(&session.command_words.join("  ")))
    };
    frame.render_widget(
        Paragraph::new(Line::from(words)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" command words the bash tool will accept "),
        ),
        words_area,
    );
}

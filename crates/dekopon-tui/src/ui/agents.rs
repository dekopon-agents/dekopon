//! The catalog's agents.

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Cell, Row, Table},
};

use super::Theme;
use crate::{app::App, redact::sanitize_line};

/// Draws the agent list.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = app.agents.iter().enumerate().map(|(index, agent)| {
        let selected = index == app.selected_agent;
        let hopped = app
            .session
            .as_ref()
            .is_some_and(|session| session.agent.as_str() == agent.metadata.name);
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else if agent.spec.enabled {
            Style::default()
        } else {
            Style::default().fg(Theme::FORGOTTEN)
        };
        Row::new(vec![
            Cell::from(if hopped { "▸" } else { " " }),
            Cell::from(sanitize_line(&agent.metadata.name)),
            Cell::from(if agent.spec.enabled {
                "enabled"
            } else {
                "disabled"
            }),
            Cell::from(
                agent
                    .spec
                    .model_class
                    .as_deref()
                    .map_or_else(|| "-".to_owned(), sanitize_line),
            ),
            Cell::from(agent.spec.capabilities.len().to_string()),
            Cell::from(sanitize_line(&agent.spec.description)),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(24),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Length(5),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec![
            "",
            "NAME",
            "STATUS",
            "MODEL CLASS",
            "CAPS",
            "DESCRIPTION",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" agents · enter to hop in "),
    );

    frame.render_widget(table, area);
}

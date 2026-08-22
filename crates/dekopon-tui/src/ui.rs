//! Drawing. Every decision here is about pixels; the state being drawn is owned by
//! [`crate::app`], which knows nothing about a terminal.
//!
//! One rule holds across every pane: **no borrowed text reaches a buffer unsanitised.** A pull
//! request title, an issue body, and a provider error are all attacker-controlled text arriving
//! through a read-only capability, and drawn raw they can move the cursor or repaint earlier
//! lines. [`crate::redact::sanitize_line`] is the only way text gets in.

pub mod agents;
pub mod chrome;
pub mod detail;
pub mod shell;
pub mod theme;
pub mod turns;

use ratatui::{
    Frame,
    layout::{Constraint, Layout},
};

pub use theme::Theme;

use crate::app::{App, Mode, Pane};

/// Draws the whole console for one frame.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let [tabs, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    chrome::draw_tabs(frame, tabs, app);
    match app.pane {
        Pane::Agents => agents::draw(frame, body, app),
        Pane::Detail => detail::draw(frame, body, app),
        Pane::Turns => turns::draw(frame, body, app),
        Pane::Shell => shell::draw(frame, body, app),
    }
    chrome::draw_status(frame, status, app);

    if app.mode == Mode::Help {
        chrome::draw_help(frame);
    }
}

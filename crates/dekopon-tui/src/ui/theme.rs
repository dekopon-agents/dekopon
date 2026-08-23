//! Colours, in one place so a pane cannot invent its own vocabulary.

use ratatui::style::Color;

/// The console's palette.
#[derive(Clone, Copy, Debug)]
pub struct Theme;

impl Theme {
    /// An external write, which is the classification worth seeing first.
    pub const EXTERNAL_WRITE: Color = Color::LightRed;
    /// A local write.
    pub const LOCAL_WRITE: Color = Color::LightYellow;
    /// A read.
    pub const READ_ONLY: Color = Color::LightGreen;
    /// Something policy refused.
    pub const DENIED: Color = Color::Red;
    /// Something that ran and failed.
    pub const FAILED: Color = Color::LightMagenta;
    /// Text outside the model's replay window.
    pub const FORGOTTEN: Color = Color::DarkGray;
    /// A redaction marker.
    pub const REDACTED: Color = Color::Yellow;

    /// The colour one trusted effect classification is drawn in.
    #[must_use]
    pub const fn effect(effect: &str) -> Color {
        match effect.as_bytes() {
            b"external-write" => Self::EXTERNAL_WRITE,
            b"local-write" => Self::LOCAL_WRITE,
            _ => Self::READ_ONLY,
        }
    }
}

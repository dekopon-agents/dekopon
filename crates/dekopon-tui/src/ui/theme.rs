use ratatui::style::Color;

#[derive(Clone, Copy, Debug)]
pub struct Theme;

impl Theme {
    pub const EXTERNAL_WRITE: Color = Color::LightRed;
    pub const LOCAL_WRITE: Color = Color::LightYellow;
    pub const READ_ONLY: Color = Color::LightGreen;
    pub const DENIED: Color = Color::Red;
    pub const FAILED: Color = Color::LightMagenta;
    pub const FORGOTTEN: Color = Color::DarkGray;
    pub const REDACTED: Color = Color::Yellow;

    #[must_use]
    pub const fn effect(effect: &str) -> Color {
        match effect.as_bytes() {
            b"external-write" => Self::EXTERNAL_WRITE,
            b"local-write" => Self::LOCAL_WRITE,
            _ => Self::READ_ONLY,
        }
    }
}

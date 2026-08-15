//! Colors and glyphs in one place, so the interface stays consistent.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const MUTED: Color = Color::DarkGray;
pub const DANGER: Color = Color::Red;
pub const ADDED: Color = Color::Green;
pub const BUSY: Color = Color::Yellow;
pub const CODE: Color = Color::LightGreen;

pub const PROMPT: &str = "❯ ";
pub const RULE: &str = "─";
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Startup logo. Every row is the same width, so the ramp below reads as one
/// block of color.
pub const LOGO: [&str; 4] = [
    r" _____ _  _  ___ _____ _  _ ",
    r"|_   _| || |/ _ \_   _| || |",
    r"  | | | __ | (_) || | | __ |",
    r"  |_| |_||_|\___/ |_| |_||_|",
];
pub const LOGO_WIDTH: usize = 28;

/// Cyan to blue, one shade per logo row. 256-color indexes rather than RGB:
/// they land on something sensible on terminals without truecolor.
pub const LOGO_RAMP: [Color; 4] = [
    Color::Indexed(51),
    Color::Indexed(45),
    Color::Indexed(39),
    Color::Indexed(33),
];

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn danger() -> Style {
    Style::default().fg(DANGER)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn muted_italic() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
}

pub fn key() -> Style {
    Style::default().fg(BUSY).add_modifier(Modifier::BOLD)
}

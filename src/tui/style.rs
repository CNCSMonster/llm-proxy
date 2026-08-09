#![allow(dead_code)] // TUI styles reserved for future screens
use ratatui::style::{Color, Modifier, Style};

pub const TITLE_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

pub const SELECTED_STYLE: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);

pub const HIGHLIGHT_STYLE: Style = Style::new().bg(Color::DarkGray);

pub const DIM_STYLE: Style = Style::new().fg(Color::DarkGray);

pub const ERROR_STYLE: Style = Style::new().fg(Color::Red);

pub const SUCCESS_STYLE: Style = Style::new().fg(Color::Green);

pub const WARNING_STYLE: Style = Style::new().fg(Color::Yellow);

pub const INSTRUCTION_STYLE: Style = Style::new().fg(Color::Gray);

pub const INPUT_STYLE: Style = Style::new().fg(Color::White).bg(Color::DarkGray);

pub const BORDER_STYLE: Style = Style::new().fg(Color::DarkGray);

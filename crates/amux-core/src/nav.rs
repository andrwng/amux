//! Directional navigation primitive, shared across the wire (proto), the mailbox (`amux nav`),
//! and the client's pane tree. One `Dir` everywhere so an in-pane program's `amux nav left` and
//! a `Ctrl+h` keypress mean the same thing.

use serde::{Deserialize, Serialize};

/// A cardinal direction for pane/split navigation and resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    /// Parse the single-letter forms `amux nav` accepts (`h`/`j`/`k`/`l` or names).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h" | "left" => Some(Dir::Left),
            "l" | "right" => Some(Dir::Right),
            "k" | "up" => Some(Dir::Up),
            "j" | "down" => Some(Dir::Down),
            _ => None,
        }
    }
}

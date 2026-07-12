//! Wire message types. Phase 0 subset: a single PTY, no agent ids yet (those arrive in
//! Phase 1 when the daemon manages many agents). See `docs/DESIGN.md` §6.

use serde::{Deserialize, Serialize};

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// Messages the client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First frame: the client's protocol version and terminal size.
    Hello { proto_version: u32, size: Size },
    /// Raw keystroke bytes destined for the PTY.
    Input(Vec<u8>),
    /// The client's viewport resized.
    Resize(Size),
    /// Ask the daemon to shut the session down.
    Shutdown,
}

/// Messages the daemon sends to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonMsg {
    /// Reply to `Hello`: the daemon's protocol version.
    Hello { proto_version: u32 },
    /// Full current screen as a `vt100` `contents_formatted()` dump, sent once on subscribe.
    OutputSnapshot(Vec<u8>),
    /// Incremental raw PTY output.
    Output(Vec<u8>),
    /// The PTY's child process exited.
    Exited { code: Option<i32> },
    /// A daemon-side error surfaced to the client.
    Error(String),
}

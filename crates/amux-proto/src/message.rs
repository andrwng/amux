//! Wire message types, v4 — agents own terminals. The sidebar lists **agents** (workspaces);
//! panes stream **terminals** (PTYs). An agent has a primary terminal (its CLI) plus any shell
//! terminals split off in the same worktree. See `docs/DESIGN.md` §6 and `docs/SPLITS.md`.

use amux_core::agent::{AgentId, AgentState, TerminalId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// The sidebar's view of one agent (workspace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub branch: String,
    pub state: AgentState,
    pub last_activity: DateTime<Utc>,
    /// The terminal that shows this agent's CLI (what "open in a pane" attaches to).
    pub primary_terminal: TerminalId,
}

/// Messages the client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First frame: the client's protocol version.
    Hello { proto_version: u32 },
    /// Request the current agent roster.
    ListAgents,
    /// Create a new agent on `branch` (worktree + a primary terminal).
    CreateAgent { branch: String },
    /// Delete an agent — kills all its terminals and removes its worktree. Only destructive op.
    DeleteAgent { id: AgentId, force: bool },
    /// Resume a suspended agent's primary terminal in its existing worktree.
    ResumeAgent { id: AgentId },
    /// Split: spawn a `$SHELL` terminal (with new id `terminal`) in the same worktree as `like`.
    SpawnShell {
        terminal: TerminalId,
        like: TerminalId,
    },
    /// Close a shell terminal (its pane was closed). No-op on a primary terminal.
    CloseTerminal { terminal: TerminalId },
    /// Start streaming a terminal into a pane (snapshot then live output).
    Attach { terminal: TerminalId, size: Size },
    /// Stop streaming a terminal (its pane closed / was replaced).
    Detach { terminal: TerminalId },
    /// Keystroke bytes for a terminal's PTY.
    Input {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    /// Resize a terminal's PTY.
    Resize { terminal: TerminalId, size: Size },
}

/// Messages the daemon sends to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonMsg {
    /// Reply to `Hello`: the daemon's protocol version.
    Hello { proto_version: u32 },
    /// The full roster (on connect and in reply to `ListAgents`).
    Agents(Vec<AgentInfo>),
    /// A new agent appeared.
    AgentAdded(AgentInfo),
    /// An agent was deleted.
    AgentRemoved { id: AgentId },
    /// An agent's state changed (the sidebar's live signal).
    StateChanged { id: AgentId, state: AgentState },
    /// Delete was refused because the worktree has uncommitted changes — confirm to force it.
    DeleteNeedsConfirm { id: AgentId, message: String },
    /// Full screen of a terminal as a `contents_formatted()` dump, sent on attach.
    OutputSnapshot {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    /// Incremental output from a terminal.
    Output {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    /// A terminal's process exited (a shell finished, or a primary's CLI exited).
    TerminalExited {
        terminal: TerminalId,
        code: Option<i32>,
    },
    /// A daemon-side error surfaced to the client.
    Error { message: String },
}

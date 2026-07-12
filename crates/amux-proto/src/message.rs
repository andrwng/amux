//! Wire message types, v1 — multi-agent. Every terminal-directed message carries an `AgentId`;
//! the client attaches to (streams) one agent at a time (minis are Phase 3), but manages and
//! sees the status of all of them. See `docs/DESIGN.md` §6.

use amux_core::agent::{AgentId, AgentState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// The sidebar's view of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub branch: String,
    pub state: AgentState,
    pub last_activity: DateTime<Utc>,
}

/// Messages the client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First frame: the client's protocol version.
    Hello { proto_version: u32 },
    /// Request the current agent roster.
    ListAgents,
    /// Create a new agent on `branch` (worktree + session).
    CreateAgent { branch: String },
    /// Delete an agent — kills its session and removes its worktree. The only destructive op.
    DeleteAgent { id: AgentId },
    /// Resume a suspended (exited) agent's session in its existing worktree.
    ResumeAgent { id: AgentId },
    /// Stream this agent into the main window (re-targets the single live stream). Sends a
    /// snapshot then live output; implicitly detaches whatever was streaming before.
    Attach { id: AgentId, size: Size },
    /// Keystroke bytes for a specific agent's PTY.
    Input { id: AgentId, bytes: Vec<u8> },
    /// Resize a specific agent's PTY.
    Resize { id: AgentId, size: Size },
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
    /// Full screen of the attached agent as a `contents_formatted()` dump, sent on attach.
    OutputSnapshot { id: AgentId, bytes: Vec<u8> },
    /// Incremental output from the attached agent.
    Output { id: AgentId, bytes: Vec<u8> },
    /// A daemon-side error surfaced to the client.
    Error { message: String },
}

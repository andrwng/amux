//! `amux-proto` — the versioned wire protocol between the `amux` client and the `amuxd`
//! daemon. Pure DTOs + a length-prefixed `postcard` codec, no sockets. See `docs/DESIGN.md`
//! §6. The socket wiring (`Framed`) lives at the call sites, which own the async runtime.

mod codec;
mod message;

pub use codec::{check_version, ClientCodec, ProtoError, ServerCodec, WireCodec, MAX_FRAME_BYTES};
pub use message::{AgentInfo, ClientMsg, DaemonMsg, Layout, RepoInfo, Size};

/// Protocol version. The client and daemon refuse to talk across a mismatch (the client
/// auto-recovers). v1 multi-agent; v2 dirty-delete; v3 multi-attach; v4 agents-own-terminals;
/// v5 multi-repo; v6 doctor (prune orphaned worktrees); v7 hook mailbox + real `claude` (bumped
/// so a pre-hook daemon is auto-refreshed even though the control wire is unchanged); v8 inbox
/// read/unread (AgentInfo.unread, Focus, UnreadChanged); v9 vim-aware nav (TerminalApp, Navigate);
/// v10 layout persistence (SetLayout, Layouts); v11 mini persistence (SetMinis, Minis); v12 active
/// agent persistence (SetActive, Active — restores the main pane on reconnect).
pub const PROTO_VERSION: u32 = 12;

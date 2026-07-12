//! `amux-proto` — the versioned wire protocol between the `amux` client and the `amuxd`
//! daemon. Pure DTOs + a length-prefixed `postcard` codec, no sockets. See `docs/DESIGN.md`
//! §6. The socket wiring (`Framed`) lives at the call sites, which own the async runtime.

mod codec;
mod message;

pub use codec::{check_version, ClientCodec, ProtoError, ServerCodec, WireCodec, MAX_FRAME_BYTES};
pub use message::{AgentInfo, ClientMsg, DaemonMsg, RepoInfo, Size};

/// Protocol version. The client and daemon refuse to talk across a mismatch (the client
/// auto-recovers). v1 multi-agent; v2 dirty-delete; v3 multi-attach; v4 agents-own-terminals;
/// v5 multi-repo.
pub const PROTO_VERSION: u32 = 5;

//! `amux-proto` — the versioned wire protocol between the `amux` client and the `amuxd`
//! daemon. Pure DTOs + framing, no logic. See `docs/DESIGN.md` §6.
//!
//! Populated in Phase 0.2.

/// Protocol version. The client and daemon refuse to talk across a mismatch.
pub const PROTO_VERSION: u32 = 0;

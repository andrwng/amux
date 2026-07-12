//! `amux-core` — pure domain logic (no sockets, no PTYs, no async runtime). This is the
//! heavily-tested heart: the agent state machine, the `AgentAdapter` trait + status
//! sources, worktree operations, and config. See `docs/DESIGN.md` §4.
//!
//! Populated across Phases 0–1.

pub mod agent;
pub mod clock;
pub mod paths;

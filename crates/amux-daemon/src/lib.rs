//! `amux-daemon` — the runtime that owns all live state: the control socket, the PTY pool,
//! the hook mailbox, the session registry, and persistence. All process/socket I/O lives
//! here. See `docs/DESIGN.md` §5.
//!
//! Populated from Phase 0.3.

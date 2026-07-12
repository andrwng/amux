//! `amux-tui` — the ratatui client: a projection of daemon state. Phase 0.5 renders a single
//! full-screen PTY (the spine made real); the sidebar, main window, and floating minis arrive
//! in Phase 1+. See `docs/DESIGN.md` §7.

mod app;
mod client;
mod input;

pub use client::{connect, ClientOptions};

use anyhow::Result;

/// Entry point for the `amux` TUI client (the default subcommand): connect to the daemon
/// (auto-spawning it if needed) and render its shell until you quit.
pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(app::run())
}

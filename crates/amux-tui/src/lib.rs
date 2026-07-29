//! `amux-tui` — the ratatui client: a projection of daemon state. Phase 1 renders the agent
//! **sidebar** + the selected agent's terminal in the **main window**. Floating minis are
//! Phase 3. See `docs/DESIGN.md` §7.

mod app;
mod client;
mod doctor;
mod input;
mod pane;
mod theme;

use anyhow::Result;

/// Entry point for the `amux` TUI client: discover the repo, connect to (or auto-spawn) its
/// daemon, and drive the sidebar until you quit.
pub fn run(profile: Option<amux_core::config::Profile>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(app::run(profile))
}

/// Entry point for `amux doctor`: prune orphaned worktrees in the current repo and print a report.
pub fn doctor() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(doctor::run())
}

//! `amux-tui` — the ratatui client: a projection of daemon state. Phase 1 renders the agent
//! **sidebar** + the selected agent's terminal in the **main window**. Floating minis are
//! Phase 3. See `docs/DESIGN.md` §7.

mod app;
mod client;
mod doctor;
mod fuzzy;
mod input;
mod pane;
mod record;
mod theme;
mod worktree;

use anyhow::Result;

pub use client::{connect_to_running_daemon, ConnectError};
pub use worktree::{CommandError, MAX_TURNS_LIMIT};

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

/// Entry point for `amux worktree list`: the text to print on standard output, or why there is
/// none (each reason has its own exit code — see [`CommandError::exit_code`]).
pub fn worktree_list(json: bool) -> std::result::Result<String, CommandError> {
    command_runtime()?.block_on(worktree::list(json))
}

/// Entry point for `amux worktree turns`.
pub fn worktree_turns(
    id: &str,
    after: u64,
    limit: u32,
    json: bool,
) -> std::result::Result<String, CommandError> {
    command_runtime()?.block_on(worktree::turns(id, after, limit, json))
}

/// A small runtime for a one-request command.
fn command_runtime() -> std::result::Result<tokio::runtime::Runtime, CommandError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CommandError::Other(anyhow::Error::new(e).context("build runtime")))
}

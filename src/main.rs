//! `amux` — the single shipped binary (like `git`), dispatching to the TUI client, the
//! background daemon, the hook bridge, and the editor-integration helpers. Thin: every
//! subcommand's work lives in a crate. See `docs/DESIGN.md` §3.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Multiplex AI coding agents in isolated git worktrees.
#[derive(Parser)]
#[command(name = "amux", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background daemon (normally auto-spawned by the client).
    Daemon {
        /// Run in the foreground without detaching — for debugging and tests.
        #[arg(long)]
        foreground: bool,
        /// Stop a running daemon (kills its sessions) instead of starting one.
        #[arg(long)]
        stop: bool,
        /// The git repository this daemon manages (defaults to the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Prune orphaned worktrees in the current repo — reclaim branches wedged as "already
    /// checked out" after a crash or an out-of-band deletion.
    Doctor,
    /// Bridge a Claude Code hook event to the daemon mailbox (invoked by Claude's hooks).
    Hook,
    /// (internal) Hand pane navigation back to amux from an in-pane program at its edge —
    /// invoked by the amux-navigator vim plugin. DIRECTION is h/j/k/l.
    Nav { direction: String },
    /// (internal) Announce that a vim-like app is (`on`) or is no longer (`off`) the foreground
    /// program in this pane, so amux knows whether to pass `Ctrl+hjkl` through — invoked by the
    /// vim plugin on enter/leave.
    Passthrough { state: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => amux_tui::run()?,
        Some(Command::Daemon {
            foreground,
            stop,
            repo,
        }) => {
            if stop {
                amux_daemon::stop()?;
            } else {
                // Resolve the repo to an absolute path BEFORE daemonizing (which chdirs to /).
                let repo = repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let repo = std::fs::canonicalize(&repo).unwrap_or(repo);
                // Detach BEFORE building the tokio runtime (fork-safety — see DESIGN §11).
                if !foreground {
                    amux_daemon::daemonize()?;
                }
                amux_daemon::run_blocking(repo)?;
            }
        }
        Some(Command::Doctor) => amux_tui::doctor()?,
        Some(Command::Hook) => amux_daemon::run_hook()?,
        Some(Command::Nav { direction }) => amux_daemon::run_nav(&direction)?,
        Some(Command::Passthrough { state }) => {
            amux_daemon::run_passthrough(state.eq_ignore_ascii_case("on"))?
        }
    }
    Ok(())
}

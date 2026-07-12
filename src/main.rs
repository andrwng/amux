//! `amux` — the single shipped binary (like `git`), dispatching to the TUI client, the
//! background daemon, and the hook bridge. See `docs/DESIGN.md` §3.
//!
//! Phase 0.1: subcommands are scaffolded but not yet implemented. The working artifact for
//! this milestone is the throwaway spike — run `cargo run --example spike`.

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
    }
    Ok(())
}

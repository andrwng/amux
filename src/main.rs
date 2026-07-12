//! `amux` — the single shipped binary (like `git`), dispatching to the TUI client, the
//! background daemon, and the hook bridge. See `docs/DESIGN.md` §3.
//!
//! Phase 0.1: subcommands are scaffolded but not yet implemented. The working artifact for
//! this milestone is the throwaway spike — run `cargo run --example spike`.

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
    },
    /// Bridge a Claude Code hook event to the daemon mailbox (invoked by Claude's hooks).
    Hook,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => amux_tui::run()?,
        Some(Command::Daemon { foreground }) => {
            // Detach BEFORE building the tokio runtime (fork-safety — see DESIGN §11).
            if !foreground {
                amux_daemon::daemonize()?;
            }
            amux_daemon::run_blocking()?;
        }
        Some(Command::Hook) => eprintln!("amux hook is not implemented yet (Phase 2)."),
    }
    Ok(())
}

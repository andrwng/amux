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
    Daemon,
    /// Bridge a Claude Code hook event to the daemon mailbox (invoked by Claude's hooks).
    Hook,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            eprintln!("amux TUI client is not implemented yet (Phase 0.5).");
            eprintln!("Phase 0.1 spike:  cargo run --example spike");
        }
        Some(Command::Daemon) => eprintln!("amux daemon is not implemented yet (Phase 0.3)."),
        Some(Command::Hook) => eprintln!("amux hook is not implemented yet (Phase 2)."),
    }
    Ok(())
}

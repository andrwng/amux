//! `amux` — the single shipped binary (like `git`), dispatching to the TUI client, the
//! background daemon, the hook bridge, and the editor-integration helpers. Thin: every
//! subcommand's work lives in a crate. See `docs/DESIGN.md` §3.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// The version string for `--version`: the git commit and build time this binary was built from.
/// The Cargo version is `0.0.0` and useless for telling two builds apart; the git hash is the fact
/// that actually matters when confirming a freshly installed binary is the one running.
fn amux_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        // Render the build epoch as a UTC `YYYY-MM-DD HH:MM:SS`, eyeball-comparable with a file
        // mtime — dependency-free (the binary crate has no chrono), civil-from-days arithmetic.
        let epoch: i64 = env!("AMUX_BUILD_EPOCH").parse().unwrap_or(0);
        format!(
            "{} (git {}, built {} UTC)",
            env!("CARGO_PKG_VERSION"),
            env!("AMUX_GIT_HASH"),
            fmt_utc(epoch)
        )
    })
}

/// Format a Unix timestamp as `YYYY-MM-DD HH:MM:SS`, UTC, without a date crate (days-since-epoch to
/// civil date via Howard Hinnant's algorithm). Only used for the `--version` build stamp.
fn fmt_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Multiplex AI coding agents in isolated git worktrees.
#[derive(Parser)]
#[command(name = "amux", version = amux_version(), about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Border/accent color profile for the TUI: blue (default), green, yellow, or red.
    #[arg(long, value_parser = parse_profile)]
    profile: Option<amux_core::config::Profile>,
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

fn parse_profile(s: &str) -> Result<amux_core::config::Profile, String> {
    s.parse()
        .map_err(|e: amux_core::config::ParseProfileError| e.to_string())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => amux_tui::run(cli.profile)?,
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

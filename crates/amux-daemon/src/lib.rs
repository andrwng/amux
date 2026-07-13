//! `amux-daemon` — the runtime that owns all live state: the control socket, the agent
//! registry (durable agents over generic sessions), and the client loop. All process/socket
//! I/O lives here. See `docs/DESIGN.md` §5.
//!
//! Phase 1: multi-agent, multi-repo. One global daemon manages agents across many registered
//! repositories (clients register their cwd on connect); a session exiting suspends its agent
//! (resumable), and only an explicit delete removes a worktree.

mod daemonize;
mod mailbox;
mod pty;
mod registry;
mod server;

pub use daemonize::daemonize;
pub use mailbox::{bind_mailbox, run_hook, run_nav, run_passthrough, serve_mailbox};
pub use registry::Registry;
pub use server::{bind_or_detect, serve};

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::signal::unix::{signal, SignalKind};

use amux_core::adapter::ClaudeAdapter;
use amux_core::worktree::WorktreeLocation;

/// Max unix-socket path length, comfortably under the `sun_path` limit (§11 gotcha 4).
const MAX_SOCKET_PATH: usize = 100;

fn pid_file(dir: &Path) -> PathBuf {
    dir.join("amuxd.pid")
}

/// Run the daemon for `repo`: bind the control socket, build the registry, and serve until a
/// shutdown signal. Must be called *after* [`daemonize`] (fork before the tokio runtime).
pub fn run_blocking(repo: PathBuf) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let paths = amux_core::paths::RuntimePaths::resolve()?;
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create runtime dir {}", paths.dir.display()))?;
    std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o700)).ok();

    let socket = paths.socket();
    let mailbox = paths.mailbox();
    for path in [&socket, &mailbox] {
        anyhow::ensure!(
            path.as_os_str().len() < MAX_SOCKET_PATH,
            "socket path is too long ({} bytes): {}",
            path.as_os_str().len(),
            path.display()
        );
    }
    let pidfile = pid_file(&paths.dir);

    // The agent CLI. Phase 2 runs real `claude` (hooks push exact status); override with
    // `AMUX_AGENT_CMD` (space-separated) to point at a shell or a fake agent for testing.
    let command = match std::env::var("AMUX_AGENT_CMD") {
        Ok(c) if !c.trim().is_empty() => c.split_whitespace().map(String::from).collect(),
        _ => vec!["claude".to_string()],
    };
    let adapter = Box::new(ClaudeAdapter::with_command(command));
    // The bridge Claude's hooks invoke (absolute path — hooks run with cwd = the worktree).
    let amux_exe = std::env::current_exe().context("locate the amux executable")?;

    let runtime = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    runtime.block_on(async move {
        let listener = server::bind_or_detect(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).ok();
        let hook_listener = mailbox::bind_mailbox(&mailbox)?;
        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o600)).ok();
        std::fs::write(&pidfile, std::process::id().to_string()).ok();
        tracing::info!(
            "amux daemon for {} listening at {} (mailbox {}, pid {})",
            repo.display(),
            socket.display(),
            mailbox.display(),
            std::process::id()
        );

        let registry = Registry::with_hooks(adapter, mailbox.clone(), amux_exe);
        // Pre-register the launching repo so `amux` in a repo dir works out of the box; clients
        // also register their own cwd on connect, so the daemon serves many repos over time.
        if let Err(e) = registry.register_path(&repo, WorktreeLocation::Global) {
            tracing::warn!("could not register launch repo {}: {e:#}", repo.display());
        }
        let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

        let outcome = tokio::select! {
            r = server::serve(listener, registry.clone()) => r,
            r = mailbox::serve_mailbox(hook_listener, registry.clone()) => r,
            _ = sigterm.recv() => { tracing::info!("SIGTERM, shutting down"); Ok(()) }
            _ = sigint.recv() => { tracing::info!("SIGINT, shutting down"); Ok(()) }
        };

        registry.shutdown_all();
        std::fs::remove_file(&socket).ok();
        std::fs::remove_file(&mailbox).ok();
        std::fs::remove_file(&pidfile).ok();
        outcome
    })
}

/// Stop a running daemon by sending it SIGTERM (read from the pidfile).
pub fn stop() -> Result<()> {
    let paths = amux_core::paths::RuntimePaths::resolve()?;
    let pidfile = pid_file(&paths.dir);
    let contents = std::fs::read_to_string(&pidfile).map_err(|_| {
        anyhow!(
            "no running amux daemon (no pidfile at {})",
            pidfile.display()
        )
    })?;
    let pid: i32 = contents.trim().parse().context("parse pidfile")?;
    kill(Pid::from_raw(pid), Signal::SIGTERM).context("signal the daemon")?;
    println!("stopped amux daemon (pid {pid})");
    Ok(())
}

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
pub use server::{acquire_and_bind, serve, Singleton};

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
    let lock = paths.lock();
    let runtime_dir = paths.dir.clone();

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
        // Arbitrates against a daemon already on the socket via the advisory lock: stands down
        // behind a compatible or merely-unreachable incumbent, evicts only a confirmed-incompatible
        // one (the reinstall case) so it cannot linger with its PTYs.
        let Some(server::Singleton {
            lock: _lock,
            listener,
        }) = server::acquire_and_bind(&lock, &socket, &pidfile).await?
        else {
            tracing::info!(
                "another amux daemon owns {}; exiting",
                runtime_dir.display()
            );
            return Ok(());
        };
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

        let state_path = amux_core::paths::state_file()?;
        let registry = Registry::with_hooks(adapter, mailbox.clone(), amux_exe, state_path);
        // Reinstate agents/repos/minis from a previous run (suspended until a client opens them).
        registry.load_state();
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

        // Capture any last durable changes (e.g. unread/activity) before the processes die.
        registry.save();
        // Graceful: agents get SIGTERM and a moment to checkpoint before they are killed.
        registry.shutdown_all().await;
        cleanup_if_owner(&pidfile, &[&socket, &mailbox], std::process::id());
        outcome
    })
}

/// Stop a running daemon: SIGTERM the pid in the pidfile, but only after confirming it is alive
/// and actually an amux process — a stale pidfile (a SIGKILL/SSH-drop leftover) can name a dead
/// or reused pid, and we must not signal an unrelated process.
fn stop_at(pidfile: &Path) -> Result<()> {
    let contents = std::fs::read_to_string(pidfile).map_err(|_| {
        anyhow!(
            "no running amux daemon (no pidfile at {})",
            pidfile.display()
        )
    })?;
    let pid: i32 = contents.trim().parse().context("parse pidfile")?;
    if !server::alive(pid) || !server::looks_like_amux(pid) {
        std::fs::remove_file(pidfile).ok();
        return Err(anyhow!(
            "no running amux daemon (stale pidfile named pid {pid}; removed it)"
        ));
    }
    kill(Pid::from_raw(pid), Signal::SIGTERM).context("signal the daemon")?;
    println!("stopped amux daemon (pid {pid})");
    Ok(())
}

/// Stop a running daemon by SIGTERM (pid from the pidfile under the resolved runtime dir).
pub fn stop() -> Result<()> {
    let paths = amux_core::paths::RuntimePaths::resolve()?;
    stop_at(&pid_file(&paths.dir))
}

/// Remove the daemon's runtime files, but only if `pidfile` still names `my_pid`. A daemon that
/// has been superseded (evicted, or slow to shut down) must never delete its successor's socket
/// and pidfile — that leaves the live daemon with a corrupted, unfindable identity.
fn cleanup_if_owner(pidfile: &Path, others: &[&Path], my_pid: u32) {
    let owner = std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if owner != Some(my_pid) {
        tracing::info!(
            "pidfile {} names {:?}, not me ({my_pid}); leaving runtime files in place",
            pidfile.display(),
            owner
        );
        return;
    }
    for p in others {
        std::fs::remove_file(p).ok();
    }
    std::fs::remove_file(pidfile).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_only_when_the_pidfile_names_me() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("amuxd.pid");
        let socket = tmp.path().join("amuxd.sock");
        std::fs::write(&socket, b"").unwrap();

        // Pidfile names a DIFFERENT process — a successor owns these files; leave them.
        std::fs::write(&pidfile, "999999").unwrap();
        cleanup_if_owner(&pidfile, &[&socket], 12345);
        assert!(socket.exists(), "must not delete a successor's socket");
        assert!(pidfile.exists(), "must not delete a successor's pidfile");

        // Pidfile names me — I own them; remove them.
        std::fs::write(&pidfile, "12345").unwrap();
        cleanup_if_owner(&pidfile, &[&socket], 12345);
        assert!(!socket.exists(), "owner removes its socket");
        assert!(!pidfile.exists(), "owner removes its pidfile");
    }

    #[test]
    fn stop_refuses_a_stale_or_dead_pid_and_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("amuxd.pid");

        // A pid that is definitely dead (spawn a trivial child and reap it).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let dead = child.id();
        std::fs::write(&pidfile, dead.to_string()).unwrap();

        let err = stop_at(&pidfile).unwrap_err();
        assert!(
            err.to_string().contains("no running amux daemon"),
            "stop must refuse a dead pid, got: {err}"
        );
        assert!(!pidfile.exists(), "stop clears the stale pidfile");
    }

    #[test]
    fn stop_errors_when_no_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        let err = stop_at(&tmp.path().join("amuxd.pid")).unwrap_err();
        assert!(
            err.to_string().contains("no running amux daemon"),
            "got: {err}"
        );
    }
}

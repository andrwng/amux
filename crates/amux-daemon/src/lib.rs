//! `amux-daemon` — the runtime that owns all live state: the control socket, persistent PTY
//! session(s), and the client loop. All process/socket I/O lives here. See `docs/DESIGN.md` §5.
//!
//! Phase 0.6: a session survives client disconnects (detach) and is re-attachable (reattach).
//! The single-session registry generalizes to the multi-agent daemon of Phase 1.

mod daemonize;
mod pty;
mod server;

pub use daemonize::daemonize;
pub use server::{bind_or_detect, serve, DaemonConfig, Registry};

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::signal::unix::{signal, SignalKind};

/// Max unix-socket path length, comfortably under the `sun_path` limit (§11 gotcha 4).
const MAX_SOCKET_PATH: usize = 100;

fn pid_file(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("amuxd.pid")
}

/// Run the daemon: bind the control socket, write a pidfile, and serve until a shutdown
/// signal. Must be called *after* [`daemonize`] (fork before the tokio runtime is built).
pub fn run_blocking() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let paths = amux_core::paths::RuntimePaths::resolve()?;
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create runtime dir {}", paths.dir.display()))?;
    std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o700)).ok();

    let socket = paths.socket();
    anyhow::ensure!(
        socket.as_os_str().len() < MAX_SOCKET_PATH,
        "socket path is too long ({} bytes): {}",
        socket.as_os_str().len(),
        socket.display()
    );
    let pidfile = pid_file(&paths.dir);

    let runtime = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    runtime.block_on(async move {
        let listener = server::bind_or_detect(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).ok();
        std::fs::write(&pidfile, std::process::id().to_string()).ok();
        tracing::info!(
            "amux daemon listening at {} (pid {})",
            socket.display(),
            std::process::id()
        );

        let registry = Arc::new(Registry::new(DaemonConfig::default()));
        let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

        let outcome = tokio::select! {
            r = server::serve(listener, Arc::clone(&registry)) => r,
            _ = sigterm.recv() => { tracing::info!("received SIGTERM, shutting down"); Ok(()) }
            _ = sigint.recv() => { tracing::info!("received SIGINT, shutting down"); Ok(()) }
        };

        registry.shutdown_all();
        std::fs::remove_file(&socket).ok();
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

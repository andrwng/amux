//! `amux-daemon` — the runtime that owns all live state: the control socket, the PTY, and
//! the client session loop. All process/socket I/O lives here. See `docs/DESIGN.md` §5.
//!
//! Phase 0.3: a single PTY per client connection, single-client in practice. The structure
//! (per-connection session, snapshot-then-stream, resize-to-slot) generalizes to the
//! multi-agent daemon of Phase 1 without a rewrite.

mod daemonize;
mod pty;
mod server;

pub use daemonize::daemonize;
pub use server::{bind_or_detect, serve, DaemonConfig};

use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result};

/// Maximum unix-socket path length we allow, comfortably under the `sun_path` limit
/// (~108 bytes on Linux, ~104 on macOS). See `docs/DESIGN.md` §11 gotcha 4.
const MAX_SOCKET_PATH: usize = 100;

/// Run the daemon: resolve paths, bind the control socket, and serve. Must be called *after*
/// [`daemonize`] (so the fork happens before the tokio runtime is built).
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

    let runtime = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    runtime.block_on(async move {
        let listener = server::bind_or_detect(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).ok();
        tracing::info!("amux daemon listening at {}", socket.display());
        server::serve(listener, DaemonConfig::default()).await
    })
}

//! Connecting to the repo's daemon: discover the git repo from the cwd, connect to the control
//! socket, and handshake. If no usable daemon answers — absent, stale, or an incompatible
//! protocol version — auto-spawn a fresh `amux daemon --repo <root>`, so a leftover daemon from
//! an older build never wedges the client.
//!
//! **The client never clears the socket itself.** Unlinking it here is what used to orphan the
//! previous daemon: the name became free, a new daemon bound it, and the old process kept running
//! unreachably with its PTYs and agent processes. Arbitration belongs to the daemon that binds —
//! see `amux-daemon`'s `acquire_and_bind`, which probes and evicts confirmed-incompatible daemons.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientCodec, ClientMsg, DaemonMsg, PROTO_VERSION};

type Connection = Framed<UnixStream, ClientCodec>;

/// Connect to the (global) daemon, auto-spawning/recovering it, and handshake. Returns the
/// connection and the repository discovered from the cwd, so the caller can register it — the
/// daemon serves many repos, and this client's repo may not be the one the daemon launched in.
pub async fn connect() -> Result<(Connection, PathBuf)> {
    let cwd = std::env::current_dir().context("get current directory")?;
    let repo = amux_core::worktree::discover_repo(&cwd)?;
    let socket = amux_core::paths::RuntimePaths::resolve()?.socket();

    // Reuse a running, compatible daemon if there is one.
    if let Ok(connection) = try_handshake(&socket).await {
        return Ok((connection, repo));
    }

    // Otherwise the daemon is absent, stale, or speaks a different protocol version. Spawn one and
    // wait for it to come up; it arbitrates over the socket (evicting an incompatible predecessor)
    // rather than us unlinking the socket out from under a process that would then be unreachable.
    spawn_daemon(&repo).await?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(connection) = try_handshake(&socket).await {
            return Ok((connection, repo));
        }
    }
    bail!("amux daemon did not come up at {}", socket.display());
}

/// Connect and complete the version handshake, or fail (so the caller can recover).
async fn try_handshake(socket: &Path) -> Result<Connection> {
    let stream = UnixStream::connect(socket).await.context("connect")?;
    let mut framed = Framed::new(stream, ClientCodec::new());
    framed
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await?;
    match framed.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => {
            check_version(proto_version)?;
            Ok(framed)
        }
        Some(Ok(DaemonMsg::Error { message })) => bail!("daemon rejected connection: {message}"),
        Some(Ok(other)) => bail!("unexpected first frame from daemon: {other:?}"),
        Some(Err(e)) => Err(e).context("read daemon hello"),
        None => bail!("daemon closed the connection during handshake"),
    }
}

async fn spawn_daemon(repo: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locate the amux executable")?;
    let mut child = tokio::process::Command::new(exe)
        .arg("daemon")
        .arg("--repo")
        .arg(repo)
        .spawn()
        .context("spawn amux daemon")?;
    let _ = child.wait().await;
    Ok(())
}

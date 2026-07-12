//! Connecting to the repo's daemon: discover the git repo from the cwd, connect to the control
//! socket, and handshake. If no usable daemon answers — absent, stale, or an incompatible
//! protocol version — clear the socket and auto-spawn a fresh `amux daemon --repo <root>`, so a
//! leftover daemon from an older build never wedges the client.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientCodec, ClientMsg, DaemonMsg, PROTO_VERSION};

type Connection = Framed<UnixStream, ClientCodec>;

/// Connect to the daemon for the current repository (auto-spawning/recovering it) and handshake.
pub async fn connect() -> Result<Connection> {
    let cwd = std::env::current_dir().context("get current directory")?;
    let repo = amux_core::worktree::discover_repo(&cwd)?;
    let socket = amux_core::paths::RuntimePaths::resolve()?.socket();

    // Reuse a running, compatible daemon if there is one.
    if let Ok(connection) = try_handshake(&socket).await {
        return Ok(connection);
    }

    // Otherwise the daemon is absent, stale, or speaks a different protocol version. Clear the
    // socket so a fresh daemon can bind it, then spawn one and wait for it to come up.
    std::fs::remove_file(&socket).ok();
    spawn_daemon(&repo).await?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(connection) = try_handshake(&socket).await {
            return Ok(connection);
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

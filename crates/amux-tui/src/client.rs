//! Connecting to the repo's daemon: discover the git repo from the cwd, connect to the control
//! socket, auto-spawn `amux daemon --repo <root>` if it isn't there, then handshake.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientCodec, ClientMsg, DaemonMsg, PROTO_VERSION};

/// Connect to the daemon for the current repository, auto-spawning it if needed, and handshake.
pub async fn connect() -> Result<Framed<UnixStream, ClientCodec>> {
    let cwd = std::env::current_dir().context("get current directory")?;
    let repo = amux_core::worktree::discover_repo(&cwd)?;
    let socket = amux_core::paths::RuntimePaths::resolve()?.socket();

    let stream = connect_or_spawn(&socket, &repo).await?;
    let mut framed = Framed::new(stream, ClientCodec::new());
    framed
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await?;
    match framed.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => check_version(proto_version)?,
        Some(Ok(DaemonMsg::Error { message })) => bail!("daemon rejected connection: {message}"),
        Some(Ok(other)) => bail!("unexpected first frame from daemon: {other:?}"),
        Some(Err(e)) => return Err(e).context("read daemon hello"),
        None => bail!("daemon closed the connection during handshake"),
    }
    Ok(framed)
}

async fn connect_or_spawn(socket: &Path, repo: &Path) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket).await {
        return Ok(stream);
    }
    spawn_daemon(repo).await?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Ok(stream);
        }
    }
    bail!("amux daemon did not come up at {}", socket.display());
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

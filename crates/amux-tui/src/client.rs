//! Connecting to the daemon: try the control socket, auto-spawn `amux daemon` if it isn't
//! there, then perform the version handshake. See `docs/DESIGN.md` §5, §7.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientCodec, ClientMsg, DaemonMsg, Size, PROTO_VERSION};

/// How the client should reach the daemon.
pub struct ClientOptions {
    pub socket: PathBuf,
    /// Auto-spawn a daemon if the socket isn't answering (off in tests).
    pub spawn_daemon: bool,
    pub size: Size,
}

impl ClientOptions {
    /// Resolve against the real runtime socket path, with auto-spawn enabled.
    pub fn resolve(size: Size) -> Result<Self> {
        let paths = amux_core::paths::RuntimePaths::resolve()?;
        Ok(Self {
            socket: paths.socket(),
            spawn_daemon: true,
            size,
        })
    }
}

/// Connect (auto-spawning the daemon if needed), handshake, and return the framed connection.
pub async fn connect(opts: &ClientOptions) -> Result<Framed<UnixStream, ClientCodec>> {
    let stream = connect_or_spawn(opts).await?;
    let mut framed = Framed::new(stream, ClientCodec::new());
    framed
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            size: opts.size,
        })
        .await?;
    match framed.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => check_version(proto_version)?,
        Some(Ok(DaemonMsg::Error(e))) => bail!("daemon rejected connection: {e}"),
        Some(Ok(other)) => bail!("unexpected first frame from daemon: {other:?}"),
        Some(Err(e)) => return Err(e).context("read daemon hello"),
        None => bail!("daemon closed the connection during handshake"),
    }
    Ok(framed)
}

async fn connect_or_spawn(opts: &ClientOptions) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(&opts.socket).await {
        return Ok(stream);
    }
    if !opts.spawn_daemon {
        bail!("no amux daemon at {}", opts.socket.display());
    }
    spawn_daemon().await?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(stream) = UnixStream::connect(&opts.socket).await {
            return Ok(stream);
        }
    }
    bail!("amux daemon did not come up at {}", opts.socket.display());
}

async fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("locate the amux executable")?;
    // The daemon self-detaches (double-fork), so our direct child is the first-fork parent,
    // which exits at once — waiting on it returns immediately and reaps it.
    let mut child = tokio::process::Command::new(exe)
        .arg("daemon")
        .spawn()
        .context("spawn amux daemon")?;
    let _ = child.wait().await;
    Ok(())
}

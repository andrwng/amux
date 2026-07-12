//! The control server: accept a client, do the version handshake, spawn a PTY, then pump
//! output to the client and input/resize/shutdown from it. See `docs/DESIGN.md` §5, §6.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientMsg, DaemonMsg, ServerCodec, PROTO_VERSION};

use crate::pty::PtySession;

/// What the daemon spawns for each client. Phase 0: a single command (default: `$SHELL`).
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub command: Vec<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self {
            command: vec![shell],
        }
    }
}

/// Bind the control socket, detecting whether a live daemon already owns it. A stale socket
/// file (no daemon accepting) is removed and rebound; a live one is an error.
pub fn bind_or_detect(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == ErrorKind::AddrInUse => match StdUnixStream::connect(path) {
            Ok(_) => bail!(
                "an amux daemon is already running (socket {})",
                path.display()
            ),
            Err(_) => {
                std::fs::remove_file(path).ok();
                UnixListener::bind(path).context("rebind after removing stale socket")
            }
        },
        Err(e) => Err(e).context("bind control socket"),
    }
}

/// Accept clients forever, handling each on its own task.
pub async fn serve(listener: UnixListener, config: DaemonConfig) -> Result<()> {
    let config = Arc::new(config);
    loop {
        let (stream, _addr) = listener.accept().await.context("accept connection")?;
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, config).await {
                tracing::warn!("client handler error: {e:#}");
            }
        });
    }
}

async fn handle_client(stream: UnixStream, config: Arc<DaemonConfig>) -> Result<()> {
    let mut framed = Framed::new(stream, ServerCodec::new());

    // Handshake: the first frame must be a version-matching Hello.
    let size = match framed.next().await {
        Some(Ok(ClientMsg::Hello {
            proto_version,
            size,
        })) => {
            if let Err(e) = check_version(proto_version) {
                let _ = framed.send(DaemonMsg::Error(e.to_string())).await;
                return Ok(());
            }
            size
        }
        Some(Ok(_)) => {
            let _ = framed
                .send(DaemonMsg::Error("expected Hello as the first frame".into()))
                .await;
            return Ok(());
        }
        Some(Err(e)) => return Err(e).context("read hello frame"),
        None => return Ok(()),
    };
    framed
        .send(DaemonMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await?;

    // Spawn the PTY and send the initial screen snapshot before the live stream.
    let (mut session, mut output_rx) = PtySession::spawn(&config.command, size)?;
    framed
        .send(DaemonMsg::OutputSnapshot(session.snapshot()))
        .await?;

    let (mut sink, mut stream) = framed.split();
    loop {
        tokio::select! {
            out = output_rx.recv() => match out {
                Some(bytes) => {
                    if sink.send(DaemonMsg::Output(bytes)).await.is_err() {
                        break; // client disconnected
                    }
                }
                None => break, // PTY closed (child exited)
            },
            msg = stream.next() => match msg {
                Some(Ok(ClientMsg::Input(bytes))) => {
                    let _ = session.write_input(&bytes);
                }
                Some(Ok(ClientMsg::Resize(size))) => {
                    let _ = session.resize(size);
                }
                Some(Ok(ClientMsg::Shutdown)) => break,
                Some(Ok(ClientMsg::Hello { .. })) => {} // ignore a duplicate hello
                Some(Err(_)) | None => break,
            },
        }
    }

    session.kill();
    let code = session.wait();
    let _ = sink.send(DaemonMsg::Exited { code }).await;
    Ok(())
}

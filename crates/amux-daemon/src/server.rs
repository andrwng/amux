//! The control server + session registry. Clients *attach* to a persistent session (spawning
//! it on first attach); disconnecting **detaches** (the session lives on); `Shutdown` or the
//! shell exiting tears it down. See `docs/DESIGN.md` §5, §6.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientMsg, DaemonMsg, ServerCodec, Size, PROTO_VERSION};

use crate::pty::Session;

/// What the daemon spawns for a session. Phase 0: a single command (default: `$SHELL`).
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

/// Holds the persistent session(s). Phase 0 keeps a single session; the shape generalizes to a
/// keyed map of agents in Phase 1.
pub struct Registry {
    config: DaemonConfig,
    session: Mutex<Option<Arc<Session>>>,
}

impl Registry {
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            session: Mutex::new(None),
        }
    }

    /// Attach to the live session (resizing it to the client), or spawn one if none exists.
    pub fn attach(&self, size: Size) -> Result<Arc<Session>> {
        let mut slot = self.session.lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            if !existing.is_exited() {
                let session = Arc::clone(existing);
                drop(slot);
                let _ = session.resize(size);
                return Ok(session);
            }
        }
        let session = Session::spawn(&self.config.command, size)?;
        *slot = Some(Arc::clone(&session));
        Ok(session)
    }

    /// Remove `target` from the registry if it is still the current session.
    pub fn remove_if(&self, target: &Arc<Session>) {
        let mut slot = self.session.lock().unwrap();
        if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, target)) {
            *slot = None;
        }
    }

    /// Kill and drop the current session (daemon shutdown).
    pub fn shutdown_all(&self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            session.kill();
        }
    }
}

/// Bind the control socket, detecting whether a live daemon already owns it.
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

/// Accept clients forever, each attaching to the shared registry on its own task.
pub async fn serve(listener: UnixListener, registry: Arc<Registry>) -> Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await.context("accept connection")?;
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, registry).await {
                tracing::warn!("client handler error: {e:#}");
            }
        });
    }
}

async fn handle_client(stream: UnixStream, registry: Arc<Registry>) -> Result<()> {
    let mut framed = Framed::new(stream, ServerCodec::new());

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

    // Attach to (or create) the persistent session and send its current screen.
    let session = registry.attach(size)?;
    let mut output_rx = session.subscribe();
    let mut exit_rx = session.exit_rx();
    let (mut sink, mut stream) = framed.split();
    sink.send(DaemonMsg::OutputSnapshot(session.snapshot()))
        .await?;

    // If it already exited between attach and here, report and go.
    if *exit_rx.borrow_and_update() {
        let _ = sink
            .send(DaemonMsg::Exited {
                code: session.exit_code(),
            })
            .await;
        registry.remove_if(&session);
        return Ok(());
    }

    loop {
        tokio::select! {
            output = output_rx.recv() => match output {
                Ok(bytes) => {
                    if sink.send(DaemonMsg::Output(bytes)).await.is_err() {
                        break; // client disconnected → detach
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Fell behind → resync from the authoritative screen.
                    if sink.send(DaemonMsg::OutputSnapshot(session.snapshot())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            msg = stream.next() => match msg {
                Some(Ok(ClientMsg::Input(bytes))) => { let _ = session.write_input(&bytes); }
                Some(Ok(ClientMsg::Resize(size))) => { let _ = session.resize(size); }
                Some(Ok(ClientMsg::Shutdown)) => {
                    session.kill();
                    registry.remove_if(&session);
                    break;
                }
                Some(Ok(ClientMsg::Hello { .. })) => {}
                // Client gone: DETACH — leave the session running for a future reattach.
                Some(Err(_)) | None => break,
            },
            _ = exit_rx.changed() => {
                let _ = sink.send(DaemonMsg::Exited { code: session.exit_code() }).await;
                registry.remove_if(&session);
                break;
            }
        }
    }
    Ok(())
}

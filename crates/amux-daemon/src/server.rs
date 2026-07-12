//! The control server. One client connection multiplexes agent management (create/delete/
//! resume/list) and a **single** live terminal stream (the selected agent — minis are Phase 3).
//! Lifecycle events (added/removed/state) are pushed to every client. See `docs/DESIGN.md` §5–6.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use amux_core::agent::AgentId;
use amux_proto::{check_version, ClientMsg, DaemonMsg, ServerCodec, PROTO_VERSION};

use crate::registry::{DeleteOutcome, Registry};

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

/// Accept clients forever, each on its own task.
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

/// The single agent whose PTY is currently streaming to this client, plus its forwarder task.
type Attachment = Option<(AgentId, JoinHandle<()>)>;

async fn handle_client(stream: UnixStream, registry: Arc<Registry>) -> Result<()> {
    let mut framed = Framed::new(stream, ServerCodec::new());

    match framed.next().await {
        Some(Ok(ClientMsg::Hello { proto_version })) => {
            if let Err(e) = check_version(proto_version) {
                let _ = framed
                    .send(DaemonMsg::Error {
                        message: e.to_string(),
                    })
                    .await;
                return Ok(());
            }
        }
        _ => return Ok(()),
    }
    framed
        .send(DaemonMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await?;
    framed.send(DaemonMsg::Agents(registry.infos())).await?;

    let (mut sink, mut stream) = framed.split();
    // Merged outbound: command replies + the attached agent's output funnel through here.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<DaemonMsg>();
    let mut events = registry.subscribe_events();
    let mut attached: Attachment = None;

    loop {
        tokio::select! {
            command = stream.next() => match command {
                Some(Ok(msg)) => handle_command(msg, &registry, &out_tx, &mut attached),
                _ => break, // client disconnected
            },
            outbound = out_rx.recv() => match outbound {
                Some(msg) => if sink.send(msg).await.is_err() { break; },
                None => break,
            },
            event = events.recv() => match event {
                Ok(msg) => if sink.send(msg).await.is_err() { break; },
                // Missed lifecycle events → resend the full roster to resync.
                Err(RecvError::Lagged(_)) => {
                    if sink.send(DaemonMsg::Agents(registry.infos())).await.is_err() { break; }
                }
                Err(RecvError::Closed) => break,
            },
        }
    }

    if let Some((_, forwarder)) = attached {
        forwarder.abort();
    }
    Ok(())
}

fn handle_command(
    msg: ClientMsg,
    registry: &Arc<Registry>,
    out_tx: &mpsc::UnboundedSender<DaemonMsg>,
    attached: &mut Attachment,
) {
    let report_err = |result: Result<()>| {
        if let Err(e) = result {
            let _ = out_tx.send(DaemonMsg::Error {
                message: format!("{e:#}"),
            });
        }
    };

    match msg {
        ClientMsg::Hello { .. } => {}
        ClientMsg::ListAgents => {
            let _ = out_tx.send(DaemonMsg::Agents(registry.infos()));
        }
        ClientMsg::CreateAgent { branch } => report_err(registry.create(&branch).map(|_| ())),
        ClientMsg::DeleteAgent { id, force } => match registry.delete(id, force) {
            Ok(DeleteOutcome::Deleted) => detach_if(attached, id),
            Ok(DeleteOutcome::NeedsConfirm(message)) => {
                let _ = out_tx.send(DaemonMsg::DeleteNeedsConfirm { id, message });
            }
            Err(e) => {
                let _ = out_tx.send(DaemonMsg::Error {
                    message: format!("{e:#}"),
                });
            }
        },
        ClientMsg::ResumeAgent { id } => report_err(registry.resume(id)),
        ClientMsg::Attach { id, size } => {
            // Re-target the single live stream.
            if let Some((_, forwarder)) = attached.take() {
                forwarder.abort();
            }
            match registry.session(id) {
                Some(session) => {
                    let _ = session.resize(size);
                    let _ = out_tx.send(DaemonMsg::OutputSnapshot {
                        id,
                        bytes: session.snapshot(),
                    });
                    let tx = out_tx.clone();
                    let mut rx = session.subscribe();
                    let forwarder = tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(bytes) => {
                                    if tx.send(DaemonMsg::Output { id, bytes }).is_err() {
                                        break;
                                    }
                                }
                                Err(RecvError::Lagged(_)) => {} // snapshot already covers the gap
                                Err(RecvError::Closed) => break,
                            }
                        }
                    });
                    *attached = Some((id, forwarder));
                }
                None => {
                    let _ = out_tx.send(DaemonMsg::Error {
                        message: "agent has no live session — resume it first".to_string(),
                    });
                }
            }
        }
        ClientMsg::Input { id, bytes } => {
            if let Some(session) = registry.session(id) {
                let _ = session.write_input(&bytes);
            }
        }
        ClientMsg::Resize { id, size } => {
            if let Some(session) = registry.session(id) {
                let _ = session.resize(size);
            }
        }
    }
}

fn detach_if(attached: &mut Attachment, id: AgentId) {
    if attached.as_ref().is_some_and(|(a, _)| *a == id) {
        if let Some((_, forwarder)) = attached.take() {
            forwarder.abort();
        }
    }
}

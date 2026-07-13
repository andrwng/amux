//! The control server. One client connection multiplexes agent management (create/delete/
//! resume) and **terminal** streams — one per visible pane, tagged by `TerminalId`. Splitting a
//! pane spawns a `$SHELL` terminal in the same worktree. Lifecycle events (agent added/removed/
//! state, terminal exited) are pushed to every client. See `docs/DESIGN.md` §5–6, `SPLITS.md`.

use std::collections::HashMap;
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

use amux_core::agent::TerminalId;
use amux_proto::{check_version, ClientMsg, DaemonMsg, ServerCodec, Size, PROTO_VERSION};

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

/// The terminals this client is streaming, each with its output-forwarder task.
type Attachments = HashMap<TerminalId, JoinHandle<()>>;

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
    framed.send(DaemonMsg::Repos(registry.repos())).await?;
    framed.send(DaemonMsg::Agents(registry.infos())).await?;
    framed.send(DaemonMsg::Layouts(registry.layouts())).await?;
    framed.send(DaemonMsg::Minis(registry.minis())).await?;

    let (mut sink, mut stream) = framed.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<DaemonMsg>();
    let mut events = registry.subscribe_events();
    let mut attached: Attachments = HashMap::new();

    loop {
        tokio::select! {
            command = stream.next() => match command {
                Some(Ok(msg)) => handle_command(msg, &registry, &out_tx, &mut attached),
                _ => break,
            },
            outbound = out_rx.recv() => match outbound {
                Some(msg) => if sink.send(msg).await.is_err() { break; },
                None => break,
            },
            event = events.recv() => match event {
                Ok(msg) => if sink.send(msg).await.is_err() { break; },
                Err(RecvError::Lagged(_)) => {
                    if sink.send(DaemonMsg::Agents(registry.infos())).await.is_err() { break; }
                }
                Err(RecvError::Closed) => break,
            },
        }
    }

    for (_, forwarder) in attached {
        forwarder.abort();
    }
    // The viewer is gone, so nothing is being watched — a later notable event should mark unread.
    registry.focus(None);
    Ok(())
}

fn handle_command(
    msg: ClientMsg,
    registry: &Arc<Registry>,
    out_tx: &mpsc::UnboundedSender<DaemonMsg>,
    attached: &mut Attachments,
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
        ClientMsg::AddRepo { path } => {
            report_err(register_repo(registry, &path));
        }
        ClientMsg::CreateAgent { repo, branch } => {
            report_err(registry.create(repo, &branch).map(|_| ()))
        }
        ClientMsg::CreateAgentAt { path, branch } => {
            report_err(registry.create_at(&path, &branch).map(|_| ()))
        }
        ClientMsg::DeleteAgent { id, force } => match registry.delete(id, force) {
            Ok(DeleteOutcome::Deleted) => {}
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
        ClientMsg::SpawnShell { terminal, like } => {
            report_err(registry.spawn_shell(terminal, like))
        }
        ClientMsg::CloseTerminal { terminal } => {
            detach(attached, terminal);
            registry.close_terminal(terminal);
        }
        ClientMsg::Focus { agent } => registry.focus(agent),
        ClientMsg::SetLayout { agent, layout } => registry.set_layout(agent, layout),
        ClientMsg::SetMinis(minis) => registry.set_minis(minis),
        ClientMsg::DoctorRepo { repo } => match registry.doctor(repo) {
            Ok(report) => {
                let _ = out_tx.send(DaemonMsg::DoctorReport {
                    repo,
                    pruned: report.pruned,
                    skipped: report.skipped,
                });
            }
            Err(e) => {
                let _ = out_tx.send(DaemonMsg::Error {
                    message: format!("{e:#}"),
                });
            }
        },
        ClientMsg::Attach { terminal, size } => attach(attached, registry, out_tx, terminal, size),
        ClientMsg::Detach { terminal } => detach(attached, terminal),
        ClientMsg::Input { terminal, bytes } => {
            if let Some(session) = registry.session(terminal) {
                let _ = session.write_input(&bytes);
            }
        }
        ClientMsg::Resize { terminal, size } => {
            if let Some(session) = registry.session(terminal) {
                let _ = session.resize(size);
            }
        }
    }
}

/// Register a repo by path (idempotent). `register_path` broadcasts `RepoAdded` if it is new.
fn register_repo(registry: &Arc<Registry>, path: &Path) -> Result<()> {
    registry
        .register_path(path, amux_core::worktree::WorktreeLocation::Global)
        .map(|_| ())
}

fn attach(
    attached: &mut Attachments,
    registry: &Arc<Registry>,
    out_tx: &mpsc::UnboundedSender<DaemonMsg>,
    terminal: TerminalId,
    size: Size,
) {
    // A suspended primary (e.g. after a daemon restart) has no session yet — attaching one revives
    // it, resuming the agent's CLI in place. Errors here fall through to the "no live session" path.
    if registry.session(terminal).is_none() {
        if let Err(e) = registry.resume_for_terminal(terminal) {
            let _ = out_tx.send(DaemonMsg::Error {
                message: format!("could not resume terminal: {e:#}"),
            });
            return;
        }
    }
    let Some(session) = registry.session(terminal) else {
        let _ = out_tx.send(DaemonMsg::Error {
            message: "terminal has no live session".to_string(),
        });
        return;
    };
    let _ = session.resize(size);
    if attached.contains_key(&terminal) {
        return;
    }
    let _ = out_tx.send(DaemonMsg::OutputSnapshot {
        terminal,
        bytes: session.snapshot(),
    });
    let tx = out_tx.clone();
    let mut rx = session.subscribe();
    let forwarder = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if tx.send(DaemonMsg::Output { terminal, bytes }).is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
    attached.insert(terminal, forwarder);
}

fn detach(attached: &mut Attachments, terminal: TerminalId) {
    if let Some(forwarder) = attached.remove(&terminal) {
        forwarder.abort();
    }
}

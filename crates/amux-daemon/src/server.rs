//! The control server. One client connection multiplexes agent management (create/delete/
//! resume) and **terminal** streams — one per visible pane, tagged by `TerminalId`. Splitting a
//! pane spawns a `$SHELL` terminal in the same worktree. Lifecycle events (agent added/removed/
//! state, terminal exited) are pushed to every client. See `docs/DESIGN.md` §5–6, `SPLITS.md`.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use amux_core::agent::TerminalId;
use amux_proto::{
    check_version, ClientCodec, ClientMsg, DaemonMsg, ServerCodec, Size, PROTO_VERSION,
};

use crate::pty::ScrollPos;
use crate::registry::{DeleteOutcome, Registry};

/// How long to wait for a daemon to answer the probe handshake. It only has to serialize a
/// `Hello` it has already built, so anything slower is wedged, not busy.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an evicted daemon gets to shut down cleanly (save state, terminate its agents) after
/// SIGTERM, before we escalate to SIGKILL.
const EVICT_GRACE: Duration = Duration::from_secs(5);
/// How often to re-check whether the evicted daemon has exited.
const EVICT_POLL: Duration = Duration::from_millis(50);

/// What answered (or failed to answer) on an already-bound control socket.
enum Probe {
    /// A daemon speaking our exact protocol version — the socket is legitimately taken.
    Compatible,
    /// Something is listening, but we cannot talk to it: a different `PROTO_VERSION` (the reinstall
    /// case), an undecodable frame, or no answer at all.
    Incompatible,
    /// Nothing is behind the socket file — a crash left it on disk.
    Dead,
}

/// Bind the control socket, arbitrating against whatever already owns it.
///
/// A *compatible* daemon means this one is redundant and refuses to start. An *incompatible* one —
/// the reinstall case, where the new binary bumped `PROTO_VERSION` — is terminated via `pidfile`
/// before we take the socket. That eviction is the whole point: unlinking the socket and walking
/// away (what the client used to do) leaves the old daemon running unreachably, holding its PTYs
/// and agent processes forever.
pub async fn bind_or_detect(socket: &Path, pidfile: &Path) -> Result<UnixListener> {
    match UnixListener::bind(socket) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == ErrorKind::AddrInUse => match probe(socket).await {
            Probe::Compatible => bail!(
                "an amux daemon is already running (socket {})",
                socket.display()
            ),
            Probe::Incompatible => {
                evict(pidfile).await;
                rebind(socket).context("rebind after evicting the previous daemon")
            }
            Probe::Dead => rebind(socket).context("rebind after removing stale socket"),
        },
        Err(e) => Err(e).context("bind control socket"),
    }
}

/// Unlink the socket path and bind a fresh one. Safe even while a doomed process still holds the
/// old inode: the name is what clients resolve, and the old inode dies with its listener.
fn rebind(socket: &Path) -> Result<UnixListener> {
    std::fs::remove_file(socket).ok();
    UnixListener::bind(socket).map_err(Into::into)
}

/// Ask whoever owns the socket whether we can talk to them, using the real handshake.
///
/// Deliberately duplicates `amux-tui`'s `try_handshake` rather than sharing it: the dependency
/// direction is `amux-tui → amux-proto ← amux-daemon` (DESIGN §2.7), and the daemon must not grow
/// an edge to the client to save twenty lines.
async fn probe(socket: &Path) -> Probe {
    let Ok(stream) = UnixStream::connect(socket).await else {
        return Probe::Dead;
    };
    let mut framed = Framed::new(stream, ClientCodec::new());
    let exchange = async {
        framed
            .send(ClientMsg::Hello {
                proto_version: PROTO_VERSION,
            })
            .await
            .ok()?;
        framed.next().await
    };
    match tokio::time::timeout(PROBE_TIMEOUT, exchange).await {
        Ok(Some(Ok(DaemonMsg::Hello { proto_version }))) if proto_version == PROTO_VERSION => {
            Probe::Compatible
        }
        // A mismatched `Hello`, a rejection `Error`, a frame our codec cannot decode (postcard is
        // positional, so an older enum layout decodes as garbage), a closed connection, or silence.
        _ => Probe::Incompatible,
    }
}

/// Terminate the daemon named by `pidfile`: SIGTERM, wait out [`EVICT_GRACE`] so it can save state
/// and shut its agents down, then SIGKILL. Best-effort — a missing or stale pidfile is logged and
/// we bind anyway, which is no worse than the behavior this replaces.
///
/// Signalling by pid is only as trustworthy as the pidfile. We know *something* is listening (the
/// caller saw `AddrInUse` and connected), and the pidfile is written by the process that binds and
/// removed when it exits cleanly, so a mismatch needs two failures at once. We still refuse to
/// signal ourselves.
async fn evict(pidfile: &Path) {
    let pid = match std::fs::read_to_string(pidfile) {
        Ok(contents) => match contents.trim().parse::<i32>() {
            Ok(pid) => pid,
            Err(e) => {
                tracing::warn!("unreadable pidfile {}: {e}", pidfile.display());
                return;
            }
        },
        Err(e) => {
            tracing::warn!(
                "no pidfile at {} — cannot evict the previous daemon: {e}",
                pidfile.display()
            );
            return;
        }
    };
    if pid == std::process::id() as i32 {
        tracing::warn!("pidfile names this process ({pid}); not evicting");
        return;
    }
    if !looks_like_amux(pid) {
        tracing::warn!("pid {pid} in {} is not an amux process; not evicting (stale pidfile with a reused pid)", pidfile.display());
        return;
    }

    tracing::info!("evicting incompatible daemon (pid {pid})");
    if kill(Pid::from_raw(pid), Signal::SIGTERM).is_err() {
        return; // already gone
    }

    let deadline = tokio::time::Instant::now() + EVICT_GRACE;
    while tokio::time::Instant::now() < deadline {
        if !alive(pid) {
            tracing::info!("previous daemon (pid {pid}) exited");
            return;
        }
        tokio::time::sleep(EVICT_POLL).await;
    }

    tracing::warn!("daemon {pid} ignored SIGTERM for {EVICT_GRACE:?}; sending SIGKILL");
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    // Give the kernel a moment to tear it down so the socket is free before we rebind.
    for _ in 0..20 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(EVICT_POLL).await;
    }
    tracing::warn!("daemon {pid} still present after SIGKILL");
}

/// Whether `pid` still exists. Signal 0 checks for the process without delivering anything.
fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Whether `pid` is running an `amux` binary — a sanity check before signalling.
///
/// The pidfile is only as good as the last shutdown, and amux is routinely used over SSH, where
/// disconnects are not graceful: a daemon killed outright leaves its pidfile behind, and the pid
/// can later be reused by something else entirely. Eviction only ever triggers when a *listener*
/// we can't talk to owns the socket, so a wrong pid needs two failures at once — but SIGTERMing an
/// unrelated process is bad enough to be worth one `ps`.
///
/// Fails **open**: if `ps` is missing or unreadable we proceed with the eviction, since leaving an
/// unreachable daemon running is the bug we came here to fix. `comm` is the executable name on
/// Linux and its path on macOS, so match on the final path component.
fn looks_like_amux(pid: i32) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    let comm = String::from_utf8_lossy(&out.stdout);
    let comm = comm.trim();
    if comm.is_empty() {
        return true;
    }
    comm.rsplit('/').next().unwrap_or(comm).contains("amux")
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
    framed.send(DaemonMsg::Active(registry.active())).await?;

    let (mut sink, mut stream) = framed.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<DaemonMsg>();
    let mut events = registry.subscribe_events();
    let mut attached: Attachments = HashMap::new();
    let mut scroll: ScrollPositions = HashMap::new();

    loop {
        tokio::select! {
            command = stream.next() => match command {
                Some(Ok(msg)) => handle_command(msg, &registry, &out_tx, &mut attached, &mut scroll),
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
    scroll: &mut ScrollPositions,
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
        ClientMsg::CreateAgent {
            repo,
            branch,
            prompt,
        } => report_err(
            registry
                .create(repo, &branch, prompt.as_deref())
                .map(|_| ()),
        ),
        ClientMsg::CreateAgentAt { path, branch } => {
            report_err(registry.create_at(&path, &branch).map(|_| ()))
        }
        ClientMsg::CreateHeadAgent { repo } => report_err(registry.create_head(repo).map(|_| ())),
        ClientMsg::CreateHeadAgentAt { path } => {
            report_err(registry.create_head_at(&path).map(|_| ()))
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
            scroll.remove(&terminal);
            registry.close_terminal(terminal);
        }
        ClientMsg::Focus { agent } => registry.focus(agent),
        ClientMsg::SetLayout { agent, layout } => registry.set_layout(agent, layout),
        ClientMsg::SetMinis(minis) => registry.set_minis(minis),
        ClientMsg::SetActive(agent) => registry.set_active(agent),
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
        ClientMsg::Detach { terminal } => {
            // A pane we are no longer showing can't be scrolled, so its position goes with it.
            scroll.remove(&terminal);
            detach(attached, terminal)
        }
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
        ClientMsg::Scroll { terminal, lines } => {
            scroll_view(scroll, registry, out_tx, terminal, lines)
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

/// Where each scrolled-back terminal's view sits for *this* client, and how deep history was when
/// we served it. Per connection, so two clients scroll the same terminal independently, and dropped
/// as soon as a client returns to the live view — nothing accumulates for panes nobody is scrolling.
type ScrollPositions = HashMap<TerminalId, ScrollPos>;

/// Move this client's scroll position by `lines` and send it the window that lands on.
///
/// The step is relative and the session re-bases it past output that has arrived since we last
/// served this client (see [`Session::scroll_step`]); all this layer does is remember where each
/// client is. That correction is exact until history reaches its cap and begins dropping lines, past
/// which the oldest window ages out no matter what we do.
fn scroll_view(
    scroll: &mut ScrollPositions,
    registry: &Arc<Registry>,
    out_tx: &mpsc::UnboundedSender<DaemonMsg>,
    terminal: TerminalId,
    lines: i32,
) {
    let Some(session) = registry.session(terminal) else {
        return;
    };
    let frame = session.scroll_step(scroll.get(&terminal).copied(), lines);
    // Offset 0 *is* the live view, so there is no position left to remember.
    if frame.offset == 0 {
        scroll.remove(&terminal);
    } else {
        scroll.insert(
            terminal,
            ScrollPos {
                offset: frame.offset,
                depth: frame.available,
            },
        );
    }
    let _ = out_tx.send(DaemonMsg::ScrollView {
        terminal,
        offset: frame.offset,
        available: frame.available,
        bytes: frame.bytes,
    });
}

fn detach(attached: &mut Attachments, terminal: TerminalId) {
    if let Some(forwarder) = attached.remove(&terminal) {
        forwarder.abort();
    }
}

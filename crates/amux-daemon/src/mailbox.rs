//! The hook mailbox: a lightweight unix socket that Claude Code hooks push status to (via
//! `amux hook`). Each connection carries exactly one postcard-encoded [`PaneMessage`]; we decode
//! it and hand it to the registry, which drives the state machine or records a conversation turn.
//! Fire-and-forget — no reply, so a hook never blocks Claude. See `docs/DESIGN.md` §5.1, §5.4.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use amux_core::hook::{HookReport, PaneMessage, TurnReport};

use crate::registry::Registry;

/// One report is small; cap the read so a malformed/hostile client can't grow the buffer without
/// bound (the control codec has its own 4 MiB cap; the mailbox stays tiny).
const MAX_REPORT_BYTES: usize = 64 * 1024;

/// Bind the mailbox socket, replacing any stale one. The daemon's control socket already did the
/// live-vs-stale arbitration for the process as a whole, so the mailbox is created fresh here.
pub fn bind_mailbox(path: &Path) -> Result<UnixListener> {
    std::fs::remove_file(path).ok();
    UnixListener::bind(path).context("bind mailbox socket")
}

/// How many conversation turns may be waiting to be written at once. A turn arriving past this is
/// dropped (with a warning that carries no text) rather than queued without bound.
pub(crate) const MAX_PENDING_TURNS: usize = 32;

/// Accept hook connections forever, each handled on its own task.
pub async fn serve_mailbox(listener: UnixListener, registry: Arc<Registry>) -> Result<()> {
    let turn_slots = Arc::new(Semaphore::new(MAX_PENDING_TURNS));
    loop {
        let (stream, _addr) = listener.accept().await.context("accept hook connection")?;
        let registry = Arc::clone(&registry);
        let turn_slots = Arc::clone(&turn_slots);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, registry, turn_slots).await {
                tracing::debug!("hook report ignored: {e:#}");
            }
        });
    }
}

async fn handle(
    stream: UnixStream,
    registry: Arc<Registry>,
    turn_slots: Arc<Semaphore>,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    stream
        .take(MAX_REPORT_BYTES as u64)
        .read_to_end(&mut buf)
        .await
        .context("read pane message")?;
    let msg: PaneMessage = postcard::from_bytes(&buf).context("decode pane message")?;
    match msg {
        PaneMessage::Hook(report) => registry.on_hook(report),
        PaneMessage::Passthrough { terminal, on } => registry.set_passthrough(terminal, on),
        PaneMessage::Nav { terminal, dir } => registry.request_nav(terminal, dir),
        PaneMessage::Turn(report) => {
            // Writing the log is file I/O: off the async workers, and bounded.
            let Ok(slot) = turn_slots.try_acquire_owned() else {
                tracing::warn!(
                    "dropped a turn for {}: {MAX_PENDING_TURNS} are already waiting to be written",
                    report.agent
                );
                return Ok(());
            };
            tokio::task::spawn_blocking(move || {
                registry.on_turn(report);
                drop(slot);
            })
            .await
            .context("record a turn")?;
        }
    }
    Ok(())
}

// --- `amux hook` / `amux nav` / `amux passthrough`: client side of the mailbox ---
//
// Each is a short-lived, dependency-light (no tokio) process that delivers one postcard frame to
// `$AMUX_HOOK_SOCK` and exits. **Fire-and-forget:** failures are swallowed (stderr only, exit 0)
// so an in-pane hook or navigator never disrupts Claude or vim.

/// `amux hook`: read Claude's hook JSON from stdin, tag it with `$AMUX_AGENT_ID`, and deliver it.
pub fn run_hook() -> Result<()> {
    // Bound the entire hook — the stdin read included — on wall-clock time. Claude runs the hook
    // synchronously and blocks the turn (and the pane's render) until this process exits, so any
    // blocking step (a stdin that never EOFs, a connect/write to a wedged or backlogged daemon)
    // freezes the agent. Observed: `amux hook` alive for 3+ minutes (Claude's own status read
    // "running Stop hook · 3m 20s"), the pane cleared with no redraw — the long-standing blank.
    // Status is best-effort: abandon a hook that overruns rather than let it hang the agent.
    swallow("hook", run_bounded(HOOK_DELIVER_TIMEOUT, deliver_hook))
}

/// `amux nav <dir>`: the in-pane navigator hit its edge — hand navigation back to amux.
pub fn run_nav(dir: &str) -> Result<()> {
    let dir = dir.to_string();
    swallow(
        "nav",
        run_bounded(HOOK_DELIVER_TIMEOUT, move || deliver_nav(&dir)),
    )
}

/// `amux passthrough <on|off>`: announce that a vim-like app is (or is no longer) foreground here.
pub fn run_passthrough(on: bool) -> Result<()> {
    swallow(
        "passthrough",
        run_bounded(HOOK_DELIVER_TIMEOUT, move || deliver_passthrough(on)),
    )
}

fn swallow(what: &str, result: Result<()>) -> Result<()> {
    if let Err(e) = result {
        eprintln!("amux {what}: {e:#}");
    }
    Ok(())
}

/// Hard ceiling on delivering one hook report. Claude runs its hooks synchronously and blocks the
/// turn — and the render — until the hook process exits, so a hook that blocks freezes the agent's
/// pane (observed: `amux hook` stuck for minutes, the agent's screen cleared with no redraw, the
/// long-standing blank). Status is best-effort, so a delivery that can't complete quickly is
/// abandoned rather than allowed to hang: better a dropped status line than a frozen agent.
const HOOK_DELIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn deliver(msg: &PaneMessage) -> Result<()> {
    // Callers wrap this in `run_bounded`, so the connect/write below cannot hang the process past
    // the deadline; the `set_write_timeout` in `deliver_blocking` is a second, in-band bound.
    let sock = std::env::var("AMUX_HOOK_SOCK").context("AMUX_HOOK_SOCK not set")?;
    deliver_to(Path::new(&sock), msg)
}

fn deliver_to(sock: &Path, msg: &PaneMessage) -> Result<()> {
    let bytes = postcard::to_stdvec(msg).context("encode pane message")?;
    deliver_blocking(sock, &bytes, HOOK_DELIVER_TIMEOUT)
}

/// How long `amux hook` gives a conversation turn, connect and write together, after the status
/// report has gone. Much shorter than [`HOOK_DELIVER_TIMEOUT`]: the hook runs inside the agent's
/// turn, and a turn is a record, not a signal anything waits on — one the daemon is not reading
/// right now is dropped rather than allowed to hold the agent.
const TURN_DELIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// The send buffer asked for on a turn's connection: room for the largest turn message with
/// plenty to spare, so a turn lands in the kernel at once even while the daemon is not reading.
/// macOS gives a unix stream socket 8 KiB by default, and a clipped turn is about 24 KiB.
const TURN_SEND_BUFFER: usize = MAX_REPORT_BYTES;

/// Deliver a conversation turn within [`TURN_DELIVER_TIMEOUT`], on a connection with a send
/// buffer of [`TURN_SEND_BUFFER`]. A turn that cannot be delivered in that time is dropped.
fn deliver_turn(sock: &Path, msg: &PaneMessage) -> Result<()> {
    let bytes = postcard::to_stdvec(msg).context("encode pane message")?;
    let sock = sock.to_path_buf();
    // `run_bounded` bounds the connect too, which has no timeout of its own: a daemon whose
    // listen backlog is full can block it.
    run_bounded(TURN_DELIVER_TIMEOUT, move || {
        let stream = connect_mailbox(&sock)?;
        // Best effort: if the kernel refuses a larger buffer, the deadline still bounds the write.
        let _ = nix::sys::socket::setsockopt(
            &stream,
            nix::sys::socket::sockopt::SndBuf,
            &TURN_SEND_BUFFER,
        );
        write_message(stream, &bytes, TURN_DELIVER_TIMEOUT)
    })
    .context("turn dropped")
}

/// Run a blocking `f` on a throwaway thread and give up on it after `dur`. Returns `f`'s result, or
/// a timeout error if it overran. The thread is detached on timeout — fine here because the caller
/// (`amux hook`) is a short-lived process that exits right after, so nothing outlives it.
fn run_bounded<F>(dur: std::time::Duration, f: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(dur) {
        Ok(result) => result,
        Err(_) => anyhow::bail!("timed out after {dur:?}"),
    }
}

fn deliver_blocking(sock: &Path, bytes: &[u8], timeout: std::time::Duration) -> Result<()> {
    write_message(connect_mailbox(sock)?, bytes, timeout)
}

fn connect_mailbox(sock: &Path) -> Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(sock).context("connect to mailbox")
}

/// Write one message and half-close. The write timeout guards against a full socket buffer (the
/// daemon not reading); it applies per `write` call, so callers that need a hard bound on the whole
/// write also wrap this in [`run_bounded`].
fn write_message(
    mut stream: std::os::unix::net::UnixStream,
    bytes: &[u8],
    timeout: std::time::Duration,
) -> Result<()> {
    use std::io::Write;
    stream
        .set_write_timeout(Some(timeout))
        .context("set write timeout")?;
    stream.write_all(bytes).context("send pane message")?;
    // Half-close so the daemon's read-to-EOF completes promptly.
    stream.shutdown(std::net::Shutdown::Write).ok();
    Ok(())
}

fn deliver_hook() -> Result<()> {
    use std::io::Read;
    let agent_str = std::env::var("AMUX_AGENT_ID").context("AMUX_AGENT_ID not set")?;
    let agent =
        amux_core::agent::AgentId::parse(&agent_str).context("AMUX_AGENT_ID is not a valid id")?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read hook payload from stdin")?;
    let sock = std::env::var("AMUX_HOOK_SOCK").context("AMUX_HOOK_SOCK not set")?;
    deliver_hook_input(Path::new(&sock), agent, &input)
}

/// What `amux hook` does once it has its environment and stdin: send the status report for this
/// hook payload to the mailbox at `sock`, then — on a connection of its own — the conversation
/// turn it carries, if any.
///
/// The status report is first and is the same message as before turns existed, byte for byte, so
/// a daemon from before this change applies it as it always did and only drops the turn (a kind
/// it cannot decode). The turn's text was clipped by [`turn_from_hook`] before it is encoded here,
/// so the message always fits the mailbox's read limit. Nothing here logs the text.
///
/// The turn has a deadline of its own, [`TURN_DELIVER_TIMEOUT`], far shorter than the status
/// report's: past it the turn is dropped (an error saying so is returned), so a daemon that is not
/// reading costs the agent's turn at most that long beyond the status report.
///
/// [`turn_from_hook`]: amux_core::hook::turn_from_hook
pub fn deliver_hook_input(
    sock: &Path,
    agent: amux_core::agent::AgentId,
    input: &str,
) -> Result<()> {
    let event: amux_core::hook::HookEvent =
        serde_json::from_str(input).context("parse hook JSON")?;
    // Unknown or mistyped turn fields cost the turn, never the status report.
    let fields: amux_core::hook::HookTurnFields = serde_json::from_str(input).unwrap_or_default();
    let turn = amux_core::hook::turn_from_hook(&event, &fields);
    deliver_to(sock, &PaneMessage::Hook(HookReport { agent, event }))?;
    if let Some(turn) = turn {
        deliver_turn(sock, &PaneMessage::Turn(TurnReport { agent, turn }))?;
    }
    Ok(())
}

fn deliver_nav(dir: &str) -> Result<()> {
    let terminal = terminal_from_env()?;
    let dir = amux_core::nav::Dir::parse(dir)
        .with_context(|| format!("not a direction: {dir:?} (want h/j/k/l)"))?;
    deliver(&PaneMessage::Nav { terminal, dir })
}

fn deliver_passthrough(on: bool) -> Result<()> {
    let terminal = terminal_from_env()?;
    deliver(&PaneMessage::Passthrough { terminal, on })
}

fn terminal_from_env() -> Result<amux_core::agent::TerminalId> {
    let s = std::env::var("AMUX_TERMINAL_ID").context("AMUX_TERMINAL_ID not set")?;
    amux_core::agent::TerminalId::parse(&s).context("AMUX_TERMINAL_ID is not a valid id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use amux_core::agent::{AgentId, TerminalId};
    use amux_core::conversation::{Speaker, Turn, MAX_TURN_TEXT_BYTES};
    use amux_core::hook::HookEvent;
    use amux_core::nav::Dir;
    use serde::{Deserialize, Serialize};

    /// `PaneMessage` as it was before turns existed — what a daemon from before the upgrade
    /// decodes with.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum OldPaneMessage {
        Hook(HookReport),
        Passthrough { terminal: TerminalId, on: bool },
        Nav { terminal: TerminalId, dir: Dir },
    }

    fn report() -> HookReport {
        HookReport {
            agent: AgentId::new(),
            event: HookEvent {
                hook_event_name: "Stop".into(),
                session_id: Some("sess-1".into()),
                notification_type: None,
                message: None,
                tool_name: None,
            },
        }
    }

    /// The status report is the old message byte for byte, so an old daemon applies it.
    #[test]
    fn the_status_report_is_unchanged_on_the_wire() {
        let r = report();
        let new = postcard::to_stdvec(&PaneMessage::Hook(r.clone())).unwrap();
        let old = postcard::to_stdvec(&OldPaneMessage::Hook(r.clone())).unwrap();
        assert_eq!(new, old);
        let t = TerminalId::new();
        assert_eq!(
            postcard::to_stdvec(&PaneMessage::Nav {
                terminal: t,
                dir: Dir::Up
            })
            .unwrap(),
            postcard::to_stdvec(&OldPaneMessage::Nav {
                terminal: t,
                dir: Dir::Up
            })
            .unwrap()
        );
    }

    /// An old daemon cannot decode a turn and so drops that connection — and only that one.
    #[test]
    fn an_old_daemon_fails_to_decode_a_turn() {
        let msg = PaneMessage::Turn(TurnReport {
            agent: AgentId::new(),
            turn: Turn::new(Speaker::User, "synthetic", None),
        });
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert!(postcard::from_bytes::<OldPaneMessage>(&bytes).is_err());
    }

    #[test]
    fn a_turn_round_trips() {
        let msg = PaneMessage::Turn(TurnReport {
            agent: AgentId::new(),
            turn: Turn::new(Speaker::Agent, "synthetic reply", Some("sess-9".into())),
        });
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(postcard::from_bytes::<PaneMessage>(&bytes).unwrap(), msg);
    }

    /// The largest turn message there can be — a full clip of four-byte characters and a long
    /// session id — still fits well inside what the mailbox reads.
    #[test]
    fn the_largest_turn_fits_the_mailbox() {
        let text = "🦀".repeat(MAX_TURN_TEXT_BYTES);
        let msg = PaneMessage::Turn(TurnReport {
            agent: AgentId::new(),
            turn: Turn::new(Speaker::User, &text, Some("s".repeat(256))),
        });
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert!(bytes.len() < MAX_REPORT_BYTES / 2, "{} bytes", bytes.len());
    }

    /// A mailbox that accepts connections and never reads them (a daemon that is wedged or busy):
    /// the accepted streams are handed back, unread, for the test to read afterwards.
    fn unread_mailbox(sock: &Path) -> std::sync::mpsc::Receiver<std::os::unix::net::UnixStream> {
        let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if tx.send(stream).is_err() {
                    return;
                }
            }
        });
        rx
    }

    fn read_message(mut stream: std::os::unix::net::UnixStream) -> PaneMessage {
        use std::io::Read;
        // The sender has closed (or abandoned) its end by now, so this ends at EOF. (No read
        // timeout: macOS refuses `setsockopt` on a socket whose peer has gone.)
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        postcard::from_bytes(&buf).unwrap()
    }

    fn prompt_payload(bytes: usize) -> String {
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1",
                           "prompt": "p".repeat(bytes)})
        .to_string()
    }

    /// `amux hook` runs inside the agent's turn, so a turn the daemon is not reading must not hold
    /// it for the status report's 3 s bound: on macOS a unix stream socket buffers only 8 KiB, and
    /// a clipped turn is three times that. The turn has its own short deadline, and the status
    /// report sent before it was received.
    #[test]
    fn a_large_turn_to_a_mailbox_that_is_not_read_returns_within_the_turn_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("hooks.sock");
        let accepted = unread_mailbox(&sock);
        let agent = AgentId::new();
        // Far past the clip, so the turn is as large as a turn can be.
        let input = prompt_payload(4 * MAX_TURN_TEXT_BYTES);
        let started = Instant::now();
        let _ = deliver_hook_input(&sock, agent, &input);
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(1000),
            "the hook waited {waited:?} on a turn nobody reads"
        );
        let status = accepted.recv_timeout(Duration::from_secs(1)).unwrap();
        match read_message(status) {
            PaneMessage::Hook(r) => {
                assert_eq!(r.agent, agent);
                assert_eq!(r.event.hook_event_name, "UserPromptSubmit");
            }
            other => panic!("expected the status report first, got {other:?}"),
        }
    }

    /// A turn that cannot be written in time — larger than any send buffer, to a mailbox nobody
    /// reads — is dropped at the turn's deadline, not the status report's.
    #[test]
    fn a_turn_that_cannot_be_written_in_time_is_dropped_at_the_turn_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("hooks.sock");
        let _accepted = unread_mailbox(&sock);
        // Built by hand, past the clip, so no buffer holds it.
        let msg = PaneMessage::Turn(TurnReport {
            agent: AgentId::new(),
            turn: Turn {
                by: Speaker::User,
                text: "x".repeat(8 * 1024 * 1024),
                clipped: false,
                session: None,
            },
        });
        let started = Instant::now();
        let result = deliver_turn(&sock, &msg);
        let waited = started.elapsed();
        assert!(result.is_err(), "the turn is dropped");
        assert!(
            waited < TURN_DELIVER_TIMEOUT + Duration::from_millis(500),
            "waited {waited:?}"
        );
    }

    /// The turn's socket gets a send buffer large enough for the largest turn, so against a daemon
    /// that is not reading right now the turn still lands whole instead of waiting.
    #[test]
    fn the_largest_turn_fits_the_send_buffer_of_a_mailbox_that_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("hooks.sock");
        let accepted = unread_mailbox(&sock);
        let agent = AgentId::new();
        let input = prompt_payload(4 * MAX_TURN_TEXT_BYTES);
        deliver_hook_input(&sock, agent, &input).expect("the turn is delivered, not dropped");
        let _status = accepted.recv_timeout(Duration::from_secs(1)).unwrap();
        let turn = accepted.recv_timeout(Duration::from_secs(1)).unwrap();
        match read_message(turn) {
            PaneMessage::Turn(t) => {
                assert_eq!(t.agent, agent);
                assert!(t.turn.clipped);
                assert!(t.turn.text.len() <= MAX_TURN_TEXT_BYTES);
            }
            other => panic!("expected the turn, got {other:?}"),
        }
    }

    /// A fast delivery returns its own result promptly.
    #[test]
    fn run_bounded_returns_a_fast_result() {
        let started = Instant::now();
        let r = run_bounded(Duration::from_secs(5), || Ok(()));
        assert!(r.is_ok());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "should not wait the full budget"
        );
    }

    /// A blocking delivery is abandoned at the deadline rather than hanging the caller — the whole
    /// point: `amux hook` runs synchronously inside Claude's turn, so an unbounded hang freezes the
    /// agent's pane (the blank bug). Here the closure sleeps far past the budget; the bound must
    /// return an error close to the deadline, not after the sleep.
    #[test]
    fn run_bounded_gives_up_on_a_hanging_delivery() {
        let started = Instant::now();
        let r = run_bounded(Duration::from_millis(200), || {
            std::thread::sleep(Duration::from_secs(30));
            Ok(())
        });
        assert!(
            r.is_err(),
            "a delivery past the deadline must fail, not block"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(200),
            "returned before the deadline: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "waited for the sleep instead of the deadline: {waited:?}"
        );
    }
}

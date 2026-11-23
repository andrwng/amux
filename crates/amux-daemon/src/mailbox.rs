//! The hook mailbox: a lightweight unix socket that Claude Code hooks push status to (via
//! `amux hook`). Each connection carries exactly one postcard-encoded [`PaneMessage`]; we decode
//! it and hand it to the registry, which drives the state machine or records a conversation turn.
//! Fire-and-forget — no reply, so a hook never blocks Claude. See `docs/DESIGN.md` §5.1, §5.4.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

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

/// Accept hook connections forever, each handled on its own task.
pub async fn serve_mailbox(listener: UnixListener, registry: Arc<Registry>) -> Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await.context("accept hook connection")?;
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &registry).await {
                tracing::debug!("hook report ignored: {e:#}");
            }
        });
    }
}

async fn handle(stream: UnixStream, registry: &Registry) -> Result<()> {
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
        PaneMessage::Turn(report) => registry.on_turn(report),
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
    deliver_blocking(sock, &bytes)
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

fn deliver_blocking(sock: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut stream = std::os::unix::net::UnixStream::connect(sock).context("connect to mailbox")?;
    // A write timeout in case the socket buffer is full (daemon not reading): bounded like connect.
    stream
        .set_write_timeout(Some(HOOK_DELIVER_TIMEOUT))
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
        deliver_to(sock, &PaneMessage::Turn(TurnReport { agent, turn }))?;
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

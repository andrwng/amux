//! The hook mailbox: a lightweight unix socket that Claude Code hooks push status to (via
//! `amux hook`). Each connection carries exactly one postcard-encoded [`HookReport`]; we decode
//! it and hand it to the registry, which drives the state machine. Fire-and-forget — no reply,
//! so a hook never blocks Claude. See `docs/DESIGN.md` §5.1.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

use amux_core::hook::{HookReport, PaneMessage};

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
    swallow("hook", deliver_hook())
}

/// `amux nav <dir>`: the in-pane navigator hit its edge — hand navigation back to amux.
pub fn run_nav(dir: &str) -> Result<()> {
    swallow("nav", deliver_nav(dir))
}

/// `amux passthrough <on|off>`: announce that a vim-like app is (or is no longer) foreground here.
pub fn run_passthrough(on: bool) -> Result<()> {
    swallow("passthrough", deliver_passthrough(on))
}

fn swallow(what: &str, result: Result<()>) -> Result<()> {
    if let Err(e) = result {
        eprintln!("amux {what}: {e:#}");
    }
    Ok(())
}

fn deliver(msg: &PaneMessage) -> Result<()> {
    use std::io::Write;
    let sock = std::env::var("AMUX_HOOK_SOCK").context("AMUX_HOOK_SOCK not set")?;
    let bytes = postcard::to_stdvec(msg).context("encode pane message")?;
    let mut stream =
        std::os::unix::net::UnixStream::connect(&sock).context("connect to mailbox")?;
    stream.write_all(&bytes).context("send pane message")?;
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
    let event: amux_core::hook::HookEvent =
        serde_json::from_str(&input).context("parse hook JSON")?;
    deliver(&PaneMessage::Hook(HookReport { agent, event }))
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

//! The hook mailbox: a lightweight unix socket that Claude Code hooks push status to (via
//! `amux hook`). Each connection carries exactly one postcard-encoded [`HookReport`]; we decode
//! it and hand it to the registry, which drives the state machine. Fire-and-forget — no reply,
//! so a hook never blocks Claude. See `docs/DESIGN.md` §5.1.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

use amux_core::hook::HookReport;

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
        .context("read hook report")?;
    let report: HookReport = postcard::from_bytes(&buf).context("decode hook report")?;
    registry.on_hook(report);
    Ok(())
}

/// The `amux hook` bridge (client side of the mailbox): read Claude's hook JSON from stdin, tag
/// it with `$AMUX_AGENT_ID`, and deliver one postcard frame to `$AMUX_HOOK_SOCK`. Synchronous and
/// dependency-light (no tokio runtime) so it starts and exits fast. **Fire-and-forget:** any
/// failure is swallowed so a hook never disrupts Claude — the caller always exits 0.
pub fn run_hook() -> Result<()> {
    if let Err(e) = deliver_hook() {
        // stderr only (ignored by Claude); never fail the hook.
        eprintln!("amux hook: {e:#}");
    }
    Ok(())
}

fn deliver_hook() -> Result<()> {
    use std::io::{Read, Write};

    let agent_str = std::env::var("AMUX_AGENT_ID").context("AMUX_AGENT_ID not set")?;
    let sock = std::env::var("AMUX_HOOK_SOCK").context("AMUX_HOOK_SOCK not set")?;
    let agent =
        amux_core::agent::AgentId::parse(&agent_str).context("AMUX_AGENT_ID is not a valid id")?;

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read hook payload from stdin")?;
    let event: amux_core::hook::HookEvent =
        serde_json::from_str(&input).context("parse hook JSON")?;

    let bytes = postcard::to_stdvec(&HookReport { agent, event }).context("encode hook report")?;
    let mut stream =
        std::os::unix::net::UnixStream::connect(&sock).context("connect to hook mailbox")?;
    stream.write_all(&bytes).context("send hook report")?;
    // Half-close so the daemon's read-to-EOF completes promptly.
    stream.shutdown(std::net::Shutdown::Write).ok();
    Ok(())
}

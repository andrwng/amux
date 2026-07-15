# Coalesced draw loop: one render per event batch, not per event

**Date:** 2026-07-15
**Status:** Approved design, pending implementation plan

## Problem

Typing quickly — especially in vim — visibly lags. The cause is structural, in
the TUI client's event loop: it performs a full-frame `draw()` after **every
single** event, with no coalescing anywhere in the pipeline.

A single keystroke in vim produces a large repaint (amux advertises a
`screen`/`tmux` `TERM` with no `bce`, so vim paints every cell explicitly —
`crates/amux-daemon/src/pty.rs`), and that repaint reaches the client as a
*burst* of `DaemonMsg::Output` frames: the daemon's PTY reader broadcasts one
message per `read()` (`pty.rs` reader thread) and the per-terminal forwarder
relays each as its own frame (`crates/amux-daemon/src/server.rs`). The client
loop (`crates/amux-tui/src/app.rs`, `event_loop`) resolves one `tokio::select!`
branch per iteration and then draws. So one keystroke → many frames → many full
redraws, all serialized on the single-threaded loop. Under a fast-typing burst
the draw backlog grows and input-echo latency grows with it.

## Current state

`event_loop` (`crates/amux-tui/src/app.rs`) is:

```rust
loop {
    tokio::select! {
        msg = stream.next() => match msg { Some(Ok(dm)) => app.on_daemon(dm, &mut sink).await?, _ => break },
        ev  = events.next() => match ev { /* Key / Resize / Mouse / Paste */ },
    }
    app.sync_focus(&mut sink).await?;   // sends only when the viewed agent changed
    draw(terminal, &app)?;              // full frame, after EVERY event
}
```

- Two async sources: `stream` (daemon `DaemonMsg`s, a split `Framed<UnixStream,
  ClientCodec>`) and `events` (crossterm `EventStream` — key/resize/mouse/paste).
- `select!` handles exactly one ready branch per iteration, then `draw()`.
- `draw()` rebuilds the whole frame (sidebar + every visible pane's
  `PseudoTerminal` + minis + status) and lets ratatui diff it to the terminal.
- Input is already forwarded to the daemon *inside* the handlers (`on_key` →
  `sink.send(ClientMsg::Input …)`), so keystrokes reach the PTY promptly today;
  only **rendering** is un-coalesced.
- `sync_focus` is cheap: it sends `ClientMsg::Focus` only when the viewed agent
  actually changed, so it is not part of the hot path.

## Decisions

Settled during brainstorming:

1. **Approach:** coalesce on the **client**, in `event_loop`, by restructuring
   it into *block → drain → draw*: block for the first event, then apply every
   event **already queued** on both sources, then `draw()` **once**.
2. **No timer.** Coalescing is purely loop-structural — we never wait to
   accumulate a burst. This adds **zero** latency when idle (a lone keystroke
   drains to nothing extra and draws immediately, exactly as today). Under load
   it is still effective: the backlog that piles up *during* each ~ms-scale
   `draw()` is scooped into the next single draw (self-correcting).
3. **Client-only.** No daemon-side batching, no PTY backpressure, no change to
   per-draw pane-render cost. Those are separate, later levers.
4. **Preserve behavior exactly** apart from render *count*. Same event handling,
   same ordering semantics, same input-forwarding immediacy, same quit and
   disconnect behavior.

## Design

### 1. Restructure `event_loop` into block → drain → draw

The coalescing lives in one primitive — `drain_ready` — which polls a stream
without ever waiting, collecting everything already buffered:

```rust
/// Pull every item already ready on `stream` into `out`, without awaiting new I/O.
/// Returns true iff the stream has ended (yielded None); false if it is merely
/// out of ready items for now (still open).
async fn drain_ready<S, T>(stream: &mut S, out: &mut Vec<T>) -> bool
where S: Stream<Item = T> + Unpin {
    loop {
        match poll_fn(|cx| Poll::Ready(stream.poll_next_unpin(cx))).await {
            Poll::Ready(Some(item)) => out.push(item),
            Poll::Ready(None) => return true,   // ended
            Poll::Pending => return false,      // nothing ready right now
        }
    }
}
```

The loop blocks for the first event, drains the rest of both sources, then
renders once:

```rust
render(app)?;                                   // initial frame
loop {
    // Phase 1 — block until one source is ready.
    let mut quit = matches!(
        tokio::select! {
            msg = stream.next() => app.handle_daemon(msg.and_then(|r| r.ok()), sink).await?,
            ev  = events.next() => app.handle_event(ev.and_then(|r| r.ok()), sink).await?,
        },
        Flow::Quit
    );

    // Phase 2 — drain everything else already queued on both sources.
    if !quit {
        let mut dmsgs = Vec::new();
        quit |= drain_ready(stream, &mut dmsgs).await;   // did the stream end?
        for m in dmsgs { quit |= matches!(app.handle_daemon(m.ok(), sink).await?, Flow::Quit); }
        let mut evs = Vec::new();
        quit |= drain_ready(events, &mut evs).await;
        for e in evs { quit |= matches!(app.handle_event(e.ok(), sink).await?, Flow::Quit); }
    }

    if quit { break; }                          // teardown — no trailing render
    app.sync_focus(sink).await?;
    render(app)?;                               // exactly one render per drained batch
}
```

Why this is correct and safe:

- **Non-blocking poll.** `drain_ready` wraps a single `poll_next` in a `poll_fn`
  that always returns `Ready`, so `.await` never parks — it inspects readiness
  and returns immediately. Only Phase 1's `select!` ever actually waits.
- **End-of-stream is data, not a race.** `drain_ready` returns `true` when a
  stream closes, so the loop finishes applying the batch it already collected
  and then breaks — a clean teardown, rather than mapping "closed" onto a
  mid-drain quit.
- **Cancellation-safety** of `stream.next()` / `events.next()` across `select!`
  re-polls is already relied upon by today's loop (`Framed` and `EventStream`
  keep partial state in the stream, not the `Next` future), so no new
  assumption is introduced.
- **Ordering.** Within a batch the first event is applied, then the remaining
  daemon messages, then the remaining input events. Input either forwards bytes
  to the daemon (no local render effect) or mutates local UI state; either way
  the batch's final frame matches applying the events one-at-a-time, so nothing
  is observably reordered.
- **Forward progress under a flood.** A pane emitting continuously (e.g. `yes`)
  cannot wedge the drain: the daemon stream is "ready" only for bytes already
  delivered into the client's bounded socket buffer, so between kernel
  deliveries `drain_ready` hits `Pending`, returns, and a render happens — the
  loop renders once per socket-delivered batch. If the client genuinely cannot
  keep up, the daemon's bounded broadcast lags and the client resyncs via
  snapshot (the existing `OutputSnapshot` path); we degrade to drop-and-resync
  rather than starving the renderer. No explicit drain cap is needed.

### 2. Extract the two handlers

Fold the current inline `match` arms into two async methods returning the
existing `Flow` enum, reused by both phases (and so exercised identically):

- `App::handle_daemon(&mut self, msg: Option<DaemonMsg>, sink) -> Result<Flow>`
  — `Some(dm)` → `on_daemon`, returns `Flow::Continue`; `None` → `Flow::Quit`.
  The caller collapses the stream item (`Some(Ok(dm)) → Some(dm)`, `None`/`Err`
  → `None`) with `.and_then(|r| r.ok())` before calling, so a decode error or a
  closed socket both read as "daemon gone" (today's `break`).
- `App::handle_event(&mut self, ev: Option<Event>, sink) -> Result<Flow>`
  — dispatches `Key` (press) / `Resize` / `Mouse` / `Paste` exactly as today; a
  `Key` yielding `Flow::Quit` propagates; `None` (stream closed/error) →
  `Flow::Quit`. The caller collapses `Some(Ok(ev)) → Some(ev)` likewise.

`Flow::Quit` thus means "stop the loop" for both the user quit (`Ctrl-Q`, sidebar
`q`) and a closed source — matching the current loop, which `break`s on both.
A `Quit` surfaced mid-drain exits the `while` (its `Flow::Continue` guard fails)
and then breaks the outer loop **before** drawing, so a quit never renders.

### 3. Testability seam

To make the coalescing property testable without a real terminal, daemon, or
tty:

- Parameterize the loop over its two input streams (generic
  `S: Stream<Item = Result<DaemonMsg, _>>` and `E: Stream<Item = Result<Event,
  _>>`), moving `framed.split()` + `EventStream::new()` up into `run()`. Tests
  drive it with `futures::stream::iter(...)`.
- Replace the direct `draw(terminal, &app)` call with an injected render step
  (`impl FnMut(&App)` in tests; a closure calling `terminal.draw` in `run()`),
  so a test can **count render passes**.
- The `Sink` stays concrete: tests supply a real one via
  `tokio::net::UnixStream::pair()` and ignore the read half.

This is a mechanical hoist — no behavior change — and it is the seam the
regression test needs.

## Consequences (out of scope, called out)

- **Daemon-side batching** (fewer/larger `Output` frames, cutting socket traffic
  and client re-parses) — deferred. This is the tmux read-side lever.
- **PTY backpressure** (slowing a runaway producer instead of drop-and-resync
  via the bounded broadcast) — deferred; a deeper architectural change.
- **Per-draw pane-render cost** (each `draw` still rebuilds every visible
  `PseudoTerminal`) — untouched. If, after this change, profiling shows per-draw
  cost dominates rather than draw *count*, that becomes the next target.
- **No wire change** (`PROTO_VERSION` unchanged) and **no new dependency**
  (`futures` is already a workspace dep; `drain_ready` uses `futures::future::poll_fn`
  and `poll_next_unpin`). The `app.rs` view
  model stays a projection mutated only from `DaemonMsg`s (DESIGN §7), and no
  new tasks are spawned, so structured concurrency (DESIGN §5.2) is unaffected.

## Testing

Both tests must **fail against the current one-draw-per-event loop and pass
after**, per CLAUDE.md ("every bug earns a regression test").

1. **Coalescing (the regression test).** Preload the daemon stream with a burst
   of N `DaemonMsg::Output` frames followed by end-of-stream, and an empty/
   pending input stream; run the loop with a counting render step. Assert the
   burst produces a **single** render pass for the batch (not N). This is the
   direct proof of the fix.
2. **Idle path unchanged.** A single event with nothing else queued produces
   exactly one render — confirming no added latency / no dropped final frame in
   the common case.
3. **Quit mid-batch.** A batch containing a quit event stops the loop without a
   trailing render.

## Insertion points (reference)

- `crates/amux-tui/src/app.rs` — restructure `event_loop` (block → drain →
  draw); add `App::handle_daemon` / `App::handle_event` wrapping today's arms;
  parameterize over the two streams and the render step; move `split()` +
  `EventStream::new()` into `run()`. Add the three tests here (the file already
  has a `#[cfg(test)]` section using `vt100::Parser` directly).

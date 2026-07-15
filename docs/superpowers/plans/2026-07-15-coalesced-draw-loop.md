# Coalesced TUI Draw Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the TUI from doing a full redraw after every single event so fast typing (especially in vim) no longer lags.

**Architecture:** Restructure the client event loop (`crates/amux-tui/src/app.rs::event_loop`) into *block → drain → draw*: block for the first event, then coalesce everything already queued on both sources (daemon `DaemonMsg` stream + crossterm event stream) via a `drain_ready` helper, then render exactly once per batch. No timer — zero added latency when idle; under load the backlog that piles up during a draw collapses into the next one.

**Tech Stack:** Rust, tokio, futures `Stream`, ratatui, crossterm, tokio-util `Framed` + `amux-proto` postcard codec.

## Global Constraints

Copied from `CLAUDE.md` and the design spec (`docs/superpowers/specs/2026-07-15-coalesced-draw-design.md`). Every task inherits these.

- **No wire change.** `PROTO_VERSION` stays **14**; do not touch `amux-proto` message types.
- **No new external dependency.** `futures`, `tokio`, `tokio-util` are already workspace deps; use them via existing imports. Do not add a crate.
- **View model stays a projection.** `App` state is mutated only through `on_daemon` (from `DaemonMsg`s) and the existing key/mouse handlers — this plan does not add new state sources.
- **No `unwrap()`/`expect()` in library code** (tests may `unwrap`). No `println!`/`eprintln!`/`dbg!` — `tracing` only.
- **Both platforms first-class** (macOS + Linux). This change is pure async/Rust with no platform-specific I/O, so parity is automatic.
- **Definition of done — all four green and observed:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace`.
- **Commits:** one logical change per commit; imperative subject (~72 chars); body explains why + mechanism + what the regression test proves; end with the trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Only commit when the user asks. Branch is `latencies` (not `main`).

---

## File Structure

- **Modify:** `crates/amux-tui/src/app.rs` — the only file touched.
  - Add a module-private free fn `drain_ready` (the coalescing primitive).
  - Add `App::handle_daemon` / `App::handle_event` (wrap today's inline `select!` arms).
  - Rewrite `event_loop` into block → drain → draw; genericize it over its two input streams and an injected render step.
  - Move `framed.split()`, `EventStream::new()`, `App::new`, terminal-size read, and the initial `draw` boundary into `run()`.
  - Add all new tests to the existing `#[cfg(test)] mod tests` block.

---

## Task 1: Add the `drain_ready` coalescing primitive

**Files:**
- Modify: `crates/amux-tui/src/app.rs` (add free fn near `event_loop`; add tests in `mod tests`)

**Interfaces:**
- Produces: `async fn drain_ready<S, T>(stream: &mut S, out: &mut Vec<T>) -> bool where S: futures::Stream<Item = T> + Unpin` — pulls every item already ready on `stream` into `out` without awaiting new I/O; returns `true` iff the stream ended (`None`), `false` if it is merely out of ready items for now (still open). Task 2 calls this.

- [ ] **Step 1: Write the failing unit tests**

Add to the `#[cfg(test)] mod tests` block at the bottom of `crates/amux-tui/src/app.rs`:

```rust
#[tokio::test]
async fn drain_ready_collects_all_ready_then_reports_ended() {
    let mut s = futures::stream::iter(vec![1, 2, 3]);
    let mut out = Vec::new();
    let ended = drain_ready(&mut s, &mut out).await;
    assert_eq!(out, vec![1, 2, 3]);
    assert!(ended, "an exhausted iter() reports the stream ended");
}

#[tokio::test]
async fn drain_ready_stops_at_pending_and_leaves_stream_open() {
    // Three ready items, then a source that never yields again.
    let mut s = futures::stream::iter(vec![1, 2, 3]).chain(futures::stream::pending::<i32>());
    let mut out = Vec::new();
    let ended = drain_ready(&mut s, &mut out).await;
    assert_eq!(out, vec![1, 2, 3]);
    assert!(!ended, "a pending tail means still-open, not ended");

    // A second drain finds nothing new and still reports open.
    let mut out2 = Vec::new();
    let ended2 = drain_ready(&mut s, &mut out2).await;
    assert!(out2.is_empty());
    assert!(!ended2);
}

#[tokio::test]
async fn drain_ready_on_empty_pending_collects_nothing() {
    let mut s = futures::stream::pending::<i32>();
    let mut out = Vec::new();
    let ended = drain_ready(&mut s, &mut out).await;
    assert!(out.is_empty());
    assert!(!ended);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p amux-tui drain_ready`
Expected: **compile error** — `cannot find function `drain_ready` in this scope`.

- [ ] **Step 3: Implement `drain_ready`**

Add this free function to `crates/amux-tui/src/app.rs`, immediately above `fn event_loop` (around line 109). `StreamExt` is already imported at the top of the file (`use futures::{SinkExt, StreamExt};`), which provides `poll_next_unpin`.

```rust
/// Pull every item already ready on `stream` into `out`, without awaiting new I/O. Returns
/// `true` iff the stream has ended (yielded `None`); `false` if it is merely out of ready items
/// for now (still open). This is the coalescing primitive: one call collects a whole burst so
/// the caller can render it in a single pass. See `docs/superpowers/specs/2026-07-15-coalesced-draw-design.md`.
async fn drain_ready<S, T>(stream: &mut S, out: &mut Vec<T>) -> bool
where
    S: futures::Stream<Item = T> + Unpin,
{
    loop {
        // Poll the stream exactly once, resolving immediately whether it is Ready or Pending —
        // never parking the task on new I/O.
        let polled =
            futures::future::poll_fn(|cx| std::task::Poll::Ready(stream.poll_next_unpin(cx))).await;
        match polled {
            std::task::Poll::Ready(Some(item)) => out.push(item),
            std::task::Poll::Ready(None) => return true, // stream ended
            std::task::Poll::Pending => return false,    // nothing more ready right now
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p amux-tui drain_ready`
Expected: **3 passed**.

- [ ] **Step 5: Commit**

```bash
git add crates/amux-tui/src/app.rs
git commit -m "$(cat <<'EOF'
Add drain_ready: non-blocking coalescing primitive for the TUI loop

drain_ready polls a stream once at a time via a poll_fn that always
returns Ready, collecting every item already buffered without ever
parking the task, and reports whether the stream has ended vs is merely
out of ready items. This is the primitive the coalesced draw loop uses
to collapse a burst of events into a single render. Tests cover the
all-ready-then-ended, ready-then-pending (still open), and empty-pending
cases.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Restructure `event_loop` into block → drain → draw

**Files:**
- Modify: `crates/amux-tui/src/app.rs` — `run` (~88-107), `event_loop` (~109-147), add `handle_daemon`/`handle_event` methods on `App`, add the `amux_proto::ProtoError` import (~line 29), add tests in `mod tests`.

**Interfaces:**
- Consumes: `drain_ready` from Task 1.
- Produces:
  - `async fn event_loop<S, E, R>(app: &mut App, sink: &mut Sink, stream: &mut S, events: &mut E, render: R, repo: PathBuf) -> Result<()>` where `S: futures::Stream<Item = Result<DaemonMsg, ProtoError>> + Unpin`, `E: futures::Stream<Item = std::io::Result<Event>> + Unpin`, `R: FnMut(&App) -> Result<()>`.
  - `async fn App::handle_daemon(&mut self, msg: Option<DaemonMsg>, sink: &mut Sink) -> Result<Flow>`
  - `async fn App::handle_event(&mut self, ev: Option<Event>, sink: &mut Sink) -> Result<Flow>`

- [ ] **Step 1: Add the `ProtoError` import**

In `crates/amux-tui/src/app.rs`, extend the existing proto import (currently `use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, RepoInfo, Size};`) to include `ProtoError`:

```rust
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, ProtoError, RepoInfo, Size};
```

- [ ] **Step 2: Add the two handler methods**

Add these to the `impl App` block (place them just before the existing `async fn on_daemon`, around line 323). They wrap today's inline `select!` arms and return the existing `Flow`. The caller collapses each stream item (`Some(Ok(x)) → Some(x)`, `None`/`Err → None`) before calling, so a decode error or a closed socket both read as "gone".

```rust
/// Apply one daemon message. `None` means the daemon stream ended or errored (the daemon
/// went away) → stop the loop.
async fn handle_daemon(&mut self, msg: Option<DaemonMsg>, sink: &mut Sink) -> Result<Flow> {
    match msg {
        Some(dm) => {
            self.on_daemon(dm, sink).await?;
            Ok(Flow::Continue)
        }
        None => Ok(Flow::Quit),
    }
}

/// Apply one terminal event. `None` means the event stream ended or errored → stop the loop.
async fn handle_event(&mut self, ev: Option<Event>, sink: &mut Sink) -> Result<Flow> {
    match ev {
        Some(Event::Key(key)) if key.kind == KeyEventKind::Press => self.on_key(key, sink).await,
        Some(Event::Resize(c, r)) => {
            self.on_resize(c, r, sink).await?;
            Ok(Flow::Continue)
        }
        Some(Event::Mouse(me)) => {
            self.on_mouse(me, sink).await?;
            Ok(Flow::Continue)
        }
        Some(Event::Paste(text)) => {
            self.on_paste(text, sink).await?;
            Ok(Flow::Continue)
        }
        Some(_) => Ok(Flow::Continue),
        None => Ok(Flow::Quit),
    }
}
```

- [ ] **Step 3: Genericize `event_loop` and move setup into `run` (behavior preserved)**

Replace the current `run` (lines ~88-107) and `event_loop` (lines ~109-147) with the following. The loop body is still one draw per event at this step — this is a pure refactor that introduces the testable seam (generic streams + injected `render`) without changing behavior.

```rust
pub async fn run() -> Result<()> {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    };
    let (framed, repo) = crate::client::connect().await?;
    let mut terminal = ratatui::init();
    // Capture the mouse so the wheel reaches panes (forwarded to apps that want it, else scrolls
    // amux's own scrollback). Hold Shift to bypass for native terminal selection.
    // Bracketed paste lets the outer terminal hand us a paste as one `Event::Paste` instead of a
    // storm of per-character key events — one write to the child, one redraw. See `on_paste`.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut app = App::new(cols, rows);
    let (mut sink, mut stream) = framed.split();
    let mut events = EventStream::new();

    let result = event_loop(
        &mut app,
        &mut sink,
        &mut stream,
        &mut events,
        |app| draw(&mut terminal, app),
        repo,
    )
    .await;

    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

/// The client event loop: block for an event, apply it, redraw. `stream` carries daemon
/// messages, `events` the terminal input; `render` draws the current view model (injected so
/// tests can drive the loop headless and count renders). Generic over the two stream types for
/// the same reason. (Restructured into block → drain → draw in the next step.)
async fn event_loop<S, E, R>(
    app: &mut App,
    sink: &mut Sink,
    stream: &mut S,
    events: &mut E,
    mut render: R,
    repo: PathBuf,
) -> Result<()>
where
    S: futures::Stream<Item = Result<DaemonMsg, ProtoError>> + Unpin,
    E: futures::Stream<Item = std::io::Result<Event>> + Unpin,
    R: FnMut(&App) -> Result<()>,
{
    // Register this client's repo with the (possibly shared) daemon so its agents show up here.
    sink.send(ClientMsg::AddRepo { path: repo }).await?;

    render(app)?;
    loop {
        let flow = tokio::select! {
            msg = stream.next() => app.handle_daemon(msg.and_then(|r| r.ok()), sink).await?,
            ev  = events.next() => app.handle_event(ev.and_then(|r| r.ok()), sink).await?,
        };
        if let Flow::Quit = flow {
            break;
        }
        // Report focus changes so the daemon can track read/unread.
        app.sync_focus(sink).await?;
        render(app)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Verify the refactor is behavior-preserving**

Run: `cargo build -p amux-tui && cargo test -p amux-tui`
Expected: builds clean; all existing tests pass (e.g. `paste_sends_one_wrapped_input_frame`, `scroll_mode_moves_and_clamps_within_scrollback`). The `drain_ready` tests from Task 1 still pass. `drain_ready` is now unused by non-test code and will warn — that is expected and resolved in Step 7; if `-D warnings` bites during this interim build, proceed to Step 5 without running clippy yet.

- [ ] **Step 5: Write the loop-level tests**

Add these helpers and tests to the `#[cfg(test)] mod tests` block. `ctrl` already exists there.

```rust
fn output(terminal: TerminalId, byte: u8) -> DaemonMsg {
    DaemonMsg::Output {
        terminal,
        bytes: vec![byte],
    }
}

/// A daemon stream that yields every message immediately, then pends once, then ends —
/// modelling a burst, then quiet, then disconnect, deterministically (no cross-stream timing).
fn burst_then_close(
    msgs: Vec<DaemonMsg>,
) -> impl futures::Stream<Item = Result<DaemonMsg, amux_proto::ProtoError>> + Unpin {
    let mut queue = msgs.into_iter();
    let mut pended = false;
    futures::stream::poll_fn(move |cx| {
        if let Some(m) = queue.next() {
            std::task::Poll::Ready(Some(Ok(m)))
        } else if !pended {
            pended = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(None)
        }
    })
}

/// A short socket pair whose write half becomes a real `Sink`; the read half is kept alive so
/// the loop's `AddRepo` send doesn't hit a broken pipe.
fn test_sink() -> (Sink, UnixStream) {
    let (client_end, server_end) = UnixStream::pair().unwrap();
    let (sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
    (sink, server_end)
}

/// THE regression test: a burst of Output frames must coalesce into a SINGLE render (plus the
/// initial one), not one render per frame. Old loop drew 1 + 3 = 4; coalesced loop draws 2.
#[tokio::test]
async fn output_burst_coalesces_into_one_render() {
    let (mut sink, _server) = test_sink();
    let mut app = App::new(100, 40);
    let t = TerminalId::new();
    let mut daemon = burst_then_close(vec![output(t, b'a'), output(t, b'b'), output(t, b'c')]);
    let mut events = futures::stream::pending::<std::io::Result<Event>>();

    let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let r = renders.clone();
    event_loop(
        &mut app,
        &mut sink,
        &mut daemon,
        &mut events,
        move |_app| {
            r.set(r.get() + 1);
            Ok(())
        },
        std::path::PathBuf::from("/repo"),
    )
    .await
    .unwrap();

    assert_eq!(
        renders.get(),
        2,
        "the 3-frame burst should coalesce into one render (plus the initial frame)"
    );
}

/// A single event with nothing else queued renders exactly once — the idle-typing common case
/// (no added latency, no dropped frame).
#[tokio::test]
async fn single_event_renders_once() {
    let (mut sink, _server) = test_sink();
    let mut app = App::new(100, 40);
    let t = TerminalId::new();
    let mut daemon = burst_then_close(vec![output(t, b'x')]);
    let mut events = futures::stream::pending::<std::io::Result<Event>>();

    let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let r = renders.clone();
    event_loop(
        &mut app,
        &mut sink,
        &mut daemon,
        &mut events,
        move |_app| {
            r.set(r.get() + 1);
            Ok(())
        },
        std::path::PathBuf::from("/repo"),
    )
    .await
    .unwrap();

    assert_eq!(renders.get(), 2, "initial frame + one render for the single event");
}

/// A quit (Ctrl-Q) stops the loop immediately and draws no trailing frame.
#[tokio::test]
async fn quit_key_stops_without_trailing_render() {
    let (mut sink, _server) = test_sink();
    let mut app = App::new(100, 40);
    let mut daemon = futures::stream::pending::<Result<DaemonMsg, amux_proto::ProtoError>>();
    let evs: Vec<std::io::Result<Event>> = vec![Ok(Event::Key(ctrl('q')))];
    let mut events = futures::stream::iter(evs);

    let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let r = renders.clone();
    event_loop(
        &mut app,
        &mut sink,
        &mut daemon,
        &mut events,
        move |_app| {
            r.set(r.get() + 1);
            Ok(())
        },
        std::path::PathBuf::from("/repo"),
    )
    .await
    .unwrap();

    assert_eq!(renders.get(), 1, "only the initial frame; quit short-circuits before any batch render");
}
```

- [ ] **Step 6: Run the loop tests to verify the regression test fails**

Run: `cargo test -p amux-tui output_burst_coalesces_into_one_render single_event_renders_once quit_key_stops_without_trailing_render`
Expected: `output_burst_coalesces_into_one_render` **FAILS** with `assertion `left == right` failed ... left: 4, right: 2` (the per-event loop drew four times). `single_event_renders_once` and `quit_key_stops_without_trailing_render` pass (they guard behavior that is already correct).

- [ ] **Step 7: Rewrite the loop body into block → drain → draw**

Replace the `loop { … }` body inside `event_loop` (the part after `render(app)?;`) with:

```rust
    render(app)?;
    loop {
        // Phase 1 — block until one source is ready.
        let mut quit = matches!(
            tokio::select! {
                msg = stream.next() => app.handle_daemon(msg.and_then(|r| r.ok()), sink).await?,
                ev  = events.next() => app.handle_event(ev.and_then(|r| r.ok()), sink).await?,
            },
            Flow::Quit
        );

        // Phase 2 — drain everything else already queued on both sources, without blocking.
        if !quit {
            let mut dmsgs = Vec::new();
            quit |= drain_ready(stream, &mut dmsgs).await; // did the daemon stream end?
            for m in dmsgs {
                if let Flow::Quit = app.handle_daemon(m.ok(), sink).await? {
                    quit = true;
                }
            }
            let mut evs = Vec::new();
            quit |= drain_ready(events, &mut evs).await; // did the event stream end?
            for e in evs {
                if let Flow::Quit = app.handle_event(e.ok(), sink).await? {
                    quit = true;
                }
            }
        }

        if quit {
            break; // teardown — no trailing render
        }
        // Report focus changes so the daemon can track read/unread.
        app.sync_focus(sink).await?;
        render(app)?; // exactly one render per drained batch
    }
    Ok(())
```

Also delete the now-stale one-line comment `// (Restructured into block → drain → draw in the next step.)` from the `event_loop` doc comment.

- [ ] **Step 8: Run the loop tests to verify they all pass**

Run: `cargo test -p amux-tui output_burst_coalesces_into_one_render single_event_renders_once quit_key_stops_without_trailing_render`
Expected: **3 passed**.

- [ ] **Step 9: Run the full Definition-of-Done gates**

Run each and confirm output:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```
Expected: all four clean. If clippy flags `quit |= drain_ready(...)` (e.g. a lint on the bit-or assign), keep it — it is idiomatic bool accumulation — but if it insists, rewrite as `if drain_ready(...).await { quit = true; }`.

- [ ] **Step 10: Verify end-to-end in the real app**

This is a latency change; a green suite is necessary but not sufficient (`superpowers:verification-before-completion`). Build and run the TUI, open a pane, run `vim`, and type quickly / hold a movement key. Confirm typing keeps up without the lag/backlog. Use the `run` skill if available.
```bash
cargo run
```
Expected (observed, not assumed): fast typing in vim tracks input without visible redraw lag; no visual corruption; quitting (`Ctrl-Q`) still exits cleanly.

- [ ] **Step 11: Commit**

```bash
git add crates/amux-tui/src/app.rs
git commit -m "$(cat <<'EOF'
Coalesce the TUI draw loop: one render per event batch

The client event loop drew a full frame after every single event. A vim
keystroke arrives as a burst of DaemonMsg::Output frames (the daemon
broadcasts one per PTY read), so one keystroke became many full redraws
serialized on the loop, and the draw backlog grew as you typed — visible
lag when typing fast.

Restructure event_loop into block -> drain -> draw: block for the first
event, then drain everything already queued on both the daemon stream
and the input stream via drain_ready, then render once. No timer, so
idle typing is unchanged; under load the backlog that accumulates during
a draw collapses into the next one. event_loop is now generic over its
two streams with an injected render step so it can be driven headless.

Regression test output_burst_coalesces_into_one_render drives a 3-frame
burst through the loop and asserts a single batch render (2 renders total
with the initial frame); it fails at 4 against the old per-event loop and
passes at 2 after. Guard tests cover the idle single-event and quit paths.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**1. Spec coverage.**
- Spec §1 (block → drain → draw via `drain_ready`, no timer) → Task 1 (`drain_ready`) + Task 2 Step 7 (loop body). ✓
- Spec §2 (extract `handle_daemon`/`handle_event`) → Task 2 Step 2. ✓
- Spec §3 (testability seam: generic streams + injected render + real sink via `UnixStream::pair()`) → Task 2 Steps 3 & 5. ✓
- Spec Testing #1 coalescing / #2 idle / #3 quit → Task 2 Step 5 tests (`output_burst_coalesces_into_one_render`, `single_event_renders_once`, `quit_key_stops_without_trailing_render`) + the `drain_ready` unit tests in Task 1. ✓
- Spec "out of scope" (daemon batching, backpressure, per-draw cost, no wire change, no new dep) → Global Constraints + nothing in the plan violates them. ✓

**2. Placeholder scan.** No TBD/TODO; every code step shows complete code; every run step shows the command and expected output. ✓

**3. Type consistency.** `drain_ready<S, T>(&mut S, &mut Vec<T>) -> bool` is defined in Task 1 and consumed with `S = daemon/event streams`, `T = Result<DaemonMsg, ProtoError>` / `std::io::Result<Event>` in Task 2. `handle_daemon(Option<DaemonMsg>)` / `handle_event(Option<Event>)` are defined in Step 2 and called with `msg.and_then(|r| r.ok())` / `m.ok()` (both yielding those `Option`s) in Steps 3 & 7. `event_loop`'s generic bounds match the concrete production types (`SplitStream<Framed<…>>` item = `Result<DaemonMsg, ProtoError>`; `EventStream` item = `std::io::Result<Event>`) and the test streams. `Flow { Continue, Quit }` is the existing enum, reused. ✓

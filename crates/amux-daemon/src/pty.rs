//! A persistent PTY session, shared across client attach/detach. The reader thread feeds a
//! vt100 parser (the screen for snapshots, and the scrollback clients page through — see
//! [`Session::scroll_step`]) and a broadcast channel (for live attach); a waiter thread
//! reaps the child and flips a `watch` on exit. Killing is by PID, so no one has to own the
//! `Child` mutably while others read/write. See `docs/DESIGN.md` §5.
//!
//! Load-bearing details (§11): reader on a dedicated thread, drop the slave after spawn, and
//! treat both `Ok(0)` (macOS EOF) and `Err` (Linux EIO) as "closed". The parser is also the pane's
//! terminal for *queries*, not just for rendering — see [`query_reply`] and §5.3.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, watch};

use amux_proto::Size;

/// The `TERM` advertised to panes, resolved once. We mirror tmux: a screen-family terminfo has no
/// `bce` (background-color-erase) capability, so full-screen apps like vim paint every cell's
/// background explicitly instead of relying on the terminal to fill erased regions with the default
/// background. That's what makes a dark vim theme fill the whole pane here (an `xterm-256color`
/// TERM, which *has* `bce`, leaves vim's "empty" cells at the pane's default background — the
/// black-only-behind-text look). Prefer `tmux-256color` (adds italics), fall back to the
/// universally present `screen-256color`. Paired with `COLORTERM=truecolor` so 24-bit color still
/// works (neither screen terminfo advertises it).
fn pane_term() -> &'static str {
    static TERM: OnceLock<&'static str> = OnceLock::new();
    TERM.get_or_init(|| {
        if terminfo_exists("tmux-256color") {
            "tmux-256color"
        } else {
            "screen-256color"
        }
    })
}

/// Whether a terminfo entry named `name` is installed (via `infocmp`, part of ncurses). Used to
/// avoid handing panes a `TERM` the system can't describe.
fn terminfo_exists(name: &str) -> bool {
    std::process::Command::new("infocmp")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Scrollback retained by the daemon-side parser. This *is* the scroll history clients page
/// through ([`Session::scroll_step`]) — the daemon owns it, so there is exactly one copy and no
/// client has to be handed it up front.
///
/// The cost is real but proportional to use: a scrolled-off line holds `cols` cells of 32 bytes, so
/// roughly 3.8 MB per 1000 lines at 120 columns, allocated only as a session actually produces
/// history. 5000 lines is ~25 screenfuls under `less`-like use and covers a long agent
/// conversation; a session that never scrolls costs nothing.
const SCROLLBACK: usize = 5000;

/// Where a client's scrolled-back window sits, and how deep history was when it was served.
///
/// The pair is what makes a *relative* step exact: comparing the recorded depth with the current one
/// says how much output has arrived since, which is how far the window has drifted from the live
/// view. Held per client (two clients scroll the same session independently), never by the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPos {
    pub offset: usize,
    pub depth: usize,
}

/// A scroll window as escape codes: each row of `screen` drawn at the row it occupies.
///
/// Emitted row by row rather than with `contents_formatted()` because that honours `row.wrapped()`,
/// and a resize makes that flag lie. vt100's `set_size` re-widths the *visible* grid only — saved
/// rows keep the width they were recorded at — so after a pane gets wider, rows that wrapped at the
/// old width are joined back into one line and the window comes back part-empty: a 40-row pane can
/// render 20 lines of history and 20 blank rows. Drawing one saved row per display row keeps the
/// window full and puts history where it was when it scrolled past.
///
/// Safe despite `rows_formatted`'s contract that a row's bytes assume the cursor already sits on the
/// row that row came from: here row `i` of the window really is drawn at row `i`. At a stable width
/// this is byte-for-byte equivalent to `contents_formatted()` (both round-trip to the screen's own
/// `rows()`), so it changes nothing but the mixed-width case.
///
/// History is not reflowed — nothing in vt100 can re-wrap a saved row — so a row saved wider than the
/// pane is truncated at the right edge. One row in, one row out, whatever the width.
fn window_bytes(screen: &vt100::Screen) -> Vec<u8> {
    let cols = screen.size().1;
    let mut out = b"\x1b[H\x1b[J".to_vec(); // clear, so a short row leaves blanks not leftovers
    for (i, row) in screen.rows_formatted(0, cols).enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H", i + 1).as_bytes());
        out.extend_from_slice(&row);
    }
    out
}

/// One screenful of a session's history, plus where it sits — the reply to a scroll request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollFrame {
    /// Lines back from the live view, *clamped* to what history exists. The client renders what it
    /// is given rather than tracking the limit itself.
    pub offset: usize,
    /// How far back history goes right now, i.e. the largest meaningful `offset`.
    pub available: usize,
    /// The window as escape codes, ready to feed to a parser (see [`window_bytes`]).
    pub bytes: Vec<u8>,
}

/// Broadcast backlog per session (chunks). On overflow a lagging client resyncs via snapshot.
const OUTPUT_BACKLOG: usize = 1024;

/// The reply to a terminal query, or `None` for anything we do not answer.
///
/// A pane's terminal is the daemon's vt100 parser, so *it* has to answer the queries a real
/// terminal would — an unanswered query is not a cosmetic gap: the asking program blocks reading
/// the reply and swallows the user's keystrokes while it waits. `gh auth login` hung exactly this
/// way at its first text prompt (survey asks DSR-CPR before drawing an editable line).
///
/// The whitelist is deliberately tiny: reply only to what we can answer truthfully, and leave every
/// other query as silently ignored (which is what it was before). `cursor` is vt100's 0-based
/// `(row, col)` and `size` its `(rows, cols)`; every reply is 1-based and inside the screen.
///
/// The clamp is load-bearing. vt100 represents a pending wrap — the cursor resting at the right
/// margin after a character landed in the last column — as `col == cols`, which is not a position
/// any terminal reports. Answering with it gives a column one past the screen, and a full-screen app
/// that lays out from the answer (Claude Code does) then computes nonsense and draws garbage.
///
/// Known gap, shared with tmux: vt100 has no DECOM, so CPR is absolute even under origin mode.
fn query_reply(
    intermediate: Option<u8>,
    params: &[&[u16]],
    c: char,
    cursor: (u16, u16),
    size: (u16, u16),
) -> Option<Vec<u8>> {
    let first = params.first().and_then(|p| p.first().copied());
    let row = cursor.0.min(size.0.saturating_sub(1)) + 1;
    let col = cursor.1.min(size.1.saturating_sub(1)) + 1;
    match (intermediate, c, first) {
        // DSR-CPR — "where is the cursor?"
        (None, 'n', Some(6)) => Some(format!("\x1b[{row};{col}R").into_bytes()),
        // DSR — "are you there?"; 0 means "ready, no malfunction".
        (None, 'n', Some(5)) => Some(b"\x1b[0n".to_vec()),
        // DECXCPR — CPR plus a page number; we have one page, so it is always 1.
        (Some(b'?'), 'n', Some(6)) => Some(format!("\x1b[?{row};{col};1R").into_bytes()),
        // DA1 — "what are you?". `ESC[c` and `ESC[0c` are the same request. VT100 with AVO, the
        // same identity tmux reports.
        (None, 'c', None | Some(0)) => Some(b"\x1b[?1;2c".to_vec()),
        _ => None,
    }
}

/// Query replies collected while parsing, drained by the reader thread.
///
/// The callback only *queues* — it runs with the parser lock held, and writing to the PTY there
/// would let a child that has stopped reading its input stall parsing (and every snapshot with it).
#[derive(Default)]
struct Queries {
    pending: Vec<u8>,
}

impl vt100::Callbacks for Queries {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i2.is_some() {
            return;
        }
        if let Some(reply) = query_reply(i1, params, c, screen.cursor_position(), screen.size()) {
            self.pending.extend_from_slice(&reply);
        }
    }
}

struct SessionIo {
    master: Box<dyn MasterPty + Send>,
}

/// The PTY's input side. Shared, because query replies originate on the reader thread while user
/// input arrives on tokio tasks; the mutex keeps a reply from interleaving into a keystroke.
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// A live PTY session. Cheap to `Arc`-share; all methods take `&self`.
pub struct Session {
    io: Mutex<SessionIo>,
    writer: Writer,
    parser: Arc<Mutex<vt100::Parser<Queries>>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    pid: Option<u32>,
    exit_rx: watch::Receiver<bool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// The `ws_ypixel` value currently set on the PTY (0 or 1). `request_repaint` toggles it to force
    /// a SIGWINCH without touching rows/cols; `resize` carries it so it never toggles by accident.
    winch_nudge: AtomicU16,
}

impl Session {
    pub fn spawn(
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
        size: Size,
    ) -> Result<Arc<Self>> {
        anyhow::ensure!(!command.is_empty(), "cannot spawn an empty command");

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&command[0]);
        for arg in &command[1..] {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        for (key, value) in env {
            cmd.env(key, value);
        }
        // amux owns TERM/COLORTERM for every pane — applied last so they win over any inherited or
        // adapter-provided value. See `pane_term`.
        cmd.env("TERM", pane_term());
        cmd.env("COLORTERM", "truecolor");

        let child = pair.slave.spawn_command(cmd).context("spawn child")?;
        drop(pair.slave); // child owns the only slave fd → its exit is observable
        let pid = child.process_id();

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;
        let writer: Writer = Arc::new(Mutex::new(writer));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            size.rows,
            size.cols,
            SCROLLBACK,
            Queries::default(),
        )));
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(false);
        let exit_code = Arc::new(Mutex::new(None));

        // Reader thread: pump PTY output into the parser and the broadcast.
        let reader_parser = Arc::clone(&parser);
        let reader_tx = output_tx.clone();
        let reader_writer = Arc::clone(&writer);
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        // Answer any terminal query in this chunk, but write the reply *after*
                        // dropping the parser lock — see `Queries`.
                        let mut reply = Vec::new();
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(chunk);
                            reply.append(&mut parser.callbacks_mut().pending);
                        }
                        if !reply.is_empty() {
                            if let Ok(mut writer) = reader_writer.lock() {
                                let _ = writer.write_all(&reply).and_then(|()| writer.flush());
                            }
                        }
                        let _ = reader_tx.send(chunk.to_vec()); // Err only means "no attached client"
                    }
                    Err(_) => break, // EIO on Linux, etc.
                }
            }
        });

        // Waiter thread: own the child, reap it, then flip the exit watch.
        let waiter_code = Arc::clone(&exit_code);
        let waiter_thread = thread::spawn(move || {
            let mut child = child;
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            *waiter_code.lock().unwrap() = code;
            let _ = exit_tx.send(true);
            // exit_tx drops here; receivers observe the change (and then Closed), which we treat
            // as exit either way.
        });

        Ok(Arc::new(Self {
            io: Mutex::new(SessionIo {
                master: pair.master,
            }),
            writer,
            parser,
            output_tx,
            pid,
            exit_rx,
            exit_code,
            threads: Mutex::new(vec![reader_thread, waiter_thread]),
            winch_nudge: AtomicU16::new(0),
        }))
    }

    /// Current screen as a `contents_formatted()` dump — sent to a (re)attaching client. Prefixed
    /// with a **mode preamble** because `contents_formatted()` replays cells + attributes but NOT
    /// terminal modes: without this, a client rebuilding its parser from the snapshot would lose
    /// mouse tracking (breaks wheel forwarding), the alternate screen, and DECCKM (arrow keys).
    ///
    /// Deliberately the visible screen only. History is not shipped on attach — clients page through
    /// it on demand via [`Self::scroll_step`], so a snapshot stays small no matter how deep history
    /// runs, and scrolling depth doesn't depend on when a client happened to attach.
    pub fn snapshot(&self) -> Vec<u8> {
        let Ok(parser) = self.parser.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let screen = parser.screen();
        if screen.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        if screen.application_cursor() {
            out.extend_from_slice(b"\x1b[?1h");
        }
        out.extend_from_slice(match screen.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => b"".as_slice(),
            vt100::MouseProtocolMode::Press => b"\x1b[?9h",
            vt100::MouseProtocolMode::PressRelease => b"\x1b[?1000h",
            vt100::MouseProtocolMode::ButtonMotion => b"\x1b[?1002h",
            vt100::MouseProtocolMode::AnyMotion => b"\x1b[?1003h",
        });
        out.extend_from_slice(match screen.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => b"".as_slice(),
            vt100::MouseProtocolEncoding::Utf8 => b"\x1b[?1005h",
            vt100::MouseProtocolEncoding::Sgr => b"\x1b[?1006h",
        });
        // The scroll region (DECSTBM), if the app set one away from the full screen. Without it a
        // reattaching client — which rebuilds its parser from these bytes — keeps a full-screen
        // region and scrolls the whole pane where the app scrolls a sub-range, so the display drifts
        // on the next line feed and stays wrong until the app repaints. Emitted before
        // `contents_formatted` (which repositions the cursor for every row), and 1-based per the
        // escape's convention against vt100's 0-based `scroll_region`.
        let (top, bottom) = screen.scroll_region();
        let rows = screen.size().0;
        if (top, bottom) != (0, rows.saturating_sub(1)) {
            out.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
        }
        out.extend_from_slice(&screen.contents_formatted());
        out
    }

    /// Move a client's scroll position by `lines` and serve the window it lands on.
    ///
    /// This is the whole of scrolling: the daemon holds the history, so a client never keeps its own
    /// copy (`DESIGN.md` §2, invariant 2) and depth is whatever this session has retained rather than
    /// whatever a client happened to observe live.
    ///
    /// `from` is where we last put that client (`None` = at the live view). History grows underneath
    /// a scrolled-back client, and that growth is exactly how far its window has drifted from live,
    /// so it is added back before the step: one keypress moves one line from *what the user is
    /// looking at*, even on a pane that is streaming output.
    ///
    /// Depth is read and the window rendered under a single lock, and the same depth is both used for
    /// that correction and reported as `available`. Splitting them across two locks loses drift
    /// permanently — a chunk landing in between records the *new* depth against the *old* offset, so
    /// the growth it represents can never be accounted for again.
    ///
    /// The target is clamped to available history and the clamped value comes back in the frame, so a
    /// client renders where it actually is rather than predicting it.
    ///
    /// A pane on the alternate screen (a full-screen app) has no history by design: vt100 saves lines
    /// only as they scroll off the *normal* screen, and none at all while a scroll region is set. Such
    /// a session reports `available: 0`, which clients render as "nothing to scroll".
    pub fn scroll_step(&self, from: Option<ScrollPos>, lines: i32) -> ScrollFrame {
        let Ok(mut parser) = self.parser.lock() else {
            return ScrollFrame {
                offset: 0,
                available: 0,
                bytes: Vec::new(),
            };
        };
        // `set_scrollback` clamps to what exists, so this reads the depth: ask for everything, see
        // what we got. The live view sits at offset 0 and is restored before returning, so nothing
        // else (snapshots, status heuristics) ever sees a moved view.
        parser.screen_mut().set_scrollback(usize::MAX);
        let available = parser.screen().scrollback();

        let base = match from {
            Some(pos) => pos.offset + available.saturating_sub(pos.depth),
            None => 0,
        };
        let target = if lines >= 0 {
            base.saturating_add(lines as usize)
        } else {
            base.saturating_sub(lines.unsigned_abs() as usize)
        };

        parser.screen_mut().set_scrollback(target);
        let offset = parser.screen().scrollback(); // clamped by vt100 to what exists
        let bytes = window_bytes(parser.screen());
        parser.screen_mut().set_scrollback(0);
        ScrollFrame {
            offset,
            available,
            bytes,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub fn exit_rx(&self) -> watch::Receiver<bool> {
        self.exit_rx.clone()
    }

    pub fn is_exited(&self) -> bool {
        *self.exit_rx.borrow()
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap()
    }

    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).context("write to pty")?;
        writer.flush().context("flush pty")?;
        Ok(())
    }

    pub fn resize(&self, size: Size) -> Result<()> {
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(size.rows, size.cols);
        }
        let io = self.io.lock().unwrap();
        io.master
            .resize(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                // Carry the current repaint nudge so a same-size resize stays a true no-op: the
                // kernel raises SIGWINCH only when the winsize *struct* changes, and `request_repaint`
                // owns that bit. If `resize` hard-coded 0 here it would itself toggle the struct
                // whenever a repaint had set the nudge to 1, firing a spurious redraw.
                pixel_height: self.winch_nudge.load(Ordering::Relaxed),
            })
            .context("resize pty")?;
        Ok(())
    }

    /// Make the pane's app repaint its whole screen, without changing its size.
    ///
    /// The reattach snapshot is a best-effort reconstruction: vt100 exposes no getter for some
    /// terminal state (origin mode, tab stops, charset), so a client parser rebuilt from a snapshot
    /// can diverge from what the app intends, leaving a pane that reattaches blank or stale until the
    /// app next redraws on its own. A full-screen app redraws everything on SIGWINCH, which heals any
    /// such gap — so on attach we provoke one.
    ///
    /// The kernel raises SIGWINCH only when the `winsize` struct changes (`tty_do_resize` memcmp's the
    /// whole struct, pixel fields included). We therefore toggle `ws_ypixel` between 0 and 1: rows and
    /// cols never change, so the vt100 grid and the client's size are untouched and nothing is
    /// truncated — the struct differs just enough to signal. **Do not "simplify" this to reissuing the
    /// current size: that is a byte-identical struct, the kernel deduplicates it, and no signal
    /// fires.** A plain shell ignores SIGWINCH, which is fine: its content is ordinary scrollback the
    /// snapshot already carries in full; only full-screen apps have hidden state to redraw.
    pub fn request_repaint(&self) -> Result<()> {
        let nudge = self.winch_nudge.fetch_xor(1, Ordering::Relaxed) ^ 1;
        let io = self.io.lock().unwrap();
        let current = io.master.get_size().context("get pty size")?;
        io.master
            .resize(PtySize {
                rows: current.rows,
                cols: current.cols,
                pixel_width: 0,
                pixel_height: nudge,
            })
            .context("nudge winsize for repaint")?;
        Ok(())
    }

    /// Terminate the child by PID (SIGKILL). The waiter thread then reaps and flips the watch.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }

    /// Ask the child to exit (SIGTERM), wait up to `grace` for it, then SIGKILL.
    ///
    /// Used when the daemon is shutting down on purpose — an upgrade eviction, `amux daemon
    /// --stop`, a logout. A coding agent killed outright never gets to checkpoint; SIGTERM gives
    /// it the chance, and the deadline guarantees shutdown still completes if it doesn't take it.
    /// Ungraceful teardown (SIGKILL, a lost SSH connection, a panic) skips this by definition,
    /// which is why durable state is written as it changes rather than on the way out.
    pub async fn terminate(&self, grace: Duration) {
        let Some(pid) = self.pid else {
            return;
        };
        if self.is_exited() {
            return;
        }
        if kill(Pid::from_raw(pid as i32), Signal::SIGTERM).is_err() {
            return; // already gone
        }
        let mut exit_rx = self.exit_rx();
        // `changed()` resolves on the waiter thread's flip; a closed channel also means exited.
        let _ = tokio::time::timeout(grace, exit_rx.changed()).await;
        if !self.is_exited() {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
        if let Ok(mut threads) = self.threads.lock() {
            for handle in threads.drain(..) {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session fed `n` numbered lines through `cat`, once the parser has caught up (bounded wait,
    /// so a stuck child fails an assertion rather than hanging the suite).
    fn session_with_lines(rows: u16, n: usize) -> Arc<Session> {
        // The shell *prints* the lines itself, then `exec cat` keeps the session alive for tests
        // that inject more input later. Feeding the lines in via stdin instead made history hold
        // two copies of each — the PTY line discipline echoes input the moment it arrives, so a
        // duplicate copy landed alongside cat's output and a window sampled on the seam between
        // them showed the tail of one copy above the head of the next. `stty -echo` can't prevent
        // it (echo fires before the shell runs stty); generating on stdout sidesteps it entirely.
        let gen = format!("i=0; while [ $i -lt {n} ]; do echo \"line $i\"; i=$((i+1)); done");
        let session = Session::spawn(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("stty -echo; {gen}; exec cat"),
            ],
            Path::new("/"),
            &[],
            Size { rows, cols: 40 },
        )
        .expect("spawn sh");

        let needle = format!("line {}", n - 1);
        for _ in 0..100 {
            if let Ok(parser) = session.parser.lock() {
                if parser.screen().contents().contains(&needle) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        session
    }

    /// Render a frame the way the TUI does: feed it to a parser and read the screen.
    fn rendered(frame: &ScrollFrame, rows: u16) -> String {
        let mut client = vt100::Parser::new(rows, 40, 0);
        client.process(&frame.bytes);
        client.screen().contents()
    }

    /// The core of daemon-owned scrolling: a client asks for a window into history and gets exactly
    /// that window, with depth that has nothing to do with when it attached. Previously the client
    /// scrolled a private ring that only held output it had personally witnessed since its last
    /// attach — so scrolling reached ~nothing on a freshly opened pane and reset on every agent
    /// switch.
    #[test]
    fn scroll_step_serves_history_a_window_at_a_time() {
        let session = session_with_lines(10, 200);

        let live = session.scroll_step(None, 0);
        assert_eq!(live.offset, 0);
        assert!(
            live.available >= 150,
            "history should be deep after 200 lines on a 10-row screen; got {}",
            live.available
        );
        assert!(
            rendered(&live, 10).contains("line 199"),
            "offset 0 is the live view"
        );

        // Scrolled back to the very start, the client sees the beginning of the output.
        let oldest = session.scroll_step(None, i32::MAX);
        assert_eq!(
            oldest.offset, live.available,
            "asked for the top, got the top"
        );
        let text = rendered(&oldest, 10);
        assert!(
            text.contains("line 0") || text.contains("line 1"),
            "the deepest window holds the first lines, got: {text:?}"
        );

        // And a window in between is genuinely in between.
        let middle = session.scroll_step(None, (live.available / 2) as i32);
        let middle_text = rendered(&middle, 10);
        assert!(
            !middle_text.contains("line 199") && !middle_text.contains("line 0"),
            "a mid-history window shows neither end, got: {middle_text:?}"
        );
    }

    /// The daemon clamps, and says what it clamped to. The client renders the reply rather than
    /// tracking the limit itself, so an over-scroll can't leave the two disagreeing about position.
    #[test]
    fn scroll_step_clamps_past_the_end_of_history() {
        let session = session_with_lines(10, 100);
        let frame = session.scroll_step(None, i32::MAX);
        assert_eq!(
            frame.offset, frame.available,
            "an absurd offset comes back clamped to the oldest line"
        );
        assert!(!frame.bytes.is_empty(), "and still renders a window");
    }

    /// Serving a window must not disturb the session: it borrows the parser's scroll position, and
    /// snapshots (and the status heuristics) must still see the live screen afterwards.
    #[test]
    fn scroll_step_leaves_the_live_view_untouched() {
        let session = session_with_lines(10, 100);
        let _ = session.scroll_step(None, 50);

        assert_eq!(
            session.parser.lock().unwrap().screen().scrollback(),
            0,
            "the parser is back at the live view"
        );
        let mut client = vt100::Parser::new(10, 40, 0);
        client.process(&session.snapshot());
        assert!(
            client.screen().contents().contains("line 99"),
            "a snapshot taken after a scroll still shows the newest output"
        );
    }

    /// The re-basing that makes a relative step exact: with output arriving under a scrolled-back
    /// client, a zero-line step must land on *the same content*, and a one-line step exactly one line
    /// older. Both depend on depth and window coming from one lock — measuring depth separately loses
    /// the correction for good, which showed up as the view jumping a whole batch of new output.
    #[test]
    fn scroll_step_re_bases_past_output_that_arrived_since() {
        let session = session_with_lines(10, 200);
        let first = session.scroll_step(None, 20);
        let before = rendered(&first, 10);
        let pos = ScrollPos {
            offset: first.offset,
            depth: first.available,
        };

        // More output lands while the client sits scrolled back.
        session.write_input(b"extra 0\nextra 1\nextra 2\n").unwrap();
        for _ in 0..100 {
            if session.scroll_step(None, 0).available > first.available {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        let held = session.scroll_step(Some(pos), 0);
        assert_eq!(
            rendered(&held, 10),
            before,
            "a zero-line step holds the window it was showing, however much output arrived"
        );
        assert!(
            held.offset > pos.offset,
            "and it did so by moving deeper ({} -> {}), not by standing still",
            pos.offset,
            held.offset
        );

        let stepped = session.scroll_step(
            Some(ScrollPos {
                offset: held.offset,
                depth: held.available,
            }),
            1,
        );
        let after = rendered(&stepped, 10);
        let (before_rows, after_rows): (Vec<_>, Vec<_>) =
            (before.lines().collect(), after.lines().collect());
        assert_eq!(
            after_rows[1..],
            before_rows[..before_rows.len() - 1],
            "one line older than what the user was looking at"
        );
    }

    /// A session whose output wraps, so saved rows carry vt100's `wrapped` flag — the flag a resize
    /// invalidates.
    fn session_with_wrapping_lines(rows: u16, cols: u16, n: usize) -> Arc<Session> {
        let session = Session::spawn(
            &["cat".to_string()],
            Path::new("/"),
            &[],
            Size { rows, cols },
        )
        .expect("spawn cat");
        // Comfortably wider than `cols`, so every line occupies two saved rows.
        let input: String = (0..n)
            .map(|i| format!("{i:03}:{}\n", "abcdefghij".repeat(4)))
            .collect();
        session.write_input(input.as_bytes()).expect("write");
        let needle = format!("{:03}:", n - 1);
        for _ in 0..100 {
            if let Ok(parser) = session.parser.lock() {
                if parser.screen().contents().contains(&needle) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        session
    }

    /// A window must be full of history, whatever the pane has been resized to since. vt100 re-widths
    /// only the visible grid, so saved rows keep the width they were recorded at; serving them with
    /// `contents_formatted()` re-joined rows that had wrapped at the old width and left the rest of
    /// the window blank — up to half a tall pane, which reads as wildly wrong spacing.
    #[test]
    fn a_widened_pane_still_serves_a_full_window_of_history() {
        let session = session_with_wrapping_lines(10, 40, 120);
        // The pane gets wider — a window resize, a closed split, a mini promoted to the main area.
        session.resize(Size { rows: 10, cols: 80 }).unwrap();

        let frame = session.scroll_step(None, 20);
        let mut client = vt100::Parser::new(10, 80, 0);
        client.process(&frame.bytes);
        let rows: Vec<String> = client.screen().rows(0, 80).collect();

        assert_eq!(rows.len(), 10, "a full screenful");
        let blank = rows.iter().filter(|r| r.trim().is_empty()).count();
        assert_eq!(
            blank, 0,
            "no blank filler: one saved row per display row, got {rows:#?}"
        );
    }

    /// The property that keeps the change honest: at a stable width, serving a window is exactly what
    /// the daemon's own screen shows at that offset — for wrapped and styled output, at every offset.
    /// Any drift here is a formatting surprise in a scrolled pane.
    #[test]
    fn a_served_window_matches_the_screen_it_came_from() {
        let session = session_with_wrapping_lines(6, 40, 60);
        let mut parser = session.parser.lock().unwrap();
        parser.screen_mut().set_scrollback(usize::MAX);
        let depth = parser.screen().scrollback();
        assert!(depth > 10, "precondition: some history to walk");

        for offset in 0..=depth {
            parser.screen_mut().set_scrollback(offset);
            let expected: Vec<String> = parser.screen().rows(0, 40).collect();
            let bytes = window_bytes(parser.screen());
            let mut client = vt100::Parser::new(6, 40, 0);
            client.process(&bytes);
            let actual: Vec<String> = client.screen().rows(0, 40).collect();
            assert_eq!(actual, expected, "window at offset {offset} differs");
        }
        parser.screen_mut().set_scrollback(0);
    }

    /// A session with nothing scrolled off reports no history, which is what lets the client say
    /// "nothing to scroll" instead of entering a scroll mode that can't move.
    #[test]
    fn scroll_step_reports_no_history_for_a_fresh_session() {
        let session = Session::spawn(
            &["cat".to_string()],
            Path::new("/"),
            &[],
            Size { rows: 10, cols: 40 },
        )
        .expect("spawn cat");
        let frame = session.scroll_step(None, 3);
        assert_eq!((frame.offset, frame.available), (0, 0));
    }

    /// End-to-end proof that a query is *answered*, not merely parsed: the child asks for the
    /// cursor position and reads the reply back off its own stdin. Before the daemon answered
    /// queries this read blocked forever — which is how `gh auth login` froze a pane, its prompt
    /// silently eating every keystroke while it waited for a report that never came.
    ///
    /// `-icanon` because the reply arrives without a newline, and the line discipline would
    /// otherwise hold it until one; real apps put the terminal in raw mode themselves.
    #[test]
    fn a_cursor_position_query_is_answered_to_the_child() {
        let script = "stty -echo -icanon; printf '\\033[3;7H\\033[6n'; \
                      r=$(head -c 6 | tr -d '\\033'); printf 'GOT<%s>' \"$r\"";
        let session = Session::spawn(
            &["sh".to_string(), "-c".to_string(), script.to_string()],
            Path::new("/"),
            &[],
            Size { rows: 10, cols: 40 },
        )
        .expect("spawn sh");

        for _ in 0..100 {
            if let Ok(parser) = session.parser.lock() {
                if parser.screen().contents().contains("GOT<") {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        let contents = session.parser.lock().unwrap().screen().contents();
        assert!(
            contents.contains("GOT<[3;7R>"),
            "child never read a DSR-CPR reply; screen was {contents:?}"
        );
    }

    /// A reattaching client rebuilds its parser from `snapshot()`, so the snapshot must carry the
    /// terminal's scroll region (DECSTBM). Without it the client defaults to a full-screen region
    /// while the daemon keeps the app's, and identical later output scrolls differently — the pane
    /// goes wrong on reattach over SSH and stays wrong until the app repaints. vt100 upstream has no
    /// getter for the region; our vendored copy adds one (see the root Cargo.toml [patch]).
    #[test]
    fn snapshot_restores_the_scroll_region() {
        // A child that sets a scroll region of rows 2..=5 (1-based) and then keeps the session live.
        let session = Session::spawn(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf '\\033[2;5r'; exec cat".to_string(),
            ],
            Path::new("/"),
            &[],
            Size { rows: 8, cols: 20 },
        )
        .expect("spawn sh");

        // Wait until the daemon parser has applied the region (0-based (1, 4)).
        let mut daemon_region = (0, 0);
        for _ in 0..100 {
            if let Ok(parser) = session.parser.lock() {
                daemon_region = parser.screen().scroll_region();
                if daemon_region == (1, 4) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            daemon_region,
            (1, 4),
            "precondition: the child set the region"
        );

        // Rebuild a client parser exactly as the TUI does on OutputSnapshot.
        let mut client = vt100::Parser::new(8, 20, 0);
        client.process(&session.snapshot());
        assert_eq!(
            client.screen().scroll_region(),
            daemon_region,
            "the snapshot must reproduce the daemon's scroll region"
        );
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;

    /// `(row, col)` is vt100's 0-based cursor; replies are 1-based, as every terminal reports them.
    const CURSOR: (u16, u16) = (2, 6);
    /// A screen big enough that `CURSOR` needs no clamping — the clamp has its own test.
    const SIZE: (u16, u16) = (24, 80);

    /// A query as `csi_dispatch` hands it over: leading intermediate, params, final byte.
    type Query<'a> = (Option<u8>, &'a [&'a [u16]], char);

    /// A cursor sitting at the right margin with a pending wrap must be reported *at* the margin,
    /// not one past it. vt100 tracks the pending-wrap state as `col == width`, which is not a
    /// position any terminal reports: on a 10-column screen the answer is column 10, never 11.
    ///
    /// This matters far beyond tidiness. A full-screen app asks where the cursor is and lays out
    /// from the answer — Claude Code does — so an impossible column corrupts its arithmetic and it
    /// draws garbage. The bug shipped in the same change that started answering queries at all.
    #[test]
    fn a_cursor_at_the_margin_is_reported_inside_the_screen() {
        let size = (5u16, 10u16); // rows, cols
                                  // (chars written, expected reply) — one short of the margin, exactly at it, past it.
        let cases = [
            (3usize, "\x1b[1;4R"),
            (9, "\x1b[1;10R"),
            (10, "\x1b[1;10R"), // at the margin: clamped, not 11
            (11, "\x1b[2;2R"),  // wrapped onto the next row
            (20, "\x1b[2;10R"),
        ];
        for (n, want) in cases {
            let mut parser =
                vt100::Parser::new_with_callbacks(size.0, size.1, 0, Queries::default());
            parser.process("x".repeat(n).as_bytes());
            parser.process(b"\x1b[6n");
            let got = String::from_utf8_lossy(&parser.callbacks().pending).to_string();
            assert_eq!(got, want, "after {n} chars on a {size:?} screen");
        }

        // The same clamp on the last row: the cursor cannot be reported below the screen.
        let mut parser = vt100::Parser::new_with_callbacks(size.0, size.1, 0, Queries::default());
        parser.process("line\r\n".repeat(12).as_bytes());
        parser.process(b"\x1b[6n");
        let got = String::from_utf8_lossy(&parser.callbacks().pending).to_string();
        assert_eq!(got, "\x1b[5;1R", "row is clamped to the screen height");
    }

    #[test]
    fn answers_the_whitelisted_queries() {
        let cases: &[(Query, &[u8])] = &[
            // DSR-CPR: the one that hung `gh auth login` — survey asks where the cursor is and
            // blocks reading the reply, swallowing keystrokes until it arrives.
            ((None, &[&[6]], 'n'), b"\x1b[3;7R"),
            ((None, &[&[5]], 'n'), b"\x1b[0n"),
            ((Some(b'?'), &[&[6]], 'n'), b"\x1b[?3;7;1R"),
            // DA1: bare `ESC[c` parses as no params at all, `ESC[0c` as an explicit 0.
            ((None, &[&[0]], 'c'), b"\x1b[?1;2c"),
            ((None, &[], 'c'), b"\x1b[?1;2c"),
        ];
        for ((i1, params, c), want) in cases {
            assert_eq!(
                query_reply(*i1, params, *c, CURSOR, SIZE).as_deref(),
                Some(*want),
                "query {i1:?} {params:?} {c}"
            );
        }
    }

    #[test]
    fn stays_silent_for_everything_else() {
        let cases: &[Query] = &[
            (None, &[&[3]], 'n'),       // DSR-3, printer status: not ours to answer
            (None, &[&[1]], 'c'),       // DA1 with a param we don't recognize
            (Some(b'>'), &[], 'c'),     // DA2 — deliberately out of scope
            (Some(b'>'), &[], 'q'),     // XTVERSION — deliberately out of scope
            (Some(b'?'), &[&[5]], 'n'), // no DECDSR-5 in the whitelist
            (None, &[&[1]], 'z'),       // not a query at all
        ];
        for (i1, params, c) in cases {
            assert_eq!(
                query_reply(*i1, params, *c, CURSOR, SIZE),
                None,
                "query {i1:?} {params:?} {c}"
            );
        }
    }
}

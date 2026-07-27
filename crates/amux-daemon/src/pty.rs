//! A persistent PTY session, shared across client attach/detach. The reader thread feeds a
//! vt100 parser (for snapshots) and a broadcast channel (for live attach); a waiter thread
//! reaps the child and flips a `watch` on exit. Killing is by PID, so no one has to own the
//! `Child` mutably while others read/write. See `docs/DESIGN.md` §5.
//!
//! Load-bearing details (§11): reader on a dedicated thread, drop the slave after spawn, and
//! treat both `Ok(0)` (macOS EOF) and `Err` (Linux EIO) as "closed".

use std::io::{Read, Write};
use std::path::Path;
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

/// Scrollback retained by the daemon-side parser (used for snapshots).
const SCROLLBACK: usize = 2000;
/// How many lines of that scrollback ride along in a snapshot. Below [`SCROLLBACK`] on purpose:
/// a snapshot is re-sent on every attach (every agent switch), and the replay costs roughly a
/// screenful of escape codes per screenful of history — deep enough to scroll back through a
/// conversation, bounded well under `MAX_FRAME_BYTES`.
const SNAPSHOT_SCROLLBACK: usize = 1000;

/// Escape codes that reproduce the parser's scrollback in a *client's* parser, oldest line first.
///
/// A client rebuilds its parser from the snapshot, so its scrollback ring starts empty and can only
/// be filled the way a live one is: by lines scrolling off the top of the screen. So walk history a
/// screenful at a time — paint the window, then park on the last row and emit exactly enough
/// newlines to scroll that window off — and the lines land in the client's ring in order. Per
/// screenful rather than per row because a single serialized row assumes the cursor already sits on
/// the row it came from (`rows_formatted`'s contract), while `contents_formatted` positions a whole
/// screen for itself.
///
/// Leaves the parser's scroll position as it found it.
fn history_replay(parser: &mut vt100::Parser) -> Vec<u8> {
    let restore = parser.screen().scrollback();
    // `set_scrollback` clamps to what's there, so this asks for "as far back as possible".
    parser.screen_mut().set_scrollback(usize::MAX);
    let available = parser.screen().scrollback().min(SNAPSHOT_SCROLLBACK);
    let rows = usize::from(parser.screen().size().0).max(1);

    let mut out = Vec::new();
    let mut back = available;
    while back > 0 {
        parser.screen_mut().set_scrollback(back);
        out.extend_from_slice(&parser.screen().contents_formatted());
        // The last window is short when history isn't a whole multiple of a screen; scrolling only
        // `back` lines there leaves the live screen's own rows in place, to be overwritten by the
        // repaint that follows this replay.
        let scroll = back.min(rows);
        out.extend_from_slice(format!("\x1b[{rows};1H").as_bytes());
        for _ in 0..scroll {
            out.extend_from_slice(b"\r\n");
        }
        back -= scroll;
    }

    parser.screen_mut().set_scrollback(restore);
    out
}
/// Broadcast backlog per session (chunks). On overflow a lagging client resyncs via snapshot.
const OUTPUT_BACKLOG: usize = 1024;

struct SessionIo {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
}

/// A live PTY session. Cheap to `Arc`-share; all methods take `&self`.
pub struct Session {
    io: Mutex<SessionIo>,
    parser: Arc<Mutex<vt100::Parser>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    pid: Option<u32>,
    exit_rx: watch::Receiver<bool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
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
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            size.rows, size.cols, SCROLLBACK,
        )));
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(false);
        let exit_code = Arc::new(Mutex::new(None));

        // Reader thread: pump PTY output into the parser and the broadcast.
        let reader_parser = Arc::clone(&parser);
        let reader_tx = output_tx.clone();
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(chunk);
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
                writer,
            }),
            parser,
            output_tx,
            pid,
            exit_rx,
            exit_code,
            threads: Mutex::new(vec![reader_thread, waiter_thread]),
        }))
    }

    /// Current screen as a `contents_formatted()` dump — sent to a (re)attaching client. Prefixed
    /// with **scrollback** and a **mode preamble**, because `contents_formatted()` describes only
    /// the visible screen: without the former a client could scroll back zero lines, and without
    /// the latter it would lose mouse tracking (breaks wheel forwarding), the alternate screen,
    /// and DECCKM (arrow keys).
    pub fn snapshot(&self) -> Vec<u8> {
        let Ok(mut parser) = self.parser.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        out.extend_from_slice(&history_replay(&mut parser));
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
        out.extend_from_slice(&screen.contents_formatted());
        out
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
        let mut io = self.io.lock().unwrap();
        io.writer.write_all(bytes).context("write to pty")?;
        io.writer.flush().context("flush pty")?;
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
                pixel_height: 0,
            })
            .context("resize pty")?;
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

    /// Feed a session `n` numbered lines through `cat` and return the snapshot once the parser has
    /// caught up (or after a bounded wait, so a stuck child fails the assertion, not the suite).
    fn snapshot_after_lines(rows: u16, n: usize) -> Vec<u8> {
        let session = Session::spawn(
            &["cat".to_string()],
            Path::new("/"),
            &[],
            Size { rows, cols: 40 },
        )
        .expect("spawn cat");
        let input: String = (0..n).map(|i| format!("line {i}\n")).collect();
        session.write_input(input.as_bytes()).expect("write");

        // `cat` echoes back through the PTY; poll until the newest line has landed.
        let needle = format!("line {}", n - 1);
        for _ in 0..100 {
            if let Ok(parser) = session.parser.lock() {
                if parser.screen().contents().contains(&needle) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        session.snapshot()
    }

    /// Rebuild a client-side parser from `snapshot` bytes, exactly as the TUI does.
    fn replay(snapshot: &[u8], rows: u16) -> vt100::Parser {
        let mut client = vt100::Parser::new(rows, 40, SCROLLBACK);
        client.process(snapshot);
        client
    }

    /// THE regression guard: a snapshot must carry scrollback, not just the visible screen. It
    /// didn't, so every attach — and attaches happen on every agent switch — handed the client a
    /// parser with an empty ring. The wheel then "scrolled" a pane that had no history in it,
    /// silently doing nothing until enough new output had accumulated to refill it.
    #[test]
    fn snapshot_carries_scrollback_so_a_reattaching_client_can_scroll_back() {
        let snapshot = snapshot_after_lines(10, 200);
        let mut client = replay(&snapshot, 10);

        client.screen_mut().set_scrollback(usize::MAX);
        let depth = client.screen().scrollback();
        assert!(
            depth >= 150,
            "a client rebuilt from the snapshot must inherit history; got {depth} lines"
        );

        // Scrolled all the way back, it sees the *start* of the output — the part that was
        // previously unreachable.
        let oldest = client.screen().contents();
        assert!(
            oldest.contains("line 5"),
            "the oldest window should hold early lines, got: {oldest:?}"
        );

        // And the live view is still correct: history replay must not disturb the visible screen.
        client.screen_mut().set_scrollback(0);
        let live = client.screen().contents();
        assert!(
            live.contains("line 199"),
            "the visible screen must still show the newest output, got: {live:?}"
        );
    }

    /// History is capped, and the cap is what bounds the frame — a session that has scrolled far
    /// more than [`SNAPSHOT_SCROLLBACK`] lines must not replay its whole ring.
    #[test]
    fn snapshot_scrollback_is_bounded() {
        // The daemon parser keeps SCROLLBACK lines; ask for more history than the cap allows.
        let snapshot = snapshot_after_lines(10, SNAPSHOT_SCROLLBACK + 300);
        let mut client = replay(&snapshot, 10);
        client.screen_mut().set_scrollback(usize::MAX);
        let depth = client.screen().scrollback();
        assert!(
            depth <= SNAPSHOT_SCROLLBACK,
            "history must be capped at {SNAPSHOT_SCROLLBACK}, got {depth}"
        );
    }

    /// `history_replay` borrows the live parser to walk its ring; it must hand back the scroll
    /// position it found, or the daemon's own view would drift every time a client attached.
    #[test]
    fn history_replay_restores_the_scroll_position() {
        let mut parser = vt100::Parser::new(4, 20, SCROLLBACK);
        for i in 0..50 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(7);
        let _ = history_replay(&mut parser);
        assert_eq!(parser.screen().scrollback(), 7, "scroll position restored");
    }
}

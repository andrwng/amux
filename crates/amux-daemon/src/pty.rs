//! A persistent PTY session, shared across client attach/detach. The reader thread feeds a
//! vt100 parser (the screen for snapshots, and the scrollback clients page through — see
//! [`Session::scroll_step`]) and a broadcast channel (for live attach); a waiter thread
//! reaps the child and flips a `watch` on exit. Killing is by PID, so no one has to own the
//! `Child` mutably while others read/write. See `docs/DESIGN.md` §5.
//!
//! Load-bearing details (§11): reader on a dedicated thread, drop the slave after spawn, and
//! treat both `Ok(0)` (macOS EOF) and `Err` (Linux EIO) as "closed". PTY *writes* run on their own
//! dedicated thread fed by a bounded channel ([`InputTx`]) — never on the reader thread — so a
//! child that stops draining its stdin can block only that writer, not reading, output, or input
//! routing; writing on the reader thread once froze whole panes. The parser is also the pane's
//! terminal for *queries*, not just for rendering — see [`query_reply`] and §5.3.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Claude Code environment markers that flag a session as a nested/child run. Inherited from
/// amux's own launch context (if amux was started from within a `claude` session) and scrubbed
/// from every PTY child so agents always run as clean top-level sessions. See `Session::spawn`.
/// `CLAUDECODE`/`CLAUDE_CODE_ENTRYPOINT` are Claude Code's own "running inside Claude" markers;
/// `CLAUDE_CODE_CHILD_SESSION` is the one whose banner ("Transcript saving is off") was observed.
const CHILD_SESSION_ENV_SCRUB: &[&str] = &[
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDECODE",
    "CLAUDE_CODE_SSE_PORT",
];

/// Continuous, per-session grid tracing: the record of what turned a grid content→blank.
///
/// The client-side `AMUX_RECORD` tap resets on every TUI reattach, so it always *starts* after the
/// grid is already blank; this lives in the daemon and spans a session's whole life. Per session it
/// logs spawn, every resize (nonblank rows before→after), each app-output chunk that flips the grid
/// content↔blank (flagged if it carried a `2J`/`J`), and attach. It is **always on** once the daemon
/// enables it at startup ([`enable_grid_trace`], default `<amux_home>/grid-trace.log`, override
/// `AMUX_GRID_TRACE`), because the one time it was needed most it was off: the blank was never
/// recorded. Off until enabled — unit and integration tests never enable it, so `cargo test` cannot
/// write into a real home. Lines carry the daemon pid so several daemons (one per repo) sharing a
/// file stay distinguishable. Bounded: the file rotates to `.1` past [`GRID_TRACE_CAP`] (§2.6).
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);
static GRID_TRACE: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Size past which the trace file rotates (one previous generation kept as `<path>.1`). Lines are
/// written only on transitions, so this covers weeks of normal use.
const GRID_TRACE_CAP: u64 = 4 * 1024 * 1024;

/// Turn grid tracing on for this process, writing to `path`. Called once by the daemon at startup;
/// a second call is ignored.
pub fn enable_grid_trace(path: std::path::PathBuf) {
    let _ = GRID_TRACE.set(path);
}

fn grid_trace_path() -> Option<&'static Path> {
    GRID_TRACE.get().map(|p| p.as_path())
}

fn gtrace(msg: &str) {
    let Some(path) = grid_trace_path() else {
        return;
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("{ms} pid={} {msg}\n", std::process::id());
    let _ = append_bounded(path, &line, GRID_TRACE_CAP);
}

/// Bytes of recent raw PTY output kept per session while tracing, so that when a grid blanks we can
/// dump the *stream that led to it* — the only way to root-cause a divergence, since the daemon's
/// vt100 is the source of truth and a diff-only app never resends what was lost. Bounded (§2).
const BLANK_CAPTURE_CAP: usize = 512 * 1024;

/// Dump a session's recent raw output to a file beside the grid trace, so the bytes that blanked a
/// grid can be replayed byte-for-byte through a fresh parser. Named by pid/sid/time; best-effort.
fn dump_blank_capture(sid: u64, ring: &std::collections::VecDeque<u8>) {
    let Some(trace) = grid_trace_path() else {
        return;
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("blanked-pid{}-sid{sid}-{ms}.bin", std::process::id());
    let path = trace.with_file_name(name);
    let bytes: Vec<u8> = ring.iter().copied().collect();
    if std::fs::write(&path, &bytes).is_ok() {
        gtrace(&format!(
            "sid={sid} BLANK-CAPTURE {} bytes -> {}",
            bytes.len(),
            path.display()
        ));
    }
}

/// Append `line` to `path`, rotating the file to `<path>.1` first when it has grown past `cap` —
/// so the trace can stay on forever without growing without bound. One previous generation is kept
/// so the events leading up to a rotation are not lost.
fn append_bounded(path: &Path, line: &str, cap: u64) -> std::io::Result<()> {
    if std::fs::metadata(path)
        .map(|m| m.len() > cap)
        .unwrap_or(false)
    {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        std::fs::rename(path, rotated)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

/// Number of visible rows that hold a **glyph** — the "is the screen blank?" signal for tracing.
///
/// A cell an app wrote a space into has non-empty `contents()` (" "), so counting non-empty cells
/// calls a screen of styled spaces "content". That is exactly the blank-pane signature — a
/// diff-renderer whose model went blank overwrites its old text with spaces — and it went unlogged
/// for that reason: 49 space-only rows read as `nb=49`. Only a non-whitespace glyph counts.
fn nonblank_rows(screen: &vt100::Screen) -> usize {
    let (rows, cols) = screen.size();
    (0..rows)
        .filter(|&y| {
            (0..cols).any(|x| {
                screen
                    .cell(y, x)
                    .is_some_and(|c| !c.contents().trim().is_empty())
            })
        })
        .count()
}

/// The first bytes of an output chunk, escape-visible and bounded, for the trace: when a chunk
/// blanks a grid, *what the app wrote* is the evidence, and it is not otherwise recorded anywhere
/// unless a client happened to be attached with `AMUX_RECORD`.
fn chunk_head(chunk: &[u8]) -> String {
    const HEAD: usize = 512;
    let shown = &chunk[..chunk.len().min(HEAD)];
    let mut out = String::with_capacity(shown.len() * 2);
    for &b in shown {
        match b {
            0x1b => out.push_str("\\e"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    if chunk.len() > HEAD {
        out.push_str(&format!("…(+{} bytes)", chunk.len() - HEAD));
    }
    out
}

/// Cap on a held partial escape: past this, flush rather than stall on a malformed unterminated
/// sequence. Well beyond any real CSI a program emits, and generous enough for a large OSC (e.g. an
/// OSC 52 clipboard write) that legitimately spans several reads.
const MAX_ESC_CARRY: usize = 64 * 1024;

/// How much of `buf` is safe to hand the parser *now* — everything except a **trailing incomplete
/// escape sequence**, which must be carried to the next read.
///
/// A PTY `read()` splits escape sequences at arbitrary byte boundaries. If the daemon parser buffers
/// such a partial, a snapshot taken between reads (on attach or resize) captures the grid but not the
/// partial, and a client rebuilt from that snapshot then parses the sequence's *tail* as literal text
/// — garbled/condensed rendering (`snapshot_between_halves_of_a_split_escape_round_trips`). Holding
/// the partial here, outside the parser, keeps the parser and the broadcast escape-aligned, so no
/// snapshot is ever mid-sequence and the client's byte stream matches the daemon's.
fn complete_prefix_len(buf: &[u8]) -> usize {
    const ESC: u8 = 0x1b;
    let Some(i) = buf.iter().rposition(|&b| b == ESC) else {
        return buf.len(); // no escape at all — all plain, all safe
    };
    let tail = &buf[i..];
    if escape_is_complete(tail) || tail.len() > MAX_ESC_CARRY {
        buf.len()
    } else {
        i // hold from the last ESC; everything before it is complete
    }
}

/// Whether `s` (which starts at an ESC) is a complete escape sequence.
fn escape_is_complete(s: &[u8]) -> bool {
    const ESC: u8 = 0x1b;
    let Some(&kind) = s.get(1) else {
        return false; // a lone trailing ESC
    };
    match kind {
        // CSI `ESC [ (params/intermediates)* final` — terminates at the first byte in 0x40..=0x7e.
        b'[' => s[2..].iter().any(|&b| (0x40..=0x7e).contains(&b)),
        // OSC `ESC ] … (BEL | ST)`.
        b']' => s.contains(&0x07) || s.windows(2).any(|w| w == [ESC, b'\\']),
        // DCS/SOS/PM/APC `ESC {P,X,^,_} … ST`.
        b'P' | b'X' | b'^' | b'_' => s.windows(2).any(|w| w == [ESC, b'\\']),
        // Charset designation `ESC (intermediate) (final)` — three bytes.
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => s.len() >= 3,
        // Any other two-byte escape (`ESC 7`, `ESC =`, `ESC M`, …) — complete once its byte is here.
        _ => true,
    }
}

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

/// Bytes that reconstruct `screen` in a fresh parser — the reattach snapshot.
///
/// A reattaching client rebuilds its parser from these bytes and is then fed the identical live
/// stream, so this must restore **every** piece of vt100 state or the two parsers diverge — and the
/// app, a diff renderer that only emits changes and never a full repaint, will never correct the
/// difference. That divergence is the whole cause of the blank/condensed reattach bugs.
///
/// vt100's state surface is small and closed, and this reproduces all of it:
///   * cells + attributes + cursor — `contents_formatted`;
///   * alternate screen — `?1049h` (a screen switch, so not in `input_mode_formatted`);
///   * application keypad/cursor, bracketed paste, mouse mode + encoding — `input_mode_formatted`;
///   * scroll region (DECSTBM) and origin mode (DECOM) — emitted here.
///
/// vt100 models no charset or tab stops, so there is nothing else to carry;
/// `snapshot_round_trips_all_vt100_state` is the guard that keeps this list complete.
///
/// Ordering is load-bearing. DECSTBM goes before the contents (it homes the cursor, but the contents
/// reposition every row anyway). DECOM goes *after*: `contents_formatted` addresses rows with
/// absolute CUP, which origin mode would reinterpret as region-relative, so the contents must run
/// with DECOM off; enabling it then homes the cursor (vt100's `set_origin_mode`), so the app's cursor
/// is re-placed region-relative afterwards to match how the app itself addresses it.
fn snapshot_bytes(screen: &vt100::Screen) -> Vec<u8> {
    let mut out = Vec::new();
    if screen.alternate_screen() {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(&screen.input_mode_formatted());
    let (top, bottom) = screen.scroll_region();
    let rows = screen.size().0;
    if (top, bottom) != (0, rows.saturating_sub(1)) {
        out.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
    }
    out.extend_from_slice(&screen.contents_formatted());
    if screen.origin_mode() {
        out.extend_from_slice(b"\x1b[?6h");
        let (crow, ccol) = screen.cursor_position();
        out.extend_from_slice(
            format!("\x1b[{};{}H", crow.saturating_sub(top) + 1, ccol + 1).as_bytes(),
        );
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
/// The whitelist covers the queries a CLI is likely to *block* on — cursor position (DSR-CPR),
/// status (DSR), and primary/secondary device attributes (DA1/DA2) — plus XTVERSION, all answered
/// with a conservative identity so nothing gates risky output on a capability we lack. Every other
/// query is silently ignored, which for capability probes (kitty keyboard, DECRQM) is the correct
/// "unsupported" answer: the app times out and falls back to behavior amux actually handles.
/// Answering a capability we don't implement would be worse than silence. `cursor` is vt100's
/// 0-based `(row, col)` and `size` its `(rows, cols)`; every reply is 1-based and inside the screen.
///
/// The clamp is load-bearing. vt100 represents a pending wrap — the cursor resting at the right
/// margin after a character landed in the last column — as `col == cols`, which is not a position
/// any terminal reports. Answering with it gives a column one past the screen, and a full-screen app
/// that lays out from the answer (Claude Code does) then computes nonsense and draws garbage.
///
/// Known minor gap, shared with tmux: the CPR reply is absolute even under origin mode — we do not
/// subtract the scroll region here, though the snapshot now restores DECOM for rendering.
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
        // DA2 (secondary device attributes) — `ESC[>c` / `ESC[>0c`. A CLI that blocks on this
        // (as `gh` did on DSR) would wedge forever unanswered. Report a conservative identity —
        // terminal type 0, version 0 — so nothing gates advanced output on a version we don't have.
        (Some(b'>'), 'c', None | Some(0)) => Some(b"\x1b[>0;0;0c".to_vec()),
        // XTVERSION — `ESC[>q` / `ESC[>0q` (Claude Code sends this at startup). Reply the DCS name
        // string; identifying as "amux" (unknown to feature-detectors) keeps apps on safe defaults.
        (Some(b'>'), 'q', None | Some(0)) => Some(b"\x1bP>|amux\x1b\\".to_vec()),
        // Deliberately NOT answered: the kitty keyboard query `ESC[?u`. A reply *claims* support for
        // the progressive-enhancement protocol, which would make the app expect kitty-encoded key
        // events — an encoding amux's input layer does not produce, so answering would break keys.
        // Silence is the correct "unsupported" signal; the app falls back to legacy keys, which we
        // do send. (Revisit if amux ever encodes kitty key events.)
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

/// Bytes queued to the PTY input side. A **dedicated writer thread** owns the write half and does
/// the (potentially blocking) `write_all`; everyone else — user keystrokes on tokio tasks, query
/// replies on the reader thread — only enqueues here. This is load-bearing for liveness: a PTY
/// write blocks when the child stops draining its stdin, so writing on the reader thread (as this
/// once did, under a shared lock) froze the whole pane — no output *and* no input — the instant a
/// child stopped reading while a reply was owed. Now only the writer thread can block; reading,
/// parsing, and broadcast keep going.
///
/// Bounded, and `try_send`-with-drop on both paths, so a wedged child cannot grow this without
/// limit (§2.6). Dropping is the right failure: a child that will not read its input is not helped
/// by queueing more of it, and query replies are best-effort anyway.
type InputTx = std::sync::mpsc::SyncSender<Vec<u8>>;

/// Capacity of the per-session input queue (messages). Keystrokes and query replies are tiny and
/// normally drained immediately; this only fills when the child has stopped reading, where dropping
/// is correct.
const INPUT_QUEUE: usize = 1024;

/// A live PTY session. Cheap to `Arc`-share; all methods take `&self`.
pub struct Session {
    io: Mutex<SessionIo>,
    input_tx: Option<InputTx>,
    parser: Arc<Mutex<vt100::Parser<Queries>>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    pid: Option<u32>,
    exit_rx: watch::Receiver<bool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Process-wide session number, for correlating grid-trace lines (see `gtrace`).
    sid: u64,
}

impl Session {
    pub fn spawn(
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
        size: Size,
    ) -> Result<Arc<Self>> {
        anyhow::ensure!(!command.is_empty(), "cannot spawn an empty command");

        let sid = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        gtrace(&format!(
            "sid={sid} spawn cmd={} {}x{}",
            command[0], size.cols, size.rows
        ));

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
        // Scrub Claude Code's "I am a nested/child session" markers from the inherited environment
        // before they reach the child. If amux itself was ever launched from inside a Claude Code
        // session (a terminal running `claude`), these vars are set in the daemon's environment and
        // would propagate into every agent amux spawns — putting each agent into Claude's degraded
        // *child-session* mode ("inherited CLAUDE_CODE_CHILD_SESSION marker · Transcript saving is
        // off"), which was found active on exactly the panes that go blank. amux owns each agent's
        // top-level session, so a child-session marker inherited from amux's own launch context is
        // never correct here. Applied to every PTY child (agents and shells) as environment hygiene.
        for key in CHILD_SESSION_ENV_SCRUB {
            cmd.env_remove(key);
        }
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
        let mut writer = pair.master.take_writer().context("take writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            size.rows,
            size.cols,
            SCROLLBACK,
            Queries::default(),
        )));
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(false);
        let exit_code = Arc::new(Mutex::new(None));

        // Writer thread: the *only* place a PTY write happens, so a write that blocks (child not
        // draining its stdin) blocks nothing but this thread — reading and parsing continue. It
        // exits when the last sender drops (Session gone) or the PTY dies (write error).
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
        let writer_sid = sid;
        let writer_thread = thread::spawn(move || {
            while let Ok(bytes) = input_rx.recv() {
                // Everything amux sends the child passes through here — user keystrokes and the
                // query replies the reader enqueues. Trace it (bounded, escapes visible) so a
                // spontaneous clear can be correlated with whatever amux wrote just before: the
                // blank captures showed Claude clearing with no amux resize/hook, and the one thing
                // left unlogged was what amux writes to the PTY. Off unless `AMUX_GRID_TRACE`.
                if grid_trace_path().is_some() {
                    gtrace(&format!(
                        "sid={writer_sid} WRITE len={} {}",
                        bytes.len(),
                        chunk_head(&bytes)
                    ));
                }
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });

        // Reader thread: pump PTY output into the parser and the broadcast, and *enqueue* query
        // replies (never write them here — that is what froze panes).
        let reader_parser = Arc::clone(&parser);
        let reader_tx = output_tx.clone();
        let reader_input_tx = input_tx.clone();
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut prev_nb: usize = 0; // for grid-trace transition detection
            let mut carry: Vec<u8> = Vec::new(); // a trailing partial escape held for the next read
                                                 // Recent raw output, kept only while tracing, to dump when a grid blanks (root-cause
                                                 // evidence). Holds the bytes fed to the parser, so a replay reconstructs the divergence.
            let tracing = grid_trace_path().is_some();
            let mut ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Prepend anything held from a mid-escape boundary, then split off a new
                        // trailing partial so the parser and the broadcast stay escape-aligned and no
                        // snapshot ever lands mid-sequence (see `complete_prefix_len`).
                        let data = if carry.is_empty() {
                            buf[..n].to_vec()
                        } else {
                            let mut d = std::mem::take(&mut carry);
                            d.extend_from_slice(&buf[..n]);
                            d
                        };
                        let keep = complete_prefix_len(&data);
                        carry = data[keep..].to_vec();
                        let chunk = &data[..keep];
                        if chunk.is_empty() {
                            continue; // the whole read was an unfinished escape — wait for the rest
                        }
                        if tracing {
                            ring.extend(chunk.iter().copied());
                            while ring.len() > BLANK_CAPTURE_CAP {
                                ring.pop_front();
                            }
                        }
                        // Answer any terminal query in this chunk, but only *after* dropping the
                        // parser lock — see `Queries` — and only by enqueuing: a blocking write here
                        // would stall reading. Best-effort, so drop the reply if the queue is full.
                        let mut reply = Vec::new();
                        let mut traced: Option<(usize, (u16, u16))> = None;
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(chunk);
                            reply.append(&mut parser.callbacks_mut().pending);
                            if grid_trace_path().is_some() {
                                traced =
                                    Some((nonblank_rows(parser.screen()), parser.screen().size()));
                            }
                        }
                        // Log a content↔blank transition and what drove it — the app's own output
                        // for this chunk (a `2J`/`ED` clear is the usual culprit).
                        if let Some((nb, (rows, cols))) = traced {
                            let blanked = prev_nb >= 4 && nb <= 1;
                            let filled = prev_nb <= 1 && nb >= 4;
                            if blanked || filled {
                                let c2j = chunk.windows(4).any(|w| w == b"\x1b[2J");
                                let cj = chunk.windows(3).any(|w| w == b"\x1b[J");
                                gtrace(&format!(
                                    "sid={sid} OUTPUT grid {prev_nb}->{nb} {cols}x{rows} 2J={c2j} J={cj} len={}",
                                    chunk.len()
                                ));
                                // The bytes that blanked it are the evidence; keep them — the
                                // culprit chunk inline, and the whole recent stream as a file to
                                // replay through a fresh parser.
                                if blanked {
                                    gtrace(&format!("sid={sid} BLANKED-BY {}", chunk_head(chunk)));
                                    dump_blank_capture(sid, &ring);
                                }
                            }
                            prev_nb = nb;
                        }
                        if !reply.is_empty() {
                            let _ = reader_input_tx.try_send(reply);
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
            input_tx: Some(input_tx),
            parser,
            output_tx,
            pid,
            exit_rx,
            exit_code,
            threads: Mutex::new(vec![reader_thread, writer_thread, waiter_thread]),
            sid,
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
        snapshot_bytes(parser.screen())
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

    /// Queue keystrokes for the PTY. Non-blocking: the dedicated writer thread does the actual
    /// write, so a child that has stopped reading its stdin cannot block the caller (a tokio task).
    /// The queue is bounded; if it is full — only when the child is not draining, where more input
    /// helps nothing — the keystrokes are dropped rather than allowed to grow unbounded (§2.6).
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        use std::sync::mpsc::TrySendError;
        let Some(tx) = self.input_tx.as_ref() else {
            anyhow::bail!("pty writer gone");
        };
        match tx.try_send(bytes.to_vec()) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => anyhow::bail!("pty writer gone"),
        }
    }

    /// The grid's current size, `(rows, cols)` as an `amux_proto::Size`. This is authoritative for a
    /// client: it sizes its own parser to match, and crops/pads its viewport to it.
    pub fn size(&self) -> Size {
        self.parser
            .lock()
            .map(|p| {
                let (rows, cols) = p.screen().size();
                Size { rows, cols }
            })
            .unwrap_or(Size { rows: 0, cols: 0 })
    }

    /// Resize the grid to `size` exactly. Returns whether the size actually changed (so the caller
    /// can re-snapshot only when it did).
    ///
    /// The grid always tracks the size of the pane the attached client shows it in — set on attach
    /// and on any later layout change, exactly like tmux resizes a pty to its client. A resize does
    /// send the app SIGWINCH; a diff-rendering TUI (ink/Claude Code) answers it with a full
    /// clear+repaint, so the grid re-fills on its own. In the gap before that repaint the pane is
    /// still not blank: `set_size` reflows the existing rows into the new grid, and `attach`/the
    /// `Resize` handler serve that reflowed grid as a fresh snapshot. A *same-size* reattach — the
    /// common reconnect — hits the early return below and sends no SIGWINCH at all.
    pub fn resize(&self, size: Size) -> Result<bool> {
        let mut parser = self.parser.lock().unwrap();
        let (cur_rows, cur_cols) = parser.screen().size();
        if (size.rows, size.cols) == (cur_rows, cur_cols) {
            return Ok(false);
        }
        let nb_before = if grid_trace_path().is_some() {
            nonblank_rows(parser.screen())
        } else {
            0
        };
        parser.screen_mut().set_size(size.rows, size.cols);
        let nb_after = if grid_trace_path().is_some() {
            nonblank_rows(parser.screen())
        } else {
            0
        };
        drop(parser);
        gtrace(&format!(
            "sid={} RESIZE {cur_cols}x{cur_rows}->{}x{} nb {nb_before}->{nb_after}",
            self.sid, size.cols, size.rows
        ));
        // A resize that empties the grid is the reconnect-blank smoking gun: keep-bottom `set_size`
        // dropped the content rows, and unless the app fully repaints they are gone. Distinguishes a
        // resize-caused blank (this line) from an app-output-caused one (`BLANKED-BY` in the reader).
        if grid_trace_path().is_some() && nb_before >= 4 && nb_after <= 1 {
            gtrace(&format!(
                "sid={} RESIZE-BLANKED {cur_cols}x{cur_rows}->{}x{} dropped {} nonblank rows",
                self.sid,
                size.cols,
                size.rows,
                nb_before - nb_after
            ));
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
        Ok(true)
    }

    /// Log a named lifecycle event (e.g. an attach) against this session's grid — no-op unless
    /// `AMUX_GRID_TRACE` is set. Lets the server correlate attach with the grid's blank state.
    pub fn trace_event(&self, event: &str) {
        if grid_trace_path().is_none() {
            return;
        }
        let (nb, rows, cols) = self
            .parser
            .lock()
            .map(|p| {
                let (r, c) = p.screen().size();
                (nonblank_rows(p.screen()), r, c)
            })
            .unwrap_or((0, 0, 0));
        gtrace(&format!("sid={} {event} nb={nb} {cols}x{rows}", self.sid));
    }

    /// Terminate the child by PID (SIGKILL). The waiter thread then reaps and flips the watch.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            gtrace(&format!("sid={} SIGNAL SIGKILL pid={pid}", self.sid));
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
        gtrace(&format!("sid={} SIGNAL SIGTERM pid={pid}", self.sid));
        if kill(Pid::from_raw(pid as i32), Signal::SIGTERM).is_err() {
            return; // already gone
        }
        let mut exit_rx = self.exit_rx();
        // `changed()` resolves on the waiter thread's flip; a closed channel also means exited.
        let _ = tokio::time::timeout(grace, exit_rx.changed()).await;
        if !self.is_exited() {
            gtrace(&format!("sid={} SIGNAL SIGKILL pid={pid}", self.sid));
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Drop our end of the input channel *first*: the writer thread blocks on `recv`, which only
        // returns once every sender is gone. Kill then makes the reader thread exit and drop its
        // clone, disconnecting the channel so the writer thread ends — otherwise the join below
        // would wait on a thread whose channel this Session was still keeping open.
        self.input_tx = None;
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

    /// A spawned child must NOT inherit Claude Code's child-session marker from amux's own launch
    /// environment — that marker puts an agent into the degraded child mode ("Transcript saving is
    /// off") seen on the blank panes. Set the marker in this process, spawn a shell through the
    /// real spawn path, and require the child to see it empty.
    #[test]
    fn spawn_scrubs_the_inherited_child_session_marker() {
        // SAFETY: single-threaded test process; we set then read our own env for the child to
        // inherit, and the value is inconsequential.
        unsafe { std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1") };
        let dir = std::env::temp_dir();
        let session = Session::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'MARK=[%s]\\n' \"$CLAUDE_CODE_CHILD_SESSION\"; sleep 0.2".to_string(),
            ],
            &dir,
            &[],
            Size { cols: 80, rows: 24 },
        )
        .expect("spawn");
        let mut rx = session.subscribe();
        std::thread::sleep(std::time::Duration::from_millis(400));
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            out.extend_from_slice(&b);
        }
        unsafe { std::env::remove_var("CLAUDE_CODE_CHILD_SESSION") };
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("MARK=[]"),
            "child inherited the scrubbed marker, got: {text:?}"
        );
    }

    /// A screen an app has overwritten with styled spaces is blank. The trace once counted such
    /// rows as content (`nb=49` for a glyph-less grid), so the chunk that blanked a pane was never
    /// logged and the failure was diagnosed from a client tap instead — when one happened to be on.
    #[test]
    fn rows_of_styled_spaces_count_as_blank() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"\x1b[48;2;38;38;38m                    \r\n                    \r\n");
        assert_eq!(
            nonblank_rows(p.screen()),
            0,
            "styled spaces are not content"
        );
        p.process(b"\x1b[3;1Hhello");
        assert_eq!(nonblank_rows(p.screen()), 1);
        // And the evidence line shows the escapes readably, bounded.
        let head = chunk_head(b"\x1b[2J\x1b[H  text");
        assert_eq!(head, "\\e[2J\\e[H  text");
        assert!(chunk_head(&[b'a'; 600]).ends_with("…(+88 bytes)"));
    }

    /// The grid trace is always on, so it must be bounded: past the cap the file rotates to `.1`
    /// (one generation kept) and appending continues in a fresh file.
    #[test]
    fn grid_trace_rotates_past_its_cap() {
        let dir = std::env::temp_dir().join(format!(
            "amux-trace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grid-trace.log");
        let cap = 64;
        append_bounded(&path, "first line, well under the cap\n", cap).unwrap();
        append_bounded(&path, "second line, pushing the file past 64 bytes\n", cap).unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() > cap,
            "precondition: over the cap"
        );

        append_bounded(&path, "third line\n", cap).unwrap();
        let rotated = std::fs::read_to_string(dir.join("grid-trace.log.1")).unwrap();
        assert!(
            rotated.contains("first line") && rotated.contains("second line"),
            "the previous generation is kept whole: {rotated:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "third line\n",
            "appending continues in a fresh file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

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

    /// A reattaching client rebuilds its parser from `snapshot()` and is then fed the identical live
    /// stream, so the snapshot must restore **every** bit of vt100's state — otherwise the two
    /// parsers diverge and the app (a diff renderer like Claude Code) never repaints to fix it. That
    /// is the whole cause of the blank/condensed reattach bugs.
    ///
    /// This drives a parser through every piece of state vt100 models, snapshots, rebuilds a fresh
    /// parser from the snapshot alone, and asserts they match. It is also the guard on completeness:
    /// vt100's state surface is small and closed (cells, cursor, scroll region, origin mode, the five
    /// mode bits, mouse mode + encoding — it models no charset or tab stops), so if vt100 is ever
    /// bumped and grows a new stateful mode, this test is where the omission must show up.
    #[test]
    fn snapshot_round_trips_all_vt100_state() {
        let mut daemon = vt100::Parser::new(24, 80, 0);
        // Exercise every stateful knob: alt screen, all the input modes, mouse tracking + SGR
        // encoding, a scroll region, origin mode, a non-home cursor, and some styled cells.
        daemon.process(b"\x1b[?1049h"); // alternate screen
        daemon.process(b"\x1b[?1h"); // DECCKM application cursor
        daemon.process(b"\x1b="); // application keypad
        daemon.process(b"\x1b[?2004h"); // bracketed paste
        daemon.process(b"\x1b[?1003h\x1b[?1006h"); // mouse any-motion + SGR encoding
        daemon.process(b"\x1b[3;20r"); // scroll region rows 3..=20
        daemon.process(b"\x1b[?6h"); // origin mode
        daemon.process(b"\x1b[5;10H\x1b[1;31mRED\x1b[0m plain"); // styled cells + cursor

        let session_screen = |p: &vt100::Parser| {
            let s = p.screen();
            (
                s.contents(),
                s.contents_formatted(),
                s.cursor_position(),
                s.alternate_screen(),
                s.application_cursor(),
                s.application_keypad(),
                s.bracketed_paste(),
                s.hide_cursor(),
                s.mouse_protocol_mode(),
                s.mouse_protocol_encoding(),
                s.scroll_region(),
                s.origin_mode(),
            )
        };

        // The real production builder, not a copy — so the test cannot drift from `snapshot()`.
        let snap = snapshot_bytes(daemon.screen());

        let mut client = vt100::Parser::new(24, 80, 0);
        client.process(&snap);
        assert_eq!(
            session_screen(&client),
            session_screen(&daemon),
            "the snapshot did not round-trip all vt100 state"
        );
    }

    /// Design-premise check for the server-authoritative question: the TUI client is NOT an
    /// independent re-derivation that can drift from the daemon — it is a faithful mirror, because it
    /// is built from the daemon's own `snapshot_bytes` and then fed the daemon's own byte stream, and
    /// re-synced from a fresh snapshot on every resize (exactly what `server::attach`/the `Resize`
    /// handler do). This test walks that lifecycle — attach, live stream, resize+re-snapshot, more
    /// stream — and asserts the client's visible screen equals the daemon's at every seam. If it
    /// holds, shipping rendered cells instead of bytes cannot change the picture the user sees: any
    /// blank/condensed screen is the *daemon grid itself*, and the fix belongs in the grid/resize
    /// model, not the render transport.
    #[test]
    fn client_reconstruction_mirrors_the_daemon_across_a_resize() {
        let visible = |p: &vt100::Parser| {
            let s = p.screen();
            (s.contents(), s.cursor_position(), s.alternate_screen())
        };
        // Feed both a daemon parser and a client parser built from the daemon's snapshot the *same*
        // subsequent bytes — the live-stream model (`DaemonMsg::Output` is the raw PTY stream both
        // sides see).
        let mut daemon = vt100::Parser::new(24, 80, 0);
        daemon.process(b"\x1b[2J\x1b[H");
        for i in 0..30 {
            daemon.process(format!("line {i}\r\n").as_bytes());
        }
        let mut client = vt100::Parser::new(24, 80, 0);
        client.process(&snapshot_bytes(daemon.screen()));
        assert_eq!(visible(&client), visible(&daemon), "after attach snapshot");

        let live = b"\x1b[10;1Hedited a line\x1b[38;2;0;200;0mgreen\x1b[0m";
        daemon.process(live);
        client.process(live);
        assert_eq!(visible(&client), visible(&daemon), "after a live chunk");

        // A resize on the daemon (its vendored keep-bottom `set_size`), then the daemon re-snapshots
        // and the client rebuilds from it — the real reattach/resize path.
        daemon.screen_mut().set_size(10, 40);
        let mut client = vt100::Parser::new(10, 40, 0);
        client.process(&snapshot_bytes(daemon.screen()));
        assert_eq!(
            visible(&client),
            visible(&daemon),
            "after a resize + fresh snapshot — the client mirrors even the shrunk grid"
        );

        let more = b"\x1b[1;1Hafter resize";
        daemon.process(more);
        client.process(more);
        assert_eq!(
            visible(&client),
            visible(&daemon),
            "and stays in lockstep on the post-resize stream"
        );
    }

    /// The freeze regression: a child that stops reading its stdin must not wedge the pane. Writes
    /// go through a dedicated writer thread, so `write_input` only ever enqueues — it must return
    /// promptly even when the child never drains, where the old design (a blocking write on the
    /// reader thread, under the writer lock) froze output *and* input together.
    #[test]
    fn write_input_never_blocks_on_a_child_that_stops_reading() {
        // `exec sleep` replaces the shell with a process that never reads stdin.
        let session = Session::spawn(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf READY; exec sleep 100".to_string(),
            ],
            Path::new("/"),
            &[],
            Size { rows: 6, cols: 40 },
        )
        .expect("spawn");
        for _ in 0..100 {
            if session
                .parser
                .lock()
                .map(|p| p.screen().contents().contains("READY"))
                .unwrap_or(false)
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        // Hammer far more than any PTY input buffer could hold. If `write_input` blocked on the
        // non-reading child, this thread would never signal and the test would time out.
        let s2 = Arc::clone(&session);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for _ in 0..10_000 {
                let _ = s2.write_input(&[b'x'; 128]);
            }
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "write_input blocked on a child that stopped reading — the frozen pane"
        );
        session.kill();
    }

    /// `resize` sets the grid size exactly — grow or shrink — and reports whether it changed, so a
    /// same-size reattach (the common reconnect) is a no-op with no SIGWINCH. The grid tracks the
    /// attached client's pane; a differently sized reconnect resizes it (see `server::attach`).
    #[test]
    fn resize_is_exact() {
        let session = session_with_lines(10, 8);
        assert_eq!(session.size(), Size { rows: 10, cols: 40 });

        // A grow.
        assert!(session.resize(Size { rows: 12, cols: 80 }).unwrap());
        assert_eq!(session.size(), Size { rows: 12, cols: 80 });

        // A genuine shrink takes effect exactly (the app's problem to repaint, not ours to refuse).
        assert!(session.resize(Size { rows: 6, cols: 20 }).unwrap());
        assert_eq!(session.size(), Size { rows: 6, cols: 20 });

        // Same size is a no-op (no SIGWINCH, no needless re-snapshot).
        assert!(!session.resize(Size { rows: 6, cols: 20 }).unwrap());
    }

    /// Shrinking the grid must keep the **bottom** rows — the cursor line and most recent output —
    /// not the top. This is the blank-pane bug: a pane previously large (as the main pane) is
    /// re-attached small (a split slot), the daemon resizes its grid down, and the reattach
    /// snapshot is served immediately. Upstream vt100 `set_size` truncated the bottom rows and kept
    /// the top (a full-screen app's blank header), so the snapshot was empty and an idle
    /// diff-renderer never repainted — a permanent blank. Our vendored `set_size` drops from the
    /// top instead (see `vendor/vt100/src/grid.rs`), so recent content survives the shrink.
    #[test]
    fn shrinking_the_grid_keeps_the_most_recent_rows() {
        // 25 lines on a 30-row grid: lines 0..=24 occupy rows 0..=24, cursor just below.
        let session = session_with_lines(30, 25);
        {
            let parser = session.parser.lock().unwrap();
            let c = parser.screen().contents();
            assert!(
                c.contains("line 0") && c.contains("line 24"),
                "precondition"
            );
        }

        // Shrink to 10 rows. A real terminal keeps the last rows; the oldest scroll off the top.
        assert!(session.resize(Size { rows: 10, cols: 40 }).unwrap());

        let parser = session.parser.lock().unwrap();
        let c = parser.screen().contents();
        assert!(
            c.contains("line 24"),
            "most recent output must survive the shrink, got:\n{c}"
        );
        assert!(
            !c.contains("line 0"),
            "the oldest rows scroll off the top, not the recent ones, got:\n{c}"
        );
    }

    /// Every line a parser holds, oldest first: scrollback, then the visible screen.
    fn history_and_screen(p: &mut vt100::Parser<Queries>) -> Vec<String> {
        let (rows, cols) = p.screen().size();
        p.screen_mut().set_scrollback(usize::MAX);
        let mut back = p.screen().scrollback();
        let mut out = Vec::new();
        while back > 0 {
            // At offset `back` the window's first rows are history lines `depth-back..`.
            p.screen_mut().set_scrollback(back);
            let take = back.min(usize::from(rows));
            out.extend(
                p.screen()
                    .rows(0, cols)
                    .take(take)
                    .map(|r| r.trim_end().to_string()),
            );
            back -= take;
        }
        p.screen_mut().set_scrollback(0);
        out.extend(p.screen().rows(0, cols).map(|r| r.trim_end().to_string()));
        out
    }

    /// Growing the grid must pull back the rows a shrink pushed into scrollback, as tmux does —
    /// the duplicated-history bug. Upstream `set_size` appended blank rows on a grow, so the pushed
    /// rows stayed in history while the app (which never saw them leave the screen) repainted them:
    /// Claude in a mini pane cycling 20 -> 3 -> 20 rows gained one full copy of its screen in
    /// scrollback per cycle. tmux 3.4, same sequence on a 10-row pane: the screen comes back
    /// `L07..BOTTOM` and history goes 6 -> 13 -> 6. This asserts exactly that.
    #[test]
    fn growing_the_grid_restores_rows_from_scrollback() {
        let mut p = vt100::Parser::new_with_callbacks(10, 40, SCROLLBACK, Queries::default());
        let out: String = (1..=15).map(|i| format!("L{i:02}\r\n")).collect();
        p.process(out.as_bytes());
        p.process(b"BOTTOM");
        let before = history_and_screen(&mut p);

        p.screen_mut().set_size(3, 40);
        p.screen_mut().set_size(10, 40);

        let screen: Vec<String> = p
            .screen()
            .rows(0, 40)
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(
            screen[0],
            "L07",
            "rows must come back from history, got:\n{}",
            screen.join("\n")
        );
        assert_eq!(screen[9], "BOTTOM");
        assert_eq!(
            p.screen().cursor_position(),
            (9, 6),
            "the cursor rides its row back down"
        );
        assert_eq!(
            history_and_screen(&mut p),
            before,
            "a shrink/grow round trip changes nothing"
        );
    }

    /// With less history than the grow needs, what history there is comes back and the rest is
    /// blank rows at the bottom — and nothing is pulled from an empty scrollback.
    #[test]
    fn growing_past_available_history_pads_with_blank_rows() {
        let mut p = vt100::Parser::new_with_callbacks(3, 40, SCROLLBACK, Queries::default());
        p.process(b"A\r\nB\r\nC\r\nD"); // A scrolls off: one row of history
        p.screen_mut().set_size(6, 40);
        let screen: Vec<String> = p
            .screen()
            .rows(0, 40)
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(screen, ["A", "B", "C", "D", "", ""]);
        assert_eq!(p.screen().cursor_position(), (3, 1));
        p.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            p.screen().scrollback(),
            0,
            "history is now on screen, not in both places"
        );
    }
}

/// Minimal repros for snapshot-fidelity gaps. The client rebuilds its parser from `snapshot_bytes`
/// and then applies the daemon's raw byte stream, so any parser state the snapshot fails to carry
/// is a *permanent* divergence between what the daemon holds and what the user sees — which is what
/// "condensed / missing newlines / garbled" looks like. Each test drives a daemon parser and a
/// client rebuilt from its snapshot through the same subsequent bytes and demands identical screens.
#[cfg(test)]
mod snapshot_fidelity_repros {
    use super::*;

    fn visible(p: &vt100::Parser) -> (String, (u16, u16)) {
        (p.screen().contents(), p.screen().cursor_position())
    }

    /// Writing exactly `cols` chars parks the cursor in vt100's pending-wrap state (`col == cols`).
    /// The next char must wrap to the next row. If the snapshot clamps that cursor into the screen,
    /// the client's next char lands on the last column instead — every char after is off by one and
    /// wraps a column early: the text "condenses".
    #[test]
    fn pending_wrap_cursor_round_trips() {
        let mut daemon = vt100::Parser::new(5, 10, 0);
        daemon.process(b"0123456789"); // exactly 10 cols → pending wrap
        let mut client = vt100::Parser::new(5, 10, 0);
        client.process(&snapshot_bytes(daemon.screen()));

        daemon.process(b"X");
        client.process(b"X");
        assert_eq!(
            visible(&client),
            visible(&daemon),
            "a pending-wrap cursor must survive the snapshot"
        );
    }

    /// The bug, at the parser level: if the daemon parser is fed a raw chunk ending inside a CSI, a
    /// snapshot omits the buffered partial and a client rebuilt from it parses the tail as text. This
    /// is *why* the reader must carry a trailing partial — kept as a guard that the hazard is real.
    #[test]
    fn feeding_the_parser_a_raw_partial_escape_diverges() {
        let mut daemon = vt100::Parser::new(5, 20, 0);
        daemon.process(b"hello");
        daemon.process(b"\x1b["); // a raw read boundary inside a CSI (what the reader must NOT do)
        let mut client = vt100::Parser::new(5, 20, 0);
        client.process(&snapshot_bytes(daemon.screen()));
        daemon.process(b"2Jworld");
        client.process(b"2Jworld");
        assert_ne!(
            visible(&client),
            visible(&daemon),
            "sanity: a raw partial does diverge, so the reader's carry-over is load-bearing"
        );
    }

    /// Mimic the reader: append `raw`, hand the parser only the escape-complete prefix, hold the
    /// rest. Returns the bytes broadcast to clients (identical to what the parser processed).
    fn feed(parser: &mut vt100::Parser, carry: &mut Vec<u8>, raw: &[u8]) -> Vec<u8> {
        carry.extend_from_slice(raw);
        let keep = complete_prefix_len(carry);
        let emitted = carry[..keep].to_vec();
        *carry = carry[keep..].to_vec();
        parser.process(&emitted);
        emitted
    }

    /// With the reader carrying the trailing partial, a snapshot taken at a read boundary that falls
    /// mid-escape no longer diverges: the parser is escape-clean at snapshot time, and the client is
    /// fed the same escape-aligned broadcast stream.
    #[test]
    fn snapshot_at_a_mid_escape_read_boundary_round_trips() {
        let mut daemon = vt100::Parser::new(5, 20, 0);
        let mut carry = Vec::new();
        feed(&mut daemon, &mut carry, b"hello\x1b["); // read ends inside a CSI; "ESC[" is carried
        assert_eq!(
            carry, b"\x1b[",
            "the partial CSI is held, not fed to the parser"
        );

        // A client attaches here — snapshot is taken between reads, parser is escape-clean.
        let mut client = vt100::Parser::new(5, 20, 0);
        client.process(&snapshot_bytes(daemon.screen()));

        // The next read completes the CSI; the client sees the same escape-aligned broadcast.
        let broadcast = feed(&mut daemon, &mut carry, b"2Jworld");
        client.process(&broadcast);
        assert_eq!(
            visible(&client),
            visible(&daemon),
            "carrying the partial keeps the client in lockstep across a mid-escape snapshot"
        );
    }

    /// Claude Code is dense with wide glyphs (emoji, CJK, powerline). Each is 2 columns, and one
    /// that would overflow the last column wraps whole. The snapshot must round-trip them — cells,
    /// widths, and the wrapped cursor — or a reattach shifts everything after a wide glyph.
    #[test]
    fn wide_chars_round_trip_including_a_wrap_at_the_margin() {
        let mut daemon = vt100::Parser::new(3, 8, 0);
        // 3 wide CJK (6 cols) + one more that cannot fit in the last 2 of an 8-wide row after it
        // — force a margin wrap by starting near the edge.
        daemon.process("ab日本語".as_bytes()); // a b (2 cols) + 日本語 (6 cols) = exactly 8 → wrap-pending
        daemon.process("×".as_bytes()); // multi-byte but 1-col; lands on the next row
        let mut client = vt100::Parser::new(3, 8, 0);
        client.process(&snapshot_bytes(daemon.screen()));
        assert_eq!(
            visible(&client),
            visible(&daemon),
            "wide glyphs and the margin wrap must survive the snapshot"
        );
    }

    /// A reattach rebuilds the client parser from the snapshot with a *fresh* OSC 8 link callback,
    /// so hyperlinks the app declared before the snapshot are not carried — a documented cost, not a
    /// garble. This pins that the *cells* are still correct (only the clickable-link overlay is lost
    /// until the app repaints), so it never manifests as shifted/'garbled text.
    #[test]
    fn osc8_links_do_not_corrupt_cells_across_a_snapshot() {
        let mut daemon = vt100::Parser::new(3, 40, 0);
        daemon.process(b"see \x1b]8;;https://example.com/x\x1b\\example\x1b]8;;\x1b\\ ok");
        let mut client = vt100::Parser::new(3, 40, 0);
        client.process(&snapshot_bytes(daemon.screen()));
        assert_eq!(
            client.screen().contents(),
            daemon.screen().contents(),
            "the visible text of a hyperlink round-trips even if the link overlay does not"
        );
        assert!(client.screen().contents().contains("see example ok"));
    }

    #[test]
    fn complete_prefix_len_holds_only_trailing_partials() {
        // Complete inputs are handed over whole.
        assert_eq!(complete_prefix_len(b"plain text"), 10);
        assert_eq!(
            complete_prefix_len(b"a\x1b[2Jb"),
            6,
            "a complete CSI mid-buffer"
        );
        assert_eq!(
            complete_prefix_len(b"x\x1b]0;title\x07"),
            11,
            "OSC ended by BEL"
        );
        assert_eq!(
            complete_prefix_len(b"x\x1b]0;t\x1b\\"),
            8,
            "OSC ended by ST"
        );
        assert_eq!(
            complete_prefix_len(b"x\x1bP1;2\x1b\\"),
            8,
            "DCS ended by ST"
        );
        assert_eq!(
            complete_prefix_len(b"x\x1b(B"),
            4,
            "3-byte charset designation"
        );
        assert_eq!(complete_prefix_len(b"x\x1b7"), 3, "2-byte escape");
        // Trailing partials are held from the last ESC.
        assert_eq!(complete_prefix_len(b"hi\x1b"), 2, "a lone trailing ESC");
        assert_eq!(complete_prefix_len(b"hi\x1b[2"), 2, "an unfinished CSI");
        assert_eq!(
            complete_prefix_len(b"hi\x1b]0;ti"),
            2,
            "an unterminated OSC"
        );
        assert_eq!(
            complete_prefix_len(b"hi\x1b("),
            2,
            "an unfinished charset designation"
        );
        // A completed sequence with plain text after it is all safe.
        assert_eq!(complete_prefix_len(b"\x1b[2Jmore"), 8);
    }

    /// Rows that wrapped at one width carry a `wrapped` flag `contents_formatted` honours; a width
    /// change must not let that flag re-join lines in the client that are separate on the daemon.
    #[test]
    fn wrapped_rows_survive_a_width_change() {
        let mut daemon = vt100::Parser::new(5, 10, 0);
        daemon.process(b"abcdefghijKLMNO\r\nnext"); // 15 chars wrap onto 2 rows, then a new line
        daemon.screen_mut().set_size(5, 20); // widen: the wrapped pair now fits one row
        let mut client = vt100::Parser::new(5, 20, 0);
        client.process(&snapshot_bytes(daemon.screen()));
        assert_eq!(
            visible(&client),
            visible(&daemon),
            "wrapped rows must not re-join in the client after a width change"
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
            // DA2 (`ESC[>c` / `ESC[>0c`): a conservative secondary identity, so a CLI that blocks
            // on it does not wedge and nothing gates output on a version we lack.
            ((Some(b'>'), &[], 'c'), b"\x1b[>0;0;0c"),
            ((Some(b'>'), &[&[0]], 'c'), b"\x1b[>0;0;0c"),
            // XTVERSION (`ESC[>q` / `ESC[>0q`, Claude Code sends it): the DCS name string.
            ((Some(b'>'), &[], 'q'), b"\x1bP>|amux\x1b\\"),
            ((Some(b'>'), &[&[0]], 'q'), b"\x1bP>|amux\x1b\\"),
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
            (Some(b'>'), &[&[1]], 'c'), // DA2 with an unrecognized param
            (Some(b'?'), &[], 'u'),     // kitty keyboard query — answering claims a protocol we
            // don't encode, so silence is the correct "unsupported" signal (the app falls back).
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

/// Manual experiment (needs a `claude` binary and a login; creates a throwaway project entry):
/// does Claude Code's full-screen UI repaint after the pty is resized? It decides the sizing
/// model in `docs/DESIGN.md` §5.3 — the pty is the pane — so it stays here to be re-run when
/// that is doubted. Run with
/// `cargo test -p amux-daemon --lib -- manual_claude_repaints_after_resizes --ignored --nocapture`
/// and read the dumps: after each resize the header must be on rows 1–3 and the prompt box on the
/// last rows of the *new* size. Measured 2026-09-14 (Claude Code 2.1.270): repaints within ~750ms
/// after a single shrink (with one `2J` clear), after a grow, and after a five-step resize storm.
#[cfg(test)]
mod resize_experiment {
    use super::*;
    fn census(
        bytes: &[u8],
        rows: u16,
        cols: u16,
    ) -> (usize, Option<u16>, Option<u16>, bool, (u16, u16)) {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(bytes);
        let sc = p.screen();
        let nb: Vec<u16> = (0..rows)
            .filter(|&y| {
                (0..cols).any(|x| {
                    sc.cell(y, x)
                        .map(|c| !c.contents().is_empty())
                        .unwrap_or(false)
                })
            })
            .collect();
        let has_prompt = (0..rows).any(|y| {
            (0..cols).any(|x| {
                sc.cell(y, x)
                    .map(|c| c.contents() == "\u{276f}")
                    .unwrap_or(false)
            })
        });
        (
            nb.len(),
            nb.first().copied(),
            nb.last().copied(),
            has_prompt,
            sc.cursor_position(),
        )
    }
    fn dump(label: &str, bytes: &[u8], rows: u16, cols: u16) {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(bytes);
        let sc = p.screen();
        eprintln!(
            "---- {label} alt={} cursor={:?} ----",
            sc.alternate_screen(),
            sc.cursor_position()
        );
        for y in 0..rows {
            let mut t = String::new();
            for x in 0..cols {
                if let Some(c) = sc.cell(y, x) {
                    let g = c.contents();
                    t.push_str(if g.is_empty() { " " } else { g });
                }
            }
            let t = t.trim_end();
            if !t.is_empty() {
                eprintln!("{y:>2}|{}", t.chars().take(100).collect::<String>());
            }
        }
    }
    /// Reproduce the divergence the user hits: after a rapid resize storm (a fast divider drag),
    /// does the daemon grid still match what Claude is showing? We can't read Claude's own screen,
    /// but a genuine resize to a fresh size forces its full repaint — so compare the grid right
    /// after the storm against the grid after a forced repaint. If the storm left the grid with far
    /// less content, amux dropped content Claude still believes is there, and (diff-only) Claude
    /// never restores it: the permanent blank.
    ///
    /// Result 2026-09-17, Claude Code 2.1.276: a wild storm ([31,47,22,53,40] rows ~15ms apart)
    /// did NOT diverge — Claude full-repaints after the SIGWINCH burst, content identical
    /// (baseline_nb == after_storm_nb == forced_nb). So resize storms are not the trigger; the
    /// real blank is captured by the reader's ring dump (`AMUX_GRID_TRACE` → `blanked-*.bin`) and
    /// replayed with `manual_replay_blank_capture`.
    /// Reproduce the user's actual trigger: reconnecting an SSH session at a DIFFERENT terminal
    /// size. On reconnect a fresh client attaches at its own size; if that differs from the grid the
    /// daemon `resize`s, and keep-bottom `set_size` drops transcript rows. Does Claude, on that one
    /// SIGWINCH, redraw the whole transcript — or only its input box, leaving the dropped rows
    /// permanently blank? Build a TALL transcript first (the throwaway UIs that "recovered" in the
    /// storm test had almost none), then shrink once (reconnect smaller), settle, grow back.
    /// Does a resize racing the app's own output (a reconnect resize landing while Claude is mid
    /// input-echo or repaint) diverge the grid? Tightly interleave Input and Resize, then type a
    /// known marker with NO trailing resize, settle, and check the marker actually appears in the
    /// grid. If it does not, amux's grid has diverged from Claude — the live bug, reproduced.
    #[test]
    #[ignore]
    fn manual_resize_racing_input_does_not_diverge() {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::spawn(
            &["claude".to_string()],
            dir.path(),
            &[],
            Size {
                cols: 120,
                rows: 40,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        while rx.try_recv().is_ok() {}
        s.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
        while rx.try_recv().is_ok() {}

        // Race: type a chunk, immediately resize, many times, no settle between.
        for i in 0..12 {
            s.write_input(format!("abc{i} ").as_bytes()).unwrap();
            let r = if i % 2 == 0 { 34 } else { 40 };
            s.resize(Size { cols: 120, rows: r }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        s.resize(Size {
            cols: 120,
            rows: 40,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        while rx.try_recv().is_ok() {}
        dump("after resize/input race", &s.snapshot(), 40, 120);

        // Now type a clean marker with NO resize, settle, and require it to be present.
        s.write_input(b" UNIQUEMARKER99").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        while rx.try_recv().is_ok() {}
        let snap = s.snapshot();
        let mut p = vt100::Parser::new(40, 120, 0);
        p.process(&snap);
        let sc = p.screen();
        let mut found = false;
        let mut whole = String::new();
        for y in 0..40 {
            for x in 0..120 {
                if let Some(c) = sc.cell(y, x) {
                    let t = c.contents();
                    whole.push_str(if t.is_empty() { " " } else { t });
                }
            }
            whole.push('\n');
        }
        if whole.contains("UNIQUEMARKER99") {
            found = true;
        }
        dump("after clean marker", &snap, 40, 120);
        eprintln!("VERDICT marker_present={found} (grid diverged if false)");
    }

    /// Result 2026-09-21, Claude 2.1.276: does NOT wedge — full recovery (content intact across
    /// the storm and the recovery resize). Resize is not the trigger.
    ///
    /// The real reconnect pattern (from grid-trace): a rapid RESIZE STORM through many sizes
    /// including a pathological tiny height (a mini at 3 rows), with no settle, then back up. tmux
    /// sends ONE resize on attach; amux sends this burst. Does it wedge ink into rendering empty
    /// and not recovering — the blank that a later resize can't repaint? Reproduce with the actual
    /// shape, faster and nastier than the earlier (too-gentle) storm test.
    /// Result 2026-09-21: child mode renders the SAME full UI as normal, idle and after 20s. The
    /// child-session marker is a red herring for the blank (the env scrub is hygiene, not the fix).
    ///
    /// Does Claude Code in child-session mode (CLAUDE_CODE_CHILD_SESSION=1) render its idle UI
    /// differently from a normal session? If child mode renders empty/degraded when idle, it is the
    /// blank; if it renders the same full UI, the child marker is a red herring for the blank.
    /// Does CLAUDE_CODE_DEBUG_LOGS_DIR make Claude write debug logs WITHOUT disturbing the pane
    /// (so amux can turn it on for agents and read the next wedge's cause from Claude's own logs)?
    /// End-to-end: a Claude launched with `tui: default` (amux's fix) renders content in the
    /// classic renderer — confirming the fix does not itself produce a blank.
    #[test]
    #[ignore]
    fn manual_classic_renderer_renders_content() {
        let dir = tempfile::tempdir().unwrap();
        let sf = dir.path().join("amux-settings.json");
        std::fs::write(&sf, br#"{"tui":"default"}"#).unwrap();
        let s = Session::spawn(
            &[
                "claude".to_string(),
                "--settings".to_string(),
                sf.to_string_lossy().into_owned(),
            ],
            dir.path(),
            &[],
            Size {
                cols: 120,
                rows: 40,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        while rx.try_recv().is_ok() {}
        s.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
        while rx.try_recv().is_ok() {}
        let c = census(&s.snapshot(), 40, 120);
        eprintln!("[classic] census={c:?}");
        dump("classic renderer", &s.snapshot(), 40, 120);
        assert!(
            c.0 >= 3,
            "classic renderer should render content, got {c:?}"
        );
    }

    #[test]
    #[ignore]
    fn manual_claude_debug_logs_dir_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let s = Session::spawn(
            &["claude".to_string()],
            dir.path(),
            &[
                (
                    "CLAUDE_CODE_DEBUG_LOGS_DIR".to_string(),
                    logs.path().to_string_lossy().into_owned(),
                ),
                ("CLAUDE_CODE_DEBUG".to_string(), "1".to_string()),
            ],
            Size {
                cols: 120,
                rows: 40,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        while rx.try_recv().is_ok() {}
        s.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
        while rx.try_recv().is_ok() {}
        eprintln!(
            "[debug-logs] UI census={:?}",
            census(&s.snapshot(), 40, 120)
        );
        dump("debug-logs UI", &s.snapshot(), 40, 120);
        let mut files = Vec::new();
        fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        walk(logs.path(), &mut files);
        eprintln!("[debug-logs] {} log files written:", files.len());
        for f in files.iter().take(5) {
            eprintln!(
                "   {} ({} bytes)",
                f.display(),
                std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)
            );
        }
    }

    #[test]
    #[ignore]
    fn manual_child_session_idle_render() {
        for child in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if child {
                unsafe { std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1") };
            } else {
                unsafe { std::env::remove_var("CLAUDE_CODE_CHILD_SESSION") };
            }
            // Spawn WITHOUT the scrub by calling CommandBuilder directly? No — Session::spawn scrubs.
            // Temporarily bypass by spawning claude with the var re-set via a wrapper sh.
            let cmd = if child {
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "export CLAUDE_CODE_CHILD_SESSION=1; exec claude".to_string(),
                ]
            } else {
                vec!["claude".to_string()]
            };
            let s = Session::spawn(
                &cmd,
                dir.path(),
                &[],
                Size {
                    cols: 120,
                    rows: 40,
                },
            )
            .unwrap();
            let mut rx = s.subscribe();
            std::thread::sleep(std::time::Duration::from_secs(6));
            while rx.try_recv().is_ok() {}
            s.write_input(b"\x1b[B").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
            s.write_input(b"\r").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(10));
            while rx.try_recv().is_ok() {}
            eprintln!(
                "[child={child}] after-startup census={:?}",
                census(&s.snapshot(), 40, 120)
            );
            // Sit idle 20s and re-census — does an idle child session clear itself?
            std::thread::sleep(std::time::Duration::from_secs(20));
            while rx.try_recv().is_ok() {}
            eprintln!(
                "[child={child}] after-20s-idle census={:?}",
                census(&s.snapshot(), 40, 120)
            );
            dump(&format!("child={child} idle"), &s.snapshot(), 40, 120);
        }
        unsafe { std::env::remove_var("CLAUDE_CODE_CHILD_SESSION") };
    }

    #[test]
    #[ignore]
    fn manual_reconnect_resize_storm_with_tiny_size() {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::spawn(
            &["claude".to_string()],
            dir.path(),
            &[],
            Size {
                cols: 332,
                rows: 78,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        while rx.try_recv().is_ok() {}
        s.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
        while rx.try_recv().is_ok() {}
        // Type a marker so there is content to lose.
        s.write_input(b"hello from the storm test").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        while rx.try_recv().is_ok() {}
        eprintln!("[before storm] census={:?}", census(&s.snapshot(), 78, 332));

        // The actual reconnect sweep, tight (a few ms apart), including a 3-row height.
        for &(cols, rows) in &[
            (176u16, 44u16),
            (176, 3),
            (332, 7),
            (176, 3),
            (332, 65),
            (332, 57),
            (332, 43),
            (332, 28),
            (332, 43),
            (327, 43),
            (332, 78),
        ] {
            s.resize(Size { cols, rows }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(6));
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
        while rx.try_recv().is_ok() {}
        let after = census(&s.snapshot(), 78, 332);
        eprintln!("[after storm @332x78] census={after:?}");
        dump("after storm", &s.snapshot(), 78, 332);
        // Now a single clean resize (like a later user resize) — does it recover?
        s.resize(Size {
            cols: 332,
            rows: 77,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        while rx.try_recv().is_ok() {}
        let recov = census(&s.snapshot(), 77, 332);
        eprintln!("[after recovery resize @332x77] census={recov:?}");
        eprintln!("VERDICT before={} after_storm={} after_recover={} (WEDGED if after_storm/recover are ~0 with content before)", census(&s.snapshot(),78,332).0, after.0, recov.0);
    }

    #[test]
    #[ignore]
    fn manual_reconnect_at_smaller_size_keeps_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::spawn(
            &["claude".to_string()],
            dir.path(),
            &[],
            Size {
                cols: 120,
                rows: 45,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        while rx.try_recv().is_ok() {}
        s.write_input(b"\x1b[B").unwrap(); // trust: "Yes"
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
        while rx.try_recv().is_ok() {}
        // A tall transcript: ask for many lines of plain output, no tools.
        s.write_input(b"print the numbers 1 through 30, each on its own line, and nothing else")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(20)); // let it answer
        while rx.try_recv().is_ok() {}
        let before = census(&s.snapshot(), 45, 120);
        eprintln!("[transcript @45] census={before:?}");
        dump("transcript @45", &s.snapshot(), 45, 120);

        // Reconnect smaller: one client attaches at 30 rows. Daemon resizes; Claude gets one SIGWINCH.
        s.resize(Size {
            cols: 120,
            rows: 30,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        while rx.try_recv().is_ok() {}
        dump("after reconnect @30", &s.snapshot(), 30, 120);

        // Then the client settles at its real size (or you resize the window). Grow back to 45.
        s.resize(Size {
            cols: 120,
            rows: 45,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        while rx.try_recv().is_ok() {}
        let after = census(&s.snapshot(), 45, 120);
        eprintln!("[back @45] census={after:?}");
        dump("back @45", &s.snapshot(), 45, 120);
        eprintln!(
            "VERDICT before_nb={} after_nb={} (transcript lost if after << before)",
            before.0, after.0
        );
    }

    #[test]
    #[ignore]
    fn manual_storm_does_not_diverge_the_grid() {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::spawn(
            &["claude".to_string()],
            dir.path(),
            &[],
            Size {
                cols: 120,
                rows: 40,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        while rx.try_recv().is_ok() {}
        s.write_input(b"\x1b[B").unwrap(); // select "Yes, trust"
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
        while rx.try_recv().is_ok() {}
        // Type a few visible characters into the prompt so the content area is non-trivial.
        s.write_input(b"hello world this is a test line").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        while rx.try_recv().is_ok() {}
        let baseline = census(&s.snapshot(), 40, 120);
        eprintln!("[baseline @40] census={baseline:?}");
        dump("baseline @40", &s.snapshot(), 40, 120);

        // A fast wild storm: sizes a real drag sweeps through, ~15ms apart, no settle. The last
        // size is 40 again, so any content loss is amux's, not a pending repaint at a smaller size.
        for &r in &[31u16, 47, 22, 53, 40] {
            s.resize(Size { cols: 120, rows: r }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        // Give Claude a beat to answer the SIGWINCH burst however it will.
        std::thread::sleep(std::time::Duration::from_secs(3));
        while rx.try_recv().is_ok() {}
        let after_storm = census(&s.snapshot(), 40, 120);
        eprintln!("[after storm @40] census={after_storm:?}");
        dump("after storm @40", &s.snapshot(), 40, 120);

        // Force a guaranteed full repaint: resize to a size not seen in the storm, wait, come back.
        s.resize(Size {
            cols: 118,
            rows: 39,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        s.resize(Size {
            cols: 120,
            rows: 40,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        while rx.try_recv().is_ok() {}
        let forced = census(&s.snapshot(), 40, 120);
        eprintln!("[after forced repaint @40] census={forced:?}");
        dump("after forced repaint @40", &s.snapshot(), 40, 120);

        eprintln!(
            "VERDICT: baseline_nb={} after_storm_nb={} forced_nb={}",
            baseline.0, after_storm.0, forced.0
        );
    }

    /// Replay a `blanked-*.bin` capture (raw PTY output the daemon kept before a grid blanked) and
    /// find the byte offset where the grid went blank — the divergence, reproduced offline.
    /// `AMUX_BLANK_CAPTURE=<path> AMUX_BLANK_COLS=.. AMUX_BLANK_ROWS=..`, then read the dumps.
    #[test]
    #[ignore]
    fn manual_replay_blank_capture() {
        let path = std::env::var("AMUX_BLANK_CAPTURE").expect("set AMUX_BLANK_CAPTURE");
        let cols: u16 = std::env::var("AMUX_BLANK_COLS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(332);
        let rows: u16 = std::env::var("AMUX_BLANK_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(65);
        let bytes = std::fs::read(&path).unwrap();
        eprintln!("replaying {} bytes at {cols}x{rows}", bytes.len());
        // Feed in 2KB steps; report each step's glyph-row count and the escapes in a step that
        // drops it, so the blanking sequence is pinpointed.
        let mut p = vt100::Parser::new(rows, cols, 0);
        let glyph_rows = |sc: &vt100::Screen| {
            (0..rows)
                .filter(|&y| {
                    (0..cols).any(|x| {
                        sc.cell(y, x)
                            .map(|c| !c.contents().trim().is_empty())
                            .unwrap_or(false)
                    })
                })
                .count()
        };
        let mut prev = 0usize;
        let step = 2048;
        for (i, ch) in bytes.chunks(step).enumerate() {
            p.process(ch);
            let nb = glyph_rows(p.screen());
            if prev >= 4 && nb <= 1 {
                eprintln!(
                    "BLANKED at step {i} (byte ~{}): {}",
                    i * step,
                    chunk_head(ch)
                );
            }
            prev = nb;
        }
        dump("final replayed grid", &bytes, rows, cols);
        eprintln!("final glyph_rows={}", glyph_rows(p.screen()));
    }

    /// End-to-end guard for the duplicated-history bug: a real Claude (classic renderer, as amux
    /// runs agents) in a pane cycling 20 -> 3 -> 20 rows — the mini-pane pattern grid-trace shows.
    /// Before the vendored grow fix every cycle appended a full copy of the screen to scrollback
    /// (`ROW025` x6 after 5 cycles); tmux 3.4 under the same Claude and resizes keeps one copy.
    #[test]
    #[ignore]
    fn manual_resize_cycles_do_not_duplicate_history() {
        let dir = tempfile::tempdir().unwrap();
        let sf = dir.path().join("amux-settings.json");
        std::fs::write(&sf, br#"{"tui":"default"}"#).unwrap();
        let s = Session::spawn(
            &[
                "claude".to_string(),
                "--settings".to_string(),
                sf.to_string_lossy().into_owned(),
            ],
            dir.path(),
            &[],
            Size {
                cols: 100,
                rows: 20,
            },
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(6));
        s.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(6));
        // Bash mode puts known lines in the transcript.
        s.write_input(b"!").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        s.write_input(b"seq -f ROW%03g 1 30").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(12));
        for _ in 0..5 {
            s.resize(Size { cols: 100, rows: 3 }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
            s.resize(Size {
                cols: 100,
                rows: 20,
            })
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let mut parser = s.parser.lock().unwrap();
        let (rows, cols) = parser.screen().size();
        parser.screen_mut().set_scrollback(usize::MAX);
        let mut back = parser.screen().scrollback();
        let mut lines = Vec::new();
        while back > 0 {
            parser.screen_mut().set_scrollback(back);
            let take = back.min(usize::from(rows));
            lines.extend(parser.screen().rows(0, cols).take(take));
            back -= take;
        }
        parser.screen_mut().set_scrollback(0);
        lines.extend(parser.screen().rows(0, cols));
        let n = lines.iter().filter(|l| l.contains("ROW025")).count();
        assert_eq!(n, 1, "ROW025 appears {n} times:\n{}", lines.join("\n"));
    }

    #[test]
    #[ignore]
    fn manual_claude_repaints_after_resizes() {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::spawn(
            &["claude".to_string()],
            dir.path(),
            &[],
            Size {
                cols: 120,
                rows: 40,
            },
        )
        .unwrap();
        let mut rx = s.subscribe();
        std::thread::sleep(std::time::Duration::from_secs(6));
        let mut got = 0usize;
        while let Ok(b) = rx.try_recv() {
            got += b.len();
        }
        eprintln!(
            "[40 rows] bytes so far={got} census={:?}",
            census(&s.snapshot(), 40, 120)
        );
        // Accept the trust dialog; reach the main UI (alt screen, prompt pinned at the bottom).
        dump("dialog @40", &s.snapshot(), 40, 120);
        s.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        s.write_input(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(12));
        let mut got = Vec::new();
        while let Ok(b) = rx.try_recv() {
            got.extend(b);
        }
        eprintln!(
            "[after Enter +10s] bytes={} raw={:?}",
            got.len(),
            String::from_utf8_lossy(&got[..got.len().min(300)])
        );
        dump("main UI? @40", &s.snapshot(), 40, 120);
        // ONE shrink, no wobble.
        s.resize(Size {
            cols: 120,
            rows: 25,
        })
        .unwrap();
        eprintln!(
            "[shrink->25] immediately after set_size census={:?}",
            census(&s.snapshot(), 25, 120)
        );
        for i in 1..=4 {
            std::thread::sleep(std::time::Duration::from_millis(750));
            let mut got = 0usize;
            let mut clears = 0usize;
            while let Ok(b) = rx.try_recv() {
                got += b.len();
                clears += b.windows(4).filter(|w| *w == b"\x1b[2J").count();
            }
            eprintln!(
                "[shrink +{}ms] bytes={got} 2J={clears} census={:?}",
                i * 750,
                census(&s.snapshot(), 25, 120)
            );
        }
        dump("after shrink settle @25", &s.snapshot(), 25, 120);
        // A divider-drag storm: five resizes 20ms apart, then settle. (The client coalesces these to
        // one, so this is the worst case, not the normal one.)
        for rows in [28u16, 31, 34, 37, 33] {
            s.resize(Size { cols: 120, rows }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let mut got = 0usize;
        while let Ok(b) = rx.try_recv() {
            got += b.len();
        }
        eprintln!(
            "[storm ->33 +1500ms] bytes={got} census={:?}",
            census(&s.snapshot(), 33, 120)
        );
        dump("after storm settle @33", &s.snapshot(), 33, 120);
        // and grow back
        s.resize(Size {
            cols: 120,
            rows: 40,
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let mut got = 0usize;
        while let Ok(b) = rx.try_recv() {
            got += b.len();
        }
        eprintln!(
            "[grow->40 +1500ms] bytes={got} census={:?}",
            census(&s.snapshot(), 40, 120)
        );
        dump("after grow settle @40", &s.snapshot(), 40, 120);
        let _ = s.write_input(b"\x03\x03");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = s.write_input(b"/exit\r");
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
}

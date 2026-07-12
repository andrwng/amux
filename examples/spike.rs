//! Phase 0.1 de-risking spike — THROWAWAY reference, not shipped architecture.
//!
//! Proves the whole PTY↔render loop in one file: spawn `$SHELL` in a portable-pty, parse its
//! output with vt100, render it with tui-term inside a ratatui/crossterm frame, forward
//! keystrokes to the PTY, and handle resize. This is the riskiest plumbing in the project,
//! proven in isolation before we split it across the daemon/client boundary.
//!
//! Run it in a real terminal:  `cargo run --example spike`   (quit with Ctrl-Q)
//!
//! Load-bearing details (see docs/DESIGN.md §11):
//!
//! * the PTY reader is blocking → dedicated thread, bridged to the UI over a channel;
//! * drop the pty *slave* after spawn or the reader never sees the child exit;
//! * treat both `Ok(0)` (macOS EOF) and `Err` (Linux EIO) as "PTY closed".
//!
//! Known spike limitation: no DECCKM application-cursor-mode handling (arrows always send
//! `ESC [`) — the real input encoder must add it.

use std::io::{Read, Write};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::{Block, Borders};
use ratatui::DefaultTerminal;
use tui_term::widget::PseudoTerminal;

/// Messages from the blocking PTY reader thread to the UI loop.
enum PtyMsg {
    Data(Vec<u8>),
    Closed,
}

/// Interior size (rows, cols) given the outer terminal size, leaving a 1-cell border.
fn inner_size(cols: u16, rows: u16) -> (u16, u16) {
    (rows.saturating_sub(2).max(1), cols.saturating_sub(2).max(1))
}

fn main() -> Result<()> {
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let (pty_rows, pty_cols) = inner_size(cols, rows);

    // Spawn $SHELL in a PTY sized to the interior region.
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: pty_rows,
        cols: pty_cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new_default_prog();
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave); // child now owns the only slave fd → its exit is observable on the reader

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let mut parser = vt100::Parser::new(pty_rows, pty_cols, 1000);

    // Blocking reads on a dedicated thread; forward to the UI loop over a channel.
    let (tx, rx) = mpsc::channel::<PtyMsg>();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(PtyMsg::Closed); // EOF (macOS)
                    break;
                }
                Ok(n) => {
                    if tx.send(PtyMsg::Data(buf[..n].to_vec())).is_err() {
                        break; // UI gone
                    }
                }
                Err(_) => {
                    let _ = tx.send(PtyMsg::Closed); // EIO on Linux, etc.
                    break;
                }
            }
        }
    });

    let mut terminal = ratatui::init(); // raw mode + alt screen + panic hook
    let result = run(
        &mut terminal,
        &mut parser,
        &rx,
        writer.as_mut(),
        pair.master.as_ref(),
    );
    ratatui::restore();

    let _ = child.kill();
    drop(pair.master);
    result
}

fn run(
    terminal: &mut DefaultTerminal,
    parser: &mut vt100::Parser,
    rx: &mpsc::Receiver<PtyMsg>,
    writer: &mut (dyn Write + Send),
    master: &(dyn MasterPty + Send),
) -> Result<()> {
    loop {
        // 1) drain PTY output into the parser
        loop {
            match rx.try_recv() {
                Ok(PtyMsg::Data(bytes)) => parser.process(&bytes),
                Ok(PtyMsg::Closed) => return Ok(()),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // 2) draw the current screen
        terminal.draw(|frame| {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" amux spike · $SHELL · Ctrl-Q to quit ");
            let widget = PseudoTerminal::new(parser.screen()).block(block);
            frame.render_widget(widget, frame.area());
        })?;

        // 3) input (poll so the loop keeps draining PTY output between keystrokes)
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('q')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    if let Some(bytes) = key_to_bytes(key.code, key.modifiers) {
                        writer.write_all(&bytes)?;
                        writer.flush()?;
                    }
                }
                Event::Resize(cols, rows) => {
                    let (r, c) = inner_size(cols, rows);
                    parser.screen_mut().set_size(r, c);
                    let _ = master.resize(PtySize {
                        rows: r,
                        cols: c,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                _ => {}
            }
        }
    }
}

/// Minimal crossterm KeyEvent → PTY byte encoder (subset of tui-term's `smux.rs`).
/// The real encoder (Phase 0.5) must also handle DECCKM, Alt-prefixing, and F-keys.
fn key_to_bytes(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    use KeyCode::*;
    let bytes = match code {
        Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                let up = c.to_ascii_uppercase();
                match up {
                    '@'..='_' => vec![(up as u8) - 0x40], // Ctrl-A=1 … Ctrl-_=31
                    ' ' => vec![0],
                    _ => vec![c as u8],
                }
            } else {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b).as_bytes().to_vec()
            }
        }
        Enter => vec![b'\r'],
        Backspace => vec![0x7f],
        Tab => vec![b'\t'],
        BackTab => vec![0x1b, b'[', b'Z'],
        Esc => vec![0x1b],
        Left => vec![0x1b, b'[', b'D'],
        Right => vec![0x1b, b'[', b'C'],
        Up => vec![0x1b, b'[', b'A'],
        Down => vec![0x1b, b'[', b'B'],
        Home => vec![0x1b, b'[', b'H'],
        End => vec![0x1b, b'[', b'F'],
        PageUp => vec![0x1b, b'[', b'5', b'~'],
        PageDown => vec![0x1b, b'[', b'6', b'~'],
        Delete => vec![0x1b, b'[', b'3', b'~'],
        Insert => vec![0x1b, b'[', b'2', b'~'],
        _ => return None,
    };
    Some(bytes)
}

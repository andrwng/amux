//! A single PTY session: spawn a command, stream its output, feed its input, resize it.
//!
//! The reader is blocking, so it runs on a dedicated OS thread and forwards chunks to the
//! async world over an unbounded channel (see `docs/DESIGN.md` §11 gotcha 1). We drop the
//! pty *slave* after spawn so the child owns the only slave fd — otherwise the child never
//! sees EOF. The reader treats both `Ok(0)` (macOS EOF) and `Err` (Linux EIO) as "closed".

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

use amux_proto::Size;

/// Scrollback retained by the daemon-side parser (used for snapshots).
const SCROLLBACK: usize = 2000;

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    reader: Option<JoinHandle<()>>,
}

impl PtySession {
    /// Spawn `command` in a PTY sized to `size`. Returns the session and the receiver of raw
    /// output chunks (fed by the reader thread).
    pub fn spawn(
        command: &[String],
        size: Size,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Vec<u8>>)> {
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
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).context("spawn child")?;
        drop(pair.slave); // child now owns the only slave fd → its exit is observable

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            size.rows, size.cols, SCROLLBACK,
        )));

        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let reader_parser = Arc::clone(&parser);
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(chunk);
                        }
                        if tx.send(chunk.to_vec()).is_err() {
                            break; // the async side is gone
                        }
                    }
                    Err(_) => break, // EIO on Linux, etc.
                }
            }
        });

        Ok((
            Self {
                master: pair.master,
                writer,
                child,
                parser,
                reader: Some(reader),
            },
            rx,
        ))
    }

    /// The current screen as a `contents_formatted()` dump — replays to reproduce the screen.
    pub fn snapshot(&self) -> Vec<u8> {
        self.parser
            .lock()
            .map(|p| p.screen().contents_formatted())
            .unwrap_or_default()
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).context("write to pty")?;
        self.writer.flush().context("flush pty")?;
        Ok(())
    }

    pub fn resize(&mut self, size: Size) -> Result<()> {
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(size.rows, size.cols);
        }
        self.master
            .resize(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")?;
        Ok(())
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Reap the child and return its exit code, if available.
    pub fn wait(&mut self) -> Option<i32> {
        self.child
            .wait()
            .ok()
            .map(|status| status.exit_code() as i32)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Kill so the reader thread's blocking read unblocks (child closes the slave fd),
        // then join it so we never leak the thread.
        let _ = self.child.kill();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

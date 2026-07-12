//! The TUI event loop: connect to the daemon, render its PTY full-screen with a status bar,
//! forward keys and resizes, and tear down cleanly. This is the Phase 0.5 spine — a single
//! terminal beside a (future) sidebar. See `docs/DESIGN.md` §7.

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::{SinkExt, StreamExt};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::DefaultTerminal;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use tui_term::widget::PseudoTerminal;

use amux_proto::{ClientCodec, ClientMsg, DaemonMsg, Size};

use crate::client::{connect, ClientOptions};
use crate::input::key_to_bytes;

/// PTY dimensions for a terminal of `cols`×`rows`, reserving one row for the status bar.
fn pty_size(cols: u16, rows: u16) -> Size {
    Size {
        cols: cols.max(1),
        rows: rows.saturating_sub(1).max(1),
    }
}

pub async fn run() -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let opts = ClientOptions::resolve(pty_size(cols, rows))?;
    let framed = connect(&opts).await?;

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, framed, opts.size).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    framed: Framed<UnixStream, ClientCodec>,
    initial: Size,
) -> Result<()> {
    let mut parser = vt100::Parser::new(initial.rows, initial.cols, 2000);
    let (mut sink, mut stream) = framed.split();
    let mut events = EventStream::new();
    let mut status = String::from(" amux · Ctrl-Q to quit ");

    draw(terminal, &parser, &status)?;
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(DaemonMsg::OutputSnapshot(bytes))) | Some(Ok(DaemonMsg::Output(bytes))) => {
                    parser.process(&bytes);
                }
                Some(Ok(DaemonMsg::Exited { code })) => {
                    status = format!(" session exited ({code:?}) · press any key to close ");
                    draw(terminal, &parser, &status)?;
                    let _ = events.next().await;
                    break;
                }
                Some(Ok(DaemonMsg::Error(e))) => status = format!(" daemon error: {e} "),
                Some(Ok(DaemonMsg::Hello { .. })) => {}
                Some(Err(_)) | None => break,
            },
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('q')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if let Some(bytes) = key_to_bytes(key, parser.screen().application_cursor()) {
                        sink.send(ClientMsg::Input(bytes)).await?;
                    }
                }
                Some(Ok(Event::Resize(cols, rows))) => {
                    let size = pty_size(cols, rows);
                    parser.screen_mut().set_size(size.rows, size.cols);
                    sink.send(ClientMsg::Resize(size)).await?;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
        draw(terminal, &parser, &status)?;
    }
    Ok(())
}

fn draw(terminal: &mut DefaultTerminal, parser: &vt100::Parser, status: &str) -> Result<()> {
    terminal.draw(|frame| render(frame, parser, status))?;
    Ok(())
}

/// Pure render: PTY fills all but the last row, which is the status bar. Backend-agnostic so
/// it can be exercised against a `TestBackend`.
fn render(frame: &mut ratatui::Frame, parser: &vt100::Parser, status: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    frame.render_widget(PseudoTerminal::new(parser.screen()), chunks[0]);
    let bar = Paragraph::new(Line::from(status.to_string()))
        .style(Style::default().fg(Color::Black).bg(Color::Cyan));
    frame.render_widget(bar, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or_default())
            .collect()
    }

    #[test]
    fn shell_fills_screen_with_status_bar_on_last_row() {
        let (w, h) = (80u16, 24u16);
        let mut parser = vt100::Parser::new(h - 1, w, 100);
        parser.process(b"hello from the shell");

        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render(f, &parser, " amux \u{b7} Ctrl-Q to quit "))
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert!(
            row_text(buffer, 0, w).contains("hello from the shell"),
            "shell content missing from row 0"
        );
        assert!(
            row_text(buffer, h - 1, w).contains("amux"),
            "status bar missing from last row"
        );
    }
}

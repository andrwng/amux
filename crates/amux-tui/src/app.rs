//! The TUI: a persistent agent sidebar beside the selected agent's terminal. Keymap mirrors
//! grove (`j`/`k` select, `n` new, `d` delete, `r` resume, `Enter` open); `Ctrl-B` toggles
//! between navigating the sidebar and typing at the agent; `Ctrl-Q` quits. See DESIGN §7.

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use tui_term::widget::PseudoTerminal;

use amux_core::agent::{sort_for_sidebar, AgentId, AgentState, RosterItem};
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, Size};

use crate::input::key_to_bytes;

const SIDEBAR_W: u16 = 30;

type Sink = SplitSink<Framed<UnixStream, ClientCodec>, ClientMsg>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Nav,
    Terminal,
    Creating,
    Confirming,
}

enum Flow {
    Continue,
    Quit,
}

pub async fn run() -> Result<()> {
    let framed = crate::client::connect().await?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, framed).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    framed: Framed<UnixStream, ClientCodec>,
) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut app = App::new(main_size(cols, rows));
    let (mut sink, mut stream) = framed.split();
    let mut events = EventStream::new();

    draw(terminal, &app)?;
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(dm)) => app.on_daemon(dm),
                _ => break,
            },
            ev = events.next() => match ev {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if let Flow::Quit = app.on_key(key, &mut sink).await? {
                        break;
                    }
                }
                Some(Ok(Event::Resize(c, r))) => app.on_resize(c, r, &mut sink).await?,
                Some(Ok(_)) => {}
                _ => break,
            },
        }
        draw(terminal, &app)?;
    }
    Ok(())
}

/// PTY size for the main window given the terminal size (sidebar + borders + status row).
fn main_size(cols: u16, rows: u16) -> Size {
    Size {
        cols: cols.saturating_sub(SIDEBAR_W + 2).max(1),
        rows: rows.saturating_sub(3).max(1), // 2 borders + 1 status row
    }
}

struct App {
    agents: Vec<AgentInfo>,
    selected: Option<AgentId>,
    attached: Option<AgentId>,
    parser: vt100::Parser,
    main_size: Size,
    mode: Mode,
    create_buf: String,
    confirm_id: Option<AgentId>,
    confirm_msg: String,
    status: String,
}

impl App {
    fn new(main_size: Size) -> Self {
        Self {
            agents: Vec::new(),
            selected: None,
            attached: None,
            parser: vt100::Parser::new(main_size.rows, main_size.cols, 2000),
            main_size,
            mode: Mode::Nav,
            create_buf: String::new(),
            confirm_id: None,
            confirm_msg: String::new(),
            status: "n new · j/k select · enter open · d del · r resume · ctrl-q quit".into(),
        }
    }

    fn on_daemon(&mut self, msg: DaemonMsg) {
        match msg {
            DaemonMsg::Agents(list) => {
                self.agents = list;
                self.ensure_selection();
            }
            DaemonMsg::AgentAdded(info) => {
                self.agents.push(info);
                self.ensure_selection();
            }
            DaemonMsg::AgentRemoved { id } => {
                self.agents.retain(|a| a.id != id);
                if self.attached == Some(id) {
                    self.attached = None;
                }
                if self.selected == Some(id) {
                    self.selected = None;
                }
                self.ensure_selection();
            }
            DaemonMsg::DeleteNeedsConfirm { id, message } => {
                self.confirm_id = Some(id);
                self.confirm_msg = message;
                self.mode = Mode::Confirming;
            }
            DaemonMsg::StateChanged { id, state } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == id) {
                    agent.state = state;
                    agent.last_activity = Utc::now();
                }
            }
            DaemonMsg::OutputSnapshot { id, bytes } => {
                if self.attached == Some(id) {
                    self.parser =
                        vt100::Parser::new(self.main_size.rows, self.main_size.cols, 2000);
                    self.parser.process(&bytes);
                }
            }
            DaemonMsg::Output { id, bytes } => {
                if self.attached == Some(id) {
                    self.parser.process(&bytes);
                }
            }
            DaemonMsg::Error { message } => self.status = message,
            DaemonMsg::Hello { .. } => {}
        }
    }

    async fn on_key(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(Flow::Quit);
        }
        match self.mode {
            Mode::Nav => self.key_nav(key, sink).await,
            Mode::Terminal => self.key_terminal(key, sink).await,
            Mode::Creating => self.key_creating(key, sink).await,
            Mode::Confirming => self.key_confirm(key, sink).await,
        }
    }

    async fn key_nav(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Quit),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('n') => {
                self.mode = Mode::Creating;
                self.create_buf.clear();
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected {
                    sink.send(ClientMsg::DeleteAgent { id, force: false })
                        .await?;
                }
            }
            KeyCode::Char('r') => {
                if let Some(id) = self.selected {
                    sink.send(ClientMsg::ResumeAgent { id }).await?;
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(id) = self.selected {
                    self.attach(id, sink).await?;
                    self.mode = Mode::Terminal;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    async fn key_terminal(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        // Ctrl-B leaves the terminal, back to sidebar navigation.
        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = Mode::Nav;
            return Ok(Flow::Continue);
        }
        if let Some(id) = self.attached {
            if let Some(bytes) = key_to_bytes(key, self.parser.screen().application_cursor()) {
                sink.send(ClientMsg::Input { id, bytes }).await?;
            }
        }
        Ok(Flow::Continue)
    }

    async fn key_creating(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Enter => {
                let branch = std::mem::take(&mut self.create_buf);
                self.mode = Mode::Nav;
                if !branch.trim().is_empty() {
                    sink.send(ClientMsg::CreateAgent {
                        branch: branch.trim().to_string(),
                    })
                    .await?;
                }
            }
            KeyCode::Esc => {
                self.mode = Mode::Nav;
                self.create_buf.clear();
            }
            KeyCode::Backspace => {
                self.create_buf.pop();
            }
            KeyCode::Char(c) => self.create_buf.push(c),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    async fn key_confirm(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(id) = self.confirm_id.take() {
                    sink.send(ClientMsg::DeleteAgent { id, force: true })
                        .await?;
                }
            }
            _ => self.confirm_id = None, // anything else cancels
        }
        self.mode = Mode::Nav;
        Ok(Flow::Continue)
    }

    async fn attach(&mut self, id: AgentId, sink: &mut Sink) -> Result<()> {
        // Single-pane for now: stop streaming the previously-open agent. (The pane UI in the
        // next step keeps several attached at once.)
        if let Some(prev) = self.attached {
            if prev != id {
                sink.send(ClientMsg::Detach { id: prev }).await?;
            }
        }
        self.attached = Some(id);
        self.parser = vt100::Parser::new(self.main_size.rows, self.main_size.cols, 2000);
        sink.send(ClientMsg::Attach {
            id,
            size: self.main_size,
        })
        .await?;
        Ok(())
    }

    async fn on_resize(&mut self, cols: u16, rows: u16, sink: &mut Sink) -> Result<()> {
        self.main_size = main_size(cols, rows);
        self.parser
            .screen_mut()
            .set_size(self.main_size.rows, self.main_size.cols);
        if let Some(id) = self.attached {
            sink.send(ClientMsg::Resize {
                id,
                size: self.main_size,
            })
            .await?;
        }
        Ok(())
    }

    /// Agent ids in sidebar order.
    fn sorted_ids(&self) -> Vec<AgentId> {
        let mut items: Vec<RosterItem> = self
            .agents
            .iter()
            .map(|a| RosterItem {
                id: a.id,
                state: a.state.clone(),
                last_activity: a.last_activity,
            })
            .collect();
        sort_for_sidebar(&mut items);
        items.into_iter().map(|i| i.id).collect()
    }

    fn move_selection(&mut self, delta: i32) {
        let ids = self.sorted_ids();
        if ids.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|s| ids.iter().position(|&i| i == s))
            .unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, ids.len() as i32 - 1) as usize;
        self.selected = Some(ids[next]);
    }

    fn ensure_selection(&mut self) {
        let ids = self.sorted_ids();
        let valid = self.selected.is_some_and(|s| ids.contains(&s));
        if !valid {
            self.selected = ids.first().copied();
        }
    }
}

fn color_for(state: &AgentState) -> Color {
    match state {
        AgentState::Working => Color::Green,
        AgentState::NeedsAttention { .. } => Color::Yellow,
        AgentState::Idle => Color::Gray,
        AgentState::Starting => Color::Cyan,
        AgentState::Exited { .. } => Color::DarkGray,
        AgentState::Error { .. } => Color::Red,
    }
}

fn draw(terminal: &mut DefaultTerminal, app: &App) -> Result<()> {
    terminal.draw(|frame| render(frame, app))?;
    Ok(())
}

fn render(frame: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_W), Constraint::Min(1)])
        .split(rows[0]);

    render_sidebar(frame, cols[0], app);
    render_main(frame, cols[1], app);
    render_status(frame, rows[1], app);
}

fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let waiting = app
        .agents
        .iter()
        .filter(|a| matches!(a.state, AgentState::NeedsAttention { .. }))
        .count();
    let title = if waiting > 0 {
        format!(" agents · {waiting} need you ")
    } else {
        " agents ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let by_id: std::collections::HashMap<_, _> = app.agents.iter().map(|a| (a.id, a)).collect();
    let mut lines = Vec::new();
    if app.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            " no agents — press n",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for id in app.sorted_ids() {
        let Some(agent) = by_id.get(&id) else {
            continue;
        };
        let selected = app.selected == Some(id);
        let attached = app.attached == Some(id);
        let marker = if selected { "\u{25b8}" } else { " " };
        let name_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let name = if attached {
            format!("{:<12.12}*", agent.name)
        } else {
            format!("{:<13.13}", agent.name)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{} ", agent.state.glyph()),
                Style::default().fg(color_for(&agent.state)),
            ),
            Span::styled(name, name_style),
        ]));
        if let AgentState::NeedsAttention {
            message: Some(msg), ..
        } = &agent.state
        {
            lines.push(Line::from(Span::styled(
                format!("     {msg}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app
        .attached
        .and_then(|id| app.agents.iter().find(|a| a.id == id))
    {
        Some(agent) => format!(" {} \u{b7} {} ", agent.branch, agent.state.glyph()),
        None => " no agent open ".to_string(),
    };
    let border_color = app
        .attached
        .and_then(|id| app.agents.iter().find(|a| a.id == id))
        .map(|a| color_for(&a.state))
        .unwrap_or(Color::DarkGray);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.attached.is_some() {
        frame.render_widget(PseudoTerminal::new(app.parser.screen()), inner);
    } else {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Select an agent and press Enter to open it.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  Press n to create one.",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(hint, inner);
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = match app.mode {
        Mode::Creating => (
            format!(" new branch: {}\u{2588}", app.create_buf),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Mode::Terminal => (
            " TERMINAL — ctrl-b sidebar · ctrl-q quit".to_string(),
            Style::default().fg(Color::Black).bg(Color::Green),
        ),
        Mode::Confirming => (
            format!(" {} — delete anyway? y/n", app.confirm_msg),
            Style::default().fg(Color::White).bg(Color::Red),
        ),
        Mode::Nav => (
            format!(" {}", app.status),
            Style::default().fg(Color::DarkGray),
        ),
    };
    frame.render_widget(Paragraph::new(Line::from(text)).style(style), area);
}

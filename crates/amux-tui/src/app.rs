//! The TUI: a persistent agent sidebar beside a **tmux-style tiled pane area**. Focus is
//! spatial — the sidebar plus every pane form one grid, and `Ctrl+hjkl` moves between them
//! (into/out of the sidebar too). `Ctrl+B` is a prefix for structure (`%`/`"` split, `x`
//! close, `r` resize). `Ctrl+Q` quits. See `docs/SPLITS.md`.

use std::collections::HashMap;

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
use crate::pane::{Axis, Dir, PaneTree};

const SIDEBAR_W: u16 = 30;
const RESIZE_STEP: f32 = 0.05;

type Sink = SplitSink<Framed<UnixStream, ClientCodec>, ClientMsg>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Panes,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
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
    let mut app = App::new(cols, rows);
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

/// The rectangle the tiled panes occupy (right of the sidebar, above the status row).
fn main_area(cols: u16, rows: u16) -> Rect {
    Rect::new(
        SIDEBAR_W,
        0,
        cols.saturating_sub(SIDEBAR_W).max(1),
        rows.saturating_sub(1).max(1),
    )
}

/// PTY size for a pane rect (inside its 1-cell border).
fn pane_size(rect: Rect) -> Size {
    Size {
        cols: rect.width.saturating_sub(2).max(1),
        rows: rect.height.saturating_sub(2).max(1),
    }
}

struct App {
    agents: Vec<AgentInfo>,
    sidebar_sel: Option<AgentId>,
    tree: PaneTree,
    parsers: HashMap<AgentId, vt100::Parser>,
    attached: HashMap<AgentId, Size>,
    focus: Focus,
    input: InputMode,
    prefix: bool,
    resize_mode: bool,
    create_buf: String,
    confirm_id: Option<AgentId>,
    confirm_msg: String,
    status: String,
    area: Rect,
}

impl App {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            agents: Vec::new(),
            sidebar_sel: None,
            tree: PaneTree::new(),
            parsers: HashMap::new(),
            attached: HashMap::new(),
            focus: Focus::Sidebar,
            input: InputMode::Normal,
            prefix: false,
            resize_mode: false,
            create_buf: String::new(),
            confirm_id: None,
            confirm_msg: String::new(),
            status: String::new(),
            area: main_area(cols, rows),
        }
    }

    // --- daemon events ---

    fn on_daemon(&mut self, msg: DaemonMsg) {
        match msg {
            DaemonMsg::Agents(list) => {
                self.agents = list;
                self.ensure_sidebar_sel();
            }
            DaemonMsg::AgentAdded(info) => {
                self.agents.push(info);
                self.ensure_sidebar_sel();
            }
            DaemonMsg::AgentRemoved { id } => {
                self.agents.retain(|a| a.id != id);
                self.tree.remove_agent(id);
                self.parsers.remove(&id);
                self.attached.remove(&id);
                if self.sidebar_sel == Some(id) {
                    self.sidebar_sel = None;
                }
                self.ensure_sidebar_sel();
            }
            DaemonMsg::DeleteNeedsConfirm { id, message } => {
                self.confirm_id = Some(id);
                self.confirm_msg = message;
                self.input = InputMode::Confirming;
            }
            DaemonMsg::StateChanged { id, state } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == id) {
                    agent.state = state;
                    agent.last_activity = Utc::now();
                }
            }
            DaemonMsg::OutputSnapshot { id, bytes } => {
                if let Some(&size) = self.attached.get(&id) {
                    let mut parser = vt100::Parser::new(size.rows, size.cols, 2000);
                    parser.process(&bytes);
                    self.parsers.insert(id, parser);
                }
            }
            DaemonMsg::Output { id, bytes } => {
                if let Some(parser) = self.parsers.get_mut(&id) {
                    parser.process(&bytes);
                }
            }
            DaemonMsg::Error { message } => self.status = message,
            DaemonMsg::Hello { .. } => {}
        }
    }

    // --- key handling ---

    async fn on_key(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if is_ctrl(key, 'q') {
            return Ok(Flow::Quit);
        }
        match self.input {
            InputMode::Creating => return self.key_creating(key, sink).await,
            InputMode::Confirming => return self.key_confirm(key, sink).await,
            InputMode::Normal => {}
        }
        if self.resize_mode {
            return self.key_resize(key, sink).await;
        }
        if self.prefix {
            self.prefix = false;
            return self.key_prefix(key, sink).await;
        }
        if is_ctrl(key, 'b') {
            self.prefix = true;
            return Ok(Flow::Continue);
        }
        if let Some(dir) = ctrl_dir(key) {
            self.navigate(dir);
            return Ok(Flow::Continue);
        }
        match self.focus {
            Focus::Sidebar => self.key_sidebar(key, sink).await,
            Focus::Panes => self.key_pane(key, sink).await,
        }
    }

    fn navigate(&mut self, dir: Dir) {
        match self.focus {
            Focus::Sidebar => {
                if dir == Dir::Right && !self.tree.is_empty() {
                    self.tree.focus_first();
                    self.focus = Focus::Panes;
                }
            }
            Focus::Panes => {
                if let crate::pane::Nav::ExitLeft = self.tree.navigate(dir, self.area) {
                    self.focus = Focus::Sidebar;
                }
            }
        }
    }

    async fn key_sidebar(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Quit),
            KeyCode::Char('j') | KeyCode::Down => self.move_sidebar_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sidebar_sel(-1),
            KeyCode::Char('n') => {
                self.input = InputMode::Creating;
                self.create_buf.clear();
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.sidebar_sel {
                    sink.send(ClientMsg::DeleteAgent { id, force: false })
                        .await?;
                }
            }
            KeyCode::Char('r') => {
                if let Some(id) = self.sidebar_sel {
                    sink.send(ClientMsg::ResumeAgent { id }).await?;
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(id) = self.sidebar_sel {
                    self.tree.open(id);
                    self.focus = Focus::Panes;
                    self.reconcile(sink).await?;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    async fn key_pane(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if let Some(agent) = self.tree.focused_agent() {
            let app_cursor = self
                .parsers
                .get(&agent)
                .map(|p| p.screen().application_cursor())
                .unwrap_or(false);
            if let Some(bytes) = key_to_bytes(key, app_cursor) {
                sink.send(ClientMsg::Input { id: agent, bytes }).await?;
            }
        }
        Ok(Flow::Continue)
    }

    async fn key_prefix(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Char('%') if !self.tree.is_empty() => {
                self.tree.split(Axis::LeftRight);
                self.focus = Focus::Panes;
                self.reconcile(sink).await?;
            }
            KeyCode::Char('"') if !self.tree.is_empty() => {
                self.tree.split(Axis::TopBottom);
                self.focus = Focus::Panes;
                self.reconcile(sink).await?;
            }
            KeyCode::Char('x') => {
                self.tree.close();
                if self.tree.is_empty() {
                    self.focus = Focus::Sidebar;
                }
                self.reconcile(sink).await?;
            }
            KeyCode::Char('r') if !self.tree.is_empty() => self.resize_mode = true,
            // Escape hatch: Ctrl+B then a Ctrl-key sends that literal to the focused agent.
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let (Some(agent), Some(byte)) = (self.tree.focused_agent(), ctrl_byte(c)) {
                    sink.send(ClientMsg::Input {
                        id: agent,
                        bytes: vec![byte],
                    })
                    .await?;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    async fn key_resize(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        let dir = match key.code {
            KeyCode::Char('h') | KeyCode::Left => Some(Dir::Left),
            KeyCode::Char('l') | KeyCode::Right => Some(Dir::Right),
            KeyCode::Char('j') | KeyCode::Down => Some(Dir::Down),
            KeyCode::Char('k') | KeyCode::Up => Some(Dir::Up),
            KeyCode::Esc | KeyCode::Enter => {
                self.resize_mode = false;
                None
            }
            _ => None,
        };
        if let Some(dir) = dir {
            self.tree.resize(dir, RESIZE_STEP);
            self.reconcile(sink).await?;
        }
        Ok(Flow::Continue)
    }

    async fn key_creating(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Enter => {
                let branch = std::mem::take(&mut self.create_buf);
                self.input = InputMode::Normal;
                if !branch.trim().is_empty() {
                    sink.send(ClientMsg::CreateAgent {
                        branch: branch.trim().to_string(),
                    })
                    .await?;
                }
            }
            KeyCode::Esc => self.input = InputMode::Normal,
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
            _ => self.confirm_id = None,
        }
        self.input = InputMode::Normal;
        Ok(Flow::Continue)
    }

    async fn on_resize(&mut self, cols: u16, rows: u16, sink: &mut Sink) -> Result<()> {
        self.area = main_area(cols, rows);
        self.reconcile(sink).await
    }

    /// Attach/detach/resize agents to match what the panes now show.
    async fn reconcile(&mut self, sink: &mut Sink) -> Result<()> {
        let mut desired: HashMap<AgentId, Size> = HashMap::new();
        for place in self.tree.layout(self.area) {
            if let Some(agent) = place.agent {
                desired.insert(agent, pane_size(place.rect));
            }
        }

        for (&agent, &size) in &desired {
            if self.attached.get(&agent) != Some(&size) {
                match self.parsers.get_mut(&agent) {
                    Some(parser) => parser.screen_mut().set_size(size.rows, size.cols),
                    None => {
                        self.parsers
                            .insert(agent, vt100::Parser::new(size.rows, size.cols, 2000));
                    }
                }
                sink.send(ClientMsg::Attach { id: agent, size }).await?;
                self.attached.insert(agent, size);
            }
        }

        let gone: Vec<AgentId> = self
            .attached
            .keys()
            .filter(|a| !desired.contains_key(a))
            .copied()
            .collect();
        for agent in gone {
            sink.send(ClientMsg::Detach { id: agent }).await?;
            self.attached.remove(&agent);
            self.parsers.remove(&agent);
        }
        Ok(())
    }

    // --- sidebar selection ---

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

    fn move_sidebar_sel(&mut self, delta: i32) {
        let ids = self.sorted_ids();
        if ids.is_empty() {
            self.sidebar_sel = None;
            return;
        }
        let current = self
            .sidebar_sel
            .and_then(|s| ids.iter().position(|&i| i == s))
            .unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, ids.len() as i32 - 1) as usize;
        self.sidebar_sel = Some(ids[next]);
    }

    fn ensure_sidebar_sel(&mut self) {
        let ids = self.sorted_ids();
        if !self.sidebar_sel.is_some_and(|s| ids.contains(&s)) {
            self.sidebar_sel = ids.first().copied();
        }
    }
}

// --- key helpers ---

fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn ctrl_dir(key: KeyEvent) -> Option<Dir> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('h') => Some(Dir::Left),
        KeyCode::Char('j') => Some(Dir::Down),
        KeyCode::Char('k') => Some(Dir::Up),
        KeyCode::Char('l') => Some(Dir::Right),
        _ => None,
    }
}

/// The control byte for `Ctrl+<c>` (e.g. `l` → 0x0c), for the escape hatch.
fn ctrl_byte(c: char) -> Option<u8> {
    let up = c.to_ascii_uppercase();
    match up {
        '@'..='_' => Some((up as u8) - 0x40),
        ' ' => Some(0),
        _ => None,
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

// --- rendering ---

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
    render_panes(frame, cols[1], app);
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
    let border = if app.focus == Focus::Sidebar {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let by_id: HashMap<_, _> = app.agents.iter().map(|a| (a.id, a)).collect();
    let shown = app.tree.agents();
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
        let selected = app.sidebar_sel == Some(id);
        let open = shown.contains(&id);
        let marker = if selected { "\u{25b8}" } else { " " };
        let name_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let name = if open {
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

fn render_panes(frame: &mut Frame, area: Rect, app: &App) {
    if app.tree.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" no panes ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Select an agent (Enter) to open it here,",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "  or Ctrl+B % / \" to split.",
                    Style::default().fg(Color::DarkGray),
                )),
            ]),
            inner,
        );
        return;
    }

    let by_id: HashMap<_, _> = app.agents.iter().map(|a| (a.id, a)).collect();
    for place in app.tree.layout(area) {
        let focused = place.focused && app.focus == Focus::Panes;
        let (title, color) = match place.agent.and_then(|id| by_id.get(&id)) {
            Some(agent) => (
                format!(" {} {} ", agent.state.glyph(), agent.branch),
                color_for(&agent.state),
            ),
            None => (" empty ".to_string(), Color::DarkGray),
        };
        let border = if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(title);
        let inner = block.inner(place.rect);
        frame.render_widget(block, place.rect);

        match place.agent {
            Some(agent) => match app.parsers.get(&agent) {
                Some(parser) => {
                    frame.render_widget(PseudoTerminal::new(parser.screen()), inner);
                }
                None => frame.render_widget(
                    Paragraph::new("  starting\u{2026}")
                        .style(Style::default().fg(Color::DarkGray)),
                    inner,
                ),
            },
            None => frame.render_widget(
                Paragraph::new("  empty \u{b7} open an agent from the sidebar")
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            ),
        }
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = if app.input == InputMode::Creating {
        (
            format!(" new branch: {}\u{2588}", app.create_buf),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else if app.input == InputMode::Confirming {
        (
            format!(" {} — delete anyway? y/n", app.confirm_msg),
            Style::default().fg(Color::White).bg(Color::Red),
        )
    } else if app.resize_mode {
        (
            " RESIZE — hjkl grow/shrink \u{b7} esc done".to_string(),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else if app.prefix {
        (
            " Ctrl+B — % split \u{b7} \" split \u{b7} x close \u{b7} r resize".to_string(),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else if !app.status.is_empty() {
        (format!(" {}", app.status), Style::default().fg(Color::Red))
    } else {
        let hint = match app.focus {
            Focus::Sidebar => {
                " n new \u{b7} j/k select \u{b7} enter open \u{b7} d del \u{b7} r resume \u{b7} ctrl+hjkl move \u{b7} ctrl+q quit"
            }
            Focus::Panes => {
                " ctrl+hjkl move \u{b7} ctrl+b %/\"/x/r \u{b7} type to talk to the agent \u{b7} ctrl+q quit"
            }
        };
        (hint.to_string(), Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(Paragraph::new(Line::from(text)).style(style), area);
}

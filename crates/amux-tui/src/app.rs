//! The TUI: a repo-grouped agent sidebar beside a tmux-style tiled pane area. Panes stream
//! **terminals**; the sidebar lists **agents** (workspaces) grouped by **repo**. Splitting a pane
//! spawns a `$SHELL` in the same worktree. Focus is spatial — `Ctrl+hjkl` moves between panes and
//! into/out of the sidebar; `Ctrl+B` is a prefix (`%`/`"` split, `x` close, `r` resize). `n`
//! creates an agent in the selected repo, `N` in a repo given by path. `Ctrl+Q` quits.

use std::collections::HashMap;
use std::path::PathBuf;

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

use amux_core::agent::{sort_for_sidebar, AgentId, AgentState, RepoId, RosterItem, TerminalId};
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, RepoInfo, Size};

use crate::input::key_to_bytes;
use crate::pane::{Axis, Dir, Nav, PaneTree};

const SIDEBAR_W: u16 = 30;
const RESIZE_STEP: f32 = 0.05;

type Sink = SplitSink<Framed<UnixStream, ClientCodec>, ClientMsg>;

/// One selectable line in the sidebar: a repo header or an agent under it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Repo(RepoId),
    Agent(AgentId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Panes,
}

/// Which field the two-field "new agent in a repo by path" prompt is editing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Dir,
    Branch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    /// `n` — branch-only, into the repo under the cursor (`create_repo`).
    Creating,
    /// `N` — two fields (directory + branch); registers the repo by path.
    CreatingRepo,
    Confirming,
}

enum Flow {
    Continue,
    Quit,
}

pub async fn run() -> Result<()> {
    let (framed, repo) = crate::client::connect().await?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, framed, repo).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    framed: Framed<UnixStream, ClientCodec>,
    repo: PathBuf,
) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut app = App::new(cols, rows);
    let (mut sink, mut stream) = framed.split();
    let mut events = EventStream::new();

    // Register this client's repo with the (possibly shared) daemon so its agents show up here.
    sink.send(ClientMsg::AddRepo { path: repo }).await?;

    draw(terminal, &app)?;
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(dm)) => app.on_daemon(dm, &mut sink).await?,
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
        // Report focus changes so the daemon can track read/unread.
        app.sync_focus(&mut sink).await?;
        draw(terminal, &app)?;
    }
    Ok(())
}

fn main_area(cols: u16, rows: u16) -> Rect {
    Rect::new(
        SIDEBAR_W,
        0,
        cols.saturating_sub(SIDEBAR_W).max(1),
        rows.saturating_sub(1).max(1),
    )
}

fn pane_size(rect: Rect) -> Size {
    Size {
        cols: rect.width.saturating_sub(2).max(1),
        rows: rect.height.saturating_sub(2).max(1),
    }
}

struct App {
    repos: Vec<RepoInfo>,
    agents: Vec<AgentInfo>,
    sidebar_sel: Option<Row>,
    tree: PaneTree<TerminalId>,
    terminals: HashMap<TerminalId, AgentId>,
    parsers: HashMap<TerminalId, vt100::Parser>,
    attached: HashMap<TerminalId, Size>,
    focus: Focus,
    /// The agent last reported to the daemon as "being viewed" (drives read/unread).
    focus_agent: Option<AgentId>,
    input: InputMode,
    prefix: bool,
    resize_mode: bool,
    /// Branch buffer (both `n` and `N`); repo-path buffer + focused field (`N` only).
    create_buf: String,
    dir_buf: String,
    create_field: Field,
    /// Target repo for the `n` flow (resolved from the cursor at prompt time).
    create_repo: Option<RepoId>,
    confirm_id: Option<AgentId>,
    confirm_msg: String,
    /// Transient banners: `status` is an error (red), `info` a notice (green). Both dismiss on
    /// the next keystroke.
    status: String,
    info: String,
    area: Rect,
}

impl App {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            repos: Vec::new(),
            agents: Vec::new(),
            sidebar_sel: None,
            tree: PaneTree::new(),
            terminals: HashMap::new(),
            parsers: HashMap::new(),
            attached: HashMap::new(),
            focus: Focus::Sidebar,
            focus_agent: None,
            input: InputMode::Normal,
            prefix: false,
            resize_mode: false,
            create_buf: String::new(),
            dir_buf: String::new(),
            create_field: Field::Dir,
            create_repo: None,
            confirm_id: None,
            confirm_msg: String::new(),
            status: String::new(),
            info: String::new(),
            area: main_area(cols, rows),
        }
    }

    fn is_primary(&self, terminal: TerminalId) -> bool {
        self.agents.iter().any(|a| a.primary_terminal == terminal)
    }

    // --- daemon events ---

    async fn on_daemon(&mut self, msg: DaemonMsg, sink: &mut Sink) -> Result<()> {
        match msg {
            DaemonMsg::Repos(list) => {
                self.repos = list;
                self.ensure_sidebar_sel();
            }
            DaemonMsg::RepoAdded(info) => {
                if !self.repos.iter().any(|r| r.id == info.id) {
                    self.repos.push(info);
                }
                self.ensure_sidebar_sel();
            }
            DaemonMsg::Agents(list) => {
                self.agents = list;
                self.ensure_sidebar_sel();
            }
            DaemonMsg::AgentAdded(info) => {
                // Select the freshly-created agent so the next Enter opens it.
                let id = info.id;
                self.agents.push(info);
                self.sidebar_sel = Some(Row::Agent(id));
                self.ensure_sidebar_sel();
            }
            DaemonMsg::AgentRemoved { id } => {
                let terms: Vec<TerminalId> = self
                    .terminals
                    .iter()
                    .filter(|(_, a)| **a == id)
                    .map(|(t, _)| *t)
                    .collect();
                for t in terms {
                    self.tree.close_payload(t);
                    self.parsers.remove(&t);
                    self.attached.remove(&t);
                    self.terminals.remove(&t);
                }
                self.agents.retain(|a| a.id != id);
                if self.sidebar_sel == Some(Row::Agent(id)) {
                    self.sidebar_sel = None;
                }
                if self.tree.is_empty() {
                    self.focus = Focus::Sidebar;
                }
                self.ensure_sidebar_sel();
                self.reconcile(sink).await?;
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
            DaemonMsg::UnreadChanged { id, unread } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == id) {
                    agent.unread = unread;
                }
            }
            DaemonMsg::OutputSnapshot { terminal, bytes } => {
                if let Some(&size) = self.attached.get(&terminal) {
                    let mut parser = vt100::Parser::new(size.rows, size.cols, 2000);
                    parser.process(&bytes);
                    self.parsers.insert(terminal, parser);
                }
            }
            DaemonMsg::Output { terminal, bytes } => {
                if let Some(parser) = self.parsers.get_mut(&terminal) {
                    parser.process(&bytes);
                }
            }
            DaemonMsg::TerminalExited { terminal, .. } => {
                self.tree.close_payload(terminal);
                self.parsers.remove(&terminal);
                self.attached.remove(&terminal);
                self.terminals.remove(&terminal);
                if self.tree.is_empty() {
                    self.focus = Focus::Sidebar;
                }
                self.reconcile(sink).await?;
            }
            DaemonMsg::DoctorReport {
                pruned, skipped, ..
            } => {
                self.info = doctor_summary(&pruned, &skipped);
            }
            DaemonMsg::Error { message } => self.status = message,
            DaemonMsg::Hello { .. } => {}
        }
        Ok(())
    }

    // --- key handling ---

    async fn on_key(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if is_ctrl(key, 'q') {
            return Ok(Flow::Quit);
        }
        // Any keystroke dismisses a lingering banner (it still performs its action).
        if self.input == InputMode::Normal {
            self.status.clear();
            self.info.clear();
        }
        match self.input {
            InputMode::Creating => return self.key_creating(key, sink).await,
            InputMode::CreatingRepo => return self.key_creating_repo(key, sink).await,
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
                if let Nav::ExitLeft = self.tree.navigate(dir, self.area) {
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
            // `n`: new agent in the repo under the cursor (branch-only prompt).
            KeyCode::Char('n') => {
                if let Some(repo) = self.selected_repo() {
                    self.create_repo = Some(repo);
                    self.create_buf.clear();
                    self.input = InputMode::Creating;
                }
            }
            // `N`: new agent in a repo given by path (directory + branch).
            KeyCode::Char('N') => {
                self.create_buf.clear();
                self.dir_buf.clear();
                self.create_field = Field::Dir;
                self.input = InputMode::CreatingRepo;
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_agent() {
                    sink.send(ClientMsg::DeleteAgent { id, force: false })
                        .await?;
                }
            }
            KeyCode::Char('r') => {
                if let Some(id) = self.selected_agent() {
                    sink.send(ClientMsg::ResumeAgent { id }).await?;
                }
            }
            // `P`: doctor — prune the selected repo's orphaned worktrees (reclaim wedged branches).
            KeyCode::Char('P') => {
                if let Some(repo) = self.selected_repo() {
                    sink.send(ClientMsg::DoctorRepo { repo }).await?;
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => self.open_selected(sink).await?,
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Open the selected agent's primary terminal into the focused pane.
    async fn open_selected(&mut self, sink: &mut Sink) -> Result<()> {
        let Some(id) = self.selected_agent() else {
            return Ok(());
        };
        let Some(terminal) = self
            .agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.primary_terminal)
        else {
            return Ok(());
        };
        self.terminals.insert(terminal, id);
        self.tree.open(terminal);
        self.focus = Focus::Panes;
        self.reconcile(sink).await
    }

    async fn key_pane(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if let Some(terminal) = self.tree.focused_payload() {
            let app_cursor = self
                .parsers
                .get(&terminal)
                .map(|p| p.screen().application_cursor())
                .unwrap_or(false);
            if let Some(bytes) = key_to_bytes(key, app_cursor) {
                sink.send(ClientMsg::Input { terminal, bytes }).await?;
            }
        }
        Ok(Flow::Continue)
    }

    async fn key_prefix(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Char('%') => self.split(Axis::LeftRight, sink).await?,
            KeyCode::Char('"') => self.split(Axis::TopBottom, sink).await?,
            KeyCode::Char('x') => {
                self.tree.close();
                if self.tree.is_empty() {
                    self.focus = Focus::Sidebar;
                }
                self.reconcile(sink).await?;
            }
            KeyCode::Char('r') if !self.tree.is_empty() => self.resize_mode = true,
            // Direct resize (tmux muscle memory): `Ctrl+B` then capital H/J/K/L resizes the
            // focused pane one step and stays in resize mode so you can keep nudging (like `-r`).
            KeyCode::Char('H' | 'J' | 'K' | 'L') if !self.tree.is_empty() => {
                if let Some(dir) = resize_dir(key.code) {
                    self.tree.resize(dir, RESIZE_STEP);
                    self.resize_mode = true;
                    self.reconcile(sink).await?;
                }
            }
            // Jump to the next unread agent (inbox navigation).
            KeyCode::Tab => self.jump_next_unread(sink).await?,
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let (Some(terminal), Some(byte)) = (self.tree.focused_payload(), ctrl_byte(c)) {
                    sink.send(ClientMsg::Input {
                        terminal,
                        bytes: vec![byte],
                    })
                    .await?;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Split: spawn a `$SHELL` terminal in the same worktree as the focused pane.
    async fn split(&mut self, axis: Axis, sink: &mut Sink) -> Result<()> {
        let Some(from) = self.tree.focused_payload() else {
            return Ok(());
        };
        let Some(&agent) = self.terminals.get(&from) else {
            return Ok(());
        };
        let terminal = TerminalId::new();
        self.terminals.insert(terminal, agent);
        self.tree.split(axis);
        self.tree.open(terminal);
        self.focus = Focus::Panes;
        sink.send(ClientMsg::SpawnShell {
            terminal,
            like: from,
        })
        .await?;
        self.reconcile(sink).await
    }

    async fn key_resize(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
            self.resize_mode = false;
            return Ok(Flow::Continue);
        }
        // Accept both hjkl and HJKL (and arrows) so it doesn't matter if Shift is still held.
        if let Some(dir) = resize_dir(key.code) {
            self.tree.resize(dir, RESIZE_STEP);
            self.reconcile(sink).await?;
        }
        Ok(Flow::Continue)
    }

    async fn key_creating(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Enter => {
                let branch = std::mem::take(&mut self.create_buf);
                let repo = self.create_repo.take();
                self.input = InputMode::Normal;
                if let (Some(repo), false) = (repo, branch.trim().is_empty()) {
                    sink.send(ClientMsg::CreateAgent {
                        repo,
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

    /// The two-field `N` prompt: Tab switches fields, Enter registers the repo + creates.
    async fn key_creating_repo(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Enter => {
                let dir = self.dir_buf.trim().to_string();
                let branch = self.create_buf.trim().to_string();
                // Enter on the first field just advances; require both to submit.
                if self.create_field == Field::Dir && !dir.is_empty() {
                    self.create_field = Field::Branch;
                } else if !dir.is_empty() && !branch.is_empty() {
                    self.dir_buf.clear();
                    self.create_buf.clear();
                    self.input = InputMode::Normal;
                    sink.send(ClientMsg::CreateAgentAt {
                        path: expand_path(&dir),
                        branch,
                    })
                    .await?;
                }
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                self.create_field = match self.create_field {
                    Field::Dir => Field::Branch,
                    Field::Branch => Field::Dir,
                };
            }
            KeyCode::Esc => self.input = InputMode::Normal,
            KeyCode::Backspace => {
                self.active_buf().pop();
            }
            KeyCode::Char(c) => self.active_buf().push(c),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn active_buf(&mut self) -> &mut String {
        match self.create_field {
            Field::Dir => &mut self.dir_buf,
            Field::Branch => &mut self.create_buf,
        }
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

    /// Attach/detach/resize terminals to match what the panes now show. A pane that leaves the
    /// layout is detached (primary — agent keeps running) or closed (shell — killed).
    async fn reconcile(&mut self, sink: &mut Sink) -> Result<()> {
        let mut desired: HashMap<TerminalId, Size> = HashMap::new();
        for place in self.tree.layout(self.area) {
            if let Some(t) = place.payload {
                desired.insert(t, pane_size(place.rect));
            }
        }

        for (&terminal, &size) in &desired {
            if self.attached.get(&terminal) != Some(&size) {
                match self.parsers.get_mut(&terminal) {
                    Some(parser) => parser.screen_mut().set_size(size.rows, size.cols),
                    None => {
                        self.parsers
                            .insert(terminal, vt100::Parser::new(size.rows, size.cols, 2000));
                    }
                }
                sink.send(ClientMsg::Attach { terminal, size }).await?;
                self.attached.insert(terminal, size);
            }
        }

        let gone: Vec<TerminalId> = self
            .attached
            .keys()
            .filter(|t| !desired.contains_key(t))
            .copied()
            .collect();
        for terminal in gone {
            if self.is_primary(terminal) {
                sink.send(ClientMsg::Detach { terminal }).await?;
            } else {
                sink.send(ClientMsg::CloseTerminal { terminal }).await?;
            }
            self.attached.remove(&terminal);
            self.parsers.remove(&terminal);
            self.terminals.remove(&terminal);
        }
        Ok(())
    }

    // --- sidebar selection ---

    /// Agent ids for one repo, ordered needs-attention-first then by recency.
    fn agent_ids_for(&self, repo: RepoId) -> Vec<AgentId> {
        let mut items: Vec<RosterItem> = self
            .agents
            .iter()
            .filter(|a| a.repo == repo)
            .map(|a| RosterItem {
                id: a.id,
                state: a.state.clone(),
                unread: a.unread,
                last_activity: a.last_activity,
            })
            .collect();
        sort_for_sidebar(&mut items);
        items.into_iter().map(|i| i.id).collect()
    }

    /// The flat, ordered list of selectable rows: each repo header followed by its agents.
    /// Repos are sorted by name so the layout is stable.
    fn sidebar_rows(&self) -> Vec<Row> {
        let mut repos: Vec<&RepoInfo> = self.repos.iter().collect();
        repos.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let mut rows = Vec::new();
        for repo in repos {
            rows.push(Row::Repo(repo.id));
            for id in self.agent_ids_for(repo.id) {
                rows.push(Row::Agent(id));
            }
        }
        rows
    }

    /// The repo the cursor is in: the selected repo header, or the selected agent's repo.
    fn selected_repo(&self) -> Option<RepoId> {
        match self.sidebar_sel? {
            Row::Repo(id) => Some(id),
            Row::Agent(id) => self.agents.iter().find(|a| a.id == id).map(|a| a.repo),
        }
    }

    /// The selected agent, if the cursor is on an agent row (not a repo header).
    fn selected_agent(&self) -> Option<AgentId> {
        match self.sidebar_sel? {
            Row::Agent(id) => Some(id),
            Row::Repo(_) => None,
        }
    }

    /// The agent the user is currently viewing: the one owning the focused terminal (when focus
    /// is in the panes), else `None`. This is what "seen" is anchored to.
    fn current_focus_agent(&self) -> Option<AgentId> {
        if self.focus != Focus::Panes {
            return None;
        }
        let terminal = self.tree.focused_payload()?;
        self.terminals.get(&terminal).copied()
    }

    /// Tell the daemon which agent is being viewed, when it changes — so it can clear/keep unread.
    async fn sync_focus(&mut self, sink: &mut Sink) -> Result<()> {
        let current = self.current_focus_agent();
        if current != self.focus_agent {
            self.focus_agent = current;
            sink.send(ClientMsg::Focus { agent: current }).await?;
        }
        Ok(())
    }

    /// Jump to the next unread agent (in sidebar order, wrapping) and open it — which views it,
    /// clearing its unread. The inbox payoff. No-op with a notice if nothing is unread.
    async fn jump_next_unread(&mut self, sink: &mut Sink) -> Result<()> {
        let order: Vec<AgentId> = self
            .sidebar_rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Agent(id) => Some(id),
                Row::Repo(_) => None,
            })
            .collect();
        let is_unread = |id: &AgentId| self.agents.iter().any(|a| a.id == *id && a.unread);
        if !order.iter().any(is_unread) {
            self.info = "no unread agents".to_string();
            return Ok(());
        }
        let start = self
            .selected_agent()
            .and_then(|c| order.iter().position(|&i| i == c))
            .map(|i| i + 1)
            .unwrap_or(0);
        let n = order.len();
        let next = (0..n)
            .map(|k| order[(start + k) % n])
            .find(|id| is_unread(id));
        if let Some(id) = next {
            self.sidebar_sel = Some(Row::Agent(id));
            self.open_selected(sink).await?;
        }
        Ok(())
    }

    fn move_sidebar_sel(&mut self, delta: i32) {
        let rows = self.sidebar_rows();
        if rows.is_empty() {
            self.sidebar_sel = None;
            return;
        }
        let current = self
            .sidebar_sel
            .and_then(|s| rows.iter().position(|&r| r == s))
            .unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, rows.len() as i32 - 1) as usize;
        self.sidebar_sel = Some(rows[next]);
    }

    fn ensure_sidebar_sel(&mut self) {
        let rows = self.sidebar_rows();
        if !self.sidebar_sel.is_some_and(|s| rows.contains(&s)) {
            self.sidebar_sel = rows.first().copied();
        }
    }
}

/// A one-line summary of a doctor run for the notice banner.
fn doctor_summary(pruned: &[String], skipped: &[(String, usize)]) -> String {
    if pruned.is_empty() && skipped.is_empty() {
        return "doctor: no orphaned worktrees — nothing to prune".to_string();
    }
    let mut parts = Vec::new();
    if !pruned.is_empty() {
        parts.push(format!("pruned {} ({})", pruned.len(), pruned.join(", ")));
    }
    if !skipped.is_empty() {
        let names: Vec<String> = skipped
            .iter()
            .map(|(n, d)| format!("{n}: {d} uncommitted"))
            .collect();
        parts.push(format!("skipped {} ({})", skipped.len(), names.join(", ")));
    }
    format!("doctor: {}", parts.join(" · "))
}

/// Expand a leading `~` to the home directory; otherwise return the path unchanged.
fn expand_path(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(dirs) = directories::BaseDirs::new() {
            return dirs.home_dir().join(rest);
        }
    }
    PathBuf::from(input)
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

/// Map a resize key to a direction — accepts hjkl, HJKL, and the arrow keys.
fn resize_dir(code: KeyCode) -> Option<Dir> {
    match code {
        KeyCode::Char('h' | 'H') | KeyCode::Left => Some(Dir::Left),
        KeyCode::Char('l' | 'L') | KeyCode::Right => Some(Dir::Right),
        KeyCode::Char('j' | 'J') | KeyCode::Down => Some(Dir::Down),
        KeyCode::Char('k' | 'K') | KeyCode::Up => Some(Dir::Up),
        _ => None,
    }
}

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
    let unread = app.agents.iter().filter(|a| a.unread).count();
    let title = if unread > 0 {
        format!(" agents · {unread} unread ")
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
    let repo_names: HashMap<_, _> = app.repos.iter().map(|r| (r.id, r.name.as_str())).collect();
    let open: Vec<AgentId> = app.terminals.values().copied().collect();
    let mut lines = Vec::new();
    if app.repos.is_empty() {
        lines.push(Line::from(Span::styled(
            " no repos yet…",
            Style::default().fg(Color::DarkGray),
        )));
    }

    for row in app.sidebar_rows() {
        let selected = app.sidebar_sel == Some(row);
        let marker = if selected { "\u{25b8}" } else { " " };
        match row {
            Row::Repo(id) => {
                let name = repo_names.get(&id).copied().unwrap_or("repo");
                let count = app.agents.iter().filter(|a| a.repo == id).count();
                let mut style = Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD);
                if selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{marker} \u{25be} "),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(format!("{name} "), style),
                    Span::styled(format!("({count})"), Style::default().fg(Color::DarkGray)),
                ]));
                if count == 0 {
                    lines.push(Line::from(Span::styled(
                        "      no agents — press n",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            Row::Agent(id) => {
                let Some(agent) = by_id.get(&id) else {
                    continue;
                };
                let is_open = open.contains(&id);
                // Unread agents render bold with a leading • dot; read ones are plain.
                let name_style = if agent.unread || selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let dot = if agent.unread { "\u{2022}" } else { " " };
                let name = if is_open {
                    format!("{:<10.10}*", agent.name)
                } else {
                    format!("{:<11.11}", agent.name)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} {dot} "), Style::default().fg(Color::Cyan)),
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
                        format!("       {msg}"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::DIM),
                    )));
                }
            }
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
                    "  Select an agent (Enter) to open it here.",
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
        let (title, color) = match place.payload {
            Some(terminal) => match app.terminals.get(&terminal).and_then(|id| by_id.get(id)) {
                Some(agent) if agent.primary_terminal == terminal => (
                    format!(" {} {} ", agent.state.glyph(), agent.branch),
                    color_for(&agent.state),
                ),
                Some(agent) => (format!(" sh \u{b7} {} ", agent.branch), Color::Blue),
                None => (" terminal ".to_string(), Color::DarkGray),
            },
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

        match place.payload.and_then(|t| app.parsers.get(&t)) {
            Some(parser) => frame.render_widget(PseudoTerminal::new(parser.screen()), inner),
            None => frame.render_widget(
                Paragraph::new("  \u{2026}").style(Style::default().fg(Color::DarkGray)),
                inner,
            ),
        }
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = if app.input == InputMode::Creating {
        let repo = app
            .create_repo
            .and_then(|id| app.repos.iter().find(|r| r.id == id))
            .map(|r| r.name.as_str())
            .unwrap_or("?");
        (
            format!(" new agent in {repo} — branch: {}\u{2588}", app.create_buf),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else if app.input == InputMode::CreatingRepo {
        let cursor = |f: Field| {
            if app.create_field == f {
                "\u{2588}"
            } else {
                ""
            }
        };
        (
            format!(
                " new agent — dir: {}{}  branch: {}{}  (tab switch \u{b7} enter next/create)",
                app.dir_buf,
                cursor(Field::Dir),
                app.create_buf,
                cursor(Field::Branch),
            ),
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
            " Ctrl+B — % split \u{b7} \" split \u{b7} x close \u{b7} HJKL/r resize \u{b7} tab unread"
                .to_string(),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else if !app.status.is_empty() {
        (
            format!(" \u{26a0} {} \u{b7} (press any key to dismiss)", app.status),
            Style::default().fg(Color::White).bg(Color::Red),
        )
    } else if !app.info.is_empty() {
        (
            format!(" \u{2713} {} \u{b7} (press any key to dismiss)", app.info),
            Style::default().fg(Color::Black).bg(Color::Green),
        )
    } else {
        let hint = match app.focus {
            Focus::Sidebar => {
                " n new \u{b7} N new+repo \u{b7} j/k select \u{b7} enter open \u{b7} d del \u{b7} r resume \u{b7} P prune \u{b7} ctrl+q quit"
            }
            Focus::Panes => {
                " ctrl+hjkl move \u{b7} ctrl+b %/\"/x/r \u{b7} type to talk \u{b7} ctrl+q quit"
            }
        };
        (hint.to_string(), Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(Paragraph::new(Line::from(text)).style(style), area);
}

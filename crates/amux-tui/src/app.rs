//! The TUI: a repo-grouped agent sidebar beside a tmux-style tiled pane area. Panes stream
//! **terminals**; the sidebar lists **agents** (workspaces) grouped by **repo**. Splitting a pane
//! spawns a `$SHELL` in the same worktree. Focus is spatial — `Ctrl+hjkl` moves between panes and
//! into/out of the sidebar; `Ctrl+B` is a prefix (`%`/`"` split, `x` close, `r` resize). `n`
//! creates an agent in the selected repo, `N` in a repo given by path. `Ctrl+Q` quits.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use ratatui::buffer::Buffer;
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
/// Max height of the minis row (capped to half the main area).
const MINI_ROWS: u16 = 14;

type Sink = SplitSink<Framed<UnixStream, ClientCodec>, ClientMsg>;

/// One selectable line in the sidebar: a repo header or an agent under it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Repo(RepoId),
    Agent(AgentId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Sidebar,
    Panes,
    /// The i-th mini (a floating row below the main panes) is focused.
    Mini(usize),
}

/// A mouse text selection isolated to one pane: anchor + head in screen (col,row) coords, clamped
/// to the pane's inner area. amux draws and copies this itself, so it never spills across panes.
#[derive(Clone, Copy)]
struct Selection {
    terminal: TerminalId,
    inner: Rect,
    anchor: (u16, u16),
    head: (u16, u16),
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
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    let (framed, repo) = crate::client::connect().await?;
    let mut terminal = ratatui::init();
    // Capture the mouse so the wheel reaches panes (forwarded to apps that want it, else scrolls
    // amux's own scrollback). Hold Shift to bypass for native terminal selection.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, framed, repo).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
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
                Some(Ok(Event::Mouse(me))) => app.on_mouse(me, &mut sink).await?,
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
    /// The **active** agent's live pane layout (what the main area shows). Each agent owns its own
    /// workspace: opening an agent swaps this out, and splits belong to that agent.
    tree: PaneTree<TerminalId>,
    /// Saved layouts for the non-active agents (this session), restored when you switch back.
    trees: HashMap<AgentId, PaneTree<TerminalId>>,
    /// Layouts the daemon persisted from a previous client session — restored the first time you
    /// open each agent, so splits survive closing the TUI.
    saved_layouts: HashMap<AgentId, amux_proto::Layout>,
    /// The agent whose workspace is currently on screen (`None` = nothing opened yet).
    active_agent: Option<AgentId>,
    terminals: HashMap<TerminalId, AgentId>,
    parsers: HashMap<TerminalId, vt100::Parser>,
    attached: HashMap<TerminalId, Size>,
    /// Terminals whose foreground app wants `Ctrl+hjkl` (vim-like), so we pass those keys through
    /// instead of navigating. Announced by the daemon via `TerminalApp`.
    passthrough: HashMap<TerminalId, bool>,
    focus: Focus,
    /// Agents shown as **minis** — a spatial row of small live terminals below the main panes,
    /// left-to-right. Each shows that agent's primary terminal.
    minis: Vec<AgentId>,
    /// Minimized minis: collapsed to a status-only strip (terminal detached), still in the row.
    minimized: HashSet<AgentId>,
    /// Peek: temporarily hide the whole minis row to see the full main area.
    minis_hidden: bool,
    /// Where focus was before it entered the minis row, so closing a mini can return there.
    focus_return: Focus,
    /// The agent last reported to the daemon as "being viewed" (drives read/unread).
    focus_agent: Option<AgentId>,
    input: InputMode,
    prefix: bool,
    resize_mode: bool,
    /// Scroll (copy) mode: the terminal being scrolled and how many rows back into its
    /// scrollback the view is. `None` = live at the bottom.
    scroll_mode: Option<TerminalId>,
    scroll_offset: usize,
    /// The active mouse selection (drag in progress or last completed), if any.
    selection: Option<Selection>,
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
            trees: HashMap::new(),
            saved_layouts: HashMap::new(),
            active_agent: None,
            terminals: HashMap::new(),
            parsers: HashMap::new(),
            attached: HashMap::new(),
            passthrough: HashMap::new(),
            focus: Focus::Sidebar,
            minis: Vec::new(),
            minimized: HashSet::new(),
            minis_hidden: false,
            focus_return: Focus::Sidebar,
            focus_agent: None,
            input: InputMode::Normal,
            prefix: false,
            resize_mode: false,
            scroll_mode: None,
            scroll_offset: 0,
            selection: None,
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

    /// The pane region (always the **full** main area — panes keep their whole rectangle) and, when
    /// minis are open, the band they **float over** at the bottom. Navigation treats the minis as
    /// the row below the panes even though visually they overlay, not displace, them.
    fn regions(&self) -> (Rect, Option<Rect>) {
        if self.minis.is_empty() || self.minis_hidden {
            return (self.area, None);
        }
        let mini_h = (self.area.height / 2).clamp(3, MINI_ROWS);
        // Inset the band 1 cell on the bottom + right, leaving room for the drop shadow.
        let minis = Rect::new(
            self.area.x,
            self.area.y + self.area.height.saturating_sub(mini_h + 1),
            self.area.width.saturating_sub(1),
            mini_h,
        );
        (self.area, Some(minis))
    }

    /// The rectangle for each mini: discrete fixed-width windows floating in the **bottom-right**
    /// of `area`, laid out left-to-right (newest in the corner). Minimized minis get a narrow
    /// status strip. The group is right-anchored; if it would overrun the left edge it's clamped.
    fn mini_rects(&self, area: Rect) -> Vec<Rect> {
        const MINI_W: u16 = 44;
        const MIN_W: u16 = 12;
        let widths: Vec<u16> = self
            .minis
            .iter()
            .map(|a| {
                if self.minimized.contains(a) {
                    MIN_W
                } else {
                    MINI_W
                }
            })
            .collect();
        let total: u16 = widths.iter().sum();
        let right = area.x + area.width;
        let mut x = right.saturating_sub(total).max(area.x);
        widths
            .iter()
            .map(|&w| {
                let w = w.min(right.saturating_sub(x)); // clip against the right edge
                let rect = Rect::new(x, area.y, w, area.height);
                x += w;
                rect
            })
            .collect()
    }

    /// The primary terminal of the i-th mini, if it maps to a known agent.
    fn mini_terminal(&self, i: usize) -> Option<TerminalId> {
        let agent = self.minis.get(i)?;
        self.agents
            .iter()
            .find(|a| a.id == *agent)
            .map(|a| a.primary_terminal)
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
            DaemonMsg::Layouts(list) => {
                self.saved_layouts = list.into_iter().collect();
            }
            DaemonMsg::Minis(list) => {
                // Restore minis for agents that still exist (their terminals kept running).
                self.minis = list
                    .into_iter()
                    .filter(|id| self.agents.iter().any(|a| a.id == *id))
                    .filter(|id| Some(*id) != self.active_agent)
                    .collect();
                self.reconcile(sink).await?;
            }
            DaemonMsg::Active(active) => {
                // Restore the main pane: reopen the agent that was active. Layouts arrived first,
                // so swap_to_agent can rebuild its tree from a saved layout.
                if let Some(id) = active {
                    if self.active_agent != Some(id) && self.agents.iter().any(|a| a.id == id) {
                        self.swap_to_agent(id);
                        self.reconcile(sink).await?;
                    }
                }
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
                    self.forget_terminal(t);
                }
                // Drop the agent's saved workspace + any mini; if it was active, go back to nothing.
                self.trees.remove(&id);
                self.minis.retain(|a| *a != id);
                self.minimized.remove(&id);
                if self.active_agent == Some(id) {
                    self.active_agent = None;
                }
                // A removed mini may have shifted indices out from under the focus.
                if let Focus::Mini(i) = self.focus {
                    if i >= self.minis.len() {
                        self.focus = self.focus_return;
                    }
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
            DaemonMsg::TerminalApp {
                terminal,
                passthrough,
            } => {
                self.passthrough.insert(terminal, passthrough);
            }
            DaemonMsg::Navigate { dir, .. } => {
                // A vim-like app hit its edge and handed navigation back — move from its (focused)
                // pane in that direction, exactly like a Ctrl+hjkl keypress would.
                self.navigate(dir);
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
                    // vt100 bumps its scrollback offset to keep the scrolled view anchored on the
                    // same lines as new output arrives; mirror that into our cached offset so the
                    // ↑N indicator and the next scroll keypress stay in sync (the view doesn't
                    // drift out from under you).
                    if self.scroll_mode == Some(terminal) {
                        self.scroll_offset = parser.screen().scrollback();
                    }
                }
            }
            DaemonMsg::TerminalExited { terminal, .. } => {
                self.forget_terminal(terminal);
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
        // Any keystroke dismisses a lingering banner + selection highlight (still acts).
        if self.input == InputMode::Normal {
            self.status.clear();
            self.info.clear();
            self.selection = None;
        }
        match self.input {
            InputMode::Creating => return self.key_creating(key, sink).await,
            InputMode::CreatingRepo => return self.key_creating_repo(key, sink).await,
            InputMode::Confirming => return self.key_confirm(key, sink).await,
            InputMode::Normal => {}
        }
        if let Some(terminal) = self.scroll_mode {
            if self.parsers.contains_key(&terminal) {
                return Ok(self.key_scroll(key, terminal));
            }
            self.scroll_mode = None; // the pane went away
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
            // If a vim-like app owns the focused pane, pass Ctrl+hjkl through to it (it moves its
            // own splits, and hands back to amux at its edge via `amux nav`).
            if self.focus == Focus::Panes && self.focused_is_passthrough() {
                return self.key_pane(key, sink).await;
            }
            self.navigate(dir);
            return Ok(Flow::Continue);
        }
        match self.focus {
            Focus::Sidebar => self.key_sidebar(key, sink).await,
            Focus::Panes => self.key_pane(key, sink).await,
            Focus::Mini(i) => self.key_mini(key, i, sink).await,
        }
    }

    /// Keystrokes for a focused mini go to that agent's primary terminal.
    async fn key_mini(&mut self, key: KeyEvent, i: usize, sink: &mut Sink) -> Result<Flow> {
        if let Some(terminal) = self.mini_terminal(i) {
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

    /// Whether the focused pane's terminal has announced it wants `Ctrl+hjkl` (a vim-like app).
    fn focused_is_passthrough(&self) -> bool {
        self.tree
            .focused_payload()
            .is_some_and(|t| self.passthrough.get(&t).copied().unwrap_or(false))
    }

    fn navigate(&mut self, dir: Dir) {
        let (pane_area, _) = self.regions();
        match self.focus {
            Focus::Sidebar => {
                if dir == Dir::Right {
                    if !self.tree.is_empty() {
                        self.tree.focus_first();
                        self.focus = Focus::Panes;
                    } else if !self.minis.is_empty() {
                        self.enter_mini(0);
                    }
                }
            }
            Focus::Panes => match self.tree.navigate(dir, pane_area) {
                Nav::ExitLeft => self.focus = Focus::Sidebar,
                // Off the bottom of the panes drops into the minis row.
                Nav::Stay if dir == Dir::Down && !self.minis.is_empty() => self.enter_mini(0),
                _ => {}
            },
            Focus::Mini(i) => match dir {
                Dir::Left if i > 0 => self.focus = Focus::Mini(i - 1),
                // The minis also sit to the *right* of the main pane, so left off the leftmost
                // enters the panes (falling through to the sidebar only when there are none).
                Dir::Left if !self.tree.is_empty() => self.focus = Focus::Panes,
                Dir::Left => self.focus = Focus::Sidebar,
                Dir::Right if i + 1 < self.minis.len() => self.focus = Focus::Mini(i + 1),
                // Climb back into the main layout (minis sit below it too).
                Dir::Up if !self.tree.is_empty() => self.focus = Focus::Panes,
                Dir::Up => self.focus = Focus::Sidebar,
                _ => {}
            },
        }
    }

    /// Move focus into the i-th mini, remembering where we came from so closing it can return.
    fn enter_mini(&mut self, i: usize) {
        if !matches!(self.focus, Focus::Mini(_)) {
            self.focus_return = self.focus;
        }
        self.focus = Focus::Mini(i);
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
            // `m`: open the selected agent as a mini (a small live window below the main panes).
            KeyCode::Char('m') => {
                if let Some(id) = self.selected_agent() {
                    self.open_mini(id, sink).await?;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Open `agent` as a mini and focus it (returning to the sidebar when closed). No-op for the
    /// agent already in the main area; focuses it if it's already a mini.
    async fn open_mini(&mut self, agent: AgentId, sink: &mut Sink) -> Result<()> {
        if self.active_agent == Some(agent) {
            return Ok(());
        }
        if let Some(i) = self.minis.iter().position(|a| *a == agent) {
            self.enter_mini(i);
            return Ok(());
        }
        self.minis.push(agent);
        self.enter_mini(self.minis.len() - 1);
        self.reconcile(sink).await
    }

    /// Close the i-th mini; focus returns to another mini, or to where it started.
    async fn close_mini(&mut self, i: usize, sink: &mut Sink) -> Result<()> {
        if i >= self.minis.len() {
            return Ok(());
        }
        let agent = self.minis.remove(i);
        self.minimized.remove(&agent);
        self.focus = if self.minis.is_empty() {
            self.focus_return
        } else {
            Focus::Mini(i.min(self.minis.len() - 1))
        };
        self.reconcile(sink).await
    }

    /// Open the selected agent — switch the main area to its workspace.
    async fn open_selected(&mut self, sink: &mut Sink) -> Result<()> {
        let Some(id) = self.selected_agent() else {
            return Ok(());
        };
        self.activate(id, sink).await
    }

    /// Fully drop a terminal that's gone for good (killed / exited / its agent deleted): remove it
    /// from the active layout *and* every saved layout, and from all per-terminal maps.
    fn forget_terminal(&mut self, terminal: TerminalId) {
        self.tree.close_payload(terminal);
        for tree in self.trees.values_mut() {
            tree.close_payload(terminal);
        }
        self.parsers.remove(&terminal);
        self.attached.remove(&terminal);
        self.terminals.remove(&terminal);
        self.passthrough.remove(&terminal);
    }

    /// Make `id` the active agent: save the current agent's layout, restore (or create) `id`'s.
    /// Each agent's workspace is its own tiled tree — switching swaps the whole main area.
    async fn activate(&mut self, id: AgentId, sink: &mut Sink) -> Result<()> {
        self.swap_to_agent(id);
        self.reconcile(sink).await
    }

    /// The pure state change behind [`activate`]: save the current agent's layout, restore (or
    /// create) `id`'s, and ensure its primary terminal is shown.
    fn swap_to_agent(&mut self, id: AgentId) {
        // An agent shown in the main area isn't also a mini (its terminal can't be two sizes).
        self.minis.retain(|a| *a != id);
        self.minimized.remove(&id);
        if self.active_agent != Some(id) {
            if let Some(prev) = self.active_agent {
                self.trees.insert(prev, std::mem::take(&mut self.tree));
            }
            // This session's live tree, else a layout the daemon persisted from a past session.
            self.tree = self
                .trees
                .remove(&id)
                .or_else(|| {
                    self.saved_layouts
                        .remove(&id)
                        .map(|l| PaneTree::from_layout(&l))
                })
                .unwrap_or_default();
            self.active_agent = Some(id);
            // A restored layout's terminals belong to this agent (for rendering + splits).
            for t in self.tree.payloads() {
                self.terminals.insert(t, id);
            }
        }
        // First time (or after its panes were all closed): show the agent's primary terminal.
        if self.tree.is_empty() {
            if let Some(primary) = self
                .agents
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.primary_terminal)
            {
                self.terminals.insert(primary, id);
                self.tree.open(primary);
            }
        }
        self.focus = Focus::Panes;
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
            KeyCode::Char('x') if matches!(self.focus, Focus::Mini(_)) => {
                if let Focus::Mini(i) = self.focus {
                    self.close_mini(i, sink).await?;
                }
            }
            // `Ctrl+B z`: peek — hide/show the whole minis row to see the full main area.
            KeyCode::Char('z') if !self.minis.is_empty() => {
                self.minis_hidden = !self.minis_hidden;
                if self.minis_hidden && matches!(self.focus, Focus::Mini(_)) {
                    self.focus = self.focus_return;
                }
                self.reconcile(sink).await?;
            }
            // `Ctrl+B -`: minimize/restore the focused mini (keeps it visible with status only).
            KeyCode::Char('-') if matches!(self.focus, Focus::Mini(_)) => {
                if let Focus::Mini(i) = self.focus {
                    if let Some(agent) = self.minis.get(i).copied() {
                        if !self.minimized.remove(&agent) {
                            self.minimized.insert(agent);
                        }
                        self.reconcile(sink).await?;
                    }
                }
            }
            // `Ctrl+B Enter`: promote the focused mini into the main area.
            KeyCode::Enter if matches!(self.focus, Focus::Mini(_)) => {
                if let Focus::Mini(i) = self.focus {
                    if let Some(agent) = self.minis.get(i).copied() {
                        self.activate(agent, sink).await?;
                    }
                }
            }
            KeyCode::Char('x') => {
                let closed = self.tree.focused_payload();
                self.tree.close();
                // Closing a shell pane kills that shell; closing the primary just stops viewing
                // it (the agent keeps running and reopens later). Reconcile only detaches.
                if let Some(t) = closed {
                    if !self.is_primary(t) {
                        sink.send(ClientMsg::CloseTerminal { terminal: t }).await?;
                        self.terminals.remove(&t);
                        self.parsers.remove(&t);
                        self.attached.remove(&t);
                        self.passthrough.remove(&t);
                    }
                }
                if self.tree.is_empty() {
                    self.focus = Focus::Sidebar;
                }
                self.reconcile(sink).await?;
            }
            KeyCode::Char('r') if !self.tree.is_empty() => self.resize_mode = true,
            // Enter scroll (copy) mode on the focused pane — tmux `Ctrl+B [`.
            KeyCode::Char('[') if self.focus == Focus::Panes => {
                if let Some(t) = self.tree.focused_payload() {
                    // A full-screen app (vim, less, and possibly the agent itself) runs on the
                    // alternate screen, which has no scrollback — there's nothing to scroll here,
                    // so say so rather than entering a mode where the keys do nothing.
                    let alt = self
                        .parsers
                        .get(&t)
                        .is_some_and(|p| p.screen().alternate_screen());
                    if alt {
                        self.info =
                            "no scrollback here — this pane runs a full-screen app".to_string();
                    } else {
                        self.scroll_mode = Some(t);
                        self.apply_scroll(t, 0);
                    }
                }
            }
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

    /// Scroll (copy) mode keys — vi-style, matching tmux `mode-keys vi`: j/k line, Ctrl-u/Ctrl-d
    /// half-page, PageUp/PageDown page, g/G top/bottom, q/Esc/Enter to exit.
    fn key_scroll(&mut self, key: KeyEvent, terminal: TerminalId) -> Flow {
        let page = self
            .attached
            .get(&terminal)
            .map(|s| s.rows as usize)
            .unwrap_or(24)
            .max(1);
        let half = (page / 2).max(1);
        let offset = self.scroll_offset;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let requested = match key.code {
            // Line: k/j, arrows, and less/vim's Ctrl-y / Ctrl-e.
            KeyCode::Char('k') | KeyCode::Up => offset.saturating_add(1),
            KeyCode::Char('j') | KeyCode::Down => offset.saturating_sub(1),
            KeyCode::Char('y') if ctrl => offset.saturating_add(1),
            KeyCode::Char('e') if ctrl => offset.saturating_sub(1),
            // Page.
            KeyCode::PageUp => offset.saturating_add(page),
            KeyCode::PageDown => offset.saturating_sub(page),
            // Half page: Ctrl-u/Ctrl-d (tmux vi) and plain u/d (less).
            KeyCode::Char('u') => offset.saturating_add(half),
            KeyCode::Char('d') => offset.saturating_sub(half),
            KeyCode::Char('g') => usize::MAX, // clamped to the buffer length below
            KeyCode::Char('G') => 0,
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                self.apply_scroll(terminal, 0); // back to live
                self.scroll_mode = None;
                return Flow::Continue;
            }
            _ => return Flow::Continue,
        };
        self.apply_scroll(terminal, requested);
        Flow::Continue
    }

    /// Set a terminal's scrollback view to `requested` rows back (clamped to its buffer), and
    /// remember the actual offset for the status/indicator.
    fn apply_scroll(&mut self, terminal: TerminalId, requested: usize) {
        if let Some(parser) = self.parsers.get_mut(&terminal) {
            parser.screen_mut().set_scrollback(requested);
            self.scroll_offset = parser.screen().scrollback();
        } else {
            self.scroll_offset = 0;
        }
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

    // --- mouse handling ---

    /// Route a mouse event to the pane under the cursor: left-click focuses it; the wheel is
    /// forwarded to an app that wants the mouse (Claude/vim/less), else it scrolls amux's own
    /// scrollback for that pane. Events over the sidebar are ignored.
    async fn on_mouse(&mut self, me: MouseEvent, sink: &mut Sink) -> Result<()> {
        // Drag/release drive the active selection, using its own pane — independent of what's now
        // under the cursor (so dragging past the pane edge still works).
        match me.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.head = clamp_to(sel.inner, me.column, me.row);
                }
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(text) = self.selection_text() {
                    if !text.trim().is_empty() {
                        copy_to_clipboard(&text);
                        self.info = format!("copied {} chars to clipboard", text.chars().count());
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        // Minis float over the panes, so a click/wheel over one targets the mini, not the pane.
        if let Some((i, inner)) = self.mini_at(me.column, me.row) {
            match me.kind {
                MouseEventKind::Down(MouseButton::Left) => self.enter_mini(i),
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let up = matches!(me.kind, MouseEventKind::ScrollUp);
                    let minimized = self
                        .minis
                        .get(i)
                        .is_some_and(|a| self.minimized.contains(a));
                    if let (false, Some(terminal)) = (minimized, self.mini_terminal(i)) {
                        if self.app_wants_mouse(terminal) {
                            if let Some(bytes) =
                                self.encode_wheel(terminal, up, me.column, me.row, inner)
                            {
                                sink.send(ClientMsg::Input { terminal, bytes }).await?;
                            }
                        } else {
                            self.wheel_scroll(terminal, up);
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        let Some((terminal, inner)) = self.pane_at(me.column, me.row) else {
            return Ok(());
        };
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.tree.focus_payload(terminal) {
                    self.focus = Focus::Panes;
                }
                // Start a pane-isolated selection — unless a full-screen mouse app owns the mouse.
                if self.on_alternate_screen(terminal) && self.app_wants_mouse(terminal) {
                    self.selection = None;
                } else {
                    let p = clamp_to(inner, me.column, me.row);
                    self.selection = Some(Selection {
                        terminal,
                        inner,
                        anchor: p,
                        head: p,
                    });
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(me.kind, MouseEventKind::ScrollUp);
                // An app that enabled mouse tracking (vim/less/htop, and Claude if it does) owns
                // the wheel; otherwise scroll amux's own scrollback (plain shells).
                if self.app_wants_mouse(terminal) {
                    if let Some(bytes) = self.encode_wheel(terminal, up, me.column, me.row, inner) {
                        sink.send(ClientMsg::Input { terminal, bytes }).await?;
                    }
                } else {
                    self.wheel_scroll(terminal, up);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The selected text, read from the pane's visible screen (reading order, trailing space
    /// trimmed, newline between rows). `None` if there's no selection.
    fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let screen = self.parsers.get(&sel.terminal)?.screen();
        let (start, end) = ordered(sel.anchor, sel.head);
        let right = sel.inner.x + sel.inner.width.saturating_sub(1);
        let mut out = String::new();
        for y in start.1..=end.1 {
            let c0 = if y == start.1 { start.0 } else { sel.inner.x };
            let c1 = if y == end.1 { end.0 } else { right };
            let mut line = String::new();
            for x in c0..=c1 {
                let contents = screen
                    .cell(y - sel.inner.y, x - sel.inner.x)
                    .map(|c| c.contents())
                    .unwrap_or_default();
                if contents.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(contents);
                }
            }
            while line.ends_with(' ') {
                line.pop();
            }
            out.push_str(&line);
            if y != end.1 {
                out.push('\n');
            }
        }
        Some(out)
    }

    /// The mini index (and its inner content area) under screen point `(col, row)`, if any. Minis
    /// float over the panes, so this is checked before `pane_at`.
    fn mini_at(&self, col: u16, row: u16) -> Option<(usize, Rect)> {
        let (_, minis_area) = self.regions();
        let ma = minis_area?;
        for (i, rect) in self.mini_rects(ma).iter().enumerate() {
            if col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom() {
                let inner = Rect {
                    x: rect.x + 1,
                    y: rect.y + 1,
                    width: rect.width.saturating_sub(2),
                    height: rect.height.saturating_sub(2),
                };
                return Some((i, inner));
            }
        }
        None
    }

    /// The terminal (and its inner content area) under screen point `(col, row)`, if any pane is.
    fn pane_at(&self, col: u16, row: u16) -> Option<(TerminalId, Rect)> {
        let (pane_area, _) = self.regions();
        for place in self.tree.layout(pane_area) {
            let r = place.rect;
            let hit = col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height;
            if let (true, Some(t)) = (hit, place.payload) {
                let inner = Rect {
                    x: r.x + 1,
                    y: r.y + 1,
                    width: r.width.saturating_sub(2),
                    height: r.height.saturating_sub(2),
                };
                return Some((t, inner));
            }
        }
        None
    }

    /// Whether the app in `terminal` has enabled mouse tracking (so it owns the wheel/clicks).
    fn app_wants_mouse(&self, terminal: TerminalId) -> bool {
        self.parsers
            .get(&terminal)
            .is_some_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
    }

    /// Whether the app in `terminal` is drawing on the alternate screen (a full-screen TUI with no
    /// scrollback of its own).
    fn on_alternate_screen(&self, terminal: TerminalId) -> bool {
        self.parsers
            .get(&terminal)
            .is_some_and(|p| p.screen().alternate_screen())
    }

    /// Encode a wheel tick as a mouse report in the app's requested encoding, with coordinates
    /// relative to the pane's inner area.
    fn encode_wheel(
        &self,
        terminal: TerminalId,
        up: bool,
        col: u16,
        row: u16,
        inner: Rect,
    ) -> Option<Vec<u8>> {
        let enc = self
            .parsers
            .get(&terminal)?
            .screen()
            .mouse_protocol_encoding();
        let cx = (col.saturating_sub(inner.x) + 1).clamp(1, inner.width.max(1));
        let cy = (row.saturating_sub(inner.y) + 1).clamp(1, inner.height.max(1));
        let button: u16 = if up { 64 } else { 65 }; // wheel up / down
        Some(match enc {
            vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{cx};{cy}M").into_bytes(),
            // X10 / UTF-8 default: ESC [ M, then button/col/row each offset by 32 (one byte).
            _ => vec![
                0x1b,
                b'[',
                b'M',
                (32 + button).min(255) as u8,
                (32 + cx).min(255) as u8,
                (32 + cy).min(255) as u8,
            ],
        })
    }

    /// Wheel-scroll amux's own scrollback for a pane whose app doesn't take the mouse — reusing
    /// scroll mode. Wheeling back to the bottom exits it (returns to live).
    fn wheel_scroll(&mut self, terminal: TerminalId, up: bool) {
        const STEP: usize = 3;
        if self.scroll_mode != Some(terminal) {
            self.scroll_mode = Some(terminal);
            self.scroll_offset = self
                .parsers
                .get(&terminal)
                .map(|p| p.screen().scrollback())
                .unwrap_or(0);
        }
        let requested = if up {
            self.scroll_offset.saturating_add(STEP)
        } else {
            self.scroll_offset.saturating_sub(STEP)
        };
        self.apply_scroll(terminal, requested);
        if !up && self.scroll_offset == 0 {
            self.scroll_mode = None; // back to live
        }
    }

    /// Attach/detach/resize terminals to match the **active** agent's layout. Terminals that
    /// aren't currently shown (another agent's workspace, or a just-hidden pane) are **detached**,
    /// not closed — they keep running headless in the daemon and restore when you switch back.
    /// Explicit closes (a shell pane via `Ctrl+B x`, a delete, an exit) kill terminals elsewhere.
    async fn reconcile(&mut self, sink: &mut Sink) -> Result<()> {
        let (pane_area, minis_area) = self.regions();
        let mut desired: HashMap<TerminalId, Size> = HashMap::new();
        for place in self.tree.layout(pane_area) {
            if let Some(t) = place.payload {
                desired.insert(t, pane_size(place.rect));
            }
        }
        // The minis row streams each agent's primary terminal, sized to its cell.
        if let Some(ma) = minis_area {
            let rects = self.mini_rects(ma);
            let mini_terms: Vec<(TerminalId, AgentId, Size)> = self
                .minis
                .iter()
                .enumerate()
                .filter(|(_, agent)| !self.minimized.contains(agent)) // minimized = status only
                .filter_map(|(i, agent)| {
                    let t = self
                        .agents
                        .iter()
                        .find(|a| a.id == *agent)?
                        .primary_terminal;
                    Some((t, *agent, pane_size(*rects.get(i)?)))
                })
                .collect();
            for (t, agent, size) in mini_terms {
                self.terminals.insert(t, agent);
                desired.insert(t, size);
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
            // Just stop streaming — the terminal belongs to a saved layout (or was explicitly
            // closed/killed already). Keep the terminal→agent mapping + passthrough for restore.
            sink.send(ClientMsg::Detach { terminal }).await?;
            self.attached.remove(&terminal);
            self.parsers.remove(&terminal);
        }

        // Persist the active agent's layout + the open minis so they survive closing the TUI.
        if let Some(agent) = self.active_agent {
            sink.send(ClientMsg::SetLayout {
                agent,
                layout: self.tree.to_layout(),
            })
            .await?;
        }
        sink.send(ClientMsg::SetMinis(self.minis.clone())).await?;
        sink.send(ClientMsg::SetActive(self.active_agent)).await?;
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
        match self.focus {
            Focus::Panes => {
                let terminal = self.tree.focused_payload()?;
                self.terminals.get(&terminal).copied()
            }
            Focus::Mini(i) => self.minis.get(i).copied(),
            Focus::Sidebar => None,
        }
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
/// Clamp a screen point into a rect's cell range.
fn clamp_to(inner: Rect, col: u16, row: u16) -> (u16, u16) {
    let x = col.clamp(inner.x, inner.x + inner.width.saturating_sub(1));
    let y = row.clamp(inner.y, inner.y + inner.height.saturating_sub(1));
    (x, y)
}

/// Order two points in reading order (top-to-bottom, then left-to-right).
fn ordered(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Reverse-video the selected cells of `sel` in the frame buffer (drawn over the pane's content).
fn highlight_selection(buf: &mut Buffer, sel: Selection) {
    let (start, end) = ordered(sel.anchor, sel.head);
    let right = sel.inner.x + sel.inner.width.saturating_sub(1);
    for y in start.1..=end.1 {
        let c0 = if y == start.1 { start.0 } else { sel.inner.x };
        let c1 = if y == end.1 { end.0 } else { right };
        for x in c0..=c1 {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

/// Copy `text` to the system clipboard via the OSC 52 terminal escape (dependency-free, works over
/// SSH; needs a terminal that honors OSC 52 — iTerm2, kitty, wezterm, tmux with `set-clipboard`).
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Minimal standard-alphabet base64 (no padding omitted) — avoids a dependency for OSC 52.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

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
    // Split the main area into the pane region and (when open) the minis row at the bottom.
    let (pane_area, minis_area) = app.regions();
    render_panes(frame, pane_area, app);
    if let Some(ma) = minis_area {
        render_minis(frame, ma, app);
    }
    render_status(frame, rows[1], app);
}

fn render_minis(frame: &mut Frame, area: Rect, app: &App) {
    let by_id: HashMap<_, _> = app.agents.iter().map(|a| (a.id, a)).collect();
    let rects = app.mini_rects(area);

    // A drop shadow around the whole floating group (its right column + bottom row, offset 1),
    // so the windows read as floating above the panes. Drawn first; the windows draw over it.
    if let (Some(left), Some(right)) = (
        rects.iter().map(|r| r.x).min(),
        rects.iter().map(|r| r.right()).max(),
    ) {
        let bottom = area.y + area.height; // one row below the band content (reserved margin)
        let shadow = Style::default().bg(Color::Black);
        let buf = frame.buffer_mut();
        // Right column: flush with the group's right edge, full height down to the bottom corner.
        for y in area.y..=bottom {
            if let Some(cell) = buf.cell_mut((right, y)) {
                cell.set_symbol(" ").set_style(shadow);
            }
        }
        // Bottom row: flush with the group's bottom edge, full width across to the corner.
        for x in left..=right {
            if let Some(cell) = buf.cell_mut((x, bottom)) {
                cell.set_symbol(" ").set_style(shadow);
            }
        }
    }

    for (i, rect) in rects.iter().enumerate() {
        let Some(agent_id) = app.minis.get(i) else {
            continue;
        };
        let focused = app.focus == Focus::Mini(i);
        let (glyph, color, branch) = by_id
            .get(agent_id)
            .map(|a| (a.state.glyph(), color_for(&a.state), a.branch.as_str()))
            .unwrap_or(('?', Color::DarkGray, "?"));
        let title = format!(" {glyph} {branch} ");
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
        let inner = block.inner(*rect);
        frame.render_widget(block, *rect);
        // Minimized minis show only their status (the terminal is detached to save bandwidth).
        if app.minimized.contains(agent_id) {
            let unread = by_id.get(agent_id).is_some_and(|a| a.unread);
            let dot = if unread { "\u{2022} " } else { "" };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {dot}{glyph}"),
                    Style::default().fg(color),
                ))),
                inner,
            );
            continue;
        }
        match app.mini_terminal(i).and_then(|t| app.parsers.get(&t)) {
            Some(parser) => frame.render_widget(PseudoTerminal::new(parser.screen()), inner),
            None => frame.render_widget(
                Paragraph::new("  \u{2026}").style(Style::default().fg(Color::DarkGray)),
                inner,
            ),
        }
    }
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
    // "Open" means visible right now: the active agent in the main area plus every mini. An agent
    // that was displaced from the main area keeps its saved layout but is sidebar-only — not open.
    let open: HashSet<AgentId> = app
        .active_agent
        .into_iter()
        .chain(app.minis.iter().copied())
        .collect();
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
        let (mut title, color) = match place.payload {
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
        // Show how far back this pane is scrolled while in scroll mode.
        if app.scroll_mode == place.payload && app.scroll_offset > 0 {
            title.push_str(&format!("\u{2191}{} ", app.scroll_offset));
        }
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
        // Draw the selection highlight on top of this pane's content.
        if let Some(sel) = app.selection {
            if Some(sel.terminal) == place.payload {
                highlight_selection(frame.buffer_mut(), sel);
            }
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
    } else if app.scroll_mode.is_some() {
        (
            format!(
                " SCROLL \u{2191}{} — j/k line \u{b7} ^u/^d half \u{b7} PgUp/PgDn page \u{b7} g/G top/bottom \u{b7} q done",
                app.scroll_offset
            ),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else if app.resize_mode {
        (
            " RESIZE — hjkl grow/shrink \u{b7} esc done".to_string(),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else if app.prefix {
        (
            " Ctrl+B — % / \" split \u{b7} x close \u{b7} HJKL/r resize \u{b7} [ scroll \u{b7} tab unread"
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
                " n new \u{b7} enter open \u{b7} m mini \u{b7} d del \u{b7} r resume \u{b7} P prune \u{b7} ctrl+hjkl \u{b7} ctrl+q quit"
            }
            Focus::Panes => {
                " ctrl+hjkl move \u{b7} ctrl+b %/\"/x/r \u{b7} type to talk \u{b7} ctrl+q quit"
            }
            Focus::Mini(_) => {
                " mini \u{b7} ctrl+hjkl \u{b7} ctrl+b: enter promote \u{b7} - min \u{b7} z peek \u{b7} x close"
            }
        };
        (hint.to_string(), Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(Paragraph::new(Line::from(text)).style(style), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn scroll_mode_moves_and_clamps_within_scrollback() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        // A tiny 4-row viewport with plenty of history to scroll through.
        let mut parser = vt100::Parser::new(4, 20, 100);
        for i in 0..50 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        app.parsers.insert(t, parser);
        app.attached.insert(t, Size { cols: 20, rows: 4 });
        app.scroll_mode = Some(t);
        app.apply_scroll(t, 0);
        assert_eq!(app.scroll_offset, 0);

        // k scrolls up a line; Ctrl-u scrolls a half page (rows/2 = 2).
        app.key_scroll(key(KeyCode::Char('k')), t);
        assert_eq!(app.scroll_offset, 1);
        app.key_scroll(ctrl('u'), t);
        assert_eq!(app.scroll_offset, 3);

        // g jumps to the top of the scrollback; further up is clamped there.
        app.key_scroll(key(KeyCode::Char('g')), t);
        let top = app.scroll_offset;
        assert!(top > 3, "g reaches the top of history");
        app.key_scroll(key(KeyCode::Char('k')), t);
        assert_eq!(app.scroll_offset, top, "clamped at the top");

        // G returns to live; q exits scroll mode.
        app.key_scroll(key(KeyCode::Char('G')), t);
        assert_eq!(app.scroll_offset, 0);
        app.key_scroll(key(KeyCode::Char('q')), t);
        assert!(app.scroll_mode.is_none());
    }

    fn agent_info(primary: TerminalId) -> AgentInfo {
        AgentInfo {
            id: AgentId::new(),
            repo: amux_core::agent::RepoId::from_canonical_path(std::path::Path::new("/r")),
            name: "a".into(),
            branch: "b".into(),
            state: AgentState::Working,
            last_activity: Utc::now(),
            unread: false,
            primary_terminal: primary,
        }
    }

    #[test]
    fn each_agent_keeps_its_own_workspace() {
        let mut app = App::new(100, 40);
        let (pa, pb) = (TerminalId::new(), TerminalId::new());
        let a = agent_info(pa);
        let b = agent_info(pb);
        let (ida, idb) = (a.id, b.id);
        app.agents = vec![a, b];

        // Open A → its primary shows; split off a shell in A's workspace.
        app.swap_to_agent(ida);
        assert_eq!(app.tree.payloads(), vec![pa]);
        let sh = TerminalId::new();
        app.terminals.insert(sh, ida);
        app.tree.split(Axis::LeftRight);
        app.tree.open(sh);
        assert_eq!(app.tree.payloads().len(), 2);

        // Switch to B → the main area is B's primary only; A's terminals are not present.
        app.swap_to_agent(idb);
        assert_eq!(app.tree.payloads(), vec![pb]);
        assert!(!app.tree.payloads().contains(&pa));

        // Switch back to A → its two-pane split (primary + shell) is restored.
        app.swap_to_agent(ida);
        let payloads = app.tree.payloads();
        assert_eq!(payloads.len(), 2);
        assert!(payloads.contains(&pa) && payloads.contains(&sh));
    }

    #[test]
    fn minis_form_a_navigable_bottom_row() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        app.active_agent = Some(AgentId::new());
        app.focus = Focus::Panes;
        app.minis = vec![AgentId::new(), AgentId::new()];

        // Off the bottom of the panes drops into the first mini; right steps across the row.
        app.navigate(Dir::Down);
        assert_eq!(app.focus, Focus::Mini(0));
        app.navigate(Dir::Right);
        assert_eq!(app.focus, Focus::Mini(1));
        // Up climbs back into the main layout.
        app.navigate(Dir::Up);
        assert_eq!(app.focus, Focus::Panes);
        // Left off the leftmost mini lands in the sidebar.
        app.focus = Focus::Mini(0);
        app.navigate(Dir::Left);
        assert_eq!(app.focus, Focus::Sidebar);

        // Two minis sit adjacent, right-anchored to the (inset) minis band.
        let (_, minis_area) = app.regions();
        let band = minis_area.unwrap();
        let rects = app.mini_rects(band);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].x + rects[0].width, rects[1].x);
        assert_eq!(rects[1].x + rects[1].width, band.x + band.width);

        // A click inside a mini hit-tests to it (they float over the panes); the top of the main
        // area (over the panes) hits no mini.
        assert_eq!(
            app.mini_at(rects[1].x + 1, rects[1].y + 1).map(|(i, _)| i),
            Some(1)
        );
        assert_eq!(app.mini_at(app.area.x + 1, app.area.y), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn selection_extracts_pane_text() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        let mut parser = vt100::Parser::new(6, 20, 100);
        parser.process(b"alpha\r\nbravo\r\ncharlie\r\n");
        app.parsers.insert(t, parser);
        // Pane inner area starting at (1,1): select rows 0..1, from col 0 across the words.
        let inner = Rect::new(1, 1, 18, 6);
        app.selection = Some(Selection {
            terminal: t,
            inner,
            anchor: (1, 1), // 'a' of alpha (screen col 1 = pane col 0, row 1 = pane row 0)
            head: (1 + 4, 1 + 1), // 'o' of bravo (pane row 1, col 4)
        });
        assert_eq!(app.selection_text().unwrap(), "alpha\nbravo");
    }

    #[test]
    fn mouse_wheel_forwards_to_apps_and_scrolls_others() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        app.terminals.insert(t, AgentId::new());
        let mut parser = vt100::Parser::new(4, 20, 100);
        for i in 0..30 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        app.parsers.insert(t, parser);
        app.attached.insert(t, Size { cols: 20, rows: 4 });

        // Hit-testing finds the pane covering the main area.
        let (hit, _inner) = app.pane_at(40, 10).expect("mouse is over the pane");
        assert_eq!(hit, t);

        // No mouse mode → the wheel scrolls amux's own scrollback; wheeling back exits.
        assert!(!app.app_wants_mouse(t));
        app.wheel_scroll(t, true);
        assert!(app.scroll_offset >= 3 && app.scroll_mode == Some(t));
        app.wheel_scroll(t, false);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.scroll_mode.is_none());

        // The app enables SGR mouse mode → the wheel is forwarded as an SGR report instead.
        app.parsers
            .get_mut(&t)
            .unwrap()
            .process(b"\x1b[?1000h\x1b[?1006h");
        assert!(app.app_wants_mouse(t));
        let bytes = app
            .encode_wheel(t, true, 35, 5, Rect::new(31, 1, 18, 2))
            .unwrap();
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.starts_with("\u{1b}[<64;") && s.ends_with('M'),
            "SGR wheel-up report: {s:?}"
        );
    }

    #[test]
    fn scroll_view_stays_anchored_as_output_arrives() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        let mut parser = vt100::Parser::new(4, 20, 100);
        for i in 0..30 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        app.parsers.insert(t, parser);
        app.attached.insert(t, Size { cols: 20, rows: 4 });
        app.scroll_mode = Some(t);
        app.apply_scroll(t, 10); // scroll 10 rows back

        let before = app.parsers[&t].screen().contents();
        let offset_before = app.scroll_offset;

        // New output arrives — mirror the Output handler: process, then resync the cached offset.
        {
            let p = app.parsers.get_mut(&t).unwrap();
            for i in 30..35 {
                p.process(format!("line {i}\r\n").as_bytes());
            }
        }
        app.scroll_offset = app.parsers[&t].screen().scrollback();

        assert_eq!(
            before,
            app.parsers[&t].screen().contents(),
            "the scrolled view stays on the same lines as output arrives"
        );
        assert_eq!(
            app.scroll_offset,
            offset_before + 5,
            "the cached offset tracks the anchor (5 new lines)"
        );
    }
}

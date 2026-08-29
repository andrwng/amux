//! The TUI: a repo-grouped agent sidebar beside a tmux-style tiled pane area. Panes stream
//! **terminals**; the sidebar lists **agents** (workspaces) grouped by **repo**. Splitting a pane
//! spawns a `$SHELL` in the same worktree. Focus is spatial — `Ctrl+hjkl` moves between panes and
//! into/out of the sidebar; `Ctrl+B` is a prefix (`%`/`"` split, `x` close, `r` resize, digit
//! opens that sidebar agent, `-` opens the previous agent). `n` creates an agent in the selected
//! repo, `N` in a repo given by path. `Ctrl+Q` quits. Arming the `Ctrl+B` prefix reveals a numeric
//! overlay on the sidebar; the next digit opens that agent's session (`1`..`9`, `0` = tenth),
//! mirroring tmux's `prefix` + N, and the previous agent's row is marked `-` (tmux's last-window).
//! `Cmd`+digit does the same on terminals that natively report the SUPER modifier (we don't force
//! the Kitty protocol — it would break Shift and slow typing).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode,
    MouseButton, MouseEvent, MouseEventKind,
};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use tui_term::widget::PseudoTerminal;

use amux_core::agent::{sort_for_sidebar, AgentId, AgentState, RepoId, RosterItem, TerminalId};
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, ProtoError, RepoInfo, Size};

use crate::input::{encode_paste, key_to_bytes};
use crate::pane::{Axis, Dir, Nav, PaneTree};
use crate::theme::Theme;

/// Full sidebar width: cursor + unread bar + glyph + open marker + name + age.
const SIDEBAR_W_FULL: u16 = 30;
/// Minimized sidebar width for narrow terminals: cursor + unread bar + glyph + a few name chars.
const SIDEBAR_W_MIN: u16 = 12;
/// The narrowest pane an agent CLI can lay its own UI out in: a boxed prompt, a diff and a tool
/// call all assume roughly this much, and below it every line wraps. Not a floor on a pane — a
/// shell pane is fine narrower, and `pane::MIN_PANE_W` is that floor. This is what chrome yields to.
const AGENT_UI_W_MIN: u16 = 60;
/// Keep at least this many columns of main area: when a full sidebar would squeeze the panes
/// below this, the sidebar minimizes to the rail instead. The panes are the workspace, the sidebar
/// is chrome, so the chrome goes first — `AGENT_UI_W_MIN` plus the pane's two border cells.
const MAIN_W_MIN: u16 = AGENT_UI_W_MIN + 2;

/// Agents in the "recent" block at the top of the sidebar. Fixed: the block is a shortcut, and a
/// fourth row costs more than the fourth-most-recent agent is worth.
const RECENT_MIN: usize = 5;
/// …and anything opened inside this window is recent, so a busy day is represented rather than cut
/// off at the floor.
const RECENT_WINDOW: chrono::Duration = chrono::Duration::hours(24);
/// …but never more than this, whatever the window says. A shortcut you have to scan is not a
/// shortcut, and ten is also where the numeric shortcuts run out (`1`..`9`, `0`).
const RECENT_MAX: usize = 10;
/// Rows of repo-grouped roster the block must leave visible: a repo header plus a few agents.
const ROSTER_MIN_ROWS: usize = 5;

/// How many agents the recent block holds: the last `RECENT_MIN` opened, **plus** everything opened
/// inside `RECENT_WINDOW`, capped at `RECENT_MAX`.
///
/// A union of the two rules, which collapses to a count because both cut on the same key: sorted by
/// `last_opened` descending, an agent inside the window cannot sit below one outside it, so the
/// union is always a prefix of that order and `max` of the two sizes names it. The floor wins over
/// the window, the ceiling wins over both, and the agent count wins over everything.
fn recent_count(agents: &[AgentInfo], now: DateTime<Utc>) -> usize {
    let in_window = agents
        .iter()
        .filter(|a| now.signed_duration_since(a.last_opened) < RECENT_WINDOW)
        .count();
    in_window.clamp(RECENT_MIN, RECENT_MAX).min(agents.len())
}

/// The agents to list in the recent block: global MRU by `last_opened`, newest first, as many as
/// [`recent_count`] allows.
///
/// Deliberately *not* `sort_for_sidebar`, which floats blocked agents to the top — under that order
/// "recent" would mean "recent, unless something is waiting", which is the roster's job, not this
/// block's. The point of the block is the one thing the roster cannot say: recency **across** repos,
/// since the roster is grouped by repo and only MRU within a group.
fn recent_ids(agents: &[AgentInfo], now: DateTime<Utc>) -> Vec<AgentId> {
    let n = recent_count(agents, now);
    let mut by_recency: Vec<&AgentInfo> = agents.iter().collect();
    // `last_activity` breaks ties (`AgentId` is not ordered); a stable sort keeps the rest as-is.
    by_recency.sort_by(|a, b| {
        b.last_opened
            .cmp(&a.last_opened)
            .then(b.last_activity.cmp(&a.last_activity))
    });
    by_recency.into_iter().take(n).map(|a| a.id).collect()
}

/// Whether the recent block earns its rows. Every guard exists to keep it from being noise:
///
/// - One repo with agents: that group is already MRU, so the block would copy its own top.
/// - It holds every agent there is: then it *is* the list, printed twice. This is the guard that
///   matters now that the 24-hour window is unbounded — a day spent touching everything would
///   otherwise double the sidebar.
/// - A sidebar too short to leave the roster room: the roster is the thing being navigated. The
///   block costs its agents plus a header and a divider, and `ROSTER_MIN_ROWS` is what must be left
///   over — a repo header and a few agents under it, or the block has crowded out the list it is a
///   shortcut into.
/// - The minimized rail: no width for the `repo/branch` names that make the block readable.
fn show_recent(
    repos_with_agents: usize,
    agents: usize,
    recent: usize,
    body_height: usize,
    minimized: bool,
) -> bool {
    !minimized
        && repos_with_agents > 1
        && recent < agents
        && body_height >= recent + 2 + ROSTER_MIN_ROWS
}

/// How many rows of the sidebar list a wheel notch scrolls, matching the pane wheel.
const SIDEBAR_WHEEL_ROWS: usize = 3;

/// A stored `sidebar_top` clamped to what the list can actually show: never past the last
/// screenful, and never non-zero for a list that fits. Applied at render, so a top left behind by
/// agents deleted underneath it corrects itself instead of showing a blank sidebar.
fn clamp_top(top: usize, len: usize, height: usize) -> usize {
    top.min(len.saturating_sub(height))
}

/// `top` moved the *minimum* distance that puts row `sel` on screen. Minimal, not centred, so `j`
/// at the bottom edge scrolls one row and the list doesn't jump under the cursor.
fn top_showing(top: usize, sel: usize, height: usize) -> usize {
    // A zero-height sidebar has no window to speak of; treat it as one row so the selection still
    // anchors the view instead of the arithmetic running past it.
    let height = height.max(1);
    if sel < top {
        sel
    } else {
        top.max(sel + 1 - height.min(sel + 1))
    }
}

/// The sidebar's width for a given total terminal width: full, unless that would leave the main
/// area under `MAIN_W_MIN` columns. The single source of truth shared by `main_area` (which
/// drives the pane region and mouse hit-testing) and `render`'s layout — the two must agree or
/// panes and clicks misalign.
fn sidebar_width(total_cols: u16) -> u16 {
    if total_cols < SIDEBAR_W_FULL + MAIN_W_MIN {
        SIDEBAR_W_MIN
    } else {
        SIDEBAR_W_FULL
    }
}

/// Floor for a full mini's width — today's historical fixed width, so minis never get
/// narrower than before.
const MINI_W_MIN: u16 = 44;
/// Cap for a full mini's width — a classic full-terminal width, so a mini stays a peek even
/// on an ultrawide screen.
const MINI_W_MAX: u16 = 80;

/// A full mini pane's width: half the available band width, clamped to `[MINI_W_MIN,
/// MINI_W_MAX]`. Pure and count-independent — the single source used by `mini_rects` so
/// rendering, hit-testing, and PTY sizing all agree.
fn mini_width(available: u16) -> u16 {
    (available / 2).clamp(MINI_W_MIN, MINI_W_MAX)
}

const RESIZE_STEP: f32 = 0.05;
/// Max height of the minis row (capped to half the main area).
const MINI_ROWS: u16 = 14;
/// How often the loop wakes up on its own, with no input, purely to redraw: the sidebar's
/// per-agent age (`age_short`) is computed from a live clock, so a quiet screen would otherwise
/// freeze it (e.g. "45s ago" staying "45s" forever). Coarse enough to be free.
const AGE_TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Two left-presses on the same cell within this window are a double-click (token select + copy).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

type Sink = SplitSink<Framed<UnixStream, ClientCodec>, ClientMsg>;

/// One rendered line of the sidebar. **Exactly one line each** — the viewport indexes rows while
/// `render_sidebar` slices lines, so a row that drew two lines (or none) would pull the two spaces
/// apart and scroll the selection off-screen. That is why the dim trailer lines are rows too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    /// The "recent" label above the MRU block.
    RecentHeader,
    /// An agent in the MRU block — the same agent also appears under its repo below.
    Recent(AgentId),
    /// The rule between the recent block and the repo groups.
    Divider,
    Repo(RepoId),
    /// The "no agents — press n" hint under an empty repo header.
    EmptyRepo(RepoId),
    Agent(AgentId),
    /// The dim message under a blocked agent, when it has one.
    Attention(AgentId),
    /// The "no repos yet…" placeholder, the only row of a sidebar with nothing in it.
    NoRepos,
}

impl Row {
    /// Whether the cursor can land here. The trailer rows are context for the row above, so `j`/`k`
    /// step over them.
    fn selectable(&self) -> bool {
        matches!(self, Row::Repo(_) | Row::Agent(_) | Row::Recent(_))
    }
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
    /// Whether this is a committed selection — a real drag gesture (even one confined to the
    /// anchor cell) or a double-click token — as distinct from a bare click. Gates the highlight
    /// and the copy. A plain click (press with no drag) stays false: no highlight, no copy.
    active: bool,
}

impl Selection {
    fn is_active(&self) -> bool {
        self.active
    }
}

/// Which field a two-field create prompt is editing: `dir`+`branch` for `N` (new repo by path),
/// `branch`+`task` for `n` (dispatch into the selected repo).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Dir,
    Branch,
    /// The task to dispatch the agent on — optional; empty means "launch it idle at its prompt".
    Task,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InputMode {
    Normal,
    /// `n` — branch-only, into the repo under the cursor (`create_repo`).
    Creating,
    /// `N` — two fields (directory + branch); registers the repo by path.
    CreatingRepo,
    /// `H` — one field (directory); registers the repo by path and opens its HEAD session. A HEAD
    /// session has no branch and takes no task, so the directory is the whole prompt.
    CreatingHead,
    Confirming,
}

enum Flow {
    Continue,
    Quit,
}

/// Scrollback kept by a client-side parser: none. History lives in the daemon and is paged in on
/// demand ([`Scroll`]), so a client holding its own ring would be a second, staler copy of it — and
/// an expensive one, at `cols` cells of 32 bytes per line per attached pane.
const CLIENT_SCROLLBACK: usize = 0;

/// A pane scrolled back into its history — one screenful, as served by the daemon.
///
/// The daemon owns scroll history *and* this client's position in it, so scrolling is a step
/// ("one line back from what you showed me") answered with a window to render. That keeps the
/// client a projection (`DESIGN.md` §2, invariant 2): it holds no scrollback, cannot scroll past
/// what exists, and cannot disagree with the daemon about where the view sits.
struct Scroll {
    terminal: TerminalId,
    /// The served window, parsed for rendering. Deliberately has no scrollback of its own — it is
    /// exactly what the daemon sent, nothing more.
    parser: vt100::Parser,
    /// Lines back from live, and how far back history goes; both straight from the daemon, for the
    /// `↑offset/available` indicator.
    offset: usize,
    available: usize,
}

/// The profile actually used: the `--profile` flag if given, else the config's `profile`
/// (which itself defaults to `Blue`). Encodes the precedence CLI > config > default.
fn effective_profile(
    cli: Option<amux_core::config::Profile>,
    config: amux_core::config::Profile,
) -> amux_core::config::Profile {
    cli.unwrap_or(config)
}

pub async fn run(profile: Option<amux_core::config::Profile>) -> Result<()> {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    };
    let (framed, repo) = crate::client::connect().await?;
    let config = amux_core::config::Config::load()?;
    let theme = Theme::for_profile(effective_profile(profile, config.profile));
    let mut terminal = ratatui::init();
    // Capture the mouse so the wheel reaches panes (forwarded to apps that want it, else scrolls
    // the daemon's scroll history). Hold Shift to bypass for native terminal selection.
    // Bracketed paste lets the outer terminal hand us a paste as one `Event::Paste` instead of a
    // storm of per-character key events — one write to the child, one redraw. See `on_paste`.
    //
    // We deliberately do NOT push the Kitty keyboard-enhancement flags. Enabling
    // REPORT_ALL_KEYS_AS_ESCAPE_CODES is the only way to observe a lone Cmd/Super press (for the
    // numeric-overlay hold), but it reroutes *all* input through `CSI u`: shifted characters then
    // arrive as base-codepoint + SHIFT (crossterm only substitutes the real glyph with the extra
    // REPORT_ALTERNATE_KEYS flag), so our PTY bridge would send lowercase; and REPORT_EVENT_TYPES
    // doubles every keystroke into press+release. The blast radius on typing is not worth a
    // Cmd-hold that most macOS terminals won't forward anyway. The `Ctrl+B <digit>` jump needs
    // none of this; `Cmd+digit` still works on any terminal that natively reports the SUPER
    // modifier without us forcing the mode.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut app = App::new(cols, rows);
    app.theme = theme;
    let (mut sink, mut stream) = framed.split();
    let mut events = EventStream::new();

    let result = event_loop(
        &mut app,
        &mut sink,
        &mut stream,
        &mut events,
        |app| draw(&mut terminal, app),
        repo,
    )
    .await;

    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

/// Pull every item already ready on `stream` into `out`, without awaiting new I/O. Returns
/// `true` iff the stream has ended (yielded `None`); `false` if it is merely out of ready items
/// for now (still open). This is the coalescing primitive: one call collects a whole burst so
/// the caller can render it in a single pass. See `docs/superpowers/specs/2026-07-15-coalesced-draw-design.md`.
async fn drain_ready<S, T>(stream: &mut S, out: &mut Vec<T>) -> bool
where
    S: futures::Stream<Item = T> + Unpin,
{
    loop {
        // Poll the stream exactly once, resolving immediately whether it is Ready or Pending —
        // never parking the task on new I/O.
        let polled =
            futures::future::poll_fn(|cx| std::task::Poll::Ready(stream.poll_next_unpin(cx))).await;
        match polled {
            std::task::Poll::Ready(Some(item)) => out.push(item),
            std::task::Poll::Ready(None) => return true, // stream ended
            std::task::Poll::Pending => return false,    // nothing more ready right now
        }
    }
}

/// The client event loop: block for the first event, then apply everything else already queued
/// on both sources (coalescing a burst), and redraw once per batch. `stream` carries daemon
/// messages, `events` the terminal input; `render` draws the current view model (injected so
/// tests can drive the loop headless and count renders). Generic over the two stream types for
/// the same reason.
async fn event_loop<S, E, R>(
    app: &mut App,
    sink: &mut Sink,
    stream: &mut S,
    events: &mut E,
    mut render: R,
    repo: PathBuf,
) -> Result<()>
where
    S: futures::Stream<Item = Result<DaemonMsg, ProtoError>> + Unpin,
    E: futures::Stream<Item = std::io::Result<Event>> + Unpin,
    R: FnMut(&App) -> Result<()>,
{
    // Register this client's repo with the (possibly shared) daemon so its agents show up here.
    sink.send(ClientMsg::AddRepo { path: repo }).await?;

    // Ticks every AGE_TICK_INTERVAL so the sidebar's age column keeps advancing even when the
    // screen is otherwise quiet (see AGE_TICK_INTERVAL). `interval_at` schedules the first tick
    // one full interval out, rather than immediately (tokio::time::interval's default), so it
    // never forces an extra render right after the loop starts.
    let mut age_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + AGE_TICK_INTERVAL,
        AGE_TICK_INTERVAL,
    );

    render(app)?;
    loop {
        // Phase 1 — block until one source is ready.
        let mut quit = matches!(
            tokio::select! {
                msg = stream.next() => app.handle_daemon(msg.and_then(|r| r.ok()), sink).await?,
                ev  = events.next() => app.handle_event(ev.and_then(|r| r.ok()), sink).await?,
                // No state to apply — just fall through to Phase 2/the render below so the age
                // column redraws with a fresh "now".
                _ = age_tick.tick() => Flow::Continue,
            },
            Flow::Quit
        );

        // Phase 2 — drain everything else already queued on both sources, without blocking.
        if !quit {
            let mut dmsgs = Vec::new();
            quit |= drain_ready(stream, &mut dmsgs).await; // did the daemon stream end?
            for m in dmsgs {
                if let Flow::Quit = app.handle_daemon(m.ok(), sink).await? {
                    quit = true;
                }
            }
            let mut evs = Vec::new();
            quit |= drain_ready(events, &mut evs).await; // did the event stream end?
            for e in evs {
                if let Flow::Quit = app.handle_event(e.ok(), sink).await? {
                    quit = true;
                }
            }
        }

        if quit {
            break; // teardown — no trailing render
        }
        // Report focus changes so the daemon can track read/unread.
        app.sync_focus(sink).await?;
        render(app)?; // exactly one render per drained batch
    }
    Ok(())
}

fn main_area(cols: u16, rows: u16) -> Rect {
    let sw = sidebar_width(cols);
    Rect::new(
        sw,
        0,
        cols.saturating_sub(sw).max(1),
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
    /// Whether anything has *deliberately* placed the sidebar cursor — a keypress, a shortcut, a
    /// freshly created agent. Until then the cursor is only parked wherever the roster so far
    /// allowed, and `ensure_sidebar_sel` re-homes it as better rows arrive: the daemon sends repos
    /// before agents, so the first roster is bare headers and the recent block does not exist yet.
    sidebar_placed: bool,
    /// First visible sidebar row. Stored rather than derived from the selection so a wheel scroll
    /// survives a redraw; moving the selection pulls it back (`scroll_sel_into_view`).
    sidebar_top: usize,
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
    /// The agent that was in the main area before [`active_agent`] — the target of the
    /// `Ctrl+B -` "jump to previous" shortcut (tmux's last-window). `None` until a second agent
    /// has been opened. Toggles back and forth as you swap, and is cleared if that agent is removed.
    prev_active_agent: Option<AgentId>,
    terminals: HashMap<TerminalId, AgentId>,
    parsers: HashMap<TerminalId, vt100::Parser>,
    attached: HashMap<TerminalId, Size>,
    /// Terminals whose foreground app wants `Ctrl+hjkl` (vim-like), so we pass those keys through
    /// instead of navigating. Announced by the daemon via `TerminalApp`.
    passthrough: HashMap<TerminalId, bool>,
    focus: Focus,
    /// The active border/accent color scheme (from `--profile` / config; default `blue`).
    theme: Theme,
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
    /// Cmd/Super is currently held (tracked from Kitty key press/release events). Shows the
    /// numeric-shortcut overlay while down.
    super_held: bool,
    resize_mode: bool,
    /// Scroll (copy) mode: the window of history the daemon last served us. `None` = live.
    scroll: Option<Scroll>,
    /// The active mouse selection (drag in progress or last completed), if any.
    selection: Option<Selection>,
    /// Last left-press (time, col, row, pane) — the first half of a possible double-click.
    last_click: Option<(Instant, u16, u16, TerminalId)>,
    /// Branch buffer (both `n` and `N`); repo-path buffer (`N` only); task buffer (`n` only).
    create_buf: String,
    dir_buf: String,
    task_buf: String,
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
            sidebar_placed: false,
            sidebar_top: 0,
            tree: PaneTree::new(),
            trees: HashMap::new(),
            saved_layouts: HashMap::new(),
            active_agent: None,
            prev_active_agent: None,
            terminals: HashMap::new(),
            parsers: HashMap::new(),
            attached: HashMap::new(),
            passthrough: HashMap::new(),
            focus: Focus::Sidebar,
            theme: Theme::default(),
            minis: Vec::new(),
            minimized: HashSet::new(),
            minis_hidden: false,
            focus_return: Focus::Sidebar,
            focus_agent: None,
            input: InputMode::Normal,
            prefix: false,
            super_held: false,
            resize_mode: false,
            scroll: None,
            selection: None,
            last_click: None,
            create_buf: String::new(),
            dir_buf: String::new(),
            task_buf: String::new(),
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
        const MIN_W: u16 = 12;
        let full_w = mini_width(area.width);
        let widths: Vec<u16> = self
            .minis
            .iter()
            .map(|a| {
                if self.minimized.contains(a) {
                    MIN_W
                } else {
                    full_w
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

    /// Apply one daemon message. `None` means the daemon stream ended or errored (the daemon
    /// went away) → stop the loop.
    async fn handle_daemon(&mut self, msg: Option<DaemonMsg>, sink: &mut Sink) -> Result<Flow> {
        match msg {
            Some(dm) => {
                self.on_daemon(dm, sink).await?;
                Ok(Flow::Continue)
            }
            None => Ok(Flow::Quit),
        }
    }

    /// Apply one terminal event. `None` means the event stream ended or errored → stop the loop.
    async fn handle_event(&mut self, ev: Option<Event>, sink: &mut Sink) -> Result<Flow> {
        match ev {
            // A lone Cmd/Super key (reported only under the Kitty protocol) toggles the
            // numeric-overlay hold on both its press and release edges — it is never PTY input,
            // so it never reaches `on_key`. Repeat counts as still-held.
            Some(Event::Key(key))
                if matches!(
                    key.code,
                    KeyCode::Modifier(ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper)
                ) =>
            {
                self.super_held = key.kind != KeyEventKind::Release;
                Ok(Flow::Continue)
            }
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                self.on_key(key, sink).await
            }
            Some(Event::Resize(c, r)) => {
                self.on_resize(c, r, sink).await?;
                Ok(Flow::Continue)
            }
            Some(Event::Mouse(me)) => {
                self.on_mouse(me, sink).await?;
                Ok(Flow::Continue)
            }
            Some(Event::Paste(text)) => {
                self.on_paste(text, sink).await?;
                Ok(Flow::Continue)
            }
            Some(_) => Ok(Flow::Continue),
            None => Ok(Flow::Quit),
        }
    }

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
                        let restored = self.swap_to_agent(id);
                        self.spawn_restored_shells(id, restored, sink).await?;
                        self.reconcile(sink).await?;
                    }
                }
            }
            DaemonMsg::Previous(prev) => {
                // Seed the jump-to-previous target from persistence. Sent after `Active`, so the
                // main-pane restore (which runs `swap_to_agent` and may set `prev_active_agent`)
                // has already happened and this persisted value wins. Reconcile so the daemon's
                // copy re-converges to it — the earlier `Minis`/`Active` reconciles pushed a stale
                // `None` before this seed arrived (the same converge dance `Active` relies on).
                self.prev_active_agent = prev.filter(|id| self.agents.iter().any(|a| a.id == *id));
                self.reconcile(sink).await?;
            }
            DaemonMsg::AgentAdded(info) => {
                // Select the freshly-created agent so the next Enter opens it.
                let id = info.id;
                self.agents.push(info);
                self.place_sidebar_sel(Row::Agent(id));
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
                // Don't let the "jump to previous" target point at a ghost.
                if self.prev_active_agent == Some(id) {
                    self.prev_active_agent = None;
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
            DaemonMsg::OpenedChanged { id, at } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == id) {
                    agent.last_opened = at;
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
                    let mut parser = vt100::Parser::new(size.rows, size.cols, CLIENT_SCROLLBACK);
                    parser.process(&bytes);
                    self.parsers.insert(terminal, parser);
                }
            }
            DaemonMsg::Output { terminal, bytes } => {
                if let Some(parser) = self.parsers.get_mut(&terminal) {
                    parser.process(&bytes);
                }
                // A scrolled-back pane keeps showing the window it was served; live output lands in
                // the parser above, ready for when the view returns to live. The daemon re-bases the
                // position on the next step, so output arriving now doesn't move what's on screen.
            }
            DaemonMsg::ScrollView {
                terminal,
                offset,
                available,
                bytes,
            } => self.on_scroll_view(terminal, offset, available, &bytes),
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
            InputMode::CreatingHead => return self.key_creating_head(key, sink).await,
            InputMode::Confirming => return self.key_confirm(key, sink).await,
            InputMode::Normal => {}
        }
        if let Some(terminal) = self.scroll.as_ref().map(|s| s.terminal) {
            if self.parsers.contains_key(&terminal) {
                return self.key_scroll(key, terminal, sink).await;
            }
            self.scroll = None; // the pane went away
        }
        if self.resize_mode {
            return self.key_resize(key, sink).await;
        }
        if self.prefix {
            self.prefix = false;
            return self.key_prefix(key, sink).await;
        }
        // `Cmd+digit` (Super held, reported under the Kitty protocol) opens the agent directly. We
        // own every Cmd+digit — even one past the last agent — so it never leaks a digit to the
        // PTY; other Cmd combos fall through untouched.
        if key.modifiers.contains(KeyModifiers::SUPER) {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_digit() {
                    if let Some(id) = self.numbered_agent(c) {
                        self.open_agent(id, sink).await?;
                    }
                    return Ok(Flow::Continue);
                }
            }
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
            // In the sidebar, Ctrl+j/Ctrl+k jump the selection to the next unread agent (down/up)
            // rather than a spatial move — there's nothing above/below the sidebar to move into.
            if self.focus == Focus::Sidebar && matches!(dir, Dir::Up | Dir::Down) {
                self.jump_unread(dir == Dir::Down);
                return Ok(Flow::Continue);
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

    /// The terminal keystrokes are currently routed to (a focused pane or mini), if any.
    fn focused_terminal(&self) -> Option<TerminalId> {
        match self.focus {
            Focus::Panes => self.tree.focused_payload(),
            Focus::Mini(i) => self.mini_terminal(i),
            Focus::Sidebar => None,
        }
    }

    /// A bracketed paste from the outer terminal, coalesced into a single event. Routes to wherever
    /// keystrokes go: a text prompt's buffer, or the focused terminal as one write (wrapped in paste
    /// markers when the child wants bracketed paste). Handling it here — rather than as per-character
    /// keys — is what makes pasting fast; it's one `Input` frame and one redraw regardless of length.
    async fn on_paste(&mut self, text: String, sink: &mut Sink) -> Result<()> {
        match self.input {
            // Prompt buffers are single-line; drop control chars so a stray newline can't submit.
            // Routed to the focused field so a task pasted from notes lands in one shot.
            InputMode::Creating => {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                self.active_buf().push_str(&clean);
            }
            InputMode::CreatingRepo | InputMode::CreatingHead => {
                let buf = self.active_buf();
                buf.extend(text.chars().filter(|c| !c.is_control()));
            }
            // A y/n confirm takes no free text.
            InputMode::Confirming => {}
            InputMode::Normal => {
                // Sub-modes consume keys for navigation/sizing, not text — ignore pastes there.
                if self.scroll.is_some() || self.resize_mode || self.prefix {
                    return Ok(());
                }
                let Some(terminal) = self.focused_terminal() else {
                    return Ok(());
                };
                let bracketed = self
                    .parsers
                    .get(&terminal)
                    .map(|p| p.screen().bracketed_paste())
                    .unwrap_or(false);
                let bytes = encode_paste(&text, bracketed);
                sink.send(ClientMsg::Input { terminal, bytes }).await?;
            }
        }
        Ok(())
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
                        self.tree.focus_most_recent();
                        self.focus = Focus::Panes;
                    } else if !self.minis.is_empty() {
                        self.enter_mini(0);
                    }
                }
            }
            Focus::Panes => match self.tree.navigate(dir, pane_area) {
                Nav::ExitLeft => self.focus = Focus::Sidebar,
                // The minis sit below and to the right of the panes, so hitting the bottom or right
                // edge drops into the leftmost mini (adjacent to the main area).
                Nav::Stay if matches!(dir, Dir::Down | Dir::Right) && !self.minis.is_empty() => {
                    self.enter_mini(0)
                }
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
            // A list taller than the terminal needs more than one row at a time. These move the
            // *selection* (the view follows it); the wheel scrolls the view on its own. `u`/`d` are
            // spoken for here — `d` deletes — so the half-page keys are the Ctrl pair only.
            KeyCode::PageDown => self.move_sidebar_sel(self.sidebar_page() as i32),
            KeyCode::PageUp => self.move_sidebar_sel(-(self.sidebar_page() as i32)),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_sidebar_sel((self.sidebar_page() / 2).max(1) as i32)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_sidebar_sel(-((self.sidebar_page() / 2).max(1) as i32))
            }
            KeyCode::Char('g') => self.move_sidebar_sel(i32::MIN),
            KeyCode::Char('G') => self.move_sidebar_sel(i32::MAX),
            // `n`: new agent in the repo under the cursor (branch-only prompt).
            KeyCode::Char('n') => {
                if let Some(repo) = self.selected_repo() {
                    self.create_repo = Some(repo);
                    self.create_buf.clear();
                    self.task_buf.clear();
                    self.create_field = Field::Branch;
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
            // `h`: new branchless HEAD session in the repo under the cursor — runs an agent in the
            // repo root on HEAD (no worktree, no branch). Singleton per repo; no prompt.
            KeyCode::Char('h') => {
                if let Some(repo) = self.selected_repo() {
                    sink.send(ClientMsg::CreateHeadAgent { repo }).await?;
                }
            }
            // `H`: the same HEAD session in a repo given by path — `h` is to `H` as `n` is to `N`.
            KeyCode::Char('H') => {
                self.dir_buf.clear();
                self.create_field = Field::Dir;
                self.input = InputMode::CreatingHead;
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
        if self.scroll.as_ref().is_some_and(|s| s.terminal == terminal) {
            self.scroll = None;
        }
    }

    /// Make `id` the active agent: save the current agent's layout, restore (or create) `id`'s.
    /// Each agent's workspace is its own tiled tree — switching swaps the whole main area.
    async fn activate(&mut self, id: AgentId, sink: &mut Sink) -> Result<()> {
        let restored = self.swap_to_agent(id);
        self.spawn_restored_shells(id, restored, sink).await?;
        self.reconcile(sink).await
    }

    /// Spawn a `$SHELL` for each pane that came back blank from a persisted layout, in the agent's
    /// worktree. Called once per restore; the daemon knows the primary terminal even while it is
    /// dormant, so `like` resolves the worktree without the agent having to be live yet.
    async fn spawn_restored_shells(
        &mut self,
        agent: AgentId,
        blanks: Vec<TerminalId>,
        sink: &mut Sink,
    ) -> Result<()> {
        if blanks.is_empty() {
            return Ok(());
        }
        let Some(primary) = self
            .agents
            .iter()
            .find(|a| a.id == agent)
            .map(|a| a.primary_terminal)
        else {
            return Ok(());
        };
        for terminal in blanks {
            sink.send(ClientMsg::SpawnShell {
                terminal,
                like: primary,
            })
            .await?;
        }
        Ok(())
    }

    /// The pure state change behind [`activate`]: save the current agent's layout, restore (or
    /// create) `id`'s, and ensure its primary terminal is shown.
    ///
    /// Returns the terminals minted for panes that a **persisted** layout brought back blank —
    /// their shells died with the previous daemon, so the caller must spawn one each (see
    /// [`Self::spawn_restored_shells`]).
    #[must_use = "restored blank panes need their shells spawned"]
    fn swap_to_agent(&mut self, id: AgentId) -> Vec<TerminalId> {
        let mut restored = Vec::new();
        // An agent shown in the main area isn't also a mini (its terminal can't be two sizes).
        self.minis.retain(|a| *a != id);
        self.minimized.remove(&id);
        if self.active_agent != Some(id) {
            if let Some(prev) = self.active_agent {
                self.trees.insert(prev, std::mem::take(&mut self.tree));
                // The agent we're leaving becomes the "jump to previous" target. Recorded only on
                // a real change, so re-opening the current agent leaves the target untouched.
                self.prev_active_agent = Some(prev);
            }
            // This session's live tree, else a layout the daemon persisted from a past session.
            match self.trees.remove(&id) {
                Some(live) => self.tree = live,
                None => {
                    self.tree = self
                        .saved_layouts
                        .remove(&id)
                        .map(|l| PaneTree::from_layout(&l))
                        .unwrap_or_default();
                    // Only a *persisted* layout can contain dead panes: the daemon blanks every
                    // leaf whose PTY died with it. A blank pane in this session's live tree is a
                    // split whose SpawnShell is still in flight, and refilling it would spawn a
                    // second shell for one pane.
                    restored = self.tree.fill_blanks(TerminalId::new);
                }
            }
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
        restored
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
            // `Ctrl+B -` anywhere else: open the previous agent (tmux's last-window). The overlay
            // marks its row with `-` while the prefix is armed. No-op if there isn't one.
            KeyCode::Char('-') => {
                if let Some(id) = self.previous_agent() {
                    self.open_agent(id, sink).await?;
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
                    // alternate screen, where nothing scrolls off — answer locally rather than
                    // making a round trip to be told there's no history.
                    let alt = self
                        .parsers
                        .get(&t)
                        .is_some_and(|p| p.screen().alternate_screen());
                    if alt {
                        self.info =
                            "no scrollback here — this pane runs a full-screen app".to_string();
                    } else {
                        // One line back, so the reply lands on a real window: the mode opens only if
                        // there is history to show (see `on_scroll_view`).
                        sink.send(ClientMsg::Scroll {
                            terminal: t,
                            lines: 1,
                        })
                        .await?;
                    }
                }
            }
            // Direct resize (tmux muscle memory): `Ctrl+B` then capital H/J/K/L snaps the focused
            // pane to the next clean stop and stays in resize mode, so a held Shift keeps snapping
            // and releasing it switches to fine hjkl nudges (like `-r`).
            KeyCode::Char('H' | 'J' | 'K' | 'L') if !self.tree.is_empty() => {
                if let Some((dir, _snap)) = resize_dir(key.code) {
                    self.tree.resize_snap(dir);
                    self.resize_mode = true;
                    self.reconcile(sink).await?;
                }
            }
            // Jump to the next unread agent (inbox navigation).
            KeyCode::Tab => self.jump_next_unread(sink).await?,
            // `Ctrl+B <digit>`: open the numbered agent's session, mirroring tmux's `prefix` + N
            // (select window N). The overlay that labels the rows is shown for the whole time the
            // prefix is armed, so the numbers are visible when you press one.
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(id) = self.numbered_agent(c) {
                    self.open_agent(id, sink).await?;
                }
            }
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
    ///
    /// Each key is a *step*, sent to the daemon, which answers with the window to show. Steps rather
    /// than absolute offsets so that output arriving while you sit scrolled back doesn't move the
    /// view: the daemon re-bases the position it holds for us.
    async fn key_scroll(
        &mut self,
        key: KeyEvent,
        terminal: TerminalId,
        sink: &mut Sink,
    ) -> Result<Flow> {
        let page = self
            .attached
            .get(&terminal)
            .map(|s| s.rows as usize)
            .unwrap_or(24)
            .max(1) as i32;
        let half = (page / 2).max(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let lines = match key.code {
            // Line: k/j, arrows, and less/vim's Ctrl-y / Ctrl-e.
            KeyCode::Char('k') | KeyCode::Up => 1,
            KeyCode::Char('j') | KeyCode::Down => -1,
            KeyCode::Char('y') if ctrl => 1,
            KeyCode::Char('e') if ctrl => -1,
            // Page.
            KeyCode::PageUp => page,
            KeyCode::PageDown => -page,
            // Half page: Ctrl-u/Ctrl-d (tmux vi) and plain u/d (less).
            KeyCode::Char('u') => half,
            KeyCode::Char('d') => -half,
            // The extremes; the daemon clamps both to what history it has.
            KeyCode::Char('g') => i32::MAX,
            KeyCode::Char('G') => i32::MIN,
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                self.exit_scroll(terminal, sink).await?;
                return Ok(Flow::Continue);
            }
            _ => return Ok(Flow::Continue),
        };
        sink.send(ClientMsg::Scroll { terminal, lines }).await?;
        Ok(Flow::Continue)
    }

    /// Leave scroll mode: back to rendering live output, and tell the daemon to forget where we were.
    async fn exit_scroll(&mut self, terminal: TerminalId, sink: &mut Sink) -> Result<()> {
        self.scroll = None;
        sink.send(ClientMsg::Scroll {
            terminal,
            lines: i32::MIN,
        })
        .await?;
        Ok(())
    }

    /// A window of history arrived.
    ///
    /// Scroll mode opens only on a window that is actually scrolled back, so a pane with no history
    /// (`available == 0`, which clamps the offset to 0) reports that instead of entering a mode whose
    /// keys couldn't move anything — and would then swallow them. Once open it stays open, including
    /// at the live view: `G` means "bottom", and only `q`/`Esc`/`Enter` (or wheeling down past it)
    /// leave, matching tmux's copy mode and what the README promises.
    fn on_scroll_view(
        &mut self,
        terminal: TerminalId,
        offset: usize,
        available: usize,
        bytes: &[u8],
    ) {
        // Only ever act on the pane we're showing a window for; another pane's reply is not ours to
        // apply. (A stale reply for a pane we've since left is dropped the same way.)
        let showing = self.scroll.as_ref().is_some_and(|s| s.terminal == terminal);
        if offset == 0 && !showing {
            if available == 0 {
                self.info = "no scrollback here yet".to_string();
            }
            return;
        }
        let Some(&size) = self.attached.get(&terminal) else {
            return;
        };
        // A fresh parser per frame: the window is self-contained, and carrying no scrollback of our
        // own is the point — the daemon is the only place history lives.
        let mut parser = vt100::Parser::new(size.rows, size.cols, 0);
        parser.process(bytes);
        self.scroll = Some(Scroll {
            terminal,
            parser,
            offset,
            available,
        });
    }

    async fn key_resize(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
            self.resize_mode = false;
            return Ok(Flow::Continue);
        }
        // Lowercase hjkl / arrows nudge; capital HJKL snap to the next clean stop.
        if let Some((dir, snap)) = resize_dir(key.code) {
            if snap {
                self.tree.resize_snap(dir);
            } else {
                self.tree.resize(dir, RESIZE_STEP);
            }
            self.reconcile(sink).await?;
        }
        Ok(Flow::Continue)
    }

    /// The two-field `n` prompt: `Tab` switches branch↔task, `Enter` creates. An empty task
    /// launches the agent idle at its prompt (the conversational flow); a task dispatches it
    /// already working. A branch is still required — `Enter` with none cancels, as before.
    async fn key_creating(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Enter => {
                let branch = std::mem::take(&mut self.create_buf);
                let task = std::mem::take(&mut self.task_buf);
                let repo = self.create_repo.take();
                self.input = InputMode::Normal;
                if let (Some(repo), false) = (repo, branch.trim().is_empty()) {
                    let task = task.trim();
                    sink.send(ClientMsg::CreateAgent {
                        repo,
                        branch: branch.trim().to_string(),
                        prompt: (!task.is_empty()).then(|| task.to_string()),
                    })
                    .await?;
                }
            }
            KeyCode::Esc => self.input = InputMode::Normal,
            KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                self.create_field = match self.create_field {
                    Field::Task => Field::Branch,
                    _ => Field::Task,
                };
            }
            KeyCode::Backspace => {
                self.active_buf().pop();
            }
            KeyCode::Char(c) => self.active_buf().push(c),
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
                // `N` has no task field; anything but Dir returns to Dir.
                self.create_field = match self.create_field {
                    Field::Dir => Field::Branch,
                    _ => Field::Dir,
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

    /// The one-field `H` prompt: Enter registers the repo at that path and opens its HEAD session.
    /// There is no second field — a HEAD session has no branch and takes no task.
    async fn key_creating_head(&mut self, key: KeyEvent, sink: &mut Sink) -> Result<Flow> {
        match key.code {
            KeyCode::Enter => {
                let dir = self.dir_buf.trim().to_string();
                // Enter with an empty field cancels, matching `n`'s empty-branch behaviour.
                self.dir_buf.clear();
                self.input = InputMode::Normal;
                if !dir.is_empty() {
                    sink.send(ClientMsg::CreateHeadAgentAt {
                        path: expand_path(&dir),
                    })
                    .await?;
                }
            }
            KeyCode::Esc => self.input = InputMode::Normal,
            KeyCode::Backspace => {
                self.dir_buf.pop();
            }
            KeyCode::Char(c) => self.dir_buf.push(c),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn active_buf(&mut self) -> &mut String {
        match self.create_field {
            Field::Dir => &mut self.dir_buf,
            Field::Branch => &mut self.create_buf,
            Field::Task => &mut self.task_buf,
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
                    sel.active = true; // a real gesture, even if it never leaves the anchor cell
                }
                self.last_click = None; // a drag isn't the first half of a double-click
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Only a committed selection copies; a bare click (no drag) selected nothing.
                if self.selection.is_some_and(|s| s.is_active()) {
                    if let Some(text) = self.selection_text() {
                        if !text.trim().is_empty() {
                            copy_to_clipboard(&text);
                            self.info =
                                format!("copied {} chars to clipboard", text.chars().count());
                        }
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        // A fresh left-press dismisses any prior selection (and its lingering highlight); the pane
        // branch below re-creates one under the cursor. Without this, a completed drag-selection
        // persists and the next Up over a non-pane target (a mini, the sidebar) would re-copy it.
        if let MouseEventKind::Down(MouseButton::Left) = me.kind {
            self.selection = None;
        }

        // A left-click anywhere in the sidebar body focuses the sidebar (keeping its current
        // selection), mirroring how clicking a pane focuses it. The sidebar occupies every column
        // left of the main area (`self.area.x` is the sidebar width) down to the status bar
        // (`self.area.bottom()`), an x-range disjoint from the minis and panes — both live in
        // `self.area` — so it's safe to resolve first. The wheel scrolls the agent list (the view
        // only, leaving the selection where it is); drag over it still does nothing.
        if me.column < self.area.x && me.row < self.area.bottom() {
            match me.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.focus = Focus::Sidebar;
                    return Ok(());
                }
                MouseEventKind::ScrollUp => {
                    self.scroll_sidebar(-(SIDEBAR_WHEEL_ROWS as i32));
                    return Ok(());
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_sidebar(SIDEBAR_WHEEL_ROWS as i32);
                    return Ok(());
                }
                _ => {}
            }
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
                            self.wheel_scroll(terminal, up, sink).await?;
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
                // A second press on the same cell within the window is a double-click: select the
                // token under it and let the following `Up` copy it via the active-selection path.
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(t, c, r, term)| {
                    term == terminal
                        && (c, r) == (me.column, me.row)
                        && now.duration_since(t) <= DOUBLE_CLICK
                });
                if double {
                    self.last_click = None; // consume, so a third press isn't a fresh double
                    if let Some(sel) = self.token_selection(terminal, inner, me.column, me.row) {
                        self.selection = Some(sel);
                        return Ok(()); // whitespace under the cursor leaves no selection
                    }
                    return Ok(());
                }
                self.last_click = Some((now, me.column, me.row, terminal));
                // Always own the left-drag for a pane-isolated text selection, even when the pane's
                // app tracks the mouse (Claude, vim, less). amux never forwards left-clicks to pane
                // apps — only the wheel — so this takes away no interaction they had, and it makes
                // their output highlightable and copyable (via OSC 52) instead of unselectable.
                let p = clamp_to(inner, me.column, me.row);
                self.selection = Some(Selection {
                    terminal,
                    inner,
                    anchor: p,
                    head: p,
                    active: false,
                });
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(me.kind, MouseEventKind::ScrollUp);
                // An app that enabled mouse tracking (vim/less/htop, and Claude if it does) owns
                // the wheel; otherwise it scrolls the daemon's history for the pane (plain shells).
                if self.app_wants_mouse(terminal) {
                    if let Some(bytes) = self.encode_wheel(terminal, up, me.column, me.row, inner) {
                        sink.send(ClientMsg::Input { terminal, bytes }).await?;
                    }
                } else {
                    self.wheel_scroll(terminal, up, sink).await?;
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
            // A row the terminal wrapped continues on the next one, so it keeps its trailing cells
            // and gets no separator: re-inserting a break the program never printed would corrupt a
            // URL (or any long token) on paste. A row that ends because the program printed a
            // newline is trimmed and separated as before.
            let wrapped = screen.row_wrapped(y - sel.inner.y) && c1 == right;
            if !wrapped {
                while line.ends_with(' ') {
                    line.pop();
                }
            }
            out.push_str(&line);
            if y != end.1 && !wrapped {
                out.push('\n');
            }
        }
        Some(out)
    }

    /// The token (word plus path/URL punctuation) under screen point `(col, row)` in pane `inner`,
    /// as a [`Selection`] ready to highlight and copy. `None` if the cell is blank or the point
    /// isn't over the pane's parser — a double-click on whitespace selects nothing.
    ///
    /// Follows the terminal's wrapping: a token longer than the pane is wide occupies several rows,
    /// and it is one token, so the span grows across the seams. Scanning a single row instead copied
    /// whatever fragment happened to fit — a truncated URL that looks like a working copy until you
    /// paste it.
    fn token_selection(
        &self,
        terminal: TerminalId,
        inner: Rect,
        col: u16,
        row: u16,
    ) -> Option<Selection> {
        let screen = self.parsers.get(&terminal)?.screen();
        let cy = row.checked_sub(inner.y)?;
        let cx = col.checked_sub(inner.x)?;
        let cells = |y: u16| -> Vec<String> {
            (0..inner.width)
                .map(|x| {
                    screen
                        .cell(y, x)
                        .map(|c| c.contents().to_string())
                        .unwrap_or_default()
                })
                .collect()
        };
        let last = inner.width.saturating_sub(1) as usize;
        let (x0, x1) = token_span(&cells(cy), cx as usize)?;

        // Backwards over seams: this row continues the one above only if that row was wrapped, and
        // the token has to actually reach the seam on both sides of it.
        let mut y0 = cy;
        let mut x0 = x0;
        while x0 == 0 && y0 > 0 && screen.row_wrapped(y0 - 1) {
            let above = cells(y0 - 1);
            match token_span(&above, last) {
                Some((ax0, _)) => {
                    y0 -= 1;
                    x0 = ax0;
                }
                None => break,
            }
        }
        // Forwards over seams, symmetrically.
        let mut y1 = cy;
        let mut x1 = x1;
        while x1 == last && screen.row_wrapped(y1) && y1 + 1 < inner.height {
            let below = cells(y1 + 1);
            match token_span(&below, 0) {
                Some((_, bx1)) => {
                    y1 += 1;
                    x1 = bx1;
                }
                None => break,
            }
        }
        Some(Selection {
            terminal,
            inner,
            anchor: (inner.x + x0 as u16, inner.y + y0),
            head: (inner.x + x1 as u16, inner.y + y1),
            active: true,
        })
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

    /// What to draw for `terminal`: the served history window while it is scrolled back, else the
    /// live screen. One place decides, so panes and minis can't disagree.
    fn screen_for(&self, terminal: TerminalId) -> Option<&vt100::Screen> {
        match self.scroll.as_ref().filter(|s| s.terminal == terminal) {
            Some(scroll) => Some(scroll.parser.screen()),
            None => self.parsers.get(&terminal).map(|p| p.screen()),
        }
    }

    /// Whether the app in `terminal` has enabled mouse tracking (so it owns the wheel/clicks).
    fn app_wants_mouse(&self, terminal: TerminalId) -> bool {
        self.parsers
            .get(&terminal)
            .is_some_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
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

    /// Wheel-scroll a pane whose app doesn't take the mouse: a step request like any other, except
    /// that wheeling back down to the live view leaves scroll mode, which is what a mouse user
    /// expects (the keyboard leaves with `q`/`Esc`).
    ///
    /// Nothing is entered speculatively — scroll mode begins when a window actually arrives, so a
    /// pane with no history never captures a keystroke while we wait for the answer.
    async fn wheel_scroll(
        &mut self,
        terminal: TerminalId,
        up: bool,
        sink: &mut Sink,
    ) -> Result<()> {
        const STEP: i32 = 3;
        let at = self
            .scroll
            .as_ref()
            .filter(|s| s.terminal == terminal)
            .map(|s| s.offset);
        if !up {
            match at {
                // Already live: the wheel has nothing to do here.
                None => return Ok(()),
                // Within a step of live — land exactly on it and hand the pane back.
                Some(offset) if offset <= STEP as usize => {
                    return self.exit_scroll(terminal, sink).await
                }
                Some(_) => {}
            }
        }
        let lines = if up { STEP } else { -STEP };
        sink.send(ClientMsg::Scroll { terminal, lines }).await?;
        Ok(())
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
                        self.parsers.insert(
                            terminal,
                            vt100::Parser::new(size.rows, size.cols, CLIENT_SCROLLBACK),
                        );
                    }
                }
                sink.send(ClientMsg::Attach { terminal, size }).await?;
                self.attached.insert(terminal, size);
                // A served window is rendered for the size it was asked at, so a resize needs a
                // fresh one; a zero-line step re-serves the same content at the new size.
                if self
                    .scroll
                    .as_ref()
                    .is_some_and(|sc| sc.terminal == terminal)
                {
                    sink.send(ClientMsg::Scroll { terminal, lines: 0 }).await?;
                }
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
            // The daemon drops our scroll position on detach, so hold no window for it either —
            // otherwise returning to this pane would show a stale scrolled view whose next step
            // would jump (the daemon would re-base it from live).
            if self
                .scroll
                .as_ref()
                .is_some_and(|sc| sc.terminal == terminal)
            {
                self.scroll = None;
            }
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
        sink.send(ClientMsg::SetPrevious(self.prev_active_agent))
            .await?;
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
                last_opened: a.last_opened,
            })
            .collect();
        sort_for_sidebar(&mut items);
        items.into_iter().map(|i| i.id).collect()
    }

    /// Whether the sidebar is drawn as the narrow rail. Derived from the width `main_area` gave the
    /// panes, the same width `render_sidebar` measures its own rect by — so the two agree.
    fn sidebar_minimized(&self) -> bool {
        self.area.x < SIDEBAR_W_FULL
    }

    /// The flat, ordered rows: each repo header followed by its agents. Repos are sorted by name so
    /// the layout is stable.
    fn sidebar_rows(&self) -> Vec<Row> {
        self.sidebar_rows_for(self.sidebar_minimized())
    }

    /// `sidebar_rows` for a known width. The rail has no room for the dim trailer lines, so they are
    /// not rows there either — `minimized` is a parameter so render can pass what it measured and
    /// keep rows and lines in lockstep.
    fn sidebar_rows_for(&self, minimized: bool) -> Vec<Row> {
        if self.repos.is_empty() {
            return vec![Row::NoRepos];
        }
        let mut repos: Vec<&RepoInfo> = self.repos.iter().collect();
        repos.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let mut rows = Vec::new();
        let with_agents = repos
            .iter()
            .filter(|r| self.agents.iter().any(|a| a.repo == r.id))
            .count();
        let recent = recent_ids(&self.agents, Utc::now());
        if show_recent(
            with_agents,
            self.agents.len(),
            recent.len(),
            self.sidebar_page(),
            minimized,
        ) {
            rows.push(Row::RecentHeader);
            rows.extend(recent.into_iter().map(Row::Recent));
            rows.push(Row::Divider);
        }
        for repo in repos {
            rows.push(Row::Repo(repo.id));
            let ids = self.agent_ids_for(repo.id);
            if ids.is_empty() && !minimized {
                rows.push(Row::EmptyRepo(repo.id));
            }
            for id in ids {
                rows.push(Row::Agent(id));
                let has_message = self.agents.iter().any(|a| {
                    a.id == id
                        && matches!(
                            &a.state,
                            AgentState::NeedsAttention {
                                message: Some(_),
                                ..
                            }
                        )
                });
                if has_message && !minimized {
                    rows.push(Row::Attention(id));
                }
            }
        }
        rows
    }

    /// The repo the cursor is in: the selected repo header, or the selected agent's repo.
    fn selected_repo(&self) -> Option<RepoId> {
        match self.sidebar_sel? {
            Row::Repo(id) | Row::EmptyRepo(id) => Some(id),
            Row::Agent(id) | Row::Attention(id) | Row::Recent(id) => {
                self.agents.iter().find(|a| a.id == id).map(|a| a.repo)
            }
            Row::RecentHeader | Row::Divider | Row::NoRepos => None,
        }
    }

    /// The selected agent, if the cursor is on an agent row (not a repo header).
    fn selected_agent(&self) -> Option<AgentId> {
        match self.sidebar_sel? {
            Row::Agent(id) | Row::Recent(id) => Some(id),
            _ => None,
        }
    }

    /// Agent ids in sidebar order (agent rows only, repo headers dropped) — the numbering the
    /// numeric overlay draws and the numeric shortcut selects from.
    fn ordered_agent_ids(&self) -> Vec<AgentId> {
        let mut seen = HashSet::new();
        self.sidebar_rows()
            .into_iter()
            .filter_map(|row| match row {
                Row::Agent(id) | Row::Recent(id) => Some(id),
                _ => None,
            })
            // An agent in the recent block also has a row under its repo. Numbering it once — at its
            // first, i.e. recent, row — keeps "the digit you see is the digit you press" true.
            .filter(|id| seen.insert(*id))
            .collect()
    }

    /// Whether the numeric overlay should be drawn: Cmd/Super is held, or the `Ctrl+B` prefix is
    /// armed (so the labels are visible while you decide which agent to jump to with a digit).
    fn numeric_overlay_active(&self) -> bool {
        self.super_held || self.prefix
    }

    /// The agent the numeric overlay labels with digit `c`, if any. Read from the *current* sidebar
    /// order, so a layout change while the overlay was up is honored. `None` if `c` isn't a digit
    /// or maps past the last agent.
    fn numbered_agent(&self, c: char) -> Option<AgentId> {
        let idx = digit_index(c)?;
        self.ordered_agent_ids().get(idx).copied()
    }

    /// The agent that was in the main area before the current one (tmux's last-window) — the target
    /// of `Ctrl+B -`. `None` when there is no previous agent yet or it has since been removed.
    fn previous_agent(&self) -> Option<AgentId> {
        let prev = self.prev_active_agent?;
        self.agents.iter().any(|a| a.id == prev).then_some(prev)
    }

    /// Jump to an agent's session: select its sidebar row and open it in the main area. This is
    /// what the numeric (`Ctrl+B <digit>` / `Cmd+digit`) and previous (`Ctrl+B -`) shortcuts do.
    async fn open_agent(&mut self, id: AgentId, sink: &mut Sink) -> Result<()> {
        self.place_sidebar_sel(Row::Agent(id));
        self.activate(id, sink).await
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
                _ => None,
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
            self.place_sidebar_sel(Row::Agent(id));
            self.open_selected(sink).await?;
        }
        Ok(())
    }

    /// Move the cursor `delta` **selectable** rows, clamped at the ends. Counting selectable rows
    /// rather than raw rows is what makes the dim trailer lines invisible to `j`/`k`.
    fn move_sidebar_sel(&mut self, delta: i32) {
        let rows = self.sidebar_rows();
        let stops: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].selectable()).collect();
        if stops.is_empty() {
            self.sidebar_sel = None;
            return;
        }
        // Where the cursor sits among the stops. A cursor on a non-selectable row (or none at all)
        // counts as the first stop, so the next `j` moves one row rather than jumping.
        let here = self
            .sidebar_sel
            .and_then(|s| rows.iter().position(|&r| r == s))
            .and_then(|i| stops.iter().position(|&stop| stop >= i))
            .unwrap_or(0) as i32;
        // Saturating because `g`/`G` pass `i32::MIN`/`i32::MAX` as "as far as it goes".
        let next = here.saturating_add(delta).clamp(0, stops.len() as i32 - 1) as usize;
        self.place_sidebar_sel(rows[stops[next]]);
    }

    /// Move the sidebar selection to the next **unread** agent in `dir` (down = `true`), skipping
    /// read rows and repo headers. No wrap and a silent no-op when none lies that way — a directional
    /// key shouldn't teleport across the list, matching how `j`/`k` clamp at the ends. Bound to
    /// `Ctrl+j`/`Ctrl+k` in the sidebar (distinct from `Ctrl+B Tab`, which cycles *and opens*).
    fn jump_unread(&mut self, down: bool) {
        let rows = self.sidebar_rows();
        // The recent block counts: it is where a cross-repo unread agent is easiest to reach.
        let agent_of = |r: &Row| match r {
            Row::Agent(id) | Row::Recent(id) => Some(*id),
            _ => None,
        };
        let here = self.sidebar_sel.as_ref().and_then(agent_of);
        // An agent in the block has a row there *and* under its repo. Skipping rows that name the
        // agent already under the cursor makes it one stop instead of two, so a jump always lands on
        // a different agent rather than stuttering on the same one.
        let is_unread_agent = |r: &Row| {
            agent_of(r).is_some_and(|id| {
                Some(id) != here && self.agents.iter().any(|a| a.id == id && a.unread)
            })
        };
        let cur = self
            .sidebar_sel
            .and_then(|s| rows.iter().position(|&r| r == s))
            .unwrap_or(0);
        let found = if down {
            ((cur + 1)..rows.len()).find(|&i| is_unread_agent(&rows[i]))
        } else {
            (0..cur).rev().find(|&i| is_unread_agent(&rows[i]))
        };
        if let Some(i) = found {
            self.place_sidebar_sel(rows[i]);
        }
    }

    /// Put the cursor on `row` because the user (or their shortcut) asked for it — which also means
    /// the roster stops moving it on its own.
    fn place_sidebar_sel(&mut self, row: Row) {
        self.sidebar_sel = Some(row);
        self.sidebar_placed = true;
        self.scroll_sel_into_view();
    }

    /// Keep the cursor on a row that exists. Until the user has placed it themselves it homes to the
    /// **first** selectable row on every roster change, which is what lands it on the newest recent
    /// agent at startup: repos arrive first, so the cursor is briefly parked on a repo header, and
    /// the recent block only appears above it once the agents follow.
    fn ensure_sidebar_sel(&mut self) {
        let rows = self.sidebar_rows();
        let valid = self
            .sidebar_sel
            .is_some_and(|s| s.selectable() && rows.contains(&s));
        if !valid || !self.sidebar_placed {
            self.sidebar_sel = rows.iter().copied().find(|r| r.selectable());
        }
        self.scroll_sel_into_view();
    }

    /// Rows of sidebar list the terminal can show: the sidebar spans the main area's height (both
    /// come from `main_area`, so they cannot disagree) less its two borders. Also the page size for
    /// `PageUp`/`PageDown`.
    fn sidebar_page(&self) -> usize {
        self.area.height.saturating_sub(2).max(1) as usize
    }

    /// Pull the view onto the selection. Called wherever the selection moves — never from render,
    /// so an explicit wheel scroll stays put until the user moves the cursor again.
    fn scroll_sel_into_view(&mut self) {
        let rows = self.sidebar_rows();
        let Some(sel) = self
            .sidebar_sel
            .and_then(|s| rows.iter().position(|&r| r == s))
        else {
            self.sidebar_top = 0;
            return;
        };
        let height = self.sidebar_page();
        self.sidebar_top = clamp_top(
            top_showing(self.sidebar_top, sel, height),
            rows.len(),
            height,
        );
    }

    /// Scroll the sidebar view by `delta` rows without touching the selection — what the wheel does.
    fn scroll_sidebar(&mut self, delta: i32) {
        let len = self.sidebar_rows().len();
        let top = (self.sidebar_top as i32 + delta).max(0) as usize;
        self.sidebar_top = clamp_top(top, len, self.sidebar_page());
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

/// The digit that labels the agent row at 0-based `index` in the numeric overlay: rows 0..=8 →
/// `'1'..='9'`, row 9 (the tenth) → `'0'`. Rows past the tenth get no label (`None`).
fn overlay_digit(index: usize) -> Option<char> {
    match index {
        0..=8 => char::from_digit(index as u32 + 1, 10),
        9 => Some('0'),
        _ => None,
    }
}

/// The 0-based agent-row index a pressed digit selects — the inverse of [`overlay_digit`].
/// `'1'..='9'` → `0..=8`, `'0'` → `9`. Non-digits yield `None`.
fn digit_index(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some(c as usize - '1' as usize),
        '0' => Some(9),
        _ => None,
    }
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

/// The cells a symbol occupies on screen, for `ForcedWidth` — a wide glyph is two, anything else
/// one. Needed because the symbol we store also holds escape sequences, whose printable width is
/// zero and which ratatui would otherwise measure as text.
fn cell_width(symbol: &str) -> NonZeroU16 {
    let wide = symbol.chars().next().is_some_and(|c| {
        // The CJK/emoji ranges vt100 itself treats as double-width.
        matches!(c as u32,
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD)
    });
    NonZeroU16::new(if wide { 2 } else { 1 }).expect("nonzero")
}

/// The URL schemes amux marks as hyperlinks. Deliberately short: a false positive turns ordinary
/// text into something clickable, which is worse than leaving a rare scheme unmarked.
const LINK_SCHEMES: [&str; 2] = ["https://", "http://"];

/// Characters trimmed from the end of a detected URL. All of them are legal inside a URI, but at the
/// *end* of one on a terminal they are almost always the sentence's punctuation rather than the
/// address's — `see https://example.com/x.` should not copy the period.
const URL_TRAILING_TRIM: &str = ".,;:!?'\")]}>";

/// The byte ranges of the URLs in one logical line of text. A URL starts at a scheme and runs to
/// whitespace, less any trailing punctuation (see [`URL_TRAILING_TRIM`]).
///
/// Works on a *logical* line — one that has had the terminal's wrapping undone — because that is the
/// only place a wrapped URL is contiguous.
fn urls_in_line(line: &str) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut at = 0;
    while at < line.len() {
        // The earliest scheme at or after `at`.
        let Some(start) = LINK_SCHEMES
            .iter()
            .filter_map(|s| line[at..].find(s).map(|i| at + i))
            .min()
        else {
            break;
        };
        let after_scheme = start
            + LINK_SCHEMES
                .iter()
                .find(|s| line[start..].starts_with(**s))
                .map_or(0, |s| s.len());
        let mut end = line[after_scheme..]
            .find(char::is_whitespace)
            .map_or(line.len(), |i| after_scheme + i);
        while end > after_scheme
            && line[..end]
                .chars()
                .next_back()
                .is_some_and(|c| URL_TRAILING_TRIM.contains(c))
        {
            end -= line[..end].chars().next_back().map_or(0, char::len_utf8);
        }
        // A bare scheme with no host is not a link.
        if end > after_scheme {
            out.push(start..end);
        }
        at = end.max(after_scheme);
    }
    out
}

/// One hyperlink on a pane's screen: the URL, and the cell runs it occupies as
/// `(row, first_col, last_col)` — one run per screen row, because a wrapped URL is several rows.
#[derive(Debug, PartialEq, Eq)]
struct PaneLink {
    url: String,
    runs: Vec<(u16, u16, u16)>,
}

/// Every hyperlink visible on `screen`, with its cell runs.
///
/// Rows are first joined into logical lines using `row_wrapped` — undoing the terminal's wrapping is
/// what makes a long URL findable at all — then each match is mapped back to the cells it came from.
/// The mapping is by cell count rather than by byte offset so a wide glyph earlier in the line
/// cannot shift a run.
fn pane_links(screen: &vt100::Screen, width: u16, height: u16) -> Vec<PaneLink> {
    let mut links = Vec::new();
    let mut y = 0;
    while y < height {
        // Gather one logical line: this row plus every row it wrapped onto.
        let mut cells: Vec<(u16, u16, String)> = Vec::new();
        let mut last = y;
        loop {
            for x in 0..width {
                let contents = screen
                    .cell(last, x)
                    .map(|c| c.contents().to_string())
                    .unwrap_or_default();
                cells.push((last, x, contents));
            }
            if screen.row_wrapped(last) && last + 1 < height {
                last += 1;
            } else {
                break;
            }
        }
        // The logical line, plus the cell each character starts at.
        let mut text = String::new();
        let mut starts: Vec<usize> = Vec::new();
        for (i, (_, _, contents)) in cells.iter().enumerate() {
            starts.push(text.len());
            // An empty cell is a blank on screen; it must occupy a byte so offsets stay aligned.
            text.push_str(if contents.is_empty() { " " } else { contents });
            let _ = i;
        }
        starts.push(text.len());
        for range in urls_in_line(&text) {
            // Cells whose character lies inside the match.
            let hit: Vec<usize> = (0..cells.len())
                .filter(|&i| starts[i] >= range.start && starts[i] < range.end)
                .collect();
            let mut runs: Vec<(u16, u16, u16)> = Vec::new();
            for i in hit {
                let (row, col, _) = cells[i];
                match runs.last_mut() {
                    Some((r, _, x1)) if *r == row && *x1 + 1 == col => *x1 = col,
                    _ => runs.push((row, col, col)),
                }
            }
            if !runs.is_empty() {
                links.push(PaneLink {
                    url: text[range].to_string(),
                    runs,
                });
            }
        }
        y = last + 1;
    }
    links
}

/// Tell the outer terminal about the hyperlinks on a pane, by wrapping each run's cells in OSC 8.
///
/// Without this the terminal has to guess from its own grid, where a wrapped URL is two fragments
/// with a pane border between them — so clicking one opens a truncated address, and hover highlights
/// only the fragment. Each run carries the **whole** URI, so a click on any row opens the right
/// address, and the shared `id=` lets a terminal highlight every row as one link.
///
/// The sequences ride along in the first and last cell of each run, with `ForcedWidth` so ratatui's
/// diffing and width arithmetic still see one cell's worth of text.
fn mark_links(buf: &mut Buffer, screen: &vt100::Screen, inner: Rect) {
    for (i, link) in pane_links(screen, inner.width, inner.height)
        .into_iter()
        .enumerate()
    {
        for (row, x0, x1) in link.runs {
            let (sx, sy) = (inner.x + x0, inner.y + row);
            let (ex, ey) = (inner.x + x1, inner.y + row);
            if !inner.contains((sx, sy).into()) || !inner.contains((ex, ey).into()) {
                continue;
            }
            // `id` ties the rows of one wrapped link together; the index keeps two links on the
            // same screen distinct.
            if let Some(cell) = buf.cell_mut((sx, sy)) {
                let sym = format!("\x1b]8;id={i};{}\x1b\\{}", link.url, cell.symbol());
                let w = cell_width(cell.symbol());
                cell.set_symbol(&sym);
                cell.set_diff_option(CellDiffOption::ForcedWidth(w));
            }
            if let Some(cell) = buf.cell_mut((ex, ey)) {
                let sym = format!("{}\x1b]8;;\x1b\\", cell.symbol());
                let w = cell_width(cell.symbol());
                cell.set_symbol(&sym);
                cell.set_diff_option(CellDiffOption::ForcedWidth(w));
            }
        }
    }
}

/// Whether a cell's contents belong to a token: alphanumerics plus the punctuation that keeps
/// paths, URLs, flags and dotted identifiers whole. Empty cells (blanks, the trailing half of a
/// wide glyph) are boundaries.
///
/// The query-string characters (`?=&%+#`) are in the class because a URL's query is part of the URL
/// — without them a double-click on `…/x?q=1` copied `…/x` and silently dropped the parameters.
/// `,` and `;` stay out: they end a token far more often in prose than they appear in a URL.
fn is_token_char(contents: &str) -> bool {
    !contents.is_empty()
        && contents
            .chars()
            .all(|c| c.is_alphanumeric() || "._-/~:@?=&%+#".contains(c))
}

/// The `[x0, x1]` inclusive cell span of the token containing cell `x` in `row` (a pane row's
/// per-cell contents), or `None` when that cell isn't a token char. Single row; the caller maps
/// the span back to screen columns.
fn token_span(row: &[String], x: usize) -> Option<(usize, usize)> {
    if row.get(x).is_none_or(|c| !is_token_char(c)) {
        return None;
    }
    let mut x0 = x;
    while x0 > 0 && is_token_char(&row[x0 - 1]) {
        x0 -= 1;
    }
    let mut x1 = x;
    while x1 + 1 < row.len() && is_token_char(&row[x1 + 1]) {
        x1 += 1;
    }
    Some((x0, x1))
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
    // A bare click (no drag, no token) selected nothing — draw no highlight.
    if !sel.is_active() {
        return;
    }
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

/// Map a resize key to `(direction, snap?)`. Lowercase `hjkl` and the arrows nudge the split by a
/// fine step; capital `HJKL` snap it to the next clean stop. Case is meaningful now, so holding
/// Shift is the difference between a nudge and a snap.
fn resize_dir(code: KeyCode) -> Option<(Dir, bool)> {
    let out = match code {
        KeyCode::Char('h') | KeyCode::Left => (Dir::Left, false),
        KeyCode::Char('H') => (Dir::Left, true),
        KeyCode::Char('l') | KeyCode::Right => (Dir::Right, false),
        KeyCode::Char('L') => (Dir::Right, true),
        KeyCode::Char('j') | KeyCode::Down => (Dir::Down, false),
        KeyCode::Char('J') => (Dir::Down, true),
        KeyCode::Char('k') | KeyCode::Up => (Dir::Up, false),
        KeyCode::Char('K') => (Dir::Up, true),
        _ => return None,
    };
    Some(out)
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
        .constraints([
            Constraint::Length(sidebar_width(rows[0].width)),
            Constraint::Min(1),
        ])
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
            .map(|a| {
                (
                    a.state.glyph(),
                    color_for(&a.state),
                    a.branch.as_deref().unwrap_or("HEAD"),
                )
            })
            .unwrap_or(('?', Color::DarkGray, "?"));
        let title = format!(" {glyph} {branch} ");
        let border = if focused {
            Style::default()
                .fg(app.theme.focus)
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
            let bar = if unread { "\u{258c}" } else { " " };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        bar,
                        Style::default()
                            .fg(app.theme.focus)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {glyph}"), Style::default().fg(color)),
                ])),
                inner,
            );
            continue;
        }
        match app.mini_terminal(i).and_then(|t| app.screen_for(t)) {
            Some(screen) => {
                frame.render_widget(PseudoTerminal::new(screen), inner);
                mark_links(frame.buffer_mut(), screen, inner);
            }
            None => frame.render_widget(
                Paragraph::new("  \u{2026}").style(Style::default().fg(Color::DarkGray)),
                inner,
            ),
        }
    }
}

fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    // On a narrow terminal the sidebar shrinks to an icon-and-initials rail (see `sidebar_width`).
    // Derived from the rect we're handed so there's no separate state to keep in sync.
    let minimized = area.width < SIDEBAR_W_FULL;
    let unread = app.agents.iter().filter(|a| a.unread).count();
    // How much of the list is off-screen, so a clipped sidebar says so instead of just ending. The
    // same numbers slice `lines` below — one `clamp_top` call, so the hint cannot contradict the
    // view. `rows()` counts what will be listed; the empty state is one unscrollable line.
    let height = area.height.saturating_sub(2) as usize;
    let rows = app.sidebar_rows_for(minimized);
    let len = rows.len();
    let top = clamp_top(app.sidebar_top, len, height);
    let below = len.saturating_sub(top + height);
    // The full title ("agents · N unread") won't fit the minimized rail, so it drops to a bare
    // unread badge — just the count when something's waiting, otherwise nothing. The rail has no
    // room for counts either, so there the hint is bare arrows.
    let hidden = match (minimized, top, below) {
        (_, 0, 0) => String::new(),
        (false, 0, b) => format!("\u{2193}{b} "),
        (false, a, 0) => format!("\u{2191}{a} "),
        (false, a, b) => format!("\u{2191}{a}\u{2193}{b} "),
        (true, 0, _) => "\u{2193}".to_string(),
        (true, _, 0) => "\u{2191}".to_string(),
        (true, _, _) => "\u{2195}".to_string(),
    };
    let title = match (minimized, unread) {
        (false, 0) => format!(" agents {hidden}"),
        (false, n) => format!(" agents · {n} unread {hidden}"),
        (true, 0) => hidden.clone(),
        (true, n) => format!(" {n} {hidden}"),
    };
    let border = if app.focus == Focus::Sidebar {
        Style::default().fg(app.theme.focus)
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
    // Numeric-shortcut overlay: while Cmd is held (or the `Ctrl+B` prefix is armed), the first ten
    // agent rows show their shortcut digit in place of the status glyph — same 3-cell width, so the
    // sidebar shape doesn't shift. Built from the current sidebar order, so it tracks live layout.
    let mut overlay_digits: HashMap<AgentId, char> = if app.numeric_overlay_active() {
        app.ordered_agent_ids()
            .into_iter()
            .enumerate()
            .filter_map(|(i, id)| overlay_digit(i).map(|d| (id, d)))
            .collect()
    } else {
        HashMap::new()
    };
    // The "jump to previous" target (Ctrl+B -) is marked with `-` in place of its digit, so the
    // overlay reads as "type a number, or - to bounce back." Marked even past the tenth agent.
    if app.numeric_overlay_active() {
        if let Some(prev) = app.prev_active_agent {
            if app.agents.iter().any(|a| a.id == prev) {
                overlay_digits.insert(prev, '-');
            }
        }
    }
    // The header's count comes from the rows themselves, so it cannot disagree with the block.
    let recent_shown = rows.iter().filter(|r| matches!(r, Row::Recent(_))).count();
    // One line pushed per row, no exceptions — see `Row`.
    let mut lines = Vec::new();
    for row in rows {
        let selected = app.sidebar_sel == Some(row);
        let marker = if selected { "\u{25b8}" } else { " " };
        match row {
            Row::Repo(id) => {
                let name = repo_names.get(&id).copied().unwrap_or("repo");
                let count = app.agents.iter().filter(|a| a.repo == id).count();
                let mut style = Style::default()
                    .fg(app.theme.focus)
                    .add_modifier(Modifier::BOLD);
                if selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if minimized {
                    // Just the disclosure caret + truncated repo name; the count and hint don't fit.
                    let name_w = (inner.width as usize).saturating_sub(4);
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{marker}\u{25be} "),
                            Style::default().fg(app.theme.focus),
                        ),
                        Span::styled(format!("{name:.name_w$}"), style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{marker} \u{25be} "),
                            Style::default().fg(app.theme.focus),
                        ),
                        Span::styled(format!("{name} "), style),
                        Span::styled(format!("({count})"), Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            // Styled as a repo header for a repo named "recent" — same caret, colour, and count
            // column — so the block reads as another group rather than a separate widget. It is
            // still not a cursor stop: there is no repo behind it for `n`/`h`/`P` to act on.
            Row::RecentHeader => lines.push(Line::from(vec![
                Span::styled("  \u{25be} ", Style::default().fg(app.theme.focus)),
                Span::styled(
                    "recent ",
                    Style::default()
                        .fg(app.theme.focus)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("({recent_shown})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            Row::Divider => lines.push(Line::from(Span::styled(
                // Inset one cell each side so the rule reads as a separator inside the sidebar
                // rather than a second border.
                format!(
                    " {} ",
                    "\u{2500}".repeat((inner.width as usize).saturating_sub(2))
                ),
                Style::default().fg(Color::DarkGray),
            ))),
            Row::Recent(id) => {
                let Some(agent) = by_id.get(&id) else {
                    lines.push(Line::default());
                    continue;
                };
                // Qualified `repo/branch`, because the block's whole job is spanning repos — an
                // unqualified name here would be ambiguous in a way the grouped rows never are.
                let repo = repo_names.get(&agent.repo).copied().unwrap_or("repo");
                let name = format!("{repo}/{}", agent.name);
                let name_style = if agent.unread || selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let glyph_span = match overlay_digits.get(&id) {
                    Some(&digit) => Span::styled(
                        format!(" {digit} "),
                        Style::default()
                            .fg(Color::Black)
                            .bg(app.theme.focus)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None => Span::styled(
                        format!(" {} ", agent.state.glyph()),
                        Style::default().fg(color_for(&agent.state)),
                    ),
                };
                // The same columns as a roster row — cursor (1) + unread bar (1) + " glyph " (3) +
                // open marker (1) = 6 — so the two lists line up and `*` means the same in both.
                let open_marker = if open.contains(&id) { "*" } else { " " };
                let age = age_short(agent.last_opened);
                let name_w = (inner.width as usize).saturating_sub(6 + age.len() + 1);
                lines.push(Line::from(vec![
                    Span::styled(marker.to_string(), Style::default().fg(app.theme.focus)),
                    Span::styled(
                        if agent.unread { "\u{258c}" } else { " " },
                        Style::default()
                            .fg(app.theme.focus)
                            .add_modifier(Modifier::BOLD),
                    ),
                    glyph_span,
                    Span::styled(open_marker, name_style),
                    Span::styled(format!("{name:<name_w$.name_w$}"), name_style),
                    Span::styled(format!(" {age}"), Style::default().fg(Color::DarkGray)),
                ]));
            }
            Row::EmptyRepo(_) => lines.push(Line::from(Span::styled(
                "      no agents — press n",
                Style::default().fg(Color::DarkGray),
            ))),
            Row::NoRepos => lines.push(Line::from(Span::styled(
                " no repos yet…",
                Style::default().fg(Color::DarkGray),
            ))),
            Row::Agent(id) => {
                let Some(agent) = by_id.get(&id) else {
                    // Unreachable (rows are built from `self.agents`), but a skipped line would
                    // desynchronise rows from lines, so spend one blank instead.
                    lines.push(Line::default());
                    continue;
                };
                let is_open = open.contains(&id);
                // Unread agents get a bold cyan gutter bar (▌) down the left edge plus a bold name,
                // so a waiting agent is obvious at a glance; read ones are plain. The bar sits in
                // its own column beside the selection cursor, so a selected *and* unread row shows
                // both.
                let name_style = if agent.unread || selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let unread_bar = if agent.unread { "\u{258c}" } else { " " };
                // The digit overlay covers the status glyph (same width) when active; otherwise the
                // usual state glyph in its state colour.
                let glyph_span = match overlay_digits.get(&id) {
                    Some(&digit) => Span::styled(
                        format!(" {digit} "),
                        Style::default()
                            .fg(Color::Black)
                            .bg(app.theme.focus)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None => Span::styled(
                        format!(" {} ", agent.state.glyph()),
                        Style::default().fg(color_for(&agent.state)),
                    ),
                };
                let cursor_span =
                    Span::styled(marker.to_string(), Style::default().fg(app.theme.focus));
                let unread_span = Span::styled(
                    unread_bar,
                    Style::default()
                        .fg(app.theme.focus)
                        .add_modifier(Modifier::BOLD),
                );
                if minimized {
                    // Rail: cursor (1) + unread bar (1) + " glyph " (3) + a few name chars. No open
                    // marker, age, or attention message — the icon and initials are all that fit.
                    let name_w = (inner.width as usize).saturating_sub(5);
                    let name = format!("{:<name_w$.name_w$}", agent.name);
                    lines.push(Line::from(vec![
                        cursor_span,
                        unread_span,
                        glyph_span,
                        Span::styled(name, name_style),
                    ]));
                    continue;
                }
                // An open agent gets a leading '*'; the marker sits in its own column so names
                // stay aligned whether open or not. The name pads whatever width is left so the
                // dim last-opened age hugs the right edge (the name gives way first on narrow
                // sidebars). Prefix columns: cursor (1) + unread bar (1) + " glyph " (3) + open
                // marker (1) = 6.
                let open_marker = if is_open { "*" } else { " " };
                let age = age_short(agent.last_opened);
                let name_w = (inner.width as usize).saturating_sub(6 + age.len() + 1);
                let name = format!("{:<name_w$.name_w$}", agent.name);
                lines.push(Line::from(vec![
                    cursor_span,
                    unread_span,
                    glyph_span,
                    Span::styled(open_marker, name_style),
                    Span::styled(name, name_style),
                    Span::styled(format!(" {age}"), Style::default().fg(Color::DarkGray)),
                ]));
            }
            Row::Attention(id) => {
                let msg = match by_id.get(&id).map(|a| &a.state) {
                    Some(AgentState::NeedsAttention {
                        message: Some(msg), ..
                    }) => msg.clone(),
                    // The row is only emitted for a blocked agent with a message; keep the line
                    // anyway rather than break the one-row-one-line invariant.
                    _ => String::new(),
                };
                lines.push(Line::from(Span::styled(
                    format!("       {msg}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::DIM),
                )));
            }
        }
    }
    // Only the visible window is handed to the paragraph: a list taller than the sidebar was
    // silently clipped at the border before, which could hide the selection itself.
    let visible: Vec<Line> = lines.into_iter().skip(top).take(height.max(1)).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// Compact "time since" for the sidebar's last-opened column: 45s, 12m, 3h, 2d.
fn age_short(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
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
                    format!(
                        " {} {} ",
                        agent.state.glyph(),
                        agent.branch.as_deref().unwrap_or("HEAD")
                    ),
                    color_for(&agent.state),
                ),
                Some(agent) => (
                    format!(" sh \u{b7} {} ", agent.branch.as_deref().unwrap_or("HEAD")),
                    app.theme.shell,
                ),
                None => (" terminal ".to_string(), Color::DarkGray),
            },
            None => (" empty ".to_string(), Color::DarkGray),
        };
        // Show how far back this pane is scrolled while in scroll mode.
        if let Some(scroll) = app
            .scroll
            .as_ref()
            .filter(|s| Some(s.terminal) == place.payload)
        {
            title.push_str(&format!("\u{2191}{} ", scroll.offset));
        }
        let border = if focused {
            Style::default()
                .fg(app.theme.focus)
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

        // A scrolled-back pane renders the window the daemon served; everything else renders live.
        match place.payload.and_then(|t| app.screen_for(t)) {
            Some(screen) => {
                frame.render_widget(PseudoTerminal::new(screen), inner);
                // After the content, so the OSC 8 markers wrap the cells as drawn.
                mark_links(frame.buffer_mut(), screen, inner);
            }
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
        let cursor = |f: Field| {
            if app.create_field == f {
                "\u{2588}"
            } else {
                ""
            }
        };
        (
            format!(
                " new agent in {repo} — branch: {}{}  task: {}{}  (tab switch \u{b7} enter create \u{b7} task optional)",
                app.create_buf,
                cursor(Field::Branch),
                app.task_buf,
                cursor(Field::Task),
            ),
            Style::default().fg(Color::Black).bg(app.theme.focus),
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
            Style::default().fg(Color::Black).bg(app.theme.focus),
        )
    } else if app.input == InputMode::CreatingHead {
        (
            format!(
                " new HEAD session \u{2014} dir: {}\u{2588}  (enter create \u{b7} esc cancel)",
                app.dir_buf,
            ),
            Style::default().fg(Color::Black).bg(app.theme.focus),
        )
    } else if app.input == InputMode::Confirming {
        (
            format!(" {} — delete anyway? y/n", app.confirm_msg),
            Style::default().fg(Color::White).bg(Color::Red),
        )
    } else if let Some(scroll) = app.scroll.as_ref() {
        // `↑offset/available` — how far back the view is, and how far back it *can* go. Both come
        // from the daemon, so the number never claims history the daemon doesn't have.
        (
            format!(
                " SCROLL \u{2191}{}/{} — j/k line \u{b7} ^u/^d half \u{b7} PgUp/PgDn page \u{b7} g/G top/bottom \u{b7} q done",
                scroll.offset, scroll.available
            ),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else if app.resize_mode {
        (
            " RESIZE — hjkl nudge \u{b7} HJKL snap \u{b7} esc done".to_string(),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else if app.prefix {
        (
            " Ctrl+B — % / \" split \u{b7} x close \u{b7} HJKL/r resize \u{b7} [ scroll \u{b7} tab unread \u{b7} # jump \u{b7} - prev"
                .to_string(),
            Style::default().fg(Color::Black).bg(app.theme.focus),
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
                " n new \u{b7} enter open \u{b7} m mini \u{b7} d del \u{b7} r resume \u{b7} P prune \u{b7} ctrl+jk unread \u{b7} ctrl+q quit"
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

    #[test]
    fn effective_profile_prefers_cli_then_config() {
        use amux_core::config::Profile;
        assert_eq!(
            effective_profile(Some(Profile::Red), Profile::Green),
            Profile::Red
        );
        assert_eq!(effective_profile(None, Profile::Green), Profile::Green);
        assert_eq!(effective_profile(None, Profile::Blue), Profile::Blue);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn output(terminal: TerminalId, byte: u8) -> DaemonMsg {
        DaemonMsg::Output {
            terminal,
            bytes: vec![byte],
        }
    }

    /// A daemon stream that yields every message immediately, then pends once, then ends —
    /// modelling a burst, then quiet, then disconnect, deterministically (no cross-stream timing).
    fn burst_then_close(
        msgs: Vec<DaemonMsg>,
    ) -> impl futures::Stream<Item = Result<DaemonMsg, amux_proto::ProtoError>> + Unpin {
        let mut queue = msgs.into_iter();
        let mut pended = false;
        futures::stream::poll_fn(move |cx| {
            if let Some(m) = queue.next() {
                std::task::Poll::Ready(Some(Ok(m)))
            } else if !pended {
                pended = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(None)
            }
        })
    }

    /// A short socket pair whose write half becomes a real `Sink`; the read half is kept alive so
    /// the loop's `AddRepo` send doesn't hit a broken pipe.
    fn test_sink() -> (Sink, UnixStream) {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        (sink, server_end)
    }

    /// Like [`test_sink`], but the far end is decoded, so a test can assert on the exact frames the
    /// client put on the wire.
    fn test_server() -> (Sink, Framed<UnixStream, amux_proto::ServerCodec>) {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        (
            sink,
            Framed::new(server_end, amux_proto::ServerCodec::new()),
        )
    }

    /// The next frame the client sent, or `None` if it sent nothing (a short timeout, so "nothing
    /// was sent" is a real assertion rather than a hang).
    async fn next_msg(
        server: &mut Framed<UnixStream, amux_proto::ServerCodec>,
    ) -> Option<ClientMsg> {
        tokio::time::timeout(Duration::from_millis(200), server.next())
            .await
            .ok()
            .flatten()
            .map(|m| m.unwrap())
    }

    /// THE regression test: a burst of Output frames must coalesce into a SINGLE render (plus the
    /// initial one), not one render per frame. Old loop drew 1 + 3 = 4; coalesced loop draws 2.
    #[tokio::test]
    async fn output_burst_coalesces_into_one_render() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        let mut daemon = burst_then_close(vec![output(t, b'a'), output(t, b'b'), output(t, b'c')]);
        let mut events = futures::stream::pending::<std::io::Result<Event>>();

        let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let r = renders.clone();
        event_loop(
            &mut app,
            &mut sink,
            &mut daemon,
            &mut events,
            move |_app| {
                r.set(r.get() + 1);
                Ok(())
            },
            std::path::PathBuf::from("/repo"),
        )
        .await
        .unwrap();

        assert_eq!(
            renders.get(),
            2,
            "the 3-frame burst should coalesce into one render (plus the initial frame)"
        );
    }

    /// A single event with nothing else queued renders exactly once — the idle-typing common case
    /// (no added latency, no dropped frame).
    #[tokio::test]
    async fn single_event_renders_once() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        let mut daemon = burst_then_close(vec![output(t, b'x')]);
        let mut events = futures::stream::pending::<std::io::Result<Event>>();

        let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let r = renders.clone();
        event_loop(
            &mut app,
            &mut sink,
            &mut daemon,
            &mut events,
            move |_app| {
                r.set(r.get() + 1);
                Ok(())
            },
            std::path::PathBuf::from("/repo"),
        )
        .await
        .unwrap();

        assert_eq!(
            renders.get(),
            2,
            "initial frame + one render for the single event"
        );
    }

    /// A quit (Ctrl-Q) stops the loop immediately and draws no trailing frame.
    #[tokio::test]
    async fn quit_key_stops_without_trailing_render() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let mut daemon = futures::stream::pending::<Result<DaemonMsg, amux_proto::ProtoError>>();
        let evs: Vec<std::io::Result<Event>> = vec![Ok(Event::Key(ctrl('q')))];
        let mut events = futures::stream::iter(evs);

        let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let r = renders.clone();
        event_loop(
            &mut app,
            &mut sink,
            &mut daemon,
            &mut events,
            move |_app| {
                r.set(r.get() + 1);
                Ok(())
            },
            std::path::PathBuf::from("/repo"),
        )
        .await
        .unwrap();

        assert_eq!(
            renders.get(),
            1,
            "only the initial frame; quit short-circuits before any batch render"
        );
    }

    /// A quit discovered while DRAINING the event stream — behind an earlier non-quit event, so it
    /// is not the Phase-1 winner — still stops the loop with no trailing render. Exercises the
    /// Phase-2 event-drain quit branch (symmetric to the daemon-drain path). `chain(pending())`
    /// keeps the stream open so the quit can only come from applying the Ctrl-Q; the timeout turns
    /// a regression (a dropped quit → the loop blocks forever) into a clean failure instead of a hang.
    #[tokio::test]
    async fn quit_while_draining_events_stops_without_trailing_render() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let mut daemon = futures::stream::pending::<Result<DaemonMsg, amux_proto::ProtoError>>();
        let evs: Vec<std::io::Result<Event>> =
            vec![Ok(Event::FocusGained), Ok(Event::Key(ctrl('q')))];
        let mut events =
            futures::stream::iter(evs).chain(futures::stream::pending::<std::io::Result<Event>>());

        let renders = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let r = renders.clone();
        let run = event_loop(
            &mut app,
            &mut sink,
            &mut daemon,
            &mut events,
            move |_app| {
                r.set(r.get() + 1);
                Ok(())
            },
            std::path::PathBuf::from("/repo"),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("event_loop must terminate when a quit is drained from the event stream")
            .unwrap();

        assert_eq!(
            renders.get(),
            1,
            "a quit drained from the event stream stops the loop with no batch render"
        );
    }

    /// A multi-line paste into a focused pane whose child wants bracketed paste must reach the
    /// daemon as exactly one `Input` frame, wrapped in paste markers (not one frame per character).
    #[tokio::test]
    async fn paste_sends_one_wrapped_input_frame() {
        use amux_proto::ServerCodec;

        // A socket pair: the app writes ClientMsgs on one end; we decode them on the other.
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        let mut server = Framed::new(server_end, ServerCodec::default());

        // A focused pane whose child turned bracketed paste on (DECSET 2004).
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?2004h");
        assert!(
            parser.screen().bracketed_paste(),
            "child requested bracketed paste"
        );
        app.parsers.insert(t, parser);
        app.tree.open(t);
        app.focus = Focus::Panes;

        app.on_paste("multi\nline".to_string(), &mut sink)
            .await
            .unwrap();

        match server.next().await {
            Some(Ok(ClientMsg::Input { terminal, bytes })) => {
                assert_eq!(terminal, t);
                assert_eq!(bytes, b"\x1b[200~multi\rline\x1b[201~".to_vec());
            }
            other => panic!("expected a single Input frame, got {other:?}"),
        }
    }

    /// A pane with a served window in it, as if the daemon had just answered a scroll request.
    fn scrolled_app(offset: usize, available: usize) -> (App, TerminalId) {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        app.terminals.insert(t, AgentId::new());
        app.parsers
            .insert(t, vt100::Parser::new(4, 20, CLIENT_SCROLLBACK));
        app.attached.insert(t, Size { cols: 20, rows: 4 });
        app.on_scroll_view(t, offset, available, b"served window");
        (app, t)
    }

    /// Scroll keys are *steps* sent to the daemon, which owns the history and our place in it. The
    /// deltas are what the vi/less bindings promise: a line, a half page (rows/2), a page, and the
    /// two extremes.
    #[tokio::test]
    async fn scroll_keys_send_relative_steps() {
        let (mut app, t) = scrolled_app(10, 500);
        let (mut sink, mut server) = test_server();
        assert!(app.scroll.is_some(), "precondition: scrolled back");

        for (pressed, expected) in [
            (key(KeyCode::Char('k')), 1),
            (key(KeyCode::Char('j')), -1),
            (ctrl('u'), 2), // half of the 4-row viewport
            (ctrl('d'), -2),
            (key(KeyCode::PageUp), 4),
            (key(KeyCode::PageDown), -4),
            (key(KeyCode::Char('g')), i32::MAX),
            (key(KeyCode::Char('G')), i32::MIN),
        ] {
            app.key_scroll(pressed, t, &mut sink).await.unwrap();
            match next_msg(&mut server).await {
                Some(ClientMsg::Scroll { terminal, lines }) => {
                    assert_eq!((terminal, lines), (t, expected), "for {pressed:?}");
                }
                other => panic!("expected a Scroll step for {pressed:?}, got {other:?}"),
            }
        }
    }

    /// Leaving scroll mode returns to live rendering at once, and tells the daemon to forget the
    /// position it was holding for us — otherwise the next entry would resume where we left off.
    #[tokio::test]
    async fn leaving_scroll_mode_resets_the_daemons_position() {
        let (mut app, t) = scrolled_app(10, 500);
        let (mut sink, mut server) = test_server();

        app.key_scroll(key(KeyCode::Char('q')), t, &mut sink)
            .await
            .unwrap();
        assert!(app.scroll.is_none(), "back to live immediately");
        assert_eq!(
            next_msg(&mut server).await,
            Some(ClientMsg::Scroll {
                terminal: t,
                lines: i32::MIN
            }),
            "and the daemon is told we are live again"
        );
    }

    /// The served window is what gets drawn — not the live screen underneath it. This is the whole
    /// point of daemon-owned history: what you scroll to can be output this client never witnessed.
    #[test]
    fn a_scrolled_pane_renders_the_served_window() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        app.terminals.insert(t, AgentId::new());
        let mut live = vt100::Parser::new(4, 20, CLIENT_SCROLLBACK);
        live.process(b"live output");
        app.parsers.insert(t, live);
        app.attached.insert(t, Size { cols: 20, rows: 4 });

        assert!(
            app.screen_for(t)
                .unwrap()
                .contents()
                .contains("live output"),
            "live until scrolled"
        );
        app.on_scroll_view(t, 7, 500, b"ancient history");
        let shown = app.screen_for(t).unwrap().contents();
        assert!(
            shown.contains("ancient history") && !shown.contains("live output"),
            "a scrolled pane shows the daemon's window, got: {shown:?}"
        );
    }

    /// Reported depth comes from the daemon, so the indicator can't claim history that isn't there.
    #[test]
    fn the_status_bar_shows_served_depth() {
        use ratatui::{backend::TestBackend, Terminal};
        let (app, _t) = scrolled_app(12, 3400);
        let mut term = Terminal::new(TestBackend::new(90, 1)).unwrap();
        term.draw(|f| render_status(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("\u{2191}12/3400"),
            "the indicator shows position and depth, got: {content}"
        );
    }

    fn agent_info(primary: TerminalId) -> AgentInfo {
        AgentInfo {
            id: AgentId::new(),
            repo: amux_core::agent::RepoId::from_canonical_path(std::path::Path::new("/r")),
            name: "a".into(),
            branch: Some("b".into()),
            state: AgentState::Working,
            last_activity: Utc::now(),
            last_opened: Utc::now(),
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
        let _ = app.swap_to_agent(ida);
        assert_eq!(app.tree.payloads(), vec![pa]);
        let sh = TerminalId::new();
        app.terminals.insert(sh, ida);
        app.tree.split(Axis::LeftRight);
        app.tree.open(sh);
        assert_eq!(app.tree.payloads().len(), 2);

        // Switch to B → the main area is B's primary only; A's terminals are not present.
        let _ = app.swap_to_agent(idb);
        assert_eq!(app.tree.payloads(), vec![pb]);
        assert!(!app.tree.payloads().contains(&pa));

        // Switch back to A → its two-pane split (primary + shell) is restored.
        let _ = app.swap_to_agent(ida);
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
        // Right off the right edge of the panes also drops into the leftmost mini (they sit to the
        // right of the main area as well as below it)...
        app.navigate(Dir::Right);
        assert_eq!(app.focus, Focus::Mini(0));
        // ...and left off the leftmost mini re-enters the panes.
        app.navigate(Dir::Left);
        assert_eq!(app.focus, Focus::Panes);

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
            active: true,
        });
        assert_eq!(app.selection_text().unwrap(), "alpha\nbravo");
    }

    #[test]
    fn token_span_grabs_words_paths_and_flags() {
        let row: Vec<String> = "/home/awong/foo.rs --all a"
            .chars()
            .map(|c| c.to_string())
            .collect();
        // A click anywhere in the path grabs the whole path (cells 0..17), slashes and dot kept.
        assert_eq!(token_span(&row, 6), Some((0, 17)));
        assert_eq!(token_span(&row, 0), Some((0, 17)));
        // The space between path and flag is a boundary.
        assert_eq!(token_span(&row, 18), None);
        // The flag keeps its leading dashes (cells 19..23).
        assert_eq!(token_span(&row, 21), Some((19, 23)));
        // A lone trailing letter is a one-cell token.
        assert_eq!(token_span(&row, 25), Some((25, 25)));

        // A URL's query is part of the URL: `?`, `=` and friends keep it whole.
        let q: Vec<String> = "https://h/x?a=1&b=2%20c#f end"
            .chars()
            .map(|c| c.to_string())
            .collect();
        assert_eq!(token_span(&q, 0), Some((0, 24)));
        assert_eq!(token_span(&q, 24), Some((0, 24)));
        assert_eq!(token_span(&q, 25), None, "the space still ends it");
        // …but a comma or semicolon ends a token, as in prose.
        let prose: Vec<String> = "a,b;c".chars().map(|c| c.to_string()).collect();
        assert_eq!(token_span(&prose, 0), Some((0, 0)));
        assert_eq!(token_span(&prose, 2), Some((2, 2)));

        // Punctuation outside the class ends the token: `foo.baz()` stops before `(`.
        let paren: Vec<String> = "foo.baz()".chars().map(|c| c.to_string()).collect();
        assert_eq!(token_span(&paren, 0), Some((0, 6)));
        assert_eq!(token_span(&paren, 7), None);

        // An empty cell (a blank, or a wide glyph's far half) is a boundary.
        let gapped = vec!["a".to_string(), String::new(), "b".to_string()];
        assert_eq!(token_span(&gapped, 0), Some((0, 0)));
        assert_eq!(token_span(&gapped, 1), None);
        // Out of range is not a token.
        assert_eq!(token_span(&gapped, 9), None);
    }

    #[test]
    fn token_selection_maps_the_word_to_screen_cells() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        let mut parser = vt100::Parser::new(6, 40, 100);
        parser.process(b"run /home/awong/foo.rs now\r\n");
        app.parsers.insert(t, parser);
        // Pane content anchored at screen (1,1); the path sits at cells 4..21 → screen cols 5..22.
        let inner = Rect::new(1, 1, 40, 6);
        let sel = app
            .token_selection(t, inner, 11, 1) // screen col 11 = cell 10, the 'a' of awong
            .expect("a token under the cursor");
        assert_eq!((sel.anchor, sel.head), ((5, 1), (22, 1)));
        assert!(sel.is_active());
        app.selection = Some(sel);
        assert_eq!(app.selection_text().unwrap(), "/home/awong/foo.rs");
        // Double-clicking the space after "run" (cell 3 → screen col 4) selects nothing.
        assert!(app.token_selection(t, inner, 4, 1).is_none());
    }

    #[tokio::test]
    async fn double_click_commits_a_token_a_single_click_does_not() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        let mut parser = vt100::Parser::new(6, 40, 100);
        parser.process(b"hello world\r\n");
        app.parsers.insert(t, parser);
        let (mut sink, _server) = test_server();

        let (_, inner) = app.pane_at(40, 10).expect("mouse is over the pane");
        let (col, row) = (inner.x + 2, inner.y); // the third cell of "hello"
        let press = |c, r| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: c,
            row: r,
            modifiers: KeyModifiers::NONE,
        };

        // First press: a bare click selects nothing (no highlight, no copy).
        app.on_mouse(press(col, row), &mut sink).await.unwrap();
        assert!(
            app.selection.is_some_and(|s| !s.is_active()),
            "a single click is not a committed selection"
        );

        // Second press on the same cell within the window: a double-click commits the token.
        app.on_mouse(press(col, row), &mut sink).await.unwrap();
        let sel = app.selection.expect("double-click made a selection");
        assert!(sel.is_active(), "double-click commits a selection");
        assert_eq!(app.selection_text().unwrap(), "hello");
    }

    #[tokio::test]
    async fn mouse_wheel_forwards_to_apps_and_scrolls_others() {
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

        // No mouse mode → the wheel asks the daemon for history, three lines at a time.
        assert!(!app.app_wants_mouse(t));
        let (mut sink, mut server) = test_server();
        app.wheel_scroll(t, true, &mut sink).await.unwrap();
        assert_eq!(
            next_msg(&mut server).await,
            Some(ClientMsg::Scroll {
                terminal: t,
                lines: 3
            })
        );

        // Wheeling down within a step of live lands on it and hands the pane back, so a mouse user
        // never has to press `q` to get their keys back.
        app.on_scroll_view(t, 2, 500, b"window");
        app.wheel_scroll(t, false, &mut sink).await.unwrap();
        assert!(app.scroll.is_none(), "back to live");
        assert_eq!(
            next_msg(&mut server).await,
            Some(ClientMsg::Scroll {
                terminal: t,
                lines: i32::MIN
            })
        );

        // Wheeling down while already live is a no-op, not a stray request.
        app.wheel_scroll(t, false, &mut sink).await.unwrap();
        assert_eq!(next_msg(&mut server).await, None, "nothing sent when live");

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

    /// A pane with nothing to scroll must not capture keystrokes. Scroll mode used to be entered
    /// before anything was known about the history, so a wheel-up on an empty ring parked the TUI at
    /// `↑0` — where `on_key` hands every keystroke to `key_scroll`, and the pane silently stopped
    /// receiving input until the user pressed `q`. Now the wheel only asks, and the mode opens on the
    /// answer, so "no history" costs a message rather than the keyboard.
    #[tokio::test]
    async fn a_pane_with_no_history_never_captures_keys() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        app.terminals.insert(t, AgentId::new());
        app.parsers
            .insert(t, vt100::Parser::new(4, 20, CLIENT_SCROLLBACK));
        app.attached.insert(t, Size { cols: 20, rows: 4 });
        let (mut sink, mut server) = test_server();

        app.wheel_scroll(t, true, &mut sink).await.unwrap();
        assert!(
            app.scroll.is_none(),
            "asking is not entering: keys still belong to the pane while we wait"
        );
        assert!(
            next_msg(&mut server).await.is_some(),
            "the request went out"
        );

        // The daemon reports no history; that must leave the pane in charge of the keyboard.
        app.on_scroll_view(t, 0, 0, b"");
        assert!(app.scroll.is_none(), "still not in scroll mode");
        assert!(!app.info.is_empty(), "and it says why nothing happened");
    }

    /// A left-press inside a Claude-style pane — one on the alternate screen that *also* tracks the
    /// mouse — must still start amux's own selection, exactly like a plain shell pane, so the text
    /// can be highlighted and copied (via OSC 52). amux never forwards left-clicks to pane apps, so
    /// owning the drag for selection costs the app nothing. Regression guard for the mouse-tracking
    /// pane that used to be left unselectable.
    #[tokio::test]
    async fn left_press_starts_selection_even_when_pane_app_tracks_mouse() {
        let (client_end, _server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();

        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        let mut parser = vt100::Parser::new(24, 80, 100);
        // Alternate screen + SGR mouse tracking — the exact combination Claude Code sets up.
        parser.process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        app.parsers.insert(t, parser);

        // Preconditions: the click lands on the pane, and it is the guarded alt-screen+mouse case.
        assert!(
            app.pane_at(40, 10).is_some(),
            "the click must land on the pane"
        );
        assert!(
            app.parsers[&t].screen().alternate_screen() && app.app_wants_mouse(t),
            "precondition: a Claude-like alt-screen, mouse-tracking pane",
        );

        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(press, &mut sink).await.unwrap();

        let sel = app
            .selection
            .expect("a left-press must start a selection even in a mouse-tracking pane");
        assert_eq!(sel.terminal, t);
    }

    /// A no-drag click (anchor == head) must paint no highlight; only a real drag reverse-videos
    /// its span. Regression guard: a plain click used to light up the single cell under the cursor.
    #[test]
    fn click_paints_no_highlight_but_a_drag_does() {
        let inner = Rect::new(1, 1, 10, 3);
        let t = TerminalId::new();
        let any_reversed = |b: &Buffer| {
            (0..b.area.height).any(|dy| {
                (0..b.area.width).any(|dx| {
                    b.cell((b.area.x + dx, b.area.y + dy))
                        .is_some_and(|c| c.style().add_modifier.contains(Modifier::REVERSED))
                })
            })
        };

        let paints = |sel: Selection| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
            highlight_selection(&mut buf, sel);
            any_reversed(&buf)
        };

        // No-drag click (active == false): nothing highlighted, even over a cell.
        assert!(
            !paints(Selection {
                terminal: t,
                inner,
                anchor: (2, 2),
                head: (2, 2),
                active: false,
            }),
            "a no-drag click must not paint a highlight"
        );

        // A drag confined to one cell (active, head == anchor): that single cell is highlighted.
        assert!(
            paints(Selection {
                terminal: t,
                inner,
                anchor: (2, 2),
                head: (2, 2),
                active: true,
            }),
            "a single-cell drag must paint its one cell"
        );

        // A multi-cell drag: the whole span is reverse-videoed.
        assert!(
            paints(Selection {
                terminal: t,
                inner,
                anchor: (2, 2),
                head: (5, 2),
                active: true,
            }),
            "a drag selection must paint its span"
        );
    }

    /// A left click with no drag (Down then Up over a non-blank cell) must NOT copy — no clipboard
    /// write, no "copied" banner. Regression guard: a plain click used to copy the single character
    /// under the cursor and clobber the clipboard.
    #[tokio::test]
    async fn bare_click_does_not_copy() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"hello world\r\n"); // ensure the clicked cell holds a visible glyph
        app.parsers.insert(t, parser);

        // The pane's inner content starts at screen (31, 1) (main area x=30 + border); (31,1) = 'h'.
        // Guard against layout drift making the click miss the pane (which would pass vacuously).
        assert!(
            app.pane_at(31, 1).is_some(),
            "precondition: the click must land inside a pane"
        );
        let click = |kind| MouseEvent {
            kind,
            column: 31,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(click(MouseEventKind::Down(MouseButton::Left)), &mut sink)
            .await
            .unwrap();
        app.on_mouse(click(MouseEventKind::Up(MouseButton::Left)), &mut sink)
            .await
            .unwrap();

        assert!(
            app.info.is_empty(),
            "a no-drag click must not copy (unexpected banner: {:?})",
            app.info
        );
    }

    /// A completed drag-selection must not survive the next unrelated left-press: a fresh press
    /// (e.g. on the sidebar, outside any pane) dismisses it, so the following Up can't re-copy the
    /// stale text. Regression guard for a selection that persisted and re-copied on later clicks.
    #[tokio::test]
    async fn fresh_left_press_clears_a_prior_selection() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        // A completed drag-selection, lingering as it does after a copy until the next action.
        app.selection = Some(Selection {
            terminal: t,
            inner: Rect::new(31, 1, 40, 20),
            anchor: (31, 1),
            head: (35, 1),
            active: true,
        });
        assert!(
            app.selection.is_some_and(|s| s.is_active()),
            "precondition: a live active selection"
        );

        // A left-press on the sidebar (column < SIDEBAR_W, outside any pane) must dismiss it.
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(press, &mut sink).await.unwrap();

        assert!(
            app.selection.is_none(),
            "a fresh left-press must clear a stale selection so its Up can't re-copy"
        );
    }

    /// A left-click anywhere in the sidebar column moves focus to the sidebar — its literal ask,
    /// mirroring how clicking a pane focuses it. Regression guard: the sidebar used to swallow
    /// clicks entirely, leaving focus wherever it happened to be.
    #[tokio::test]
    async fn left_click_in_sidebar_focuses_it() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        app.focus = Focus::Panes;
        let sel = app.sidebar_sel;

        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(press, &mut sink).await.unwrap();

        assert_eq!(
            app.focus,
            Focus::Sidebar,
            "a left-click in the sidebar column must focus the sidebar"
        );
        assert_eq!(
            app.sidebar_sel, sel,
            "focusing by click must not change the sidebar's selection"
        );
    }

    /// The status bar spans the full width, including under the sidebar column. A click there is
    /// not a sidebar click, so it must not steal focus into the sidebar.
    #[tokio::test]
    async fn left_click_on_status_bar_below_sidebar_does_not_focus_it() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        app.focus = Focus::Panes;

        // Row 39 is the status bar (height 40, body is rows 0..=38).
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 39,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(press, &mut sink).await.unwrap();

        assert_eq!(
            app.focus,
            Focus::Panes,
            "a click on the status-bar row must not focus the sidebar"
        );
    }

    /// A drag that stays within a single cell is still a drag — the pointer moved with the button
    /// held, so the one character under it must stay selectable and copyable. Guards single-char
    /// copy against a cell-span definition, which would treat a one-cell selection as a bare click.
    #[tokio::test]
    async fn single_cell_drag_counts_as_a_drag() {
        let (mut sink, _server) = test_sink();
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"hello\r\n");
        app.parsers.insert(t, parser);
        assert!(
            app.pane_at(31, 1).is_some(),
            "precondition: the gesture lands on the pane"
        );

        let at = |kind| MouseEvent {
            kind,
            column: 31,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left)), &mut sink)
            .await
            .unwrap();
        // Press-and-drag without leaving the cell: a real gesture, but head stays == anchor.
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left)), &mut sink)
            .await
            .unwrap();

        let sel = app.selection.expect("the drag keeps the selection");
        assert_eq!(sel.anchor, sel.head, "the pointer never left the cell");
        assert!(
            sel.is_active(),
            "a drag within one cell still counts as a drag (so its one character copies)"
        );
    }

    /// `G` is "bottom", not "exit" — the README lists `q`/`Esc`/`Enter` as the ways out, so landing on
    /// the live view keeps the mode (as tmux's copy mode does) and the keys keep working.
    #[test]
    fn reaching_the_live_view_by_key_stays_in_scroll_mode() {
        let (mut app, t) = scrolled_app(10, 500);
        // `G` lands on the live view: offset 0, but history still exists.
        app.on_scroll_view(t, 0, 500, b"the live screen");
        assert!(
            app.scroll.is_some(),
            "still in scroll mode at the bottom; only q/Esc/Enter leave"
        );
        assert_eq!(app.scroll.as_ref().unwrap().offset, 0);
    }

    /// A reply for one pane must not disturb the window being shown for another.
    #[test]
    fn a_reply_for_another_pane_is_ignored() {
        let (mut app, t) = scrolled_app(10, 500);
        let other = TerminalId::new();
        app.on_scroll_view(other, 0, 0, b"");
        assert!(
            app.scroll.as_ref().is_some_and(|s| s.terminal == t),
            "the other pane's empty history must not close this pane's window"
        );
    }

    /// Switching agents (or closing a pane) detaches its terminal, and the daemon drops the scroll
    /// position it held for us. The window has to go too: keeping it would render stale history on
    /// return, and the next step would jump, because the daemon would re-base it from the live view.
    #[tokio::test]
    async fn detaching_a_scrolled_pane_drops_its_window() {
        let (mut app, ids) = app_with_agents(2);
        let (mut sink, _server) = test_sink();
        app.activate(ids[0], &mut sink).await.unwrap();
        let terminal = app.tree.focused_payload().expect("a pane for the agent");
        app.on_scroll_view(terminal, 12, 500, b"old history");
        assert!(app.scroll.is_some(), "precondition: scrolled back");

        // Switching agents detaches the pane we were scrolling.
        app.activate(ids[1], &mut sink).await.unwrap();
        assert!(
            app.scroll.is_none(),
            "the window went with the pane, so returning shows live output"
        );
    }

    /// A scrolled-back pane shows a still frame: live output keeps flowing into the live parser, but
    /// what is on screen does not move until the user asks for another window. The matching half of
    /// this — that the *next* step still means one line from what you were looking at — is the
    /// daemon's re-basing, covered end-to-end in the daemon's `scroll_step_is_relative_to_the_served_window`.
    #[test]
    fn a_scrolled_pane_holds_still_while_output_arrives() {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.parsers
            .insert(t, vt100::Parser::new(4, 20, CLIENT_SCROLLBACK));
        app.attached.insert(t, Size { cols: 20, rows: 4 });
        app.on_scroll_view(t, 10, 500, b"an old window");

        let before = app.screen_for(t).unwrap().contents();
        for i in 0..20 {
            let bytes = format!("new line {i}\r\n").into_bytes();
            app.parsers.get_mut(&t).unwrap().process(&bytes);
        }
        assert_eq!(
            before,
            app.screen_for(t).unwrap().contents(),
            "the served window is a still frame; new output must not move it"
        );
    }

    fn agent_with(state: AgentState, unread: bool) -> AgentInfo {
        AgentInfo {
            id: AgentId::new(),
            repo: RepoId::from_canonical_path(std::path::Path::new("/r")),
            name: "a".into(),
            branch: Some("b".into()),
            state,
            last_activity: Utc::now(),
            last_opened: Utc::now(),
            unread,
            primary_terminal: TerminalId::new(),
        }
    }

    #[test]
    fn age_short_buckets() {
        let now = Utc::now();
        assert_eq!(age_short(now), "0s");
        assert_eq!(age_short(now - chrono::Duration::seconds(45)), "45s");
        assert_eq!(age_short(now - chrono::Duration::minutes(12)), "12m");
        assert_eq!(age_short(now - chrono::Duration::hours(3)), "3h");
        assert_eq!(age_short(now - chrono::Duration::days(2)), "2d");
    }

    /// The sidebar renders a right-aligned "time since last opened" per agent row.
    #[test]
    fn sidebar_shows_last_opened_age() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 40);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        let mut agent = agent_with(AgentState::Idle, false);
        agent.last_opened = Utc::now() - chrono::Duration::minutes(5);
        app.agents = vec![agent];

        let mut term = Terminal::new(TestBackend::new(30, 8)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("5m"), "age column renders, got: {content}");
    }

    #[test]
    fn sidebar_shows_head_session_label() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 40);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        // A branchless HEAD session: no branch, labeled "HEAD".
        let mut agent = agent_with(AgentState::Idle, false);
        agent.name = "HEAD".into();
        agent.branch = None;
        app.agents = vec![agent];

        let mut term = Terminal::new(TestBackend::new(30, 8)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("HEAD"),
            "HEAD session label renders, got: {content}"
        );
    }

    /// The chrome accent follows the active profile: with a non-default profile, the focused
    /// sidebar border is drawn in that profile's focus color (not the default cyan). Focus
    /// defaults to the sidebar, so `render_sidebar` uses the focus accent for its border.
    #[test]
    fn sidebar_border_uses_the_profile_focus_color() {
        use amux_core::config::Profile;
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(24, 6);
        app.theme = Theme::for_profile(Profile::Green);
        let mut term = Terminal::new(TestBackend::new(24, 6)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let fg = term.backend().buffer().cell((0, 0)).unwrap().fg;
        assert_eq!(
            fg,
            Color::LightGreen,
            "the focused sidebar border should use the profile's focus color"
        );
    }

    /// The sidebar minimizes when a full sidebar would leave the main area with fewer than
    /// `MAIN_W_MIN` columns, and stays full at/above that boundary — the width that both the
    /// layout and pane region key off must switch on exactly that boundary.
    #[test]
    fn sidebar_width_minimizes_when_narrow() {
        let boundary = SIDEBAR_W_FULL + MAIN_W_MIN;
        assert_eq!(sidebar_width(boundary - 1), SIDEBAR_W_MIN);
        assert_eq!(sidebar_width(boundary), SIDEBAR_W_FULL);
        assert_eq!(sidebar_width(200), SIDEBAR_W_FULL);
    }

    /// Regression: the sidebar used to keep its full 30 columns down to an 80-column terminal,
    /// leaving the pane 48 columns — narrow enough that an agent CLI wraps its whole UI. The
    /// chrome yields first: whenever the full sidebar is shown, the pane can hold an agent UI.
    #[test]
    fn a_full_sidebar_never_squeezes_the_pane_below_a_usable_width() {
        for cols in 40u16..=220 {
            let pty = pane_size(main_area(cols, 40));
            if sidebar_width(cols) == SIDEBAR_W_FULL {
                assert!(
                    pty.cols >= AGENT_UI_W_MIN,
                    "{cols}-column terminal keeps the full sidebar but leaves the pane {} columns",
                    pty.cols
                );
            }
        }
        assert_eq!(
            sidebar_width(80),
            SIDEBAR_W_MIN,
            "an 80-column terminal spends its width on the pane, not the sidebar"
        );
        assert!(
            pane_size(main_area(80, 24)).cols >= AGENT_UI_W_MIN,
            "…and that leaves the pane a usable width"
        );
    }

    /// A full mini is half the available band width, clamped to a floor of today's fixed
    /// width and a cap that keeps it a peek. Below the floor → 44; mid-range → half; above
    /// the cap → 80.
    #[test]
    fn mini_width_scales_between_floor_and_cap() {
        assert_eq!(
            mini_width(50),
            MINI_W_MIN,
            "narrow → floor (half of 50 = 25 < 44)"
        );
        assert_eq!(
            mini_width(88),
            MINI_W_MIN,
            "exactly at the floor boundary (44)"
        );
        assert_eq!(mini_width(120), 60, "mid-range → half the available width");
        assert_eq!(
            mini_width(200),
            MINI_W_MAX,
            "wide → cap (half of 200 = 100 > 80)"
        );
        assert_eq!(
            mini_width(160),
            MINI_W_MAX,
            "exactly at the cap boundary (80)"
        );
    }

    /// On the narrow rail the sidebar shows the state glyph and a few name characters but drops
    /// the last-opened age column, which has no room.
    #[test]
    fn minimized_sidebar_truncates_names_and_drops_age() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 40);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        let mut agent = agent_with(AgentState::Idle, false);
        agent.name = "apiserver".into();
        agent.last_opened = Utc::now() - chrono::Duration::minutes(5);
        app.agents = vec![agent];

        let mut term = Terminal::new(TestBackend::new(SIDEBAR_W_MIN, 8)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("apis"),
            "a few name chars render, got: {content}"
        );
        assert!(
            !content.contains("apiserver"),
            "the full name must be truncated on the rail, got: {content}"
        );
        assert!(
            !content.contains("5m"),
            "the age column is dropped on the rail, got: {content}"
        );
    }

    /// `Ctrl+j`/`Ctrl+k` in the sidebar move the selection to the next unread agent below/above,
    /// skipping read agents and repo headers, without wrapping past the ends.
    #[test]
    fn ctrl_jk_jump_between_unread_agents_in_the_sidebar() {
        use amux_core::agent::AttentionKind;
        let mut app = App::new(100, 40);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];

        // s0 is blocked so it pins to the top; the rest order by last_opened (MRU), staggered
        // here so the order is fixed regardless of the unread bits (which no longer reorder
        // non-blocked rows). Mark the 2nd and 4th unread → order is read, unread, read, unread.
        let s0 = agent_with(
            AgentState::NeedsAttention {
                kind: AttentionKind::Question,
                message: None,
            },
            false,
        );
        let mut s1 = agent_with(AgentState::Working, true);
        let mut s2 = agent_with(AgentState::Idle, false);
        let mut s3 = agent_with(AgentState::Exited { code: Some(0) }, true);
        s1.last_opened = Utc::now();
        s2.last_opened = Utc::now() - chrono::Duration::minutes(1);
        s3.last_opened = Utc::now() - chrono::Duration::minutes(2);
        let (id0, id1, id2, id3) = (s0.id, s1.id, s2.id, s3.id);
        // Insertion order scrambled to prove the sort — not the push order — fixes the layout.
        app.agents = vec![s2, s0, s3, s1];

        let order: Vec<AgentId> = app
            .sidebar_rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Agent(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            vec![id0, id1, id2, id3],
            "sidebar order precondition"
        );

        // Ctrl-j from the first (read) agent walks down through the unread ones, then stops.
        app.sidebar_sel = Some(Row::Agent(id0));
        app.jump_unread(true);
        assert_eq!(app.sidebar_sel, Some(Row::Agent(id1)));
        app.jump_unread(true);
        assert_eq!(app.sidebar_sel, Some(Row::Agent(id3)));
        app.jump_unread(true);
        assert_eq!(
            app.sidebar_sel,
            Some(Row::Agent(id3)),
            "no wrap past the last unread"
        );

        // Ctrl-k walks back up through the unread ones, then stops (id0 is read, so nothing above).
        app.jump_unread(false);
        assert_eq!(app.sidebar_sel, Some(Row::Agent(id1)));
        app.jump_unread(false);
        assert_eq!(
            app.sidebar_sel,
            Some(Row::Agent(id1)),
            "no wrap above the first unread"
        );

        // From a repo header, Ctrl-j finds the first unread below it.
        app.sidebar_sel = Some(Row::Repo(repo));
        app.jump_unread(true);
        assert_eq!(app.sidebar_sel, Some(Row::Agent(id1)));
    }

    #[tokio::test]
    async fn drain_ready_collects_all_ready_then_reports_ended() {
        let mut s = futures::stream::iter(vec![1, 2, 3]);
        let mut out = Vec::new();
        let ended = drain_ready(&mut s, &mut out).await;
        assert_eq!(out, vec![1, 2, 3]);
        assert!(ended, "an exhausted iter() reports the stream ended");
    }

    #[tokio::test]
    async fn drain_ready_stops_at_pending_and_leaves_stream_open() {
        // Three ready items, then a source that never yields again.
        let mut s = futures::stream::iter(vec![1, 2, 3]).chain(futures::stream::pending::<i32>());
        let mut out = Vec::new();
        let ended = drain_ready(&mut s, &mut out).await;
        assert_eq!(out, vec![1, 2, 3]);
        assert!(!ended, "a pending tail means still-open, not ended");

        // A second drain finds nothing new and still reports open.
        let mut out2 = Vec::new();
        let ended2 = drain_ready(&mut s, &mut out2).await;
        assert!(out2.is_empty());
        assert!(!ended2);
    }

    #[tokio::test]
    async fn drain_ready_on_empty_pending_collects_nothing() {
        let mut s = futures::stream::pending::<i32>();
        let mut out = Vec::new();
        let ended = drain_ready(&mut s, &mut out).await;
        assert!(out.is_empty());
        assert!(!ended);
    }

    /// An app with `n` idle agents in one repo, staggered so the sidebar order is deterministic
    /// (MRU — newest `last_opened` first). Returns the app plus the agent ids in sidebar order.
    /// The view never hangs past the end of the list, and a list that fits is never scrolled —
    /// `clamp_top` is what keeps a stale `sidebar_top` (agents deleted under it) honest at render.
    #[test]
    fn clamp_top_never_scrolls_past_the_last_row() {
        // (top, len, height, want)
        let cases = [
            (0usize, 30usize, 10usize, 0usize),
            (5, 30, 10, 5),
            (25, 30, 10, 20), // the last screenful, not one row of it
            (99, 30, 10, 20), // a stale top from a list that shrank
            (5, 8, 10, 0),    // fits entirely → never scrolled
            (5, 0, 10, 0),    // empty list
            (5, 30, 0, 5),    // no room to render: nothing to clamp against
        ];
        for (top, len, height, want) in cases {
            assert_eq!(
                clamp_top(top, len, height),
                want,
                "clamp_top({top}, {len}, {height})"
            );
        }
    }

    /// Moving the selection pulls the view the *minimum* distance needed to show it, so `j` at the
    /// bottom edge scrolls one row rather than re-centring and making the whole list jump.
    #[test]
    fn top_showing_pulls_the_view_the_minimum_distance() {
        // (top, sel, height, want)
        let cases = [
            (0usize, 3usize, 10usize, 0usize), // already visible → untouched
            (0, 9, 10, 0),                     // last visible row
            (0, 10, 10, 1),                    // one past the bottom → scroll exactly one
            (0, 29, 10, 20),                   // a jump to the end
            (20, 4, 10, 4),                    // above the view → the selection becomes the top row
            (20, 20, 10, 20),
            (7, 0, 10, 0), // `g`
            (0, 5, 0, 5),  // degenerate height: keep the selection at the top
        ];
        for (top, sel, height, want) in cases {
            assert_eq!(
                top_showing(top, sel, height),
                want,
                "top_showing({top}, {sel}, {height})"
            );
        }
    }

    /// A sidebar taller than the terminal is navigable: the paging keys move the selection and the
    /// view follows it. Regression — the sidebar had no viewport at all, so rows past the bottom
    /// border were clipped and `sidebar_sel` could sit somewhere invisible.
    #[tokio::test]
    async fn paging_keys_walk_a_sidebar_taller_than_the_terminal() {
        // A 12-row terminal: one status row, two borders → 9 visible sidebar rows for 1 repo
        // header + 30 agents.
        let (mut app, ids) = app_with_agents(30);
        app.area = main_area(100, 12);
        app.focus = Focus::Sidebar;
        app.sidebar_sel = Some(Row::Agent(ids[0]));
        app.scroll_sel_into_view();
        let (mut sink, _server) = test_server();
        let page = app.sidebar_page();
        assert_eq!(page, 9, "9 rows of sidebar body in a 12-row terminal");

        let sel_index = |app: &App| {
            let rows = app.sidebar_rows();
            app.sidebar_sel
                .and_then(|s| rows.iter().position(|&r| r == s))
                .expect("a selection")
        };

        app.on_key(key(KeyCode::PageDown), &mut sink).await.unwrap();
        assert_eq!(sel_index(&app), 1 + page, "PageDown moves a full page");
        assert!(
            app.sidebar_top > 0 && sel_index(&app) < app.sidebar_top + page,
            "the view followed: top {}, selection {}",
            app.sidebar_top,
            sel_index(&app)
        );

        app.on_key(ctrl('d'), &mut sink).await.unwrap();
        assert_eq!(
            sel_index(&app),
            1 + page + page / 2,
            "Ctrl+d is half a page"
        );

        app.on_key(key(KeyCode::Char('G')), &mut sink)
            .await
            .unwrap();
        let rows = app.sidebar_rows();
        assert_eq!(sel_index(&app), rows.len() - 1, "G goes to the last row");
        assert_eq!(
            app.sidebar_top,
            rows.len() - page,
            "…and the view shows the final screenful"
        );

        app.on_key(key(KeyCode::Char('g')), &mut sink)
            .await
            .unwrap();
        assert_eq!(sel_index(&app), 0, "g goes back to the first row");
        assert_eq!(app.sidebar_top, 0);
    }

    /// The wheel scrolls the *view* and leaves the selection alone — and the scroll survives a
    /// redraw, which is why `sidebar_top` is stored rather than derived from the selection.
    #[tokio::test]
    async fn a_wheel_over_the_sidebar_scrolls_the_view_only() {
        let (mut app, ids) = app_with_agents(30);
        app.area = main_area(100, 12);
        app.focus = Focus::Sidebar;
        app.sidebar_sel = Some(Row::Agent(ids[0]));
        let (mut sink, _server) = test_server();

        let wheel = |kind: MouseEventKind| MouseEvent {
            kind,
            column: 2, // inside the sidebar: left of the main area
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(wheel(MouseEventKind::ScrollDown), &mut sink)
            .await
            .unwrap();
        assert_eq!(app.sidebar_top, 3, "three rows per notch");
        assert_eq!(
            app.sidebar_sel,
            Some(Row::Agent(ids[0])),
            "the selection stays put"
        );

        // A redraw must not yank the view back to the selection.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 12)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        assert_eq!(app.sidebar_top, 3);

        // But the next `j` does — minimally, so the selection lands on the top visible row rather
        // than the view snapping all the way back to where it started.
        app.on_key(key(KeyCode::Char('j')), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.sidebar_top, 2,
            "moving the selection pulls the view to it"
        );

        app.on_mouse(wheel(MouseEventKind::ScrollUp), &mut sink)
            .await
            .unwrap();
        assert_eq!(app.sidebar_top, 0, "already at the top: nothing to scroll");
    }

    /// What the user actually sees: the selected agent is on screen even when it sits well past the
    /// bottom of a short terminal, and the title says how many rows are hidden each way.
    #[test]
    fn a_short_sidebar_renders_the_selection_and_says_what_is_hidden() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 12);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        app.agents = (0..30)
            .map(|i| {
                let mut a = agent_with(AgentState::Idle, false);
                a.name = format!("agent{i:02}");
                a.last_opened = Utc::now() - chrono::Duration::seconds(i);
                a
            })
            .collect();
        let ids = app.ordered_agent_ids();
        app.sidebar_sel = Some(Row::Agent(ids[25]));
        app.scroll_sel_into_view();

        let mut term = Terminal::new(TestBackend::new(SIDEBAR_W_FULL, 12)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("agent25"),
            "the selected agent must be on screen, got: {content}"
        );
        assert!(
            !content.contains("agent00"),
            "the top of the list has scrolled off, got: {content}"
        );
        assert!(
            content.contains('\u{2191}'),
            "the title reports the rows hidden above, got: {content}"
        );
    }

    /// Every sidebar row must occupy exactly one rendered line, because the viewport indexes rows
    /// while `render_sidebar` slices lines. An attention message used to push an extra line, so the
    /// two spaces drifted apart and the selection could scroll off-screen anyway — the bug the
    /// viewport was supposed to fix.
    #[test]
    fn a_selected_row_is_on_screen_even_below_an_attention_message() {
        use amux_core::agent::AttentionKind;
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 12);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        app.agents = (0..30)
            .map(|i| {
                let mut a = agent_with(
                    if i % 4 == 0 {
                        AgentState::NeedsAttention {
                            kind: AttentionKind::Permission,
                            message: Some(format!("waiting {i:02}")),
                        }
                    } else {
                        AgentState::Idle
                    },
                    false,
                );
                a.name = format!("agent{i:02}");
                a.last_opened = Utc::now() - chrono::Duration::seconds(i);
                a
            })
            .collect();
        let ids = app.ordered_agent_ids();
        app.sidebar_sel = Some(Row::Agent(ids[14]));
        app.scroll_sel_into_view();

        let name = app
            .agents
            .iter()
            .find(|a| a.id == ids[14])
            .map(|a| a.name.clone())
            .unwrap();
        let mut term = Terminal::new(TestBackend::new(SIDEBAR_W_FULL, 12)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains(&name),
            "the selected row {name} must be visible, got: {content}"
        );
    }

    /// `j`/`k` step over the dim trailer rows: an attention message and an empty repo's hint are
    /// context for the row above, not places the cursor can land.
    #[tokio::test]
    async fn moving_the_cursor_skips_the_trailer_rows() {
        use amux_core::agent::AttentionKind;
        let mut app = App::new(100, 40);
        let with_agents = RepoId::from_canonical_path(std::path::Path::new("/r"));
        let empty = RepoId::from_canonical_path(std::path::Path::new("/z"));
        app.repos = vec![
            RepoInfo {
                id: with_agents,
                name: "r".into(),
                path: "/r".into(),
            },
            RepoInfo {
                id: empty,
                name: "z".into(),
                path: "/z".into(),
            },
        ];
        let mut blocked = agent_with(
            AgentState::NeedsAttention {
                kind: AttentionKind::Permission,
                message: Some("may I".into()),
            },
            false,
        );
        blocked.name = "blocked".into();
        let mut idle = agent_with(AgentState::Idle, false);
        idle.name = "idle".into();
        app.agents = vec![blocked, idle];
        app.focus = Focus::Sidebar;
        app.ensure_sidebar_sel();

        // Both trailer rows really are in the row list, so the skipping is load-bearing.
        let rows = app.sidebar_rows();
        assert!(
            rows.iter().any(|r| matches!(r, Row::Attention(_)))
                && rows.iter().any(|r| matches!(r, Row::EmptyRepo(_))),
            "expected both trailer rows, got {rows:?}"
        );

        let (mut sink, _server) = test_server();
        let mut seen = vec![app.sidebar_sel.unwrap()];
        for _ in 0..rows.len() {
            app.on_key(key(KeyCode::Char('j')), &mut sink)
                .await
                .unwrap();
            seen.push(app.sidebar_sel.unwrap());
        }
        for _ in 0..rows.len() {
            app.on_key(key(KeyCode::Char('k')), &mut sink)
                .await
                .unwrap();
            seen.push(app.sidebar_sel.unwrap());
        }
        assert!(
            seen.iter().all(|r| r.selectable()),
            "the cursor landed on a trailer row: {seen:?}"
        );
        // And every selectable row is reachable by walking down.
        let stops: Vec<Row> = rows.into_iter().filter(|r| r.selectable()).collect();
        for stop in stops {
            assert!(seen.contains(&stop), "{stop:?} was never reachable");
        }
    }

    /// Two repos, `a` agents in one and `b` in the other, opened newest-first across both so the
    /// recent block interleaves them. Opens are spread six hours apart, so the first four fall
    /// inside `RECENT_WINDOW` and the rest are older — a list the block is a shortcut *into* rather
    /// than a copy of.
    /// `Ctrl+j`/`Ctrl+k` reach the recent block, and an unread agent listed in *both* the block and
    /// its repo group is one stop, not two — jumping row by row would stutter on the same agent.
    #[tokio::test]
    async fn jumping_unread_covers_the_recent_block_without_repeating_an_agent() {
        let (mut app, _ids) = app_with_two_repos(4, 4);
        app.focus = Focus::Sidebar;
        // agent01 is in the recent block; agent05 is only in a repo group.
        let recent_unread = app.agents.iter().find(|a| a.name == "agent01").unwrap().id;
        let grouped_unread = app.agents.iter().find(|a| a.name == "agent05").unwrap().id;
        for a in app.agents.iter_mut() {
            a.unread = a.id == recent_unread || a.id == grouped_unread;
        }
        let rows = app.sidebar_rows();
        assert!(
            rows.contains(&Row::Recent(recent_unread)) && rows.contains(&Row::Agent(recent_unread)),
            "the unread agent must appear twice for this test to mean anything"
        );

        // Start above everything, then walk down through every unread stop.
        app.sidebar_sel = Some(Row::RecentHeader);
        let (mut sink, _server) = test_server();
        let mut visited = Vec::new();
        for _ in 0..6 {
            app.on_key(ctrl('j'), &mut sink).await.unwrap();
            let sel = app.sidebar_sel.unwrap();
            if visited.last() != Some(&sel) {
                visited.push(sel);
            }
        }
        assert_eq!(
            visited,
            vec![Row::Recent(recent_unread), Row::Agent(grouped_unread)],
            "down: the block's row, then the other repo's — and no second visit to agent01"
        );

        // Up from agent05 the *nearest* unread stop is agent01's row under its repo — its recent
        // row is further away — and from there the second press finds nothing, because both of
        // agent01's rows name the agent already under the cursor.
        let mut back = Vec::new();
        for _ in 0..6 {
            app.on_key(ctrl('k'), &mut sink).await.unwrap();
            let sel = app.sidebar_sel.unwrap();
            if back.last() != Some(&sel) {
                back.push(sel);
            }
        }
        assert_eq!(
            back,
            vec![Row::Agent(recent_unread)],
            "up: the nearest of the agent's two rows, then no further"
        );
    }

    /// Recency is the union of two rules — the last `RECENT_MIN` opened, plus anything opened inside
    /// `RECENT_WINDOW`. Because both are cut on `last_opened`, the union is a prefix of the MRU
    /// list: if the seventh-most-recent is inside the window, the sixth necessarily is too.
    #[test]
    fn recent_count_unions_the_floor_and_the_window() {
        let now = Utc::now();
        let hours = |h: i64| now - chrono::Duration::hours(h);
        // (last_opened ages in hours, want)
        let cases: &[(&[i64], usize)] = &[
            // Nothing inside 24h: the floor of 5 carries the block.
            (&[48, 50, 52, 54, 56, 58, 60], RECENT_MIN),
            // Everything stale and fewer than the floor: all of them, not a padded 5.
            (&[48, 50], 2),
            // Eight inside the window beats the floor.
            (&[1, 2, 3, 4, 5, 6, 7, 8, 40, 44], 8),
            // Exactly at the floor either way.
            (&[1, 2, 3, 4, 5, 90], RECENT_MIN),
            // The window boundary is exclusive of older-than-24h.
            (&[1, 23, 25, 30, 40, 50, 60], RECENT_MIN),
            // The ceiling caps a busy day: fourteen inside the window, ten listed.
            (&[1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7], RECENT_MAX),
            (&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], RECENT_MAX),
            (&[1, 2, 3, 4, 5, 6, 7, 8, 9], 9),
            (&[] as &[i64], 0),
        ];
        for (ages, want) in cases {
            let agents: Vec<AgentInfo> = ages
                .iter()
                .map(|&h| {
                    let mut a = agent_with(AgentState::Idle, false);
                    a.last_opened = hours(h);
                    a
                })
                .collect();
            assert_eq!(
                recent_count(&agents, now),
                *want,
                "recent_count for ages {ages:?}"
            );
        }
    }

    /// The block lists exactly the newest `recent_count` agents, newest first — and a stale agent
    /// only makes it in on the floor, never on the window.
    #[test]
    fn recent_ids_take_the_newest_of_the_union() {
        let now = Utc::now();
        let mut agents: Vec<AgentInfo> = (0..8)
            .map(|i| {
                let mut a = agent_with(AgentState::Idle, false);
                a.name = format!("a{i}");
                // a0..a2 inside the window; a3..a7 days old.
                a.last_opened = if i < 3 {
                    now - chrono::Duration::hours(i + 1)
                } else {
                    now - chrono::Duration::days(i)
                };
                a
            })
            .collect();
        agents.reverse(); // insertion order must not matter
        let names: Vec<String> = recent_ids(&agents, now)
            .into_iter()
            .map(|id| {
                agents
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| a.name.clone())
                    .unwrap()
            })
            .collect();
        assert_eq!(
            names,
            vec!["a0", "a1", "a2", "a3", "a4"],
            "three inside the window, then the floor tops it up to {RECENT_MIN}"
        );
    }

    /// On startup the cursor lands on the newest recent agent, not on a repo header. The daemon
    /// sends repos before agents, so the first `ensure_sidebar_sel` sees a sidebar of bare headers;
    /// once the agents arrive the block appears *above* the selection, and an untouched cursor has
    /// to follow it.
    #[tokio::test]
    async fn the_cursor_starts_on_the_newest_recent_agent() {
        let (reference, _ids) = app_with_two_repos(4, 4);
        let mut app = App::new(100, 40);
        let (mut sink, _server) = test_server();

        // Startup order: repos first, then the roster.
        app.on_daemon(DaemonMsg::Repos(reference.repos.clone()), &mut sink)
            .await
            .unwrap();
        assert!(
            matches!(app.sidebar_sel, Some(Row::Repo(_))),
            "with no agents yet, a repo header is all there is"
        );
        app.on_daemon(DaemonMsg::Agents(reference.agents.clone()), &mut sink)
            .await
            .unwrap();

        let newest = recent_ids(&app.agents, Utc::now())[0];
        assert_eq!(
            app.sidebar_sel,
            Some(Row::Recent(newest)),
            "the cursor moved to the top of the recent block"
        );
        assert_eq!(app.sidebar_top, 0, "and the view is at the top");

        // Once the user has moved the cursor, a roster update must not yank it back.
        app.on_key(key(KeyCode::Char('j')), &mut sink)
            .await
            .unwrap();
        app.on_key(key(KeyCode::Char('j')), &mut sink)
            .await
            .unwrap();
        let chosen = app.sidebar_sel;
        app.on_daemon(DaemonMsg::Agents(reference.agents.clone()), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.sidebar_sel, chosen,
            "a refresh must not steal the cursor"
        );
    }

    /// A pane whose text is too long for its width, so the terminal wrapped it. `cols` is the pane's
    /// inner width; the returned app has one pane covering `inner`.
    fn app_with_wrapped_text(text: &str, cols: u16, rows: u16) -> (App, TerminalId, Rect) {
        let mut app = App::new(100, 40);
        let t = TerminalId::new();
        app.tree.open(t);
        app.terminals.insert(t, AgentId::new());
        let mut parser = vt100::Parser::new(rows, cols, 100);
        parser.process(text.as_bytes());
        app.parsers.insert(t, parser);
        (app, t, Rect::new(0, 0, cols, rows))
    }

    /// A double-clicked token follows the terminal's wrap: a URL too long for the pane occupies
    /// three rows, and clicking any of them yields the whole URL. Before this, `token_span` stopped
    /// at the pane's right edge and copied `https://example.` — a truncation that looks like a
    /// working copy until you paste it.
    #[tokio::test]
    async fn a_double_clicked_url_survives_wrapping() {
        let url = "https://example.com/a/very/long/path/x?q=1";
        let (mut app, t, inner) = app_with_wrapped_text(&format!("see {url} ok"), 20, 6);
        // Confirm the premise: the URL really is spread over three wrapped rows.
        let wrapped: Vec<bool> = (0..3)
            .map(|y| app.parsers[&t].screen().row_wrapped(y))
            .collect();
        assert_eq!(wrapped, vec![true, true, false], "the text must wrap");

        // Clicking the first, middle and last row of the URL all copy the same thing.
        for (col, row) in [(8u16, 0u16), (4, 1), (2, 2)] {
            let sel = app
                .token_selection(t, inner, col, row)
                .unwrap_or_else(|| panic!("a token at ({col},{row})"));
            app.selection = Some(Selection {
                active: true,
                ..sel
            });
            assert_eq!(
                app.selection_text().as_deref(),
                Some(url),
                "double-click at ({col},{row})"
            );
        }

        // The trailing word is its own token — the wrap join must not swallow what follows.
        let sel = app.token_selection(t, inner, 8, 2).expect("the `ok` token");
        app.selection = Some(Selection {
            active: true,
            ..sel
        });
        assert_eq!(app.selection_text().as_deref(), Some("ok"));
    }

    /// A drag across wrapped rows copies one unbroken line: the terminal broke it to fit the pane,
    /// and re-inserting that break would corrupt a URL (or any long token) on paste. A break the
    /// program itself printed is still a newline.
    #[tokio::test]
    async fn dragging_across_a_wrap_copies_one_line() {
        let url = "https://example.com/a/very/long/path/x?q=1";
        let (mut app, t, inner) = app_with_wrapped_text(&format!("see {url} ok"), 20, 6);
        app.selection = Some(Selection {
            terminal: t,
            inner,
            anchor: (0, 0),
            head: (8, 2),
            active: true,
        });
        assert_eq!(
            app.selection_text().as_deref(),
            Some(&format!("see {url} ok")[..]),
            "wrapped rows join with nothing between them"
        );

        // Two genuinely separate lines still get their newline.
        let (mut app, t, inner) = app_with_wrapped_text("alpha\r\nbeta\r\n", 20, 6);
        app.selection = Some(Selection {
            terminal: t,
            inner,
            anchor: (0, 0),
            head: (3, 1),
            active: true,
        });
        assert_eq!(app.selection_text().as_deref(), Some("alpha\nbeta"));
    }

    /// Finding URLs in a logical line: anchored on a scheme, ended by whitespace, with trailing
    /// punctuation left outside — a URL at the end of a sentence must not swallow the period.
    /// The printable text of a cell symbol, with any OSC 8 sequence removed — what the outer
    /// terminal actually shows.
    fn strip_osc8(symbol: &str) -> String {
        let mut out = String::new();
        let mut rest = symbol;
        while let Some(i) = rest.find("\x1b]8;") {
            out.push_str(&rest[..i]);
            let after = &rest[i..];
            match after.find("\x1b\\") {
                Some(j) => rest = &after[j + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn urls_in_line_finds_schemes_and_trims_trailing_punctuation() {
        // (line, expected matches)
        let cases: &[(&str, &[&str])] = &[
            ("see https://example.com/x ok", &["https://example.com/x"]),
            (
                "http://a.b and https://c.d/e",
                &["http://a.b", "https://c.d/e"],
            ),
            // Trailing sentence punctuation is not part of the URL…
            ("go to https://example.com/x.", &["https://example.com/x"]),
            ("(see https://example.com/x)", &["https://example.com/x"]),
            ("\"https://example.com/x\",", &["https://example.com/x"]),
            // …but a path may legitimately end in one of those characters mid-URL.
            ("https://e.com/a)b end", &["https://e.com/a)b"]),
            // A query keeps its parameters.
            ("https://e.com/x?a=1&b=2#f", &["https://e.com/x?a=1&b=2#f"]),
            // Not a URL.
            ("no links here", &[]),
            ("ftp://unsupported.example", &[]),
            ("say https:// alone", &[]),
            ("", &[]),
        ];
        for (line, want) in cases {
            let got: Vec<&str> = urls_in_line(line).into_iter().map(|r| &line[r]).collect();
            assert_eq!(got, *want, "urls_in_line({line:?})");
        }
    }

    /// A URL that wrapped maps back to one run of cells per screen row it covers — the shape OSC 8
    /// needs, since each run has to carry the whole URI.
    #[test]
    fn a_wrapped_url_maps_to_one_run_per_row() {
        let url = "https://example.com/a/very/long/path/x?q=1";
        let (app, t, inner) = app_with_wrapped_text(&format!("see {url} ok"), 20, 6);
        let screen = app.parsers[&t].screen();
        let links = pane_links(screen, inner.width, inner.height);

        assert_eq!(links.len(), 1, "one link, got {links:?}");
        let link = &links[0];
        assert_eq!(link.url, url);
        assert_eq!(
            link.runs,
            // row 0 from col 4 to the edge, then the two continuation rows.
            vec![(0u16, 4u16, 19u16), (1, 0, 19), (2, 0, 5)],
            "runs were {:?}",
            link.runs
        );
        // The runs must cover exactly the URL's characters and nothing else.
        let covered: String = link
            .runs
            .iter()
            .flat_map(|&(y, x0, x1)| (x0..=x1).map(move |x| (y, x)))
            .map(|(y, x)| {
                screen
                    .cell(y, x)
                    .map(|c| c.contents().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(covered, url, "the runs cover exactly the URL");
    }

    /// The rendered buffer carries OSC 8 sequences so the outer terminal knows the link exists
    /// rather than guessing from its own grid — where a wrapped URL is two fragments with a pane
    /// border between them, which is why clicking one opened a truncated address.
    #[test]
    fn a_wrapped_url_renders_as_one_osc8_hyperlink() {
        use ratatui::{backend::TestBackend, Terminal};
        let url = "https://example.com/a/very/long/path/x?q=1";
        let (app, t, inner) = app_with_wrapped_text(&format!("see {url} ok"), 20, 6);
        let mut term = Terminal::new(TestBackend::new(20, 6)).unwrap();
        term.draw(|f| {
            let screen = app.screen_for(t).unwrap();
            f.render_widget(PseudoTerminal::new(screen), inner);
            mark_links(f.buffer_mut(), screen, inner);
        })
        .unwrap();

        let buf = term.backend().buffer();
        let opens: Vec<String> = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .filter(|sym| sym.contains("\x1b]8;"))
            .collect();
        assert_eq!(
            opens.len(),
            6,
            "three rows, each opening and closing a run: {opens:?}"
        );
        // Every run carries the *whole* URL, so clicking any row opens the same address.
        let carrying = opens.iter().filter(|s| s.contains(url)).count();
        assert_eq!(
            carrying, 3,
            "each row's opener names the full URL: {opens:?}"
        );
        // A shared id ties the runs together, so hover highlights all three rows as one link.
        assert!(
            opens.iter().filter(|s| s.contains("id=")).count() >= 3,
            "runs share an id: {opens:?}"
        );
        // The visible text is unchanged: stripping the sequences back out returns the original row,
        // so the link markers ride along with the characters rather than replacing any.
        let row0: String = (0..20).map(|x| strip_osc8(buf[(x, 0)].symbol())).collect();
        assert_eq!(
            row0, "see https://example.",
            "the row still reads as its own text"
        );
    }

    /// Two links on one screen get distinct ids, so hovering one does not highlight the other.
    #[test]
    fn separate_links_get_separate_ids() {
        let (app, t, inner) =
            app_with_wrapped_text("a https://one.example/x b https://two.example/y", 60, 4);
        let links = pane_links(app.parsers[&t].screen(), inner.width, inner.height);
        assert_eq!(links.len(), 2, "got {links:?}");
        assert_eq!(links[0].url, "https://one.example/x");
        assert_eq!(links[1].url, "https://two.example/y");

        use ratatui::{backend::TestBackend, Terminal};
        let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
        term.draw(|f| {
            let screen = app.screen_for(t).unwrap();
            f.render_widget(PseudoTerminal::new(screen), inner);
            mark_links(f.buffer_mut(), screen, inner);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let ids: Vec<String> = buf
            .content()
            .iter()
            .filter_map(|c| {
                let sym = c.symbol();
                sym.find("id=")
                    .map(|i| sym[i..].split(';').next().unwrap_or_default().to_string())
            })
            .collect();
        assert_eq!(ids, vec!["id=0", "id=1"], "one id per link: {ids:?}");
    }

    /// A pane with no links is left exactly as the content widget drew it — `mark_links` is only
    /// allowed to add sequences where a URL actually is.
    #[test]
    fn a_pane_without_links_is_untouched() {
        use ratatui::{backend::TestBackend, Terminal};
        let (app, t, inner) =
            app_with_wrapped_text("just some ordinary output\r\nno urls\r\n", 20, 5);

        let render = |mark: bool| {
            let mut term = Terminal::new(TestBackend::new(20, 5)).unwrap();
            term.draw(|f| {
                let screen = app.screen_for(t).unwrap();
                f.render_widget(PseudoTerminal::new(screen), inner);
                if mark {
                    mark_links(f.buffer_mut(), screen, inner);
                }
            })
            .unwrap();
            term.backend().buffer().clone()
        };
        assert_eq!(render(false), render(true));
    }

    fn app_with_two_repos(a: usize, b: usize) -> (App, Vec<AgentId>) {
        let mut app = App::new(100, 40);
        let r = RepoId::from_canonical_path(std::path::Path::new("/r"));
        let z = RepoId::from_canonical_path(std::path::Path::new("/z"));
        app.repos = vec![
            RepoInfo {
                id: r,
                name: "r".into(),
                path: "/r".into(),
            },
            RepoInfo {
                id: z,
                name: "z".into(),
                path: "/z".into(),
            },
        ];
        app.agents = (0..(a + b))
            .map(|i| {
                let mut agent = agent_with(AgentState::Idle, false);
                agent.name = format!("agent{i:02}");
                agent.repo = if i < a { r } else { z };
                agent.last_opened = Utc::now() - chrono::Duration::hours(6 * i as i64);
                agent
            })
            .collect();
        let ids = app.ordered_agent_ids();
        (app, ids)
    }

    /// The recent block is global MRU by `last_opened` — deliberately not `sort_for_sidebar`, whose
    /// blocked-first rule would make "recent" mean something else.
    #[test]
    fn recent_ids_are_global_mru() {
        let repo_a = RepoId::from_canonical_path(std::path::Path::new("/a"));
        let repo_b = RepoId::from_canonical_path(std::path::Path::new("/b"));
        let mut agents: Vec<AgentInfo> = (0..5)
            .map(|i| {
                let mut a = agent_with(AgentState::Idle, false);
                a.name = format!("a{i}");
                a.repo = if i % 2 == 0 { repo_a } else { repo_b };
                // a0 newest … a4 oldest.
                a.last_opened = Utc::now() - chrono::Duration::minutes(i);
                a
            })
            .collect();
        // A blocked agent that was opened long ago must not be dragged to the front.
        agents[4].state = AgentState::NeedsAttention {
            kind: amux_core::agent::AttentionKind::Permission,
            message: None,
        };
        let names = |ids: Vec<AgentId>| -> Vec<String> {
            ids.into_iter()
                .map(|id| {
                    agents
                        .iter()
                        .find(|a| a.id == id)
                        .map(|a| a.name.clone())
                        .unwrap()
                })
                .collect()
        };
        assert_eq!(
            names(recent_ids(&agents, Utc::now())),
            vec!["a0", "a1", "a2", "a3", "a4"],
            "strict MRU: the long-idle blocked agent stays last"
        );
        assert!(recent_ids(&[], Utc::now()).is_empty());
    }

    /// The block earns its five rows or it does not appear: with one repo the roster is already MRU,
    /// with three agents it would copy its own top, on a short sidebar the roster matters more, and
    /// the rail has no room for `repo/branch` names.
    #[test]
    fn show_recent_only_when_it_earns_its_rows() {
        // (repos_with_agents, agents, recent, body_height, minimized, want)
        let cases = [
            (2usize, 8usize, 5usize, 20usize, false, true),
            (1, 8, 5, 20, false, false), // single repo: the roster is already MRU
            // The block holds every agent there is — it *is* the list, printed twice. The guard
            // that carries the unbounded 24-hour window.
            (2, 5, 5, 20, false, false),
            (2, 20, 20, 40, false, false),
            (2, 6, 5, 20, false, true), // one agent the block does not hold
            // Height: the block costs recent + 2, and ROSTER_MIN_ROWS must be left over.
            (2, 8, 5, 11, false, false),
            (2, 8, 5, 12, false, true),
            // A bigger block needs a taller sidebar for the same roster allowance. `recent` never
            // exceeds RECENT_MAX, so a full block is the tallest this gets.
            (2, 30, RECENT_MAX, 16, false, false),
            (2, 30, RECENT_MAX, 17, false, true),
            (2, 8, 5, 20, true, false), // the rail
        ];
        for (repos, agents, recent, height, minimized, want) in cases {
            assert_eq!(
                show_recent(repos, agents, recent, height, minimized),
                want,
                "show_recent({repos}, {agents}, {recent}, {height}, {minimized})"
            );
        }
    }

    /// A two-repo sidebar leads with the recent block, then the repo groups unchanged. The header
    /// and divider are trailer rows, so the cursor steps over them.
    #[test]
    fn the_recent_block_leads_the_sidebar() {
        let (app, _ids) = app_with_two_repos(4, 4);
        let rows = app.sidebar_rows();
        let n = recent_ids(&app.agents, Utc::now()).len();
        assert_eq!(n, RECENT_MIN, "four inside the window, floored up to five");
        let kinds: Vec<&str> = rows
            .iter()
            .take(n + 2)
            .map(|r| match r {
                Row::RecentHeader => "header",
                Row::Recent(_) => "recent",
                Row::Divider => "divider",
                Row::Repo(_) => "repo",
                _ => "other",
            })
            .collect();
        let mut want = vec!["header"];
        want.extend(std::iter::repeat_n("recent", n));
        want.push("divider");
        assert_eq!(kinds, want, "rows were {rows:?}");
        assert!(
            matches!(rows[n + 2], Row::Repo(_)),
            "then the first repo group"
        );
        assert!(
            rows.iter().any(|r| matches!(r, Row::Agent(_))),
            "the grouped roster is still there in full"
        );
        assert!(
            !Row::RecentHeader.selectable() && !Row::Divider.selectable(),
            "the block's chrome is not a cursor stop"
        );
    }

    /// A recent row is a real agent row: `Enter` opens it, and the agent commands read it the same
    /// way they read a grouped row.
    #[test]
    fn a_recent_row_is_actionable() {
        let (mut app, _ids) = app_with_two_repos(4, 4);
        let rows = app.sidebar_rows();
        let Row::Recent(id) = rows[1] else {
            panic!("row 1 is a recent row, got {:?}", rows[1])
        };
        app.sidebar_sel = Some(rows[1]);
        assert_eq!(app.selected_agent(), Some(id));
        assert_eq!(
            app.selected_repo(),
            app.agents.iter().find(|a| a.id == id).map(|a| a.repo)
        );
    }

    /// Digits number what you see, top-down, and an agent listed twice is numbered once — in the
    /// recent block. So `1` is the agent you last opened, and the roster continues from there.
    #[test]
    fn digits_number_the_recent_block_first_and_never_twice() {
        let (app, _ids) = app_with_two_repos(4, 4);
        let recent = recent_ids(&app.agents, Utc::now());
        let ordered = app.ordered_agent_ids();
        assert_eq!(
            ordered[..recent.len()],
            recent[..],
            "the first digits are the recent block"
        );
        let mut seen = std::collections::HashSet::new();
        for id in &ordered {
            assert!(seen.insert(*id), "agent numbered twice");
        }
        assert_eq!(ordered.len(), app.agents.len(), "every agent gets a number");
        assert_eq!(app.numbered_agent('1'), Some(recent[0]));
    }

    /// What the user sees: the block, its `repo/branch` names, and no block at all in a
    /// single-repo sidebar.
    #[test]
    fn the_recent_block_renders_qualified_names() {
        use ratatui::{backend::TestBackend, Terminal};
        let (app, _ids) = app_with_two_repos(4, 4);
        let mut term = Terminal::new(TestBackend::new(SIDEBAR_W_FULL, 24)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("recent"),
            "the block's header, got: {content}"
        );
        assert!(
            content.contains("r/agent") || content.contains("z/agent"),
            "recent rows name the repo, got: {content}"
        );

        let (single, _) = app_with_agents(8);
        let mut term = Terminal::new(TestBackend::new(SIDEBAR_W_FULL, 24)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &single)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            !content.contains("recent"),
            "one repo is already MRU — no block, got: {content}"
        );
    }

    fn app_with_agents(n: usize) -> (App, Vec<AgentId>) {
        let mut app = App::new(100, 40);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        app.agents = (0..n)
            .map(|i| {
                let mut a = agent_with(AgentState::Idle, false);
                a.last_opened = Utc::now() - chrono::Duration::seconds(i as i64);
                a
            })
            .collect();
        let ids = app.ordered_agent_ids();
        (app, ids)
    }

    /// The overlay digit and its inverse round-trip: rows 0..=8 → `'1'..='9'`, the tenth (index
    /// 9) → `'0'`, and nothing past the tenth.
    #[test]
    fn overlay_digit_and_digit_index_round_trip() {
        assert_eq!(overlay_digit(0), Some('1'));
        assert_eq!(overlay_digit(8), Some('9'));
        assert_eq!(overlay_digit(9), Some('0'));
        assert_eq!(overlay_digit(10), None);
        for i in 0..10 {
            let d = overlay_digit(i).unwrap();
            assert_eq!(digit_index(d), Some(i), "digit {d} maps back to index {i}");
        }
        assert_eq!(digit_index('a'), None);
    }

    /// `Cmd+digit` selects the labelled agent and pulls focus to the sidebar, from any focus.
    #[tokio::test]
    async fn cmd_digit_opens_the_labelled_agent() {
        let (mut app, ids) = app_with_agents(3);
        let (mut sink, _server) = test_sink();
        let cmd2 = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::SUPER);
        app.on_key(cmd2, &mut sink).await.unwrap();
        assert_eq!(
            app.active_agent,
            Some(ids[1]),
            "the agent opens in the main area"
        );
        assert!(
            matches!(app.focus, Focus::Panes),
            "focus moves into the session"
        );
    }

    /// `Cmd+digit` past the last agent is still consumed (never leaks a digit to the PTY) and
    /// opens nothing.
    #[tokio::test]
    async fn cmd_digit_past_last_agent_is_a_consumed_noop() {
        let (mut app, _ids) = app_with_agents(2);
        let (mut sink, _server) = test_sink();
        let cmd9 = KeyEvent::new(KeyCode::Char('9'), KeyModifiers::SUPER);
        let flow = app.on_key(cmd9, &mut sink).await.unwrap();
        assert!(matches!(flow, Flow::Continue));
        assert_eq!(app.active_agent, None, "out-of-range digit opened nothing");
    }

    /// `Ctrl+B` arms the prefix (lighting the overlay); the next digit opens that agent's session
    /// and disarms it, mirroring tmux's `prefix` + N.
    #[tokio::test]
    async fn ctrl_b_prefix_then_digit_opens() {
        let (mut app, ids) = app_with_agents(3);
        let (mut sink, _server) = test_sink();
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        assert!(app.prefix, "Ctrl+B arms the prefix");
        assert!(
            app.numeric_overlay_active(),
            "the overlay is up while the prefix is armed"
        );
        app.on_key(key(KeyCode::Char('3')), &mut sink)
            .await
            .unwrap();
        assert!(!app.prefix, "the digit disarms the prefix");
        assert_eq!(
            app.active_agent,
            Some(ids[2]),
            "the agent opens in the main area"
        );
        assert!(matches!(app.focus, Focus::Panes));
    }

    /// A non-digit after the prefix runs its own binding (or nothing) and disarms — nothing opens.
    #[tokio::test]
    async fn ctrl_b_prefix_non_digit_does_not_open() {
        let (mut app, _ids) = app_with_agents(3);
        let (mut sink, _server) = test_sink();
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        app.on_key(key(KeyCode::Esc), &mut sink).await.unwrap();
        assert!(!app.prefix);
        assert_eq!(
            app.active_agent, None,
            "Esc after the prefix opened nothing"
        );
    }

    /// Drive the `n` create form: type into the focused field, `Tab` to switch, `Enter` submit.
    /// Returns the `ClientMsg` the app sent, or `None` if it sent nothing.
    /// Restoring a layout the daemon persisted across a restart must bring the split back *live*.
    /// The daemon blanks every leaf whose PTY died with it, so without a refill the user would get
    /// their geometry back as a dead " empty " pane — worse than the collapse it replaced.
    #[tokio::test]
    async fn restoring_a_saved_layout_respawns_its_shells() {
        use amux_proto::ServerCodec;
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        let mut server = Framed::new(server_end, ServerCodec::default());

        let (mut app, ids) = app_with_agents(1);
        let id = ids[0];
        let primary = app
            .agents
            .iter()
            .find(|a| a.id == id)
            .unwrap()
            .primary_terminal;

        // Exactly what the daemon replays after a restart: the geometry, shell leaf blanked.
        app.saved_layouts.insert(
            id,
            amux_proto::Layout::Split {
                axis: Axis::LeftRight,
                ratio: 0.5,
                first: Box::new(amux_proto::Layout::Leaf {
                    terminal: Some(primary),
                }),
                second: Box::new(amux_proto::Layout::Leaf { terminal: None }),
            },
        );

        app.activate(id, &mut sink).await.unwrap();

        // The split is back, with both panes holding a terminal.
        let payloads = app.tree.payloads();
        assert_eq!(payloads.len(), 2, "the split geometry is restored");
        assert!(payloads.contains(&primary), "the primary kept its terminal");
        let refilled = *payloads.iter().find(|t| **t != primary).unwrap();
        assert_eq!(
            app.terminals.get(&refilled),
            Some(&id),
            "the new terminal is registered to this agent"
        );

        // And a shell was actually requested for it, in the agent's worktree (`like` = primary).
        let mut spawn = None;
        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(Duration::from_millis(200), server.next()).await
        {
            if let ClientMsg::SpawnShell { terminal, like } = msg {
                spawn = Some((terminal, like));
                break;
            }
        }
        assert_eq!(
            spawn,
            Some((refilled, primary)),
            "a SpawnShell must go out for the refilled pane"
        );
    }

    /// The restored split renders as two live panes — the agent's primary and a shell — with no
    /// " empty " placeholder left over. This is what the user actually sees after a reinstall.
    #[tokio::test]
    async fn a_restored_split_renders_as_two_live_panes() {
        use ratatui::{backend::TestBackend, Terminal};
        let (client_end, _server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();

        let (mut app, ids) = app_with_agents(1);
        let id = ids[0];
        let primary = app
            .agents
            .iter()
            .find(|a| a.id == id)
            .unwrap()
            .primary_terminal;
        app.saved_layouts.insert(
            id,
            amux_proto::Layout::Split {
                axis: Axis::LeftRight,
                ratio: 0.5,
                first: Box::new(amux_proto::Layout::Leaf {
                    terminal: Some(primary),
                }),
                second: Box::new(amux_proto::Layout::Leaf { terminal: None }),
            },
        );
        app.activate(id, &mut sink).await.unwrap();

        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term.draw(|f| render_panes(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(
            !content.contains("empty"),
            "no dead placeholder pane should survive the restore, got: {content}"
        );
        assert!(
            content.contains("sh \u{b7}"),
            "the refilled pane renders as a shell, got: {content}"
        );
    }

    /// The counterpart guard: a blank pane in *this session's* live tree is a split whose
    /// `SpawnShell` is still in flight. Refilling it would spawn a second shell for one pane.
    #[tokio::test]
    async fn switching_back_to_a_live_tree_does_not_respawn_its_pending_pane() {
        use amux_proto::ServerCodec;
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        let mut server = Framed::new(server_end, ServerCodec::default());

        let (mut app, ids) = app_with_agents(2);
        app.activate(ids[0], &mut sink).await.unwrap();
        // Split without letting the shell arrive: the new pane is blank and pending.
        app.tree.split(Axis::LeftRight);
        assert!(app.tree.focused_payload().is_none(), "pane is pending");

        // Switch away and back — the live tree is stashed and restored verbatim.
        app.activate(ids[1], &mut sink).await.unwrap();
        app.activate(ids[0], &mut sink).await.unwrap();

        let mut spawns = 0;
        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(Duration::from_millis(200), server.next()).await
        {
            if matches!(msg, ClientMsg::SpawnShell { .. }) {
                spawns += 1;
            }
        }
        assert_eq!(spawns, 0, "a pending pane must not be refilled");
    }

    async fn run_create_form(keys: &str, tab_at: Option<usize>) -> Option<ClientMsg> {
        use amux_proto::ServerCodec;
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        let mut server = Framed::new(server_end, ServerCodec::default());

        let (mut app, ids) = app_with_agents(1);
        app.focus = Focus::Sidebar;
        app.sidebar_sel = Some(Row::Agent(ids[0]));
        app.on_key(key(KeyCode::Char('n')), &mut sink)
            .await
            .unwrap();
        assert_eq!(app.input, InputMode::Creating, "`n` opens the create form");

        for (i, c) in keys.chars().enumerate() {
            if tab_at == Some(i) {
                app.on_key(key(KeyCode::Tab), &mut sink).await.unwrap();
            }
            app.on_key(key(KeyCode::Char(c)), &mut sink).await.unwrap();
        }
        app.on_key(key(KeyCode::Enter), &mut sink).await.unwrap();

        tokio::time::timeout(Duration::from_millis(200), server.next())
            .await
            .ok()
            .flatten()
            .map(|m| m.unwrap())
    }

    /// Dispatch: branch in the first field, task in the second, both reach the daemon in one
    /// message — the whole point of the feature.
    #[tokio::test]
    async fn create_form_dispatches_branch_and_task() {
        // "fix-x" then Tab then "do it"
        let sent = run_create_form("fix-xdo it", Some(5)).await;
        assert_eq!(
            sent,
            Some(ClientMsg::CreateAgent {
                repo: RepoId::from_canonical_path(std::path::Path::new("/r")),
                branch: "fix-x".into(),
                prompt: Some("do it".into()),
            })
        );
    }

    /// The conversational flow is untouched: no task typed → no prompt on the wire.
    #[tokio::test]
    async fn create_form_without_a_task_sends_no_prompt() {
        let sent = run_create_form("fix-x", None).await;
        assert_eq!(
            sent,
            Some(ClientMsg::CreateAgent {
                repo: RepoId::from_canonical_path(std::path::Path::new("/r")),
                branch: "fix-x".into(),
                prompt: None,
            })
        );
    }

    /// A branch is still required — a task alone creates nothing (cancel-on-empty is unchanged).
    #[tokio::test]
    async fn create_form_still_requires_a_branch() {
        // Tab straight to the task field, type only there.
        let sent = run_create_form("do it", Some(0)).await;
        assert_eq!(sent, None, "no branch, no agent");
    }

    /// Both fields are visible in the prompt, so the form is discoverable.
    #[test]
    fn create_form_renders_both_fields() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 40);
        let repo = RepoId::from_canonical_path(std::path::Path::new("/r"));
        app.repos = vec![RepoInfo {
            id: repo,
            name: "r".into(),
            path: "/r".into(),
        }];
        app.input = InputMode::Creating;
        app.create_repo = Some(repo);
        app.create_field = Field::Branch;
        app.create_buf = "fix-x".into();
        app.task_buf = "do it".into();

        let mut term = Terminal::new(TestBackend::new(90, 1)).unwrap();
        term.draw(|f| render_status(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("branch: fix-x") && content.contains("task: do it"),
            "both fields render, got: {content}"
        );
    }

    /// Drive the sidebar from a clean start: focus it with the cursor on the one agent, press
    /// `keys`, and return the first message that reached the daemon (`None` if none did).
    async fn run_sidebar(keys: &[KeyCode]) -> (App, Option<ClientMsg>) {
        use amux_proto::ServerCodec;
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (mut sink, _rx) = Framed::new(client_end, ClientCodec::default()).split();
        let mut server = Framed::new(server_end, ServerCodec::default());

        let (mut app, ids) = app_with_agents(1);
        app.focus = Focus::Sidebar;
        app.sidebar_sel = Some(Row::Agent(ids[0]));
        for k in keys {
            app.on_key(key(*k), &mut sink).await.unwrap();
        }
        let sent = tokio::time::timeout(Duration::from_millis(200), server.next())
            .await
            .ok()
            .flatten()
            .map(|m| m.unwrap());
        (app, sent)
    }

    fn chars(s: &str) -> Vec<KeyCode> {
        s.chars().map(KeyCode::Char).collect()
    }

    /// `h` is the no-prompt HEAD session in the repo under the cursor — what `H` used to do.
    #[tokio::test]
    async fn lowercase_h_opens_head_in_the_selected_repo() {
        let (app, sent) = run_sidebar(&[KeyCode::Char('h')]).await;
        assert_eq!(app.input, InputMode::Normal, "`h` opens no prompt");
        assert_eq!(
            sent,
            Some(ClientMsg::CreateHeadAgent {
                repo: RepoId::from_canonical_path(std::path::Path::new("/r")),
            })
        );
    }

    /// `H` is to `h` as `N` is to `n`: a one-field prompt that names the repo by path.
    #[tokio::test]
    async fn uppercase_h_prompts_for_a_dir_then_creates_head_there() {
        let mut keys = vec![KeyCode::Char('H')];
        keys.extend(chars("/repos/other"));
        keys.push(KeyCode::Enter);
        let (app, sent) = run_sidebar(&keys).await;
        assert_eq!(app.input, InputMode::Normal, "Enter closes the prompt");
        assert_eq!(
            sent,
            Some(ClientMsg::CreateHeadAgentAt {
                path: "/repos/other".into(),
            }),
            "the typed path goes out as a by-path HEAD session — not the selected repo"
        );
    }

    /// The `H` prompt expands `~/` like `N`'s does, and cancels on an empty field or `Esc`.
    #[tokio::test]
    async fn head_prompt_expands_tilde_and_cancels_when_empty() {
        let mut keys = vec![KeyCode::Char('H')];
        keys.extend(chars("~/x"));
        keys.push(KeyCode::Enter);
        let (_, sent) = run_sidebar(&keys).await;
        let home = directories::BaseDirs::new().unwrap().home_dir().join("x");
        assert_eq!(sent, Some(ClientMsg::CreateHeadAgentAt { path: home }));

        let (app, sent) = run_sidebar(&[KeyCode::Char('H'), KeyCode::Enter]).await;
        assert_eq!(sent, None, "an empty dir creates nothing");
        assert_eq!(app.input, InputMode::Normal);

        let (app, sent) =
            run_sidebar(&[KeyCode::Char('H'), KeyCode::Char('x'), KeyCode::Esc]).await;
        assert_eq!(sent, None, "Esc creates nothing");
        assert_eq!(app.input, InputMode::Normal);
    }

    /// The `H` prompt names itself and shows what you've typed, so it isn't mistaken for `N`.
    #[test]
    fn head_prompt_renders_its_single_field() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new(100, 40);
        app.input = InputMode::CreatingHead;
        app.create_field = Field::Dir;
        app.dir_buf = "~/Repos/amux".into();

        let mut term = Terminal::new(TestBackend::new(90, 1)).unwrap();
        term.draw(|f| render_status(f, f.area(), &app)).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("HEAD session") && content.contains("dir: ~/Repos/amux"),
            "the HEAD prompt renders its field, got: {content}"
        );
        assert!(
            !content.contains("branch"),
            "a HEAD session has no branch field, got: {content}"
        );
    }

    /// THE regression guard: a bare digit (no Cmd, no prefix) is not a shortcut — it must fall
    /// through untouched so the focused agent's PTY still receives it.
    #[tokio::test]
    async fn bare_digit_is_not_a_shortcut() {
        let (mut app, _ids) = app_with_agents(3);
        let (mut sink, _server) = test_sink();
        app.on_key(key(KeyCode::Char('1')), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.active_agent, None,
            "a bare digit opened nothing — it is not a shortcut"
        );
    }

    /// The "jump to previous" target follows the main area: it is the agent left behind on each
    /// real swap, untouched when the active agent is re-opened, and toggles for ping-pong.
    #[test]
    fn prev_agent_tracks_last_active_and_pingpongs() {
        let (mut app, ids) = app_with_agents(3);
        assert_eq!(
            app.prev_active_agent, None,
            "no previous until a second agent"
        );
        let _ = app.swap_to_agent(ids[0]);
        assert_eq!(
            app.prev_active_agent, None,
            "the first activation leaves no previous"
        );
        let _ = app.swap_to_agent(ids[1]);
        assert_eq!(
            app.prev_active_agent,
            Some(ids[0]),
            "leaving agent 0 makes it the previous"
        );
        let _ = app.swap_to_agent(ids[2]);
        assert_eq!(app.prev_active_agent, Some(ids[1]));
        let _ = app.swap_to_agent(ids[2]);
        assert_eq!(
            app.prev_active_agent,
            Some(ids[1]),
            "re-opening the active agent leaves the target alone"
        );
        let _ = app.swap_to_agent(ids[1]);
        assert_eq!(app.prev_active_agent, Some(ids[2]), "ping-pong back");
    }

    /// `Ctrl+B -` opens the previous agent's session, and pressing it again bounces back — a true
    /// last-window toggle, because opening the previous agent makes the one you left the new target.
    #[tokio::test]
    async fn ctrl_b_dash_opens_previous_agent_and_toggles() {
        let (mut app, ids) = app_with_agents(3);
        let (mut sink, _server) = test_sink();
        let _ = app.swap_to_agent(ids[0]);
        let _ = app.swap_to_agent(ids[1]); // active = ids[1], prev = ids[0]
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        app.on_key(key(KeyCode::Char('-')), &mut sink)
            .await
            .unwrap();
        assert_eq!(app.active_agent, Some(ids[0]), "opens the previous agent");
        assert!(matches!(app.focus, Focus::Panes), "focus is in the session");
        assert_eq!(
            app.prev_active_agent,
            Some(ids[1]),
            "the one we left becomes the target"
        );
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        app.on_key(key(KeyCode::Char('-')), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.active_agent,
            Some(ids[1]),
            "a second press bounces back"
        );
    }

    /// With no previous agent, `Ctrl+B -` is a no-op — nothing opens.
    #[tokio::test]
    async fn ctrl_b_dash_is_noop_without_previous() {
        let (mut app, ids) = app_with_agents(2);
        let (mut sink, _server) = test_sink();
        let _ = app.swap_to_agent(ids[0]); // only ever one active → no previous
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        app.on_key(key(KeyCode::Char('-')), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.active_agent,
            Some(ids[0]),
            "still on the only agent opened"
        );
    }

    /// Removing the previous agent clears the jump target, so `Ctrl+B -` never points at a ghost.
    #[tokio::test]
    async fn agent_removal_clears_previous_target() {
        let (mut app, ids) = app_with_agents(2);
        let (mut sink, _server) = test_sink();
        let _ = app.swap_to_agent(ids[0]);
        let _ = app.swap_to_agent(ids[1]); // prev = ids[0]
        app.on_daemon(DaemonMsg::AgentRemoved { id: ids[0] }, &mut sink)
            .await
            .unwrap();
        assert_eq!(app.prev_active_agent, None);
        assert_eq!(app.previous_agent(), None, "no target left to jump to");
    }

    /// A restart seeds `Ctrl+B -` from the daemon: the persisted previous target arrives as
    /// `DaemonMsg::Previous` on connect, so the jump works before the user has swapped anything.
    #[tokio::test]
    async fn daemon_previous_seeds_the_jump_target() {
        let (mut app, ids) = app_with_agents(2);
        let (mut sink, _server) = test_sink();
        // Simulate the connect-time main-pane restore (DaemonMsg::Active reopened ids[1]).
        let _ = app.swap_to_agent(ids[1]);
        assert_eq!(
            app.prev_active_agent, None,
            "restoring the active agent from nothing leaves no previous"
        );
        // Previous arrives last on connect and seeds the jump target.
        app.on_daemon(DaemonMsg::Previous(Some(ids[0])), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.prev_active_agent,
            Some(ids[0]),
            "seed the jump target from the daemon's persisted previous"
        );
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        app.on_key(key(KeyCode::Char('-')), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.active_agent,
            Some(ids[0]),
            "Ctrl+B - jumps to the seeded previous immediately after a restart"
        );
    }

    /// While the overlay is active, the previous agent's row is marked `-` in place of its digit.
    #[test]
    fn overlay_marks_previous_agent_with_dash() {
        use ratatui::{backend::TestBackend, Terminal};
        let (mut app, ids) = app_with_agents(3);

        let content = |app: &App| -> String {
            let mut term = Terminal::new(TestBackend::new(30, 8)).unwrap();
            term.draw(|f| render_sidebar(f, f.area(), app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        let _ = app.swap_to_agent(ids[0]);
        let _ = app.swap_to_agent(ids[1]); // prev = ids[0]
        assert!(
            !content(&app).contains(" - "),
            "no marker while the overlay is off"
        );
        app.super_held = true;
        assert!(
            content(&app).contains(" - "),
            "the previous agent's row is marked with - while the overlay is active"
        );
    }

    /// While the overlay is active, the shortcut digit is drawn over the status glyph — same
    /// width, so the sidebar shape is unchanged.
    #[test]
    fn overlay_covers_status_glyph_with_digit() {
        use ratatui::{backend::TestBackend, Terminal};
        let (mut app, _ids) = app_with_agents(1);
        let glyph = AgentState::Idle.glyph();

        let content = |app: &App| -> String {
            let mut term = Terminal::new(TestBackend::new(30, 8)).unwrap();
            term.draw(|f| render_sidebar(f, f.area(), app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        let off = content(&app);
        assert!(off.contains(glyph), "glyph shows when the overlay is off");

        app.super_held = true;
        let on = content(&app);
        assert!(
            !on.contains(glyph),
            "the digit covers the status glyph while the overlay is active"
        );
        assert!(
            on.matches('1').count() > off.matches('1').count(),
            "the shortcut digit renders in the overlay"
        );
    }
}

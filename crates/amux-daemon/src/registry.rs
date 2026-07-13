//! The agent registry. The daemon manages many **repos**; each **Agent** is a durable workspace
//! (worktree + branch) belonging to one repo, and owns a **primary terminal** (its CLI) plus any
//! **shell terminals** split off in the same worktree. Terminals are the streaming unit; agents
//! are what the sidebar lists (grouped by repo). A session exiting suspends the agent (primary)
//! or removes the terminal (shell); only delete destroys a worktree. See `docs/DESIGN.md` §5,
//! `docs/SPLITS.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use amux_core::adapter::{AgentAdapter, HookSetup, LaunchContext};
use amux_core::agent::{
    is_notable_transition, next_state, AgentEvent, AgentId, AgentState, RepoId, TerminalId,
};
use amux_core::hook::{classify, HookReport};
use amux_core::worktree::{WorktreeLocation, WorktreeService};
use amux_proto::{AgentInfo, DaemonMsg, RepoInfo, Size};

use crate::pty::Session;

const EVENT_BACKLOG: usize = 256;
const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };
/// How long a primary terminal may go with **no PTY output** before the heartbeat settles a
/// `Working` agent to `Idle` — a backstop for a missed `Stop` hook. Generous, so genuine mid-work
/// pauses (a silent tool) rarely cause a false idle; the uncommon real miss recovers within this
/// window, and a later hook (`PostToolUse`, etc.) corrects an over-eager idle immediately.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of a delete request.
pub enum DeleteOutcome {
    Deleted,
    NeedsConfirm(String),
}

/// What a `doctor` run did: worktrees pruned, and worktrees left alone because they had
/// uncommitted changes (name + dirty count).
pub struct DoctorReport {
    pub pruned: Vec<String>,
    pub skipped: Vec<(String, usize)>,
}

struct Agent {
    id: AgentId,
    repo: RepoId,
    name: String,
    branch: String,
    worktree: PathBuf,
    ai_session_id: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    state: AgentState,
    /// Inbox unread bit — a notable moment the user hasn't seen yet (see `is_notable_transition`).
    unread: bool,
    primary: TerminalId,
}

impl Agent {
    fn info(&self) -> AgentInfo {
        AgentInfo {
            id: self.id,
            repo: self.repo,
            name: self.name.clone(),
            branch: self.branch.clone(),
            state: self.state.clone(),
            last_activity: self.last_activity,
            unread: self.unread,
            primary_terminal: self.primary,
        }
    }
}

struct Terminal {
    agent: AgentId,
    worktree: PathBuf,
    primary: bool,
    session: Option<Arc<Session>>,
    /// A vim-like app is foreground here, so `Ctrl+hjkl` passes through to it (set via the nav
    /// plugin's `amux passthrough`).
    passthrough: bool,
}

/// A registered repository and its (cheaply cloneable) worktree service.
struct RepoEntry {
    info: RepoInfo,
    worktrees: WorktreeService,
}

#[derive(Default)]
struct State {
    repos: HashMap<RepoId, RepoEntry>,
    agents: HashMap<AgentId, Agent>,
    terminals: HashMap<TerminalId, Terminal>,
    /// The agent the user is currently viewing, if any. A notable event on this agent does not
    /// mark it unread (you're watching it); everyone else's does.
    focused: Option<AgentId>,
    /// Saved pane layouts per agent, replayed to a re-attaching client so splits survive the TUI
    /// closing. In-memory (survives client restart, not a daemon restart — that's state.json).
    layouts: HashMap<AgentId, amux_proto::Layout>,
    /// Which agents are open as minis (left-to-right) — replayed to a re-attaching client.
    minis: Vec<AgentId>,
    /// Which agent occupies the main area — replayed so a re-attaching client restores its main
    /// pane. Durable (unlike `focused`, which is the transient viewed-cell for unread).
    active: Option<AgentId>,
}

/// The durable slice of the daemon's state, written to `state.json` so agents/repos/minis survive
/// a daemon restart. Live processes (PTYs) are *not* here — they die with the daemon and are
/// re-spawned lazily (via `resume`) when a client next attaches a suspended agent's primary.
#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    repos: Vec<PersistedRepo>,
    agents: Vec<PersistedAgent>,
    /// Which agents were open as minis, in order.
    minis: Vec<AgentId>,
    /// Which agent occupied the main area (restored into the client's main pane on reconnect).
    #[serde(default)]
    active: Option<AgentId>,
}

/// A repo as `repo` + `base` paths, enough to rebuild its [`WorktreeService`] verbatim.
#[derive(Serialize, Deserialize)]
struct PersistedRepo {
    repo: PathBuf,
    base: PathBuf,
}

/// An agent's durable identity. State is not persisted — a reloaded agent is suspended (`Exited`)
/// until a client attaches its primary, which resumes it (reusing `primary` + `ai_session_id`).
#[derive(Serialize, Deserialize)]
struct PersistedAgent {
    id: AgentId,
    repo: RepoId,
    name: String,
    branch: String,
    worktree: PathBuf,
    ai_session_id: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    unread: bool,
    primary: TerminalId,
}

/// Where the daemon's hook mailbox lives and how to invoke the bridge, so launched CLIs can
/// push status back. Absent in tests (the fake `cat` agent has no hooks).
struct HookIntegration {
    socket: PathBuf,
    amux_exe: PathBuf,
}

pub struct Registry {
    state: Mutex<State>,
    adapter: Box<dyn AgentAdapter>,
    events: broadcast::Sender<DaemonMsg>,
    hooks: Option<HookIntegration>,
    /// PTY-silence window after which the heartbeat settles a `Working` agent to `Idle`.
    idle_timeout: Duration,
    /// Where durable state (repos/agents/minis) is written so it survives a daemon restart.
    /// `None` disables persistence (tests, unless they opt in via [`Registry::with_state`]).
    state_path: Option<PathBuf>,
}

impl Registry {
    pub fn new(adapter: Box<dyn AgentAdapter>) -> Arc<Self> {
        Self::build(adapter, None, DEFAULT_IDLE_TIMEOUT, None)
    }

    /// Build a registry that wires launched agents' hooks to `socket`, invoking `amux_exe hook`,
    /// and persists durable state to `state_path`.
    pub fn with_hooks(
        adapter: Box<dyn AgentAdapter>,
        socket: PathBuf,
        amux_exe: PathBuf,
        state_path: PathBuf,
    ) -> Arc<Self> {
        Self::build(
            adapter,
            Some(HookIntegration { socket, amux_exe }),
            DEFAULT_IDLE_TIMEOUT,
            Some(state_path),
        )
    }

    /// Build a registry with a custom heartbeat idle timeout — used by tests to exercise the
    /// backstop on a human timescale.
    pub fn with_idle_timeout(adapter: Box<dyn AgentAdapter>, idle_timeout: Duration) -> Arc<Self> {
        Self::build(adapter, None, idle_timeout, None)
    }

    /// Build a registry that persists durable state to `state_path` (for persistence tests).
    pub fn with_state(adapter: Box<dyn AgentAdapter>, state_path: PathBuf) -> Arc<Self> {
        Self::build(adapter, None, DEFAULT_IDLE_TIMEOUT, Some(state_path))
    }

    fn build(
        adapter: Box<dyn AgentAdapter>,
        hooks: Option<HookIntegration>,
        idle_timeout: Duration,
        state_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        Arc::new(Self {
            state: Mutex::new(State::default()),
            adapter,
            events,
            hooks,
            idle_timeout,
            state_path,
        })
    }

    /// The hook wiring to hand an adapter, if hook integration is enabled.
    fn hook_setup(&self) -> Option<HookSetup<'_>> {
        self.hooks.as_ref().map(|h| HookSetup {
            socket: &h.socket,
            amux_exe: &h.amux_exe,
        })
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<DaemonMsg> {
        self.events.subscribe()
    }

    /// Register a repository (idempotent, keyed by canonical path). Broadcasts `RepoAdded` the
    /// first time a repo appears; returns its info either way so the caller learns the id.
    pub fn register(&self, worktrees: WorktreeService) -> RepoInfo {
        let path = worktrees.repo().to_path_buf();
        let id = RepoId::from_canonical_path(&path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_string());
        let info = RepoInfo { id, name, path };
        let is_new = {
            use std::collections::hash_map::Entry;
            let mut state = self.state.lock().unwrap();
            match state.repos.entry(id) {
                Entry::Occupied(_) => false,
                Entry::Vacant(slot) => {
                    slot.insert(RepoEntry {
                        info: info.clone(),
                        worktrees,
                    });
                    true
                }
            }
        };
        if is_new {
            self.save();
            let _ = self.events.send(DaemonMsg::RepoAdded(info.clone()));
        }
        info
    }

    /// Register the repository at `path` (idempotent), building its worktree service.
    pub fn register_path(&self, path: &Path, location: WorktreeLocation) -> Result<RepoInfo> {
        let worktrees = WorktreeService::new(path, location).context("open repository")?;
        Ok(self.register(worktrees))
    }

    /// Save (or clear, on `None`) an agent's pane layout for replay to re-attaching clients.
    pub fn set_layout(&self, agent: AgentId, layout: Option<amux_proto::Layout>) {
        let mut state = self.state.lock().unwrap();
        match layout {
            Some(l) => {
                state.layouts.insert(agent, l);
            }
            None => {
                state.layouts.remove(&agent);
            }
        }
    }

    /// All saved layouts (sent to a client on connect).
    pub fn layouts(&self) -> Vec<(AgentId, amux_proto::Layout)> {
        self.state
            .lock()
            .unwrap()
            .layouts
            .iter()
            .map(|(id, l)| (*id, l.clone()))
            .collect()
    }

    /// Persist which agents are open as minis (replayed to a re-attaching client). Saves only on a
    /// real change, so the client re-sending it on every reconcile doesn't churn the disk.
    pub fn set_minis(&self, minis: Vec<AgentId>) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            let changed = state.minis != minis;
            state.minis = minis;
            changed
        };
        if changed {
            self.save();
        }
    }

    pub fn minis(&self) -> Vec<AgentId> {
        self.state.lock().unwrap().minis.clone()
    }

    /// Persist which agent is in the main area (replayed to a re-attaching client so it restores
    /// its main pane). Saves only on a real change (same anti-churn reasoning as `set_minis`).
    pub fn set_active(&self, active: Option<AgentId>) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            let changed = state.active != active;
            state.active = active;
            changed
        };
        if changed {
            self.save();
        }
    }

    pub fn active(&self) -> Option<AgentId> {
        self.state.lock().unwrap().active
    }

    /// Serialize durable state (repos/agents/minis) to `state.json` via an atomic temp+rename.
    /// No-op without a state path; a write failure is logged, never fatal.
    pub fn save(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let snapshot = {
            let state = self.state.lock().unwrap();
            PersistedState {
                repos: state
                    .repos
                    .values()
                    .map(|e| PersistedRepo {
                        repo: e.worktrees.repo().to_path_buf(),
                        base: e.worktrees.base().to_path_buf(),
                    })
                    .collect(),
                agents: state
                    .agents
                    .values()
                    .map(|a| PersistedAgent {
                        id: a.id,
                        repo: a.repo,
                        name: a.name.clone(),
                        branch: a.branch.clone(),
                        worktree: a.worktree.clone(),
                        ai_session_id: a.ai_session_id.clone(),
                        created_at: a.created_at,
                        last_activity: a.last_activity,
                        unread: a.unread,
                        primary: a.primary,
                    })
                    .collect(),
                minis: state.minis.clone(),
                active: state.active,
            }
        };
        if let Err(e) = write_atomic(path, &snapshot) {
            tracing::warn!("could not persist state to {}: {e:#}", path.display());
        }
    }

    /// Load durable state on startup: re-register repos and reinstate their agents as **suspended**
    /// (no live session, `Exited`) with a dormant primary terminal, so a later attach can resume
    /// them (reusing the primary id + `ai_session_id`). No-op if the file is absent/unreadable.
    pub fn load_state(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!("could not read state {}: {e:#}", path.display());
                return;
            }
        };
        let persisted: PersistedState = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("ignoring unreadable state {}: {e:#}", path.display());
                return;
            }
        };
        let mut state = self.state.lock().unwrap();
        for r in persisted.repos {
            match WorktreeService::with_base(&r.repo, &r.base) {
                Ok(worktrees) => {
                    let id = RepoId::from_canonical_path(worktrees.repo());
                    let name = worktrees
                        .repo()
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "repo".to_string());
                    let info = RepoInfo {
                        id,
                        name,
                        path: worktrees.repo().to_path_buf(),
                    };
                    state
                        .repos
                        .entry(id)
                        .or_insert(RepoEntry { info, worktrees });
                }
                Err(e) => tracing::warn!("dropping saved repo {}: {e:#}", r.repo.display()),
            }
        }
        for a in persisted.agents {
            // Skip agents whose repo could not be re-registered (its worktree base is gone).
            if !state.repos.contains_key(&a.repo) {
                continue;
            }
            // A dormant primary terminal (no session) so `resume` can find and revive it.
            state.terminals.insert(
                a.primary,
                Terminal {
                    agent: a.id,
                    worktree: a.worktree.clone(),
                    primary: true,
                    session: None,
                    passthrough: false,
                },
            );
            state.agents.insert(
                a.id,
                Agent {
                    id: a.id,
                    repo: a.repo,
                    name: a.name,
                    branch: a.branch,
                    worktree: a.worktree,
                    ai_session_id: a.ai_session_id,
                    created_at: a.created_at,
                    last_activity: a.last_activity,
                    state: AgentState::Exited { code: None },
                    unread: a.unread,
                    primary: a.primary,
                },
            );
        }
        // Keep only minis whose agent survived the load.
        state.minis = persisted
            .minis
            .into_iter()
            .filter(|id| state.agents.contains_key(id))
            .collect();
        // Restore the active agent only if it survived (and isn't also a mini).
        state.active = persisted
            .active
            .filter(|id| state.agents.contains_key(id) && !state.minis.contains(id));
    }

    /// Ensure `terminal`'s agent is live, resuming a suspended primary if needed. Called when a
    /// client attaches a primary with no session (e.g. after a daemon restart). No-op if it's
    /// already live or the terminal isn't a suspended primary.
    pub fn resume_for_terminal(self: &Arc<Self>, terminal: TerminalId) -> Result<()> {
        let agent = {
            let state = self.state.lock().unwrap();
            match state.terminals.get(&terminal) {
                Some(t) if t.primary && t.session.is_none() => t.agent,
                _ => return Ok(()),
            }
        };
        self.resume(agent)
    }

    pub fn repos(&self) -> Vec<RepoInfo> {
        self.state
            .lock()
            .unwrap()
            .repos
            .values()
            .map(|e| e.info.clone())
            .collect()
    }

    /// The cloneable worktree service for a repo, if registered.
    fn worktrees_for(&self, repo: RepoId) -> Option<WorktreeService> {
        self.state
            .lock()
            .unwrap()
            .repos
            .get(&repo)
            .map(|e| e.worktrees.clone())
    }

    pub fn infos(&self) -> Vec<AgentInfo> {
        self.state
            .lock()
            .unwrap()
            .agents
            .values()
            .map(Agent::info)
            .collect()
    }

    /// The live session for a terminal, if it has one.
    pub fn session(&self, terminal: TerminalId) -> Option<Arc<Session>> {
        self.state
            .lock()
            .unwrap()
            .terminals
            .get(&terminal)
            .and_then(|t| t.session.clone())
    }

    /// Register the repo at `path` (idempotent) and create an agent on `branch` in it.
    pub fn create_at(self: &Arc<Self>, path: &Path, branch: &str) -> Result<AgentInfo> {
        let info = self.register_path(path, WorktreeLocation::Global)?;
        self.create(info.id, branch)
    }

    /// Create an agent in `repo`: worktree + a primary terminal running the agent CLI.
    pub fn create(self: &Arc<Self>, repo: RepoId, branch: &str) -> Result<AgentInfo> {
        let worktrees = self.worktrees_for(repo).context("no such repo")?;
        // One agent per (repo, branch): a branch maps to a single worktree, so a duplicate would
        // just collide. Refuse early with a clear message rather than a raw git error.
        {
            let state = self.state.lock().unwrap();
            if state
                .agents
                .values()
                .any(|a| a.repo == repo && a.branch == branch)
            {
                anyhow::bail!("an agent for branch '{branch}' already exists");
            }
        }
        let worktree = worktrees.create(branch).context("create worktree")?;
        // The agent + terminal ids are exported to in-pane tools (hooks, the nav plugin), so they
        // must exist before we launch.
        let agent_id = AgentId::new();
        let terminal_id = TerminalId::new();
        let agent_full = agent_id.to_full_string();
        let ctx = LaunchContext {
            worktree: &worktree,
            branch,
            resume: None,
            agent_id: &agent_full,
            hooks: self.hook_setup(),
        };
        self.adapter.prepare_worktree(&ctx)?;
        let mut spec = self.adapter.spawn_spec(&ctx);
        spec.env
            .push(("AMUX_TERMINAL_ID".to_string(), terminal_id.to_full_string()));
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;

        let now = Utc::now();
        let info = {
            let mut state = self.state.lock().unwrap();
            state.terminals.insert(
                terminal_id,
                Terminal {
                    agent: agent_id,
                    worktree: worktree.clone(),
                    primary: true,
                    session: Some(Arc::clone(&session)),
                    passthrough: false,
                },
            );
            let agent = Agent {
                id: agent_id,
                repo,
                name: agent_name(branch),
                branch: branch.to_string(),
                worktree,
                ai_session_id: None,
                created_at: now,
                last_activity: now,
                // A freshly launched CLI is sitting at its prompt — idle/waiting, not working.
                // Real work is signalled by hooks (UserPromptSubmit/… → Working); starting in
                // Working would show a false "⋯" and, when the heartbeat settled it, a false unread.
                state: AgentState::Idle,
                unread: false,
                primary: terminal_id,
            };
            let info = agent.info();
            state.agents.insert(agent_id, agent);
            info
        };
        self.spawn_primary_monitor(agent_id, terminal_id, session);
        self.save();
        let _ = self.events.send(DaemonMsg::AgentAdded(info.clone()));
        Ok(info)
    }

    /// Split: spawn a `$SHELL` terminal (id `new`) in the same worktree as `like`.
    pub fn spawn_shell(self: &Arc<Self>, new: TerminalId, like: TerminalId) -> Result<()> {
        let (agent, worktree) = {
            let state = self.state.lock().unwrap();
            let term = state.terminals.get(&like).context("no such terminal")?;
            (term.agent, term.worktree.clone())
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let env = self.terminal_env(new);
        let session = Session::spawn(&[shell], &worktree, &env, DEFAULT_SIZE)?;
        {
            let mut state = self.state.lock().unwrap();
            state.terminals.insert(
                new,
                Terminal {
                    agent,
                    worktree,
                    primary: false,
                    session: Some(Arc::clone(&session)),
                    passthrough: false,
                },
            );
        }
        self.spawn_shell_monitor(new, session);
        Ok(())
    }

    /// Apply a hook report from a launched CLI: capture its session id (for resume) and drive the
    /// state machine. Broadcasts `StateChanged` / `UnreadChanged` when things move. Unknown agents
    /// and no-op events are silently ignored.
    pub fn on_hook(&self, report: HookReport) {
        let classified = classify(&report.event);
        // Capturing the AI session id is the one hook-driven change worth persisting (it's what
        // lets a resume across a daemon restart continue the same conversation). Save only when it
        // actually changes — every hook carries it, but it's set once per session.
        let session_changed = if let Some(sid) = classified.session_id {
            let mut state = self.state.lock().unwrap();
            match state.agents.get_mut(&report.agent) {
                Some(a) if a.ai_session_id.as_deref() != Some(sid.as_str()) => {
                    a.ai_session_id = Some(sid);
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        if session_changed {
            self.save();
        }
        if let Some(event) = classified.event {
            self.apply_event(report.agent, event);
        }
    }

    /// Fold an [`AgentEvent`] into an agent's state via the pure state machine, flag unread on a
    /// notable transition, and broadcast what changed. Shared by hooks and the idle heartbeat.
    fn apply_event(&self, agent: AgentId, event: AgentEvent) {
        let (state_change, unread_change) = {
            let mut state = self.state.lock().unwrap();
            let Some(a) = state.agents.get(&agent) else {
                return;
            };
            let next = next_state(&a.state.clone(), &event);
            set_state(&mut state, agent, next)
        };
        self.broadcast_changes(agent, state_change, unread_change);
    }

    /// Set (or clear) which agent the user is currently viewing. Focusing an agent clears its
    /// unread bit; while focused, notable events on it won't re-mark it unread.
    pub fn focus(&self, agent: Option<AgentId>) {
        let cleared = {
            let mut state = self.state.lock().unwrap();
            state.focused = agent;
            match agent.and_then(|id| state.agents.get_mut(&id).map(|a| (id, a))) {
                Some((id, a)) if a.unread => {
                    a.unread = false;
                    Some(id)
                }
                _ => None,
            }
        };
        if let Some(id) = cleared {
            let _ = self
                .events
                .send(DaemonMsg::UnreadChanged { id, unread: false });
        }
    }

    /// Env exported into every terminal so in-pane tools can reach the mailbox and identify their
    /// own pane: the mailbox socket + this terminal's id. Empty when hook integration is off.
    fn terminal_env(&self, terminal: TerminalId) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(h) = &self.hooks {
            env.push((
                "AMUX_HOOK_SOCK".to_string(),
                h.socket.to_string_lossy().into_owned(),
            ));
        }
        env.push(("AMUX_TERMINAL_ID".to_string(), terminal.to_full_string()));
        env
    }

    /// Record that a terminal's foreground app does (or no longer does) want `Ctrl+hjkl`, and tell
    /// clients so their keypress routing can adapt. Announced by the nav plugin.
    pub fn set_passthrough(&self, terminal: TerminalId, on: bool) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            match state.terminals.get_mut(&terminal) {
                Some(t) if t.passthrough != on => {
                    t.passthrough = on;
                    true
                }
                _ => false,
            }
        };
        if changed {
            let _ = self.events.send(DaemonMsg::TerminalApp {
                terminal,
                passthrough: on,
            });
        }
    }

    /// Relay an in-pane program's edge navigation to clients (which own the pane tree). The daemon
    /// stays layout-agnostic — it just forwards the intent.
    pub fn request_nav(&self, terminal: TerminalId, dir: amux_core::nav::Dir) {
        let _ = self.events.send(DaemonMsg::Navigate { terminal, dir });
    }

    fn broadcast_changes(&self, id: AgentId, state: Option<AgentState>, unread: Option<bool>) {
        if let Some(state) = state {
            let _ = self.events.send(DaemonMsg::StateChanged { id, state });
        }
        if let Some(unread) = unread {
            let _ = self.events.send(DaemonMsg::UnreadChanged { id, unread });
        }
    }

    /// Kill a shell terminal (its pane closed). No-op on a primary terminal.
    pub fn close_terminal(&self, terminal: TerminalId) {
        let mut state = self.state.lock().unwrap();
        if state.terminals.get(&terminal).is_some_and(|t| !t.primary) {
            if let Some(session) = state.terminals.remove(&terminal).and_then(|t| t.session) {
                session.kill();
            }
        }
    }

    /// Resume a suspended agent — respawn its primary terminal in the existing worktree.
    pub fn resume(self: &Arc<Self>, id: AgentId) -> Result<()> {
        let (worktree, branch, resume_id, primary, already_live) = {
            let state = self.state.lock().unwrap();
            let agent = state.agents.get(&id).context("no such agent")?;
            let live = state
                .terminals
                .get(&agent.primary)
                .is_some_and(|t| t.session.is_some());
            (
                agent.worktree.clone(),
                agent.branch.clone(),
                agent.ai_session_id.clone(),
                agent.primary,
                live,
            )
        };
        if already_live {
            return Ok(());
        }
        let agent_full = id.to_full_string();
        let ctx = LaunchContext {
            worktree: &worktree,
            branch: &branch,
            resume: resume_id.as_deref(),
            agent_id: &agent_full,
            hooks: self.hook_setup(),
        };
        self.adapter.prepare_worktree(&ctx)?;
        let mut spec = self.adapter.spawn_spec(&ctx);
        spec.env
            .push(("AMUX_TERMINAL_ID".to_string(), primary.to_full_string()));
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;
        let state_change = {
            let mut state = self.state.lock().unwrap();
            if let Some(term) = state.terminals.get_mut(&primary) {
                term.session = Some(Arc::clone(&session));
            }
            match state.agents.get_mut(&id) {
                Some(agent) => {
                    // Resuming lands back at the prompt — idle/waiting, not working (same reasoning
                    // as create): let hooks drive Working so we don't flash a false "⋯"/unread.
                    agent.state = AgentState::Idle;
                    agent.last_activity = Utc::now();
                    agent.state.clone()
                }
                None => return Ok(()),
            }
        };
        self.spawn_primary_monitor(id, primary, session);
        let _ = self.events.send(DaemonMsg::StateChanged {
            id,
            state: state_change,
        });
        Ok(())
    }

    /// Delete an agent — the only destructive op: kill all its terminals and remove its worktree
    /// (branch and commits are kept). Refuses a dirty worktree unless `force`.
    pub fn delete(&self, id: AgentId, force: bool) -> Result<DeleteOutcome> {
        let (branch, repo) = match self.state.lock().unwrap().agents.get(&id) {
            Some(agent) => (agent.branch.clone(), agent.repo),
            None => return Ok(DeleteOutcome::Deleted),
        };
        let worktrees = self.worktrees_for(repo);
        if !force {
            let dirty = worktrees
                .as_ref()
                .and_then(|w| w.dirty_count(&branch).ok())
                .unwrap_or(0);
            if dirty > 0 {
                let plural = if dirty == 1 { "" } else { "s" };
                return Ok(DeleteOutcome::NeedsConfirm(format!(
                    "{dirty} uncommitted change{plural}"
                )));
            }
        }
        let sessions: Vec<Arc<Session>> = {
            let mut state = self.state.lock().unwrap();
            state.agents.remove(&id);
            state.layouts.remove(&id);
            state.minis.retain(|a| *a != id);
            if state.active == Some(id) {
                state.active = None;
            }
            let terminal_ids: Vec<TerminalId> = state
                .terminals
                .iter()
                .filter(|(_, t)| t.agent == id)
                .map(|(&tid, _)| tid)
                .collect();
            terminal_ids
                .into_iter()
                .filter_map(|tid| state.terminals.remove(&tid))
                .filter_map(|t| t.session)
                .collect()
        };
        for session in sessions {
            session.kill();
        }
        if let Some(worktrees) = worktrees {
            worktrees.remove(&branch).ok();
        }
        self.save();
        let _ = self.events.send(DaemonMsg::AgentRemoved { id });
        Ok(DeleteOutcome::Deleted)
    }

    /// Prune orphaned worktrees in `repo` — git-tracked worktrees under our base that no live
    /// agent holds — to reclaim wedged branches. Worktrees with uncommitted changes are spared.
    pub fn doctor(&self, repo: RepoId) -> Result<DoctorReport> {
        let worktrees = self.worktrees_for(repo).context("no such repo")?;
        let keep: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .agents
                .values()
                .filter(|a| a.repo == repo)
                .map(|a| a.branch.clone())
                .collect()
        };
        let mut pruned = Vec::new();
        let mut skipped = Vec::new();
        for orphan in worktrees.orphans(&keep)? {
            if orphan.dirty > 0 {
                skipped.push((orphan.name, orphan.dirty));
            } else {
                worktrees.prune_worktree(&orphan.name)?;
                pruned.push(orphan.name);
            }
        }
        Ok(DoctorReport { pruned, skipped })
    }

    pub fn shutdown_all(&self) {
        let state = self.state.lock().unwrap();
        for terminal in state.terminals.values() {
            if let Some(session) = &terminal.session {
                session.kill();
            }
        }
    }

    fn spawn_primary_monitor(
        self: &Arc<Self>,
        agent: AgentId,
        terminal: TerminalId,
        session: Arc<Session>,
    ) {
        // Exit watcher: flips the agent to Exited when its process dies.
        let registry = Arc::clone(self);
        let mut exit_rx = session.exit_rx();
        let exit_session = Arc::clone(&session);
        tokio::spawn(async move {
            let _ = exit_rx.changed().await;
            registry.on_primary_exit(agent, terminal, exit_session.exit_code());
        });
        // Idle heartbeat: settles a stuck `Working` to `Idle` after prolonged PTY silence.
        self.spawn_heartbeat(agent, session);
    }

    /// Watch a primary terminal's PTY output; if it goes quiet for `idle_timeout`, apply a
    /// `WentIdle` event — the backstop for a missed `Stop` hook. Output resets the clock; the task
    /// ends when the session's broadcast closes (the process exited). This is server-side and
    /// runs whether or not a client is attached.
    fn spawn_heartbeat(self: &Arc<Self>, agent: AgentId, session: Arc<Session>) {
        let registry = Arc::clone(self);
        let idle = self.idle_timeout;
        let mut rx = session.subscribe();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match tokio::time::timeout(idle, rx.recv()).await {
                    // Output (or a dropped-lag notice) means it's alive — re-arm the timer.
                    Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => {}
                    // The session's output channel closed — the process is gone.
                    Ok(Err(RecvError::Closed)) => break,
                    // Prolonged silence: settle to Idle (a no-op unless it's still Working).
                    Err(_elapsed) => {
                        registry.apply_event(agent, AgentEvent::WentIdle);
                        // Wait for activity to resume before timing again (don't re-fire while
                        // legitimately idle).
                        match rx.recv().await {
                            Ok(_) | Err(RecvError::Lagged(_)) => {}
                            Err(RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
    }

    fn on_primary_exit(&self, agent: AgentId, terminal: TerminalId, code: Option<i32>) {
        let (state_change, unread_change) = {
            let mut state = self.state.lock().unwrap();
            if let Some(term) = state.terminals.get_mut(&terminal) {
                term.session = None;
            }
            if !state.agents.contains_key(&agent) {
                return;
            }
            // Exit is a notable transition, so this also flags unread if you weren't watching.
            set_state(&mut state, agent, AgentState::Exited { code })
        };
        self.broadcast_changes(agent, state_change, unread_change);
    }

    fn spawn_shell_monitor(self: &Arc<Self>, terminal: TerminalId, session: Arc<Session>) {
        let registry = Arc::clone(self);
        let mut exit_rx = session.exit_rx();
        tokio::spawn(async move {
            let _ = exit_rx.changed().await;
            let code = session.exit_code();
            registry.state.lock().unwrap().terminals.remove(&terminal);
            let _ = registry
                .events
                .send(DaemonMsg::TerminalExited { terminal, code });
        });
    }
}

fn agent_name(branch: &str) -> String {
    branch.rsplit('/').next().unwrap_or(branch).to_string()
}

/// Write `state` to `path` atomically: serialize to a sibling temp file, then rename over `path`
/// so a reader never sees a half-written file.
fn write_atomic(path: &Path, state: &PersistedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(state).context("serialize state")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Apply `next` to `id` inside the locked state: update the state + activity, and flag the agent
/// unread on a notable transition — unless it's the agent the user is currently viewing. Returns
/// `(state change, unread change)` for the caller to broadcast. A no-op transition returns
/// `(None, None)`.
fn set_state(
    state: &mut State,
    id: AgentId,
    next: AgentState,
) -> (Option<AgentState>, Option<bool>) {
    let focused = state.focused;
    let Some(agent) = state.agents.get_mut(&id) else {
        return (None, None);
    };
    let prev = agent.state.clone();
    agent.last_activity = Utc::now();
    if next == prev {
        return (None, None);
    }
    let notable = is_notable_transition(&prev, &next);
    agent.state = next.clone();
    let unread_change = if notable && focused != Some(id) && !agent.unread {
        agent.unread = true;
        Some(true)
    } else {
        None
    };
    (Some(next), unread_change)
}

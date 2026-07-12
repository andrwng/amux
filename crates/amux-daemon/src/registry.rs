//! The agent registry. The daemon manages many **repos**; each **Agent** is a durable workspace
//! (worktree + branch) belonging to one repo, and owns a **primary terminal** (its CLI) plus any
//! **shell terminals** split off in the same worktree. Terminals are the streaming unit; agents
//! are what the sidebar lists (grouped by repo). A session exiting suspends the agent (primary)
//! or removes the terminal (shell); only delete destroys a worktree. See `docs/DESIGN.md` §5,
//! `docs/SPLITS.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use amux_core::adapter::{AgentAdapter, HookSetup, LaunchContext};
use amux_core::agent::{
    is_notable_transition, next_state, AgentId, AgentState, RepoId, TerminalId,
};
use amux_core::hook::{classify, HookReport};
use amux_core::worktree::{WorktreeLocation, WorktreeService};
use amux_proto::{AgentInfo, DaemonMsg, RepoInfo, Size};

use crate::pty::Session;

const EVENT_BACKLOG: usize = 256;
const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };

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
    #[allow(dead_code)] // durable metadata; used by state.json persistence (deferred)
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
}

impl Registry {
    pub fn new(adapter: Box<dyn AgentAdapter>) -> Arc<Self> {
        Self::build(adapter, None)
    }

    /// Build a registry that wires launched agents' hooks to `socket`, invoking `amux_exe hook`.
    pub fn with_hooks(
        adapter: Box<dyn AgentAdapter>,
        socket: PathBuf,
        amux_exe: PathBuf,
    ) -> Arc<Self> {
        Self::build(adapter, Some(HookIntegration { socket, amux_exe }))
    }

    fn build(adapter: Box<dyn AgentAdapter>, hooks: Option<HookIntegration>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        Arc::new(Self {
            state: Mutex::new(State::default()),
            adapter,
            events,
            hooks,
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
            let _ = self.events.send(DaemonMsg::RepoAdded(info.clone()));
        }
        info
    }

    /// Register the repository at `path` (idempotent), building its worktree service.
    pub fn register_path(&self, path: &Path, location: WorktreeLocation) -> Result<RepoInfo> {
        let worktrees = WorktreeService::new(path, location).context("open repository")?;
        Ok(self.register(worktrees))
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
        // The agent id is exported to the CLI's hooks, so it must exist before we launch.
        let agent_id = AgentId::new();
        let agent_full = agent_id.to_full_string();
        let ctx = LaunchContext {
            worktree: &worktree,
            branch,
            resume: None,
            agent_id: &agent_full,
            hooks: self.hook_setup(),
        };
        self.adapter.prepare_worktree(&ctx)?;
        let spec = self.adapter.spawn_spec(&ctx);
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;

        let terminal_id = TerminalId::new();
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
                state: AgentState::Working,
                unread: false,
                primary: terminal_id,
            };
            let info = agent.info();
            state.agents.insert(agent_id, agent);
            info
        };
        self.spawn_primary_monitor(agent_id, terminal_id, session);
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
        let session = Session::spawn(&[shell], &worktree, &[], DEFAULT_SIZE)?;
        {
            let mut state = self.state.lock().unwrap();
            state.terminals.insert(
                new,
                Terminal {
                    agent,
                    worktree,
                    primary: false,
                    session: Some(Arc::clone(&session)),
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
        let (state_change, unread_change) = {
            let mut state = self.state.lock().unwrap();
            if !state.agents.contains_key(&report.agent) {
                return;
            }
            if let Some(sid) = classified.session_id {
                if let Some(agent) = state.agents.get_mut(&report.agent) {
                    agent.ai_session_id = Some(sid);
                }
            }
            match classified.event {
                Some(event) => {
                    let next = {
                        let agent = state.agents.get(&report.agent).unwrap();
                        next_state(&agent.state, &event)
                    };
                    set_state(&mut state, report.agent, next)
                }
                None => (None, None),
            }
        };
        self.broadcast_changes(report.agent, state_change, unread_change);
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
        let spec = self.adapter.spawn_spec(&ctx);
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;
        let state_change = {
            let mut state = self.state.lock().unwrap();
            if let Some(term) = state.terminals.get_mut(&primary) {
                term.session = Some(Arc::clone(&session));
            }
            match state.agents.get_mut(&id) {
                Some(agent) => {
                    agent.state = AgentState::Working;
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
        let registry = Arc::clone(self);
        let mut exit_rx = session.exit_rx();
        tokio::spawn(async move {
            let _ = exit_rx.changed().await;
            registry.on_primary_exit(agent, terminal, session.exit_code());
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

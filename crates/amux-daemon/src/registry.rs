//! The agent registry. The daemon manages many **repos**; each **Agent** is a durable workspace
//! belonging to one repo — usually a worktree on its own branch, or (for a singleton HEAD session)
//! the repo root on `HEAD` with no managed worktree — and owns a **primary terminal** (its CLI) plus any
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
/// How long agents get to exit on their own when the daemon shuts down deliberately, before they
/// are killed. Long enough for a coding agent to checkpoint, short enough that a stuck one doesn't
/// hold up an upgrade.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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

/// A session's workspace: either an amux-managed worktree on its own branch, or the repo root
/// on `HEAD` (a branchless "HEAD session" — no managed worktree, no amux-created branch). Making
/// "has a managed worktree" an explicit variant forces every branch-assuming path (delete/prune,
/// uniqueness, labels) to handle the branchless case.
#[derive(Clone)]
enum Workspace {
    Worktree { branch: String },
    Head,
}

impl Workspace {
    /// The agent's branch, or `None` for a HEAD session.
    fn branch(&self) -> Option<&str> {
        match self {
            Workspace::Worktree { branch } => Some(branch),
            Workspace::Head => None,
        }
    }

    /// Sidebar/pane label: the branch basename for a worktree, or `"HEAD"` for a HEAD session.
    fn name(&self) -> String {
        match self {
            Workspace::Worktree { branch } => agent_name(branch),
            Workspace::Head => "HEAD".to_string(),
        }
    }
}

struct Agent {
    id: AgentId,
    repo: RepoId,
    workspace: Workspace,
    worktree: PathBuf,
    ai_session_id: Option<String>,
    /// The task this agent was dispatched with, if any. Kept (not consumed) so a relaunch that
    /// has no conversation to resume — `ai_session_id` only arrives from a hook, so an agent that
    /// died at boot has none — starts on the same task instead of an empty session. The adapter
    /// drops it whenever `--resume` applies, so it is passed unconditionally.
    prompt: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    /// When the user last opened (viewed) this agent — the sidebar's MRU key. Stamped on
    /// create, focus, and resume; persisted so ordering survives a daemon restart.
    last_opened: DateTime<Utc>,
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
            name: self.workspace.name(),
            branch: self.workspace.branch().map(str::to_string),
            state: self.state.clone(),
            last_activity: self.last_activity,
            last_opened: self.last_opened,
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
    /// closing — and, since they are part of [`PersistedState`], a daemon restart or an upgrade too.
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
    /// Each agent's pane layout, so tiled splits survive a daemon restart rather than collapsing
    /// back to a single pane. A `Vec` of pairs because `AgentId` is not a JSON object key;
    /// `default` so a `state.json` written before this field still loads.
    #[serde(default)]
    layouts: Vec<(AgentId, amux_proto::Layout)>,
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
    /// The agent's branch, or `None` for a branchless HEAD session. Flat (rather than a nested
    /// `Workspace`) so older `state.json` files with a plain `branch` string still load, and a
    /// now-unused `name` field is simply ignored. `Some` ⇒ `Workspace::Worktree`, `None` ⇒ `Head`.
    #[serde(default)]
    branch: Option<String>,
    worktree: PathBuf,
    ai_session_id: Option<String>,
    /// The task the agent was dispatched with. `default` so older `state.json` files load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    #[serde(default)]
    last_opened: Option<DateTime<Utc>>,
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
        // Guard: never register a path inside amux's own worktrees directory. `discover_repo`
        // already collapses a worktree cwd to its main repo, but a worktree path can still arrive
        // raw (via `--repo <worktree>` or the create-at flow); accepting it would mint a phantom
        // repo named after a branch (the "mount" bug).
        if amux_core::worktree::is_managed_worktree(path)? {
            anyhow::bail!(
                "{} is inside amux's worktrees directory (an agent worktree, not a repository)",
                path.display()
            );
        }
        let worktrees = WorktreeService::new(path, location).context("open repository")?;
        Ok(self.register(worktrees))
    }

    /// Save (or clear, on `None`) an agent's pane layout for replay to re-attaching clients, and
    /// persist it. Saves only on a real change: the client re-sends its layout on every reconcile
    /// (a resize, a focus move), so writing unconditionally would hammer the disk — the same
    /// anti-churn reasoning as [`Self::set_minis`].
    pub fn set_layout(&self, agent: AgentId, layout: Option<amux_proto::Layout>) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            match layout {
                Some(l) if state.layouts.get(&agent) == Some(&l) => false,
                Some(l) => {
                    state.layouts.insert(agent, l);
                    true
                }
                None => state.layouts.remove(&agent).is_some(),
            }
        };
        if changed {
            self.save();
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
                        branch: a.workspace.branch().map(str::to_string),
                        worktree: a.worktree.clone(),
                        ai_session_id: a.ai_session_id.clone(),
                        prompt: a.prompt.clone(),
                        created_at: a.created_at,
                        last_activity: a.last_activity,
                        last_opened: Some(a.last_opened),
                        unread: a.unread,
                        primary: a.primary,
                    })
                    .collect(),
                minis: state.minis.clone(),
                active: state.active,
                layouts: state
                    .layouts
                    .iter()
                    .map(|(id, l)| (*id, l.clone()))
                    .collect(),
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
            let workspace = match a.branch {
                Some(branch) => Workspace::Worktree { branch },
                None => Workspace::Head,
            };
            state.agents.insert(
                a.id,
                Agent {
                    id: a.id,
                    repo: a.repo,
                    workspace,
                    worktree: a.worktree,
                    ai_session_id: a.ai_session_id,
                    prompt: a.prompt,
                    created_at: a.created_at,
                    last_activity: a.last_activity,
                    last_opened: a.last_opened.unwrap_or(a.last_activity),
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
        // Restore each surviving agent's layout, blanking the leaves whose PTYs died with the
        // previous daemon. Built separately because it reads `state.agents` while writing
        // `state.layouts`.
        let layouts: HashMap<AgentId, amux_proto::Layout> = persisted
            .layouts
            .into_iter()
            .filter_map(|(id, layout)| {
                let agent = state.agents.get(&id)?;
                Some((id, blank_dead_terminals(&layout, agent.primary)))
            })
            .collect();
        state.layouts = layouts;
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

    /// Register the repo at `path` (idempotent) and create an agent on `branch` in it. The
    /// "new agent in a new repo" flow registers a repo rather than dispatching work, so it never
    /// carries a task.
    pub fn create_at(self: &Arc<Self>, path: &Path, branch: &str) -> Result<AgentInfo> {
        let info = self.register_path(path, WorktreeLocation::Global)?;
        self.create(info.id, branch, None)
    }

    /// Register the repo at `path` (idempotent) and create its singleton HEAD session. Registration
    /// is what the caller cannot do for itself: a client only learns a `RepoId` from the
    /// `RepoAdded` broadcast, which a repo the daemon already knows never emits.
    pub fn create_head_at(self: &Arc<Self>, path: &Path) -> Result<AgentInfo> {
        let info = self.register_path(path, WorktreeLocation::Global)?;
        self.create_head(info.id)
    }

    /// Create an agent in `repo`: worktree + a primary terminal running the agent CLI. `prompt` is
    /// the task to start it on — `Some` dispatches an agent already working, `None` launches it
    /// idle at its prompt.
    pub fn create(
        self: &Arc<Self>,
        repo: RepoId,
        branch: &str,
        prompt: Option<&str>,
    ) -> Result<AgentInfo> {
        let worktrees = self.worktrees_for(repo).context("no such repo")?;
        // One agent per (repo, branch): a branch maps to a single worktree, so a duplicate would
        // just collide. Refuse early with a clear message rather than a raw git error.
        {
            let state = self.state.lock().unwrap();
            if state
                .agents
                .values()
                .any(|a| a.repo == repo && a.workspace.branch() == Some(branch))
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
            branch: Some(branch),
            resume: None,
            prompt,
            agent_id: &agent_full,
            hooks: self.hook_setup(),
            settings_path: None,
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
                workspace: Workspace::Worktree {
                    branch: branch.to_string(),
                },
                worktree,
                ai_session_id: None,
                prompt: prompt.map(str::to_string),
                created_at: now,
                last_activity: now,
                last_opened: now,
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

    /// Create (or return the existing) singleton branchless **HEAD session** in `repo`: an agent
    /// running in the repo root on `HEAD`, with no amux-managed worktree and no amux-created
    /// branch. Deliberately breaks the isolation invariant (it shares the user's live tree); the
    /// blast radius is contained by the singleton, the explicit `Workspace::Head` type, and hook
    /// settings written out of tree (so nothing is written into the repo). See `docs/DESIGN.md` §2.
    pub fn create_head(self: &Arc<Self>, repo: RepoId) -> Result<AgentInfo> {
        let worktrees = self.worktrees_for(repo).context("no such repo")?;
        // Singleton: one HEAD session per repo. Return the existing one rather than duplicating.
        {
            let state = self.state.lock().unwrap();
            if let Some(existing) = state
                .agents
                .values()
                .find(|a| a.repo == repo && matches!(a.workspace, Workspace::Head))
            {
                return Ok(existing.info());
            }
        }
        // Runs in the repo root itself — no worktree is created, so the "branch already checked
        // out" guard is never reached.
        let repo_root = worktrees.repo().to_path_buf();
        let agent_id = AgentId::new();
        let terminal_id = TerminalId::new();
        let agent_full = agent_id.to_full_string();
        let hooks = self.hook_setup();
        // Only compute the out-of-tree settings path when hooks are on; without them there's
        // nothing to write and we must not touch the live tree.
        let settings = hooks
            .as_ref()
            .map(|_| amux_core::paths::head_settings_path(&agent_id))
            .transpose()?;
        let ctx = LaunchContext {
            worktree: &repo_root,
            branch: None,
            resume: None,
            // A HEAD session is "help me with what I'm doing right now" — conversational by
            // nature, and a singleton, so it takes no dispatched task.
            prompt: None,
            agent_id: &agent_full,
            hooks,
            settings_path: settings.as_deref(),
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
                    worktree: repo_root.clone(),
                    primary: true,
                    session: Some(Arc::clone(&session)),
                    passthrough: false,
                },
            );
            let agent = Agent {
                id: agent_id,
                repo,
                workspace: Workspace::Head,
                worktree: repo_root,
                ai_session_id: None,
                prompt: None,
                created_at: now,
                last_activity: now,
                last_opened: now,
                // Same reasoning as `create`: launch Idle and let hooks drive Working.
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
    /// unread bit and stamps its `last_opened` (the sidebar's MRU key — persisted and broadcast);
    /// while focused, notable events on it won't re-mark it unread.
    pub fn focus(&self, agent: Option<AgentId>) {
        let (opened, cleared) = {
            let mut state = self.state.lock().unwrap();
            state.focused = agent;
            match agent.and_then(|id| state.agents.get_mut(&id).map(|a| (id, a))) {
                Some((id, a)) => {
                    let at = Utc::now();
                    a.last_opened = at;
                    let cleared = if a.unread {
                        a.unread = false;
                        true
                    } else {
                        false
                    };
                    (Some((id, at)), cleared.then_some(id))
                }
                None => (None, None),
            }
        };
        if let Some((id, at)) = opened {
            self.save();
            let _ = self.events.send(DaemonMsg::OpenedChanged { id, at });
        }
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
        let (worktree, branch, resume_id, prompt, primary, already_live) = {
            let state = self.state.lock().unwrap();
            let agent = state.agents.get(&id).context("no such agent")?;
            let live = state
                .terminals
                .get(&agent.primary)
                .is_some_and(|t| t.session.is_some());
            (
                agent.worktree.clone(),
                agent.workspace.branch().map(str::to_string),
                agent.ai_session_id.clone(),
                agent.prompt.clone(),
                agent.primary,
                live,
            )
        };
        if already_live {
            return Ok(());
        }
        let agent_full = id.to_full_string();
        let hooks = self.hook_setup();
        // A HEAD session (no branch) resumes in the repo root with out-of-tree hook settings, just
        // as it was first launched — never writing into the user's live tree.
        let settings = match (&branch, &hooks) {
            (None, Some(_)) => Some(amux_core::paths::head_settings_path(&id)?),
            _ => None,
        };
        let ctx = LaunchContext {
            worktree: &worktree,
            branch: branch.as_deref(),
            resume: resume_id.as_deref(),
            // Passed unconditionally: the adapter drops the task whenever `--resume` applies (a
            // resumed conversation already contains it), so this only takes effect when there is
            // no session to resume — an agent that died before its first hook.
            prompt: prompt.as_deref(),
            agent_id: &agent_full,
            hooks,
            settings_path: settings.as_deref(),
        };
        self.adapter.prepare_worktree(&ctx)?;
        let mut spec = self.adapter.spawn_spec(&ctx);
        spec.env
            .push(("AMUX_TERMINAL_ID".to_string(), primary.to_full_string()));
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;
        let (state_change, at) = {
            let mut state = self.state.lock().unwrap();
            if let Some(term) = state.terminals.get_mut(&primary) {
                term.session = Some(Arc::clone(&session));
            }
            match state.agents.get_mut(&id) {
                Some(agent) => {
                    // Resuming lands back at the prompt — idle/waiting, not working (same reasoning
                    // as create): let hooks drive Working so we don't flash a false "⋯"/unread.
                    agent.state = AgentState::Idle;
                    let at = Utc::now();
                    agent.last_activity = at;
                    agent.last_opened = at;
                    (agent.state.clone(), at)
                }
                None => return Ok(()),
            }
        };
        self.spawn_primary_monitor(id, primary, session);
        self.save();
        let _ = self.events.send(DaemonMsg::StateChanged {
            id,
            state: state_change,
        });
        let _ = self.events.send(DaemonMsg::OpenedChanged { id, at });
        Ok(())
    }

    /// Delete an agent — the only destructive op: kill all its terminals and remove its worktree
    /// (branch and commits are kept). Refuses a dirty worktree unless `force`.
    pub fn delete(&self, id: AgentId, force: bool) -> Result<DeleteOutcome> {
        let (branch, repo) = match self.state.lock().unwrap().agents.get(&id) {
            Some(agent) => (agent.workspace.branch().map(str::to_string), agent.repo),
            None => return Ok(DeleteOutcome::Deleted),
        };
        let worktrees = self.worktrees_for(repo);
        if !force {
            // Only a worktree session can be dirty; a HEAD session owns no managed worktree.
            let dirty = branch
                .as_deref()
                .and_then(|b| worktrees.as_ref().and_then(|w| w.dirty_count(b).ok()))
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
        // Remove the managed worktree/branch only for a worktree session; a HEAD session has none
        // and must never prune anything in the user's live tree. Clean up its out-of-tree hook
        // settings file instead (branchless ⇒ HEAD session).
        match branch.as_deref() {
            Some(branch) => {
                if let Some(worktrees) = worktrees {
                    worktrees.remove(branch).ok();
                }
            }
            None => {
                if let Ok(path) = amux_core::paths::head_settings_path(&id) {
                    let _ = std::fs::remove_file(path);
                }
            }
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
                .filter_map(|a| a.workspace.branch().map(str::to_string))
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

    /// Shut every live terminal down **gracefully**: SIGTERM, a shared grace period, then SIGKILL.
    ///
    /// Sessions are terminated concurrently under one budget, so a wedged agent delays shutdown by
    /// [`SHUTDOWN_GRACE`] at most rather than by the sum of them. The sessions are collected out of
    /// the lock first — holding the registry mutex across an await would block every other task.
    pub async fn shutdown_all(&self) {
        let sessions: Vec<Arc<Session>> = {
            let state = self.state.lock().unwrap();
            state
                .terminals
                .values()
                .filter_map(|t| t.session.clone())
                .collect()
        };
        futures::future::join_all(
            sessions
                .iter()
                .map(|session| session.terminate(SHUTDOWN_GRACE)),
        )
        .await;
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

/// Strip terminal ids that no longer exist from a reloaded layout, keeping the geometry.
///
/// PTYs die with the daemon. The agent's **primary** id is durable (it is persisted and reused by
/// `resume`), so its leaf survives untouched; every other leaf held a shell terminal whose process
/// is gone, and handing that id back to a client would have it `Attach` to nothing. Those leaves
/// become blank — the same state a fresh split occupies before its shell arrives — so the client
/// can refill them (see `PaneTree::fill_blanks`). Splits keep their axis and ratio, which is what
/// makes the restored screen look like the one you left.
fn blank_dead_terminals(layout: &amux_proto::Layout, keep: TerminalId) -> amux_proto::Layout {
    match layout {
        amux_proto::Layout::Leaf { terminal } => amux_proto::Layout::Leaf {
            terminal: terminal.filter(|t| *t == keep),
        },
        amux_proto::Layout::Split {
            axis,
            ratio,
            first,
            second,
        } => amux_proto::Layout::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(blank_dead_terminals(first, keep)),
            second: Box::new(blank_dead_terminals(second, keep)),
        },
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::nav::Axis;
    use amux_proto::Layout;

    fn leaf(t: Option<TerminalId>) -> Layout {
        Layout::Leaf { terminal: t }
    }

    fn split(first: Layout, second: Layout) -> Layout {
        Layout::Split {
            axis: Axis::LeftRight,
            ratio: 0.35,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// The primary's leaf survives (its id is durable across a restart); shell leaves are blanked
    /// because their PTYs died with the daemon — and the geometry is preserved exactly, which is
    /// the whole point of persisting the layout.
    #[test]
    fn blanking_keeps_the_primary_and_the_geometry() {
        let primary = TerminalId::new();
        let shell = TerminalId::new();
        let other_shell = TerminalId::new();

        let saved = split(
            leaf(Some(primary)),
            split(leaf(Some(shell)), leaf(Some(other_shell))),
        );
        let restored = blank_dead_terminals(&saved, primary);

        assert_eq!(
            restored,
            split(leaf(Some(primary)), split(leaf(None), leaf(None))),
            "only the primary keeps its terminal; axes and ratios are untouched"
        );
    }

    /// A layout that is a bare primary pane comes back completely unchanged.
    #[test]
    fn blanking_a_single_primary_pane_is_a_no_op() {
        let primary = TerminalId::new();
        let saved = leaf(Some(primary));
        assert_eq!(blank_dead_terminals(&saved, primary), saved);
    }

    /// An already-blank leaf (a split whose shell had not arrived yet) stays blank rather than
    /// being confused for a live terminal.
    #[test]
    fn blanking_leaves_an_empty_pane_empty() {
        let primary = TerminalId::new();
        let saved = split(leaf(Some(primary)), leaf(None));
        assert_eq!(blank_dead_terminals(&saved, primary), saved);
    }
}

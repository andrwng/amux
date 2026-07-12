//! The agent registry — durable **Agents** over generic **Sessions**. An `Agent` is the
//! workspace (worktree + branch + adapter + `ai_session_id`); it holds zero-or-one live
//! `Session`. A session exiting **suspends** the agent (state → Exited, session → None) but
//! never removes it; only [`Registry::delete`] destroys a worktree. Resume is always manual.
//! See `docs/DESIGN.md` §5 and `docs/PHASE-1.md` §1.5.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use amux_core::adapter::{AgentAdapter, LaunchContext};
use amux_core::agent::{AgentId, AgentState};
use amux_core::worktree::WorktreeService;
use amux_proto::{AgentInfo, DaemonMsg, Size};

use crate::pty::Session;

/// Backlog of lifecycle events buffered per subscribed client before it must resync.
const EVENT_BACKLOG: usize = 256;
/// PTY size a session starts at, before a client attaches and resizes it.
const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };

struct Agent {
    id: AgentId,
    name: String,
    branch: String,
    worktree: PathBuf,
    ai_session_id: Option<String>,
    #[allow(dead_code)] // durable metadata; used by state.json persistence (deferred to later 1.5)
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    state: AgentState,
    session: Option<Arc<Session>>,
}

impl Agent {
    fn info(&self) -> AgentInfo {
        AgentInfo {
            id: self.id,
            name: self.name.clone(),
            branch: self.branch.clone(),
            state: self.state.clone(),
            last_activity: self.last_activity,
        }
    }
}

pub struct Registry {
    agents: Mutex<HashMap<AgentId, Agent>>,
    worktrees: WorktreeService,
    adapter: Box<dyn AgentAdapter>,
    events: broadcast::Sender<DaemonMsg>,
}

impl Registry {
    pub fn new(worktrees: WorktreeService, adapter: Box<dyn AgentAdapter>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        Arc::new(Self {
            agents: Mutex::new(HashMap::new()),
            worktrees,
            adapter,
            events,
        })
    }

    /// Subscribe to lifecycle events (AgentAdded / AgentRemoved / StateChanged).
    pub fn subscribe_events(&self) -> broadcast::Receiver<DaemonMsg> {
        self.events.subscribe()
    }

    /// Current roster.
    pub fn infos(&self) -> Vec<AgentInfo> {
        self.agents
            .lock()
            .unwrap()
            .values()
            .map(Agent::info)
            .collect()
    }

    /// The live session for an agent, if it has one.
    pub fn session(&self, id: AgentId) -> Option<Arc<Session>> {
        self.agents
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|a| a.session.clone())
    }

    /// Create an agent: worktree + a fresh session. Broadcasts `AgentAdded`.
    pub fn create(self: &Arc<Self>, branch: &str) -> Result<AgentInfo> {
        let worktree = self.worktrees.create(branch).context("create worktree")?;
        let ctx = LaunchContext {
            worktree: &worktree,
            branch,
            resume: None,
        };
        self.adapter.prepare_worktree(&ctx)?;
        let spec = self.adapter.spawn_spec(&ctx);
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;

        let id = AgentId::new();
        let now = Utc::now();
        let agent = Agent {
            id,
            name: agent_name(branch),
            branch: branch.to_string(),
            worktree,
            ai_session_id: None,
            created_at: now,
            last_activity: now,
            state: AgentState::Working,
            session: Some(Arc::clone(&session)),
        };
        let info = agent.info();
        self.agents.lock().unwrap().insert(id, agent);
        self.spawn_exit_monitor(id, session);
        let _ = self.events.send(DaemonMsg::AgentAdded(info.clone()));
        Ok(info)
    }

    /// Resume a suspended agent — a new session in the existing worktree. `StateChanged`.
    pub fn resume(self: &Arc<Self>, id: AgentId) -> Result<()> {
        let (branch, worktree, resume_id) = {
            let agents = self.agents.lock().unwrap();
            let agent = agents.get(&id).context("no such agent")?;
            if agent.session.is_some() {
                return Ok(()); // already live
            }
            (
                agent.branch.clone(),
                agent.worktree.clone(),
                agent.ai_session_id.clone(),
            )
        };
        let ctx = LaunchContext {
            worktree: &worktree,
            branch: &branch,
            resume: resume_id.as_deref(),
        };
        let spec = self.adapter.spawn_spec(&ctx);
        let session = Session::spawn(&spec.command, &spec.cwd, &spec.env, DEFAULT_SIZE)?;

        let state = {
            let mut agents = self.agents.lock().unwrap();
            let agent = agents.get_mut(&id).context("agent vanished")?;
            agent.session = Some(Arc::clone(&session));
            agent.state = AgentState::Working;
            agent.last_activity = Utc::now();
            agent.state.clone()
        };
        self.spawn_exit_monitor(id, session);
        let _ = self.events.send(DaemonMsg::StateChanged { id, state });
        Ok(())
    }

    /// Delete an agent — the only destructive op: kill its session and remove its worktree.
    pub fn delete(&self, id: AgentId) -> Result<()> {
        let agent = self.agents.lock().unwrap().remove(&id);
        if let Some(agent) = agent {
            if let Some(session) = &agent.session {
                session.kill();
            }
            self.worktrees.remove(&agent.branch).ok();
            let _ = self.events.send(DaemonMsg::AgentRemoved { id });
        }
        Ok(())
    }

    /// Kill every live session (daemon shutdown). Worktrees are left on disk.
    pub fn shutdown_all(&self) {
        for agent in self.agents.lock().unwrap().values() {
            if let Some(session) = &agent.session {
                session.kill();
            }
        }
    }

    /// Watch a session; when it exits, suspend the agent (Exited + no session) and broadcast.
    fn spawn_exit_monitor(self: &Arc<Self>, id: AgentId, session: Arc<Session>) {
        let registry = Arc::clone(self);
        let mut exit_rx = session.exit_rx();
        tokio::spawn(async move {
            let _ = exit_rx.changed().await; // resolves when the session exits (or drops)
            let code = session.exit_code();
            registry.mark_exited(id, code);
        });
    }

    fn mark_exited(&self, id: AgentId, code: Option<i32>) {
        let state = {
            let mut agents = self.agents.lock().unwrap();
            match agents.get_mut(&id) {
                Some(agent) => {
                    agent.session = None;
                    agent.state = AgentState::Exited { code };
                    agent.last_activity = Utc::now();
                    agent.state.clone()
                }
                None => return, // already deleted
            }
        };
        let _ = self.events.send(DaemonMsg::StateChanged { id, state });
    }
}

/// Human-friendly name from a branch: its last path segment.
fn agent_name(branch: &str) -> String {
    branch.rsplit('/').next().unwrap_or(branch).to_string()
}

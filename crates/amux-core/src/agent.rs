//! The agent domain model: identity + the state machine that the sidebar renders. Pure and
//! exhaustively tested — the correctness core grove's screen-scraping never had. See
//! `docs/DESIGN.md` §4.2.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable, persisted identity for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form: the first 8 hex chars, enough to disambiguate in the UI.
        write!(f, "{:.8}", self.0.as_simple().to_string())
    }
}

/// Stable identity for a terminal — a PTY within a worktree. An agent has a primary terminal
/// (its CLI) plus any shell terminals split off in the same worktree. See `docs/SPLITS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalId(Uuid);

impl TerminalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TerminalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.8}", self.0.as_simple().to_string())
    }
}

/// Why an agent wants the user's attention. Only produced from Phase 2 hook signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionKind {
    Permission,
    Question,
    Info,
}

/// What an agent is doing right now — the thing the sidebar renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Spawned, not yet producing output.
    Starting,
    /// Actively producing output / working.
    Working,
    /// Blocked on the user (permission or a question).
    NeedsAttention {
        kind: AttentionKind,
        message: Option<String>,
    },
    /// Alive but quiet — waiting for the next instruction.
    Idle,
    /// The process exited.
    Exited { code: Option<i32> },
    /// A daemon-side failure.
    Error { message: String },
}

impl AgentState {
    /// A single glyph for the sidebar status column.
    pub fn glyph(&self) -> char {
        match self {
            AgentState::Starting => '◌',
            AgentState::Working => '●',
            AgentState::NeedsAttention { .. } => '⚠',
            AgentState::Idle => '○',
            AgentState::Exited { .. } => '□',
            AgentState::Error { .. } => '✗',
        }
    }

    /// Terminal states no longer transition on coarse activity/idle signals.
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentState::Exited { .. } | AgentState::Error { .. })
    }

    /// Sidebar sort bucket — lower sorts higher: attention first, terminal last.
    pub fn priority(&self) -> u8 {
        match self {
            AgentState::NeedsAttention { .. } => 0,
            AgentState::Working | AgentState::Starting => 1,
            AgentState::Idle => 2,
            AgentState::Exited { .. } | AgentState::Error { .. } => 3,
        }
    }
}

/// Inputs that drive [`next_state`]. Phase 1 emits the coarse ones; `NeedsUser` arrives in
/// Phase 2 from Claude Code hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Started,
    SawActivity,
    WentIdle,
    NeedsUser {
        kind: AttentionKind,
        message: Option<String>,
    },
    Exited(Option<i32>),
    Failed(String),
}

/// The pure, total transition function. No side effects — trivially testable.
pub fn next_state(current: &AgentState, event: &AgentEvent) -> AgentState {
    use AgentEvent as E;
    match (current.is_terminal(), event) {
        // Terminal + restart signals win from any state.
        (_, E::Exited(code)) => AgentState::Exited { code: *code },
        (_, E::Failed(message)) => AgentState::Error {
            message: message.clone(),
        },
        (_, E::Started) => AgentState::Starting,
        // Once terminal, coarse signals are ignored.
        (true, _) => current.clone(),
        (false, E::SawActivity) => AgentState::Working,
        (false, E::NeedsUser { kind, message }) => AgentState::NeedsAttention {
            kind: kind.clone(),
            message: message.clone(),
        },
        // Idleness doesn't clear a pending ⚠ — the user is still owed a response.
        (false, E::WentIdle) => match current {
            AgentState::NeedsAttention { .. } => current.clone(),
            _ => AgentState::Idle,
        },
    }
}

/// The facet of an agent the sidebar sorts on — kept separate from the full agent record so
/// the ordering policy stays pure and testable. The whole "how is the inbox ordered" question
/// lives in [`sort_for_sidebar`]: one place to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterItem {
    pub id: AgentId,
    pub state: AgentState,
    pub last_activity: DateTime<Utc>,
}

/// Order agents for the sidebar: those needing attention first, then everything else by
/// most-recent activity, terminal (exited/error) states last. Stable.
///
/// This is the tweak point. To instead float the **longest-waiting** attention item to the
/// very top, reverse the recency comparison for the `priority() == 0` bucket.
pub fn sort_for_sidebar(items: &mut [RosterItem]) {
    items.sort_by(|a, b| {
        a.state
            .priority()
            .cmp(&b.state.priority())
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needs_attention() -> AgentState {
        AgentState::NeedsAttention {
            kind: AttentionKind::Permission,
            message: None,
        }
    }

    #[test]
    fn activity_moves_to_working_from_any_live_state() {
        for start in [AgentState::Starting, AgentState::Idle, needs_attention()] {
            assert_eq!(
                next_state(&start, &AgentEvent::SawActivity),
                AgentState::Working
            );
        }
    }

    #[test]
    fn idle_from_working_but_attention_persists() {
        assert_eq!(
            next_state(&AgentState::Working, &AgentEvent::WentIdle),
            AgentState::Idle
        );
        assert_eq!(
            next_state(&needs_attention(), &AgentEvent::WentIdle),
            needs_attention()
        );
    }

    #[test]
    fn needs_user_raises_attention_with_details() {
        let s = next_state(
            &AgentState::Working,
            &AgentEvent::NeedsUser {
                kind: AttentionKind::Question,
                message: Some("which region?".into()),
            },
        );
        assert_eq!(
            s,
            AgentState::NeedsAttention {
                kind: AttentionKind::Question,
                message: Some("which region?".into()),
            }
        );
    }

    #[test]
    fn exit_and_error_are_terminal_and_sticky() {
        let exited = next_state(&AgentState::Working, &AgentEvent::Exited(Some(0)));
        assert_eq!(exited, AgentState::Exited { code: Some(0) });
        assert_eq!(next_state(&exited, &AgentEvent::SawActivity), exited);
        assert_eq!(next_state(&exited, &AgentEvent::WentIdle), exited);

        let errored = next_state(&AgentState::Idle, &AgentEvent::Failed("boom".into()));
        assert_eq!(
            errored,
            AgentState::Error {
                message: "boom".into()
            }
        );
        assert_eq!(next_state(&errored, &AgentEvent::SawActivity), errored);
    }

    #[test]
    fn started_resets_even_a_terminal_state() {
        assert_eq!(
            next_state(&AgentState::Exited { code: Some(1) }, &AgentEvent::Started),
            AgentState::Starting
        );
    }

    #[test]
    fn every_state_has_a_distinct_glyph() {
        use std::collections::HashSet;
        let glyphs: HashSet<char> = [
            AgentState::Starting,
            AgentState::Working,
            needs_attention(),
            AgentState::Idle,
            AgentState::Exited { code: None },
            AgentState::Error {
                message: String::new(),
            },
        ]
        .iter()
        .map(AgentState::glyph)
        .collect();
        assert_eq!(glyphs.len(), 6);
    }

    #[test]
    fn sidebar_order_is_attention_first_then_recent() {
        let base = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = |secs: i64| base + chrono::Duration::seconds(secs);
        let item = |state, secs| RosterItem {
            id: AgentId::new(),
            state,
            last_activity: at(secs),
        };
        let mut items = vec![
            item(AgentState::Idle, 100),
            item(needs_attention(), 10),
            item(AgentState::Working, 50),
            item(
                AgentState::NeedsAttention {
                    kind: AttentionKind::Question,
                    message: None,
                },
                30,
            ),
            item(AgentState::Exited { code: Some(0) }, 200),
        ];
        sort_for_sidebar(&mut items);

        let buckets: Vec<u8> = items.iter().map(|i| i.state.priority()).collect();
        assert_eq!(buckets, vec![0, 0, 1, 2, 3]); // attention, attention, working, idle, exited
        assert!(items[0].last_activity >= items[1].last_activity); // recent-first within a bucket
    }
}

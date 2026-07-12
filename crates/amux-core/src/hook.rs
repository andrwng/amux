//! Claude Code hook payloads and the pure mapping from a hook event to an [`AgentEvent`]. This
//! is the "exact status, no screen-scraping" core of Phase 2: Claude's hooks fire structured
//! JSON, `amux hook` forwards it to the daemon mailbox, and [`classify`] turns it into a state
//! transition. Kept pure and fixture-tested — the single place the hook→state policy lives.
//!
//! See `docs/DESIGN.md` §4.2–4.3. Hook payload shape follows Claude Code's documented stdin
//! JSON (common fields + per-event fields); unknown fields are ignored so the parser is
//! forward-compatible.

use serde::{Deserialize, Serialize};

use crate::agent::{AgentEvent, AttentionKind};

/// A Claude Code hook payload, as delivered to the hook command on stdin. Only the fields we act
/// on are modelled; everything else in the JSON is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvent {
    /// The event kind, e.g. `Notification`, `Stop`, `PreToolUse`, `PostToolUse`,
    /// `UserPromptSubmit`, `SessionStart`.
    pub hook_event_name: String,
    /// The Claude session id — stable for the session's life; used for `claude --resume <id>`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Structured notification kind (present on `Notification`): `permission_prompt`,
    /// `idle_prompt`, `agent_needs_input`, `agent_completed`, … — the authoritative
    /// permission-vs-idle discriminator (preferred over the free-text `message`).
    #[serde(default)]
    pub notification_type: Option<String>,
    /// Free-text message (present on `Notification`): what Claude wants from the user.
    #[serde(default)]
    pub message: Option<String>,
    /// The tool involved (present on `PreToolUse` / `PostToolUse`).
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// What `amux hook` sends to the daemon mailbox: which agent, and the event that fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookReport {
    pub agent: crate::agent::AgentId,
    pub event: HookEvent,
}

/// The result of interpreting a hook event: an optional state transition, plus any session id to
/// capture (for resume). The session id is surfaced on every event that carries one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Classified {
    pub event: Option<AgentEvent>,
    pub session_id: Option<String>,
}

/// Map a Claude hook event to an agent state transition. **The one tweak point** for hook→state
/// policy. Exact and structured — no screen text is consulted.
///
/// - `Notification` → the agent wants you: a permission message becomes `NeedsAttention`
///   (`Permission`); a "waiting for your input" idle nudge becomes `Idle`; anything else is
///   surfaced as `NeedsAttention` (`Info`) so nothing silently vanishes.
/// - `Stop` (Claude finished responding) → `Idle`.
/// - `UserPromptSubmit` / `PreToolUse` / `PostToolUse` → `Working` (activity).
/// - `SessionStart` and others → no transition, but the session id is still captured.
pub fn classify(h: &HookEvent) -> Classified {
    let session_id = h.session_id.clone().filter(|s| !s.is_empty());
    let event = match h.hook_event_name.as_str() {
        "Notification" => Some(classify_notification(
            h.notification_type.as_deref(),
            h.message.as_deref(),
        )),
        "Stop" => Some(AgentEvent::WentIdle),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Some(AgentEvent::SawActivity),
        _ => None,
    };
    Classified { event, session_id }
}

/// Interpret a `Notification`. Prefer the structured `notification_type` (exact); fall back to
/// the free-text message only when the type is absent (older CLI builds). A permission prompt is
/// the ⚠ that must light up the sidebar; an idle nudge is quiet (○); anything else is surfaced
/// as `Info` so nothing silently vanishes.
fn classify_notification(kind: Option<&str>, message: Option<&str>) -> AgentEvent {
    let permission = || AgentEvent::NeedsUser {
        kind: AttentionKind::Permission,
        message: normalize(message),
    };
    let info = || AgentEvent::NeedsUser {
        kind: AttentionKind::Info,
        message: normalize(message),
    };
    match kind {
        Some("permission_prompt") | Some("agent_needs_input") | Some("elicitation_dialog") => {
            permission()
        }
        Some("idle_prompt") | Some("agent_completed") => AgentEvent::WentIdle,
        Some(_) => info(),
        // No structured type (older CLI): fall back to reading the message text.
        None => {
            let text = message.unwrap_or("").to_ascii_lowercase();
            if text.contains("waiting for your input") || text.contains("waiting for input") {
                AgentEvent::WentIdle
            } else if text.contains("permission")
                || text.contains("approve")
                || text.contains("allow")
            {
                permission()
            } else {
                info()
            }
        }
    }
}

fn normalize(message: Option<&str>) -> Option<String> {
    message
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str) -> HookEvent {
        HookEvent {
            hook_event_name: name.to_string(),
            session_id: Some("sess-abc".to_string()),
            notification_type: None,
            message: None,
            tool_name: None,
        }
    }

    #[test]
    fn permission_notification_needs_attention_by_type() {
        let mut h = ev("Notification");
        h.notification_type = Some("permission_prompt".to_string());
        h.message = Some("Claude needs your permission to use Bash".to_string());
        let c = classify(&h);
        assert_eq!(
            c.event,
            Some(AgentEvent::NeedsUser {
                kind: AttentionKind::Permission,
                message: Some("Claude needs your permission to use Bash".to_string()),
            })
        );
        assert_eq!(c.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn idle_prompt_notification_is_idle_by_type() {
        let mut h = ev("Notification");
        h.notification_type = Some("idle_prompt".to_string());
        h.message = Some("Claude is waiting for your input".to_string());
        assert_eq!(classify(&h).event, Some(AgentEvent::WentIdle));
    }

    #[test]
    fn structured_type_beats_message_text() {
        // A permission type wins even if the message happens to say "waiting".
        let mut h = ev("Notification");
        h.notification_type = Some("agent_needs_input".to_string());
        h.message = Some("waiting for your input".to_string());
        assert!(matches!(
            classify(&h).event,
            Some(AgentEvent::NeedsUser {
                kind: AttentionKind::Permission,
                ..
            })
        ));
    }

    #[test]
    fn permission_notification_falls_back_to_message_without_a_type() {
        let mut h = ev("Notification");
        h.message = Some("Claude needs your permission to use Bash".to_string());
        assert!(matches!(
            classify(&h).event,
            Some(AgentEvent::NeedsUser {
                kind: AttentionKind::Permission,
                ..
            })
        ));
    }

    #[test]
    fn waiting_notification_falls_back_to_idle_without_a_type() {
        let mut h = ev("Notification");
        h.message = Some("Claude is waiting for your input".to_string());
        assert_eq!(classify(&h).event, Some(AgentEvent::WentIdle));
    }

    #[test]
    fn unknown_notification_is_surfaced_as_info() {
        let mut h = ev("Notification");
        h.message = Some("Something happened".to_string());
        assert_eq!(
            classify(&h).event,
            Some(AgentEvent::NeedsUser {
                kind: AttentionKind::Info,
                message: Some("Something happened".to_string()),
            })
        );
    }

    #[test]
    fn stop_goes_idle() {
        assert_eq!(classify(&ev("Stop")).event, Some(AgentEvent::WentIdle));
    }

    #[test]
    fn activity_events_are_working() {
        for name in ["UserPromptSubmit", "PreToolUse", "PostToolUse"] {
            assert_eq!(
                classify(&ev(name)).event,
                Some(AgentEvent::SawActivity),
                "{name} should mean working"
            );
        }
    }

    #[test]
    fn session_id_is_captured_even_without_a_transition() {
        let c = classify(&ev("SessionStart"));
        assert_eq!(c.event, None);
        assert_eq!(c.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn empty_session_id_is_dropped() {
        let mut h = ev("Stop");
        h.session_id = Some(String::new());
        assert_eq!(classify(&h).session_id, None);
    }

    #[test]
    fn real_payload_json_deserializes() {
        // A representative Notification payload with extra fields we ignore.
        let json = r#"{
            "session_id": "abc123",
            "transcript_path": "/x/transcript.jsonl",
            "cwd": "/repo/wt/feat",
            "permission_mode": "default",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Edit"
        }"#;
        let h: HookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(h.hook_event_name, "Notification");
        assert_eq!(h.session_id.as_deref(), Some("abc123"));
        assert_eq!(h.notification_type.as_deref(), Some("permission_prompt"));
        let c = classify(&h);
        assert!(matches!(
            c.event,
            Some(AgentEvent::NeedsUser {
                kind: AttentionKind::Permission,
                ..
            })
        ));
    }
}

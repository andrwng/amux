//! A worktree's conversation, as amux records it: who spoke, what they said, and in which agent
//! session. CLI-agnostic — an adapter's hook interpretation (see [`crate::hook::turn_from_hook`])
//! produces a [`Turn`]; the daemon stamps it with a sequence number and its clock and keeps it in
//! a bounded per-worktree log. Pure: the clipping rule lives here so it is tested without I/O.
//! See `docs/DESIGN.md` §5.4.

use serde::{Deserialize, Serialize};

/// The most text one turn may carry, in bytes of UTF-8. `amux hook` clips to this *before*
/// encoding, so a turn message always fits the mailbox's read limit with room to spare; a larger
/// message would be cut short there, fail to decode, and be dropped whole.
pub const MAX_TURN_TEXT_BYTES: usize = 24 * 1024;

/// What a clipped text carries between the head and the tail it keeps, so a reader can see where
/// the cut is.
pub const CLIP_MARKER: &str = "\n[… clipped by amux …]\n";

/// Who said it: the user's prompt, or the agent's reply when it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    User,
    Agent,
}

/// One entry of a conversation, as it leaves the adapter seam: before the daemon gives it a
/// sequence number and a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub by: Speaker,
    /// The text, already clipped to [`MAX_TURN_TEXT_BYTES`].
    pub text: String,
    /// Whether [`clip_text`] cut the text.
    pub clipped: bool,
    /// The agent session the hook carried, if any.
    pub session: Option<String>,
}

impl Turn {
    /// Build a turn, clipping `text` to the limit.
    pub fn new(by: Speaker, text: &str, session: Option<String>) -> Self {
        let (text, clipped) = clip_text(text, MAX_TURN_TEXT_BYTES);
        Self {
            by,
            text,
            clipped,
            session,
        }
    }
}

/// Clip `text` to at most `limit` bytes, keeping its head and its tail with [`CLIP_MARKER`]
/// between them. Returns the text and whether it was cut. Cuts land on character boundaries, so
/// the result is always valid UTF-8 and never longer than `limit`.
pub fn clip_text(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let Some(room) = limit.checked_sub(CLIP_MARKER.len()) else {
        // A limit smaller than the marker leaves room for nothing but a head.
        return (text[..floor_char_boundary(text, limit)].to_string(), true);
    };
    let head_end = floor_char_boundary(text, room / 2);
    let tail_len = room - head_end;
    let tail_start = ceil_char_boundary(text, text.len() - tail_len);
    let mut out = String::with_capacity(limit);
    out.push_str(&text[..head_end]);
    out.push_str(CLIP_MARKER);
    out.push_str(&text[tail_start..]);
    (out, true)
}

/// The largest char boundary at or below `i`.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The smallest char boundary at or above `i`.
fn ceil_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_at_the_limit_is_kept_whole() {
        let text = "a".repeat(MAX_TURN_TEXT_BYTES);
        let (out, clipped) = clip_text(&text, MAX_TURN_TEXT_BYTES);
        assert!(!clipped);
        assert_eq!(out, text);
    }

    #[test]
    fn text_one_under_the_limit_is_kept_whole() {
        let text = "a".repeat(MAX_TURN_TEXT_BYTES - 1);
        assert_eq!(clip_text(&text, MAX_TURN_TEXT_BYTES), (text, false));
    }

    #[test]
    fn text_one_over_the_limit_is_clipped_to_head_and_tail() {
        let text = format!("HEAD{}TAIL", "x".repeat(MAX_TURN_TEXT_BYTES - 7));
        assert_eq!(text.len(), MAX_TURN_TEXT_BYTES + 1);
        let (out, clipped) = clip_text(&text, MAX_TURN_TEXT_BYTES);
        assert!(clipped);
        assert!(out.len() <= MAX_TURN_TEXT_BYTES, "{} bytes", out.len());
        assert!(out.starts_with("HEAD"), "keeps the head");
        assert!(out.ends_with("TAIL"), "keeps the tail");
        assert!(out.contains(CLIP_MARKER), "shows where it was cut");
    }

    #[test]
    fn a_megabyte_is_clipped_under_the_limit() {
        let text = "synthetic ".repeat(110_000);
        let (out, clipped) = clip_text(&text, MAX_TURN_TEXT_BYTES);
        assert!(clipped);
        assert!(out.len() <= MAX_TURN_TEXT_BYTES);
        assert!(out.len() > MAX_TURN_TEXT_BYTES - 8, "uses the room it has");
    }

    #[test]
    fn cuts_land_on_character_boundaries() {
        // Four-byte characters on both sides of every possible cut.
        let text = "🦀".repeat(MAX_TURN_TEXT_BYTES / 2);
        let (out, clipped) = clip_text(&text, MAX_TURN_TEXT_BYTES);
        assert!(clipped);
        assert!(out.len() <= MAX_TURN_TEXT_BYTES);
        let stripped = out.replace(CLIP_MARKER, "");
        assert!(stripped.chars().all(|c| c == '🦀'), "no split character");
    }

    #[test]
    fn a_limit_smaller_than_the_marker_keeps_only_a_head() {
        let (out, clipped) = clip_text("abcdefghij", 4);
        assert!(clipped);
        assert_eq!(out, "abcd");
    }

    #[test]
    fn speaker_serializes_as_a_lowercase_name() {
        assert_eq!(serde_json::to_string(&Speaker::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Speaker::Agent).unwrap(), "\"agent\"");
    }
}

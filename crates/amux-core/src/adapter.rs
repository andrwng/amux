//! The agent-CLI boundary. Almost nothing in amux is CLI-specific — only *how to launch a CLI*
//! and *how to derive its status*. Both live behind [`AgentAdapter`], so adding `codex`/`gemini`
//! later is one new adapter and nothing else. Status detection is a separate strategy that
//! arrives in Phase 2 (Claude hooks); Phase 1 only needs launch. See `docs/DESIGN.md` §4.3.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// What an adapter needs to know to launch (or resume) an agent in a worktree.
pub struct LaunchContext<'a> {
    pub worktree: &'a Path,
    pub branch: &'a str,
    /// A prior session id to resume, if the adapter supports it.
    pub resume: Option<&'a str>,
}

/// A concrete recipe for spawning the agent process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// What an adapter's backend supports, so the UI can light up features per agent kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Exact status from hooks/OSC (vs. coarse heuristics).
    pub structured_status: bool,
    /// Can resume a prior session.
    pub resumable: bool,
}

/// The single seam between the daemon and a specific agent CLI.
pub trait AgentAdapter: Send + Sync {
    /// Stable identifier, e.g. `"claude-code"`.
    fn kind(&self) -> &str;

    fn capabilities(&self) -> Capabilities;

    /// How to start (or resume) this CLI inside a worktree.
    fn spawn_spec(&self, ctx: &LaunchContext) -> SpawnSpec;

    /// One-time integration setup in the worktree (e.g. write `.claude/settings.json` hooks).
    /// Phase 1: a no-op; the hook mailbox lands in Phase 2.
    fn prepare_worktree(&self, ctx: &LaunchContext) -> Result<()>;
}

/// Adapter for Claude Code. The command is configurable so tests (and users without `claude`
/// on `PATH`) can substitute `$SHELL`/`cat`.
pub struct ClaudeAdapter {
    pub command: Vec<String>,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self {
            command: vec!["claude".to_string()],
        }
    }
}

impl ClaudeAdapter {
    pub fn with_command(command: Vec<String>) -> Self {
        Self { command }
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn kind(&self) -> &str {
        "claude-code"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_status: true, // via hooks, wired in Phase 2
            resumable: true,
        }
    }

    fn spawn_spec(&self, ctx: &LaunchContext) -> SpawnSpec {
        let mut command = self.command.clone();
        // `claude --resume <id>` when resuming a known session.
        if let Some(id) = ctx.resume {
            if self.kind() == "claude-code" && command.first().map(String::as_str) == Some("claude")
            {
                command.push("--resume".to_string());
                command.push(id.to_string());
            }
        }
        SpawnSpec {
            command,
            cwd: ctx.worktree.to_path_buf(),
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
        }
    }

    fn prepare_worktree(&self, _ctx: &LaunchContext) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_spec_targets_the_worktree() {
        let adapter = ClaudeAdapter::default();
        let ctx = LaunchContext {
            worktree: Path::new("/tmp/wt/feature-x"),
            branch: "feature/x",
            resume: None,
        };
        let spec = adapter.spawn_spec(&ctx);
        assert_eq!(spec.command, vec!["claude".to_string()]);
        assert_eq!(spec.cwd, PathBuf::from("/tmp/wt/feature-x"));
        assert!(spec.env.iter().any(|(k, _)| k == "TERM"));
    }

    #[test]
    fn resume_appends_the_flag() {
        let adapter = ClaudeAdapter::default();
        let ctx = LaunchContext {
            worktree: Path::new("/tmp/wt/x"),
            branch: "x",
            resume: Some("sess-123"),
        };
        let spec = adapter.spawn_spec(&ctx);
        assert_eq!(
            spec.command,
            vec![
                "claude".to_string(),
                "--resume".to_string(),
                "sess-123".to_string()
            ]
        );
    }

    #[test]
    fn custom_command_is_used_verbatim() {
        let adapter = ClaudeAdapter::with_command(vec!["cat".to_string()]);
        let ctx = LaunchContext {
            worktree: Path::new("/tmp/wt/x"),
            branch: "x",
            resume: Some("ignored"), // only appended for the real `claude` command
        };
        assert_eq!(adapter.spawn_spec(&ctx).command, vec!["cat".to_string()]);
    }
}

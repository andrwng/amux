//! The agent-CLI boundary. Almost nothing in amux is CLI-specific — only *how to launch a CLI*
//! and *how to derive its status*. Both live behind [`AgentAdapter`], so adding `codex`/`gemini`
//! later is one new adapter and nothing else. Status detection is a separate strategy that
//! arrives in Phase 2 (Claude hooks); Phase 1 only needs launch. See `docs/DESIGN.md` §4.3.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where the daemon's hook mailbox lives and how to reach it — injected so the launched CLI's
/// hooks can push status back. Absent in tests / for CLIs without hook integration.
#[derive(Debug, Clone, Copy)]
pub struct HookSetup<'a> {
    /// The mailbox socket `amux hook` connects to.
    pub socket: &'a Path,
    /// The `amux` executable to invoke as the hook command.
    pub amux_exe: &'a Path,
}

/// What an adapter needs to know to launch (or resume) an agent in its working directory.
pub struct LaunchContext<'a> {
    pub worktree: &'a Path,
    /// The agent's branch, or `None` for a branchless HEAD session (runs in the repo root).
    pub branch: Option<&'a str>,
    /// A prior session id to resume, if the adapter supports it.
    pub resume: Option<&'a str>,
    /// The agent's full id (round-trippable), exported so its hooks tag reports with it.
    pub agent_id: &'a str,
    /// Hook mailbox wiring; `None` disables hook integration (tests, hookless CLIs).
    pub hooks: Option<HookSetup<'a>>,
    /// Where to write hook settings when the working directory must not be touched (a HEAD
    /// session's cwd is the user's live repo root). When `Some`, [`AgentAdapter::prepare_worktree`]
    /// writes hooks here instead of into `worktree/.claude`, and the launch command points the CLI
    /// at this file. `None` = the normal per-worktree `.claude/settings.local.json`.
    pub settings_path: Option<&'a Path>,
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
        // Point Claude at an out-of-tree settings file (HEAD sessions) so its hooks fire without
        // writing into the user's live repo. `--settings` merges below managed settings and above
        // the project's `.claude/` files.
        if let Some(path) = ctx.settings_path {
            command.push("--settings".to_string());
            command.push(path.to_string_lossy().into_owned());
        }
        // TERM/COLORTERM are owned by the daemon's PTY layer (it advertises a screen-family
        // terminal so apps fill backgrounds), so they're deliberately not set here.
        let mut env: Vec<(String, String)> = Vec::new();
        // Export the mailbox wiring so the CLI's hooks (see `prepare_worktree`) can reach the
        // daemon and tag their reports with this agent.
        if let Some(hooks) = ctx.hooks {
            env.push((
                "AMUX_HOOK_SOCK".to_string(),
                hooks.socket.to_string_lossy().into_owned(),
            ));
            env.push(("AMUX_AGENT_ID".to_string(), ctx.agent_id.to_string()));
        }
        SpawnSpec {
            command,
            cwd: ctx.worktree.to_path_buf(),
            env,
        }
    }

    /// Write Claude Code hook settings into the worktree so status flows back to the daemon.
    /// Uses `.claude/settings.local.json` (per-worktree, gitignored) so it never touches the
    /// repo's committed config, and is a no-op when hook integration is disabled.
    fn prepare_worktree(&self, ctx: &LaunchContext) -> Result<()> {
        let Some(hooks) = ctx.hooks else {
            return Ok(());
        };
        let settings = claude_hook_settings(hooks.amux_exe);
        let json = serde_json::to_string_pretty(&settings).context("serialize hook settings")?;
        match ctx.settings_path {
            // HEAD session: write out of tree and leave the live repo untouched.
            Some(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).context("create settings dir")?;
                }
                std::fs::write(path, json).context("write external hook settings")?;
            }
            // Worktree session: the per-worktree, gitignored settings file.
            None => {
                let dir = ctx.worktree.join(".claude");
                std::fs::create_dir_all(&dir).context("create .claude dir")?;
                std::fs::write(dir.join("settings.local.json"), json)
                    .context("write hook settings")?;
            }
        }
        Ok(())
    }
}

/// The events whose hooks feed the state machine: attention/idle, finish, and activity.
const HOOK_EVENTS: [&str; 5] = [
    "Notification",
    "Stop",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
];

/// Build the `.claude/settings.local.json` value that runs `<amux_exe> hook` on each event we
/// care about (matcher `""` = all occurrences).
fn claude_hook_settings(amux_exe: &Path) -> serde_json::Value {
    let command = format!("{:?} hook", amux_exe.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for event in HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            serde_json::json!([{
                "matcher": "",
                "hooks": [{ "type": "command", "command": command }],
            }]),
        );
    }
    serde_json::json!({ "hooks": hooks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(worktree: &'a Path, branch: &'a str, resume: Option<&'a str>) -> LaunchContext<'a> {
        LaunchContext {
            worktree,
            branch: Some(branch),
            resume,
            agent_id: "agent-1",
            hooks: None,
            settings_path: None,
        }
    }

    #[test]
    fn spawn_spec_targets_the_worktree() {
        let adapter = ClaudeAdapter::default();
        let spec = adapter.spawn_spec(&ctx(Path::new("/tmp/wt/feature-x"), "feature/x", None));
        assert_eq!(spec.command, vec!["claude".to_string()]);
        assert_eq!(spec.cwd, PathBuf::from("/tmp/wt/feature-x"));
        // TERM is set by the daemon's PTY layer, not the adapter.
        assert!(!spec.env.iter().any(|(k, _)| k == "TERM"));
        // No hook wiring without a HookSetup.
        assert!(!spec.env.iter().any(|(k, _)| k == "AMUX_HOOK_SOCK"));
    }

    #[test]
    fn resume_appends_the_flag() {
        let adapter = ClaudeAdapter::default();
        let spec = adapter.spawn_spec(&ctx(Path::new("/tmp/wt/x"), "x", Some("sess-123")));
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
        // resume only appends for the real `claude` command.
        let spec = adapter.spawn_spec(&ctx(Path::new("/tmp/wt/x"), "x", Some("ignored")));
        assert_eq!(spec.command, vec!["cat".to_string()]);
    }

    #[test]
    fn hooks_inject_env_and_write_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path();
        let exe = Path::new("/opt/amux/bin/amux");
        let sock = Path::new("/run/amux/amuxd-hooks.sock");
        let adapter = ClaudeAdapter::default();
        let lc = LaunchContext {
            worktree,
            branch: Some("feat/x"),
            resume: None,
            agent_id: "agent-xyz",
            hooks: Some(HookSetup {
                socket: sock,
                amux_exe: exe,
            }),
            settings_path: None,
        };

        // Env carries the mailbox + agent id.
        let spec = adapter.spawn_spec(&lc);
        let env: std::collections::HashMap<_, _> = spec.env.into_iter().collect();
        assert_eq!(
            env.get("AMUX_HOOK_SOCK").map(String::as_str),
            Some("/run/amux/amuxd-hooks.sock")
        );
        assert_eq!(
            env.get("AMUX_AGENT_ID").map(String::as_str),
            Some("agent-xyz")
        );

        // Settings are written with a hook per event, invoking `<exe> hook`.
        adapter.prepare_worktree(&lc).unwrap();
        let written =
            std::fs::read_to_string(worktree.join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        for event in HOOK_EVENTS {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(cmd.contains("amux") && cmd.ends_with("hook"), "cmd: {cmd}");
        }
    }

    #[test]
    fn head_session_uses_external_settings_and_writes_nothing_into_tree() {
        let tree = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let settings = ext.path().join("head-settings.json");
        let exe = Path::new("/opt/amux/bin/amux");
        let sock = Path::new("/run/amux/amuxd-hooks.sock");
        let adapter = ClaudeAdapter::default();
        let lc = LaunchContext {
            worktree: tree.path(),
            branch: None,
            resume: None,
            agent_id: "agent-head",
            hooks: Some(HookSetup {
                socket: sock,
                amux_exe: exe,
            }),
            settings_path: Some(&settings),
        };

        // Hooks land at the external path, and nothing is written into the (live) tree.
        adapter.prepare_worktree(&lc).unwrap();
        assert!(settings.exists());
        assert!(!tree.path().join(".claude").exists());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        for event in HOOK_EVENTS {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(cmd.contains("amux") && cmd.ends_with("hook"), "cmd: {cmd}");
        }

        // The launch command points Claude at that external settings file.
        let spec = adapter.spawn_spec(&lc);
        let want = settings.to_string_lossy().to_string();
        assert!(
            spec.command
                .windows(2)
                .any(|w| w[0] == "--settings" && w[1] == want),
            "command: {:?}",
            spec.command
        );
    }

    #[test]
    fn prepare_worktree_is_a_noop_without_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let adapter = ClaudeAdapter::default();
        adapter
            .prepare_worktree(&ctx(tmp.path(), "x", None))
            .unwrap();
        assert!(!tmp.path().join(".claude").exists());
    }
}

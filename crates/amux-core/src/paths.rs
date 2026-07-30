//! Path resolution for the amux home and the daemon's unix sockets.
//!
//! Everything amux stores hangs off one **amux home** (`amux_home`): worktrees, the daemon's
//! `state.json`, and the runtime sockets. The home defaults to `~/.amux` but can be relocated
//! via `config.toml`'s `root` (see `crate::config`). A configured `root` makes a fully
//! self-contained instance — its sockets live under `<root>/run`, ignoring `$XDG_RUNTIME_DIR`,
//! so two instances never collide on one socket. With no `root`, socket resolution is unchanged:
//! prefer `$XDG_RUNTIME_DIR/amux` (set natively on Linux, `/run/user/<uid>`), else `~/.amux/run`.
//! Both the daemon (which creates the dir) and the client (which finds the socket) use this.
//! See `docs/DESIGN.md` §4.4, §5.1 and §11 (gotcha 4: ownership/mode checks + `sun_path` limit).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;

/// The amux runtime directory and the well-known paths within it.
pub struct RuntimePaths {
    pub dir: PathBuf,
}

impl RuntimePaths {
    /// Resolve the runtime directory. Does not create it.
    pub fn resolve() -> Result<Self> {
        let cfg = Config::load()?;
        let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let dir = resolve_runtime_dir(cfg.root.as_deref(), xdg.as_deref(), &home()?);
        Ok(Self { dir })
    }

    /// The control socket the client connects to.
    pub fn socket(&self) -> PathBuf {
        self.dir.join("amuxd.sock")
    }

    /// The mailbox socket Claude Code hooks push status to (via `amux hook`).
    pub fn mailbox(&self) -> PathBuf {
        self.dir.join("amuxd-hooks.sock")
    }

    /// The advisory lock file that enforces one daemon per home. Held (via `flock`) for the
    /// daemon's lifetime; the OS releases it on any exit, so it needs no cleanup.
    pub fn lock(&self) -> PathBuf {
        self.dir.join("amuxd.lock")
    }
}

/// The user's home directory. (`home_dir()` is the one `directories` lookup that agrees across
/// macOS and Linux — see `crate::config` for why we don't use its `config_dir()`.)
pub(crate) fn home() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("cannot determine home directory")?
        .home_dir()
        .to_path_buf())
}

/// The amux home directory (`~/.amux` by default, or `config.toml`'s `root`). Worktrees,
/// `state.json`, and the runtime sockets all hang off this. Errors if the config is malformed.
pub fn amux_home() -> Result<PathBuf> {
    Ok(resolve_amux_home(Config::load()?.root.as_deref(), &home()?))
}

/// The daemon's durable state file (`<amux_home>/state.json`). Unlike the runtime dir (which may
/// live under `/run` and be wiped on reboot), this persists agents/repos/minis across restarts.
pub fn state_file() -> Result<PathBuf> {
    Ok(amux_home()?.join("state.json"))
}

/// Where a branchless HEAD session's Claude hook settings live
/// (`<amux_home>/head-settings/<agent-id>.json`) — deliberately outside any repo, so a HEAD
/// session running in the user's live tree never has settings written into that tree.
pub fn head_settings_path(agent_id: &crate::agent::AgentId) -> Result<PathBuf> {
    Ok(amux_home()?
        .join("head-settings")
        .join(format!("{}.json", agent_id.to_full_string())))
}

/// Expand a leading `~` or `~/` against `home`; otherwise return the path unchanged.
fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        return home.to_path_buf();
    }
    match input.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(input),
    }
}

/// Resolve the amux home root: `config_root` (tilde-expanded) if set and non-empty, else
/// `<home>/.amux`. Pure.
fn resolve_amux_home(config_root: Option<&str>, home: &Path) -> PathBuf {
    match config_root.map(str::trim).filter(|s| !s.is_empty()) {
        Some(root) => expand_tilde(root, home),
        None => home.join(".amux"),
    }
}

/// Resolve the runtime (socket) directory. Pure. A configured `root` makes a self-contained
/// instance rooted at `<root>/run`, ignoring `$XDG_RUNTIME_DIR`. Otherwise prefer
/// `$XDG_RUNTIME_DIR/amux`, else `<home>/.amux/run`.
fn resolve_runtime_dir(config_root: Option<&str>, xdg: Option<&str>, home: &Path) -> PathBuf {
    if config_root.map(str::trim).is_some_and(|s| !s.is_empty()) {
        return resolve_amux_home(config_root, home).join("run");
    }
    match xdg.map(str::trim).filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x).join("amux"),
        None => home.join(".amux").join("run"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/u")
    }

    #[test]
    fn expand_tilde_handles_bare_and_prefixed_and_passthrough() {
        assert_eq!(expand_tilde("~", &home()), PathBuf::from("/home/u"));
        assert_eq!(
            expand_tilde("~/xfs2/.amux", &home()),
            PathBuf::from("/home/u/xfs2/.amux")
        );
        assert_eq!(
            expand_tilde("/abs/path", &home()),
            PathBuf::from("/abs/path")
        );
        // A `~` not followed by `/` is not a home reference — leave it be.
        assert_eq!(expand_tilde("~foo", &home()), PathBuf::from("~foo"));
    }

    #[test]
    fn amux_home_defaults_when_no_root() {
        assert_eq!(
            resolve_amux_home(None, &home()),
            PathBuf::from("/home/u/.amux")
        );
        // Whitespace-only root is treated as unset.
        assert_eq!(
            resolve_amux_home(Some("  "), &home()),
            PathBuf::from("/home/u/.amux")
        );
    }

    #[test]
    fn amux_home_uses_configured_root() {
        assert_eq!(
            resolve_amux_home(Some("~/xfs2/.amux"), &home()),
            PathBuf::from("/home/u/xfs2/.amux")
        );
        assert_eq!(
            resolve_amux_home(Some("/data/amux"), &home()),
            PathBuf::from("/data/amux")
        );
    }

    #[test]
    fn runtime_dir_configured_root_is_self_contained() {
        // A configured root ignores XDG entirely and lives under <root>/run.
        assert_eq!(
            resolve_runtime_dir(Some("~/xfs2/.amux"), Some("/run/user/1000"), &home()),
            PathBuf::from("/home/u/xfs2/.amux/run")
        );
    }

    #[test]
    fn runtime_dir_default_prefers_xdg_then_home() {
        assert_eq!(
            resolve_runtime_dir(None, Some("/run/user/1000"), &home()),
            PathBuf::from("/run/user/1000/amux")
        );
        assert_eq!(
            resolve_runtime_dir(None, None, &home()),
            PathBuf::from("/home/u/.amux/run")
        );
        // Empty XDG is treated as unset.
        assert_eq!(
            resolve_runtime_dir(None, Some(""), &home()),
            PathBuf::from("/home/u/.amux/run")
        );
    }

    #[test]
    fn lock_path_sits_beside_the_socket() {
        let paths = RuntimePaths {
            dir: std::path::PathBuf::from("/run/amux"),
        };
        assert_eq!(
            paths.lock(),
            std::path::PathBuf::from("/run/amux/amuxd.lock")
        );
        assert_eq!(
            paths.socket(),
            std::path::PathBuf::from("/run/amux/amuxd.sock")
        );
    }
}

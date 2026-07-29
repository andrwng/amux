//! Global user configuration (`~/.config/amux/config.toml`, respecting `$XDG_CONFIG_HOME`).
//! Loaded at startup to resolve the amux home (see `paths::amux_home`); the seed of the config
//! system sketched in `docs/DESIGN.md` §4.4.
//!
//! The path is resolved by hand rather than via `directories`' platform config dir, which is
//! `~/Library/Application Support` on macOS — that would put the config in a different place on
//! each of our two first-class platforms, and would silently ignore `$XDG_CONFIG_HOME` there.
//! amux is a Unix CLI: one XDG path, identical on macOS and Linux.
//!
//! A missing file is normal — amux runs on built-in defaults (`~/.amux`). A file that fails to
//! parse (or has an unknown key) is a **hard error**, never a silent fallback: otherwise you'd
//! believe your data moved to `root` while amux quietly kept using the default.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The TUI's border/accent color profile. Selects which color scheme the client's chrome
/// (focused borders, shell-pane borders, selection accents) uses — so concurrent amux sessions
/// are visually distinguishable. `Blue` reproduces the original look. Kept ratatui-free here so
/// `amux-core` stays pure; the name→`Color` mapping lives in `amux-tui`'s `theme` module.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    #[default]
    Blue,
    Green,
    Yellow,
    Red,
}

/// A `--profile`/`profile=` value that names no known profile.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseProfileError(pub String);

impl std::fmt::Display for ParseProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown profile {:?} (valid: blue, green, yellow, red)",
            self.0
        )
    }
}

impl std::error::Error for ParseProfileError {}

impl std::str::FromStr for Profile {
    type Err = ParseProfileError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "blue" => Ok(Profile::Blue),
            "green" => Ok(Profile::Green),
            "yellow" => Ok(Profile::Yellow),
            "red" => Ok(Profile::Red),
            other => Err(ParseProfileError(other.to_string())),
        }
    }
}

/// The global amux config. Everything is optional; an empty/missing file means "all defaults".
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The amux home root — where worktrees, `state.json`, and the runtime sockets live. May
    /// start with `~`. When unset, defaults to `~/.amux`. Example: `root = "~/xfs2/.amux"`.
    #[serde(default)]
    pub root: Option<String>,
    /// The TUI border/accent color profile. Defaults to `blue` (the original look).
    /// Overridden by the `--profile` CLI flag. Example: `profile = "green"`.
    #[serde(default)]
    pub profile: Profile,
}

impl Config {
    /// Load the config from `~/.config/amux/config.toml`. Missing file → defaults; malformed
    /// file → error.
    pub fn load() -> Result<Self> {
        match config_path() {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read config file {}", path.display()))?;
                Self::from_toml(&text)
                    .with_context(|| format!("parse config file {}", path.display()))
            }
            _ => Ok(Self::default()),
        }
    }

    /// Parse config from TOML text. Pure — no filesystem.
    fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("invalid TOML")
    }
}

/// `$XDG_CONFIG_HOME/amux/config.toml`, else `~/.config/amux/config.toml` — the same path on
/// macOS and Linux. `None` if the home dir is unknown.
pub fn config_path() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    Some(resolve_config_path(
        xdg.as_deref(),
        &crate::paths::home().ok()?,
    ))
}

/// Resolve the config file path: `$XDG_CONFIG_HOME` if set and non-empty, else `<home>/.config`,
/// then `amux/config.toml`. Pure.
fn resolve_config_path(xdg_config_home: Option<&str>, home: &Path) -> PathBuf {
    match xdg_config_home.map(str::trim).filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => home.join(".config"),
    }
    .join("amux")
    .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_all_defaults() {
        assert_eq!(
            Config::from_toml("").unwrap(),
            Config {
                root: None,
                profile: Profile::Blue
            }
        );
    }

    #[test]
    fn parses_root() {
        let cfg = Config::from_toml(r#"root = "~/xfs2/.amux""#).unwrap();
        assert_eq!(cfg.root.as_deref(), Some("~/xfs2/.amux"));
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(
            Config::from_toml("root = ").is_err(),
            "syntactically broken TOML must error"
        );
    }

    #[test]
    fn config_path_prefers_xdg_then_dot_config() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            resolve_config_path(Some("/xdg"), &home),
            PathBuf::from("/xdg/amux/config.toml")
        );
        assert_eq!(
            resolve_config_path(None, &home),
            PathBuf::from("/home/u/.config/amux/config.toml")
        );
        // Empty/whitespace XDG is treated as unset (matching `paths::resolve_runtime_dir`).
        assert_eq!(
            resolve_config_path(Some("  "), &home),
            PathBuf::from("/home/u/.config/amux/config.toml")
        );
    }

    #[test]
    fn unknown_key_is_an_error() {
        // A typo like `roott` must be loud, not silently ignored (which would leave the user
        // thinking their home moved when it didn't).
        assert!(
            Config::from_toml(r#"roott = "~/xfs2/.amux""#).is_err(),
            "an unknown key must error"
        );
    }

    #[test]
    fn profile_parses_from_str() {
        use std::str::FromStr;
        assert_eq!(Profile::from_str("blue"), Ok(Profile::Blue));
        assert_eq!(Profile::from_str("green"), Ok(Profile::Green));
        assert_eq!(Profile::from_str("yellow"), Ok(Profile::Yellow));
        assert_eq!(Profile::from_str("red"), Ok(Profile::Red));
        assert!(Profile::from_str("purple").is_err());
    }

    #[test]
    fn config_parses_profile_and_defaults_to_blue() {
        assert_eq!(
            Config::from_toml("profile = \"green\"").unwrap().profile,
            Profile::Green
        );
        assert_eq!(Config::from_toml("").unwrap().profile, Profile::Blue);
        assert!(Config::from_toml("profile = \"purple\"").is_err());
    }
}

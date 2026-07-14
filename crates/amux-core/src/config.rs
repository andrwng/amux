//! Global user configuration (`~/.config/amux/config.toml`, respecting `$XDG_CONFIG_HOME`).
//! Loaded at startup to resolve the amux home (see `paths::amux_home`); the seed of the config
//! system sketched in `docs/DESIGN.md` §4.4.
//!
//! A missing file is normal — amux runs on built-in defaults (`~/.amux`). A file that fails to
//! parse (or has an unknown key) is a **hard error**, never a silent fallback: otherwise you'd
//! believe your data moved to `root` while amux quietly kept using the default.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The global amux config. Everything is optional; an empty/missing file means "all defaults".
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The amux home root — where worktrees, `state.json`, and the runtime sockets live. May
    /// start with `~`. When unset, defaults to `~/.amux`. Example: `root = "~/xfs2/.amux"`.
    #[serde(default)]
    pub root: Option<String>,
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

/// `~/.config/amux/config.toml` (respects `$XDG_CONFIG_HOME`). `None` if the home dir is unknown.
pub fn config_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("amux").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_all_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config { root: None });
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
    fn unknown_key_is_an_error() {
        // A typo like `roott` must be loud, not silently ignored (which would leave the user
        // thinking their home moved when it didn't).
        assert!(
            Config::from_toml(r#"roott = "~/xfs2/.amux""#).is_err(),
            "an unknown key must error"
        );
    }
}

//! Runtime path resolution for the daemon's unix sockets. Prefer `$XDG_RUNTIME_DIR` (set
//! natively on Linux, `/run/user/<uid>`), else fall back to `~/.amux/run`. Both the daemon
//! (which creates the dir) and the client (which finds the socket) use this. See
//! `docs/DESIGN.md` §5.1 and §11 (gotcha 4: ownership/mode checks + `sun_path` limit).

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The amux runtime directory and the well-known paths within it.
pub struct RuntimePaths {
    pub dir: PathBuf,
}

impl RuntimePaths {
    /// Resolve the runtime directory. Does not create it.
    pub fn resolve() -> Result<Self> {
        let dir = match std::env::var("XDG_RUNTIME_DIR") {
            Ok(x) if !x.trim().is_empty() => PathBuf::from(x).join("amux"),
            _ => fallback_dir()?,
        };
        Ok(Self { dir })
    }

    /// The control socket the client connects to.
    pub fn socket(&self) -> PathBuf {
        self.dir.join("amuxd.sock")
    }
}

fn fallback_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("cannot determine home directory")?;
    Ok(base.home_dir().join(".amux").join("run"))
}

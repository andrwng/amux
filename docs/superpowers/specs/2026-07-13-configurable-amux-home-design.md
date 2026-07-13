# Configurable amux home via `~/.config/amux/config.toml`

**Date:** 2026-07-13
**Status:** Approved design, pending implementation plan

## Problem

The amux home directory is hardcoded to `~/.amux`. Every path amux uses —
worktree checkouts, the daemon's `state.json`, and the daemon's runtime
sockets — is independently derived from `$HOME/.amux`. There is no way to
relocate this root (e.g. onto a different filesystem such as `~/xfs2/.amux`).

We want the entire amux home to be relocatable via a config file.

## Current state

amux is a Rust workspace. `~/.amux` is recomputed independently in three
places:

- `crates/amux-core/src/worktree.rs` — `global_base()` (~line 249) builds
  `home/.amux/worktrees/<repo>-<hash>/`.
- `crates/amux-core/src/paths.rs` — `state_file()` (~line 43) builds
  `home/.amux/state.json`.
- `crates/amux-core/src/paths.rs` — `fallback_dir()` (~line 36) builds
  `home/.amux/run`, used by `RuntimePaths::resolve()` when
  `$XDG_RUNTIME_DIR` is unset.

There is **no config system** today (no config file, no env var, no CLI flag
for this). `crates/amux-core/src/lib.rs` declares only `adapter, agent, clock,
hook, nav, paths, worktree`. A config system is *designed but unbuilt* —
`docs/DESIGN.md` §4.4 sketches global `~/.amux/config.toml` + per-repo
`.amux/project.toml`. No TOML parser is currently a dependency (only `serde` +
`serde_json`).

The daemon resolves these paths in-process, so any mechanism only takes effect
for a daemon that reads it at startup; an already-running daemon keeps its old
paths.

## Decisions

Settled during brainstorming:

1. **Scope:** the *entire* amux home moves (worktrees + `state.json` +
   `run/` sockets), not just worktrees. `root` becomes a fully self-contained
   amux instance.
2. **Mechanism:** a TOML config file. **Config-file only** — no environment
   variable override.
3. **Config location:** `~/.config/amux/config.toml` (XDG standard, respects
   `$XDG_CONFIG_HOME`). Fixed and independent of the `root` it configures, so
   it is never circular — a config file that *defines* the home cannot live
   *inside* that home.
4. **Precedence:** `config.toml` `root` → default `~/.amux`.

## Design

### 1. New `config` module — `crates/amux-core/src/config.rs`

- Add the `toml` crate to the workspace and `amux-core` `Cargo.toml` (`serde`
  is already present). Register `config` in `crates/amux-core/src/lib.rs`.
- Minimal struct, with room to grow into the DESIGN §4.4 config system:

  ```rust
  #[derive(Debug, Default, Deserialize)]
  pub struct Config {
      #[serde(default)]
      pub root: Option<String>,   // e.g. "~/xfs2/.amux"
  }
  ```

- **Fixed config path** (independent of `root`): `BaseDirs::config_dir()`
  joined with `amux/config.toml` → `~/.config/amux/config.toml`, respecting
  `$XDG_CONFIG_HOME`.
- `Config::load() -> Result<Config>`:
  - Missing file → `Config::default()` (today's behavior; no config required).
  - Malformed file → **hard error**, surfaced at startup. Never a silent
    fallback to `~/.amux`, which would leave the user believing their data is
    under `root` while amux quietly used the default.

### 2. One resolution point — `crates/amux-core/src/paths.rs`

A pure, testable core plus a cached wrapper:

```rust
fn resolve_amux_home(config_root: Option<&str>, home: &Path) -> PathBuf {
    match config_root.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => expand_tilde(v, home),   // ~/xfs2/.amux -> /home/awong/xfs2/.amux
        None => home.join(".amux"),          // unchanged default
    }
}

pub fn amux_home() -> &'static Path { /* OnceLock, populated from Config::load() */ }
```

- `expand_tilde` expands a leading `~`/`~/` against the home dir (covers the
  case where the value was not shell-expanded). A small shared helper; the TUI
  already has equivalent `~` expansion in `crates/amux-tui/src/app.rs`
  (`expand_path`) that can be lifted or mirrored.
- Config is validated **loudly once at startup** — daemon init, CLI entry, and
  `doctor` call `Config::load()` and abort with a clear message on a parse
  error. The pervasive `amux_home()` calls then read the cached value on the
  hot path. By the time they run, the config is known-good or genuinely absent.

### 3. Route the three current hardcodings through `amux_home()`

- `worktree.rs::global_base()` → `amux_home().join("worktrees").join(key)`
- `paths.rs::state_file()` → `amux_home().join("state.json")`
- `paths.rs::fallback_dir()` → `amux_home().join("run")`

### 4. Runtime socket dir

`RuntimePaths::resolve()` currently prefers `$XDG_RUNTIME_DIR/amux`, which
would **not** move with `root` — so a `~/xfs2/.amux` daemon and a `~/.amux`
daemon would collide on the same socket. Rule:

- **`root` present in config** → socket dir = `amux_home()/run`, ignoring
  `$XDG_RUNTIME_DIR` (fully self-contained instance).
- **No `root`** → current behavior exactly (`$XDG_RUNTIME_DIR/amux`, else
  `~/.amux/run`).

This makes a configured `root` a genuinely separate instance: its own daemon,
state, sockets, and worktrees.

### 5. `doctor` reporting

`amux doctor` reports the resolved amux home, the config file path, and the
config parse status, so the user can verify where amux thinks its home is.

## Consequences (out of scope, called out)

- **Separate instance / no migration.** Pointing `root` at a fresh directory
  yields an empty amux; existing `~/.amux` worktrees and state are not copied.
  Auto-migration is out of scope (YAGNI).
- **Daemon (re)start required.** After adding `root`, a fresh daemon must start
  to pick it up. The old `~/.amux` daemon keeps running independently until
  stopped.
- **Per-repo `project.toml`** (DESIGN §4.4) stays out of scope — `root` is
  inherently global.

## Testing

Unit-test the pure functions and config loading, with explicit inputs and no
process-env or filesystem-global mutation (so no test-ordering flakiness):

- `resolve_amux_home` — with/without `root`, whitespace/empty `root`.
- `expand_tilde` — `~`, `~/x`, absolute, relative passthrough.
- The runtime-dir rule — `root` present vs absent, `$XDG_RUNTIME_DIR` set vs
  unset.
- `Config::load` — valid file, missing file (→ default), malformed file
  (→ error).

## Insertion points (reference)

- `crates/amux-core/src/config.rs` — new module.
- `crates/amux-core/src/lib.rs` — register `config`.
- `crates/amux-core/src/paths.rs` — `amux_home()`, `resolve_amux_home`,
  `expand_tilde`; reroute `state_file()`, `fallback_dir()`, and the
  runtime-dir rule in `RuntimePaths::resolve()`.
- `crates/amux-core/src/worktree.rs` — reroute `global_base()`.
- `crates/amux-core/Cargo.toml` + workspace `Cargo.toml` — add `toml`.
- `src/main.rs` (`doctor`) and daemon startup — call `Config::load()` for
  loud early validation.

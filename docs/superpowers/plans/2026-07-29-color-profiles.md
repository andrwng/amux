# Border/accent color profiles — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add named color profiles (`blue` default, `green`, `yellow`, `red`) that recolor the TUI's focus accent + shell-pane border, selectable via `--profile` CLI flag and a `profile` config key, so concurrent amux sessions are visually distinguishable.

**Architecture:** A dependency-free `Profile` enum + `Config.profile` field in `amux-core` (no ratatui — core stays pure). A new `theme` module in `amux-tui` maps `Profile → Theme { focus, shell }` (ratatui `Color`s); `App` holds a `Theme`, and the inline accent literals in `app.rs` render functions read it. The binary parses `--profile`, `run()` computes the effective profile (CLI > config > default) and builds the theme. No daemon involvement, no wire change.

**Tech Stack:** Rust workspace — `amux-core` (config), `amux-tui` (ratatui rendering), root binary (`clap`). Tests: `cargo test`.

## Global Constraints

- **`amux-core` stays pure — no `ratatui`, and no NEW dependency.** `Profile` uses only `serde` + `std`. Its parse error is a hand-rolled `std::error::Error` type (core has no `thiserror`). Colors (`ratatui::Color`) live only in `amux-tui`.
- **The `blue` profile reproduces today's look exactly:** focus `Color::Cyan`, shell `Color::Blue`. Default behavior is byte-for-byte unchanged.
- **Exact profile colors:**
  | Profile | focus | shell |
  |---|---|---|
  | `Blue` | `Color::Cyan` | `Color::Blue` |
  | `Green` | `Color::LightGreen` | `Color::Green` |
  | `Yellow` | `Color::LightYellow` | `Color::Rgb(180, 140, 0)` |
  | `Red` | `Color::LightRed` | `Color::Red` |
- **Precedence:** CLI `--profile` > config `profile` > default `Blue`, expressed as `cli.unwrap_or(config.profile)` (config defaults to `Blue`).
- **Do NOT change `color_for` (`app.rs:2202`, `AgentState::Starting => Color::Cyan`)** or any other state-driven color — those are semantic, not chrome.
- **No wire change / no `PROTO_VERSION` bump** (client-only rendering).
- **`tracing` only** (no `println!`/`eprintln!`/`dbg!` in library crates); **no `unwrap()`/`expect()` in library code** (tests may unwrap); typed errors at public boundaries.
- **Definition of done (all green, observed):** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace`.
- **Commit trailer on every commit:** `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. One logical change per commit. Every task's commit compiles the whole workspace (no `--no-verify` needed).

## File Structure

- `crates/amux-core/src/config.rs` — `Profile` enum, `ParseProfileError`, `FromStr`, `Config.profile` field, tests.
- `crates/amux-tui/src/theme.rs` (new) — `Theme { focus, shell }`, `Theme::for_profile`, `Default`, tests.
- `crates/amux-tui/src/lib.rs` — declare `mod theme;`, change `run()` signature.
- `crates/amux-tui/src/app.rs` — `App.theme` field, replace accent literals, `run(profile)` + `effective_profile` helper, tests.
- `src/main.rs` — `--profile` arg, `parse_profile`, dispatch.
- `Cargo.toml` (root) — add `amux-core` dependency.
- `README.md` — document `--profile` and the `profile` config key.

---

### Task 1: `Profile` enum + `Config.profile` (amux-core)

**Files:**
- Modify: `crates/amux-core/src/config.rs` (struct at lines 20-27; tests module after line ~71)

**Interfaces:**
- Produces: `amux_core::config::Profile` — `enum { Blue (default), Green, Yellow, Red }`, `Copy`, `serde::Deserialize` (lowercase), `FromStr` (`Err = ParseProfileError`). `amux_core::config::ParseProfileError` (impls `Display` + `Error`). `Config.profile: Profile`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `crates/amux-core/src/config.rs` (it already has `use super::*;`):

```rust
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
        assert_eq!(Config::from_toml("profile = \"green\"").unwrap().profile, Profile::Green);
        assert_eq!(Config::from_toml("").unwrap().profile, Profile::Blue);
        assert!(Config::from_toml("profile = \"purple\"").is_err());
    }
```

- [ ] **Step 2: Run tests to verify they fail (compile error)**

Run: `cargo test -p amux-core config`
Expected: FAIL — `cannot find type/value Profile`, `no field profile on Config`.

- [ ] **Step 3: Add the `Profile` enum + error + `FromStr`**

In `crates/amux-core/src/config.rs`, after the imports (after line 17, `use serde::Deserialize;`):

```rust
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
        write!(f, "unknown profile {:?} (valid: blue, green, yellow, red)", self.0)
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
```

- [ ] **Step 4: Add the `profile` field to `Config`**

In the `Config` struct (after the `root` field, before the closing brace at line 26):

```rust
    /// The TUI border/accent color profile. Defaults to `blue` (the original look).
    /// Overridden by the `--profile` CLI flag. Example: `profile = "green"`.
    #[serde(default)]
    pub profile: Profile,
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p amux-core config`
Expected: PASS (including the existing `empty_toml_is_all_defaults`, `unknown_key_is_an_error`, etc. — `profile` defaults to `Blue`, so an empty config still equals `Config::default()`).

- [ ] **Step 6: Commit**

```bash
git add crates/amux-core/src/config.rs
git commit -m "Add a Profile enum and config.profile for TUI color schemes"
```

---

### Task 2: `theme` module + apply to render sites (amux-tui)

**Files:**
- Create: `crates/amux-tui/src/theme.rs`
- Modify: `crates/amux-tui/src/lib.rs` (add `mod theme;` beside the other `mod` lines, ~line 5-9)
- Modify: `crates/amux-tui/src/app.rs` (`App` struct field; `App::new`; render-site literals; test module)

**Interfaces:**
- Consumes: `amux_core::config::Profile` (Task 1).
- Produces: `crate::theme::Theme { focus: Color, shell: Color }`, `Theme::for_profile(Profile) -> Theme`, `impl Default for Theme` (= `Blue`). `App.theme: Theme`.

This task is behavior-neutral: `App::new` defaults the theme to `Blue`, whose colors are the current literals, so the running app looks identical. It only introduces the indirection + tests.

- [ ] **Step 1: Write the failing theme test**

Create `crates/amux-tui/src/theme.rs` with the test first (implementation added in Step 3, but write the whole file in Step 3 — here, add just the test to make the intent concrete). To keep TDD honest, create the file now containing ONLY:

```rust
//! Placeholder — replaced in Step 3.
```

then add `mod theme;` to `crates/amux-tui/src/lib.rs` (next to `mod pane;`), and add this test target by creating the real file in Step 3. Proceed to Step 2 to observe the failure.

*(Rationale: `theme.rs`'s test lives in the same file as the code; there is no separate test file. The failing state is "module/type does not exist yet," observed in Step 2.)*

- [ ] **Step 2: Confirm the type does not exist yet**

Run: `cargo test -p amux-tui theme 2>&1 | head -20`
Expected: FAIL — no `Theme` type / empty module, or (once `App.theme` is referenced) a compile error. This confirms we are adding something new.

- [ ] **Step 3: Write `theme.rs` (implementation + test)**

Replace the entire contents of `crates/amux-tui/src/theme.rs` with:

```rust
//! Border/accent color scheme for the client chrome. Maps a pure `amux_core::config::Profile`
//! to the ratatui `Color`s the render functions use, so concurrent sessions can wear distinct
//! colors. `focus` replaces the former `Color::Cyan` accent (focused borders, selection,
//! markers, status chips); `shell` replaces the former `Color::Blue` secondary-pane border.
//! Agent *state* colors (`color_for`) are semantic and unaffected.

use amux_core::config::Profile;
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub focus: Color,
    pub shell: Color,
}

impl Theme {
    pub fn for_profile(profile: Profile) -> Self {
        match profile {
            Profile::Blue => Theme { focus: Color::Cyan, shell: Color::Blue },
            Profile::Green => Theme { focus: Color::LightGreen, shell: Color::Green },
            Profile::Yellow => Theme { focus: Color::LightYellow, shell: Color::Rgb(180, 140, 0) },
            Profile::Red => Theme { focus: Color::LightRed, shell: Color::Red },
        }
    }
}

impl Default for Theme {
    /// The original scheme — cyan focus, blue shell.
    fn default() -> Self {
        Theme::for_profile(Profile::Blue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_map_to_expected_colors() {
        assert_eq!(
            Theme::for_profile(Profile::Blue),
            Theme { focus: Color::Cyan, shell: Color::Blue }
        );
        assert_eq!(
            Theme::for_profile(Profile::Green),
            Theme { focus: Color::LightGreen, shell: Color::Green }
        );
        assert_eq!(
            Theme::for_profile(Profile::Yellow),
            Theme { focus: Color::LightYellow, shell: Color::Rgb(180, 140, 0) }
        );
        assert_eq!(
            Theme::for_profile(Profile::Red),
            Theme { focus: Color::LightRed, shell: Color::Red }
        );
        assert_eq!(Theme::default(), Theme::for_profile(Profile::Blue));
    }
}
```

- [ ] **Step 4: Add the `theme` field to `App` and initialize it**

In `crates/amux-tui/src/app.rs`, add an import near the other `use crate::...;` lines:

```rust
use crate::theme::Theme;
```

Add a field to the `App` struct (place it right after the `focus: Focus,` field, ~line 337):

```rust
    /// The active border/accent color scheme (from `--profile` / config; default `blue`).
    theme: Theme,
```

In `App::new` (the struct literal, after `focus: Focus::Sidebar,` ~line 390):

```rust
            theme: Theme::default(),
```

- [ ] **Step 5: Replace the accent literals with the theme**

In `crates/amux-tui/src/app.rs`, in the render functions ONLY (`render_minis`, `render_sidebar`, `render_panes`, `render_status` — all take `app: &App`), replace:
- every `Color::Cyan` with `app.theme.focus`
- every `Color::Blue` with `app.theme.shell`

There are exactly these sites (locate by context; line numbers may drift): `Color::Cyan` at 2283, 2303, 2336, 2393, 2404, 2412, 2447 (`.bg(...)`), 2456, 2460, 2573, 2624, 2642, 2650, 2676; `Color::Blue` at 2557.

**DO NOT touch line ~2202 (`AgentState::Starting => Color::Cyan` inside `color_for`)** — it is semantic, and `color_for` is a free function with no `app` in scope. After editing, verify none remain:

Run: `grep -n "Color::Cyan\|Color::Blue" crates/amux-tui/src/app.rs`
Expected: exactly one line — the `color_for` `Starting => Color::Cyan`.

- [ ] **Step 6: Add a render test proving the theme reaches a render site**

Add to `app.rs`'s `#[cfg(test)] mod tests` (it already imports `Terminal`, `TestBackend`, `Color` via the other render tests — if not in scope, add `use ratatui::style::Color;` and the `TestBackend`/`Terminal` imports the neighboring tests use):

```rust
    /// The chrome accent follows the active profile: with a non-default profile, the focused
    /// sidebar border is drawn in that profile's focus color (not the default cyan). Focus
    /// defaults to the sidebar, so `render_sidebar` uses the focus accent for its border.
    #[test]
    fn sidebar_border_uses_the_profile_focus_color() {
        use amux_core::config::Profile;
        let mut app = App::new(24, 6);
        app.theme = Theme::for_profile(Profile::Green);
        let mut term = Terminal::new(TestBackend::new(24, 6)).unwrap();
        term.draw(|f| render_sidebar(f, f.area(), &app)).unwrap();
        let fg = term.backend().buffer().cell((0, 0)).unwrap().style().fg;
        assert_eq!(
            fg,
            Some(Color::LightGreen),
            "the focused sidebar border should use the profile's focus color"
        );
    }
```

If `.cell((0, 0)).unwrap().style().fg` does not compile against this ratatui version, use the equivalent accessor the neighboring render tests use to read a cell (the goal is: assert the top-left border cell's foreground is `Color::LightGreen`). The test must fail if Step 5's replacement at the sidebar border (line ~2336) were reverted to `Color::Cyan`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p amux-tui theme sidebar_border_uses_the_profile_focus_color profiles_map_to_expected_colors`
Then the whole crate to confirm no render test regressed (the default theme is still cyan/blue):
`cargo test -p amux-tui`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/amux-tui/src/theme.rs crates/amux-tui/src/lib.rs crates/amux-tui/src/app.rs
git commit -m "Introduce a Theme and route chrome accents through it"
```

---

### Task 3: Wire `--profile` and config selection (binary + TUI entry)

**Files:**
- Modify: `crates/amux-tui/src/app.rs` (`run` signature + `effective_profile` helper + set `app.theme`; test)
- Modify: `crates/amux-tui/src/lib.rs` (`run` signature)
- Modify: `src/main.rs` (`--profile` arg, `parse_profile`, dispatch)
- Modify: `Cargo.toml` (root — add `amux-core` dep)
- Modify: `README.md` (Command line + Configuration)

**Interfaces:**
- Consumes: `amux_core::config::{Config, Profile, ParseProfileError}` (Task 1); `crate::theme::Theme` (Task 2).
- Produces: `amux_tui::run(profile: Option<Profile>) -> Result<()>`; `app::effective_profile(cli: Option<Profile>, config: Profile) -> Profile`.

- [ ] **Step 1: Write the failing `effective_profile` test**

Add to `app.rs`'s test module:

```rust
    #[test]
    fn effective_profile_prefers_cli_then_config() {
        use amux_core::config::Profile;
        assert_eq!(effective_profile(Some(Profile::Red), Profile::Green), Profile::Red);
        assert_eq!(effective_profile(None, Profile::Green), Profile::Green);
        assert_eq!(effective_profile(None, Profile::Blue), Profile::Blue);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p amux-tui effective_profile_prefers_cli_then_config`
Expected: FAIL — `cannot find function effective_profile`.

- [ ] **Step 3: Add the helper and use it in `run`**

In `crates/amux-tui/src/app.rs`, add the helper near `run` (above `pub async fn run`):

```rust
/// The profile actually used: the `--profile` flag if given, else the config's `profile`
/// (which itself defaults to `Blue`). Encodes the precedence CLI > config > default.
fn effective_profile(
    cli: Option<amux_core::config::Profile>,
    config: amux_core::config::Profile,
) -> amux_core::config::Profile {
    cli.unwrap_or(config)
}
```

Change the `run` signature and set the theme. Replace the current `pub async fn run() -> Result<()> {` (line 157) with:

```rust
pub async fn run(profile: Option<amux_core::config::Profile>) -> Result<()> {
```

Immediately after `let mut app = App::new(cols, rows);` (line 180), add:

```rust
    let config = amux_core::config::Config::load()?;
    app.theme = Theme::for_profile(effective_profile(profile, config.profile));
```

- [ ] **Step 4: Update the `lib.rs` entry point**

In `crates/amux-tui/src/lib.rs`, change `pub fn run() -> Result<()>` (line 15) to:

```rust
pub fn run(profile: Option<amux_core::config::Profile>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(app::run(profile))
}
```

- [ ] **Step 5: Add the `amux-core` dependency to the root binary**

In the root `Cargo.toml` `[dependencies]`, add (keep alphabetical — before `amux-daemon`):

```toml
amux-core = { workspace = true }
```

- [ ] **Step 6: Add the `--profile` flag and pass it through**

In `src/main.rs`, add a field to the `Cli` struct (after the `command` field, ~line 13):

```rust
    /// Border/accent color profile for the TUI: blue (default), green, yellow, or red.
    #[arg(long, value_parser = parse_profile)]
    profile: Option<amux_core::config::Profile>,
```

Add a parser helper (top-level fn, e.g. after the `Command` enum):

```rust
fn parse_profile(s: &str) -> Result<amux_core::config::Profile, String> {
    s.parse().map_err(|e: amux_core::config::ParseProfileError| e.to_string())
}
```

Change the TUI dispatch arm (line 48) from `None => amux_tui::run()?,` to:

```rust
        None => amux_tui::run(cli.profile)?,
```

- [ ] **Step 7: Run tests + build to verify**

Run: `cargo test -p amux-tui effective_profile_prefers_cli_then_config`
Then: `cargo build --workspace --all-targets`
Then confirm the flag is wired: `cargo run -- --help 2>&1 | grep -i profile` (expect a `--profile` line). Also confirm a bad value errors: `cargo run -- --profile purple 2>&1 | grep -i "unknown profile"` (clap surfaces the `FromStr` error).
Expected: test PASS, build OK, `--profile` shown in help, bad value rejected with the "unknown profile" message.

*(Note: do not launch the actual TUI here — `cargo run` with a valid profile connects to the live daemon. `--help` and the invalid-value path exit before that.)*

- [ ] **Step 8: Update the README**

In `README.md`:
- In the **Command line** section, add a line for the default TUI invocation's flag, e.g.:
  `amux --profile <blue|green|yellow|red>` — start the TUI with a border/accent color profile (default `blue`); use it to tell concurrent sessions apart.
- In the **Configuration** section, document the key:
  `profile` — border/accent color scheme: `blue` (default), `green`, `yellow`, or `red`. The `--profile` flag overrides it.

Match the existing wording/altitude of those sections (read them first; keep it to a line or two each, consistent with the neighbors).

- [ ] **Step 9: Commit**

```bash
git add crates/amux-tui/src/app.rs crates/amux-tui/src/lib.rs src/main.rs Cargo.toml README.md
git commit -m "Select the color profile from --profile and config"
```

---

### Task 4: Full Definition-of-Done gate

**Files:** none (verification only).

- [ ] **Step 1: Format** — `cargo fmt --all -- --check` (expect clean).
- [ ] **Step 2: Clippy** — `cargo clippy --workspace --all-targets -- -D warnings` (expect no warnings).
- [ ] **Step 3: Build** — `cargo build --workspace --all-targets` (expect success).
- [ ] **Step 4: Test** — `cargo test --workspace` (expect all pass, including `config` profile tests, `theme` tests, `sidebar_border_uses_the_profile_focus_color`, `effective_profile_prefers_cli_then_config`).
- [ ] **Step 5: Manual confirmation (runtime changed — required by CLAUDE.md).** Per project memory, do NOT `cargo run` amux from an agent worktree (hits the live daemon). For the user / a throwaway `AMUX_HOME`:
  1. `amux` (no flag) — borders look exactly as before (cyan focus, blue shell).
  2. `amux --profile green` — focused borders/sidebar/selection/status chips are green, shell panes darker green.
  3. Set `profile = "red"` in `~/.config/amux/config.toml`, launch `amux` — red scheme; then `amux --profile yellow` — yellow wins (CLI overrides config).

---

## Self-Review

**Spec coverage:**
- `Profile` enum (Blue/Green/Yellow/Red, default Blue), core-pure, `FromStr` → Task 1. ✓
- `Config.profile` with `serde(default)` → Task 1. ✓
- `Theme`/`theme.rs` mapping the exact color table; `blue` == current → Task 2. ✓
- Accent literals routed through theme; `color_for` untouched → Task 2 (Step 5 + explicit exclusion). ✓
- `--profile` CLI flag + validation → Task 3 (Steps 6-7). ✓
- Precedence CLI > config > Blue → Task 3 (`effective_profile`, Step 3 + test). ✓
- README (`--profile` + `profile` key) → Task 3 (Step 8). ✓
- No wire change / core purity / no new core dep → Global Constraints + Task 1 (hand-rolled error). ✓
- Tests: core from_str + config parse (Task 1), theme mapping + render-site (Task 2), precedence (Task 3). ✓

**Placeholder scan:** none — all code is literal. Task 2 Step 1/2 intentionally stage a "type doesn't exist" failure before Step 3 writes the real file; this is a TDD sequencing note, not a placeholder in delivered code.

**Type consistency:** `Profile` (core), `Theme`/`Theme::for_profile`/`Theme::default` (tui), `effective_profile(Option<Profile>, Profile) -> Profile`, `run(Option<Profile>)`, `parse_profile(&str) -> Result<Profile, String>`, `ParseProfileError` — names and signatures match across Tasks 1-3. `app.theme.focus`/`app.theme.shell` are the field paths used at every render site.

**Every task compiles the whole workspace:** Task 1 adds a defaulted field + new types (no consumer breaks). Task 2 defaults the theme to Blue and changes no signatures (behavior-neutral). Task 3 changes `run()` and updates its only two callers (`lib.rs`, `main.rs`) in the same commit. No `--no-verify` needed anywhere.

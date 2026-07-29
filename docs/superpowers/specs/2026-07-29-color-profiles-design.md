# Border/accent color profiles

**Status:** approved (design)
**Date:** 2026-07-29

## Problem

A user runs several amux TUIs at once on different terminals (e.g. one local, one
remote) and wants to tell them apart at a glance. Today the border/accent color scheme
is fixed — a cyan focus accent plus a darker-blue shell-pane border, hardcoded as inline
`ratatui::Color` literals throughout `crates/amux-tui/src/app.rs`. There is no theme
module and no way to change the scheme.

The user wants named color *profiles* selectable per session, so each TUI can wear a
distinct scheme.

## Concept

A **profile** is a named pair of colors applied to the client's chrome:

- **focus** — the accent color, used everywhere cyan is used today: focused pane / sidebar
  / mini borders, the sidebar selected-row highlight, row markers, and status-bar hint
  chips.
- **shell** — the secondary color, used for shell/secondary-pane borders (today's blue).

Agent *state* colors (`color_for`: working=green, needs-attention=yellow, error=red,
starting=cyan, idle=gray, exited=dark-gray) are **semantic, not chrome, and are left
untouched.** A profile only recolors the chrome accent + shell border. A green profile
frame will therefore coexist with a green "working" state border; this is expected — the
always-on frame is the session-identity signal, the state color is content.

### Built-in profiles

| Profile | focus (accent) | shell (secondary pane) |
|---|---|---|
| `blue` (default) | `Color::Cyan` | `Color::Blue` — **unchanged from today** |
| `green` | `Color::LightGreen` | `Color::Green` |
| `yellow` | `Color::LightYellow` | `Color::Rgb(180, 140, 0)` (amber) |
| `red` | `Color::LightRed` | `Color::Red` |

The `blue` profile reproduces the current look exactly (the same `Cyan`/`Blue` literals),
so the default behavior is byte-for-byte unchanged.

## Architecture

The split preserves the `amux-core` purity invariant (no `ratatui` in core).

- **`amux-core`** (`src/config.rs`): a new `Profile` enum — `Blue` | `Green` | `Yellow` |
  `Red`, `#[default] Blue` — deriving `serde::Deserialize` and implementing `FromStr`
  (typed error). **No `ratatui` dependency.** `Config` gains:
  ```rust
  #[serde(default)]
  pub profile: Profile,
  ```
  `serde(default)` so existing `config.toml` files without the key still load as `Blue`.
  `deny_unknown_fields` stays; an unknown *value* (`profile = "purple"`) is a
  deserialize error, consistent with the existing fail-fast config philosophy.

- **`amux-tui`** (`src/theme.rs`, new): a `Theme { focus: Color, shell: Color }` struct and
  `Theme::for_profile(Profile) -> Theme` mapping the table above. This is the only place
  that maps a `Profile` to `ratatui::Color`s. `App` gains a `theme: Theme` field, set in
  `App::new`. The ~20 inline `Color::Cyan` accent literals and the `Color::Blue`
  shell literals in `app.rs` render functions (`render_panes`, `render_sidebar`,
  `render_minis`, `render_status`) are replaced with `self.theme.focus` / `self.theme.shell`.
  `color_for(&AgentState)` is **not** changed.

- **No daemon involvement, no wire message, no `PROTO_VERSION` bump.** Border color is a
  purely client-side rendering concern; the daemon neither knows nor cares about it.

## Selection and precedence

- **CLI:** `amux --profile <name>` — a root-level flag (the no-subcommand path already
  launches the TUI). Parsed to `Profile` via `FromStr`; an invalid name produces a clear
  error naming the valid set. (`main.rs` owns clap; core stays clap-free — the arg is a
  string validated through `Profile::from_str`, or a clap `value_parser` that calls it.)
- **Config:** `profile = "green"` in `~/.config/amux/config.toml`.
- **Precedence:** CLI flag > config file value > built-in default (`Blue`).

### Flow

`amux_tui::run()` (today no args) takes the CLI-selected profile (`Option<Profile>`),
loads `Config`, and computes the effective profile as `cli_profile.unwrap_or(config.profile)`.
Because `config.profile` is itself `Blue` by default (via `serde(default)`), this single
expression yields the full precedence chain CLI > config > `Blue` with no special-casing.
It then builds `Theme::for_profile(effective)` and passes it into `App::new` so the render
functions read it. `lib.rs::run` and `main.rs`'s dispatch (`None => run(...)`) change
signature accordingly.

## Testing

- **`amux-core`:** a `Profile` `FromStr` round-trip test (valid names parse, unknown
  errors) and a `Config` parse test (`profile = "green"` → `Profile::Green`; missing key →
  `Blue`; bad value → error), added beside the existing `config.rs` tests.
- **`amux-tui`:** a `theme` unit test asserting each `Profile` maps to the expected
  `focus`/`shell` `Color`s (table above), and a `TestBackend` render test that renders a
  focused pane border under a non-default profile and asserts the border cell's
  `.style().fg` equals that profile's `focus` color — proving the theme actually reaches
  the render site. (Style-cell assertions are new but `TestBackend` supports them.)

## Documentation

The README gains: the `--profile` flag in the "Command line" section, and the `profile`
key (with the four valid values and the default) in the "Configuration" section — same
commit as the code, per the repo's README-rides-the-change rule.

## Non-goals

- **User-defined / custom-hex palettes.** YAGNI — four built-in named profiles cover the
  stated need. The `Profile` enum can grow later, or gain a custom variant, without
  reworking the flow.
- **Per-repo profile merge.** There is no per-repo config layer today; the profile is a
  single global (or CLI-overridden) choice, matching how config works now.
- **Theming the semantic state colors** (`color_for`). Out of scope and intentionally so.

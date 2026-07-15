# amux

A terminal UI for multiplexing AI coding agents in isolated git worktrees — a persistent
sidebar of agents, one focused main window, and several floating live mini-terminals you can
answer without losing your place.

- **Design & architecture:** [`docs/DESIGN.md`](docs/DESIGN.md)
- **Phase 0 build plan:** [`docs/PHASE-0.md`](docs/PHASE-0.md)

Status: **pre-alpha**, Phase 0 (the spine). Unix only (macOS + Linux/Ubuntu).

## Quickstart

```sh
cargo run   # the TUI — auto-spawns the daemon on first run
```

### The three kinds of panels

```
┌ agents ────────┐┌ ⋯ feature-x ──────────────────────────────┐
│ ▾ myrepo (3)   ││                                            │
│ ▸▌⋯ *feature-x ││   main area — the active agent's workspace │
│   ⚠ feature-y  ││   (tiled panes: agent + shell splits)      │
│   ○ feature-z  ││                                            │
│                ││            ┌ ⚠ feature-y ─┐┌ ○ feature-z ─┐│
│                ││            │  mini        ││  mini        ││
│                ││            └──────────────┘└──────────────┘│
└────────────────┘└────────────────────────────────────────────┘
 status bar — always shows the shortcuts for whatever is focused
```

- **Sidebar** (left): every agent, grouped by repo, needs-attention first. Each row shows a
  state glyph (colored by state), a cyan `▌` bar when the agent has unread output, and a `*`
  when it's currently visible. This is a selection list — keys here are *commands*.
- **Main area** (center): the **active agent's workspace** — a tmux-style tree of tiled panes.
  It starts as the agent's terminal; splits spawn a `$SHELL` in the same worktree. Each agent
  owns its own layout: opening another agent swaps the whole main area, and layouts are saved
  and restored (even across TUI restarts).
- **Minis** (floating, bottom-right): small live terminals overlaid on the main area, one per
  agent, each showing that agent's primary terminal — answer another agent's prompt without
  losing your place. A mini can be minimized to a status-only strip, and the whole row can be
  hidden temporarily ("peek").

**The input model in one sentence:** in the sidebar keys are commands; in a pane or mini every
keystroke is typed into that terminal, except three reserved chords — `Ctrl+Q` (quit),
`Ctrl+h/j/k/l` (move focus), and `Ctrl+B` (the tmux-style command prefix).

### Navigation

Focus is **spatial**: picture the layout above and move with `Ctrl+h/j/k/l`. Between panes it
moves through the splits; off the left edge it lands in the sidebar; off the bottom or right
edge it drops into the minis row; from a mini, left/right walk the row and up/left climb back
into the panes (or the sidebar when there are none). Clicking a pane or mini also focuses it.

Two deliberate exceptions:

- In the **sidebar**, `Ctrl+j`/`Ctrl+k` jump to the next/previous **unread** agent (there is
  nothing above or below the sidebar to move into).
- A vim-like app that announces it handles splits gets `Ctrl+h/j/k/l` passed through; when it
  hits its own edge it hands navigation back to amux (via `amux nav`), so one motion works
  across both.

### Shortcuts

Everywhere: `Ctrl+Q` quit · `Ctrl+h/j/k/l` move focus · `Ctrl+B` command prefix.

**Sidebar**

| Key | Action |
| --- | --- |
| `j`/`k` or arrows | move the selection |
| `Enter` or `l` | open the agent in the main area |
| `m` | open the agent as a mini |
| `n` | new agent in the selected repo (prompts for a branch) |
| `N` | new agent in a repo by path (prompts for directory + branch; `Tab` switches fields) |
| `d` | delete the agent (asks `y/n` before discarding uncommitted work) |
| `r` | resume an exited agent |
| `P` | doctor: prune the repo's orphaned worktrees |
| `Ctrl+j`/`Ctrl+k` | jump to next/previous unread agent |
| `q` | quit |

**Panes and minis — `Ctrl+B`, then:**

| Key | Action |
| --- | --- |
| `%` / `"` | split left‑right / top‑bottom (spawns `$SHELL` in the same worktree) |
| `x` | close the focused pane or mini (a shell pane's shell is killed; the agent's own terminal just hides — the agent keeps running) |
| `r` or `H`/`J`/`K`/`L` | resize mode: `hjkl`/arrows nudge the split, `Esc`/`Enter` done |
| `[` | scroll (copy) mode — see below |
| `Tab` | jump to the next agent with unread output and open it |
| `Ctrl+<key>` | send a literal control key to the focused pane (e.g. `Ctrl+B Ctrl+L`, since bare `Ctrl+h/j/k/l` navigate) |
| `Enter` | *(mini only)* promote the mini into the main area |
| `-` | *(mini only)* minimize/restore it to a status strip |
| `z` | peek: hide/show the whole minis row |

**Scroll mode** (`Ctrl+B [`, vi keys like tmux): `j`/`k` line, `Ctrl+U`/`Ctrl+D` half page,
`PgUp`/`PgDn` page, `g`/`G` top/bottom, `q`/`Esc`/`Enter` back to live. Unavailable in panes
running a full-screen app (there's no scrollback on the alternate screen).

**Mouse:** click focuses; the wheel scrolls amux's scrollback, or is forwarded to apps that
take the mouse (vim, less, Claude); drag to select and copy from a single pane (via OSC 52,
works over SSH); hold `Shift` to bypass amux and use your terminal's native selection.

## Try the Phase 0.1 spike

Proves the PTY↔render loop — a live `$SHELL` inside a ratatui frame:

```sh
cargo run --example spike   # quit with Ctrl-Q
```

On Ubuntu you need a C toolchain first: `sudo apt-get install -y build-essential pkg-config`.

## Development

Enable the shared git hooks once per clone so your commits stay CI-green:

```sh
./.githooks/setup   # sets core.hooksPath to .githooks
```

The `pre-commit` hook runs `cargo fmt --check` and `cargo clippy -D warnings` — the
fast half of [CI](.github/workflows/ci.yml). Bypass it for a single commit with
`git commit --no-verify`.

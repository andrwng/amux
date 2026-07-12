# Tiled Splits — Plan

**Goal:** make the main area a tmux-style split space — tiled panes, each streaming a
**terminal**, navigated with tmux muscle memory. First-class, coexisting with the floating minis
(§7.5 of `docs/DESIGN.md`). Independent of Phase 2 (hooks); can land before or after it.

**The terminal model (proto v4).** The sidebar lists **agents** (durable workspaces: worktree +
branch). Panes stream **terminals** (PTYs). An agent owns a *primary terminal* (its CLI) plus any
*shell terminals* split off in the same worktree. A split is not "an empty slot for another
agent" — it opens the default `$SHELL` in the same worktree, a sibling terminal, and it lives
only as long as its pane. `Output` / `Input` / `Resize` / `Attach` / `Detach` are all keyed by
`TerminalId`.

---

## S.1 — Daemon: multiple simultaneous attachments  ✅

Replace the single re-targetable stream with a set of attachments. Client subscribes to N
terminals; the daemon runs one output forwarder per attached terminal, tagging
`Output { terminal, .. }`.

- Proto v4: Attach/Detach/Input/Resize keyed by `TerminalId`; `SpawnShell { terminal, like }`
  opens a `$SHELL` in `like`'s worktree; `CloseTerminal { terminal }` kills a shell (no-op on a
  primary). `AgentInfo` carries `primary_terminal`.
- `handle_command`: `attached: HashMap<TerminalId, JoinHandle>`. `Attach` spawns a forwarder +
  snapshot if not already attached; `Detach` aborts it.
- **DoD (done):** integration tests — attach two primary terminals on one connection and assert
  both echo independently, tagged by id; `SpawnShell` yields an attachable shell in the same
  worktree; force-delete kills every terminal + removes the worktree.

## S.2 — Client: pane tree  ✅

A binary space-partition tree generic over its payload, instantiated as `PaneTree<TerminalId>`:
- Leaf = a pane holding a `TerminalId`; the client keeps `parsers: HashMap<TerminalId, Parser>`
  and `terminals: HashMap<TerminalId, AgentId>` alongside it.
- Internal node = `Split { axis: H|V, ratio, first, second }`; focus is a path to the focused
  leaf.
- A pure `layout(area) -> Vec<Placement<TerminalId>>` drives rendering; each leaf's parser is fed
  by that terminal's `Output`. `reconcile` diffs the layout against what's attached and sends
  `Attach`/`Resize` for newly-visible terminals; a terminal that leaves the layout is `Detach`ed
  (primary — the agent keeps running) or `CloseTerminal`d (shell — killed).
- **DoD (done):** pure unit tests for split / close / navigate on the tree; render tests of the
  2- and 3-pane layouts.

## S.3 — Navigation, resize, and the keymap

**Focus is spatial.** The whole screen — the sidebar plus every tiled pane — is one grid, and
focus is exactly one cell. There is no separate Nav/Terminal *mode*: what a keystroke does is
determined by *where focus is* (sidebar = commands; pane = agent input) plus a few global keys.

### Movement — direct `Ctrl+hjkl` (no prefix), sidebar included
- `Ctrl+h/j/k/l` move focus **directionally** to the neighboring cell. The sidebar sits to the
  left of all panes, so `Ctrl+h` from a left-edge pane lands in the sidebar, and `Ctrl+l` from
  the sidebar enters the nearest pane. Geometry-based (pick the adjacent cell whose rect is
  closest in that direction and overlaps on the perpendicular axis) — like vim-tmux-navigator.
- **Collision (accepted):** intercepting `Ctrl+hjkl` means the focused agent doesn't receive
  those control codes (`Ctrl+L` clear, `Ctrl+K` kill-line, `Ctrl+J` LF, `Ctrl+H` backspace-ish).
  Right default for a multi-agent tool; escape hatch below. Nav keys are keymap-configurable.

### Structure — the `Ctrl+B` prefix (less-frequent commands)
- `Ctrl+B %` split focused pane left/right · `Ctrl+B "` split top/bottom. **A split opens a new
  `$SHELL` terminal in the same worktree** (`SpawnShell`), not a slot for a different agent.
- `Ctrl+B x` close focused pane. Primary terminal → `Detach` (the agent keeps running);
  shell terminal → `CloseTerminal` (the shell process is killed, ending with its pane).
- `Ctrl+B r` enter **resize mode** (below)
- `Ctrl+B <any Ctrl-key>` → send that literal control key to the focused terminal (the escape
  hatch, e.g. `Ctrl+B Ctrl+L` clears its screen; `Ctrl+B Ctrl+B` sends a literal `Ctrl+B`)

### Resize — a small `hjkl` submode
`Ctrl+B r` enters resize mode (status bar: `RESIZE — hjkl grow/shrink · esc done`). Then, on the
focused pane, `l`/`h` widen/narrow and `j`/`k` grow/shrink height, each press one step (~5%),
by adjusting the ratio of the nearest ancestor split of the matching axis (clamped 0.1–0.9).
`Esc`/`Enter` exits. Repeatable in-mode so you can nudge quickly — the `hjkl` you like.

### Sidebar cell (when focused)
`j`/`k` select · `n` new · `d` delete · `r` resume · `Enter`/`l` open the selected agent into the
most-recently-focused pane (or the first pane if the layout is empty). Opening an agent attaches
its **primary terminal**. `Ctrl+l` moves into panes.

**DoD:** pure unit tests for directional nav, split, close, and resize on the pane tree; the
prefix + resize state machines tested; keys route to the right pane; interactive check.

## S.4 — Persistence hook (small)

The pane tree + focus is workspace layout — it rides the same `~/.amux/state.json` /
`SetLayout` mechanism as the rest (deferred with the other persistence work). Not required for
a first splits release.

---

**Order:** S.1 (daemon) → S.2 (tree) → S.3 (keymap) landed as one coordinated change on the
terminal model (proto v4), green across the workspace. Minis (Phase 3) layer on top later.

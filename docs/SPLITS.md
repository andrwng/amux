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
- **vim-aware passthrough (proto v9):** when the focused pane runs a vim-like app, `Ctrl+hjkl` is
  passed *through* to it so it can move its own splits, then handed back at the edge — full
  vim-tmux-navigator parity. The `contrib/amux.vim` plugin is the single integration point: on
  enter/leave it runs `amux passthrough on/off` (daemon → `TerminalApp`, client caches it and
  routes `Ctrl+hjkl` to the program); at vim's edge it runs `amux nav <dir>` (daemon relays
  `Navigate` → client moves focus). Layout/focus stay entirely client-side; the daemon only
  reports the app kind and relays the nav intent. `AMUX_TERMINAL_ID` + the mailbox socket are
  injected into every terminal so the plugin can identify its pane.

### Structure — the `Ctrl+B` prefix (less-frequent commands)
- `Ctrl+B %` split focused pane left/right · `Ctrl+B "` split top/bottom. **A split opens a new
  `$SHELL` terminal in the same worktree** (`SpawnShell`), not a slot for a different agent.
- `Ctrl+B x` close focused pane. Primary terminal → `Detach` (the agent keeps running);
  shell terminal → `CloseTerminal` (the shell process is killed, ending with its pane).
- `Ctrl+B r` enter **resize mode** (below)
- `Ctrl+B <any Ctrl-key>` → send that literal control key to the focused terminal (the escape
  hatch, e.g. `Ctrl+B Ctrl+L` clears its screen; `Ctrl+B Ctrl+B` sends a literal `Ctrl+B`)

### Resize — `Ctrl+B H/J/K/L` (tmux muscle memory) + a submode
`Ctrl+B` then **capital `H/J/K/L`** resizes the focused pane one step (~5%) and stays in resize
mode so you can keep nudging without re-prefixing — matching tmux's `bind -r H/J/K/L resize-pane`.
`Ctrl+B r` enters the same mode without an initial move. In-mode, both `hjkl` and `HJKL` (and
arrows) resize by adjusting the nearest ancestor split's ratio (clamped 0.1–0.9); `Esc`/`Enter`
exits.

### Per-agent workspaces
Each **agent owns its own tiled layout** — the main area shows exactly one agent's workspace at a
time. Opening an agent from the sidebar **swaps** the whole main area to that agent's tree
(created with its primary terminal the first time, restored thereafter); splitting adds a sibling
shell **to that agent's** workspace. Switching agents **detaches** the previous agent's terminals
(they keep running headless in the daemon and restore on return) — it never mixes two agents in
one layout. The only place different agents appear at once is the **minis** (below), each showing just that
agent's primary (Claude) terminal.

### Minis (Phase 3, proto v10–v11)
A docked row of small live terminals **below** the main panes — the bottom row of the same grid.
`m` in the sidebar opens the selected agent as a mini (an agent is never both in the main area and
a mini). Spatial `Ctrl+hjkl` flows in/out (`Ctrl+j` from the bottom pane → minis, `Ctrl+k` back,
`Ctrl+h/l` across, `Ctrl+h` off the left → sidebar); a focused mini takes keystrokes to its primary
terminal. `Ctrl+B`: `Enter` promote→main · `-` minimize (status-only strip, terminal detached) ·
`z` peek (hide the whole row) · `x` close. Which agents are minis persists across TUI restarts
(`SetMinis`/`Minis`, same mechanism as the layout).

### Sidebar cell (when focused)
`j`/`k` select · `n` new · `d` delete · `r` resume · `Enter`/`l` open the selected agent into the
most-recently-focused pane (or the first pane if the layout is empty). Opening an agent attaches
its **primary terminal**. `N` new agent in a repo by path · `P` doctor (prune the selected repo's
orphaned worktrees). `Ctrl+l` moves into panes.

**DoD:** pure unit tests for directional nav, split, close, and resize on the pane tree; the
prefix + resize state machines tested; keys route to the right pane; interactive check.

## S.4 — Layout persistence (DONE, proto v10)

Each agent's pane tree is persisted so splits survive closing the TUI. The client serializes the
active agent's tree to a `Layout` (leaves = terminal ids, splits = axis + ratio) and sends
`SetLayout { agent, layout }` on every layout change (via `reconcile`); the daemon holds the
layouts and replays them to a re-attaching client (`DaemonMsg::Layouts` on connect). On first
open of an agent, the client rebuilds its tree from the saved layout and re-attaches the shells
(which kept running headless in the daemon). This covers **client** restarts. `Axis`/`Dir` are
shared via `amux_core::nav`.

## S.5 — Daemon-restart persistence (DONE, `~/.amux/state.json`)

The daemon writes its **durable** state — repos (as repo+base paths), agents (id, repo, branch,
worktree, `ai_session_id`, timestamps, unread, **primary terminal id**), and the minis list — to
`~/.amux/state.json` on every meaningful change (create/delete/repo-add/set-minis/first
session-id capture) via an atomic temp+rename, plus a final flush on shutdown. On startup it
`load_state()`s: repos re-register (rebuilt via `WorktreeService::with_base`, so ids match) and
agents come back **suspended** (`Exited`, no live session) with a dormant primary terminal.

Live processes (PTYs) die with the daemon and are **not** persisted; instead a suspended agent
is revived **lazily** — when a client attaches its primary and finds no session, the daemon
`resume`s it (reusing the same primary id + `ai_session_id`, so Claude continues the same
conversation). Only *visible* agents (the active one + minis) resume on reconnect; the rest stay
suspended in the sidebar until opened. Pane **layouts don't survive a daemon restart** (their
shell processes are gone) — an agent reopens with a single primary pane. No proto change: this is
entirely server-side, transparent over the existing `Attach`/`StateChanged` wire.

---

**Order:** S.1 (daemon) → S.2 (tree) → S.3 (keymap) landed as one coordinated change on the
terminal model (proto v4), green across the workspace. Minis (Phase 3) layer on top later.

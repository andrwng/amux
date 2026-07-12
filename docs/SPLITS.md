# Tiled Splits — Plan

**Goal:** make the main area a tmux-style split space — tiled panes, each streaming an agent,
navigated with tmux muscle memory. First-class, coexisting with the floating minis (§7.5 of
`docs/DESIGN.md`). Independent of Phase 2 (hooks); can land before or after it.

The protocol was built for this: `Output` / `Input` / `Resize` are already per-`AgentId`. The
two real changes are the daemon streaming *several* agents at once, and the client holding a
*pane tree* instead of a single `main`.

---

## S.1 — Daemon: multiple simultaneous attachments

Replace the single re-targetable stream with a set of attachments. Client subscribes to N
agents; the daemon runs one output forwarder per attached agent, tagging `Output { id, .. }`.

- Proto v3: add `ClientMsg::Detach { id }` (Attach already adds a stream; Detach removes it).
- `handle_client`: `attached: HashMap<AgentId, JoinHandle>` instead of `Option<(id, handle)>`.
  `Attach{id}` spawns a forwarder + snapshot if not already attached; `Detach{id}` aborts it.
- **DoD:** integration test — attach two `cat` agents on one connection, feed each, assert both
  streams echo independently, tagged by id.

## S.2 — Client: pane tree

Replace `attached: Option<AgentId>` + the single parser with a binary space-partition tree:
- Leaf = a pane: `{ agent: AgentId, parser: vt100::Parser }`.
- Internal node = `{ dir: H|V, ratio, left, right }`.
- `focus`: a path/id to the focused leaf.
- A pure `layout(tree, area) -> Vec<(leaf, Rect)>` (recursive) drives rendering; each leaf's
  parser is fed by that agent's `Output`. On any layout change, each visible pane sends
  `Resize { id, pane_size }` (resize-to-slot, per pane).
- **DoD:** pure unit tests for split / close / navigate on the tree; `TestBackend` snapshot of a
  2- and 3-pane layout.

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
- `Ctrl+B %` split focused pane left/right · `Ctrl+B "` split top/bottom
- `Ctrl+B x` close focused pane (agent keeps running; `Detach`)
- `Ctrl+B r` enter **resize mode** (below)
- `Ctrl+B <any Ctrl-key>` → send that literal control key to the agent (the escape hatch, e.g.
  `Ctrl+B Ctrl+L` clears the agent's screen; `Ctrl+B Ctrl+B` sends a literal `Ctrl+B`)

### Resize — a small `hjkl` submode
`Ctrl+B r` enters resize mode (status bar: `RESIZE — hjkl grow/shrink · esc done`). Then, on the
focused pane, `l`/`h` widen/narrow and `j`/`k` grow/shrink height, each press one step (~5%),
by adjusting the ratio of the nearest ancestor split of the matching axis (clamped 0.1–0.9).
`Esc`/`Enter` exits. Repeatable in-mode so you can nudge quickly — the `hjkl` you like.

### Sidebar cell (when focused)
`j`/`k` select · `n` new · `d` delete · `r` resume · `Enter`/`l` open the selected agent into the
most-recently-focused pane (or the first pane if the layout is empty). `Ctrl+l` moves into panes.

**DoD:** pure unit tests for directional nav, split, close, and resize on the pane tree; the
prefix + resize state machines tested; keys route to the right pane; interactive check.

## S.4 — Persistence hook (small)

The pane tree + focus is workspace layout — it rides the same `~/.amux/state.json` /
`SetLayout` mechanism as the rest (deferred with the other persistence work). Not required for
a first splits release.

---

**Order:** S.1 (daemon) → S.2 (tree) → S.3 (keymap) as one coordinated change (like the
multi-agent transition), green when the workspace builds. Minis (Phase 3) layer on top later.

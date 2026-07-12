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

## S.3 — tmux keymap (the `Ctrl-B` prefix)

Rework `Ctrl-B` from a Nav/Terminal toggle into a real **tmux-style prefix**. In Terminal mode,
`Ctrl-B` enters "prefix pending"; the next key is a command:
- `%` split focused pane vertically · `"` split horizontally
- `h`/`j`/`k`/`l` or arrows — move focus between panes
- `x` close the focused pane (agent keeps running; `Detach`)
- `o` / Enter (from the sidebar) open the selected agent into the focused pane
- `Ctrl-B` again → send a literal `Ctrl-B` to the agent (tmux behavior)
- anything unmapped → back to Terminal input
- **DoD:** prefix state machine unit-tested; keys route to the right pane; interactive check.

## S.4 — Persistence hook (small)

The pane tree + focus is workspace layout — it rides the same `~/.amux/state.json` /
`SetLayout` mechanism as the rest (deferred with the other persistence work). Not required for
a first splits release.

---

**Order:** S.1 (daemon) → S.2 (tree) → S.3 (keymap) as one coordinated change (like the
multi-agent transition), green when the workspace builds. Minis (Phase 3) layer on top later.

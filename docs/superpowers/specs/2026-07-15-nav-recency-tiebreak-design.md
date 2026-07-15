# Last-focused tie-break for pane navigation

**Date:** 2026-07-15
**Status:** approved (design), not yet implemented
**Owner:** `crates/amux-tui/src/pane.rs` (`PaneTree`)

## Problem

`PaneTree::navigate` picks the destination pane by minimizing `(gap, center-alignment)`:
the nearest candidate in the travel direction, tie-broken by how close its center is to the
current pane's center on the perpendicular axis. With a full-width top pane over several
bottom panes, every bottom pane ties on gap and "down" always lands in the horizontally
centered one — regardless of which bottom pane the user was just working in. Round trips
(down, up, down) don't return where the user came from, which contradicts spatial muscle
memory (tmux users expect a return hop to be an undo).

## Decision

Add **focus recency** as the middle tie-break. The candidate ordering becomes lexicographic
min over:

1. **gap** — distance along the travel direction (unchanged; nearer always wins),
2. **recency** — most recently *focused* candidate first,
3. **center alignment** — perpendicular center distance (unchanged; the fallback when
   candidates share a recency stamp, e.g. never-focused panes in a freshly restored layout).

"Focused" means focused by any means — `Ctrl+hjkl` navigation, a mouse click, entering from
the sidebar, or opening a new split. This was chosen over a strict "return to the pane you
exited" memory because it also helps when the current pane was reached by mouse, and it
needs only one stamp per pane instead of per-edge bookkeeping.

## Mechanism

All changes stay inside `PaneTree`:

- `clock: u64` — monotonic counter, bumped on every focus change.
- `last_focus: HashMap<PaneId, u64>` — stamp per leaf. `PaneId`s are never reused
  (`next_id` is monotonic), so stale entries are impossible; entries are removed on close
  anyway to keep the map tidy.
- A single private `set_focus(&mut self, id: PaneId)` helper assigns `self.focus` and
  stamps `id`. Every code path that currently assigns `self.focus` goes through it:
  `navigate`, `focus_payload` (mouse click), `focus_first` (sidebar entry), `open`
  (new pane / split), `close` (focus moves to a neighbor), and `from_layout` (the restored
  focused leaf gets the first stamp).
- `dist(...)` grows a recency component or `navigate` sorts by
  `(gap, Reverse(last_focus), perp)` — implementation's choice; behavior is the ordering
  above.

## Non-goals / explicitly out of scope

- **No persistence.** `Layout` (wire type) and its (de)serialization are untouched; recency
  is session state. It still survives switching agents mid-session because each agent's
  workspace is its own `PaneTree` and the stamps travel with it.
- **No wire change.** No `PROTO_VERSION` bump, no daemon involvement, no `app.rs` changes.
- **No per-direction memory.** One stamp per pane; we are not modeling tmux's exact
  behavior, just its "return where I was" feel.
- **No config knob.** The tie-break is unconditional (fewer knobs is a project goal).

## Testing (test-first)

New cases in `pane.rs`'s existing test module, written before the implementation:

1. **Round trip returns** — full-width top pane over three bottom panes; focus the bottom-
   left pane, navigate `Up` then `Down`; focus must be bottom-left again. This fails
   against today's center rule (which picks the middle pane).
2. **Mouse focus counts** — same layout, but focus the bottom-right pane via
   `focus_payload`, then `Up`, then `Down`; must land bottom-right.
3. **No history falls back to center** — same layout restored via `from_layout`, where only
   the restored focused leaf (the top pane) carries a stamp and the bottom row has none;
   `Down` from the top picks the center-aligned (middle) pane, preserving today's behavior.
   (A tree built interactively always has stamps — every `open`/split focuses the new pane —
   so `from_layout` is the only real no-history case.)
4. **Gap beats recency** — a recently focused pane that is strictly farther in the travel
   direction must lose to a nearer, never-focused pane.

Existing navigation tests (`navigate_moves_across_panes_and_exits_left`, etc.) must pass
unchanged.

## Risks

Low. The feature is a pure, session-local ordering change in one file. The main behavioral
risk is surprise when an *old* recency stamp beats center alignment long after the user
forgot the history; accepted deliberately — it matches "go back to where I was working",
and gap still dominates so navigation never leaves the nearest row.

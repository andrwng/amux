# Responsive mini-pane width

**Status:** approved (design)
**Date:** 2026-07-29

## Problem

Mini panes (the floating row of small terminals) have a **hard-coded width of 44
columns** (`MINI_W` inside `mini_rects`, `crates/amux-tui/src/app.rs`). On a small
terminal that's ~most of the main area; on a wide terminal it's a tiny sliver, wasting the
available space. Their height already scales (the band is half the main area, capped at
`MINI_ROWS = 14`), but width does not.

The user wants a mini's width to scale with the screen, never dropping below roughly
today's size.

## Design

Replace the fixed `MINI_W = 44` with a pure helper that scales the full-mini width with the
available width, mirroring the existing `sidebar_width(total_cols)` single-source pattern:

```
const MINI_W_MIN: u16 = 44;   // floor — today's fixed width
const MINI_W_MAX: u16 = 80;   // cap — a classic full-terminal width; a mini stays a peek

fn mini_width(available: u16) -> u16 {
    (available / 2).clamp(MINI_W_MIN, MINI_W_MAX)
}
```

- **50% of the available band width**, clamped to `[44, 80]`.
- `available` is the band width already passed into `mini_rects` as `area.width` (which is
  the main-area width minus the 1-column shadow inset). So width tracks the main area,
  which itself tracks the terminal width minus the sidebar.
- **Count-independent** (per the chosen model): each full mini is sized on its own. The
  minimized width stays `MIN_W = 12` (a status-glyph strip — nothing to scale).

Reference points (main area = total cols − sidebar width):

| Terminal cols | Main area | `mini_width` |
|---|---|---|
| 80 | ~50 | 44 (floor) |
| 130 | ~100 | ~49 |
| 170 | ~140 | ~69 |
| 200+ | ~170+ | 80 (cap) |

**Single source of truth.** `mini_width` is consumed only inside `mini_rects`, which is
already the one function feeding rendering (`render_minis`), hit-testing (`mini_at`), and
the PTY sizing in `reconcile`. So all three agree automatically, and the PTY size
(derived from the rect via `pane_size`, minus 2 for the border) reflows to the new width
for free — no separate change.

**Overflow unchanged.** When several wide minis don't all fit, today's behavior stands:
the group is right-anchored and clipped to the right edge (newest flush bottom-right, older
ones clipped off the left).

**Height unchanged.** It already scales; out of scope.

## Testing

- **`mini_width` unit test** (pure, mirrors `sidebar_width_minimizes_when_narrow`): below
  the floor (`available` small → 44), a mid-range value → `available / 2`, and above the cap
  (large `available` → 80). Exact boundary values chosen in the plan.
- **Update `minis_form_a_navigable_bottom_row`** (`app.rs`, `App::new(100, 40)`): it asserts
  adjacency, right-anchoring, and hit-testing rather than an absolute width, so it should
  still pass (on a 100-col terminal `mini_width` is the 44 floor); adjust only if a hidden
  absolute-width assumption surfaces.

## Non-goals

- Changing mini **height** or the `MINI_ROWS` cap.
- Making width depend on the number of open minis (the "fill the row" model was
  considered and rejected in favor of per-mini proportional).
- Changing the minimized-mini width or the right-anchor/clip overflow behavior.

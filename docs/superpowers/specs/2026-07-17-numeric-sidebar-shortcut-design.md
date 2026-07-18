# Numeric sidebar shortcut — design

## Goal

A fast keyboard jump that moves the sidebar selection to the *N*-th agent, with a
temporary numeric overlay revealed while the trigger is engaged. Must work on macOS
(iTerm2) and on Linux SSH'd in through a Mac, without getting in the way of that SSH path.

## Behavior

- **Numbered rows:** agent rows only; repo headers are skipped. Numbered top-to-bottom in
  the current `sidebar_rows()` order.
- **Digit mapping:** 1st agent → `1`, …, 9th → `9`, 10th → `0`. Agents past the 10th get no
  digit and no shortcut.
- **Action:** *select only* — move focus to the sidebar and set the cursor to that agent
  row. The user then presses `Enter`/`l` to open. Active from any focus (sidebar, pane, mini).
- **Layout-at-selection:** the target is recomputed from `sidebar_rows()` at the instant the
  digit is pressed, so a layout change while the overlay is displayed is honored automatically.

## Triggers (hybrid — same action)

1. **Cmd-hold (primary, best UX).** Holding Cmd reveals the overlay; `Cmd+digit` selects;
   releasing Cmd hides it. Requires the terminal to (a) speak the Kitty keyboard protocol and
   (b) forward Cmd to the application. iTerm2 needs a one-time key-mapping to forward Cmd —
   documented in the implementation notes. Where unsupported, this path is simply inert.
2. **Leader (fallback, universal).** Press `Ctrl-G` → overlay appears and stays; the next
   digit selects; `Esc` or any other key cancels. Pure `Press` events — works on every
   terminal, over SSH, with zero config.

## Overlay rendering

In `render_sidebar`, when the overlay is active, each of the first 10 agent rows renders its
digit in place of the 3-char `" glyph "` status cell (e.g. `" ● "` → `" 5 "`), styled
distinctly (reversed/cyan). Width is unchanged so the sidebar shape does not shift. Applies in
both the full and minimized sidebar layouts. Rows past the 10th keep their glyph. Covered by a
new `insta` snapshot test.

## Input handling & state

Two new `App` fields: `super_held: bool` and `numeric_leader: bool`. Overlay is visible when
either is set.

- At startup, if `supports_keyboard_enhancement()` is true, push
  `REPORT_EVENT_TYPES | REPORT_ALL_KEYS_AS_ESCAPE_CODES` (needed to observe a lone Cmd
  press/release); pop on teardown. If unsupported, the Cmd-hold path is inert and the leader
  still works.
- In `on_key` (the existing global interception point, beside the `Ctrl+B` prefix and
  `Ctrl+hjkl`):
  - `KeyCode::Modifier(LeftSuper|RightSuper)` Press → `super_held = true`; Release → `false`.
    The current `Press`-only event filter is relaxed to let these `Release` events through.
  - `Ctrl-G` → `numeric_leader = true` (show overlay).
  - A digit **only when `SUPER` is in modifiers OR `numeric_leader`** → select the mapped
    agent, clear `numeric_leader`. Plain digits still pass through to the focused PTY — this
    guard is essential (agents type numbers).
  - Leader pending + a non-digit → cancel the overlay.
- The digit↔ordinal mapping is a small pure function with unit tests. The selection lookup
  reuses the existing `sidebar_rows()` machinery.

## Decisions / non-goals

- **Leader is hard-coded `Ctrl-G`**, matching how `Ctrl+B` and `Ctrl+hjkl` are already
  hard-coded. No keymap-config system exists yet; building one now is out of scope (YAGNI). It
  rides along if such a system lands later.
- **Select-only, not open** — a numeric shortcut selects; opening stays an explicit `Enter`/`l`.

## Testing

- Unit test the pure digit↔ordinal mapping (both directions, and the >10 boundary).
- `insta` snapshot of `render_sidebar` with the overlay active (digits over glyphs), full and
  minimized.
- Unit-test the input decision (given `super_held`/`numeric_leader` + a key → action) so plain
  digit passthrough is a regression guard.

## Verification (observed, not asserted)

- Enabling `REPORT_ALL_KEYS_AS_ESCAPE_CODES` changes how the outer terminal encodes all input;
  crossterm normalizes it back to the same semantic `KeyEvent`s. Prove PTY passthrough is
  unaffected by driving a real agent (vim + Claude) through amux with flags on.
- Live-check in iTerm2 that Cmd reaches amux once the forwarding key-map is configured.

# Responsive mini-pane width — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a full mini pane's width scale with the terminal (50% of the available band width, clamped to `[44, 80]`) instead of a fixed 44 columns, so minis use more space on wide screens but never shrink below today's size.

**Architecture:** A pure `mini_width(available) -> u16` helper (mirroring the existing `sidebar_width` single-source pattern) replaces the `MINI_W = 44` literal inside `mini_rects`. Because `mini_rects` is the single source feeding rendering, hit-testing, and PTY sizing, the change propagates everywhere automatically. One file: `crates/amux-tui/src/app.rs`.

**Tech Stack:** Rust, ratatui. Tests: `cargo test`.

## Global Constraints

- **Formula:** `mini_width(available) = (available / 2).clamp(44, 80)` — 50% of the band width, floor `MINI_W_MIN = 44` (today's fixed width), cap `MINI_W_MAX = 80`.
- **Minimized-mini width stays `MIN_W = 12`.** Height, `MINI_ROWS`, and the right-anchor/clip overflow behavior are unchanged (out of scope).
- **Count-independent:** each full mini is sized on its own; do not make width depend on the number of minis.
- `mini_rects` must remain the single source of truth (consumed by `render_minis`, `mini_at`, and `reconcile`) — put the helper where `mini_rects` uses it, don't duplicate the formula.
- `tracing` only; no `println!`/`eprintln!`/`dbg!` in library crates; no `unwrap()`/`expect()` in library code (tests may unwrap).
- Not a user-facing knob/key/command/config — **no README change** required.
- **Definition of done (all green, observed):** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace`.
- **Commit trailer:** `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. One logical change per commit; the commit compiles the whole workspace (plain `git commit`, no `--no-verify`).

---

### Task 1: Responsive `mini_width`

**Files:**
- Modify: `crates/amux-tui/src/app.rs` — add consts + `mini_width` helper (near `sidebar_width`, ~line 54-60); use it in `mini_rects` (~line 456-482); add a unit test (near `sidebar_width_minimizes_when_narrow`, ~line 3738).

**Interfaces:**
- Produces: `fn mini_width(available: u16) -> u16` (module-level, pure), consts `MINI_W_MIN: u16 = 44`, `MINI_W_MAX: u16 = 80`.

- [ ] **Step 1: Write the failing unit test**

Add near `sidebar_width_minimizes_when_narrow` in `app.rs`'s test module:

```rust
    /// A full mini is half the available band width, clamped to a floor of today's fixed
    /// width and a cap that keeps it a peek. Below the floor → 44; mid-range → half; above
    /// the cap → 80.
    #[test]
    fn mini_width_scales_between_floor_and_cap() {
        assert_eq!(mini_width(50), MINI_W_MIN, "narrow → floor (half of 50 = 25 < 44)");
        assert_eq!(mini_width(88), MINI_W_MIN, "exactly at the floor boundary (44)");
        assert_eq!(mini_width(120), 60, "mid-range → half the available width");
        assert_eq!(mini_width(200), MINI_W_MAX, "wide → cap (half of 200 = 100 > 80)");
        assert_eq!(mini_width(160), MINI_W_MAX, "exactly at the cap boundary (80)");
    }
```

- [ ] **Step 2: Run it to verify it fails (compile error)**

Run: `cargo test -p amux-tui mini_width_scales_between_floor_and_cap`
Expected: FAIL — `cannot find function mini_width` / `cannot find value MINI_W_MIN`.

- [ ] **Step 3: Add the consts + helper**

In `crates/amux-tui/src/app.rs`, immediately after `sidebar_width` (after its closing brace at ~line 60), add:

```rust
/// Floor for a full mini's width — today's historical fixed width, so minis never get
/// narrower than before.
const MINI_W_MIN: u16 = 44;
/// Cap for a full mini's width — a classic full-terminal width, so a mini stays a peek even
/// on an ultrawide screen.
const MINI_W_MAX: u16 = 80;

/// A full mini pane's width: half the available band width, clamped to `[MINI_W_MIN,
/// MINI_W_MAX]`. Pure and count-independent — the single source used by `mini_rects` so
/// rendering, hit-testing, and PTY sizing all agree.
fn mini_width(available: u16) -> u16 {
    (available / 2).clamp(MINI_W_MIN, MINI_W_MAX)
}
```

- [ ] **Step 4: Use the helper in `mini_rects`**

In `mini_rects` (~line 456), replace the `const MINI_W: u16 = 44;` line and the per-mini width mapping so the full-mini width comes from `mini_width(area.width)`. The result should read:

```rust
    fn mini_rects(&self, area: Rect) -> Vec<Rect> {
        const MIN_W: u16 = 12;
        let full_w = mini_width(area.width);
        let widths: Vec<u16> = self
            .minis
            .iter()
            .map(|a| {
                if self.minimized.contains(a) {
                    MIN_W
                } else {
                    full_w
                }
            })
            .collect();
        let total: u16 = widths.iter().sum();
        let right = area.x + area.width;
        let mut x = right.saturating_sub(total).max(area.x);
        widths
            .iter()
            .map(|&w| {
                let w = w.min(right.saturating_sub(x)); // clip against the right edge
                let rect = Rect::new(x, area.y, w, area.height);
                x += w;
                rect
            })
            .collect()
    }
```

(Only the `MINI_W` const and the `else` branch change; the anchoring/clip math is untouched.)

- [ ] **Step 5: Run the new test + the existing mini geometry test**

Run: `cargo test -p amux-tui mini_width_scales_between_floor_and_cap minis_form_a_navigable_bottom_row`
Expected: PASS. (On the geometry test's 100-col terminal, `area.width` ≈ 69, so `mini_width` = 44 — the same width as before, so its adjacency/right-anchor assertions are unchanged.)

- [ ] **Step 6: Run the whole crate to confirm no render/layout regression**

Run: `cargo test -p amux-tui`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/amux-tui/src/app.rs
git commit -m "Scale mini pane width with the terminal, floored at the old size"
```

(Commit body: explain 50%-of-band width clamped to [44,80] via a single `mini_width` helper feeding `mini_rects`; PTY size auto-follows the rect. End with the trailer.)

---

### Task 2: Full Definition-of-Done gate

**Files:** none (verification only).

- [ ] **Step 1: Format** — `cargo fmt --all -- --check` (expect clean).
- [ ] **Step 2: Clippy** — `cargo clippy --workspace --all-targets -- -D warnings` (expect no warnings).
- [ ] **Step 3: Build** — `cargo build --workspace --all-targets` (expect success).
- [ ] **Step 4: Test** — `cargo test --workspace` (expect all pass, including `mini_width_scales_between_floor_and_cap`). Note: `pty::tests::scroll_step_serves_history_a_window_at_a_time` is a pre-existing flake unrelated to this branch (touches no `amux-tui` code); if it fails, re-run — it passes in the full suite on retry.
- [ ] **Step 5: Manual confirmation.** Per project memory, do NOT `cargo run` amux from an agent worktree (hits the live daemon). For the user: open two minis on a wide terminal (≳130 cols) → each mini is visibly wider than 44; shrink the terminal → minis clamp back to 44 and no narrower.

---

## Self-Review

**Spec coverage:** `mini_width` = `(available/2).clamp(44,80)` → Task 1 Step 3. ✓ Used in `mini_rects`, single source → Step 4. ✓ Minimized width 12 unchanged → Step 4 (`MIN_W` kept). ✓ Height/overflow untouched → only the width literal changes. ✓ Unit test at floor/mid/cap boundaries → Step 1. ✓ Existing geometry test still green → Step 5. ✓ No README change → Global Constraints. ✓

**Placeholder scan:** none — all code literal.

**Type consistency:** `mini_width(u16) -> u16`, `MINI_W_MIN`/`MINI_W_MAX` used identically in the helper, `mini_rects`, and the test. `full_w` replaces the former `MINI_W` const; `MIN_W = 12` retained.

**Compiles whole workspace:** single-file internal change, no signature changes, no new deps — plain commit, hook passes.

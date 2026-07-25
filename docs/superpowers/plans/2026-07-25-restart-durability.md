# Restart Durability — Implementation Plan

> **Status: implemented** on branch `reinstall-save` (2026-07-25). All six tasks landed; the
> four gates are green and the eviction path was verified end-to-end with real binaries.

> **For agentic workers:** Steps use checkbox (`- [x]`) syntax for tracking. Implement task-by-task,
> test-first (`CLAUDE.md` → "The loop"). Do not skip ahead: Task 1 is a prerequisite for the
> observable behavior in Tasks 3–4.

**Goal:** Reinstalling amux must not cost you your splits or your conversation. Today a reinstall
that bumps `PROTO_VERSION` **orphans** the running daemon — it keeps its PTYs and its `claude`
children forever — and every pane layout, which lives only in daemon memory, is lost.

**Evidence (2026-07-25):** eight `amux daemon` processes were live on the author's machine, the
oldest 13 days old, holding 19 `claude`, 23 `zsh`, 2 `Vim` processes across 55 processes in total.
Only one owned the sockets. The other seven were unreachable and unkillable through amux.

**Architecture:** three independent fixes, none of which touches the wire.

1. **The new daemon evicts the old one** instead of the client orphaning it. `bind_or_detect`
   becomes handshake-aware: when the control socket is already owned, it probes with a real
   `Hello`. A *compatible* daemon means "already running" (today's behavior). An *incompatible* or
   wedged one is SIGTERM'd via its pidfile, awaited, and SIGKILL'd as a backstop — then we bind.
   The client stops blindly unlinking the socket, which is what created the orphan.
2. **Layouts become durable.** `PersistedState` gains `layouts`. On load, leaves whose PTY died with
   the daemon are blanked; the agent's `primary` terminal id is already stable across restart, so
   the primary leaf reattaches as-is.
3. **Agents get a chance to flush.** `shutdown_all` sends SIGTERM and waits a short grace before
   SIGKILL, so a `claude` being shut down can checkpoint rather than being shot mid-write.

**Why this ordering matters:** (2) is worthless without (1) — a durable layout that only ever
reloads in a *second* daemon while the first still holds the PTYs just moves the confusion.

**Tech Stack:** Rust, tokio, `nix` (already an `amux-daemon` dep), `tokio-util` `Framed` +
`amux-proto` postcard codec, serde/`serde_json` for `state.json`, ratatui `TestBackend` for client
render assertions.

## Global Constraints

Inherited from `CLAUDE.md` and `docs/DESIGN.md` §2. Every task is bound by these.

- **No wire change.** `PROTO_VERSION` stays **17** (unchanged). `Layout`, `SpawnShell`, `SetLayout` already
  exist and are sufficient; if a task seems to need a new message, stop and re-read the design —
  the fallback in Task 4 is the only place a bump was even considered, and it is explicitly
  deferred.
- **No new external dependency, and no new crate-level dep edge.** `nix` is already in
  `amux-daemon`; it must not be added to `amux-tui` or `amux-core`. Dependency direction stays
  `main → amux-tui → amux-proto ← amux-daemon → amux-core`; `amux-daemon` must not depend on
  `amux-tui` (this is why the handshake probe in Task 1 is deliberately duplicated rather than
  shared with `amux-tui::client::try_handshake`).
- **`amux-core` stays pure** — no tokio, no signals, no sockets. All process signalling lives in
  `amux-daemon`.
- **The daemon remains the single source of truth.** The client renders what the daemon sends; the
  restore path in Task 4 issues existing `ClientMsg`s and invents no state.
- **No `unwrap()`/`expect()` in library code** (tests may `unwrap`). `tracing` only — no
  `println!`/`eprintln!`/`dbg!`.
- **Both platforms first-class** (macOS + Linux). Task 1 and Task 5 touch signals and process
  reaping; respect §11 (`Ok(0)` on macOS *and* `EIO` on Linux mean "PTY closed").
- **Definition of done — all four green and observed:** `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`,
  `cargo test --workspace`. A feature exits on **green CI**, not green local.
- **Commits:** one logical change per commit; imperative subject (~72 chars); body explains why +
  mechanism + what the regression test proves; trailer
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Only commit when asked.
  Branch is `reinstall-save` (not `main`).

---

## File Structure

- **Modify:** `crates/amux-daemon/src/server.rs` — handshake-aware `bind_or_detect` + eviction.
- **Modify:** `crates/amux-daemon/src/lib.rs` — `pid_file` becomes `pub(crate)`; `shutdown_all`
  await point.
- **Modify:** `crates/amux-daemon/src/registry.rs` — `PersistedState.layouts`; `set_layout` saves on
  change; `load_state` restores layouts through a pure blanking transform; async `shutdown_all`.
- **Modify:** `crates/amux-daemon/src/pty.rs` — `Session::terminate` (SIGTERM → grace → SIGKILL).
- **Modify:** `crates/amux-tui/src/client.rs` — stop unlinking the socket before spawning.
- **Modify:** `crates/amux-tui/src/pane.rs` — `PaneTree::fill_blanks`.
- **Modify:** `crates/amux-tui/src/app.rs` — refill blank leaves with shells on layout restore.
- **Modify:** `docs/DESIGN.md` §5.5, `README.md` (one line).

---

## Task 1: The new daemon evicts an incompatible old one

**Files:**
- Modify: `crates/amux-daemon/src/server.rs` (`bind_or_detect`, new private `probe` + `evict`)
- Modify: `crates/amux-daemon/src/lib.rs` (`pid_file` → `pub(crate)`)
- Modify: `crates/amux-tui/src/client.rs` (drop the blind unlink)

**Interfaces:**
- `pub fn bind_or_detect(path: &Path) -> Result<UnixListener>` — signature unchanged; it is already
  called inside `runtime.block_on`, so the probe may be async.
- Private `async fn probe(path: &Path) -> Probe` where
  `enum Probe { Compatible, Incompatible, Dead }` — `Compatible` iff the peer answers
  `DaemonMsg::Hello { proto_version }` equal to ours within a 2s timeout; `Incompatible` on a
  mismatched `Hello`, a `DaemonMsg::Error`, or a timeout; `Dead` if connect fails.
- Private `fn evict(dir: &Path) -> Result<()>` — read the pidfile, `SIGTERM`, poll for exit up to
  5s (`kill(pid, None)` probes liveness), `SIGKILL` backstop, then remove the socket.

- [x] **Step 1: Write the failing tests**

Add to `crates/amux-daemon/tests/session.rs` (it already spins real daemons):

```rust
/// A daemon speaking a different protocol version must be replaced, not orphaned: the second
/// bind evicts the first, and the first is actually gone.
#[tokio::test]
async fn incompatible_daemon_is_evicted_not_orphaned() { /* ... */ }

/// The compatible case is unchanged — a second daemon refuses to start.
#[tokio::test]
async fn compatible_daemon_is_left_alone() { /* ... */ }

/// A socket file with no listener behind it (crash, no cleanup) is still cleared and rebound.
#[tokio::test]
async fn stale_socket_file_is_reclaimed() { /* ... */ }
```

Simulating "incompatible" without a second build: bind the socket from the test with a listener
that answers `DaemonMsg::Error { .. }` (or never answers) and write a pidfile pointing at a live
sacrificial child process; assert the child is reaped and the bind succeeds.

- [x] **Step 2: Implement `probe` + `evict`, rewire `bind_or_detect`**

`AddrInUse` → `probe(path).await`:
- `Compatible` → `bail!("an amux daemon is already running (socket {})", ...)` (today's message).
- `Incompatible` → `evict(dir)?` then bind. Log at `info` with the evicted pid.
- `Dead` → remove the socket and rebind (today's behavior).

- [x] **Step 3: Stop the client from orphaning**

In `crates/amux-tui/src/client.rs::connect`, delete `std::fs::remove_file(&socket).ok();`. The
comment above it must be rewritten — the new daemon now arbitrates, and unlinking here is exactly
what let the old daemon survive unreachably. Keep the spawn + retry loop as-is.

- [x] **Step 4: Verify by hand (this is a process-lifecycle bug; tests are necessary, not sufficient)**

Build, launch, split a pane, then bump `PROTO_VERSION` locally, rebuild, relaunch, and confirm with
`ps -eo pid,command | grep '[a]mux daemon'` that exactly **one** daemon exists afterwards and no
orphaned `claude` remains. Revert the local `PROTO_VERSION` bump.

**DoD:** the three tests above pass; a manual reinstall leaves exactly one daemon; `amux daemon
--stop` still stops the live one.

---

## Task 2: `set_layout` persists, and only on a real change

**Files:**
- Modify: `crates/amux-daemon/src/registry.rs`

**Interfaces:** `pub fn set_layout(&self, agent: AgentId, layout: Option<Layout>)` — unchanged
signature, now calls `self.save()` when (and only when) the stored value actually changed.

- [x] **Step 1: Write the failing test** — `set_layout` twice with the same value writes
      `state.json` once (assert on mtime or a save counter); a different value writes again.
- [x] **Step 2: Implement**, mirroring the existing anti-churn shape of `set_minis` (compare, assign,
      save outside the lock). The client re-sends the layout on every `reconcile`, so an
      unconditional save would hammer the disk on every keystroke-driven relayout.

**DoD:** test passes; no `save()` on a no-op `SetLayout`.

---

## Task 3: Layouts survive a daemon restart

**Files:**
- Modify: `crates/amux-daemon/src/registry.rs`

**Interfaces:**
- `PersistedState` gains `#[serde(default)] layouts: Vec<(AgentId, Layout)>` (a `Vec` of pairs, not
  a map — `AgentId` is not a string key, and `default` keeps older `state.json` files loading).
- Free fn `fn blank_dead_terminals(layout: &Layout, keep: TerminalId) -> Layout` — pure, recursive:
  any `Leaf { terminal: Some(t) }` with `t != keep` becomes `Leaf { terminal: None }`. Unit-testable
  with no daemon.

- [x] **Step 1: Write the failing tests**

```rust
/// A leaf holding the agent's primary survives; every shell leaf is blanked, and the tree's
/// shape (axes + ratios) is preserved exactly.
#[test]
fn blanking_keeps_the_primary_and_the_geometry() { /* ... */ }

/// Round-trip through state.json: save a split layout, reload, and get the same geometry back.
#[test]
fn layouts_survive_a_reload() { /* ... */ }

/// A layout whose agent did not survive the load is dropped, not resurrected.
#[test]
fn layouts_for_dead_agents_are_dropped() { /* ... */ }
```

- [x] **Step 2: Implement `save()`** — snapshot `state.layouts` into the new field.
- [x] **Step 3: Implement `load_state()`** — restore each layout only if its agent survived the
      load, passing it through `blank_dead_terminals(l, agent.primary)`. Shell PTYs died with the
      daemon; their ids must never be handed back to a client, which would `Attach` to nothing.

**DoD:** the three tests pass; `state.json` written by this build still loads in a build without
the field, and vice versa (`serde(default)` both ways).

---

## Task 4: The client refills restored blank panes with shells

**Files:**
- Modify: `crates/amux-tui/src/pane.rs` (`fill_blanks`)
- Modify: `crates/amux-tui/src/app.rs` (restore path in `swap_to_agent`)

**Rationale:** after Task 3 a restored layout has `Leaf { terminal: None }` where each split shell
used to be. That is already a legal, rendered state (`" empty "`, `app.rs`), and it is exactly the
transient state `PaneTree::split` produces before its `SpawnShell` lands — so "a blank leaf wants a
shell" is the established meaning, and refilling needs no new wire message.

**Interfaces:**
- `pub fn fill_blanks(&mut self, mut next: impl FnMut() -> P) -> Vec<P>` on `PaneTree<P>` — assigns
  a fresh payload to every `None` leaf, returning them in assignment order so the caller can send
  one `SpawnShell` per new terminal. Pure; no I/O.

- [x] **Step 1: Write the failing tests**

```rust
/// pane.rs — pure: every blank leaf gets a payload, occupied leaves are untouched, and the
/// returned vector matches what was assigned.
#[test]
fn fill_blanks_populates_only_empty_leaves() { /* ... */ }

/// app.rs — restoring a daemon-persisted layout with a blanked shell leaf sends exactly one
/// SpawnShell (keyed to the agent's primary via `like`) and renders two panes.
#[tokio::test]
async fn restoring_a_saved_layout_respawns_its_shells() { /* ... */ }
```

- [x] **Step 2: Implement `fill_blanks`** in `pane.rs` alongside `open`/`split`.
- [x] **Step 3: Wire the restore path** — in `swap_to_agent`, after
      `PaneTree::from_layout(&l)`, call `fill_blanks(TerminalId::new)`, register each new terminal
      in `self.terminals` against this agent, and send `SpawnShell { terminal, like: primary }` for
      each. Then `reconcile` as today.
- [x] **Step 4: Add a `TestBackend` snapshot** of a restored two-pane layout, per `CLAUDE.md`
      ("add a `ratatui` `TestBackend` snapshot for new rendering").

**Deferred fallback (do not implement without asking):** if genuinely-empty panes turn out to be a
state users want to keep, distinguishing "empty on purpose" from "wants a shell" needs a new
`Layout` variant — a wire change and a `PROTO_VERSION` bump. Out of scope here.

**DoD:** both tests + the snapshot pass; splitting a pane, quitting, restarting the *daemon*, and
reopening the agent brings the split back live in the same worktree.

---

## Task 5: Give agents a grace period on shutdown

**Files:**
- Modify: `crates/amux-daemon/src/pty.rs` (`Session::terminate`)
- Modify: `crates/amux-daemon/src/registry.rs` (`shutdown_all`)
- Modify: `crates/amux-daemon/src/lib.rs` (await the new async `shutdown_all`)

**Interfaces:**
- `pub async fn terminate(&self, grace: Duration)` on `Session` — `SIGTERM`, await the exit watch
  up to `grace`, then `SIGKILL`. `kill()` (SIGKILL, immediate) stays for `delete`, where the
  worktree is being destroyed anyway.
- `pub async fn shutdown_all(&self)` — terminates every session concurrently under one shared
  budget (2s total), so a wedged agent cannot stall daemon exit.

- [x] **Step 1: Write the failing tests** — a session running a SIGTERM-trapping script exits via
      the signal, not the backstop (assert the exit code / that it exited before the grace elapsed);
      a session that ignores SIGTERM is still gone after the grace.
- [x] **Step 2: Implement `terminate`**, careful to hold no lock across the await.
- [x] **Step 3: Make `shutdown_all` async** and await it in `run_blocking` — it already runs inside
      `block_on`, after `registry.save()`.

**DoD:** both tests pass; `amux daemon --stop` still exits promptly (≤ ~2s) with no surviving
children — verify with `pgrep -P <pid>` before and after.

---

## Task 6: Documentation rides the change

**Files:**
- Modify: `docs/DESIGN.md` §5.5 ("Persistence & restart honesty")
- Modify: `README.md` (line ~56)

- [x] **Step 1: `DESIGN.md` §5.5** — record that a daemon restart now preserves pane layouts, that
      an incompatible daemon is evicted rather than orphaned (with the handshake-probe rule), and
      that shell terminals are respawned empty rather than restored (their processes are gone —
      "restart honesty" is the section's whole point, so say so plainly).
- [x] **Step 2: `README.md`** — the current claim is that a layout is "restored (even across TUI
      restarts)". After this work that is true across daemon restarts and upgrades too. One line;
      no new section; no mention of daemons, pidfiles, or eviction — that is `DESIGN.md`'s job.

**DoD:** §5.5 no longer describes layout as memory-only; the README line is accurate against the
code, not against its previous wording.

---

## Out of scope (needs its own approval)

**Stale-process hook poisoning.** `HookReport` carries only an `AgentId` — no terminal id, no pid
(`amux-core/src/hook.rs:54`). So a hook from a *stale* `claude` is indistinguishable from the live
one's, and `Registry::on_hook` will happily overwrite the agent's `ai_session_id` with the stale
process's session — after which the next resume continues the wrong conversation. Task 1 removes
the mechanism that creates stale processes, which is the practical fix. Hardening the mailbox
itself means stamping reports with `AMUX_TERMINAL_ID` (already exported into every terminal's env)
and dropping reports whose terminal is not the agent's current primary — **a wire change and a
`PROTO_VERSION` bump**, which `CLAUDE.md` requires be confirmed first.

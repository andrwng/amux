# Persist the "previous session" pointer across TUI restarts

**Status:** approved (design)
**Date:** 2026-07-29

## Problem

`ctrl+b -` jumps the main pane to the previously-active agent (tmux's "last-window").
Today this pointer, `prev_active_agent`, lives **only** in the TUI client
(`crates/amux-tui/src/app.rs`). It is never sent to or stored by the daemon, so it
resets every time the TUI restarts — after a restart, `ctrl+b -` has nowhere to jump.

By contrast, the *active* pointer already survives restarts: the daemon persists it to
`~/.amux/state.json` and replays it to a reconnecting client as `DaemonMsg::Active`
(sent once on connect at `server.rs:246`). The active-restore behavior is confirmed
working; only "previous" is missing.

`active` and `previous` are the two halves of the same selection concept, yet only `active`
survives a restart. This aligns them: `previous` becomes durable daemon state like `active`,
instead of a client-only value that dies with the TUI.

## Key insight

Mirror how `active` already works. The daemon does **not** derive `active` — `set_active`
(`registry.rs:383`) is *dumb storage*. The client decides selection in `swap_to_agent`,
then pushes it to the daemon via `ClientMsg::SetActive` inside `reconcile`
(`app.rs:1830`); the daemon stores it and replays it on connect. `previous` should flow the
same way: the client (which already computes `prev_active_agent` correctly and has it
tested) pushes it; the daemon stores/persists/replays. This keeps the client's already-tested
`swap_to_agent` the *single authority* for the ping-pong logic — nothing is duplicated, so
nothing can drift.

> **Rejected alternative — daemon derives `previous` from the `SetActive` stream.** Tempting
> because it needs no new `ClientMsg`, but it forces the daemon to re-implement the client's
> selection guard (`old.is_some() && new.is_some() && old != new`). That duplication is a
> latent drift bug (an earlier draft of this spec got the `old.is_some()` case wrong,
> clobbering a preserved `previous` on open-from-empty). Storing the client's decision, as
> we already do for `active`, is correct by construction.

## Design

### 1. Wire — `crates/amux-proto`

- Add `ClientMsg::SetPrevious(Option<AgentId>)` alongside `SetActive` (`message.rs:133`).
- Add `DaemonMsg::Previous(Option<AgentId>)` alongside `Active` (`message.rs:163`).
- **Bump `PROTO_VERSION` 19 → 20** (`amux-proto/src/lib.rs`). *(User-approved.)*
- Add codec round-trip tests covering both new variants.

### 2. Client — `crates/amux-tui/src/app.rs`

- In `reconcile`, one line after the existing `SetActive` send (`app.rs:1830`):
  `sink.send(ClientMsg::SetPrevious(self.prev_active_agent)).await?;`. The client's
  `prev_active_agent` is already maintained by `swap_to_agent` (`app.rs:1038`) and cleared
  on removal (`app.rs:590`) — this just publishes it for persistence.
- Handle `DaemonMsg::Previous(id)` in `on_daemon` (`app.rs:526`) by seeding
  `self.prev_active_agent = id`. Ordering matters: the daemon sends `Previous` *after*
  `Active` on connect (see §3), so the restore of the main pane — which runs
  `swap_to_agent` and may touch `prev_active_agent` — happens first and the persisted seed
  wins.
- `swap_to_agent` and `previous_agent()` (`app.rs:1913`) are otherwise untouched; in-session
  behavior is unchanged. `previous_agent()` already returns the target only if it still
  exists, so a stale seed (e.g. pointing at an agent deleted while the TUI was down) is
  harmless.

### 3. Daemon storage — `crates/amux-daemon/src/registry.rs`

- Add `previous: Option<AgentId>` to the in-memory `State` (`registry.rs:137`) and to the
  durable `PersistedState` (`registry.rs:157`) with `#[serde(default)]` (matching `active`
  at `registry.rs:164`, so an older `state.json` still loads). Wire it through
  `save()`/`load_state()`.
- Add `set_previous(Option<AgentId>)` — dumb storage with the same guard-on-change +
  save-only-on-change anti-churn as `set_active` — and a `previous()` accessor.
- Handle the new `ClientMsg::SetPrevious` in `server.rs` (next to `SetActive` at
  `server.rs:344`): `registry.set_previous(agent)`.
- Send `DaemonMsg::Previous(registry.previous())` on connect, immediately after the existing
  `Active` send (`server.rs:246`). Sent once on connect only — not broadcast on change —
  exactly matching `Active` (live tracking is client-side).

### 4. Agent removal — `crates/amux-daemon/src/registry.rs`

At the existing removal site (`registry.rs:1057-1059`, which already nulls `active` when its
agent is deleted), add the symmetric `if state.previous == Some(id) { state.previous = None; }`.
The client also nulls its copy (`app.rs:590`) and will push `SetPrevious(None)` on its next
`reconcile`, but the daemon-side null closes the crash-before-reconcile window and avoids
persisting a dangling id.

## Testing

- **Daemon/registry unit test:** `set_previous` stores the pushed value, saves only on a
  real change, survives a `save()` → `load_state()` round-trip, and is nulled when its agent
  is removed. (Ping-pong semantics are *not* tested here — the daemon doesn't own them.)
- **Client (`app.rs`, existing suite at `app.rs:4310+`):** the ping-pong tests already cover
  `swap_to_agent`/`prev_active_agent`; keep them. Add a test that a `DaemonMsg::Previous(Some(id))`
  on a fresh connect seeds the jump target so a subsequent `ctrl+b -` opens `id` — the
  regression test that proves the restart bug is fixed.
- **Codec (`amux-proto`):** round-trip tests for `ClientMsg::SetPrevious` and
  `DaemonMsg::Previous`.

## Non-goals

- Broadcasting `previous` live to multiple connected clients (active isn't broadcast
  either; out of scope and unchanged).
- Persisting the sidebar cursor (`sidebar_sel`) — separate concern, not requested.

## Invariants / checklist touchpoints

- Wire change → `PROTO_VERSION` bump + codec round-trip tests (invariant #3). **Approved.**
- Invariant #2: `previous` now flows exactly like `active` — the client decides it live and
  the daemon persists/replays it. The client no longer *invents* a `previous` that dies at
  restart; the durable value lives in the daemon.
- README: `ctrl+b -` is already documented as "previous session"; the shortcut's behavior
  is unchanged from the user's view (it just now survives restart). Verify the existing row
  still reads correctly against the code; no new row expected. Update only if the current
  wording implies session-scoped-only.

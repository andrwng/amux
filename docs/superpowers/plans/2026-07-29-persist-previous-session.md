# Persist the "previous session" pointer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ctrl+b -` (jump to previous session) survive a TUI restart by persisting the "previous active agent" pointer in the daemon, exactly the way the "active" pointer is already persisted.

**Architecture:** `previous` flows like `active`: the client's `swap_to_agent` remains the single authority that decides it, the client pushes it via a new `ClientMsg::SetPrevious` inside `reconcile`, and the daemon stores it (dumb storage), persists it in `state.json`, and replays it on connect as a new `DaemonMsg::Previous`. The client seeds `prev_active_agent` from that replay. No selection logic is duplicated into the daemon.

**Tech Stack:** Rust workspace (`amux-proto` wire DTOs + `postcard` codec, `amux-daemon` tokio runtime + registry, `amux-tui` ratatui client). Tests: `cargo test --workspace`.

## Global Constraints

- **`PROTO_VERSION` bumps 19 → 20** (`crates/amux-proto/src/lib.rs`). User-approved. Any wire change requires the bump **and** codec round-trip test coverage (DESIGN.md §6, invariant #3).
- **Daemon is the single source of truth** (invariant #2); the daemon stores what the client decides — it must **not** re-derive `previous` from the `SetActive` stream.
- **`amux-core` stays pure** — this plan does not touch it. `previous` is daemon runtime/persisted state, not domain logic.
- **`thiserror` at boundaries, `anyhow` at edges; no `unwrap()`/`expect()` in library code; `tracing` only — never `println!`/`eprintln!`/`dbg!`.**
- **Definition of done (all green, observed):** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace`.
- **Commit trailer on every commit:** `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Branch off `main`; do not commit to `main`. One logical change per commit. Only commit when the user has asked.

## File Structure

- `crates/amux-proto/src/message.rs` — add `ClientMsg::SetPrevious` and `DaemonMsg::Previous` variants (DTOs only).
- `crates/amux-proto/src/lib.rs` — bump `PROTO_VERSION` and add its changelog line.
- `crates/amux-proto/src/codec.rs` — extend the round-trip tests.
- `crates/amux-daemon/src/registry.rs` — add `previous` to `State` + `PersistedState`, `set_previous`/`previous` accessors, save/load wiring, removal null.
- `crates/amux-daemon/src/server.rs` — handle `ClientMsg::SetPrevious`; send `DaemonMsg::Previous` on connect.
- `crates/amux-daemon/tests/session.rs` — integration tests for reconnect replay + removal clearing.
- `crates/amux-tui/src/app.rs` — push `SetPrevious` in `reconcile`; handle `DaemonMsg::Previous` (seed + reconcile); add a regression test.

---

### Task 1: Wire protocol — new messages + version bump

**Files:**
- Modify: `crates/amux-proto/src/message.rs` (after line 133; after line 163)
- Modify: `crates/amux-proto/src/lib.rs` (PROTO_VERSION + doc comment)
- Test: `crates/amux-proto/src/codec.rs` (`client_messages_roundtrip` after line 228; `daemon_messages_roundtrip` after line 260)

**Interfaces:**
- Produces: `ClientMsg::SetPrevious(Option<AgentId>)` — client → daemon, "persist my jump-to-previous target". `DaemonMsg::Previous(Option<AgentId>)` — daemon → client, "here is the persisted previous target, on connect". `PROTO_VERSION == 20`.

- [ ] **Step 1: Add the failing round-trip test lines**

In `crates/amux-proto/src/codec.rs`, inside `client_messages_roundtrip` (right after line 228, the `SetActive(None)` line):

```rust
        roundtrip(ClientMsg::SetPrevious(Some(id)));
        roundtrip(ClientMsg::SetPrevious(None));
```

Inside `daemon_messages_roundtrip` (right after line 260, the `Active(None)` line):

```rust
        roundtrip(DaemonMsg::Previous(Some(id)));
        roundtrip(DaemonMsg::Previous(None));
```

- [ ] **Step 2: Run the tests to verify they fail (compile error)**

Run: `cargo test -p amux-proto`
Expected: FAIL — `no variant named SetPrevious` / `no variant named Previous`.

- [ ] **Step 3: Add the message variants**

In `crates/amux-proto/src/message.rs`, in the `ClientMsg` enum immediately after `SetActive(Option<AgentId>)` (line 133):

```rust
    /// Persist the "jump to previous" target (tmux's last-window) so `Ctrl+B -` survives closing
    /// the TUI. Decided by the client (mirrors `SetActive`); the daemon just stores it.
    SetPrevious(Option<AgentId>),
```

In the `DaemonMsg` enum immediately after `Active(Option<AgentId>)` (line 163):

```rust
    /// The persisted "jump to previous" target (on connect) so a re-attaching client can seed
    /// `Ctrl+B -`. Sibling of `Active`.
    Previous(Option<AgentId>),
```

- [ ] **Step 4: Bump `PROTO_VERSION` and document it**

In `crates/amux-proto/src/lib.rs`, append to the changelog doc comment (after the `v19` sentence):

```rust
/// v20 previous-session persistence (SetPrevious, Previous — `Ctrl+B -` jump-to-previous target
/// survives a TUI restart, restored on reconnect like the active agent).
```

Change the constant:

```rust
pub const PROTO_VERSION: u32 = 20;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p amux-proto`
Expected: PASS (including `version_mismatch`-style tests that reference `PROTO_VERSION`).

- [ ] **Step 6: Commit**

```bash
git add crates/amux-proto/src/message.rs crates/amux-proto/src/lib.rs crates/amux-proto/src/codec.rs
git commit -m "Add SetPrevious/Previous wire messages (proto v20)"
```

---

### Task 2: Daemon stores, persists, and replays `previous`

**Files:**
- Modify: `crates/amux-daemon/src/registry.rs` (`State` after line 151; `PersistedState` after line 165; `save()` snapshot ~line 437; `load_state()` after line 539; accessors after `set_active`/`active` ~line 397; removal null at lines 1057-1059)
- Modify: `crates/amux-daemon/src/server.rs` (handler near line 344; connect-send after line 246)
- Test: `crates/amux-daemon/tests/session.rs` (two new `#[tokio::test]`s modeled on `layout_persists_for_a_reconnecting_client` at line 887)

**Interfaces:**
- Consumes: `ClientMsg::SetPrevious`, `DaemonMsg::Previous` (Task 1).
- Produces: `Registry::set_previous(&self, previous: Option<AgentId>)` (guard-on-change + save, like `set_active`); `Registry::previous(&self) -> Option<AgentId>`.

- [ ] **Step 1: Write the failing integration tests**

In `crates/amux-daemon/tests/session.rs`, add (place after `layout_persists_for_a_reconnecting_client`, ~line 962). This mirrors that test's structure (in-memory registry, reconnect, read the replay frames):

```rust
/// The "jump to previous" target is persisted and replayed to a reconnecting client, so
/// `Ctrl+B -` still works after the TUI restarts.
#[tokio::test]
async fn previous_persists_for_a_reconnecting_client() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(adapter);
    let repo_id = registry.register(worktrees).id;
    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry));

    let mut client = handshake(&socket).await;
    let a1 = create_agent(&mut client, repo_id, "feat/one").await;
    let a2 = create_agent(&mut client, repo_id, "feat/two").await;
    client.send(ClientMsg::SetActive(Some(a2.id))).await.unwrap();
    client.send(ClientMsg::SetPrevious(Some(a1.id))).await.unwrap();
    // Round-trip so the daemon has surely processed SetPrevious before we reconnect.
    client.send(ClientMsg::ListAgents).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Ok(DaemonMsg::Agents(_))) = client.next().await {
                return;
            }
        }
    })
    .await
    .unwrap();

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut c2 = Framed::new(stream, ClientCodec::new());
    c2.send(ClientMsg::Hello {
        proto_version: PROTO_VERSION,
    })
    .await
    .unwrap();
    let mut got_previous = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c2.next().await {
                Some(Ok(DaemonMsg::Previous(prev))) => {
                    got_previous = Some(prev);
                    return; // Previous is the last handshake frame
                }
                Some(Ok(_)) => {}
                _ => return,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        got_previous,
        Some(Some(a1.id)),
        "reconnecting client should receive the saved previous target"
    );
}

/// Deleting the previous agent clears the persisted target, so the daemon never replays a ghost.
#[tokio::test]
async fn removing_the_previous_agent_clears_it() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(adapter);
    let repo_id = registry.register(worktrees).id;
    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry));

    let mut client = handshake(&socket).await;
    let a1 = create_agent(&mut client, repo_id, "feat/one").await;
    let a2 = create_agent(&mut client, repo_id, "feat/two").await;
    client.send(ClientMsg::SetActive(Some(a2.id))).await.unwrap();
    client.send(ClientMsg::SetPrevious(Some(a1.id))).await.unwrap();
    client
        .send(ClientMsg::DeleteAgent {
            id: a1.id,
            force: true,
        })
        .await
        .unwrap();
    assert!(wait_for_removed(&mut client, a1.id).await);

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut c2 = Framed::new(stream, ClientCodec::new());
    c2.send(ClientMsg::Hello {
        proto_version: PROTO_VERSION,
    })
    .await
    .unwrap();
    let mut got_previous = Some(Some(a1.id));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c2.next().await {
                Some(Ok(DaemonMsg::Previous(prev))) => {
                    got_previous = Some(prev);
                    return;
                }
                Some(Ok(_)) => {}
                _ => return,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        got_previous,
        Some(None),
        "deleting the previous agent must clear the persisted target"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail (compile error)**

Run: `cargo test -p amux-daemon --test session previous`
Expected: FAIL — `no variant named SetPrevious`/`Previous` handled by the daemon (the client sends it but the daemon drops/ignores it, and never sends `DaemonMsg::Previous`), so `got_previous` never matches.

- [ ] **Step 3: Add `previous` to in-memory `State`**

In `crates/amux-daemon/src/registry.rs`, in `struct State` immediately after the `active` field (line 151):

```rust
    /// The "jump to previous" target (`Ctrl+B -`) — replayed to a re-attaching client so the
    /// last-window jump survives the TUI closing. Durable, like `active`; the client decides it.
    previous: Option<AgentId>,
```

- [ ] **Step 4: Add `previous` to `PersistedState`**

In `struct PersistedState` immediately after the `active` field (line 165):

```rust
    /// The persisted "jump to previous" target (restored into the client's `Ctrl+B -` on connect).
    /// `default` so a `state.json` written before this field still loads.
    #[serde(default)]
    previous: Option<AgentId>,
```

- [ ] **Step 5: Persist it in `save()`**

In `save()`, in the `PersistedState { ... }` literal, add immediately after `active: state.active,` (line 437):

```rust
                previous: state.previous,
```

- [ ] **Step 6: Restore it in `load_state()`**

In `load_state()`, immediately after the `state.active = ...` block (lines 536-539):

```rust
        // Restore the previous-jump target only if its agent survived the load.
        state.previous = persisted
            .previous
            .filter(|id| state.agents.contains_key(id));
```

- [ ] **Step 7: Add `set_previous` and `previous` accessors**

In `crates/amux-daemon/src/registry.rs`, immediately after `set_active`/`active` (after line 397):

```rust
    /// Persist the "jump to previous" target (`Ctrl+B -`), replayed to a re-attaching client.
    /// Dumb storage decided by the client — the daemon never derives it. Saves only on a real
    /// change (same anti-churn reasoning as `set_active`).
    pub fn set_previous(&self, previous: Option<AgentId>) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            let changed = state.previous != previous;
            state.previous = previous;
            changed
        };
        if changed {
            self.save();
        }
    }

    pub fn previous(&self) -> Option<AgentId> {
        self.state.lock().unwrap().previous
    }
```

- [ ] **Step 8: Null `previous` when its agent is removed**

In the removal method, inside the `state` lock block right after the existing active-null (lines 1057-1059):

```rust
            if state.previous == Some(id) {
                state.previous = None;
            }
```

- [ ] **Step 9: Handle `SetPrevious` in the server**

In `crates/amux-daemon/src/server.rs`, next to the `SetActive` arm (line 344):

```rust
        ClientMsg::SetPrevious(agent) => registry.set_previous(agent),
```

- [ ] **Step 10: Send `Previous` on connect**

In `crates/amux-daemon/src/server.rs`, immediately after the `Active` send (line 246):

```rust
    framed.send(DaemonMsg::Previous(registry.previous())).await?;
```

- [ ] **Step 11: Run the tests to verify they pass**

Run: `cargo test -p amux-daemon --test session previous`
Expected: PASS — both `previous_persists_for_a_reconnecting_client` and `removing_the_previous_agent_clears_it`.

- [ ] **Step 12: Commit**

```bash
git add crates/amux-daemon/src/registry.rs crates/amux-daemon/src/server.rs crates/amux-daemon/tests/session.rs
git commit -m "Persist and replay the previous-session target in the daemon"
```

---

### Task 3: Client pushes and seeds `previous`

**Files:**
- Modify: `crates/amux-tui/src/app.rs` (`reconcile` after line 1830; `on_daemon` new arm after line 564)
- Test: `crates/amux-tui/src/app.rs` (new `#[tokio::test]` next to the ping-pong tests, ~line 4395)

**Interfaces:**
- Consumes: `ClientMsg::SetPrevious`, `DaemonMsg::Previous` (Task 1); `Registry` replays `Previous` last on connect (Task 2). The existing `swap_to_agent` (line 1038) maintains `prev_active_agent`; `previous_agent()` (line 1913) guards existence.

- [ ] **Step 1: Write the failing regression test**

In `crates/amux-tui/src/app.rs`, add after `agent_removal_clears_previous_target` (~line 4395):

```rust
    /// A restart seeds `Ctrl+B -` from the daemon: the persisted previous target arrives as
    /// `DaemonMsg::Previous` on connect, so the jump works before the user has swapped anything.
    #[tokio::test]
    async fn daemon_previous_seeds_the_jump_target() {
        let (mut app, ids) = app_with_agents(2);
        let (mut sink, _server) = test_sink();
        // Simulate the connect-time main-pane restore (DaemonMsg::Active reopened ids[1]).
        let _ = app.swap_to_agent(ids[1]);
        assert_eq!(
            app.prev_active_agent, None,
            "restoring the active agent from nothing leaves no previous"
        );
        // Previous arrives last on connect and seeds the jump target.
        app.on_daemon(DaemonMsg::Previous(Some(ids[0])), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.prev_active_agent,
            Some(ids[0]),
            "seed the jump target from the daemon's persisted previous"
        );
        app.on_key(ctrl('b'), &mut sink).await.unwrap();
        app.on_key(key(KeyCode::Char('-')), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            app.active_agent,
            Some(ids[0]),
            "Ctrl+B - jumps to the seeded previous immediately after a restart"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p amux-tui daemon_previous_seeds_the_jump_target`
Expected: FAIL — `no variant named Previous` in the `on_daemon` match / `prev_active_agent` stays `None`.

- [ ] **Step 3: Push `SetPrevious` from `reconcile`**

In `crates/amux-tui/src/app.rs`, in `reconcile`, immediately after the `SetActive` send (line 1830):

```rust
        sink.send(ClientMsg::SetPrevious(self.prev_active_agent))
            .await?;
```

- [ ] **Step 4: Handle `DaemonMsg::Previous` in `on_daemon`**

In `on_daemon`, add a new arm immediately after the `DaemonMsg::Active` arm closes (after line 564):

```rust
            DaemonMsg::Previous(prev) => {
                // Seed the jump-to-previous target from persistence. Sent after `Active`, so the
                // main-pane restore (which runs `swap_to_agent` and may set `prev_active_agent`)
                // has already happened and this persisted value wins. Reconcile so the daemon's
                // copy re-converges to it — the earlier `Minis`/`Active` reconciles pushed a stale
                // `None` before this seed arrived (the same converge dance `Active` relies on).
                self.prev_active_agent = prev.filter(|id| self.agents.iter().any(|a| a.id == *id));
                self.reconcile(sink).await?;
            }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p amux-tui daemon_previous_seeds_the_jump_target`
Expected: PASS. Also run the existing previous/ping-pong tests to confirm no regression:
`cargo test -p amux-tui prev_agent_tracks_last_active_and_pingpongs ctrl_b_dash_opens_previous_agent_and_toggles agent_removal_clears_previous_target`
Expected: PASS.

- [ ] **Step 6: Verify the README needs no change**

`ctrl+b -` is a pre-existing shortcut; this change makes it *survive restart* but adds no key, command, panel, or config knob. Confirm the existing shortcut row still reads correctly against the code:

Run: `grep -n 'b -\|previous' README.md`
Expected: the `Ctrl+B -` / "previous session" row is present and still accurate (no wording implies it is session-scoped-only). If a row's description is now wrong, fix it in this commit; otherwise leave the README unchanged.

- [ ] **Step 7: Commit**

```bash
git add crates/amux-tui/src/app.rs
git commit -m "Push and seed the previous-session target in the TUI"
```

---

### Task 4: Full Definition-of-Done gate

**Files:** none (verification only).

- [ ] **Step 1: Format**

Run: `cargo fmt --all -- --check`
Expected: clean (no diff).

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Build**

Run: `cargo build --workspace --all-targets`
Expected: success.

- [ ] **Step 4: Full test suite**

Run: `cargo test --workspace`
Expected: all pass, including the three new tests (`previous_persists_for_a_reconnecting_client`, `removing_the_previous_agent_clears_it`, `daemon_previous_seeds_the_jump_target`) and the pre-existing proto/app previous tests.

- [ ] **Step 5: End-to-end manual confirmation (runtime behavior changed — required by CLAUDE.md)**

> **Note:** per the project memory, do **not** `cargo run` amux from an agent worktree — it attaches the user's live daemon. This step is for the user (or a session running against a throwaway `AMUX_HOME`).

1. Open agent A, then agent B in the main pane. Press `ctrl+b -` → jumps back to A. Good (unchanged live behavior).
2. Open A then B. Quit the TUI (`ctrl+b q` or however you exit). Relaunch `cargo run`.
3. Press `ctrl+b -`. Expected: it jumps to A — the previous target survived the restart. (Before this change it was a no-op after restart.)

---

## Self-Review

**Spec coverage:**
- Wire (`ClientMsg::SetPrevious`, `DaemonMsg::Previous`, PROTO 20, codec tests) → Task 1. ✓
- Client push in `reconcile` + `Previous` seed handler → Task 3. ✓
- Daemon state + `PersistedState` (`#[serde(default)]`) + `set_previous`/`previous` + save/load → Task 2 (Steps 3-7). ✓
- Agent-removal null → Task 2 (Step 8). ✓
- Connect-send after `Active` → Task 2 (Step 10). ✓
- Tests: daemon reconnect-replay + removal-clear (Task 2), client seed + `ctrl+b -` regression (Task 3), codec round-trips (Task 1). ✓
- README check → Task 3 (Step 6). ✓

**Placeholder scan:** none — every code step has literal code and an exact anchor.

**Type consistency:** `set_previous(Option<AgentId>)`/`previous() -> Option<AgentId>` used identically in registry, server, and tests. `ClientMsg::SetPrevious(Option<AgentId>)` and `DaemonMsg::Previous(Option<AgentId>)` consistent across all tasks. Test helpers (`app_with_agents`, `test_sink`, `ctrl`, `key`, `handshake`, `create_agent`, `wait_for_removed`) all verified present in the current test files.

**Ordering note (why the `Previous` handler reconciles):** on connect the daemon sends `Active` then `Previous`; the client's `Minis`/`Active` handlers each call `reconcile`, which now pushes `SetPrevious(self.prev_active_agent)` while it is still stale (`None`), transiently clobbering the daemon's stored value. The `Previous` handler seeds the real value and reconciles last, so the daemon re-converges — this is the exact mirror of how `Active` already survives the same `Minis`-step clobber.

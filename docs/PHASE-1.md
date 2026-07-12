# Phase 1 — Agents, Worktrees, Sidebar

**Goal:** turn the single persistent shell into **N agents** — each a git worktree running an
agent CLI — listed in a **persistent sidebar** with live (coarse) status, one selected into the
main window. Exact hook-based status is **Phase 2**; floating minis are **Phase 3**.

This is a clean continuation of Phase 0: the daemon's single-session `Registry` becomes a keyed
map of agents, per-connection attach becomes per-agent attach, and we graft in grove's worktree
logic. See `docs/DESIGN.md` §4 (domain), §5 (daemon), §7 (TUI).

Each milestone is done when its Definition of Done holds and its tests are green (macOS + the
Ubuntu Docker check).

---

## 1.1 — Agent domain model (`amux-core`) &nbsp; · &nbsp; **✓ code-complete** (6 transition-table tests green)

`AgentId`, `AgentState` (+ `AttentionKind`), and the pure `next_state` transition function —
the correctness core, exhaustively tested. `NeedsAttention` exists now; only Phase 2 hooks
will actually produce it (Phase 1 emits the coarse `Started/SawActivity/WentIdle/Exited`).

**DoD:** `next_state` is total and covered by a transition table; distinct sidebar glyphs.

## 1.2 — Worktree service (`amux-core`) &nbsp; · &nbsp; **✓ code-complete** (create/list/remove round-trip on a temp repo; global-base stability tested)

Port grove's `src/git/worktree.rs` (git2 + `vendored-libgit2`): create/remove/list worktrees,
symlink shared files. Config: branch prefix, main branch, worktree location. Tests against a
temp git repo (`tempfile` + git2).

**DoD:** create/remove/list round-trip on a temp repo, on macOS + Ubuntu.

## 1.3 — `AgentAdapter` + `ClaudeAdapter` (`amux-core`) &nbsp; · &nbsp; **✓ code-complete** (spawn_spec/resume/custom-command tests green)

The CLI boundary: `spawn_spec` (command/args/env/cwd to launch in a worktree), `prepare_worktree`
(minimal in Phase 1 — hook install is Phase 2), `capabilities`. `ClaudeAdapter` defaults to
`claude`; command is configurable, with `$SHELL`/`cat` used in tests.

**DoD:** `spawn_spec` yields the right command+cwd+env; unit-tested.

## 1.4 — Protocol v1 (`amux-proto`) &nbsp; · &nbsp; **✓ code-complete** (agent-id'd messages + AgentInfo; 8 codec tests; PROTO_VERSION=1)

Introduce `AgentId` into the wire. New messages: `CreateAgent`, `DeleteAgent`, `ListAgents` /
`AgentList` / `AgentAdded` / `AgentRemoved` / `StateChanged`; per-agent `Attach` / `Subscribe` /
`Unsubscribe` / `Input{id}` / `Resize{id}` / `Output{id}` / `OutputSnapshot{id}` / `Exited{id}`.
Bump `PROTO_VERSION` (breaking; single-user, no back-compat needed).

**DoD:** round-trip + property tests for the new messages; version bump.

## 1.5 — Daemon: durable agents over generic sessions &nbsp; · &nbsp; **✓ code-complete** (create/attach/echo/delete integration test; exit→suspend monitor; per-repo daemon)

**Agent ≠ Session.** A `Session` (Phase 0's `pty::Session`) is a *generic* PTY process — it just
runs a command. An **Agent** is the *durable* record: id + worktree + branch + adapter +
`ai_session_id` + timestamps, holding **zero-or-one** live `Session`. `Registry` →
`HashMap<AgentId, Agent>`, each agent optionally attached to a session.

Lifecycle (the safety net for accidental exits + daemon restarts):
- `CreateAgent` — worktree (1.2) + `spawn_spec` → a fresh `Session`.
- **Session exits** (accidental `exit`, crash, daemon restart) → the agent becomes
  **Suspended/resumable**, NOT removed: worktree stays, metadata persisted, sidebar shows it as
  resumable (`□ · r to resume`).
- **Resume** — new `Session` via `spawn_spec(resume = ai_session_id)` → `claude --resume <id>` in
  the same worktree, continuing the conversation. `ai_session_id` comes from Phase-2 hook payloads
  (`session_id`); `claude --continue` is the pre-hook fallback.
- `DeleteAgent` — the **only** destructive op: kill + remove worktree + drop the record.
- Per-agent attach/subscribe/`StateChanged`; agent metadata persisted to `~/.amux/state.json`
  (survives daemon restart; live sessions don't, so agents reload Suspended).

**DoD:** integration test — create 2 agents on a temp repo, list them, attach/echo each; let one
exit → it stays resumable and resumes into the *same* worktree; delete one → worktree gone. Green
on macOS + Ubuntu.

## 1.6 — TUI: the sidebar &nbsp; · &nbsp; **✓ code-complete** (sidebar + main + nav/terminal/create modes; grove keymap; builds clean)

Left **sidebar** (agent roster + status glyph, selection) + **main window** (selected agent's
terminal). Keymap **mirrors grove** for muscle memory: `j`/`k` select, `n` new, `d` delete,
`Enter`/`l` focus the main terminal, a leader/`Esc` back to the sidebar, `Ctrl-Q` detach. Focus
model: keystrokes route to the sidebar (nav) or the focused agent's PTY, never both.

**DoD:** create agents, see them listed with live status, select into main, interact, delete —
interactive run confirms; `TestBackend` snapshot test for the sidebar layout.

---

## Decisions (defaults chosen; flag any you'd change)

- **Worktree location:** default `~/.amux/worktrees/<repo>/<branch>` (global, grove-style),
  configurable to in-repo `.amux/worktrees/`. *(The one genuine preference — confirm before 1.2.)*
- **Agent CLI:** `ClaudeAdapter` defaults to `claude`; configurable; `$SHELL`/`cat` in tests.
- **Keymap:** mirror grove (muscle-memory reuse is a stated goal).
- **Protocol:** clean breaking bump to v1 — single-user, so no compatibility shims.
- **Agent ≠ Session** (durable workspace vs. disposable PTY process). Exiting suspends (keeps the
  worktree + metadata, resumable); only **delete** destroys. **Resume is ALWAYS manual** — no
  auto-resume, no auto-continue-on-restart. Nothing ever respawns on its own, so a crash stays
  visible and is recovered by an explicit `r`.
- **Minis stay out** (Phase 3); **hook status stays out** (Phase 2).

### Exit of Phase 1 &nbsp; · &nbsp; **✓ COMPLETE (1.1–1.6)**

Multiple agents in isolated worktrees, listed in a live sidebar (needs-attention-sorted), one
open in the main window at a time; create/delete/resume; exit suspends (resumable), delete
destroys. 25 tests green; clippy + fmt clean. **Deferred within Phase 1** (follow-ups, not
blockers): idle-timer Working↔Idle status refinement, `~/.amux/state.json` metadata persistence,
per-repo socket keying (currently one global socket → one repo per daemon), and running real
`claude` instead of `$SHELL` (that pairs with Phase 2 hooks). Phase 2 swaps coarse status for
exact hook signals; Phase 3 adds the floating minis.

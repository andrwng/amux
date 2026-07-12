# Phase 0 — The Spine

**Goal:** prove the entire **daemon ↔ client ↔ PTY ↔ render** loop works end-to-end, with
nothing else attached, on both macOS and Ubuntu. If this loop is solid, every later feature is
just layering. See `docs/DESIGN.md` §8 (build plan), §11 (verified deps + gotchas), §12 (testing).

Two shared **test fixtures** are built during this phase and reused forever after:

- **Injected clock** (`amux-core::clock`) — deterministic time under test.
- **Headless test client** — scripts daemon interactions and asserts on the event stream.

(The scriptable **fake-agent** arrives in Phase 1, when agents exist.)

Each milestone is *done* only when its **Definition of Done** holds **and** its tests are green
in CI on `ubuntu-latest` + `macos-latest`. Exit criteria are automated acceptance tests, not
manual checklists — except where a step genuinely needs an interactive TTY (noted inline).

---

## 0.1 — Bootstrap + de-risking spike &nbsp; · &nbsp; **✓ code-complete** (builds/tests green; interactive spike run + Ubuntu CI pending)

Prove the four scary integrations (portable-pty, vt100, tui-term, crossterm input/resize) in one
throwaway file *before* committing to the real architecture.

**Deliverables**
- Cargo workspace: `amux` binary + `crates/{amux-proto,amux-core,amux-daemon,amux-tui}`, with a
  pinned `[workspace.dependencies]` set (DESIGN §11).
- `examples/spike.rs` — spawn `$SHELL` in a PTY → vt100 → tui-term in a ratatui frame → forward
  keys → handle resize. Throwaway reference, not shipped code.
- CI matrix (`.github/workflows/ci.yml`): fmt + clippy (`-D warnings`) + build + test on both
  OSes; Ubuntu installs `build-essential pkg-config`.
- `amux-core::clock` (`SystemClock` + `ManualClock`) — the first real, tested core code.

**Tests**
- `cargo build --workspace --all-targets` green on both OSes.
- `clock` unit test (`ManualClock` advance/set).
- *Manual (needs a TTY):* `cargo run --example spike` runs a live shell — `vim` works, arrows
  work, resize reflows, `Ctrl-Q` quits, terminal restores cleanly.

**Definition of Done:** the spike runs a real shell inside a ratatui frame on macOS and Ubuntu;
the workspace builds clean; CI is green.

---

## 0.2 — `amux-proto`: framing + handshake &nbsp; · &nbsp; **✓ code-complete** (9 tests green)

**Deliverables**
- Phase-0 message subset: `Hello`, `Input`, `Resize`, `Output`, `OutputSnapshot`, `Shutdown`.
- `LengthDelimitedCodec` transport + `postcard` body encoding; a `Framed` helper.
- `PROTO_VERSION` handshake that refuses a mismatch.

**Tests**
- encode→decode round-trip for every message (property tests via `proptest`).
- version-mismatch handshake is rejected.
- two in-process tasks exchange a `Hello` over a socket pair.

**Definition of Done:** the protocol crate is fully unit-tested and carries no I/O.

---

## 0.3 — `amux-daemon`: control socket + one PTY + I/O tasks &nbsp; · &nbsp; **✓ code-complete** (2 integration tests + binary smoke-test green)

**Deliverables**
- `amux daemon`: self-daemonize (nix double-fork + `setsid`) **before** starting tokio; create
  the runtime dir; bind a `0600` unix control socket; write a lockfile.
- On client connect: spawn one `$SHELL` PTY; reader thread → vt100 parser + broadcast;
  snapshot-on-subscribe (`contents_formatted()`) then live stream; writer applies `Input`/`Resize`.
- Reader treats both `Ok(0)` and `Err` as closed (Linux EIO). Drop the slave after spawn.

**Tests**
- integration: headless test client connects → receives snapshot + stream; sends
  `Input "echo hi\n"` → observes `hi`; sends `Resize` → PTY winsize changes.
- socket permissions are `0600`; a second bind fails cleanly (lockfile honored).

**Definition of Done:** a scripted client drives a live shell through the daemon on both OSes.

---

## 0.4 — Client auto-spawn + attach &nbsp; · &nbsp; **✓ code-complete** (connect/handshake tested; detached-daemon + live-detection smoke-tested)

**Deliverables**
- `amux` (default): connect to the control socket; if absent, spawn `amux daemon`, wait for the
  socket (bounded retry), connect, handshake.
- Socket path: prefer `$XDG_RUNTIME_DIR/amux/`, else `~/.amux/run/` (ownership + `0700` checked;
  mind the `sun_path` length limit — DESIGN §11).

**Tests**
- integration: `amux` with no daemon running starts one and attaches; a second `amux` reuses it.
- fallback path chosen correctly when `$XDG_RUNTIME_DIR` is unset.

**Definition of Done:** cold-start `amux` auto-spawns a daemon and attaches, twice-over reuses it.

---

## 0.5 — `amux-tui`: render + input + resize + clean teardown &nbsp; · &nbsp; **✓ code-complete** (input+render tests green; interactive run pending; terminal restore via `ratatui::init` panic hook)

**Deliverables**
- ratatui/crossterm app: full-frame `tui-term` render of the PTY; key forwarding (KeyEvent→bytes,
  **incl. DECCKM**; one key reserved for detach); resize → `Resize`.
- Terminal-restore guard: a panic hook + drop guard that **always** restores cooked mode.
- Promote the spike's proven approach into real `amux-tui` code (spike is then deleted).

**Tests**
- input encoder tables (incl. DECCKM app-cursor mode); round-trip bytes back through vt100.
- TUI render snapshots via ratatui `TestBackend` + `insta`.
- panic-mid-render test asserts the terminal is restored.

**Definition of Done:** `amux` shows a live shell; `vim`/arrows/Ctrl-C work; resize reflows; quit
and panic both restore the terminal.

---

## 0.6 — Detach / reattach

**Deliverables**
- A detach key leaves the daemon + shell running; reattach resumes identically.
- `amux daemon stop` tears down daemon + PTY.

**Tests**
- integration: type in the shell, detach, reattach → same shell state; explicit stop kills
  daemon + PTY; reattach after stop starts fresh.

**Definition of Done:** the full spine — attach, interact, detach, reattach, stop — works on both
OSes, and the persistence guarantee (agent survives client exit) is proven by an automated test.

---

### Exit of Phase 0

The riskiest plumbing is proven and CI-green cross-platform. Phase 1 layers agents, worktrees,
and the sidebar on top of a spine we trust.

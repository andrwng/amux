# CLAUDE.md — how to work in this repo

This file is **how you work**. [`docs/DESIGN.md`](docs/DESIGN.md) is **what we're building and why**;
on any architectural question it is the contract and it wins. Read `DESIGN.md` §2 (principles)
before changing a crate boundary, §6 before touching the wire, §11 before touching dependencies
or platform-specific I/O.

Operate at a staff level: leave the codebase more legible than you found it, make the smallest
change that fully solves the problem, and never claim something works until you have watched it
work. When a rule here is only *review-enforced* (not caught by CI), it is called out — treat
those as the ones most likely to bite.

## Quick reference

```sh
cargo run                                              # the amux TUI (auto-spawns the daemon)
cargo run --example spike                              # Phase 0.1 PTY↔render spike (Ctrl-Q quits)
./.githooks/setup                                      # once per clone: enable the pre-commit hook

cargo fmt --all -- --check                             # \ 
cargo clippy --workspace --all-targets -- -D warnings  #  } the CI gates — see "Definition of done"
cargo build --workspace --all-targets                  # /
cargo test --workspace                                 #
```

The `amux` binary (`src/main.rs`) is thin dispatch: no args → TUI client · `amux daemon` → the
daemon (normally auto-spawned) · `amux hook` → the Claude hook→mailbox bridge · `amux doctor` →
prune orphaned worktrees.

## The loop (every non-trivial task)

This project is driven **interactively**. Do not skip to code.

1. **Brainstorm** intent and constraints (the `brainstorming` skill). Agree on *what* before *how*.
2. **Plan** — write the plan, get approval (the `writing-plans` skill) for anything multi-step.
3. **Implement test-first** — for pure logic in `amux-core`, the test (often a transition table)
   comes before the code (`test-driven-development`). Read the neighboring module first and match
   its patterns; consistency beats personal preference.
4. **Verify** against "Definition of done" below — observed, not assumed.
5. **Request review** (`requesting-code-review`) before you consider it finished.

Use `systematic-debugging` for any bug before proposing a fix. These skills are the default path,
not optional extras.

**Always stop and confirm before** (each is expensive to unwind): changing the wire protocol /
bumping `PROTO_VERSION`; adding an external dependency; committing or pushing; touching `main`;
deleting a branch or worktree. When the request is ambiguous about product behavior, ask — do not
guess.

## Invariants you may not break

These come from `DESIGN.md` §2. Each is tagged with the crate that owns it.

1. **`amux-core` stays pure.** No `tokio`, no PTY, no sockets, no ambient time — use the `Clock`
   trait (`amux-core/src/clock.rs`), never `Utc::now()`. Its only deps are `serde`/`serde_json`/
   `toml`/`git2`/`uuid`/`chrono`/`directories`/`anyhow`. If you reach for `tokio` in core, you are in the wrong
   crate. *(Review-enforced: nothing fails the build if you add an I/O dep here — reviewers must
   catch it.)*
2. **The daemon is the single source of truth.** `amux-daemon` owns all live state (processes,
   PTYs, state machines). Clients are projections: they render daemon state and **never invent it**.
   New runtime state lives in the daemon and reaches clients only as a `DaemonMsg`.
3. **The daemon↔client boundary is `amux-proto` — and only that.** Never leak internal types across
   the wire. Any change to a wire message bumps `PROTO_VERSION` (`amux-proto/src/lib.rs`, currently
   14) and updates the codec round-trip tests. *(Review-enforced: a missing bump compiles and
   passes CI but breaks a real client↔daemon pair — watch for it.)*
4. **CLI-specifics live behind one seam.** Everything is agent-CLI-agnostic except `AgentAdapter`
   and `StatusSource` (`amux-core/src/adapter.rs`). Special-casing `"claude"` anywhere outside an
   adapter is a bug. Adding a CLI must touch neither the daemon nor the TUI.
5. **Bounded everything.** No unbounded channels or buffers; scrollback is a bounded ring;
   backpressure is required. A runaway agent must not be able to OOM the daemon.
6. **Structured concurrency.** Every per-agent and per-client task has one clear owner and is
   cancelled deterministically on exit/disconnect. No orphan tasks.
7. **Dependency direction is acyclic:** `main → amux-tui → amux-proto ← amux-daemon → amux-core`.
   `amux-core` depends on no internal crate. *(This one Cargo enforces — a cycle won't compile.)*

## Where things live & how to extend

The map, then the recipes. Recipes name the **seam and the exemplar to copy**, not line numbers.

```
src/main.rs              binary dispatch (TUI / daemon / hook / doctor)
crates/amux-proto/       wire only: message.rs (ClientMsg/DaemonMsg), codec.rs (framing +
                         version handshake), lib.rs (PROTO_VERSION). No logic.
crates/amux-core/        domain: agent.rs (Agent + AgentState + next_state), adapter.rs
                         (AgentAdapter + StatusSource + ClaudeAdapter), clock.rs, config.rs,
                         hook.rs, worktree.rs, paths.rs, nav.rs. Pure; heavily unit-tested.
crates/amux-daemon/      runtime: server.rs (control socket), mailbox.rs (hook socket),
                         pty.rs, registry.rs (session registry), daemonize.rs. Owns async I/O.
crates/amux-tui/         ratatui client: app.rs (view model), client.rs (proto conn),
                         pane.rs (split tree + rendering), input.rs (key→bytes), doctor.rs.
```

- **Add an agent CLI (e.g. codex).** Implement `AgentAdapter` + a `StatusSource` in
  `amux-core/src/adapter.rs` (copy `ClaudeAdapter`), register it. Touch nothing in the daemon or
  TUI. Tests: `spawn_spec` command/args/env, `prepare_worktree` writes the exact files, and
  golden status fixtures (raw signal → emitted state). See `DESIGN.md` §4.3.
- **Add or change a wire message.** Edit `amux-proto/src/message.rs`, **bump `PROTO_VERSION`**, add
  a codec round-trip test, *then* handle the new arm in `amux-daemon/src/server.rs` and the TUI.
  Keep it a stable DTO — never a re-exported internal type. See `DESIGN.md` §6.
- **Add a daemon capability.** Follow the per-agent task ownership + cancellation model in
  `registry.rs` (`DESIGN.md` §5.2); persist any new durable state to `~/.amux/state.json`.
- **Add a TUI surface or keybinding.** The view model in `app.rs` is a projection — mutate it only
  from `DaemonMsg`s. Keymaps are config-driven, not hard-coded. Add a `ratatui` `TestBackend` +
  `insta` snapshot for new rendering. See `DESIGN.md` §7.
- **Add a config knob (resist this — YAGNI).** Two-level merge, global + per-repo, via `config.rs`
  / `paths.rs`. Add a parse/merge test. Fewer knobs than grove is a goal, not an accident.

## Conventions

- **Errors.** Typed `thiserror` errors for anything crossing a public boundary or matched on by a
  caller (wire + domain). `anyhow` for internal fallible glue and at the binary edges (`main.rs`).
  Never `unwrap()`/`expect()` in library code except for a genuinely unreachable startup invariant —
  and then with a message saying why.
- **Logging.** `tracing` only; structured; JSON logs to `~/.amux/log/`. No `println!`/`eprintln!`/
  `dbg!` in library crates.
- **Dependencies.** Do not add an external crate without first declaring it in
  `[workspace.dependencies]` (root `Cargo.toml`) and consuming it with `{ workspace = true }`. The
  pinned set in `DESIGN.md` §11 is mutually verified and load-bearing — prefer it, and justify any
  addition in the commit body. `git2` is local-only (no `https`/`ssh` features); anything networked
  shells out to the user's `git` CLI.
- **Platform parity.** Unix only; **macOS and Linux are both first-class.** If it only works on one,
  it is broken. Respect the §11 footguns: PTY reader on a dedicated blocking thread; drop the PTY
  slave right after spawn; treat both `Ok(0)` (macOS) and `EIO` (Linux) as "PTY closed"; daemonize
  *before* building the tokio runtime; handle DECCKM; short socket paths with an ownership check.
- **Formatting.** `rustfmt` is authoritative; `clippy -D warnings` is the floor. Don't hand-format
  around them.

## Definition of done

All four **green and observed** — never asserted from memory — before you say it's done:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

- The pre-commit hook (`./.githooks/setup`) runs the first two locally; CI runs all four on
  **Ubuntu and macOS**. Cross-platform breakage fails the build.
- **Every bug earns a regression test** — especially in status detection. The test must fail
  before your fix and pass after.
- **Tests ride the change.** Never defer them to "a later phase." See `DESIGN.md` §12.
- If you changed runtime behavior, drive it end-to-end and confirm the observable result — a green
  test suite is necessary, not sufficient (`verification-before-completion`).

## Commits

- One logical change per commit. Imperative subject (~72 chars).
- The body explains **why + the mechanism + what the regression test proves** — match the existing
  log (`git log` is the style guide; the recent history is exemplary, follow it).
- End agent commits with the trailer:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- Branch off `main`; never commit to `main` directly. Only commit or push when the user asks.

## Scope guardrail

Build the differentiators (`DESIGN.md` §1), not grove's breadth. Resist creep: third-party
integrations are explicitly out of scope, and every new knob or feature must earn its place against
the "strong bones" principles. When in doubt, the smaller design that upholds §2 wins.

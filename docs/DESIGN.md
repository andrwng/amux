# amux — Design & Architecture

> A terminal UI for multiplexing AI coding agents in isolated git worktrees.
> Status: **design draft** (pre-code). This document is the contract we agree on before implementation.

---

## 1. Product summary

amux runs many AI coding agents at once — each on its own git branch in an isolated
worktree — and lets you move between them **without ever losing context**.

The defining ergonomics, and how they differ from grove (the tool this grows out of):

| | grove | amux |
|---|---|---|
| Interacting with an agent | `tmux attach` = **full-screen takeover**; sidebar disappears | Agent renders in a pane **beside a persistent sidebar** |
| Knowing an agent needs you | ~2,600 lines of regex screen-scraping (fragile) | **Structured signals** from Claude Code hooks (exact) |
| Handling several agents at once | one at a time | a **tmux-splittable main area** (tiled panes) + **floating minis** (Gmail-style popups) |
| Session lifetime | tmux server | **`amuxd` daemon** — agents survive the UI closing |

Three interaction pillars:

1. **Sidebar = a calm inbox.** Every agent, with live status. A waiting agent shows *why*
   it wants you (pulled from the hook message, e.g. `api ⚠ perm — cargo test?`). Nothing
   ever pops up or steals focus. You *pull* an agent into view when you choose.
2. **Main window = one focused agent**, full interactive terminal.
3. **Floating minis = several other agents** as small live terminals in the bottom-right
   corner, added leftward. You go back and forth between them like multiple open Gmail chats.

```
┌ agents ─────┐┌ feat/auth ▸ claude ─────────────────────────────┐
│▸ auth  ● run││ ⏺ Edit src/login.rs                             │
│  api   ⚠perm││ ● wiring the refresh token                      │
│  docs  ○ idl││ > the token should rotate every 15m_            │
│  infra ⚠ ask││                                                 │
│  + new      │└──────────────────────────────┌ infra ▸ ask ──┐──┘
│             │                               │ Which region? │
│             │                        ┌ api ▸ perm ──────┐   │
│             │                        │ Run `cargo test`?│ _ │
│             │                        │ ❯1 Yes 2 Ask 3 No│───┘
└─ 2 need you ┘                        └──────────────────┘
```

### Platform support

amux targets **Unix only — macOS and Linux (Ubuntu) are both first-class**; Windows is out of
scope (the daemon + unix-socket + fork/setsid model is Unix-native). Nothing in the design is
macOS-specific, and two choices actually *favor* Linux: the control/mailbox sockets prefer
`$XDG_RUNTIME_DIR` (set natively on Ubuntu — `/run/user/<uid>`, tmpfs, auto-cleaned on logout;
macOS falls back to `~/.amux/run`), and the "daemonize before starting the tokio runtime"
ordering is the portable Unix rule (mandatory on macOS, harmless on Linux). Parity is
**enforced, not hoped for**, via a CI matrix (`ubuntu-latest` + `macos-latest`) from Phase 0.

**Ubuntu build prerequisites:** a C toolchain for the vendored libgit2 — `build-essential`,
`cmake`, `pkg-config`; TLS uses `rustls` so **no** `libssl-dev`/OpenSSL is needed. (Exact apt
list confirmed during the Phase 0 spike.)

---

## 2. Architecture principles ("strong bones")

These are non-negotiable and shape every module:

1. **One source of truth.** The daemon owns all live state (processes, PTYs, agent state
   machines). Clients are *projections* of daemon state — they hold no authority. This is
   what makes "close the UI, agents keep running, reattach later" correct by construction.
2. **Pure core, I/O at the edges.** `amux-core` is domain logic with no sockets, no PTYs,
   no tokio — just types, the agent state machine, the adapter trait, worktree/config
   operations. It is unit-testable in milliseconds. All process/socket I/O lives in the
   daemon; all rendering in the client.
3. **An explicit, versioned wire protocol.** The daemon↔client boundary is a real API
   (`amux-proto`), not leaked internal types. It has a version handshake so a client and
   daemon can refuse to talk if incompatible.
4. **The agent-CLI boundary is a single trait.** Everything is CLI-agnostic except two
   things: *how you launch/resume a CLI* and *how you derive its status*. Both live behind
   `AgentAdapter`. Adding `codex` later = writing one adapter, touching nothing else. This
   is the discipline grove lacked (CLI specifics smeared across an 11k-line `main.rs`).
5. **Status is a strategy, tiered by fidelity.** Behind the adapter, status comes from the
   best available source: (1) **hooks** (exact, Claude Code), (2) **OSC escape sequences**
   (in-band, agent-agnostic), (3) **screen heuristics** (last resort). Core never knows
   which tier produced a transition.
6. **Bounded everything.** PTY output is buffered with backpressure and ring-buffered
   scrollback caps. No unbounded channels, no unbounded memory. A runaway agent cannot OOM
   the daemon.
7. **Structured concurrency & cancellation.** Every per-agent and per-client task has a
   clear owner and is cancelled deterministically on disconnect/exit. No orphan tasks.

**One deliberate exception — HEAD sessions.** The product's isolation guarantee (each agent in
its own worktree, §1) is intentionally waived for a single opt-in session type: a *HEAD session*
runs an agent in the repo root on `HEAD` with no managed worktree or branch, sharing the user's
live tree. The blast radius is contained by three things: it's a **singleton** per repo; the
"no managed worktree" fact is an **explicit `Workspace::Head`** variant (so worktree logic —
delete/prune, uniqueness — can't silently misfire on it); and its Claude hook settings are written
**out of tree** (via `claude --settings`), so amux writes *nothing* into the live repo. Guarding
against the user's own concurrent edits to that tree is the user's responsibility, not amux's.

Error handling: typed errors (`thiserror`) in `amux-core`/`amux-proto`; `anyhow` at binary
edges. Logging: `tracing` throughout, JSON logs to `~/.amux/log/`.

---

## 3. Workspace layout

A Cargo workspace, four libraries + one shipped binary. Single binary (like `git`) keeps
distribution simple and guarantees the hook bridge is always on `PATH`.

```
amux/
├── Cargo.toml                 # workspace
├── crates/
│   ├── amux-proto/            # wire types + framing + version handshake. NO logic.
│   ├── amux-core/             # domain: Agent, AgentState machine, AgentAdapter trait,
│   │                          #   ClaudeAdapter, StatusSource strategies, worktree, config.
│   │                          #   Pure-ish (git2/fs ok; no tokio/pty/socket). Heavily tested.
│   ├── amux-daemon/           # the runtime: control server, PTY pool, mailbox listener,
│   │                          #   session registry, persistence. Owns all async I/O.
│   └── amux-tui/              # ratatui client: view model, sidebar/main/mini rendering,
│                              #   vt100+tui-term, focus/input routing, keymap.
└── src/
    └── main.rs                # `amux` binary — thin dispatch:
                               #   amux            → TUI client (auto-spawns daemon)
                               #   amux daemon     → run daemon (normally auto-spawned)
                               #   amux hook       → hook→mailbox bridge (used by Claude hooks)
```

Dependency direction (strictly acyclic):

```
main ──▶ amux-tui ──▶ amux-proto ◀── amux-daemon ──▶ amux-core
                          ▲                             │
                          └──────── amux-core ◀─────────┘   (daemon & proto both use core types
                                                              where they cross the wire, via proto DTOs)
```

`amux-core` never depends on daemon/tui/proto. `amux-proto` depends only on serde (+ a few
core value types re-exported as stable DTOs). This keeps the testable heart isolated.

---

## 4. The domain model (`amux-core`)

### 4.1 Identifiers & Agent

```rust
pub struct AgentId(Uuid);        // stable, persisted
pub struct WorkspaceId(Uuid);    // a repo amux is managing

pub struct Agent {
    pub id: AgentId,
    pub name: String,            // display / branch-derived
    pub branch: String,
    pub worktree: PathBuf,
    pub adapter: AdapterKind,    // "claude-code", ...
    pub ai_session_id: Option<String>, // for `claude --resume`
    pub created_at: DateTime<Utc>,
    pub state: AgentState,       // runtime, driven by the state machine
}
```

### 4.2 The state machine (the heart)

State is a small, explicit enum. It is the thing the sidebar renders and the thing every
status tier ultimately produces.

```rust
pub enum AgentState {
    Starting,
    Working,                                   // ● actively processing
    NeedsAttention { kind: AttentionKind, message: Option<String> }, // ⚠ you should look
    Idle,                                      // ○ at prompt, finished, not urgent
    Exited { code: Option<i32> },
    Error { message: String },
}

pub enum AttentionKind { Permission, Question, Info }
```

Transitions are driven by `AgentEvent`s and computed by a pure function so they can be
exhaustively unit-tested:

```rust
pub enum AgentEvent {
    Spawned,
    Signal(StatusSignal),   // from a StatusSource (below)
    ProcessExited(Option<i32>),
}

// pure, total, tested with a table of (state, event) -> state
pub fn next_state(current: &AgentState, event: &AgentEvent) -> AgentState;
```

Key rule: **`NeedsAttention` and `Idle` are distinct.** `Idle` is quiet (○); `NeedsAttention`
is the ⚠ that lights up the sidebar. Claude's hooks give us this distinction for free
(`permission_prompt` → `NeedsAttention{Permission}`, `idle_prompt`/`Stop` → `Idle`).

### 4.3 The adapter boundary

```rust
pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AdapterKind;
    fn capabilities(&self) -> Capabilities;

    /// How to start (or resume) this CLI inside a worktree: command, args, env, cwd.
    fn spawn_spec(&self, ctx: &LaunchContext) -> SpawnSpec;

    /// One-time integration setup in the worktree (e.g. write .claude/settings.json hooks).
    fn prepare_worktree(&self, ctx: &LaunchContext) -> anyhow::Result<()>;

    /// A fresh status source that maps raw signals to state transitions.
    fn status_source(&self) -> Box<dyn StatusSource>;
}

pub struct Capabilities {
    pub structured_status: bool, // hooks/OSC vs heuristic-only
    pub resumable: bool,
    pub summarizable: bool,
}
```

The **status strategy** is separate from the adapter and tiered:

```rust
pub enum RawSignal<'a> {
    HookMailbox(&'a HookEvent),   // parsed JSON from the mailbox socket
    Osc(&'a OscSequence),         // OSC 9/99/777 pulled from the PTY stream
    PtyActivity,                  // bytes are flowing (a hint, not authoritative)
    Tick,                         // periodic, for heuristic timers
    Exited(Option<i32>),
}

pub trait StatusSource: Send {
    /// Given a signal and the current screen (for heuristics), maybe emit a transition.
    fn on_signal(&mut self, sig: RawSignal, screen: &vt100::Screen) -> Option<StatusSignal>;
}
```

- `ClaudeStatusSource`: authoritative on `HookMailbox`; treats `PtyActivity` as a soft
  "still working" heartbeat; ignores screen text entirely. **Exact, no scraping.**
- `HeuristicStatusSource` (fallback for a hookless CLI): consumes `Osc` when present, else
  screen + `Tick` timers (à la the good parts of grove's detector, but isolated and small).

Adding a CLI is: implement `AgentAdapter`, pick/implement a `StatusSource`, register it.
Nothing in the daemon or TUI changes.

### 4.4 Worktree & config services

- `worktree`: create/remove/list worktrees. Port grove's `src/git/worktree.rs` (clean,
  ~205 lines, git2-based) nearly verbatim — it's the one piece of grove worth lifting.
- `config`: global (`~/.amux/config.toml`) + per-repo (`.amux/project.toml`) + keymap.
  Two-level merge like grove, but far fewer knobs.

---

## 5. The daemon (`amux-daemon`, `amux daemon`)

The daemon is a detached, long-lived, single-user process. It is **auto-spawned** by the
first client (tmux-server model): the client connects to the control socket; if absent, it
`fork`+`setsid`+`exec`s `amux daemon` (fully detached so it outlives the launching terminal),
waits for the socket, then connects. You may also run `amux daemon` explicitly.

**Multi-repo.** One global daemon manages agents across many repositories — it does not assume
the launching repo. A repo is **registered** (idempotent, keyed by its canonical path → a stable
`RepoId`) either at daemon start (the launch repo) or by any client on connect (`AddRepo` for its
cwd). The sidebar groups agents under repo headers; `CreateAgent { repo, .. }` targets a known
repo, and `CreateAgentAt { path, .. }` registers-then-creates for a repo given by path. Each repo
owns its own `WorktreeService` (worktree base `~/.amux/worktrees/<repo>-<hash>/`).

**Doctor.** A crash or an out-of-band deletion can leave a worktree that git still tracks but no
live agent holds, wedging its branch as "already checked out". `DoctorRepo { repo }` (the `P` key
in the sidebar, or `amux doctor` from a repo) prunes those orphans — only worktrees **under the
repo's amux base** that no live agent references, and never one with uncommitted changes (those
are reported and spared). It reclaims the branch without dropping to `git worktree prune`.

### 5.1 Sockets (unix domain, `0600`, in `$XDG_RUNTIME_DIR/amux/` or `~/.amux/run/`)

- **Control socket** — clients connect here. Speaks `amux-proto`.
- **Mailbox socket** — Claude Code hooks connect here (via `amux hook`) to push status.

Both are user-only permissioned and live in the user runtime dir.

### 5.2 Session registry & per-agent tasks

```
AgentHandle {
    agent: Agent,
    pty_master, child,                 // portable-pty
    parser: vt100::Parser,             // daemon-side screen (for OSC + snapshots + heuristics)
    scrollback: RingBuffer,            // bounded recent output for late-joining clients
    status: Box<dyn StatusSource>,
    output_tx: broadcast::Sender<Bytes>,   // fan-out raw PTY bytes to subscribed clients
    state: AgentState,
}
```

Per agent, three concerns run as owned tokio tasks:

1. **PTY reader**: read master → (a) feed daemon `vt100::Parser`, (b) scan for OSC, (c) push
   into `scrollback`, (d) `output_tx.send(bytes)`, (e) feed `PtyActivity` to the status source.
2. **Status driver**: folds `RawSignal`s (mailbox events, OSC, activity, ticks, exit) through
   the `StatusSource` → `next_state` → on change, emit `StateChanged` to all clients.
3. **PTY writer**: applies `Input`/`Resize` requests routed from clients.

All three are cancelled together when the agent exits or is deleted.

### 5.3 Rendering model — why the daemon streams *raw bytes*

A PTY has exactly **one** winsize at a time, so there is one authoritative screen. We use
the proven mosh/agentapi-style split:

- Daemon keeps a `vt100::Parser` (for OSC detection, heuristics, and snapshots) **and**
  broadcasts the **raw PTY output bytes** to subscribed clients.
- Each client runs its *own* `vt100::Parser` fed by `[snapshot] ++ [live stream]` and renders
  it with `tui-term`. Because all clients share the PTY's single size, they reconstruct an
  identical screen.
- **Late-join snapshot**: on `SubscribeOutput`, the daemon sends
  `parser.screen().contents_formatted()` (a byte dump that recreates the current screen) then
  the live stream. No infinite history needed.

This keeps the client a near-dumb renderer, avoids a bespoke cell-diff protocol, and makes
multi-client attach fall out naturally.

### 5.4 The mailbox (exact status, no scraping)

The elegant part, and the payoff of the whole status thread:

1. On `prepare_worktree`, `ClaudeAdapter` writes `.claude/settings.local.json` (gitignored)
   with `Notification` + `Stop` hooks whose command is simply **`amux hook`**.
2. At spawn, the daemon injects two env vars into the agent process:
   `AMUX_HOOK_SOCK` (the mailbox path) and `AMUX_AGENT_ID`.
3. When Claude fires a hook, `amux hook` reads the hook JSON from stdin, tags it with the
   agent id, and writes one line to the mailbox socket. The daemon parses it into a
   `HookEvent`, feeds it as `RawSignal::HookMailbox` to that agent's `ClaudeStatusSource`.

Why this is good:
- The hook command depends on **no external tools** (`nc`/`jq`) — just the `amux` binary
  that is already installed.
- Identity + socket path travel through **env**, injected by the daemon — so settings.json
  is static and generic, and this simultaneously solves the **environment-inheritance**
  concern (the daemon controls the full spawn env; agents are launched through a login shell
  so `PATH`, keys, direnv all match a normal terminal).
- Reference plumbing exists: `disler/claude-code-hooks-multi-agent-observability`.

> Spike required (Phase 2): confirm exact hook event names, matchers
> (`permission_prompt` vs `idle_prompt`), and payload keys against the live CLI before
> hard-coding. ~20-minute experiment.

### 5.5 Persistence & restart honesty

- **Client restart**: free. The daemon keeps running; reattach restores everything including
  layout (which agent is in main, which minis are open). This is also what an ungraceful SSH
  disconnect looks like — the daemon is double-forked and `setsid`, so the client's `SIGHUP`
  never reaches it, and agents keep working until you reconnect.
- **Daemon restart / crash**: child processes die with it (they are its children). We persist
  agent metadata + `ai_session_id` to `~/.amux/state.json`, so on restart the daemon offers
  **resume** (`claude --resume <id>`) rather than silently losing work. Full crash-survival
  (re-parentable agents) is explicitly **out of scope v1**; noted as future work.
- **Upgrade / reinstall**: the daemon that binds the control socket arbitrates. `bind_or_detect`
  probes an already-bound socket with a real `Hello`: a **compatible** daemon means this one is
  redundant and refuses to start; an **incompatible** one (a `PROTO_VERSION` bump, or a wedged
  process that won't answer) is SIGTERM'd via its pidfile, awaited, then SIGKILL'd, and we take
  the socket. The client must **never** unlink the socket itself — doing so was how a reinstall
  orphaned the previous daemon, which then kept its PTYs and agent processes alive and unreachable
  forever. Eviction sanity-checks that the pid is an amux process first, because ungraceful
  teardown (an SSH drop, a reboot) leaves stale pidfiles and pids get reused.

---

## 6. The protocol (`amux-proto`)

Length-delimited frames (`tokio_util::codec::LengthDelimitedCodec`). Body encoding:
`postcard`/`bincode` for compact output frames; the handshake carries a protocol version and
both sides refuse mismatches.

> **Implemented contract (proto v4) lives in `crates/amux-proto/src/message.rs`.** It refined the
> sketch below to the **terminal model**: the sidebar lists agents (workspaces), but output is
> keyed by `TerminalId`, not `AgentId`. `SubscribeOutput`/`Unsubscribe` became
> `Attach { terminal, size }`/`Detach { terminal }`; `Input`/`Resize`/`Output`/`OutputSnapshot`
> all carry a `terminal`. A split adds a shell terminal in the same worktree via
> `SpawnShell { terminal, like }`, closed with `CloseTerminal { terminal }`. `AgentInfo` carries
> its `primary_terminal`. The sketch below is kept for the shape of the create/roster half.

```rust
enum ClientMsg {
    Hello { proto_version: u32, client_size: Size },
    ListAgents,
    CreateAgent { branch: String, base: Option<String>, adapter: AdapterKind },
    DeleteAgent { id: AgentId },
    SubscribeOutput { id: AgentId },       // start receiving snapshot+stream
    Unsubscribe { id: AgentId },
    Input { id: AgentId, bytes: Bytes },
    Resize { id: AgentId, size: Size },    // resize-to-slot
    SetLayout(Layout),                     // persist workspace (main + minis + focus)
}

enum DaemonMsg {
    Hello { proto_version: u32 },
    Agents(Vec<AgentDto>),
    AgentAdded(AgentDto),
    AgentRemoved(AgentId),
    StateChanged { id: AgentId, state: AgentState },
    OutputSnapshot { id: AgentId, bytes: Bytes },  // contents_formatted() dump
    Output { id: AgentId, bytes: Bytes },          // live raw stream
    Exited { id: AgentId, code: Option<i32> },
    Error { message: String },
}
```

Efficiency rule: the client **only subscribes to output for agents it is currently showing**
(main + expanded minis). Sidebar rows need `StateChanged` only, never output. This bounds
bandwidth regardless of agent count.

---

## 7. The client (`amux-tui`, `amux`)

### 7.1 View model

A projection of daemon state: `agents: Map<AgentId, AgentDto>` (+ their `AgentState`), and a
`Layout { main: Option<AgentId>, minis: Vec<Mini>, focus: Focus }`. All mutations arrive as
`DaemonMsg`s; the client never invents state.

### 7.2 Layout & rendering

- **Sidebar** (left, fixed width): roster + status glyph + attention preview
  (`api ⚠ perm — cargo test?`, from `NeedsAttention.message`). This is the inbox.
- **Main pane**: the `main` agent's screen via `tui-term` over the client's `vt100::Parser`.
- **Floating minis** (Option A — true overlay): rendered *after/on top of* the main pane,
  anchored bottom-right, added leftward. Implementation: for each mini, `Clear` its Rect then
  draw a `tui-term` `PseudoTerminal`. Because they overlay, the main pane keeps its full
  rectangle underneath.
  - **Unfocused minis collapse to a title-only strip** (`api ▸ perm`) — minimal occlusion;
    also the horizontal-overflow behavior when the bottom fills.
  - **Peek/hide toggle** momentarily clears all minis to reveal what's beneath.

### 7.3 Focus & input routing

Exactly one surface (sidebar / main / a specific mini) holds input focus: highlighted border,
others dimmed but live. The focused terminal receives all keystrokes verbatim (forwarded as
`Input`). A **leader key** always escapes back to navigation; leader+number / tab cycles
focus. Keymap is config-driven and mirrors tmux/grove reflexes where sensible (muscle-memory
reuse is a stated goal).

### 7.4 Resize-to-slot

When an agent enters a slot (main, or a mini expands on focus), the client sends
`Resize { id, size }` for that slot's dimensions. The daemon applies the PTY winsize; Claude
reflows; the momentary reflow is accepted. **Displaced-main = (a)**: opening an agent in main
while one is already there returns the displaced agent to sidebar-only (still running), not
into a mini.

### 7.5 Tiled splits — the main area is tmux-splittable (first-class)

The **main area is not a single pane** — it is a tmux-style split space. You can split it into
tiled panes, each streaming an agent's terminal, and navigate with tmux muscle memory (the
leader is already `Ctrl-B`):

- `Ctrl-B %` split the focused pane vertically · `Ctrl-B "` split horizontally
- `Ctrl-B` + `h`/`j`/`k`/`l` (or arrows) move focus between panes
- `Ctrl-B x` close the focused pane (the agent keeps running; it just leaves the layout)
- open a sidebar-selected agent into the focused pane

This is **distinct from the floating minis** (§7.2): splits are a *tiled workspace* for actively
working several agents side by side; minis are a *transient overlay* for quickly answering a
waiting agent without disturbing that layout. All three coexist — sidebar (inbox) + tiled main
(workspace) + floating minis (attention popups).

**Daemon requirement:** several panes stream at once, so the daemon must support **multiple
simultaneous attachments** — the client subscribes to output for every agent that has a pane,
each tagged by `AgentId`. Phase 1 shipped a single re-targetable stream; multi-attach is the
small change that unlocks splits, and the protocol was always ready for it (`Output`/`Input`/
`Resize` are all per-`AgentId`). The client's `Layout` becomes a **pane tree** rather than
`main: Option<AgentId>`. Plan: `docs/SPLITS.md`.

---

## 8. Build plan (phased vertical slices)

Each phase is end-to-end runnable and independently valuable. We build the *spine* first
(the hardest plumbing, proven with nothing else attached), then layer capability.

**Testing is per-phase, not deferred (see §12):** every phase ships its own unit + integration
tests and automates its exit-criteria as an acceptance test that must be green in CI on both
OSes before the phase counts as done. Two shared fixtures make this possible — a **scriptable
fake-agent** (Phase 1) and an **injected clock** + **headless test client** (Phase 0) — so all
I/O and timing is deterministic under test.

### Phase 0 — The spine (no features, just proof the pipe works)
- Workspace + all four crates scaffolded.
- `amux-proto`: `Hello` handshake, framing, `Input`/`Resize`/`Output`/`OutputSnapshot`.
- `amux-daemon`: auto-spawn+detach, control socket, spawn **one** PTY (a plain `$SHELL`),
  reader/writer tasks, raw-byte broadcast, snapshot on subscribe.
- `amux-tui`: attach, render one full-screen PTY via `vt100`+`tui-term`, forward keys, resize.
- **CI matrix from day one:** GitHub Actions on `ubuntu-latest` + `macos-latest` — build, test,
  `cargo fmt --check`, `clippy -D warnings`; cross-platform breakage fails the build.
- **Exit criteria:** `amux` launches the daemon, opens a shell in a PTY, you type in it, resize
  works, you can quit the client and reconnect to the same live shell. This validates the
  entire daemon↔client↔PTY↔render loop — the riskiest plumbing — in isolation, **green on both
  Ubuntu and macOS in CI.**

### Phase 1 — Agents, worktrees, sidebar
- `amux-core`: `Agent`, `AgentState` + `next_state` (with unit tests), worktree service
  (ported), config.
- `AgentAdapter` + `ClaudeAdapter` (`spawn_spec` + `prepare_worktree`; status stubbed to
  `Working`/`Idle` on activity for now).
- Daemon: `CreateAgent` (make worktree → spawn claude in PTY), registry, `ListAgents`,
  `DeleteAgent`, metadata persistence.
- TUI: sidebar roster, select agent → show in main, create/delete, sidebar↔main focus.
- **Exit criteria:** a usable grove-like tool — spin up N Claude agents on branches, switch
  between them in the main pane, persistent sidebar. (Minus real status, minus minis.)

### Phase 2 — Exact status via the mailbox
- `amux hook` subcommand (stdin→mailbox bridge).
- Daemon: mailbox socket; inject `AMUX_HOOK_SOCK`+`AMUX_AGENT_ID`; login-shell spawn env.
- `ClaudeAdapter::prepare_worktree` writes the hook settings; `ClaudeStatusSource` maps
  `HookMailbox` → states.
- **Spike first:** verify hook names/matchers/payloads against the live CLI.
- TUI: sidebar shows live `● ○ ⚠` + attention-reason previews.
- **Exit criteria:** the sidebar is a true live inbox; permission-vs-idle is exact; zero
  screen-scraping. Verified by driving a real permission prompt and watching the sidebar flip.

### Phase 3 — Minis (floating live terminals)
- Client layout engine: `Layout` with main + N minis + focus; focus cycling; per-slot
  resize-to-slot; multi-subscription (output for every visible surface).
- Floating overlay rendering (bottom-right, leftward), collapse-to-header (unfocused +
  overflow), peek/hide toggle.
- Commands: open-in-mini, open-in-main (displaced=(a)), promote mini→main, close mini.
- `SetLayout` persistence; restore on attach.
- **Exit criteria:** the full picture — heads-down in main, pull two waiting agents into
  minis, answer both without leaving main, reattach later to the same layout.

### Phase 4 — Hardening to production
- OSC status tier + capability-driven UI affordances.
- Resilience: client reconnect/backoff; daemon-restart resume via `ai_session_id`;
  graceful agent-exit handling.
- Config + keymap file (muscle-memory mapping), theming, `amux doctor`.
- **Robustness layer** (the per-phase unit + integration tests already ride each earlier phase
  — see §12): soak (N agents × M min, no fd/mem/task leak), bounds (flood → scrollback ring
  stays bounded), property/fuzz on the proto codec + vt100 feed (`proptest`/`cargo-fuzz`),
  `loom` on hand-rolled sync, `miri` over the daemonize `unsafe`, and a panic-mid-render test
  asserting the terminal is restored.
- **Stretch:** a second adapter (codex or gemini) via `HeuristicStatusSource` — proves the
  boundary holds under a real second CLI.

---

## 9. Key risks & how the design absorbs them

| Risk | Mitigation |
|---|---|
| Hook payload shape differs from research | Isolated in `ClaudeStatusSource`; Phase-2 spike verifies before hard-coding; heuristic fallback exists |
| Daemon crash loses agents | Metadata + `ai_session_id` persisted → resume; full re-parenting deferred but the seam is there |
| Terminal reflow jank on resize | Accepted + localized (resize-to-slot); minis collapse to headers to minimize churn |
| Mini overlay hides main's input line | Collapse-to-header default + peek toggle; minis are small and corner-anchored |
| Feature creep back toward grove's breadth | Adapter boundary + "differentiators only" scope discipline; integrations are explicitly out |
| Fragile screen-scraping | Designed out for Claude (hooks); only the fallback tier scrapes, and it's isolated |

---

## 10. Open questions (to resolve as we go)

- Keymap defaults: mirror grove, mirror tmux, or a fresh scheme? (Leaning: tmux-flavored.)
- Output frame encoding: `postcard` vs `bincode` (benchmark in Phase 0).
- Scrollback: how much per agent, and do we expose a copy-mode in v1?
- Mini sizing: fixed dimensions vs proportional to terminal size.
- `amux doctor` scope (tmux-free, but checks git version, claude on PATH, socket dir perms).

---

## 11. Verified dependency set & load-bearing gotchas (spike, 2026-07)

Pinned, mutually-compatible set (verified vs crates.io/docs.rs). Note we ride the **ratatui
0.30 line** (grove is on 0.29); tui-term 0.3.4 tracks 0.30 and re-exports the vt100 it pins.

| crate | pin | note |
|---|---|---|
| portable-pty | 0.9 | PTY ownership |
| vt100 | 0.16.2 | use tui-term's re-export to avoid skew; `contents_formatted()` = snapshot, `contents_diff()` = incremental |
| tui-term | 0.3.4 | tracks ratatui 0.30; `PseudoTerminal::new(&screen)` |
| ratatui | 0.30.2 | `ratatui::init()`/`restore()` (raw+altscreen+panic hook) |
| crossterm | 0.29 | features = ["event-stream"] (ratatui only pulls it as dev-dep) |
| tokio / tokio-util | 1.52 / 0.7 | tokio-util features = ["codec"] for LengthDelimitedCodec |
| **postcard** | 1.1 | wire encoding — **NOT bincode 3.0**, which dropped serde-by-default (a real trap) |
| serde / bytes / anyhow | 1 / 1 / 1 | |
| thiserror | 2 | v2 is semver-major; pin `2.x` |
| git2 | 0.21 | `vendored-libgit2` (no system libgit2 needed); pin exact |
| uuid / clap / directories | 1 / 4 / 6 | |
| nix | 0.30 | daemonize (double-fork + setsid) |

**Gotchas to encode in the code (each is a real footgun):**
1. **PTY reader is blocking** → a dedicated `std::thread` per PTY, bridged to async via a
   channel; never read it on the tokio reactor. `take_writer()` is single-shot; clone the
   reader before moving `master`. **Drop `pair.slave` immediately after `spawn_command`** or
   the reader never sees EOF on child exit (silent hang). Keep `master` alive for the process
   lifetime; drop it to unblock/kill on shutdown. **Portability:** on child exit the master read returns a
   clean **EOF (`Ok(0)`) on macOS but `EIO` (an `Err`) on Linux** — the reader loop must treat
   *both* `Ok(0)` and an I/O error as "PTY closed" and exit, or Linux logs a spurious error on
   every agent that quits.
2. **Daemonize before tokio.** `amux daemon` = parse args → daemonize (nix double-fork +
   setsid) → *then* build the runtime. Forking after a multi-threaded tokio runtime exists is
   UB (macOS aborts outright). Never wrap the daemonize in `#[tokio::main]`. Portable rule —
   applies to Linux too.
3. **Input encoder:** copy tui-term's `examples/smux.rs::handle_pane_key_event` (complete
   KeyEvent→bytes map). Add **DECCKM** handling for full fidelity: when the child enables
   application-cursor-key mode, arrows must send `ESC O A/B/C/D`, not `ESC [ A/B/C/D` — read
   that mode off the vt100 `Screen`. Required for vim / full-screen agent TUIs.
4. **Socket path:** prefer `$XDG_RUNTIME_DIR/amux/amux.sock` (logind sets it on Ubuntu:
   `/run/user/<uid>`, 0700) but **verify it's owned by you + mode 0700 before trusting it**.
   When unset (cron, `su`, non-login shells, non-lingering user services) fall back to
   `~/.amux/run/` created 0700 with an ownership check + a logged warning. Mind the **`sun_path`
   limit (~108 bytes Linux / 104 macOS)** — keep the path short. `dirs`/`directories` return
   `None` when the var is unset, so the fallback is our code, not theirs.

**Ubuntu build prereqs (confirmed):** just `sudo apt-get install -y build-essential pkg-config`
— **no cmake** (git2 0.21's `libgit2-sys` builds vendored libgit2 with `cc`, not cmake; zlib
comes vendored via `libz-sys`), and **no OpenSSL/`libssl-dev`** given the decision below.

**Decision — git2 stays local-only; network git shells out to the `git` CLI.** git2's TLS is
libgit2's *own* OpenSSL backend (rustls does **not** apply to it) and is pulled in only by the
`https` feature. amux uses git2 for local plumbing only — worktrees, status, refs, branches —
so we do **not** enable `https`/`ssh` → **zero OpenSSL, no `libssl-dev`**. Any networked git
(fetch/push, later) runs the user's `git` CLI as a subprocess, which also reuses their existing
git credentials/auth instead of re-implementing it. `libssl-dev` would only ever be needed if
we enabled git2 networking directly, which we won't.

---

## 12. Testing strategy

Guiding principle: the architecture is *designed* for testability — **pure core, I/O at the
edges** — so the bulk of correctness is proven in fast deterministic unit tests, and the scary
parts (PTYs, sockets, the forked daemon, the TUI) are pinned by a small number of integration
and end-to-end tests built on shared fixtures. **Testing is per-phase, not deferred:** each
phase ships its own tests and automates its exit-criteria as acceptance tests that must be
**green in CI on both Ubuntu and macOS** before the phase is done.

### Shared test infrastructure (built early)
- **Scriptable fake-agent** (Phase 1) — a tiny stand-in binary for `claude` that follows a
  script ("print X, wait, fire hook `permission_prompt`, print Y, exit 0"). Drives the entire
  daemon pipeline deterministically with no API key, no network, no nondeterminism. The single
  highest-leverage test asset in the project.
- **Injected clock** (Phase 0) — all time flows through a `Clock` trait (no ambient
  `Utc::now()`), so idle timers / heuristics / debounces are deterministic under test.
- **Headless test client** (Phase 0) — scripts daemon interactions and asserts on the event
  stream.
- **CI matrix** — the whole suite on `ubuntu-latest` + `macos-latest`; this is what catches the
  EIO-vs-EOF, socket-path, and daemonize divergences between the platforms.

### The pyramid
**Unit — the bulk, pure, millisecond-fast:**
- `next_state(state, event)` — an **exhaustive (state × event) transition table**. The heart of
  correctness; the thing grove never had.
- `StatusSource`s — golden fixtures of real hook JSON → assert emitted states; the heuristic
  source fed captured terminal snapshots (seed the corpus from grove's detector tests) →
  assert states.
- Layout engine — `(layout, action) → layout` for open-in-main/mini, displaced = (a), focus
  cycling, overflow-collapse.
- Adapters — `spawn_spec` command/args/env correctness; `prepare_worktree` writes the exact
  `settings.json`.
- `amux-proto` — encode/decode round-trip via **property tests** (`proptest`); version mismatch
  is rejected at handshake.
- config + keymap parse/merge.

**Integration — real I/O, still deterministic:**
- Protocol over a real socketpair: exchange, backpressure, disconnect handling.
- **Daemon + fake-agent** (the big one) — spawn the daemon, create a scripted fake agent,
  assert state transitions, output broadcast, mailbox delivery, and snapshot-on-subscribe. This
  one harness covers PTY read/close (incl. Linux `EIO`), the status pipeline, and socket fan-out.
- vt100 resync — parse → `contents_formatted()` → re-parse → assert identical screen (proves
  late-join snapshot correctness); fuzz arbitrary bytes to prove the parser/renderer never panic.
- Worktree ops against a temp git repo (`tempfile` + git2).
- Persistence & reattach — create agents, drop the client, reconnect, assert layout + state
  restored; daemon-restart → resume metadata intact.

**End-to-end / acceptance — few, the phase exit-criteria, automated:**
- TUI rendering via ratatui's `TestBackend` + `insta` snapshots: sidebar rows / status glyphs /
  attention previews, mini overlay geometry (bottom-right, leftward), collapse-to-header, focus
  highlight, peek/hide. **Yes, the TUI is deterministically testable** — assert the rendered
  buffer.
- Input encoder — KeyEvent→bytes tables incl. **DECCKM** app-cursor mode; round-trip the bytes
  back through vt100 to confirm the child sees the right thing.
- Each phase's exit-criteria scenario, scripted against the fake-agent, green in CI.

### Robustness layer (Phase 4, on top of the per-phase tests)
- **Bounds/leak:** flood an agent → assert the scrollback ring + channels stay bounded (the
  "a runaway agent can't OOM the daemon" invariant becomes an actual test).
- **Soak:** N agents churning for M minutes → assert no fd / memory / task leak, clean shutdown.
- **Property/fuzz:** `proptest` on the proto codec + vt100 feed; `cargo-fuzz` on the parsers.
- **Concurrency:** stress under `--test-threads`; `loom` for any hand-rolled sync; `miri` over
  the daemonize `unsafe` fork path.
- **Terminal-restore safety:** a test that panics mid-render and asserts the guard restores
  cooked mode — a terminal left in raw mode on panic is unacceptable.

### Discipline
- Exit criteria are **automated acceptance tests**, not manual checklists.
- The pure core is **TDD-lean** — transition tables before implementation.
- **Every bug earns a regression test**, especially in status detection: a future Claude UI
  change that breaks us should fail a fixture in CI, not a user in the field. That is the exact
  rot that sank grove's approach, closed here by construction.
- Coverage tracked (`cargo-llvm-cov`) as a signal, never a target.
```

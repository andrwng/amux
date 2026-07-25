# Task at launch — design

## Goal

Let `n` dispatch an agent **with its task**, so starting three agents on three known tasks costs
three trips through one sidebar form instead of three round trips through the main area.

Today the task can only be typed into a live pane, which means the agent's boot time **serializes**
the whole batch: agent two's worktree isn't created until you've opened agent one, waited for the
CLI to draw its prompt box, typed, and navigated back. Cheap dispatch is what fills the sidebar
inbox (`DESIGN.md` §1, pillar 1) — the inbox only earns its keep when several agents are working at
once, and right now reaching that state is expensive enough to discourage it.

## Behavior

`n` gains a second field, mirroring the existing `N` two-field form (`Tab`/`Up`/`Down` switch,
`Esc` cancels, `Enter` submits):

```
┌ new agent in amux ──────────────────┐
│ branch  fix-flaky-test              │
│ task    the config_home test fails  │
│         on macOS — fix it           │
│ Tab switches · Enter create · Esc   │
└─────────────────────────────────────┘
```

- **Branch first, task second.** Branch names stay the user's to choose — no slug derivation, no
  pre-fill, so field order matches the muscle memory `N` already established.
- **An empty task is exactly today's behavior.** Enter on the branch alone submits with
  `prompt: None`; the agent launches idle at its prompt box. The feature is strictly additive —
  conversational sessions are untouched.
- **A non-empty task is passed to the CLI at spawn**, so the agent is already working by the time
  you've typed the next one.
- **Branch still cancels on empty** (unchanged): no branch, no agent, whatever the task field says.
- **Single-line, paste-friendly.** The task field is a single-line buffer like the branch field,
  stripping control characters. Bracketed paste already lands a long task in one shot
  (`on_paste` routes into the active prompt buffer), which covers the paragraph case without a
  multi-line editor.

## The prompt through the stack

Each layer keeps its existing shape; nothing learns anything CLI-specific outside the adapter.

| Layer | Change |
|---|---|
| `amux-proto` | `ClientMsg::CreateAgent { repo, branch }` gains `prompt: Option<String>`. **Bump `PROTO_VERSION` 16 → 17** + codec round-trip test. |
| `amux-core` | `LaunchContext` gains `prompt: Option<&str>`, beside `resume`. |
| `ClaudeAdapter::spawn_spec` | Appends the task as the **final positional argument** — `claude [flags] <task>`. Verified against the installed CLI: `claude [options] [command] [prompt]`, interactive by default (`-p/--print` is the non-interactive mode we do *not* want). |
| `amux-daemon` | `create` threads the prompt into `LaunchContext`, and persists it on the `Agent` record (see below). |
| `amux-tui` | `key_creating` becomes two-field (copy `key_creating_repo`); the create-prompt renderer grows a row. |

Argv is the whole mechanism. No writing into the PTY after spawn — that would need the daemon to
guess when the CLI is ready to accept input, which is exactly the fragile screen-state coupling
`DESIGN.md` §5.4 designs out.

## Resume interaction — the sharp edge

`spawn_spec` must pass the prompt **only on a spawn that is not continuing an existing
conversation.** Replaying it onto `claude --resume <id>` would inject the original task as a fresh
turn into a conversation that already contains it — silent, confusing, and destructive to a long
session.

Concretely: `resume: Some(_)` wins, `prompt` is dropped. They are mutually exclusive in
`spawn_spec`, and that is the first test written.

The prompt is nonetheless **persisted on the agent record**, because the reverse case is real:
`ai_session_id` only arrives from a hook payload, so an agent that dies before its first hook fires
(a bad flag, a crash at boot — precisely the dispatch-time failure) resumes today with
`resume: None` and lands in an empty session with its task lost. Persisting the prompt means such a
resume relaunches onto the same task. Rule, stated once: **the prompt is passed on any spawn that
isn't resuming a conversation.**

## Decisions / non-goals

- **No auto-open change.** An earlier sketch had "task given → stay in the sidebar; no task → open
  it." Dropped: creation already selects-without-opening (`AgentAdded` sets `sidebar_sel`), which is
  *already* the right behavior for dispatch, and changing the no-task path would alter existing
  muscle memory to save one keystroke.
- **`N` (`CreateAgentAt`) is unchanged.** It's the once-per-repo "register a repo by path" flow, not
  the dispatch flow; a third field there earns nothing. Revisit only if it itches.
- **`H` (HEAD sessions) gets no task field.** A HEAD session is "help me with what I'm doing right
  now" — inherently conversational, and a singleton per repo.
- **No multi-line editor, no `$EDITOR` hand-off.** Single line plus paste covers the cases; both
  alternatives are more TUI machinery than the payoff justifies (YAGNI).
- **Permission posture is out of scope, and is the sequel.** Three dispatched agents block on three
  permission prompts a minute later. This feature makes starting N agents cheap; making N agents
  *run* unattended is a separate question (`--permission-mode`, with worktree isolation as the
  containment that makes a looser mode defensible). Worth deciding before concluding dispatch pays
  off in practice.

## Testing

- **Adapter goldens** (`amux-core`, pure): a `LaunchContext` with a prompt yields `claude <task>`
  with the task last; the same context with `resume: Some(id)` yields `--resume <id>` and **no**
  task; a `None` prompt is byte-identical to today's output (the regression guard for the additive
  claim). Non-`claude` commands (`$SHELL`/`cat` in tests) are unaffected.
- **Codec round-trip** for `CreateAgent` with `Some`/`None` prompt, plus the version bump.
- **TUI**: `Tab` moves between branch and task; `Enter` with an empty task sends `prompt: None`;
  `Enter` with an empty *branch* still cancels. `TestBackend` snapshot of the two-field form.
- **Daemon**: create-with-prompt persists it across a `save`/`load_state` round trip, and a resume
  with no `ai_session_id` relaunches with the task.

## Verification (observed, not asserted)

- Dispatch a real agent with a task and watch it start working without any typing into the pane —
  the claim the whole feature rests on.
- Resume an agent mid-conversation and confirm the original task is **not** replayed as a new turn.
- Dispatch three in a row without leaving the sidebar; confirm all three worktrees exist and the
  rows go `● working` independently, rather than one-at-a-time.

## Gate before implementation

Changing `ClientMsg::CreateAgent` bumps `PROTO_VERSION`, which `CLAUDE.md` lists under "always stop
and confirm." This spec is the proposal; the bump needs an explicit go-ahead before code.

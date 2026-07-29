# amux

A terminal UI for multiplexing AI coding agents in isolated git worktrees — a persistent
sidebar of agents, one focused main window, and several floating live mini-terminals you can
answer without losing your place.

Status: pre-alpha. Unix only; macOS and Linux are both first-class. You need `git` and an agent
CLI (`claude`) on your `PATH`.

## Quickstart

```sh
cargo run   # the TUI — auto-spawns the daemon on first run
```

Agents keep running when you quit the client: a background daemon owns the terminals, so `Ctrl+Q`
detaches rather than kills. Start amux again and you're back where you left off, layout included.

### Install

To get an `amux` on your `PATH` instead of running from the source tree, install the binary from
a clone with cargo:

```sh
cargo install --path .   # builds and installs `amux` into ~/.cargo/bin
```

Then run `amux` from any repo (make sure `~/.cargo/bin` is on your `PATH`). On Ubuntu you need a C
toolchain first: `sudo apt-get install -y build-essential pkg-config`. Re-run the same command to
upgrade after pulling — a cargo-installed binary does not update itself.

### The three kinds of panels

```
┌ agents ────────┐┌ ⋯ feature-x ──────────────────────────────┐
│ ▾ myrepo (3)   ││                                            │
│ ▸▌⋯ *feature-x ││   main area — the active agent's workspace │
│   ⚠ feature-y  ││   (tiled panes: agent + shell splits)      │
│   ○ feature-z  ││                                            │
│                ││            ┌ ⚠ feature-y ─┐┌ ○ feature-z ─┐│
│                ││            │  mini        ││  mini        ││
│                ││            └──────────────┘└──────────────┘│
└────────────────┘└────────────────────────────────────────────┘
 status bar — always shows the shortcuts for whatever is focused
```

- **Sidebar** (left): every agent, grouped by repo, needs-attention first and then most-recently-
  opened. Each row shows a state glyph (colored by state), a cyan `▌` bar when the agent has unread
  output, a `*` when it's currently visible, and how long ago you last opened it. Status is exact,
  not scraped — Claude reports it through hooks, so "waiting on you" really means waiting on you.
  This is a selection list: keys here are *commands*. On a narrow terminal it collapses to an icon
  rail.
- **Main area** (center): the **active agent's workspace** — a tmux-style tree of tiled panes.
  It starts as the agent's terminal; splits spawn a `$SHELL` in the same worktree. Each agent
  owns its own layout: opening another agent swaps the whole main area, and layouts are saved
  and restored — across restarts of the TUI, of amux itself, and of upgrades. Split shells are
  respawned fresh in the same worktree, since their processes don't survive a restart.
- **Minis** (floating, bottom-right): small live terminals overlaid on the main area, one per
  agent, each showing that agent's primary terminal — answer another agent's prompt without
  losing your place. A mini can be minimized to a status-only strip, and the whole row can be
  hidden temporarily ("peek").

**The input model in one sentence:** in the sidebar keys are commands; in a pane or mini every
keystroke is typed into that terminal, except three reserved chords — `Ctrl+Q` (quit),
`Ctrl+h/j/k/l` (move focus), and `Ctrl+B` (the tmux-style command prefix).

### Agents and worktrees

`n` in the sidebar creates an agent: amux makes a git worktree for the branch you name, launches
the agent CLI in it, and adds it to the roster. `Tab` moves to a second field where you can give
the agent its task up front — it starts working immediately, so you can dispatch several in a row
without leaving the sidebar. Leave the task empty to get an agent idling at its prompt instead. Agents are durable — if one exits (or the daemon
restarts), it goes **suspended**, not away: the worktree and conversation id survive, and `r`
resumes it in place. `d` is the only destructive command, and it asks before discarding
uncommitted work.

`h` starts a **HEAD session** instead: an agent in the repo root on your current checkout, with no
worktree and no branch. One per repo, for when you want an agent in the tree you're already in.

`N` and `H` are the by-path versions of `n` and `h`: they prompt for a directory, so you can reach a
repo that isn't in the sidebar yet without leaving amux.

### Navigation

Focus is **spatial**: picture the layout above and move with `Ctrl+h/j/k/l`. Between panes it
moves through the splits; off the left edge it lands in the sidebar; off the bottom or right
edge it drops into the minis row; from a mini, left/right walk the row and up/left climb back
into the panes (or the sidebar when there are none). Clicking a pane or mini also focuses it.

Two deliberate exceptions:

- In the **sidebar**, `Ctrl+j`/`Ctrl+k` jump to the next/previous **unread** agent (there is
  nothing above or below the sidebar to move into).
- A vim-like app that announces it handles splits gets `Ctrl+h/j/k/l` passed through; when it
  hits its own edge it hands navigation back to amux, so one motion works across both. See
  [`contrib/README.md`](contrib/README.md) to install the vim plugin.

### Shortcuts

Everywhere: `Ctrl+Q` quit · `Ctrl+h/j/k/l` move focus · `Ctrl+B` command prefix.

**Sidebar**

| Key | Action |
| --- | --- |
| `j`/`k` or arrows | move the selection |
| `Enter` or `l` | open the agent in the main area |
| `m` | open the agent as a mini |
| `n` | new agent in the selected repo (prompts for a branch, and an optional task to start it on) |
| `N` | new agent in a repo by path (prompts for directory + branch; `Tab` switches fields) |
| `h` | new HEAD session in the selected repo (no worktree, no branch) |
| `H` | new HEAD session in a repo by path (prompts for a directory) |
| `d` | delete the agent (asks `y/n` before discarding uncommitted work) |
| `r` | resume an exited agent |
| `P` | doctor: prune the repo's orphaned worktrees |
| `Ctrl+j`/`Ctrl+k` | jump to next/previous unread agent |
| `q` | quit |

**Panes and minis — `Ctrl+B`, then:**

| Key | Action |
| --- | --- |
| `%` / `"` | split left‑right / top‑bottom (spawns `$SHELL` in the same worktree) |
| `x` | close the focused pane or mini (a shell pane's shell is killed; the agent's own terminal just hides — the agent keeps running) |
| `r` or `H`/`J`/`K`/`L` | resize mode: `hjkl`/arrows nudge the split, `Esc`/`Enter` done |
| `[` | scroll (copy) mode — see below |
| `Tab` | jump to the next agent with unread output and open it |
| `1`–`9`, `0` | open that numbered sidebar agent (`0` is the tenth; the digits appear on the rows while the prefix is armed) |
| `-` | open the previous agent — tmux's last-window |
| `Ctrl+<key>` | send a literal control key to the focused pane (e.g. `Ctrl+B Ctrl+L`, since bare `Ctrl+h/j/k/l` navigate) |
| `Enter` | *(mini only)* promote the mini into the main area |
| `-` | *(mini only)* minimize/restore it to a status strip |
| `z` | peek: hide/show the whole minis row |

`Cmd`+digit does the same jump without the prefix, on terminals that natively report the `Cmd`
modifier. amux deliberately doesn't force the Kitty keyboard protocol on to get it (that breaks
Shift and slows typing), so if your terminal doesn't send it, use the prefix form.

**Scroll mode** (`Ctrl+B [`, vi keys like tmux): `j`/`k` line, `Ctrl+U`/`Ctrl+D` half page,
`PgUp`/`PgDn` page, `g`/`G` top/bottom, `q`/`Esc`/`Enter` back to live. The status bar shows how far
back you are and how far back you can go. History belongs to the agent, not to your window: it
survives switching agents and reconnecting, so you can open a long-running agent and scroll back
through output from before you got there. Unavailable in panes running a full-screen app (there's no
scrollback on the alternate screen).

**Mouse:** click focuses; the wheel scrolls back through history (wheel down to the bottom returns to
live), or is forwarded to apps that take the mouse (vim, less, Claude); drag to select and copy from a
single pane (via OSC 52, works over SSH); hold `Shift` to bypass amux and use your terminal's native
selection.

## Command line

`amux` with no arguments is the TUI. The rest are occasional-use:

```sh
amux doctor                             # prune orphaned worktrees in this repo, and show where amux stores things
amux daemon --stop                      # stop the daemon and every agent it owns
amux --profile <blue|green|yellow|red>  # start the TUI with a border/accent color profile (default: blue),
                                         # to tell concurrent sessions apart
```

## Configuration

There is no config file until you write one, and amux runs fine without it. When you do want one,
it goes at `$XDG_CONFIG_HOME/amux/config.toml` — else `~/.config/amux/config.toml`, on macOS as
well as Linux. It holds two settings today:

```toml
# ~/.config/amux/config.toml — every key is optional
root = "~/somewhere-else/.amux"   # the amux home (default: ~/.amux). A leading ~ is expanded.
profile = "green"                 # border/accent color: blue (default), green, yellow, or red.
                                   # Overridden by the --profile flag.
```

**`root`** is where everything amux owns lives, and it's the answer to "how do I keep this off my
home directory / small disk?":

| Under the root | What it is |
| --- | --- |
| `worktrees/<repo>-<hash>/<branch>/` | one worktree per agent, kept out of your project tree |
| `state.json` | the agent roster, layouts, and minis — what survives a daemon restart |
| `head-settings/` | hook settings for HEAD sessions, written here so your live repo is untouched |
| `run/` | the daemon's control + hook sockets and pidfile |

Two details worth knowing:

- **A configured `root` is self-contained.** Its sockets move under `<root>/run` instead of
  `$XDG_RUNTIME_DIR`, so two different roots are two fully independent amux instances that never
  collide. (With no `root` set, sockets prefer `$XDG_RUNTIME_DIR/amux`, else `~/.amux/run`.)
- **Mistakes are loud, never silent.** A malformed file or an unknown key (say `roott`) is a
  startup error rather than a shrug back to defaults — otherwise you'd believe your data had moved
  when it hadn't. Setting `root` does not migrate anything: point it at a new path and amux starts
  empty there, so move the old directory yourself if you want your agents to come along.

Run `amux doctor` to see which config file was loaded and where the amux home resolved to.

## Development

Enable the shared git hooks once per clone so your commits stay CI-green:

```sh
./.githooks/setup   # sets core.hooksPath to .githooks
```

The `pre-commit` hook runs `cargo fmt --check` and `cargo clippy -D warnings` — the
fast half of [CI](.github/workflows/ci.yml). Architecture and the reasoning behind it live in
[`docs/DESIGN.md`](docs/DESIGN.md).

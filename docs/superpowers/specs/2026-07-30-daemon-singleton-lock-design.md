# Lock-file singleton for the amux daemon

**Status:** approved (design)
**Date:** 2026-07-30

## Problem

Multiple `amux` daemon processes ended up running for the same amux-home, and a departing
one corrupted the live one's state. The daemon is meant to be a **singleton per home** (one
`amuxd.sock` + `amuxd.pid` under the runtime dir; `--repo` only pre-registers a repo).

Root cause, traced through the lifecycle code:

1. **Startup arbitration is a 2-second socket probe.** A new daemon binds `amuxd.sock`;
   on `AddrInUse` it probes the incumbent and classifies it `Compatible` / `Incompatible`
   / `Dead` (`server.rs` `bind_or_detect`/`probe`). A live, *compatible* incumbent that is
   simply **slow to answer within 2 s under load** is misclassified `Incompatible` and
   **evicted**.
2. **Eviction's grace is shorter than a real shutdown.** `evict` SIGTERMs, waits
   `EVICT_GRACE = 5 s`, then SIGKILLs and rebinds. But a daemon shuts down its agents one
   at a time (SIGTERM → grace → SIGKILL each), which can exceed 5 s — so the evicted daemon
   is **still alive when the successor rebinds** → two daemons coexist.
3. **Shutdown cleanup has no ownership check.** On exit the daemon unlinks `amuxd.sock`,
   `amuxd-hooks.sock`, and `amuxd.pid` unconditionally (`lib.rs`). A slow, evicted daemon
   finishing *after* its successor rebound deletes the **successor's** `amuxd.pid`/socket
   (the empty-`amuxd.pid` corruption observed).
4. **No lock / no atomic claim.** Arbitration is entirely socket-probe + heuristic, so it's
   also racy if two clients start at once.

## Design

No wire change, no `PROTO_VERSION` bump — the lock is local to a host/home.

### 1. `amuxd.lock` advisory lock (the singleton guarantee)

Add `amuxd.lock` in the runtime dir (beside `amuxd.sock`/`amuxd.pid`; a new path in
`amux-core::paths`, same resolution as the others). The daemon opens it and takes a
**non-blocking exclusive `flock`** (`nix::fcntl::flock`, `FlockArg::LockExclusiveNonblock`
— `nix` is already a dependency; `flock` works on Linux and macOS) early in startup, and
**holds the `File`/fd for its entire lifetime** (stored so it is not dropped — dropping
releases the lock). The OS releases the lock on *any* process exit, including SIGKILL or an
SSH drop, so there is no stale-lock failure mode.

Acquisition happens **after daemonizing**, in the final long-lived process (the double-fork
grandchild), so the lock lives in the daemon that actually serves — not the short-lived
fork parent.

### 2. Lock-gated startup arbitration (`bind_or_detect`, `server.rs`)

Replace "probe → evict on anything non-compatible" with:

| `flock(LOCK_EX\|LOCK_NB)` | State | Action |
|---|---|---|
| **Acquired** | No live daemon (lock was free) | Remove any stale `amuxd.sock`, bind, write `amuxd.pid`, run. |
| **Held**, probe → our proto | Healthy compatible incumbent | Exit cleanly ("already running"); the client connects to it. |
| **Held**, probe → *different* proto | Confirmed-incompatible incumbent | Evict (SIGTERM → grace → SIGKILL); death frees the lock; acquire, remove stale socket, bind, run. |
| **Held**, probe inconclusive (timeout / no answer / connect error) | Alive but slow/busy — or wedged | **Back off and exit — do NOT kill.** |

**The decisive rule:** the daemon evicts **only on a *confirmed* proto mismatch, never on a
timeout.** This removes the root-cause trigger — a busy-but-healthy daemon holds the lock
and a slow probe no longer reads as "incompatible," so it can't be mis-evicted under load.

**Accepted tradeoff:** a genuinely *wedged* compatible daemon (alive, holding the lock,
never answering) is **not** auto-replaced. Recovery is manual: `amux daemon --stop` (or
`kill`). This is deliberate — silently killing a live daemon under load is the bug we're
removing; auto-replacing a wedged one is a rarer need with a manual escape hatch.

Eviction (the confirmed-mismatch row) reuses the existing `evict` SIGTERM→grace→SIGKILL +
`looks_like_amux` machinery; after the incumbent dies the lock is free, so the new daemon
acquires it (poll `LOCK_NB` until it succeeds) before binding.

The client (`client.rs`) is unchanged in shape: it still tries to handshake and, failing,
spawns `amux daemon`. A spawned daemon that finds a healthy incumbent (lock held, back off)
just exits; the client's existing retry loop then reconnects to the incumbent.

### 3. Ownership-checked shutdown cleanup (`lib.rs`)

Before unlinking `amuxd.sock`, `amuxd-hooks.sock`, and `amuxd.pid` on shutdown, **re-read
`amuxd.pid` and unlink only if it still names my pid.** So a departing (e.g. evicted, slow)
daemon can never delete a successor's files. Belt-and-suspenders on top of the lock. (The
`amuxd.lock` file itself is left in place — it's an empty lock target; the flock, not the
file, is the state, and it's released by fd close on exit.)

### 4. `stop()` hardening (`lib.rs`)

`amux daemon --stop` reads `amuxd.pid` and SIGTERMs it with no liveness/identity guard.
Add the same `looks_like_amux(pid)` + `alive(pid)` checks `evict` already uses, so a stale
pidfile naming a **reused/unrelated** pid can't cause `stop()` to signal an innocent
process. If the pid is dead or not an amux process, report "no running amux daemon" and
remove the stale pidfile.

## Testing

Integration/unit tests with a temp runtime dir (mirroring the existing
`Registry::with_state` daemon tests in `crates/amux-daemon/tests/session.rs`):

1. **Singleton:** with the lock held (first daemon up), a second `bind_or_detect` / daemon
   start resolves to "already running" and does **not** bind a second socket.
2. **Release on exit:** after the lock holder exits, a new daemon acquires the lock and
   binds cleanly.
3. **Ownership-checked cleanup:** a shutdown path with `amuxd.pid` naming a *different* pid
   leaves that pidfile (and socket) untouched.
4. **`stop()` guard:** `stop()` with `amuxd.pid` naming a non-amux / dead pid refuses to
   signal it and reports no running daemon.

Confirmed-mismatch eviction already has coverage via the existing incompatible-daemon path;
extend only if a gap shows.

## Non-goals

- **`amux doctor` daemon reaper** — considered and declined for this change; the lock +
  ownership-checked cleanup make strays structurally unlikely, and `--stop` covers the
  wedged case.
- **Any wire-protocol change / `PROTO_VERSION` bump** — the lock is local.
- **Per-repo daemons** — the singleton is per-home, unchanged.
- **Auto-replacing a wedged (alive, non-responding) compatible daemon** — see the accepted
  tradeoff above.

# Daemon singleton lock — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the amux daemon a true singleton-per-home via an advisory `flock` on `amuxd.lock` (held for the daemon's lifetime, released by the OS on any exit), so load/timeout/race conditions can no longer spawn duplicate daemons or let a departing daemon stomp a successor's state.

**Architecture:** A new `amuxd.lock` in the runtime dir. The daemon takes a non-blocking exclusive `flock` after daemonizing; if it can't, a live daemon already owns the home and it stands down (evicting **only** on a *confirmed* protocol mismatch, never on a timeout). Shutdown cleanup and `amux daemon --stop` are hardened to act only on a pidfile that still names the acting process.

**Tech Stack:** Rust; `nix` 0.30 (`fs` feature, already enabled) for `flock`; tokio. Tests: `cargo test`.

## Global Constraints

- **No wire change / no `PROTO_VERSION` bump** — the lock is local to a host/home.
- **`amux-core` stays pure** — the only core change is a pure path method (`RuntimePaths::lock()`); no new dep, no I/O in core.
- **Evict only on a *confirmed* different `PROTO_VERSION`.** A probe timeout / closed connection / unreachable socket is **stand-down**, never eviction. (This is the whole point — a busy healthy daemon must not be killed under load.)
- **`flock` acquired after `daemonize()`**, in the final long-lived process; the `Flock<File>` guard is held for the daemon's whole lifetime (dropping it releases the lock).
- Handle the non-block "held" errno with a **guard** (`e == Errno::EWOULDBLOCK || e == Errno::EAGAIN`), not an `|`-pattern — on Linux the two are equal and an or-pattern trips the unreachable-pattern lint under `-D warnings`.
- `tracing` only; no `println!`/`eprintln!`/`dbg!` in library crates **except** `stop()`'s existing user-facing `println!` (it's a CLI action path, pre-existing); no `unwrap()`/`expect()` in library code (tests may unwrap).
- **Definition of done (all green, observed):** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace`. (Note: `pty::tests::scroll_step_serves_history_a_window_at_a_time` is a pre-existing flake that passes on retry in the full suite.)
- **Commit trailer:** `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. One logical change per commit; every task compiles the whole workspace (plain `git commit`, no `--no-verify`).

## File Structure

- `crates/amux-core/src/paths.rs` — add `RuntimePaths::lock()`.
- `crates/amux-daemon/src/server.rs` — 3-way `Probe`, rewritten `probe()`, new `acquire_and_bind` + `Singleton`, `pub(crate)` `alive`/`looks_like_amux`; remove old `bind_or_detect`.
- `crates/amux-daemon/src/lib.rs` — wire `acquire_and_bind` into `run_blocking` (stand-down → clean exit, hold the lock guard); ownership-checked cleanup helper; `stop()` hardening; exports.
- `crates/amux-daemon/tests/session.rs` — singleton integration test.

---

### Task 1: `RuntimePaths::lock()` (amux-core)

**Files:**
- Modify: `crates/amux-core/src/paths.rs` (`impl RuntimePaths`, after `mailbox()` ~line 40; tests below)

**Interfaces:**
- Produces: `RuntimePaths::lock(&self) -> PathBuf` → `self.dir.join("amuxd.lock")`.

- [ ] **Step 1: Write the failing test**

Add to `paths.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn lock_path_sits_beside_the_socket() {
        let paths = RuntimePaths { dir: std::path::PathBuf::from("/run/amux") };
        assert_eq!(paths.lock(), std::path::PathBuf::from("/run/amux/amuxd.lock"));
        assert_eq!(paths.socket(), std::path::PathBuf::from("/run/amux/amuxd.sock"));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p amux-core lock_path_sits_beside_the_socket`
Expected: FAIL — `no method named lock`.

- [ ] **Step 3: Add the method**

In `impl RuntimePaths` (after `mailbox()`):

```rust
    /// The advisory lock file that enforces one daemon per home. Held (via `flock`) for the
    /// daemon's lifetime; the OS releases it on any exit, so it needs no cleanup.
    pub fn lock(&self) -> PathBuf {
        self.dir.join("amuxd.lock")
    }
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p amux-core lock_path_sits_beside_the_socket`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/amux-core/src/paths.rs
git commit -m "Add the runtime lock path for the daemon singleton"
```

---

### Task 2: Lock-gated singleton arbitration (amux-daemon)

**Files:**
- Modify: `crates/amux-daemon/src/server.rs` (`Probe` enum ~40-48; `bind_or_detect` ~57-73 → replace; `probe` ~87-109; `alive`/`looks_like_amux` ~172, 187 → `pub(crate)`; keep `rebind`)
- Modify: `crates/amux-daemon/src/lib.rs` (`run_blocking` ~40-117; `pub use` line 18)
- Test: `crates/amux-daemon/tests/session.rs`

**Interfaces:**
- Consumes: `RuntimePaths::lock()` (Task 1).
- Produces: `pub struct Singleton { pub lock: nix::fcntl::Flock<std::fs::File>, pub listener: tokio::net::UnixListener }`; `pub async fn acquire_and_bind(lock_path: &Path, socket: &Path, pidfile: &Path) -> Result<Option<Singleton>>` (`None` = stand down, a live daemon owns the home). `pub(crate) fn alive(pid: i32) -> bool`, `pub(crate) fn looks_like_amux(pid: i32) -> bool`.

- [ ] **Step 1: Write the failing integration test**

In `crates/amux-daemon/tests/session.rs`, add (it already imports `amux_daemon`, `tempfile`, uses `#[tokio::test]`):

```rust
/// The advisory lock makes the daemon a true singleton: a second daemon cannot claim the home
/// while the first holds the lock (it stands down without binding or evicting), and once the
/// holder exits the lock frees so a fresh daemon can claim it.
#[tokio::test]
async fn daemon_singleton_lock_prevents_a_second_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let first = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .unwrap();
    assert!(first.is_some(), "first daemon claims the singleton");

    // While the first holds the lock, a second stands down — it must NOT bind a second socket
    // or evict the live holder (the probe can't handshake the non-serving first, so it reads as
    // unreachable, which is stand-down, not eviction).
    let second = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .unwrap();
    assert!(second.is_none(), "second daemon stands down while the lock is held");

    // The holder exits → lock frees → a fresh daemon claims it.
    drop(first);
    let third = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .unwrap();
    assert!(third.is_some(), "a new daemon claims the singleton after the holder exits");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p amux-daemon --test session daemon_singleton_lock_prevents_a_second_daemon`
Expected: FAIL — `acquire_and_bind` / `Singleton` don't exist.

- [ ] **Step 3: Rework `Probe` and `probe()` in `server.rs`**

Replace the `Probe` enum (lines ~40-48) with a 3-way classification that distinguishes a *confirmed* mismatch from mere silence:

```rust
/// What answered (or failed to answer) on the socket a live daemon holds.
enum Probe {
    /// A daemon speaking our exact protocol version — a healthy incumbent; stand down.
    Compatible,
    /// A confirmed *different* protocol: a `Hello` with another version, an unexpected frame, or
    /// an undecodable one (postcard is positional, so an older layout decodes as garbage). The
    /// reinstall case — evict and replace it.
    Incompatible,
    /// Alive (it holds the lock) but not answering the handshake in time, or the socket isn't
    /// accepting yet: busy/mid-start/wedged. Stand down — never kill a live daemon for being slow.
    Unreachable,
}
```

Rewrite `probe` (lines ~87-109) — same connect+handshake, new mapping:

```rust
async fn probe(socket: &Path) -> Probe {
    let Ok(stream) = UnixStream::connect(socket).await else {
        return Probe::Unreachable; // socket not accepting — owner mid-start or wedged
    };
    let mut framed = Framed::new(stream, ClientCodec::new());
    let exchange = async {
        framed
            .send(ClientMsg::Hello { proto_version: PROTO_VERSION })
            .await
            .ok()?;
        framed.next().await
    };
    match tokio::time::timeout(PROBE_TIMEOUT, exchange).await {
        Ok(Some(Ok(DaemonMsg::Hello { proto_version }))) if proto_version == PROTO_VERSION => {
            Probe::Compatible
        }
        // Silence past the timeout, or a clean close with no frame: alive but not answering.
        Err(_) | Ok(None) => Probe::Unreachable,
        // A different-version `Hello`, an unexpected frame, or an undecodable one: different protocol.
        _ => Probe::Incompatible,
    }
}
```

- [ ] **Step 4: Replace `bind_or_detect` with `acquire_and_bind` + `Singleton`**

Delete `bind_or_detect` (lines ~50-73). Keep `rebind` (~77-80). Add (near the top of the arbitration section):

```rust
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// A daemon's exclusive claim on a home: the lifetime `flock` guard plus the bound control
/// socket. Dropping `lock` releases the advisory lock.
pub struct Singleton {
    pub lock: Flock<std::fs::File>,
    pub listener: UnixListener,
}

/// How many times we'll try to evict a confirmed-incompatible incumbent before giving up.
const MAX_EVICTIONS: u32 = 3;

/// Acquire the per-home singleton lock and bind the control socket.
///
/// Returns `Some(Singleton)` when this process now owns the home, or `None` when a live daemon
/// already owns it and this process should stand down (the client will connect to the incumbent).
/// A *confirmed* protocol mismatch (the reinstall case) is evicted and replaced; a busy/slow/
/// unreachable incumbent is left alone — killing a live daemon for being slow is the bug we're
/// removing.
pub async fn acquire_and_bind(
    lock_path: &Path,
    socket: &Path,
    pidfile: &Path,
) -> Result<Option<Singleton>> {
    let mut evictions = 0u32;
    loop {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(lock_path)
            .with_context(|| format!("open lock file {}", lock_path.display()))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                // No live daemon holds the home. Any socket on disk is a crash leftover.
                let listener = rebind(socket).context("bind control socket")?;
                return Ok(Some(Singleton { lock, listener }));
            }
            Err((_file, e)) if e == Errno::EWOULDBLOCK || e == Errno::EAGAIN => {
                match probe(socket).await {
                    Probe::Compatible => {
                        tracing::info!("an amux daemon is already running; standing down");
                        return Ok(None);
                    }
                    Probe::Unreachable => {
                        tracing::info!(
                            "a daemon holds the lock but isn't answering yet; standing down"
                        );
                        return Ok(None);
                    }
                    Probe::Incompatible => {
                        evictions += 1;
                        if evictions > MAX_EVICTIONS {
                            bail!("could not evict the incompatible daemon after {evictions} tries");
                        }
                        evict(pidfile).await; // its death releases the lock; loop to claim it
                        continue;
                    }
                }
            }
            Err((_file, e)) => {
                return Err(anyhow::anyhow!(e)).context("flock the daemon lock file")
            }
        }
    }
}
```

Change `alive` (line ~172) and `looks_like_amux` (line ~187) from `fn` to `pub(crate) fn` (for `stop()` in Task 4). Leave `evict` private.

- [ ] **Step 5: Wire it into `run_blocking` and update exports (`lib.rs`)**

Change the export (line 18) from `pub use server::{bind_or_detect, serve};` to:

```rust
pub use server::{acquire_and_bind, serve, Singleton};
```

In `run_blocking`, compute the lock path with the others (after `let pidfile = pid_file(&paths.dir);`, ~line 60):

```rust
    let lock = paths.lock();
    let runtime_dir = paths.dir.clone();
```

Replace the start of the `block_on` body (the `bind_or_detect` call + socket perms, lines ~74-77) with:

```rust
        let Some(server::Singleton { lock: _lock, listener }) =
            server::acquire_and_bind(&lock, &socket, &pidfile).await?
        else {
            tracing::info!("another amux daemon owns {}; exiting", runtime_dir.display());
            return Ok(());
        };
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).ok();
```

`_lock` is bound (not `_`) so the guard lives to the end of the async block — the flock is held for the daemon's whole life and released when the block returns (or the process dies). The rest (mailbox bind, pidfile write, registry, serve) is unchanged. (The ownership-checked cleanup replacing lines ~112-114 is Task 3 — leave those three `remove_file` lines as-is for now; the workspace still compiles.)

- [ ] **Step 6: Run the singleton test + the crate suite**

Run: `cargo test -p amux-daemon --test session daemon_singleton_lock_prevents_a_second_daemon`
Expected: PASS (takes ~2s — the second call waits out `PROBE_TIMEOUT` before standing down).
Run: `cargo test -p amux-daemon`
Expected: PASS (existing tests unaffected; `scroll_step_serves_history_a_window_at_a_time` may need a retry).

- [ ] **Step 7: Commit**

```bash
git add crates/amux-daemon/src/server.rs crates/amux-daemon/src/lib.rs crates/amux-daemon/tests/session.rs
git commit -m "Make the daemon a singleton via an advisory lock file"
```

(Body: explain the lock-gated arbitration, evict-only-on-confirmed-mismatch, and stand-down-on-unreachable; note it fixes mis-eviction of a busy daemon under load.)

---

### Task 3: Ownership-checked shutdown cleanup (amux-daemon)

**Files:**
- Modify: `crates/amux-daemon/src/lib.rs` (cleanup at ~lines 112-114; add helper + test)

**Interfaces:**
- Produces: `fn cleanup_if_owner(pidfile: &Path, others: &[&Path], my_pid: u32)`.

- [ ] **Step 1: Write the failing test**

Add a `#[cfg(test)] mod tests` to `lib.rs` (or extend one if present):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_only_when_the_pidfile_names_me() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("amuxd.pid");
        let socket = tmp.path().join("amuxd.sock");
        std::fs::write(&socket, b"").unwrap();

        // Pidfile names a DIFFERENT process — a successor owns these files; leave them.
        std::fs::write(&pidfile, "999999").unwrap();
        cleanup_if_owner(&pidfile, &[&socket], 12345);
        assert!(socket.exists(), "must not delete a successor's socket");
        assert!(pidfile.exists(), "must not delete a successor's pidfile");

        // Pidfile names me — I own them; remove them.
        std::fs::write(&pidfile, "12345").unwrap();
        cleanup_if_owner(&pidfile, &[&socket], 12345);
        assert!(!socket.exists(), "owner removes its socket");
        assert!(!pidfile.exists(), "owner removes its pidfile");
    }
}
```

(`tempfile` is a dev-dependency of `amux-daemon` — it's used in `tests/session.rs`; confirm it's under `[dev-dependencies]`, add it there if the unit test can't see it.)

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p amux-daemon --lib cleanup_only_when_the_pidfile_names_me`
Expected: FAIL — `cannot find function cleanup_if_owner`.

- [ ] **Step 3: Add the helper and use it**

Add to `lib.rs`:

```rust
/// Remove the daemon's runtime files, but only if `pidfile` still names `my_pid`. A daemon that
/// has been superseded (evicted, or slow to shut down) must never delete its successor's socket
/// and pidfile — that leaves the live daemon with a corrupted, unfindable identity.
fn cleanup_if_owner(pidfile: &Path, others: &[&Path], my_pid: u32) {
    let owner = std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if owner != Some(my_pid) {
        tracing::info!(
            "pidfile {} names {:?}, not me ({my_pid}); leaving runtime files in place",
            pidfile.display(),
            owner
        );
        return;
    }
    for p in others {
        std::fs::remove_file(p).ok();
    }
    std::fs::remove_file(pidfile).ok();
}
```

Replace the three unconditional `remove_file` lines (~112-114) with:

```rust
        cleanup_if_owner(&pidfile, &[&socket, &mailbox], std::process::id());
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p amux-daemon --lib cleanup_only_when_the_pidfile_names_me`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/amux-daemon/src/lib.rs
git commit -m "Only clean up daemon runtime files when the pidfile still names us"
```

---

### Task 4: Harden `amux daemon --stop` (amux-daemon)

**Files:**
- Modify: `crates/amux-daemon/src/lib.rs` (`stop()` ~lines 120-133; add `stop_at` helper + test)

**Interfaces:**
- Consumes: `server::alive`, `server::looks_like_amux` (Task 2, now `pub(crate)`).
- Produces: `fn stop_at(pidfile: &Path) -> Result<()>`.

- [ ] **Step 1: Write the failing test**

Add to `lib.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn stop_refuses_a_stale_or_dead_pid_and_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("amuxd.pid");

        // A pid that is definitely dead (spawn a trivial child and reap it).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let dead = child.id();
        std::fs::write(&pidfile, dead.to_string()).unwrap();

        let err = stop_at(&pidfile).unwrap_err();
        assert!(
            err.to_string().contains("no running amux daemon"),
            "stop must refuse a dead pid, got: {err}"
        );
        assert!(!pidfile.exists(), "stop clears the stale pidfile");
    }

    #[test]
    fn stop_errors_when_no_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        let err = stop_at(&tmp.path().join("amuxd.pid")).unwrap_err();
        assert!(err.to_string().contains("no running amux daemon"), "got: {err}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p amux-daemon --lib stop_refuses_a_stale_or_dead_pid_and_clears_it stop_errors_when_no_pidfile`
Expected: FAIL — `cannot find function stop_at`.

- [ ] **Step 3: Extract + harden `stop_at`, keep `stop()` as the resolver**

Replace `stop()` (lines ~120-133) with:

```rust
/// Stop a running daemon: SIGTERM the pid in the pidfile, but only after confirming it is alive
/// and actually an amux process — a stale pidfile (a SIGKILL/SSH-drop leftover) can name a dead
/// or reused pid, and we must not signal an unrelated process.
fn stop_at(pidfile: &Path) -> Result<()> {
    let contents = std::fs::read_to_string(pidfile)
        .map_err(|_| anyhow!("no running amux daemon (no pidfile at {})", pidfile.display()))?;
    let pid: i32 = contents.trim().parse().context("parse pidfile")?;
    if !server::alive(pid) || !server::looks_like_amux(pid) {
        std::fs::remove_file(pidfile).ok();
        return Err(anyhow!(
            "no running amux daemon (stale pidfile named pid {pid}; removed it)"
        ));
    }
    kill(Pid::from_raw(pid), Signal::SIGTERM).context("signal the daemon")?;
    println!("stopped amux daemon (pid {pid})");
    Ok(())
}

/// Stop a running daemon by SIGTERM (pid from the pidfile under the resolved runtime dir).
pub fn stop() -> Result<()> {
    let paths = amux_core::paths::RuntimePaths::resolve()?;
    stop_at(&pid_file(&paths.dir))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p amux-daemon --lib stop_refuses_a_stale_or_dead_pid_and_clears_it stop_errors_when_no_pidfile`
Expected: PASS. (The dead-pid path short-circuits on `!alive` before any signal, so no process is ever signalled by the test — safe even if `ps` is absent.)

- [ ] **Step 5: Commit**

```bash
git add crates/amux-daemon/src/lib.rs
git commit -m "Guard amux daemon --stop against a stale or reused pid"
```

---

### Task 5: Full Definition-of-Done gate

**Files:** none (verification only).

- [ ] **Step 1: Format** — `cargo fmt --all -- --check` (expect clean).
- [ ] **Step 2: Clippy** — `cargo clippy --workspace --all-targets -- -D warnings` (expect no warnings; watch the errno guard doesn't become an or-pattern).
- [ ] **Step 3: Build** — `cargo build --workspace --all-targets` (expect success).
- [ ] **Step 4: Test** — `cargo test --workspace` (expect all pass, including the singleton, cleanup, and stop tests). If `pty::tests::scroll_step_serves_history_a_window_at_a_time` fails, re-run once — it's a pre-existing flake unrelated to this change (it touches no daemon-lifecycle code).
- [ ] **Step 5: Manual confirmation (runtime changed).** Per project memory, do NOT `cargo run` amux from an agent worktree (hits the live daemon). For the user / a throwaway `AMUX_HOME`: with a daemon running, start a second `amux` — confirm exactly one `amux daemon` process remains (`ps` shows one), and `~/…/amux/run/amuxd.lock` exists. Kill the daemon with SIGKILL; confirm a fresh `amux` starts a new daemon cleanly (lock auto-released). Then `amux daemon --stop` with no daemon running prints "no running amux daemon".

---

## Self-Review

**Spec coverage:**
- `amuxd.lock` path → Task 1. ✓
- Lifetime `flock`, acquired post-daemonize, held via guard → Task 2 (`Singleton.lock`, `_lock` in `run_blocking`). ✓
- Lock-gated arbitration; evict only on confirmed proto mismatch; stand down on Compatible/Unreachable → Task 2 (`acquire_and_bind`, 3-way `Probe`). ✓
- Client unchanged (stands-down daemon exits, client reconnects) → Task 2 (no client edit; `run_blocking` returns `Ok(())` on stand-down). ✓
- Ownership-checked cleanup → Task 3. ✓
- `stop()` hardening (liveness + `looks_like_amux`, clears stale pidfile) → Task 4. ✓
- No wire change / core purity / no new dep (`nix` already present) → Global Constraints + Task 1 (pure path). ✓
- Tests: lock path (T1), singleton + release (T2), ownership cleanup (T3), stop guard (T4). ✓

**Placeholder scan:** none — all code literal.

**Type consistency:** `acquire_and_bind(&Path,&Path,&Path) -> Result<Option<Singleton>>`, `Singleton{lock: Flock<File>, listener: UnixListener}`, `Probe{Compatible,Incompatible,Unreachable}`, `cleanup_if_owner(&Path,&[&Path],u32)`, `stop_at(&Path)->Result<()>`, `alive`/`looks_like_amux` now `pub(crate)`. Export line updated to match (`bind_or_detect` removed). Consistent across tasks.

**Compiles per task:** T1 adds a method (no consumers break). T2 replaces `bind_or_detect` and updates its only caller (`run_blocking`) + the export in the same commit. T3 and T4 are self-contained `lib.rs` edits whose new helpers are used in the same commit. No `--no-verify` anywhere.

**Watch item:** the errno "held" arm MUST be a guard (`e == Errno::EWOULDBLOCK || e == Errno::EAGAIN`), not `Errno::EWOULDBLOCK | Errno::EAGAIN` — on Linux they're equal and the or-pattern is an unreachable-pattern error under `-D warnings`. Called out in Global Constraints and Task 2 Step 4.

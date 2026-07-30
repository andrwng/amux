//! Eviction-path integration tests against the lock model.
//!
//! `acquire_and_bind` must evict ONLY a *confirmed*-incompatible incumbent (the reinstall case —
//! the whole reason eviction exists) and must leave a compatible or merely-unresponsive one
//! alone. This file also pins the two safety guards inside `evict()` that stop it from ever
//! signalling the wrong process, and the crash-leftover (stale socket, no lock holder) reclaim
//! path.
//!
//! Simulating "a live daemon holds the flock" faithfully needs a *real* process holding it: an
//! in-test `Flock` guard can't be released by killing a decoy pid, because the lock belongs to
//! the open file description that took it, not to whatever pid a test happens to write into the
//! pidfile. So the tests that need eviction to actually *succeed* (freeing the lock for a
//! successor) spawn `fake_daemon` — a `[[bin]]` in this crate (`src/bin/fake_daemon.rs`,
//! auto-discovered by Cargo) that really opens the lock, really answers the socket a chosen way,
//! and really dies on `SIGTERM`. `CARGO_BIN_EXE_fake_daemon` is set by Cargo for integration
//! tests of a package that declares that bin target, so the path needs no guessing.
//!
//! The two safety-guard tests don't need the subprocess: since `evict()` never actually signals
//! in either case, the lock never frees regardless, so it's enough to hold the flock via a second
//! file descriptor *inside this test's own process* (advisory locks are scoped to the open file
//! description, not the process, so this is a real conflict) and confirm `acquire_and_bind` gives
//! up after `MAX_EVICTIONS` rather than ever touching the guarded pid.

use std::path::{Path, PathBuf};
use std::time::Duration;

use amux_proto::{ClientCodec, ClientMsg, DaemonMsg, ServerCodec, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

/// Whether `pid` still exists. Signal 0 probes without delivering anything.
fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Wait for `pid` to disappear (it is reaped asynchronously by a background thread — see
/// `spawn_fake_daemon`).
fn wait_gone(pid: i32) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Absolute path to the `fake_daemon` helper binary, set by Cargo because this package declares
/// it as a `[[bin]]` target.
fn fake_daemon_exe() -> &'static str {
    env!("CARGO_BIN_EXE_fake_daemon")
}

/// A symlink to the fake-daemon binary, named `amux`, so `ps -o comm=` reports an amux process —
/// `evict`'s `looks_like_amux` sanity check must pass for the eviction test to actually reach
/// `kill`. (See `amux-daemon/src/server.rs`'s `looks_like_amux` doc for why that check exists.)
fn amux_named(dir: &Path) -> PathBuf {
    let link = dir.join("amux");
    if !link.exists() {
        std::os::unix::fs::symlink(fake_daemon_exe(), &link).expect("symlink fake_daemon as amux");
    }
    link
}

/// Spawn the fake daemon (see its module doc for `mode`'s meaning), reaped by a background thread
/// so it can never become a zombie — a zombie still answers `kill(pid, 0)`, which would make
/// `alive()` lie.
fn spawn_fake_daemon(dir: &Path, lock: &Path, socket: Option<&Path>, mode: &str) -> i32 {
    let exe = amux_named(dir);
    let mut cmd = std::process::Command::new(&exe);
    cmd.env("AMUX_FAKE_LOCK", lock).env("AMUX_FAKE_MODE", mode);
    match socket {
        Some(s) => cmd.env("AMUX_FAKE_SOCKET", s),
        None => cmd.env_remove("AMUX_FAKE_SOCKET"),
    };
    let child = cmd.spawn().expect("spawn fake daemon");
    let pid = child.id() as i32;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    pid
}

/// Block until a connection to `socket` succeeds — the fake daemon has bound and is accepting.
/// Needed because `probe()` tries the connect exactly once: calling `acquire_and_bind` before the
/// socket exists would read as `Unreachable` instead of exercising the real handshake.
async fn wait_for_socket(socket: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake daemon never bound its socket");
}

/// Serve a rejection `DaemonMsg::Error` to every connection — an in-process stand-in for an
/// incompatible daemon's socket, paired with a `Flock` held separately in the same test (used by
/// the two safety-guard tests, which never need the real subprocess since `evict()` refuses to
/// act on either of them and the lock never actually needs to free).
fn spawn_incompatible_responder(socket: &Path) {
    let listener = UnixListener::bind(socket).expect("bind fake incompatible socket");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut framed = Framed::new(stream, ServerCodec::new());
                let _ = framed.next().await;
                let _ = framed
                    .send(DaemonMsg::Error {
                        message: "protocol mismatch".into(),
                    })
                    .await;
                std::future::pending::<()>().await;
            });
        }
    });
}

/// The core regression eviction exists for: a confirmed-incompatible incumbent (the reinstall
/// case) is terminated and replaced, not left running unreachably with its PTYs.
#[tokio::test]
async fn incompatible_daemon_is_evicted_and_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let pid = spawn_fake_daemon(tmp.path(), &lock, Some(&socket), "incompatible");
    wait_for_socket(&socket).await;
    std::fs::write(&pidfile, pid.to_string()).unwrap();

    let singleton = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .expect("a confirmed-incompatible incumbent must be evicted, not fatal")
        .expect("eviction frees the lock for this process to claim");
    drop(singleton);

    assert!(
        wait_gone(pid),
        "the evicted daemon's process must be gone — this is the orphan bug eviction fixes"
    );
}

/// The inverse of the (removed) old "unresponsive daemon is evicted" test: silence past the probe
/// timeout is now a reason to stand down, never to evict. A wedged-but-alive daemon must survive.
#[tokio::test]
async fn an_unresponsive_incumbent_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let pid = spawn_fake_daemon(tmp.path(), &lock, Some(&socket), "silent");
    wait_for_socket(&socket).await;
    std::fs::write(&pidfile, pid.to_string()).unwrap();

    let outcome = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .expect("standing down must not be an error");
    assert!(
        outcome.is_none(),
        "an unresponsive incumbent means stand down, not eviction"
    );
    assert!(
        alive(pid),
        "silence must never be evicted — killing a live daemon for being slow is the bug this reverses"
    );

    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    wait_gone(pid);
}

/// The compatible case: a second daemon stands down and leaves the first alone, untouched.
#[tokio::test]
async fn a_compatible_incumbent_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let pid = spawn_fake_daemon(tmp.path(), &lock, Some(&socket), "compatible");
    wait_for_socket(&socket).await;
    std::fs::write(&pidfile, pid.to_string()).unwrap();

    let outcome = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .expect("a compatible incumbent must not be an error");
    assert!(outcome.is_none(), "a compatible incumbent means stand down");
    assert!(
        alive(pid),
        "a compatible daemon must never be evicted — that would kill a working session"
    );

    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    wait_gone(pid);
}

/// A socket file left behind by a crash (no lock holder) is reclaimed outright, without probing
/// or signalling anything — nobody holds the lock, so this is not eviction at all.
#[tokio::test]
async fn stale_socket_file_is_reclaimed() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    // Binding and dropping leaves the socket file on disk with nothing behind it.
    let dead = UnixListener::bind(&socket).unwrap();
    drop(dead);

    let singleton = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile)
        .await
        .expect("a stale socket must not error")
        .expect("nobody holds the lock, so this process claims it outright");

    // The rebound socket really is ours: a client can connect and handshake.
    let registry = amux_daemon::Registry::new(Box::new(
        amux_core::adapter::ClaudeAdapter::with_command(vec!["cat".into()]),
    ));
    tokio::spawn(amux_daemon::serve(singleton.listener, registry));
    let stream = UnixStream::connect(&socket).await.expect("connect");
    let mut client = Framed::new(stream, ClientCodec::new());
    client
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await
        .unwrap();
    match client.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => assert_eq!(proto_version, PROTO_VERSION),
        other => panic!("expected Hello from the rebound socket, got {other:?}"),
    }
}

/// Eviction must never signal the evicting process itself, however the pidfile got that way. The
/// lock is held in-process (see the module doc), so it never frees; the observable proof of the
/// guard is that `acquire_and_bind` gives up after `MAX_EVICTIONS` rather than signalling us —
/// and that this test is still here to say so.
#[tokio::test]
async fn own_pid_in_the_pidfile_is_never_signalled() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    let _held = Flock::lock(file, FlockArg::LockExclusive).unwrap();
    spawn_incompatible_responder(&socket);

    std::fs::write(&pidfile, std::process::id().to_string()).unwrap();

    let result = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile).await;
    assert!(
        result.is_err(),
        "evict() refusing to touch our own pid means the lock never frees; acquire_and_bind must \
         give up rather than hang forever or succeed some other way"
    );
    assert!(
        alive(std::process::id() as i32),
        "the clearest proof: this process is still here to check"
    );
}

/// The pid-reuse guard: a pidfile naming a live process that is **not** amux must not be
/// signalled. Ungraceful teardown (an SSH drop, a reboot) routinely leaves stale pidfiles behind,
/// and pids get reused — SIGTERMing an unrelated process would be far worse than the orphan.
#[tokio::test]
async fn a_reused_pid_belonging_to_another_program_is_not_signalled() {
    let tmp = tempfile::tempdir().unwrap();
    let lock = tmp.path().join("amuxd.lock");
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    let _held = Flock::lock(file, FlockArg::LockExclusive).unwrap();
    spawn_incompatible_responder(&socket);

    // A live process that is emphatically not a daemon of ours.
    let bystander = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn bystander");
    let pid = bystander.id() as i32;
    std::thread::spawn(move || {
        let mut child = bystander;
        let _ = child.wait();
    });
    std::fs::write(&pidfile, pid.to_string()).unwrap();

    let result = amux_daemon::acquire_and_bind(&lock, &socket, &pidfile).await;
    assert!(
        result.is_err(),
        "looks_like_amux must reject the bystander, so eviction never happens and the lock never \
         frees"
    );
    assert!(alive(pid), "an unrelated process must never be signalled");

    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
}

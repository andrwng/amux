//! Daemon eviction: a reinstall must **replace** the running daemon, not orphan it.
//!
//! Before this, the client unlinked the control socket and spawned a new daemon whenever the
//! handshake failed, leaving the old one alive forever with its PTYs and its agent processes —
//! eight of them accumulated on the author's machine over 13 days. `bind_or_detect` now probes
//! the socket with a real `Hello` and, when the answer is incompatible, terminates the daemon
//! named by the pidfile before binding.

use std::path::Path;
use std::time::Duration;

use amux_core::adapter::ClaudeAdapter;
use amux_daemon::{bind_or_detect, serve, Registry};
use amux_proto::{ClientCodec, ClientMsg, DaemonMsg, ServerCodec, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

/// Whether `pid` still exists. Signal 0 probes without delivering anything.
fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// A long-lived stand-in for the previous daemon's process, with a thread parked in `wait()` so
/// it is reaped the instant it dies.
///
/// The reaper is the point: a signalled child that nobody waits on stays a zombie, and a zombie
/// still answers `kill(pid, 0)` — so without this, `alive()` could never go false and the test
/// would assert on a liveness signal that means nothing. A real daemon is detached (double-fork +
/// `setsid`) and reaped by init, which is the behavior this reproduces.
///
/// It is spawned through a symlink **named `amux`** because eviction sanity-checks the pid's
/// command before signalling (a stale pidfile's pid may have been reused). Using a plain `sleep`
/// here would exercise the guard instead of the eviction.
fn spawn_sacrificial_daemon(dir: &Path) -> i32 {
    let child = std::process::Command::new(amux_named_sleep(dir))
        .arg("30")
        .spawn()
        .expect("spawn sacrificial process");
    let pid = child.id() as i32;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    assert!(alive(pid), "the sacrificial process should be running");
    pid
}

/// A symlink to the real `sleep` binary, named `amux`, so `ps -o comm=` reports an amux process.
fn amux_named_sleep(dir: &Path) -> std::path::PathBuf {
    let link = dir.join("amux");
    if !link.exists() {
        let out = std::process::Command::new("sh")
            .args(["-c", "command -v sleep"])
            .output()
            .expect("locate sleep");
        let target = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(!target.is_empty(), "no `sleep` on PATH");
        std::os::unix::fs::symlink(&target, &link).expect("symlink sleep as amux");
    }
    link
}

/// Wait for `pid` to disappear (it is reaped asynchronously by the thread above).
fn wait_gone(pid: i32) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Serve `answer` (or nothing at all, when `None`) to one client on `socket`, standing in for a
/// daemon from an older build whose reply we cannot use.
fn spawn_incompatible_daemon(socket: &Path, answer: Option<DaemonMsg>) {
    let listener = UnixListener::bind(socket).expect("bind fake daemon");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let answer = answer.clone();
            tokio::spawn(async move {
                let mut framed = Framed::new(stream, ServerCodec::new());
                let _ = framed.next().await; // their Hello
                if let Some(msg) = answer {
                    let _ = framed.send(msg).await;
                }
                // Then hold the connection open (a wedged daemon) until the test ends.
                std::future::pending::<()>().await;
            });
        }
    });
}

/// The core regression: an incompatible daemon is terminated and replaced, not left running.
#[tokio::test]
async fn incompatible_daemon_is_evicted_not_orphaned() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let pid = spawn_sacrificial_daemon(tmp.path());
    std::fs::write(&pidfile, pid.to_string()).unwrap();
    spawn_incompatible_daemon(
        &socket,
        Some(DaemonMsg::Error {
            message: "protocol version mismatch: ours=1, theirs=16".into(),
        }),
    );

    let listener = bind_or_detect(&socket, &pidfile)
        .await
        .expect("an incompatible daemon must be evicted, not fatal");
    drop(listener);

    assert!(
        wait_gone(pid),
        "the evicted daemon's process must be gone — this is the orphan bug"
    );
}

/// A daemon that never answers the handshake is wedged, not compatible: evict it too, and do so
/// without hanging the new daemon's startup forever.
#[tokio::test]
async fn unresponsive_daemon_is_evicted() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let pid = spawn_sacrificial_daemon(tmp.path());
    std::fs::write(&pidfile, pid.to_string()).unwrap();
    spawn_incompatible_daemon(&socket, None);

    let listener = tokio::time::timeout(Duration::from_secs(20), bind_or_detect(&socket, &pidfile))
        .await
        .expect("probing a silent daemon must time out, not hang startup")
        .expect("a wedged daemon must be evicted");
    drop(listener);

    assert!(wait_gone(pid), "the wedged daemon must be terminated");
}

/// The compatible case is unchanged: a second daemon refuses to start and leaves the first alone.
#[tokio::test]
async fn compatible_daemon_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    let pid = spawn_sacrificial_daemon(tmp.path());
    std::fs::write(&pidfile, pid.to_string()).unwrap();

    // A real daemon on the socket, speaking our version.
    let registry = Registry::new(Box::new(ClaudeAdapter::with_command(vec!["cat".into()])));
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry));

    let err = bind_or_detect(&socket, &pidfile)
        .await
        .expect_err("a compatible daemon already owns the socket");
    assert!(
        err.to_string().contains("already running"),
        "unexpected error: {err:#}"
    );
    assert!(
        alive(pid),
        "a compatible daemon must never be evicted — that would kill a working session"
    );
    let _ = kill(Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
}

/// A socket file left behind by a crash (no listener) is reclaimed, as before — and without
/// signalling whatever the stale pidfile happens to name.
#[tokio::test]
async fn stale_socket_file_is_reclaimed() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    // Binding and dropping leaves the socket file on disk with nothing behind it.
    let dead = UnixListener::bind(&socket).unwrap();
    drop(dead);

    let pid = spawn_sacrificial_daemon(tmp.path());
    std::fs::write(&pidfile, pid.to_string()).unwrap();

    let listener = bind_or_detect(&socket, &pidfile)
        .await
        .expect("a stale socket file should be reclaimed");
    assert!(
        alive(pid),
        "nothing was listening, so nothing should have been signalled"
    );
    let _ = kill(Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);

    // The rebound socket really is ours: a client can connect and handshake.
    let registry = Registry::new(Box::new(ClaudeAdapter::with_command(vec!["cat".into()])));
    tokio::spawn(serve(listener, registry));
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

/// Eviction must never signal the evicting process itself, however the pidfile got that way.
#[tokio::test]
async fn own_pid_in_the_pidfile_is_never_signalled() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

    std::fs::write(&pidfile, std::process::id().to_string()).unwrap();
    spawn_incompatible_daemon(&socket, None);

    // Must return (binding over the unlinked socket) rather than terminating the test process.
    let listener = tokio::time::timeout(Duration::from_secs(20), bind_or_detect(&socket, &pidfile))
        .await
        .expect("must not hang")
        .expect("must still bind");
    drop(listener);
}

/// The pid-reuse guard: a pidfile naming a live process that is **not** amux must not be
/// signalled. Ungraceful teardown (an SSH drop, a reboot) routinely leaves stale pidfiles behind,
/// and pids get reused — SIGTERMing an unrelated process would be far worse than the orphan.
#[tokio::test]
async fn a_reused_pid_belonging_to_another_program_is_not_signalled() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("amuxd.sock");
    let pidfile = tmp.path().join("amuxd.pid");

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
    spawn_incompatible_daemon(&socket, None);

    let listener = tokio::time::timeout(Duration::from_secs(20), bind_or_detect(&socket, &pidfile))
        .await
        .expect("must not hang")
        .expect("must still bind");
    drop(listener);

    assert!(alive(pid), "an unrelated process must never be signalled");
    let _ = kill(Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
}

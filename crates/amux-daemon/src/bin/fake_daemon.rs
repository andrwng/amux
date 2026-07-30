//! Test-support process, not a user-facing binary: stands in for a live daemon holding the
//! singleton `flock`, so `tests/eviction.rs` can drive `acquire_and_bind`'s eviction and
//! stand-down paths against a *real* process.
//!
//! That realism is the point: an in-test `Flock` guard can't be released by killing a decoy pid
//! (the lock belongs to the open file description that took it, not to whatever pid a test
//! happens to write into the pidfile), so proving eviction genuinely frees the lock for a
//! successor needs a process that actually holds it and actually dies on `SIGTERM`. Cargo sets
//! `CARGO_BIN_EXE_fake_daemon` for integration tests of this package because it declares this
//! `[[bin]]` (auto-discovered from `src/bin/`), so the test can find this binary without
//! guessing a target-dir path.
//!
//! Configured entirely by environment variables — nobody runs this by hand:
//! - `AMUX_FAKE_LOCK` (required): path to `flock` exclusively for the process's lifetime.
//! - `AMUX_FAKE_SOCKET` (optional): if set, bind and serve the control socket per `AMUX_FAKE_MODE`.
//! - `AMUX_FAKE_MODE`: `compatible` (answer with our real `PROTO_VERSION`), `incompatible`
//!   (answer a rejection `Error`), or anything else (accept the connection, never answer —
//!   simulates a wedged/silent daemon). Ignored if `AMUX_FAKE_SOCKET` is unset.
//!
//! Holds the lock and parks until killed. `SIGTERM`'s default disposition terminates the
//! process, which releases the lock exactly as a real daemon exiting would.

use std::path::PathBuf;

use amux_proto::{DaemonMsg, ServerCodec, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use nix::fcntl::{Flock, FlockArg};
use tokio_util::codec::Framed;

#[tokio::main]
async fn main() {
    let lock_path =
        PathBuf::from(std::env::var("AMUX_FAKE_LOCK").expect("AMUX_FAKE_LOCK must be set"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open the lock file");
    // Held for the rest of this process's life; dropping it (only on exit) releases it.
    let _lock = Flock::lock(file, FlockArg::LockExclusive).expect("acquire the fake lock");

    if let Ok(socket) = std::env::var("AMUX_FAKE_SOCKET") {
        let socket_path = PathBuf::from(socket);
        let mode = std::env::var("AMUX_FAKE_MODE").unwrap_or_default();
        let listener =
            tokio::net::UnixListener::bind(&socket_path).expect("bind the fake control socket");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let mode = mode.clone();
            tokio::spawn(async move {
                let mut framed = Framed::new(stream, ServerCodec::new());
                let _ = framed.next().await; // their Hello
                match mode.as_str() {
                    "compatible" => {
                        let _ = framed
                            .send(DaemonMsg::Hello {
                                proto_version: PROTO_VERSION,
                            })
                            .await;
                    }
                    "incompatible" => {
                        let _ = framed
                            .send(DaemonMsg::Error {
                                message: "protocol version mismatch (fake daemon)".into(),
                            })
                            .await;
                    }
                    _ => {} // silent: hold the connection open, never answer
                }
                std::future::pending::<()>().await;
            });
        }
    }

    // Lock-only mode: nothing to serve, just hold the lock until killed.
    std::future::pending::<()>().await;
}

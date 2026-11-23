//! Connecting to the repo's daemon: discover the git repo from the cwd, connect to the control
//! socket, and handshake. If no usable daemon answers — absent, stale, or an incompatible
//! protocol version — auto-spawn a fresh `amux daemon --repo <root>`, so a leftover daemon from
//! an older build never wedges the client.
//!
//! **The client never clears the socket itself.** Unlinking it here is what used to orphan the
//! previous daemon: the name became free, a new daemon bound it, and the old process kept running
//! unreachably with its PTYs and agent processes. Arbitration belongs to the daemon that binds —
//! see `amux-daemon`'s `acquire_and_bind`, which probes and evicts confirmed-incompatible daemons.
//!
//! **Commands use the other door.** [`connect_to_running_daemon`] only attempts the handshake:
//! it never spawns a daemon, never unlinks the socket and so can never cause an eviction. A
//! command that found a daemon of another version and started a new one would have that new
//! daemon evict the old — and kill its agents — just to answer a question.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use amux_proto::{check_version, ClientCodec, ClientMsg, DaemonMsg, PROTO_VERSION};

pub(crate) type Connection = Framed<UnixStream, ClientCodec>;

/// How long a command waits for a daemon to answer the handshake. It only has to send a `Hello` it
/// has already built (the daemon's own probe of an incumbent uses the same bound).
const COMMAND_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Why [`connect_to_running_daemon`] did not return a connection. Decided from what answered on
/// the socket, never from the text of an error.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Nothing accepts on the control socket: no daemon is running.
    #[error("no amux daemon is running")]
    NoDaemon,
    /// Something answered, but not with our protocol: a `Hello` with another version, an
    /// unexpected frame (an older daemon rejects our `Hello` with an `Error`), or a frame that
    /// does not decode. The classification the daemon's own probe of an incumbent makes.
    #[error("the running amux daemon is a different version")]
    VersionDiffers,
    /// Anything else: a daemon that is there and does not answer in time, a socket we may not
    /// open, a configuration that does not load.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Connect to the daemon that is already running for this amux home and complete the version
/// handshake — for commands. Never starts a daemon, never touches the socket file, and so never
/// causes an eviction; the caller gets a typed reason instead. Needs no git repository.
pub async fn connect_to_running_daemon() -> std::result::Result<Connection, ConnectError> {
    let socket = amux_core::paths::RuntimePaths::resolve()?.socket();
    connect_to_running_daemon_at(&socket, COMMAND_HANDSHAKE_TIMEOUT).await
}

/// [`connect_to_running_daemon`] against an explicit socket and handshake bound.
pub(crate) async fn connect_to_running_daemon_at(
    socket: &Path,
    timeout: Duration,
) -> std::result::Result<Connection, ConnectError> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        // No socket file, or a file nobody listens on (a crash leftover): no daemon.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ConnectError::NoDaemon)
        }
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("connect to {}", socket.display()))
                .into())
        }
    };
    let mut framed = Framed::new(stream, ClientCodec::new());
    let exchange = async {
        framed
            .send(ClientMsg::Hello {
                proto_version: PROTO_VERSION,
            })
            .await?;
        Ok::<_, amux_proto::ProtoError>(framed.next().await)
    };
    match tokio::time::timeout(timeout, exchange).await {
        Err(_) => Err(anyhow::anyhow!("the amux daemon did not answer within {timeout:?}").into()),
        Ok(Err(e)) => Err(anyhow::Error::new(e).context("send hello").into()),
        Ok(Ok(None)) => Err(anyhow::anyhow!("the amux daemon closed the connection").into()),
        Ok(Ok(Some(Ok(DaemonMsg::Hello { proto_version })))) if proto_version == PROTO_VERSION => {
            Ok(framed)
        }
        Ok(Ok(Some(Ok(_)))) => Err(ConnectError::VersionDiffers),
        Ok(Ok(Some(Err(amux_proto::ProtoError::Io(e))))) => {
            Err(anyhow::Error::new(e).context("read daemon hello").into())
        }
        // Undecodable or oversized: positional postcard reads another layout as garbage.
        Ok(Ok(Some(Err(_)))) => Err(ConnectError::VersionDiffers),
    }
}

/// Connect to the (global) daemon, auto-spawning/recovering it, and handshake. Returns the
/// connection and the repository discovered from the cwd, so the caller can register it — the
/// daemon serves many repos, and this client's repo may not be the one the daemon launched in.
pub async fn connect() -> Result<(Connection, PathBuf)> {
    let cwd = std::env::current_dir().context("get current directory")?;
    let repo = amux_core::worktree::discover_repo(&cwd)?;
    let socket = amux_core::paths::RuntimePaths::resolve()?.socket();

    // Reuse a running, compatible daemon if there is one.
    if let Ok(connection) = try_handshake(&socket).await {
        return Ok((connection, repo));
    }

    // Otherwise the daemon is absent, stale, or speaks a different protocol version. Spawn one and
    // wait for it to come up; it arbitrates over the socket (evicting an incompatible predecessor)
    // rather than us unlinking the socket out from under a process that would then be unreachable.
    spawn_daemon(&repo).await?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(connection) = try_handshake(&socket).await {
            return Ok((connection, repo));
        }
    }
    bail!("amux daemon did not come up at {}", socket.display());
}

/// Connect and complete the version handshake, or fail (so the caller can recover).
async fn try_handshake(socket: &Path) -> Result<Connection> {
    let stream = UnixStream::connect(socket).await.context("connect")?;
    let mut framed = Framed::new(stream, ClientCodec::new());
    framed
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await?;
    match framed.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => {
            check_version(proto_version)?;
            Ok(framed)
        }
        Some(Ok(DaemonMsg::Error { message })) => bail!("daemon rejected connection: {message}"),
        Some(Ok(other)) => bail!("unexpected first frame from daemon: {other:?}"),
        Some(Err(e)) => Err(e).context("read daemon hello"),
        None => bail!("daemon closed the connection during handshake"),
    }
}

async fn spawn_daemon(repo: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locate the amux executable")?;
    let mut child = tokio::process::Command::new(exe)
        .arg("daemon")
        .arg("--repo")
        .arg(repo)
        .spawn()
        .context("spawn amux daemon")?;
    let _ = child.wait().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_proto::ServerCodec;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    const QUICK: Duration = Duration::from_millis(500);

    /// Serve every connection on `socket` with `answer`: read the client's hello, then reply.
    fn serve(socket: &Path, answer: fn() -> Option<DaemonMsg>) {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut framed = Framed::new(stream, ServerCodec::new());
                    let _ = framed.next().await;
                    if let Some(msg) = answer() {
                        let _ = framed.send(msg).await;
                    }
                    std::future::pending::<()>().await;
                });
            }
        });
    }

    /// With no daemon, the command path says so and starts nothing: a daemon it spawned would bind
    /// the socket, and none appears.
    #[tokio::test]
    async fn no_daemon_is_reported_and_none_is_started() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        let got = connect_to_running_daemon_at(&socket, QUICK).await;
        assert!(
            matches!(got, Err(ConnectError::NoDaemon)),
            "{:?}",
            got.as_ref().err()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!socket.exists(), "nothing was started to bind the socket");
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "nothing was created"
        );
    }

    /// A socket file nobody listens on (a crash leftover) is no daemon, and is left where it is:
    /// unlinking it is the daemon's arbitration, never a client's.
    #[tokio::test]
    async fn a_stale_socket_is_no_daemon_and_is_not_unlinked() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        let got = connect_to_running_daemon_at(&socket, QUICK).await;
        assert!(
            matches!(got, Err(ConnectError::NoDaemon)),
            "{:?}",
            got.as_ref().err()
        );
        assert!(socket.exists(), "the socket file is untouched");
    }

    /// A daemon of another version is reported as such and keeps running: its socket is still
    /// there and still accepts.
    #[tokio::test]
    async fn another_version_is_reported_and_the_daemon_is_left_running() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        serve(&socket, || {
            Some(DaemonMsg::Hello {
                proto_version: PROTO_VERSION + 1,
            })
        });
        let got = connect_to_running_daemon_at(&socket, QUICK).await;
        assert!(
            matches!(got, Err(ConnectError::VersionDiffers)),
            "{:?}",
            got.as_ref().err()
        );
        assert!(socket.exists());
        assert!(
            UnixStream::connect(&socket).await.is_ok(),
            "still accepting"
        );
    }

    /// An older daemon answers our `Hello` with an `Error`: another version.
    #[tokio::test]
    async fn a_rejection_is_another_version() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        serve(&socket, || {
            Some(DaemonMsg::Error {
                message: "protocol version mismatch".into(),
            })
        });
        let got = connect_to_running_daemon_at(&socket, QUICK).await;
        assert!(
            matches!(got, Err(ConnectError::VersionDiffers)),
            "{:?}",
            got.as_ref().err()
        );
    }

    /// A frame that does not decode is another version too.
    #[tokio::test]
    async fn an_undecodable_frame_is_another_version() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // A well-framed body that is no `DaemonMsg` (variant index far past the last).
            stream.write_all(&[0, 0, 0, 2, 0xff, 0x7f]).await.unwrap();
            std::future::pending::<()>().await;
        });
        let got = connect_to_running_daemon_at(&socket, QUICK).await;
        assert!(
            matches!(got, Err(ConnectError::VersionDiffers)),
            "{:?}",
            got.as_ref().err()
        );
    }

    /// A daemon that is there but silent is neither: the command fails as anything else.
    #[tokio::test]
    async fn a_silent_daemon_is_neither() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        serve(&socket, || None);
        let got = connect_to_running_daemon_at(&socket, Duration::from_millis(200)).await;
        assert!(
            matches!(got, Err(ConnectError::Other(_))),
            "{:?}",
            got.as_ref().err()
        );
    }

    #[tokio::test]
    async fn a_compatible_daemon_connects() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        serve(&socket, || {
            Some(DaemonMsg::Hello {
                proto_version: PROTO_VERSION,
            })
        });
        assert!(connect_to_running_daemon_at(&socket, QUICK).await.is_ok());
    }
}

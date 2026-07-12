//! Integration tests for the daemon: a headless test client drives a deterministic `cat` PTY,
//! covering echo, **detach + reattach to the same live session** (the Phase 0.6 promise), and
//! shell-exit reporting. See `docs/PHASE-0.md`.

use std::sync::Arc;
use std::time::Duration;

use amux_daemon::{bind_or_detect, serve, DaemonConfig, Registry};
use amux_proto::{ClientCodec, ClientMsg, DaemonMsg, Size, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

type Client = Framed<UnixStream, ClientCodec>;

fn cat_registry() -> Arc<Registry> {
    Arc::new(Registry::new(DaemonConfig {
        command: vec!["cat".into()],
    }))
}

/// Connect and complete the Hello handshake (leaving the snapshot for the caller to read).
async fn handshake(socket: &std::path::Path) -> Client {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let mut client = Framed::new(stream, ClientCodec::new());
    client
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            size: Size { cols: 80, rows: 24 },
        })
        .await
        .expect("send hello");
    match client.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => assert_eq!(proto_version, PROTO_VERSION),
        other => panic!("expected daemon Hello, got {other:?}"),
    }
    client
}

/// Drain frames until the accumulated output contains `needle` (or time out).
async fn wait_for_output(client: &mut Client, needle: &str) -> bool {
    let mut acc = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Output(b))) | Some(Ok(DaemonMsg::OutputSnapshot(b))) => {
                    acc.extend_from_slice(&b);
                    if String::from_utf8_lossy(&acc).contains(needle) {
                        return true;
                    }
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test]
async fn cat_pty_echoes_input() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(listener, cat_registry()));

    let mut client = handshake(&socket).await;
    assert!(matches!(
        client.next().await,
        Some(Ok(DaemonMsg::OutputSnapshot(_)))
    ));
    client
        .send(ClientMsg::Input(b"amux\n".to_vec()))
        .await
        .unwrap();
    assert!(wait_for_output(&mut client, "amux").await, "no echo");

    server.abort();
}

#[tokio::test]
async fn detach_then_reattach_sees_the_same_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(listener, cat_registry()));

    // Client A: leave a marker on the screen, then detach by dropping the connection.
    let mut a = handshake(&socket).await;
    assert!(matches!(
        a.next().await,
        Some(Ok(DaemonMsg::OutputSnapshot(_)))
    ));
    a.send(ClientMsg::Input(b"marker-XYZ\n".to_vec()))
        .await
        .unwrap();
    assert!(
        wait_for_output(&mut a, "marker-XYZ").await,
        "marker not echoed"
    );
    drop(a); // detach — the session must live on

    // Client B: reattach; the snapshot should already contain the marker.
    let mut b = handshake(&socket).await;
    let snapshot = match b.next().await {
        Some(Ok(DaemonMsg::OutputSnapshot(bytes))) => bytes,
        other => panic!("expected OutputSnapshot on reattach, got {other:?}"),
    };
    assert!(
        String::from_utf8_lossy(&snapshot).contains("marker-XYZ"),
        "reattach snapshot lost the marker → session was not persisted"
    );

    // And it is the SAME live process: more input still echoes.
    b.send(ClientMsg::Input(b"second-PQR\n".to_vec()))
        .await
        .unwrap();
    assert!(
        wait_for_output(&mut b, "second-PQR").await,
        "reattached session is not live"
    );

    server.abort();
}

#[tokio::test]
async fn shell_exit_reports_exited() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(listener, cat_registry()));

    let mut client = handshake(&socket).await;
    assert!(matches!(
        client.next().await,
        Some(Ok(DaemonMsg::OutputSnapshot(_)))
    ));
    // Ctrl-D at the start of a line → EOF → cat exits.
    client.send(ClientMsg::Input(vec![0x04])).await.unwrap();

    let saw_exit = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Exited { .. })) => return true,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_exit, "cat exit did not produce an Exited frame");

    server.abort();
}

#[tokio::test]
async fn bind_or_detect_rejects_a_live_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(listener, cat_registry()));

    assert!(
        bind_or_detect(&socket).is_err(),
        "should reject a live daemon's socket"
    );

    server.abort();
}

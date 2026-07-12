//! Integration tests for the daemon's control server, driven by a headless test client
//! (the reusable Phase-0 fixture) against a deterministic `cat` PTY. See `docs/PHASE-0.md`.

use std::time::Duration;

use amux_daemon::{bind_or_detect, serve, DaemonConfig};
use amux_proto::{ClientCodec, ClientMsg, DaemonMsg, Size, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

type Client = Framed<UnixStream, ClientCodec>;

/// Connect, complete the Hello handshake, and consume the initial snapshot.
async fn connect(socket: &std::path::Path) -> Client {
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
    match client.next().await {
        Some(Ok(DaemonMsg::OutputSnapshot(_))) => {}
        other => panic!("expected OutputSnapshot, got {other:?}"),
    }
    client
}

#[tokio::test]
async fn cat_pty_echoes_input_then_reports_exit() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(
        listener,
        DaemonConfig {
            command: vec!["cat".into()],
        },
    ));

    let mut client = connect(&socket).await;

    // Send input; the pty (echo on) + cat should surface "amux" in the output stream.
    client
        .send(ClientMsg::Input(b"amux\n".to_vec()))
        .await
        .unwrap();

    let mut acc = Vec::new();
    let saw_echo = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Output(bytes))) => {
                    acc.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&acc).contains("amux") {
                        return true;
                    }
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        saw_echo,
        "never saw echoed input; accumulated: {:?}",
        String::from_utf8_lossy(&acc)
    );

    // Ask the daemon to shut the session down; expect an Exited frame.
    client.send(ClientMsg::Shutdown).await.unwrap();
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
    assert!(saw_exit, "never saw Exited after Shutdown");

    server.abort();
}

#[tokio::test]
async fn bind_or_detect_rejects_a_live_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(
        listener,
        DaemonConfig {
            command: vec!["cat".into()],
        },
    ));

    // A second daemon trying the same socket must detect the running one and refuse.
    let result = bind_or_detect(&socket);
    assert!(
        result.is_err(),
        "bind_or_detect should reject a live daemon's socket"
    );

    server.abort();
}

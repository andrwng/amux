//! Integration test for the client's connect + handshake against a real daemon `serve()`
//! (the reuse-existing-daemon path; auto-spawn is exercised manually via the binary).

use amux_daemon::{serve, DaemonConfig};
use amux_proto::{DaemonMsg, Size};
use amux_tui::{connect, ClientOptions};
use futures::StreamExt;
use tokio::net::UnixListener;

#[tokio::test]
async fn connect_reuses_running_daemon_and_receives_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(
        listener,
        DaemonConfig {
            command: vec!["cat".into()],
        },
    ));

    let opts = ClientOptions {
        socket: socket.clone(),
        spawn_daemon: false,
        size: Size { cols: 80, rows: 24 },
    };
    let mut framed = connect(&opts).await.expect("connect + handshake");

    match framed.next().await {
        Some(Ok(DaemonMsg::OutputSnapshot(_))) => {}
        other => panic!("expected OutputSnapshot after handshake, got {other:?}"),
    }

    server.abort();
}

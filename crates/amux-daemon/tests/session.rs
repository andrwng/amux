//! Integration test for the multi-agent daemon: a headless client creates an agent (a worktree
//! plus a `cat` session), attaches, echoes input, then deletes it — over the v1 protocol against
//! a temp git repo built via libgit2 (no `git` binary needed). See `docs/PHASE-1.md` §1.5.

use std::path::Path;
use std::time::Duration;

use amux_core::adapter::ClaudeAdapter;
use amux_core::agent::AgentId;
use amux_core::worktree::WorktreeService;
use amux_daemon::{serve, Registry};
use amux_proto::{ClientCodec, ClientMsg, DaemonMsg, Size, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use git2::Repository;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

type Client = Framed<UnixStream, ClientCodec>;

fn init_repo(dir: &Path) {
    let repo = Repository::init(dir).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "amux test").unwrap();
        config.set_str("user.email", "test@amux.local").unwrap();
    }
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
}

async fn handshake(socket: &Path) -> Client {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let mut client = Framed::new(stream, ClientCodec::new());
    client
        .send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await
        .unwrap();
    match client.next().await {
        Some(Ok(DaemonMsg::Hello { proto_version })) => assert_eq!(proto_version, PROTO_VERSION),
        other => panic!("expected Hello, got {other:?}"),
    }
    match client.next().await {
        Some(Ok(DaemonMsg::Agents(_))) => {}
        other => panic!("expected Agents, got {other:?}"),
    }
    client
}

/// Drain until an `AgentAdded` arrives; return its id.
async fn wait_for_added(client: &mut Client) -> AgentId {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::AgentAdded(info))) => return info.id,
                Some(Ok(_)) => {}
                other => panic!("stream ended before AgentAdded: {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for AgentAdded")
}

async fn wait_for_output(client: &mut Client, needle: &str) -> bool {
    let mut acc = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Output { bytes, .. }))
                | Some(Ok(DaemonMsg::OutputSnapshot { bytes, .. })) => {
                    acc.extend_from_slice(&bytes);
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
async fn create_attach_echo_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);

    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(worktrees, adapter);

    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve(listener, registry));

    let mut client = handshake(&socket).await;

    // Create an agent (worktree + cat session) and learn its id from the broadcast event.
    client
        .send(ClientMsg::CreateAgent {
            branch: "feat/x".into(),
        })
        .await
        .unwrap();
    let id = wait_for_added(&mut client).await;

    // Attach and confirm the cat session echoes input.
    client
        .send(ClientMsg::Attach {
            id,
            size: Size { cols: 80, rows: 24 },
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Input {
            id,
            bytes: b"echo-marker\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(
        wait_for_output(&mut client, "echo-marker").await,
        "attached session did not echo input"
    );

    // Delete removes the agent (and its worktree); expect the broadcast.
    let worktree_path = tmp.path().join("wt").join("feat-x");
    assert!(
        worktree_path.exists(),
        "worktree should exist before delete"
    );
    client.send(ClientMsg::DeleteAgent { id }).await.unwrap();
    let removed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::AgentRemoved { id: gone })) => return gone == id,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(removed, "expected AgentRemoved after delete");

    server.abort();
}

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
    client
        .send(ClientMsg::DeleteAgent { id, force: true })
        .await
        .unwrap();
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

#[tokio::test]
async fn two_agents_stream_simultaneously() {
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
    let size = Size { cols: 80, rows: 24 };

    client
        .send(ClientMsg::CreateAgent { branch: "a".into() })
        .await
        .unwrap();
    let a = wait_for_added(&mut client).await;
    client
        .send(ClientMsg::CreateAgent { branch: "b".into() })
        .await
        .unwrap();
    let b = wait_for_added(&mut client).await;

    // Attach both, then feed each — both should stream back, tagged by id.
    client
        .send(ClientMsg::Attach { id: a, size })
        .await
        .unwrap();
    client
        .send(ClientMsg::Attach { id: b, size })
        .await
        .unwrap();
    client
        .send(ClientMsg::Input {
            id: a,
            bytes: b"AAAA\n".to_vec(),
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Input {
            id: b,
            bytes: b"BBBB\n".to_vec(),
        })
        .await
        .unwrap();

    let both = tokio::time::timeout(Duration::from_secs(5), async {
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Output { id, bytes }))
                | Some(Ok(DaemonMsg::OutputSnapshot { id, bytes })) => {
                    if id == a {
                        sa.extend_from_slice(&bytes);
                    } else if id == b {
                        sb.extend_from_slice(&bytes);
                    }
                    if String::from_utf8_lossy(&sa).contains("AAAA")
                        && String::from_utf8_lossy(&sb).contains("BBBB")
                    {
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
    assert!(both, "both attached agents should stream, tagged by id");

    server.abort();
}

#[tokio::test]
async fn dirty_worktree_requires_delete_confirmation() {
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
    client
        .send(ClientMsg::CreateAgent {
            branch: "feat/dirty".into(),
        })
        .await
        .unwrap();
    let id = wait_for_added(&mut client).await;

    // Dirty the worktree with an untracked file.
    let worktree = tmp.path().join("wt").join("feat-dirty");
    std::fs::write(worktree.join("scratch.txt"), "uncommitted").unwrap();

    // Delete without force → the daemon refuses and asks to confirm.
    client
        .send(ClientMsg::DeleteAgent { id, force: false })
        .await
        .unwrap();
    let needs_confirm = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::DeleteNeedsConfirm { id: got, .. })) => return got == id,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(needs_confirm, "dirty worktree should require confirmation");
    assert!(
        worktree.exists(),
        "worktree must survive an unconfirmed delete"
    );

    // Force delete → gone.
    client
        .send(ClientMsg::DeleteAgent { id, force: true })
        .await
        .unwrap();
    let removed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::AgentRemoved { id: got })) => return got == id,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(removed, "force delete should remove the agent");
    assert!(
        !worktree.exists(),
        "worktree should be gone after force delete"
    );

    server.abort();
}

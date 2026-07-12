//! Integration tests for the multi-agent daemon (v4): a headless client creates an agent (a
//! worktree + a primary `cat` terminal), attaches, echoes input, splits a shell in the same
//! worktree, and deletes — over a temp git repo built via libgit2 (no `git` binary needed).

use std::path::Path;
use std::time::Duration;

use amux_core::adapter::ClaudeAdapter;
use amux_core::agent::TerminalId;
use amux_core::worktree::WorktreeService;
use amux_daemon::{serve, Registry};
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, Size, PROTO_VERSION};
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

/// Spin up a daemon over a fresh temp repo with a `cat` primary; return (client, socket, tmp).
async fn setup() -> (Client, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);

    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(worktrees, adapter);

    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry));

    let client = handshake(&socket).await;
    (client, tmp)
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

async fn create_agent(client: &mut Client, branch: &str) -> AgentInfo {
    client
        .send(ClientMsg::CreateAgent {
            branch: branch.into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::AgentAdded(info))) => return info,
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

const SIZE: Size = Size { cols: 80, rows: 24 };

#[tokio::test]
async fn create_attach_echo_delete() {
    let (mut client, _tmp) = setup().await;
    let agent = create_agent(&mut client, "feat/x").await;
    let term = agent.primary_terminal;

    client
        .send(ClientMsg::Attach {
            terminal: term,
            size: SIZE,
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Input {
            terminal: term,
            bytes: b"echo-marker\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(
        wait_for_output(&mut client, "echo-marker").await,
        "primary terminal did not echo input"
    );

    client
        .send(ClientMsg::DeleteAgent {
            id: agent.id,
            force: true,
        })
        .await
        .unwrap();
    let removed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::AgentRemoved { id })) => return id == agent.id,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(removed, "expected AgentRemoved after delete");
}

#[tokio::test]
async fn split_spawns_an_attachable_shell_in_the_same_worktree() {
    let (mut client, _tmp) = setup().await;
    let agent = create_agent(&mut client, "feat/split").await;

    // Split: spawn a shell terminal (client-generated id) beside the primary.
    let shell = TerminalId::new();
    client
        .send(ClientMsg::SpawnShell {
            terminal: shell,
            like: agent.primary_terminal,
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Attach {
            terminal: shell,
            size: SIZE,
        })
        .await
        .unwrap();

    // Attaching the new shell yields a snapshot tagged with its terminal id.
    let attached = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::OutputSnapshot { terminal, .. })) => return terminal == shell,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(attached, "the split shell terminal should be attachable");
}

#[tokio::test]
async fn two_terminals_stream_simultaneously() {
    let (mut client, _tmp) = setup().await;
    let a = create_agent(&mut client, "a").await;
    let b = create_agent(&mut client, "b").await;
    let (ta, tb) = (a.primary_terminal, b.primary_terminal);

    client
        .send(ClientMsg::Attach {
            terminal: ta,
            size: SIZE,
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Attach {
            terminal: tb,
            size: SIZE,
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Input {
            terminal: ta,
            bytes: b"AAAA\n".to_vec(),
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::Input {
            terminal: tb,
            bytes: b"BBBB\n".to_vec(),
        })
        .await
        .unwrap();

    let both = tokio::time::timeout(Duration::from_secs(5), async {
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Output { terminal, bytes }))
                | Some(Ok(DaemonMsg::OutputSnapshot { terminal, bytes })) => {
                    if terminal == ta {
                        sa.extend_from_slice(&bytes);
                    } else if terminal == tb {
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
    assert!(both, "both terminals should stream, tagged by id");
}

#[tokio::test]
async fn dirty_worktree_requires_delete_confirmation() {
    let (mut client, tmp) = setup().await;
    let agent = create_agent(&mut client, "feat/dirty").await;

    let worktree = tmp.path().join("wt").join("feat-dirty");
    std::fs::write(worktree.join("scratch.txt"), "uncommitted").unwrap();

    client
        .send(ClientMsg::DeleteAgent {
            id: agent.id,
            force: false,
        })
        .await
        .unwrap();
    let needs_confirm = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::DeleteNeedsConfirm { id, .. })) => return id == agent.id,
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

    client
        .send(ClientMsg::DeleteAgent {
            id: agent.id,
            force: true,
        })
        .await
        .unwrap();
    let removed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::AgentRemoved { id })) => return id == agent.id,
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
}

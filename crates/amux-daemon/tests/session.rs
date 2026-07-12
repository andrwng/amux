//! Integration tests for the multi-agent daemon (v4): a headless client creates an agent (a
//! worktree + a primary `cat` terminal), attaches, echoes input, splits a shell in the same
//! worktree, and deletes — over a temp git repo built via libgit2 (no `git` binary needed).

use std::path::Path;
use std::time::Duration;

use amux_core::adapter::ClaudeAdapter;
use amux_core::agent::{AgentId, AgentState, RepoId, TerminalId};
use amux_core::hook::{HookEvent, HookReport};
use amux_core::worktree::WorktreeService;
use amux_daemon::{bind_mailbox, serve, serve_mailbox, Registry};
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, Size, PROTO_VERSION};
use futures::{SinkExt, StreamExt};
use git2::Repository;
use tokio::io::AsyncWriteExt;
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

/// Spin up a daemon over a fresh temp repo with a `cat` primary; return (client, repo id, tmp).
async fn setup() -> (Client, RepoId, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);

    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(adapter);
    let repo_id = registry.register(worktrees).id;

    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry));

    let client = handshake(&socket).await;
    (client, repo_id, tmp)
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
        Some(Ok(DaemonMsg::Repos(_))) => {}
        other => panic!("expected Repos, got {other:?}"),
    }
    match client.next().await {
        Some(Ok(DaemonMsg::Agents(_))) => {}
        other => panic!("expected Agents, got {other:?}"),
    }
    client
}

async fn create_agent(client: &mut Client, repo: RepoId, branch: &str) -> AgentInfo {
    client
        .send(ClientMsg::CreateAgent {
            repo,
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

/// Deliver one hook report the way `amux hook` does: a single postcard frame, then EOF.
async fn send_hook(mailbox: &Path, report: HookReport) {
    let mut stream = UnixStream::connect(mailbox).await.expect("connect mailbox");
    let bytes = postcard::to_stdvec(&report).unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.shutdown().await.unwrap();
}

fn hook(name: &str, notification_type: Option<&str>, message: Option<&str>) -> HookEvent {
    HookEvent {
        hook_event_name: name.into(),
        session_id: Some("sess-1".into()),
        notification_type: notification_type.map(Into::into),
        message: message.map(Into::into),
        tool_name: None,
    }
}

/// Read control-stream frames until the given agent reaches a state matching `pred`.
async fn wait_for_state(
    client: &mut Client,
    id: AgentId,
    pred: impl Fn(&AgentState) -> bool,
) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::StateChanged { id: sid, state })) if sid == id => {
                    if pred(&state) {
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
    let (mut client, repo, _tmp) = setup().await;
    let agent = create_agent(&mut client, repo, "feat/x").await;
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
async fn hooks_drive_the_state_machine_over_the_mailbox() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(adapter);
    let repo_id = registry.register(worktrees).id;

    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry.clone()));
    let mailbox = tmp.path().join("hooks.sock");
    let hook_listener = bind_mailbox(&mailbox).unwrap();
    tokio::spawn(serve_mailbox(hook_listener, registry.clone()));

    let mut client = handshake(&socket).await;
    let agent = create_agent(&mut client, repo_id, "feat/hooks").await;

    // A permission Notification lights up the sidebar (⚠).
    send_hook(
        &mailbox,
        HookReport {
            agent: agent.id,
            event: hook(
                "Notification",
                Some("permission_prompt"),
                Some("Claude needs your permission to use Bash"),
            ),
        },
    )
    .await;
    assert!(
        wait_for_state(&mut client, agent.id, |s| matches!(
            s,
            AgentState::NeedsAttention { .. }
        ))
        .await,
        "a permission hook should flip the agent to NeedsAttention"
    );

    // Activity (the user answered → tools run) clears attention back to Working.
    send_hook(
        &mailbox,
        HookReport {
            agent: agent.id,
            event: hook("PreToolUse", None, None),
        },
    )
    .await;
    assert!(
        wait_for_state(&mut client, agent.id, |s| matches!(s, AgentState::Working)).await,
        "activity should return the agent to Working"
    );

    // Stop (Claude finished) settles to Idle.
    send_hook(
        &mailbox,
        HookReport {
            agent: agent.id,
            event: hook("Stop", None, None),
        },
    )
    .await;
    assert!(
        wait_for_state(&mut client, agent.id, |s| matches!(s, AgentState::Idle)).await,
        "Stop should settle the agent to Idle"
    );
}

/// Read control-stream frames until the given agent's unread bit matches `want`.
async fn wait_for_unread(client: &mut Client, id: AgentId, want: bool) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::UnreadChanged { id: uid, unread })) if uid == id => {
                    if unread == want {
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
async fn heartbeat_settles_a_silent_working_agent_to_idle() {
    // A backstop for a missed Stop hook: with no PTY output for the idle window, a Working agent
    // settles to Idle on its own. `cat` is silent until fed input, so it goes quiet immediately.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::with_idle_timeout(adapter, Duration::from_millis(200));
    let repo_id = registry.register(worktrees).id;

    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry.clone()));

    let mut client = handshake(&socket).await;
    let agent = create_agent(&mut client, repo_id, "feat/idle").await;
    assert!(matches!(agent.state, AgentState::Working));

    assert!(
        wait_for_state(&mut client, agent.id, |s| matches!(s, AgentState::Idle)).await,
        "the heartbeat should settle a silent Working agent to Idle"
    );
}

#[tokio::test]
async fn unread_is_set_on_finish_when_unfocused_and_cleared_on_focus() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(adapter);
    let repo_id = registry.register(worktrees).id;

    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry.clone()));
    let mailbox = tmp.path().join("hooks.sock");
    let hook_listener = bind_mailbox(&mailbox).unwrap();
    tokio::spawn(serve_mailbox(hook_listener, registry.clone()));

    let mut client = handshake(&socket).await;
    let agent = create_agent(&mut client, repo_id, "feat/unread").await;

    // Focus is in the sidebar (nothing viewed). Claude finishes a turn → unread.
    send_hook(
        &mailbox,
        HookReport {
            agent: agent.id,
            event: hook("Stop", None, None),
        },
    )
    .await;
    assert!(
        wait_for_unread(&mut client, agent.id, true).await,
        "finishing a turn while unfocused should mark the agent unread"
    );

    // Viewing the agent clears it.
    client
        .send(ClientMsg::Focus {
            agent: Some(agent.id),
        })
        .await
        .unwrap();
    assert!(
        wait_for_unread(&mut client, agent.id, false).await,
        "focusing the agent should clear unread"
    );

    // While it's focused, a real notable transition (Working → Idle) must NOT re-mark it unread.
    // Drive activity then finish, then round-trip ListAgents: the Agents reply must show it still
    // read (any UnreadChanged{true} in between fails the assertion).
    send_hook(
        &mailbox,
        HookReport {
            agent: agent.id,
            event: hook("PreToolUse", None, None),
        },
    )
    .await;
    assert!(
        wait_for_state(&mut client, agent.id, |s| matches!(s, AgentState::Working)).await,
        "activity should move the focused agent to Working"
    );
    send_hook(
        &mailbox,
        HookReport {
            agent: agent.id,
            event: hook("Stop", None, None),
        },
    )
    .await;
    client.send(ClientMsg::ListAgents).await.unwrap();
    let still_read = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::UnreadChanged { id, unread: true })) if id == agent.id => {
                    return false
                }
                Some(Ok(DaemonMsg::Agents(list))) => {
                    return list
                        .iter()
                        .find(|a| a.id == agent.id)
                        .is_some_and(|a| !a.unread)
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        still_read,
        "a finish while the agent is focused must stay read"
    );
}

#[tokio::test]
async fn two_repos_keep_their_agents_separate() {
    // Register two independent repos on one daemon; each agent should carry its own repo id and
    // land in the matching worktree base — the core of multi-repo management.
    let tmp = tempfile::tempdir().unwrap();
    let make = |name: &str| {
        let repo = tmp.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        WorktreeService::with_base(&repo, tmp.path().join(format!("{name}-wt"))).unwrap()
    };
    let adapter = Box::new(ClaudeAdapter::with_command(vec!["cat".into()]));
    let registry = Registry::new(adapter);
    let a = registry.register(make("alpha"));
    let b = registry.register(make("beta"));
    assert_ne!(a.id, b.id, "distinct repos get distinct ids");

    let agent_a = registry.create(a.id, "feat/a").unwrap();
    let agent_b = registry.create(b.id, "feat/b").unwrap();
    assert_eq!(agent_a.repo, a.id);
    assert_eq!(agent_b.repo, b.id);

    // Registering the same path again is idempotent (same id, still two repos).
    let alpha_again =
        WorktreeService::with_base(tmp.path().join("alpha"), tmp.path().join("alpha-wt")).unwrap();
    assert_eq!(registry.register(alpha_again).id, a.id);
    assert_eq!(registry.repos().len(), 2);

    // Each agent's worktree lives under its own repo's base.
    assert!(tmp.path().join("alpha-wt").join("feat-a").exists());
    assert!(tmp.path().join("beta-wt").join("feat-b").exists());

    // A duplicate agent on the same (repo, branch) is refused with a clear message.
    let dup = registry.create(a.id, "feat/a").unwrap_err();
    assert!(
        dup.to_string().contains("already exists"),
        "duplicate branch should be refused: {dup}"
    );

    registry.shutdown_all();
}

#[tokio::test]
async fn split_spawns_an_attachable_shell_in_the_same_worktree() {
    let (mut client, repo, _tmp) = setup().await;
    let agent = create_agent(&mut client, repo, "feat/split").await;

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
    let (mut client, repo, _tmp) = setup().await;
    let a = create_agent(&mut client, repo, "a").await;
    let b = create_agent(&mut client, repo, "b").await;
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
    let (mut client, repo, tmp) = setup().await;
    let agent = create_agent(&mut client, repo, "feat/dirty").await;

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

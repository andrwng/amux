//! Integration tests for the multi-agent daemon (v4): a headless client creates an agent (a
//! worktree + a primary `cat` terminal), attaches, echoes input, splits a shell in the same
//! worktree, and deletes — over a temp git repo built via libgit2 (no `git` binary needed).

use std::path::Path;
use std::time::Duration;

use amux_core::adapter::ClaudeAdapter;
use amux_core::agent::{AgentId, AgentState, RepoId, TerminalId};
use amux_core::hook::{HookEvent, HookReport, PaneMessage};
use amux_core::nav::Dir;
use amux_core::worktree::WorktreeService;
use amux_daemon::{bind_mailbox, serve, serve_mailbox, Registry};
use amux_proto::{AgentInfo, ClientCodec, ClientMsg, DaemonMsg, Layout, Size, PROTO_VERSION};
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
    match client.next().await {
        Some(Ok(DaemonMsg::Layouts(_))) => {}
        other => panic!("expected Layouts, got {other:?}"),
    }
    match client.next().await {
        Some(Ok(DaemonMsg::Minis(_))) => {}
        other => panic!("expected Minis, got {other:?}"),
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

/// Deliver one pane message the way `amux hook`/`amux nav`/`amux passthrough` do: a single
/// postcard frame, then EOF.
async fn send_pane(mailbox: &Path, msg: PaneMessage) {
    let mut stream = UnixStream::connect(mailbox).await.expect("connect mailbox");
    let bytes = postcard::to_stdvec(&msg).unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn send_hook(mailbox: &Path, report: HookReport) {
    send_pane(mailbox, PaneMessage::Hook(report)).await
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
async fn reattach_snapshot_preserves_mouse_mode() {
    // A re-attaching client rebuilds its parser from the snapshot; the snapshot must replay the
    // app's terminal modes (here mouse tracking), or wheel forwarding etc. would silently break.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    // An agent that *prints* the mouse-mode enable (so the daemon's parser records it), then idles.
    let adapter = Box::new(ClaudeAdapter::with_command(vec![
        "sh".into(),
        "-c".into(),
        "printf '\\033[?1003h\\033[?1006hMARK\\n'; sleep 30".into(),
    ]));
    let registry = Registry::new(adapter);
    let repo_id = registry.register(worktrees).id;
    let socket = tmp.path().join("amuxd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, registry));
    let mut client = handshake(&socket).await;
    let agent = create_agent(&mut client, repo_id, "feat/mouse").await;
    let term = agent.primary_terminal;

    client
        .send(ClientMsg::Attach {
            terminal: term,
            size: SIZE,
        })
        .await
        .unwrap();
    assert!(
        wait_for_output(&mut client, "MARK").await,
        "agent did not print"
    );

    // Re-attach: the fresh snapshot must include the mouse-mode preamble.
    client
        .send(ClientMsg::Detach { terminal: term })
        .await
        .unwrap();
    client
        .send(ClientMsg::Attach {
            terminal: term,
            size: SIZE,
        })
        .await
        .unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::OutputSnapshot { bytes, .. })) => {
                    let s = String::from_utf8_lossy(&bytes);
                    return s.contains("\x1b[?1003h") && s.contains("\x1b[?1006h");
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(ok, "re-attach snapshot must replay the mouse mode");
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
async fn passthrough_and_nav_are_relayed_to_clients() {
    // The vim navigator plugin's `amux passthrough`/`amux nav` arrive over the mailbox and the
    // daemon relays them to clients as TerminalApp / Navigate (layout stays client-side).
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
    let agent = create_agent(&mut client, repo_id, "feat/vim").await;
    let term = agent.primary_terminal;

    send_pane(
        &mailbox,
        PaneMessage::Passthrough {
            terminal: term,
            on: true,
        },
    )
    .await;
    let got_app = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::TerminalApp {
                    terminal,
                    passthrough,
                })) => return terminal == term && passthrough,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(got_app, "passthrough should be relayed as TerminalApp");

    send_pane(
        &mailbox,
        PaneMessage::Nav {
            terminal: term,
            dir: Dir::Left,
        },
    )
    .await;
    let got_nav = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(DaemonMsg::Navigate { terminal, dir })) => {
                    return terminal == term && dir == Dir::Left
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(got_nav, "nav should be relayed as Navigate");
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
async fn layout_persists_for_a_reconnecting_client() {
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

    let mut client = handshake(&socket).await;
    let agent = create_agent(&mut client, repo_id, "feat/layout").await;
    let layout = Layout::Leaf {
        terminal: Some(agent.primary_terminal),
    };
    client
        .send(ClientMsg::SetLayout {
            agent: agent.id,
            layout: Some(layout.clone()),
        })
        .await
        .unwrap();
    client
        .send(ClientMsg::SetMinis(vec![agent.id]))
        .await
        .unwrap();
    // Round-trip a command so the daemon has surely processed SetLayout before we reconnect.
    client.send(ClientMsg::ListAgents).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Ok(DaemonMsg::Agents(_))) = client.next().await {
                return;
            }
        }
    })
    .await
    .unwrap();

    // A fresh connection's handshake must replay the saved layout.
    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut c2 = Framed::new(stream, ClientCodec::new());
    c2.send(ClientMsg::Hello {
        proto_version: PROTO_VERSION,
    })
    .await
    .unwrap();
    let (mut got_layout, mut got_minis) = (false, false);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c2.next().await {
                Some(Ok(DaemonMsg::Layouts(list))) => {
                    got_layout = list.iter().any(|(a, l)| *a == agent.id && *l == layout);
                }
                Some(Ok(DaemonMsg::Minis(minis))) => {
                    got_minis = minis == vec![agent.id];
                    return; // Minis is the last handshake frame
                }
                Some(Ok(_)) => {}
                _ => return,
            }
        }
    })
    .await
    .unwrap();
    assert!(
        got_layout,
        "reconnecting client should receive the saved layout"
    );
    assert!(
        got_minis,
        "reconnecting client should receive the saved minis"
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

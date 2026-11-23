//! Recording a worktree's conversation end to end in-process: `amux hook`'s delivery
//! (`deliver_hook_input`) sends the status report and then the turn, each on its own mailbox
//! connection; the daemon applies the first and appends the second to the worktree's log. All text
//! here is synthetic.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amux_core::adapter::ClaudeAdapter;
use amux_core::agent::{AgentId, AgentState};
use amux_core::clock::ManualClock;
use amux_core::conversation::{Speaker, MAX_TURN_TEXT_BYTES};
use amux_core::worktree::WorktreeService;
use amux_daemon::{bind_mailbox, deliver_hook_input, serve_mailbox, Registry, RegistryOptions};
use chrono::{DateTime, Utc};
use git2::Repository;

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

fn start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-22T10:15:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

struct Fixture {
    registry: Arc<Registry>,
    clock: Arc<ManualClock>,
    mailbox: std::path::PathBuf,
    agent: AgentId,
    tmp: tempfile::TempDir,
}

/// A registry recording conversations under the temp dir, on a manual clock, with its mailbox
/// served and one `cat` agent created.
async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let clock = Arc::new(ManualClock::new(start()));
    let registry = Registry::with_options(
        Box::new(ClaudeAdapter::with_command(vec!["cat".into()])),
        RegistryOptions {
            state_path: Some(tmp.path().join("state.json")),
            conversation_dir: Some(tmp.path().join("conversations")),
            clock: clock.clone(),
            ..RegistryOptions::default()
        },
    );
    let worktrees = WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
    let repo_id = registry.register(worktrees).id;
    let agent = registry.create(repo_id, "feat/talk", None).unwrap().id;
    let mailbox = tmp.path().join("hooks.sock");
    let listener = bind_mailbox(&mailbox).unwrap();
    tokio::spawn(serve_mailbox(listener, registry.clone()));
    Fixture {
        registry,
        clock,
        mailbox,
        agent,
        tmp,
    }
}

/// Run `amux hook`'s delivery for one payload, off the async runtime (it is blocking, as the real
/// short-lived process is).
async fn hook(f: &Fixture, payload: serde_json::Value) {
    let sock = f.mailbox.clone();
    let agent = f.agent;
    let input = payload.to_string();
    tokio::task::spawn_blocking(move || deliver_hook_input(&sock, agent, &input))
        .await
        .unwrap()
        .expect("deliver");
}

async fn wait_for_turns(f: &Fixture, n: usize) -> Vec<amux_daemon::ConversationEntry> {
    for _ in 0..250 {
        let page = f
            .registry
            .turns(f.agent, 0, 200, usize::MAX)
            .unwrap()
            .unwrap();
        if page.turns.len() >= n {
            return page.turns;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never saw {n} turns");
}

async fn wait_for_state(f: &Fixture, want: impl Fn(&AgentState) -> bool) {
    for _ in 0..250 {
        let infos = f.registry.infos();
        if infos.iter().any(|a| a.id == f.agent && want(&a.state)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("agent never reached the expected state");
}

#[tokio::test]
async fn a_prompt_and_its_reply_are_recorded_with_the_daemons_clock() {
    let f = fixture().await;
    hook(
        &f,
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1",
                           "prompt": "synthetic prompt"}),
    )
    .await;
    wait_for_state(&f, |s| matches!(s, AgentState::Working)).await;
    // The turn is stamped when it is written (off the async workers), not with the status.
    wait_for_turns(&f, 1).await;
    f.clock.advance(chrono::Duration::seconds(30));
    hook(
        &f,
        serde_json::json!({"hook_event_name": "Stop", "session_id": "s1",
                           "stop_hook_active": false, "last_assistant_message": "synthetic reply"}),
    )
    .await;
    let turns = wait_for_turns(&f, 2).await;
    assert_eq!(turns[0].seq, 1);
    assert_eq!(turns[0].by, Speaker::User);
    assert_eq!(turns[0].text, "synthetic prompt");
    assert_eq!(turns[0].at, start());
    assert_eq!(turns[1].seq, 2);
    assert_eq!(turns[1].by, Speaker::Agent);
    assert_eq!(turns[1].at, start() + chrono::Duration::seconds(30));
    assert_eq!(turns[1].session.as_deref(), Some("s1"));
    wait_for_state(&f, |s| matches!(s, AgentState::Idle)).await;
}

/// The condition the clip exists for: a turn of 1 MiB on stdin is clipped below the mailbox's read
/// limit before encoding and is recorded, and the status report sent before it still arrives.
#[tokio::test]
async fn a_megabyte_turn_is_clipped_and_recorded_and_the_state_report_still_arrives() {
    let f = fixture().await;
    let huge = format!("BEGIN{}END", "m".repeat(1024 * 1024));
    hook(
        &f,
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1",
                           "prompt": huge}),
    )
    .await;
    wait_for_state(&f, |s| matches!(s, AgentState::Working)).await;
    let turns = wait_for_turns(&f, 1).await;
    assert!(turns[0].clipped);
    assert!(turns[0].text.len() <= MAX_TURN_TEXT_BYTES);
    assert!(turns[0].text.starts_with("BEGIN") && turns[0].text.ends_with("END"));
}

/// Two Stops with no prompt between (another Stop hook blocked the first) are two entries.
#[tokio::test]
async fn every_stop_with_a_reply_is_an_entry() {
    let f = fixture().await;
    for (n, (active, text)) in [(false, "first reply"), (true, "closing reply")]
        .into_iter()
        .enumerate()
    {
        hook(
            &f,
            serde_json::json!({"hook_event_name": "Stop", "session_id": "s1",
                               "stop_hook_active": active, "last_assistant_message": text}),
        )
        .await;
        // Claude runs one stop's hooks to completion before the next stop, so they arrive in turn.
        wait_for_turns(&f, n + 1).await;
    }
    let turns = wait_for_turns(&f, 2).await;
    let texts: Vec<&str> = turns.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, ["first reply", "closing reply"]);
}

/// A hook from an `amux hook` that predates turns sends only the status report: it is applied and
/// nothing is recorded. (The same bytes as before: `PaneMessage::Hook` is unchanged.)
#[tokio::test]
async fn a_hook_without_turn_fields_records_nothing() {
    let f = fixture().await;
    hook(
        &f,
        serde_json::json!({"hook_event_name": "PreToolUse", "session_id": "s1",
                           "tool_name": "Bash"}),
    )
    .await;
    wait_for_state(&f, |s| matches!(s, AgentState::Working)).await;
    let page = f
        .registry
        .turns(f.agent, 0, 200, usize::MAX)
        .unwrap()
        .unwrap();
    assert!(page.turns.is_empty());
    assert!(page.summary.is_none());
}

/// The log survives a daemon restart and its numbering goes on; deleting the worktree removes it.
#[tokio::test]
async fn the_log_survives_a_restart_and_goes_with_the_worktree() {
    let f = fixture().await;
    hook(
        &f,
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1",
                           "prompt": "before restart"}),
    )
    .await;
    wait_for_turns(&f, 1).await;

    let reloaded = Registry::with_options(
        Box::new(ClaudeAdapter::with_command(vec!["cat".into()])),
        RegistryOptions {
            state_path: Some(f.tmp.path().join("state.json")),
            conversation_dir: Some(f.tmp.path().join("conversations")),
            ..RegistryOptions::default()
        },
    );
    reloaded.load_state();
    let page = reloaded
        .turns(f.agent, 0, 200, usize::MAX)
        .unwrap()
        .unwrap();
    let summary = page.summary.expect("rebuilt from the file");
    assert_eq!((summary.first_seq, summary.last.seq), (1, 1));
    reloaded.on_turn(amux_core::hook::TurnReport {
        agent: f.agent,
        turn: amux_core::conversation::Turn::new(Speaker::Agent, "after restart", None),
    });
    let page = reloaded
        .turns(f.agent, 0, 200, usize::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(page.turns.last().unwrap().seq, 2);

    let file = f
        .tmp
        .path()
        .join("conversations")
        .join(format!("{}.jsonl", f.agent.to_full_string()));
    assert!(file.exists());
    reloaded.delete(f.agent, true).unwrap();
    assert!(!file.exists(), "removing the worktree removes its log");
    assert!(reloaded.turns(f.agent, 0, 200, usize::MAX).is_none());
}

/// A turn for an agent the daemon does not know is dropped, and creates no file.
#[tokio::test]
async fn a_turn_for_an_unknown_agent_is_dropped() {
    let f = fixture().await;
    let stranger = AgentId::new();
    f.registry.on_turn(amux_core::hook::TurnReport {
        agent: stranger,
        turn: amux_core::conversation::Turn::new(Speaker::User, "x", None),
    });
    let file = f
        .tmp
        .path()
        .join("conversations")
        .join(format!("{}.jsonl", stranger.to_full_string()));
    assert!(!file.exists());
}

/// A `Write` that appends into a shared buffer, for capturing everything the daemon logs.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sentinel text in a prompt and a reply reaches the conversation log and nowhere else: not the
/// daemon's log at any level (the mailbox's tasks run on this test's single thread, under the
/// capturing subscriber), not `state.json`.
#[tokio::test]
async fn turn_text_reaches_the_log_and_nowhere_else() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let f = fixture().await;
    const SENTINEL_PROMPT: &str = "SENTINEL-PROMPT-7f3a";
    const SENTINEL_REPLY: &str = "SENTINEL-REPLY-9c1e";
    hook(
        &f,
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1",
                           "prompt": SENTINEL_PROMPT}),
    )
    .await;
    wait_for_turns(&f, 1).await;
    hook(
        &f,
        serde_json::json!({"hook_event_name": "Stop", "session_id": "s1",
                           "last_assistant_message": SENTINEL_REPLY}),
    )
    .await;
    wait_for_turns(&f, 2).await;
    // A message the daemon cannot decode is logged (at debug); it must not quote anything.
    {
        use std::io::Write;
        let mut s = std::os::unix::net::UnixStream::connect(&f.mailbox).unwrap();
        s.write_all(SENTINEL_PROMPT.as_bytes()).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.registry.save();

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.is_empty(), "the capture is live");
    for sentinel in [SENTINEL_PROMPT, SENTINEL_REPLY] {
        assert!(
            !logs.contains(sentinel),
            "{sentinel} leaked into the daemon log"
        );
        let state = std::fs::read_to_string(f.tmp.path().join("state.json")).unwrap();
        assert!(
            !state.contains(sentinel),
            "{sentinel} leaked into state.json"
        );
    }
    let file = f
        .tmp
        .path()
        .join("conversations")
        .join(format!("{}.jsonl", f.agent.to_full_string()));
    let log = std::fs::read_to_string(file).unwrap();
    assert!(log.contains(SENTINEL_PROMPT) && log.contains(SENTINEL_REPLY));
}

/// `agent` in a worktree listing is the process of this daemon run: present while it runs and
/// after it exits in this run (as `exited`), absent for a worktree reloaded from `state.json`.
#[tokio::test]
async fn a_listing_reports_an_agent_only_for_this_run() {
    let f = fixture().await;
    let listed = |r: &Registry| {
        r.worktrees()
            .into_iter()
            .find(|w| w.id == f.agent)
            .expect("listed")
    };
    assert!(matches!(
        listed(&f.registry).agent.map(|a| a.state),
        Some(AgentState::Idle)
    ));

    let primary = f
        .registry
        .infos()
        .into_iter()
        .find(|a| a.id == f.agent)
        .unwrap()
        .primary_terminal;
    // End `cat` with an end-of-file.
    f.registry
        .session(primary)
        .unwrap()
        .write_input(b"\x04")
        .unwrap();
    wait_for_state(&f, |s| matches!(s, AgentState::Exited { .. })).await;
    assert!(matches!(
        listed(&f.registry).agent.map(|a| a.state),
        Some(AgentState::Exited { .. })
    ));

    f.registry.save();
    let reloaded = Registry::with_options(
        Box::new(ClaudeAdapter::with_command(vec!["cat".into()])),
        RegistryOptions {
            state_path: Some(f.tmp.path().join("state.json")),
            ..RegistryOptions::default()
        },
    );
    reloaded.load_state();
    let w = listed(&reloaded);
    assert!(w.agent.is_none(), "no process after a restart");
    assert_eq!(w.branch.as_deref(), Some("feat/talk"));
}

/// A log whose worktree is not in `state.json` (deleted while the daemon was down, or by an older
/// daemon) is removed at start, once the state has loaded; the logs of known worktrees and any
/// other file stay. If the state could not be read, nothing is removed.
#[tokio::test]
async fn orphaned_logs_are_removed_at_start() {
    let f = fixture().await;
    hook(
        &f,
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1",
                           "prompt": "kept"}),
    )
    .await;
    wait_for_turns(&f, 1).await;
    f.registry.save();
    let dir = f.tmp.path().join("conversations");
    let kept = dir.join(format!("{}.jsonl", f.agent.to_full_string()));
    let orphan = dir.join(format!("{}.jsonl", AgentId::new().to_full_string()));
    let orphan_tmp = dir.join(format!("{}.jsonl.tmp", AgentId::new().to_full_string()));
    let other = dir.join("notes.txt");
    for p in [&orphan, &orphan_tmp, &other] {
        std::fs::write(p, b"{\"seq\":1}\n").unwrap();
    }
    let restart = |state: &str| {
        let r = Registry::with_options(
            Box::new(ClaudeAdapter::with_command(vec!["cat".into()])),
            RegistryOptions {
                state_path: Some(f.tmp.path().join(state)),
                conversation_dir: Some(dir.clone()),
                ..RegistryOptions::default()
            },
        );
        r.load_state();
        r
    };

    // A state that cannot be read removes nothing.
    std::fs::write(f.tmp.path().join("damaged.json"), b"{not json").unwrap();
    restart("damaged.json");
    assert!(orphan.exists() && orphan_tmp.exists() && kept.exists());

    restart("state.json");
    assert!(!orphan.exists(), "the orphaned log is removed");
    assert!(!orphan_tmp.exists(), "and an orphaned temporary file");
    assert!(kept.exists(), "a known worktree's log stays");
    assert!(other.exists(), "a file that is not a log stays");
}

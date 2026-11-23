//! `amux worktree list` and `amux worktree turns`: read-only thin clients of the daemon, for a
//! status bar, a script or another tool. `amux worktree open` asks the terminal client you last
//! used to show a worktree, and otherwise changes nothing itself. They connect through
//! [`connect_to_running_daemon`](crate::client::connect_to_running_daemon), so they can never start
//! or replace a daemon; they send one read-only request, print the reply, and leave — changing
//! nothing (not unread, not last opened, not focus, not a terminal's size).
//!
//! `--json` output is produced from types owned here, not by serialising protocol types, so the
//! wire can evolve without changing what a script reads. `schema` is a number: fields may be added
//! without changing it; a change of meaning changes it.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use futures::{SinkExt, StreamExt};
use serde::Serialize;

use amux_core::agent::{AgentId, AgentState, AttentionKind};
use amux_core::conversation::Speaker;
use amux_proto::{ClientMsg, DaemonMsg, InstanceInfo, TurnInfo, WorktreeInfo};

use crate::client::{ConnectError, Connection};

/// The version of the `--json` documents.
pub const SCHEMA: u32 = 1;
/// The most turns one `amux worktree turns` prints, and its default.
pub const MAX_TURNS_LIMIT: u32 = 200;
/// How long a command waits for the daemon's reply once connected.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a command printed nothing. Each kind has its own exit code and one fixed line for standard
/// error, which never quotes a turn.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("no amux daemon is running")]
    NoDaemon,
    #[error("the running amux daemon is a different version (open amux to upgrade it)")]
    VersionDiffers,
    #[error("no such worktree")]
    NoSuchWorktree,
    #[error("not a worktree id (use the id `amux worktree list` prints)")]
    BadId,
    #[error("no amux terminal client is attached")]
    NoClient,
    #[error("could not read from the amux daemon: {0:#}")]
    Other(anyhow::Error),
}

impl CommandError {
    /// The process exit code for this outcome: 3 no daemon, 4 another version, 5 no terminal
    /// client attached (`open`), 1 anything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            CommandError::NoDaemon => 3,
            CommandError::VersionDiffers => 4,
            CommandError::NoClient => 5,
            CommandError::NoSuchWorktree | CommandError::BadId | CommandError::Other(_) => 1,
        }
    }
}

impl From<ConnectError> for CommandError {
    fn from(e: ConnectError) -> Self {
        match e {
            ConnectError::NoDaemon => CommandError::NoDaemon,
            ConnectError::VersionDiffers => CommandError::VersionDiffers,
            ConnectError::Other(e) => CommandError::Other(e),
        }
    }
}

/// `amux worktree list [--json]`: what to print on standard output.
pub async fn list(json: bool) -> Result<String, CommandError> {
    let connection = crate::client::connect_to_running_daemon().await?;
    list_over(connection, json).await
}

/// `amux worktree turns <id> [--after n] [--limit n] [--json]`: what to print on standard output.
pub async fn turns(id: &str, after: u64, limit: u32, json: bool) -> Result<String, CommandError> {
    let worktree = AgentId::parse(id).ok_or(CommandError::BadId)?;
    let connection = crate::client::connect_to_running_daemon().await?;
    turns_over(connection, worktree, after, limit, json).await
}

/// `amux worktree open <id>`: ask the terminal client you last used to show the worktree. Prints
/// nothing. Never starts a client, an agent or a window.
pub async fn open(id: &str) -> Result<String, CommandError> {
    let worktree = AgentId::parse(id).ok_or(CommandError::BadId)?;
    let connection = crate::client::connect_to_running_daemon().await?;
    open_over(connection, worktree).await
}

pub(crate) async fn open_over(
    mut connection: Connection,
    worktree: AgentId,
) -> Result<String, CommandError> {
    send(&mut connection, ClientMsg::OpenWorktree { worktree }).await?;
    reply(&mut connection, |msg| match msg {
        DaemonMsg::WorktreeOpened { worktree: w } if w == worktree => Some(Ok(String::new())),
        DaemonMsg::NoClientAttached { worktree: w } if w == worktree => {
            Some(Err(CommandError::NoClient))
        }
        DaemonMsg::NoSuchWorktree { worktree: w } if w == worktree => {
            Some(Err(CommandError::NoSuchWorktree))
        }
        _ => None,
    })
    .await
}

pub(crate) async fn list_over(
    mut connection: Connection,
    json: bool,
) -> Result<String, CommandError> {
    send(&mut connection, ClientMsg::ListWorktrees).await?;
    let (instance, worktrees) = reply(&mut connection, |msg| match msg {
        DaemonMsg::Worktrees {
            instance,
            worktrees,
        } => Some(Ok((instance, worktrees))),
        _ => None,
    })
    .await?;
    Ok(if json {
        render_list_json(&instance, &worktrees)
    } else {
        render_list_table(&worktrees)
    })
}

pub(crate) async fn turns_over(
    mut connection: Connection,
    worktree: AgentId,
    after: u64,
    limit: u32,
    json: bool,
) -> Result<String, CommandError> {
    let limit = limit.min(MAX_TURNS_LIMIT);
    send(
        &mut connection,
        ClientMsg::Turns {
            worktree,
            after,
            limit,
        },
    )
    .await?;
    let page = reply(&mut connection, |msg| match msg {
        DaemonMsg::Turns {
            instance,
            worktree: w,
            first_seq,
            last_seq,
            turns,
        } if w == worktree => Some(Ok(TurnsPage {
            instance,
            worktree,
            first_seq,
            last_seq,
            turns,
        })),
        DaemonMsg::NoSuchWorktree { worktree: w } if w == worktree => {
            Some(Err(CommandError::NoSuchWorktree))
        }
        _ => None,
    })
    .await?;
    Ok(if json {
        render_turns_json(&page)
    } else {
        render_turns_text(&page)
    })
}

/// A `Turns` reply, gathered.
pub(crate) struct TurnsPage {
    pub instance: InstanceInfo,
    pub worktree: AgentId,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub turns: Vec<TurnInfo>,
}

async fn send(connection: &mut Connection, msg: ClientMsg) -> Result<(), CommandError> {
    connection
        .send(msg)
        .await
        .map_err(|e| CommandError::Other(anyhow::Error::new(e).context("send request")))
}

/// Read frames until `pick` recognises the reply, skipping the connect burst and any broadcast in
/// between. A daemon-side `Error` (only ever sent to the connection that caused it) ends the wait.
async fn reply<T>(
    connection: &mut Connection,
    mut pick: impl FnMut(DaemonMsg) -> Option<Result<T, CommandError>>,
) -> Result<T, CommandError> {
    let wait = async {
        while let Some(frame) = connection.next().await {
            let msg = frame
                .map_err(|e| CommandError::Other(anyhow::Error::new(e).context("read reply")))?;
            if let DaemonMsg::Error { message } = msg {
                return Err(CommandError::Other(anyhow::anyhow!(message)));
            }
            if let Some(result) = pick(msg) {
                return result;
            }
        }
        Err(CommandError::Other(anyhow::anyhow!(
            "the daemon closed the connection before replying"
        )))
    };
    tokio::time::timeout(REPLY_TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| {
            Err(CommandError::Other(anyhow::anyhow!(
                "the daemon did not reply within {REPLY_TIMEOUT:?}"
            )))
        })
}

// --- The `--json` documents: owned here, independent of the wire types. ---

#[derive(Serialize)]
struct InstanceJson {
    id: String,
    daemon_started_at: String,
}

#[derive(Serialize)]
struct ListJson {
    schema: u32,
    instance: InstanceJson,
    worktrees: Vec<WorktreeJson>,
}

#[derive(Serialize)]
struct RepositoryJson {
    id: String,
    name: String,
    path: String,
}

#[derive(Serialize)]
struct WorktreeJson {
    id: String,
    repository: RepositoryJson,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<String>,
    last_opened: String,
    unread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<AgentJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation: Option<ConversationJson>,
}

#[derive(Serialize)]
struct AgentJson {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
    last_activity: String,
}

#[derive(Serialize)]
struct ConversationJson {
    first_seq: u64,
    last_seq: u64,
    last: TurnHeadJson,
}

#[derive(Serialize)]
struct TurnHeadJson {
    seq: u64,
    by: &'static str,
    at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

#[derive(Serialize)]
struct TurnsJson {
    schema: u32,
    instance: InstanceJson,
    worktree: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seq: Option<u64>,
    turns: Vec<TurnJson>,
}

#[derive(Serialize)]
struct TurnJson {
    seq: u64,
    by: &'static str,
    at: String,
    text: String,
    clipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

/// RFC 3339 in UTC with a `Z`, to the second.
fn time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn speaker(by: Speaker) -> &'static str {
    match by {
        Speaker::User => "user",
        Speaker::Agent => "agent",
    }
}

fn instance_json(i: &InstanceInfo) -> InstanceJson {
    InstanceJson {
        id: i.id.clone(),
        daemon_started_at: time(i.daemon_started_at),
    }
}

/// The state's stable name, and the fields that go with it.
fn agent_json(state: &AgentState, last_activity: DateTime<Utc>) -> AgentJson {
    let mut out = AgentJson {
        state: "",
        kind: None,
        message: None,
        code: None,
        last_activity: time(last_activity),
    };
    match state {
        AgentState::Starting => out.state = "starting",
        AgentState::Working => out.state = "working",
        AgentState::NeedsAttention { kind, message } => {
            out.state = "needs_attention";
            out.kind = Some(match kind {
                AttentionKind::Permission => "permission",
                AttentionKind::Question => "question",
                AttentionKind::Info => "info",
            });
            out.message = message.clone();
        }
        AgentState::Idle => out.state = "idle",
        AgentState::Exited { code } => {
            out.state = "exited";
            out.code = *code;
        }
        AgentState::Error { message } => {
            out.state = "error";
            out.message = Some(message.clone());
        }
    }
    out
}

fn worktree_json(w: &WorktreeInfo) -> WorktreeJson {
    WorktreeJson {
        id: w.id.to_full_string(),
        repository: RepositoryJson {
            id: w.repo.id.to_string(),
            name: w.repo.name.clone(),
            path: w.repo.path.to_string_lossy().into_owned(),
        },
        name: w.name.clone(),
        branch: w.branch.clone(),
        path: w.path.to_string_lossy().into_owned(),
        task: w.task.clone(),
        last_opened: time(w.last_opened),
        unread: w.unread,
        session: w.session.clone(),
        agent: w
            .agent
            .as_ref()
            .map(|a| agent_json(&a.state, a.last_activity)),
        conversation: w.conversation.as_ref().map(|c| ConversationJson {
            first_seq: c.first_seq,
            last_seq: c.last_seq,
            last: TurnHeadJson {
                seq: c.last.seq,
                by: speaker(c.last.by),
                at: time(c.last.at),
                session: c.last.session.clone(),
            },
        }),
    }
}

fn to_json_line(value: &impl Serialize) -> String {
    // Serialising these plain structs cannot fail: every key is a string, every value finite.
    let mut s = serde_json::to_string_pretty(value).unwrap_or_default();
    s.push('\n');
    s
}

pub(crate) fn render_list_json(instance: &InstanceInfo, worktrees: &[WorktreeInfo]) -> String {
    to_json_line(&ListJson {
        schema: SCHEMA,
        instance: instance_json(instance),
        worktrees: worktrees.iter().map(worktree_json).collect(),
    })
}

pub(crate) fn render_turns_json(page: &TurnsPage) -> String {
    to_json_line(&TurnsJson {
        schema: SCHEMA,
        instance: instance_json(&page.instance),
        worktree: page.worktree.to_full_string(),
        first_seq: page.first_seq,
        last_seq: page.last_seq,
        turns: page
            .turns
            .iter()
            .map(|t| TurnJson {
                seq: t.seq,
                by: speaker(t.by),
                at: time(t.at),
                text: t.text.clone(),
                clipped: t.clipped,
                session: t.session.clone(),
            })
            .collect(),
    })
}

// --- Human-readable output. ---
//
// Everything in these renderers that did not come from amux itself — turn text, but also session
// ids, names, branches, paths and messages — is passed through [`safe_text`] or [`safe_line`]
// first: an agent's reply can carry escape sequences (a clipboard write, a title change, a clear
// screen) that would otherwise reach the user's terminal. JSON output needs none of this: it
// escapes every control character.

/// `s` with everything a terminal would act on replaced by a visible stand-in, keeping its
/// newlines and tabs: for multi-line text.
pub fn safe_text(s: &str) -> String {
    replace_controls(s, true)
}

/// [`safe_text`] for a value that must stay on one line (a table cell, a heading, an error
/// message): newlines and tabs are replaced too.
pub fn safe_line(s: &str) -> String {
    replace_controls(s, false)
}

/// Replace C0 controls (keeping newline and tab when `keep_layout`), DEL and C1 controls, and
/// the characters that reorder text (the bidi embeddings, overrides and isolates). A C0 control
/// or DEL becomes its Unicode control picture (ESC is `␛`), so what was there stays readable; a C1
/// control or a reordering character becomes `�`.
fn replace_controls(s: &str, keep_layout: bool) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\t' if keep_layout => c,
            '\0'..='\x1f' => char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'),
            '\x7f' => '\u{2421}',
            '\u{80}'..='\u{9f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '\u{fffd}',
            _ => c,
        })
        .collect()
}

/// One row per worktree: id, repo/name, the agent's state, unread, and the last recorded turn.
pub(crate) fn render_list_table(worktrees: &[WorktreeInfo]) -> String {
    if worktrees.is_empty() {
        return "no worktrees\n".to_string();
    }
    let rows: Vec<[String; 5]> = worktrees
        .iter()
        .map(|w| {
            let state = match &w.agent {
                Some(a) => agent_json(&a.state, a.last_activity).state.to_string(),
                None => "-".to_string(),
            };
            let last = match &w.conversation {
                Some(c) => format!(
                    "#{} {} {}",
                    c.last.seq,
                    speaker(c.last.by),
                    c.last.at.format("%Y-%m-%d %H:%M")
                ),
                None => "-".to_string(),
            };
            [
                safe_line(&w.id.to_full_string()),
                safe_line(&format!("{}/{}", w.repo.name, w.name)),
                safe_line(&state),
                if w.unread { "yes" } else { "" }.to_string(),
                safe_line(&last),
            ]
        })
        .collect();
    let header = ["ID", "WORKTREE", "AGENT", "UNREAD", "LAST TURN"].map(String::from);
    let mut widths = header.clone().map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let mut line = String::new();
        for (i, (cell, width)) in row.iter().zip(widths).enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}  "));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Each turn as a heading (`#seq who time`) and its text; a change of session — where the
/// conversation was cleared or restarted — gets a line of its own.
pub(crate) fn render_turns_text(page: &TurnsPage) -> String {
    if page.turns.is_empty() {
        return match page.last_seq {
            Some(last) => format!("no turns after that (the log ends at #{last})\n"),
            None => "no turns recorded\n".to_string(),
        };
    }
    let mut out = String::new();
    let mut session: Option<&Option<String>> = None;
    for t in &page.turns {
        if session.is_some_and(|s| *s != t.session) {
            out.push_str(&format!(
                "-- new session {} --\n\n",
                safe_line(t.session.as_deref().unwrap_or("(none)"))
            ));
        }
        session = Some(&t.session);
        let clipped = if t.clipped { " (clipped)" } else { "" };
        out.push_str(&format!(
            "#{} {} {}{clipped}\n{}\n\n",
            t.seq,
            speaker(t.by),
            time(t.at),
            safe_text(t.text.trim_end())
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::agent::RepoId;
    use amux_proto::{AgentStatus, ConversationInfo, RepoInfo, TurnHead};
    use std::path::PathBuf;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn id(n: u8) -> AgentId {
        AgentId::parse(&format!("00000000-0000-4000-8000-0000000000{n:02x}")).unwrap()
    }

    fn instance() -> InstanceInfo {
        InstanceInfo {
            id: "6f1d2c3b-4a59-4e68-9d7c-8b9a0f1e2d3c".into(),
            daemon_started_at: at("2026-11-22T09:00:00Z"),
        }
    }

    fn repo() -> RepoInfo {
        RepoInfo {
            id: RepoId::from_canonical_path(&PathBuf::from("/src/demo")),
            name: "demo".into(),
            path: PathBuf::from("/src/demo"),
        }
    }

    /// The cases the golden file pins: an agent asking for permission, with its conversation; one
    /// that exited in this run; a HEAD session with no branch and no conversation; a worktree with
    /// no agent (suspended, or after a restart) whose session differs from its last entry's.
    fn worktrees() -> Vec<WorktreeInfo> {
        let base = |n: u8, name: &str| WorktreeInfo {
            id: id(n),
            repo: repo(),
            name: name.into(),
            branch: Some(format!("feat/{name}")),
            path: PathBuf::from(format!("/home/u/.amux/worktrees/demo-1/{name}")),
            task: None,
            last_opened: at("2026-11-22T10:00:00Z"),
            unread: false,
            session: Some(format!("session-{name}")),
            agent: None,
            conversation: None,
        };
        let conversation = |last_seq: u64, by: Speaker, session: &str| {
            Some(ConversationInfo {
                first_seq: 1,
                last_seq,
                last: TurnHead {
                    seq: last_seq,
                    by,
                    at: at("2026-11-22T10:15:00Z"),
                    session: Some(session.into()),
                },
            })
        };
        vec![
            WorktreeInfo {
                task: Some("synthetic task".into()),
                unread: true,
                agent: Some(AgentStatus {
                    state: AgentState::NeedsAttention {
                        kind: AttentionKind::Permission,
                        message: Some("synthetic permission request".into()),
                    },
                    last_activity: at("2026-11-22T10:16:00Z"),
                }),
                conversation: conversation(42, Speaker::Agent, "session-auth"),
                ..base(1, "auth")
            },
            WorktreeInfo {
                agent: Some(AgentStatus {
                    state: AgentState::Exited { code: Some(1) },
                    last_activity: at("2026-11-22T10:05:00Z"),
                }),
                conversation: conversation(3, Speaker::User, "session-cli"),
                ..base(2, "cli")
            },
            WorktreeInfo {
                name: "HEAD".into(),
                branch: None,
                path: PathBuf::from("/src/demo"),
                session: None,
                agent: Some(AgentStatus {
                    state: AgentState::Idle,
                    last_activity: at("2026-11-22T09:30:00Z"),
                }),
                ..base(3, "head")
            },
            WorktreeInfo {
                conversation: conversation(7, Speaker::Agent, "session-before-clear"),
                ..base(4, "docs")
            },
        ]
    }

    fn page() -> TurnsPage {
        let turn = |seq: u64, by: Speaker, text: &str, session: &str| TurnInfo {
            seq,
            by,
            at: at(&format!("2026-11-22T10:{:02}:00Z", seq)),
            text: text.into(),
            clipped: false,
            session: Some(session.into()),
        };
        TurnsPage {
            instance: instance(),
            worktree: id(1),
            first_seq: Some(1),
            last_seq: Some(42),
            turns: vec![
                turn(40, Speaker::Agent, "synthetic earlier reply", "session-a"),
                turn(41, Speaker::User, "synthetic prompt", "session-b"),
                TurnInfo {
                    clipped: true,
                    ..turn(
                        42,
                        Speaker::Agent,
                        "synthetic head\n[… clipped by amux …]\nsynthetic tail",
                        "session-b",
                    )
                },
            ],
        }
    }

    /// Compare with a golden file; `AMUX_BLESS=1` rewrites it.
    fn golden(name: &str, got: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        if std::env::var_os("AMUX_BLESS").is_some() {
            std::fs::write(&path, got).unwrap();
        }
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            got,
            want,
            "{} differs (AMUX_BLESS=1 to rewrite)",
            path.display()
        );
    }

    #[test]
    fn list_json_matches_its_golden_file() {
        golden(
            "worktree-list.json",
            &render_list_json(&instance(), &worktrees()),
        );
    }

    #[test]
    fn turns_json_matches_its_golden_file() {
        golden("worktree-turns.json", &render_turns_json(&page()));
    }

    #[test]
    fn an_empty_turns_page_omits_the_bounds() {
        let empty = TurnsPage {
            first_seq: None,
            last_seq: None,
            turns: vec![],
            ..page()
        };
        golden("worktree-turns-empty.json", &render_turns_json(&empty));
    }

    #[test]
    fn list_table_matches_its_golden_file() {
        golden("worktree-list.txt", &render_list_table(&worktrees()));
        assert_eq!(render_list_table(&[]), "no worktrees\n");
    }

    #[test]
    fn turns_text_matches_its_golden_file() {
        golden("worktree-turns.txt", &render_turns_text(&page()));
    }

    /// Text that would drive the terminal — ESC (a colour, a clear screen), BEL, an OSC 52
    /// clipboard write, a C1 CSI, a bidi override that reorders what follows — arrives as visible
    /// stand-ins in every field the text renderers print, while newlines and tabs in a turn keep
    /// their layout.
    #[test]
    fn text_output_shows_control_characters_instead_of_obeying_them() {
        const HOSTILE: &str =
            "a\x1b[2Jb\x07c\x1b]52;c;c2VjcmV0\x07d\u{9b}31me\u{202e}f\u{2066}g\x7fh\u{1}";
        let mut list = worktrees();
        list[0].name = format!("auth{HOSTILE}");
        list[0].repo.name = format!("demo\n{HOSTILE}");
        golden("worktree-list-hostile.txt", &render_list_table(&list[..1]));
        let mut p = page();
        p.turns[0].text = format!("line one\n\tindented {HOSTILE}\nline three");
        p.turns[1].session = Some(format!("session\n{HOSTILE}"));
        let out = render_turns_text(&p);
        golden("worktree-turns-hostile.txt", &out);
        for bad in [
            '\x1b', '\x07', '\u{9b}', '\u{202e}', '\u{2066}', '\x7f', '\u{1}',
        ] {
            assert!(!out.contains(bad), "{bad:?} reached the terminal");
            assert!(!render_list_table(&list).contains(bad));
        }
        assert!(out.contains("line one\n\tindented"), "layout is kept");
        assert_eq!(safe_line("a\tb\nc"), "a\u{2409}b\u{240a}c");
    }

    #[test]
    fn exit_codes() {
        assert_eq!(CommandError::NoDaemon.exit_code(), 3);
        assert_eq!(CommandError::VersionDiffers.exit_code(), 4);
        assert_eq!(CommandError::NoClient.exit_code(), 5);
        assert_eq!(CommandError::NoSuchWorktree.exit_code(), 1);
        assert_eq!(CommandError::BadId.exit_code(), 1);
        assert_eq!(CommandError::Other(anyhow::anyhow!("x")).exit_code(), 1);
    }

    /// `turns` refuses an id it cannot parse before connecting to anything.
    #[tokio::test]
    async fn a_bad_id_is_refused_before_connecting() {
        assert!(matches!(
            turns("not-an-id", 0, 10, true).await,
            Err(CommandError::BadId)
        ));
    }

    // --- End to end, in-process: a real registry and control server on a temp socket. ---

    use amux_core::adapter::ClaudeAdapter;
    use amux_core::clock::ManualClock;
    use amux_core::conversation::Turn;
    use amux_core::hook::{HookEvent, HookReport, TurnReport};
    use amux_daemon::{serve, Registry, RegistryOptions};
    use amux_proto::{ClientCodec, PROTO_VERSION};
    use std::path::Path;
    use std::sync::Arc;
    use tokio::net::{UnixListener, UnixStream};
    use tokio_util::codec::Framed;

    use crate::client::connect_to_running_daemon_at;

    fn init_repo(dir: &Path) {
        let repo = git2::Repository::init(dir).unwrap();
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

    struct Daemon {
        registry: Arc<Registry>,
        socket: PathBuf,
        _tmp: tempfile::TempDir,
    }

    async fn daemon() -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let registry = Registry::with_options(
            Box::new(ClaudeAdapter::with_command(vec!["cat".into()])),
            RegistryOptions {
                conversation_dir: Some(tmp.path().join("conversations")),
                clock: Arc::new(ManualClock::new(at("2026-11-22T10:15:00Z"))),
                instance_id: Some("6f1d2c3b-4a59-4e68-9d7c-8b9a0f1e2d3c".into()),
                ..RegistryOptions::default()
            },
        );
        let worktrees =
            amux_core::worktree::WorktreeService::with_base(&repo, tmp.path().join("wt")).unwrap();
        registry.register(worktrees);
        let socket = tmp.path().join("amuxd.sock");
        tokio::spawn(serve(
            UnixListener::bind(&socket).unwrap(),
            registry.clone(),
        ));
        Daemon {
            registry,
            socket,
            _tmp: tmp,
        }
    }

    async fn connect(d: &Daemon) -> Connection {
        connect_to_running_daemon_at(&d.socket, Duration::from_secs(2))
            .await
            .unwrap()
    }

    fn hook(agent: AgentId, name: &str) -> HookReport {
        HookReport {
            agent,
            event: HookEvent {
                hook_event_name: name.into(),
                session_id: Some("s1".into()),
                notification_type: None,
                message: None,
                tool_name: None,
            },
        }
    }

    /// Finish a turn on `agent` (Working, then Idle): a notable transition.
    fn finish(registry: &Registry, agent: AgentId) {
        registry.on_hook(hook(agent, "PreToolUse"));
        registry.on_hook(hook(agent, "Stop"));
    }

    fn bits(registry: &Registry) -> Vec<(AgentId, bool, DateTime<Utc>)> {
        let mut v: Vec<_> = registry
            .infos()
            .into_iter()
            .map(|a| (a.id, a.unread, a.last_opened))
            .collect();
        v.sort_by_key(|(id, ..)| id.as_uuid());
        v
    }

    #[tokio::test]
    async fn list_and_turns_read_the_daemon_and_change_nothing() {
        let d = daemon().await;
        let repo = d.registry.repos()[0].id;
        let watched = d.registry.create(repo, "feat/watched", None).unwrap().id;
        let other = d.registry.create(repo, "feat/other", None).unwrap().id;
        for (by, text) in [
            (Speaker::User, "synthetic prompt"),
            (Speaker::Agent, "synthetic reply"),
        ] {
            d.registry.on_turn(TurnReport {
                agent: watched,
                turn: Turn::new(by, text, Some("s1".into())),
            });
        }
        // A terminal client watching `watched`; `other` finishes unwatched and goes unread.
        let mut viewer = Framed::new(
            UnixStream::connect(&d.socket).await.unwrap(),
            ClientCodec::new(),
        );
        viewer
            .send(ClientMsg::Hello {
                proto_version: PROTO_VERSION,
            })
            .await
            .unwrap();
        viewer
            .send(ClientMsg::Focus {
                agent: Some(watched),
            })
            .await
            .unwrap();
        viewer.send(ClientMsg::ListAgents).await.unwrap();
        loop {
            if let Some(Ok(DaemonMsg::OpenedChanged { id, .. })) = viewer.next().await {
                if id == watched {
                    break;
                }
            }
        }
        finish(&d.registry, other);
        let before = bits(&d.registry);

        // `list`
        let out = list_over(connect(&d).await, true).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["schema"], 1);
        assert_eq!(
            doc["instance"]["id"],
            "6f1d2c3b-4a59-4e68-9d7c-8b9a0f1e2d3c"
        );
        let rows = doc["worktrees"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let row = |id: AgentId| {
            rows.iter()
                .find(|r| r["id"] == id.to_full_string())
                .unwrap()
                .clone()
        };
        assert_eq!(row(watched)["conversation"]["last_seq"], 2);
        assert_eq!(row(watched)["conversation"]["last"]["by"], "agent");
        assert_eq!(
            row(watched)["conversation"]["last"]["at"],
            "2026-11-22T10:15:00Z"
        );
        assert_eq!(row(watched)["agent"]["state"], "idle");
        assert_eq!(row(other)["unread"], true);
        assert!(row(other).get("conversation").is_none());
        assert!(list_over(connect(&d).await, false)
            .await
            .unwrap()
            .contains("repo/watched"));

        // `turns`, paged with --after and --limit
        let page = |after, limit| {
            let socket = d.socket.clone();
            async move {
                let c = connect_to_running_daemon_at(&socket, Duration::from_secs(2))
                    .await
                    .unwrap();
                let out = turns_over(c, watched, after, limit, true).await.unwrap();
                serde_json::from_str::<serde_json::Value>(&out).unwrap()
            }
        };
        let first = page(0, 1).await;
        assert_eq!(first["turns"].as_array().unwrap().len(), 1);
        assert_eq!(first["turns"][0]["seq"], 1);
        assert_eq!(first["turns"][0]["text"], "synthetic prompt");
        assert_eq!(
            (first["first_seq"].clone(), first["last_seq"].clone()),
            (1.into(), 2.into())
        );
        let rest = page(1, 200).await;
        assert_eq!(rest["turns"][0]["seq"], 2);
        assert_eq!(rest["turns"][0]["by"], "agent");
        assert!(page(2, 200).await["turns"].as_array().unwrap().is_empty());

        // An unknown worktree is exit 1.
        let err = turns_over(connect(&d).await, AgentId::new(), 0, 10, true)
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::NoSuchWorktree));
        assert_eq!(err.exit_code(), 1);

        // Nothing changed: unread and last opened are as they were...
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            bits(&d.registry),
            before,
            "the commands changed no unread or last opened"
        );
        // ...and the viewer's focus survived the commands coming and going: a finish on the
        // agent it watches stays read.
        finish(&d.registry, watched);
        let unread = bits(&d.registry)
            .into_iter()
            .find(|(id, ..)| *id == watched)
            .unwrap()
            .1;
        assert!(!unread, "the viewer's focus is undisturbed");
    }

    /// Exit 3 with no daemon and exit 4 with another version — through the same mapping `amux`
    /// uses — and in neither case is anything started or replaced.
    #[tokio::test]
    async fn no_daemon_is_exit_3_and_another_version_is_exit_4() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("amuxd.sock");
        let err: CommandError = connect_to_running_daemon_at(&socket, Duration::from_millis(500))
            .await
            .err()
            .unwrap()
            .into();
        assert_eq!(err.exit_code(), 3);
        assert!(!socket.exists());

        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut framed = Framed::new(stream, amux_proto::ServerCodec::new());
                let _ = framed.next().await;
                let _ = framed
                    .send(DaemonMsg::Hello {
                        proto_version: PROTO_VERSION - 1,
                    })
                    .await;
            }
        });
        let err: CommandError = connect_to_running_daemon_at(&socket, Duration::from_millis(500))
            .await
            .err()
            .unwrap()
            .into();
        assert_eq!(err.exit_code(), 4);
        assert!(
            UnixStream::connect(&socket).await.is_ok(),
            "the other daemon is still there"
        );
    }

    /// A terminal client in the test: handshake, then optionally a focus (which counts as the user
    /// acting in it).
    async fn terminal_client(d: &Daemon, focus: Option<AgentId>) -> Connection {
        let mut c = Framed::new(
            UnixStream::connect(&d.socket).await.unwrap(),
            ClientCodec::new(),
        );
        c.send(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        })
        .await
        .unwrap();
        c.send(ClientMsg::Focus { agent: focus }).await.unwrap();
        // Round-trip so the daemon has applied it.
        c.send(ClientMsg::ListWorktrees).await.unwrap();
        loop {
            if let Some(Ok(DaemonMsg::Worktrees { .. })) = c.next().await {
                return c;
            }
        }
    }

    /// Whether `c` is asked to show `id` within a short wait.
    async fn asked_to_show(c: &mut Connection, id: AgentId) -> bool {
        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                match c.next().await {
                    Some(Ok(DaemonMsg::ShowWorktree { worktree })) => return worktree == id,
                    Some(Ok(_)) => {}
                    _ => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    #[tokio::test]
    async fn open_asks_the_most_recently_used_client_and_only_it() {
        let d = daemon().await;
        let repo = d.registry.repos()[0].id;
        let a = d.registry.create(repo, "feat/a", None).unwrap().id;
        let b = d.registry.create(repo, "feat/b", None).unwrap().id;
        let mut older = terminal_client(&d, Some(a)).await;
        let mut newer = terminal_client(&d, Some(a)).await;
        // The user goes back to the first client: it is now the most recently used.
        older.send(ClientMsg::Focus { agent: None }).await.unwrap();
        older.send(ClientMsg::ListWorktrees).await.unwrap();
        loop {
            if let Some(Ok(DaemonMsg::Worktrees { .. })) = older.next().await {
                break;
            }
        }
        let agents_before = d.registry.infos().len();

        assert_eq!(open_over(connect(&d).await, b).await.unwrap(), "");
        assert!(
            asked_to_show(&mut older, b).await,
            "the client used last is asked"
        );
        assert!(!asked_to_show(&mut newer, b).await, "the other is not");
        assert_eq!(
            d.registry.infos().len(),
            agents_before,
            "nothing was created"
        );

        let err = open_over(connect(&d).await, AgentId::new())
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::NoSuchWorktree));
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    async fn open_with_no_client_attached_is_exit_5_and_starts_nothing() {
        let d = daemon().await;
        let repo = d.registry.repos()[0].id;
        let a = d.registry.create(repo, "feat/a", None).unwrap().id;
        // Another command connected at the same time is not a terminal client.
        let _bystander = connect(&d).await;
        let err = open_over(connect(&d).await, a).await.unwrap_err();
        assert!(matches!(err, CommandError::NoClient));
        assert_eq!(err.exit_code(), 5);
        assert_eq!(d.registry.infos().len(), 1);
    }
}

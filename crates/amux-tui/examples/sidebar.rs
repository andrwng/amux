//! Phase 1 DESIGN PREVIEW — throwaway, fake data. Not wired to the daemon.
//!
//! A live look at the sidebar-and-main layout so we can tune glyphs/colors/spacing/**ordering**
//! before building the real multi-agent backend (1.4–1.6). The roster is ordered by the real
//! `amux_core::agent::sort_for_sidebar` (needs-attention first, then most-recent activity), and
//! each row shows an age so the recency ordering is visible. Run in a real terminal:
//!
//!     cargo run -p amux-tui --example sidebar
//!
//! `j`/`k` (or arrows) move the selection; `q`/`Esc` quits.

use amux_core::agent::{sort_for_sidebar, AgentId, AgentState, AttentionKind, RosterItem};
use chrono::{DateTime, Duration, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};

struct Agent {
    id: AgentId,
    name: &'static str,
    branch: &'static str,
    state: AgentState,
    last_activity: DateTime<Utc>,
    body: &'static [&'static str],
}

fn fake_agents() -> Vec<Agent> {
    let ago = |mins: i64| Utc::now() - Duration::minutes(mins);
    vec![
        Agent {
            id: AgentId::new(),
            name: "auth",
            branch: "feat/auth",
            state: AgentState::Working,
            last_activity: ago(2),
            body: &[
                "\u{23fa} Edit src/login.rs",
                "\u{23fa} Bash cargo build",
                "\u{25cf} wiring the refresh-token path\u{2026}",
            ],
        },
        Agent {
            id: AgentId::new(),
            name: "api",
            branch: "feat/api-refactor",
            state: AgentState::NeedsAttention {
                kind: AttentionKind::Permission,
                message: Some("Run `cargo test --all`?".to_string()),
            },
            last_activity: ago(5),
            body: &[
                "I'd like to run the full test suite to confirm the refactor.",
                "",
                "  \u{276f} 1 Yes    2 Yes, don't ask again    3 No",
            ],
        },
        Agent {
            id: AgentId::new(),
            name: "docs",
            branch: "docs/readme",
            state: AgentState::Idle,
            last_activity: ago(12),
            body: &["Done. Updated the README install section.", ""],
        },
        Agent {
            id: AgentId::new(),
            name: "infra",
            branch: "feat/infra",
            state: AgentState::NeedsAttention {
                kind: AttentionKind::Question,
                message: Some("Which region should I deploy to first?".to_string()),
            },
            last_activity: ago(1),
            body: &[
                "A few clarifying questions before I proceed:",
                "",
                "  1. Which region first?",
            ],
        },
        Agent {
            id: AgentId::new(),
            name: "search",
            branch: "feat/search",
            state: AgentState::Starting,
            last_activity: Utc::now(),
            body: &["starting claude\u{2026}"],
        },
        Agent {
            id: AgentId::new(),
            name: "payments",
            branch: "fix/payments",
            state: AgentState::Exited { code: Some(0) },
            last_activity: ago(30),
            body: &["session exited (0)"],
        },
    ]
}

fn color_for(state: &AgentState) -> Color {
    match state {
        AgentState::Working => Color::Green,
        AgentState::NeedsAttention { .. } => Color::Yellow,
        AgentState::Idle => Color::Gray,
        AgentState::Starting => Color::Cyan,
        AgentState::Exited { .. } => Color::DarkGray,
        AgentState::Error { .. } => Color::Red,
    }
}

fn label_for(state: &AgentState) -> String {
    match state {
        AgentState::Working => "Working".to_string(),
        AgentState::NeedsAttention { kind, .. } => match kind {
            AttentionKind::Permission => "Needs permission".to_string(),
            AttentionKind::Question => "Needs an answer".to_string(),
            AttentionKind::Info => "Needs you".to_string(),
        },
        AgentState::Idle => "Idle".to_string(),
        AgentState::Starting => "Starting".to_string(),
        AgentState::Exited { code } => format!("Exited ({})", code.unwrap_or(-1)),
        AgentState::Error { .. } => "Error".to_string(),
    }
}

fn age(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn main() -> std::io::Result<()> {
    let agents = fake_agents();

    // Order the roster with the real policy from amux-core.
    let mut roster: Vec<RosterItem> = agents
        .iter()
        .map(|a| RosterItem {
            id: a.id,
            state: a.state.clone(),
            unread: false,
            last_activity: a.last_activity,
        })
        .collect();
    sort_for_sidebar(&mut roster);
    let ordered: Vec<&Agent> = roster
        .iter()
        .map(|r| agents.iter().find(|a| a.id == r.id).unwrap())
        .collect();

    let mut selected = 0usize;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &ordered, &mut selected);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut DefaultTerminal,
    agents: &[&Agent],
    selected: &mut usize,
) -> std::io::Result<()> {
    loop {
        terminal.draw(|f| draw(f, agents, *selected))?;
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('j') | KeyCode::Down => {
                    *selected = (*selected + 1).min(agents.len() - 1)
                }
                KeyCode::Char('k') | KeyCode::Up => *selected = selected.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, agents: &[&Agent], selected: usize) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(0)])
        .split(frame.area());
    render_sidebar(frame, cols[0], agents, selected);
    render_main(frame, cols[1], agents[selected]);
}

fn render_sidebar(frame: &mut Frame, area: Rect, agents: &[&Agent], selected: usize) {
    let block = Block::default().borders(Borders::ALL).title(" agents ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut lines = Vec::new();
    for (i, agent) in agents.iter().enumerate() {
        let selected_row = i == selected;
        let marker = if selected_row { "\u{25b8}" } else { " " };
        let name_style = if selected_row {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{} ", agent.state.glyph()),
                Style::default().fg(color_for(&agent.state)),
            ),
            Span::styled(format!("{:<8.8} ", agent.name), name_style),
            Span::styled(format!("{:<12.12} ", agent.branch), dim),
            Span::styled(format!("{:>3}", age(agent.last_activity)), dim),
        ]));
        if let AgentState::NeedsAttention {
            message: Some(msg), ..
        } = &agent.state
        {
            lines.push(Line::from(Span::styled(
                format!("     {msg}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), rows[0]);

    let waiting = agents
        .iter()
        .filter(|a| matches!(a.state, AgentState::NeedsAttention { .. }))
        .count();
    let footer = vec![
        Line::from(Span::styled(
            format!(" {waiting} need you"),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            " j/k move \u{b7} n new \u{b7} d del \u{b7} q quit",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(footer), rows[1]);
}

fn render_main(frame: &mut Frame, area: Rect, agent: &Agent) {
    let title = format!(
        " {} \u{b7} {} {} ",
        agent.branch,
        agent.state.glyph(),
        label_for(&agent.state)
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color_for(&agent.state)))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = agent.body.iter().map(|l| Line::from(*l)).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "> _",
        Style::default().fg(Color::Green),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

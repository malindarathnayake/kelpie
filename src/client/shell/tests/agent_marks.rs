use super::*;
use crate::config::StatusIndicatorStyle;

fn agent(
    pane_id: &str,
    workspace_id: &str,
    label: Option<&str>,
    status: AgentStatus,
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: format!("{workspace_id}_tab"),
        name: None,
        display_agent: None,
        agent: label.map(str::to_owned),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

fn frame_text(state: &mut ClientShellState) -> String {
    let frame = state.compose(106, 20).expect("composed frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn agent_marks_follow_style_and_fall_back_to_status_dots() {
    let expected = [
        (StatusIndicatorStyle::Agents, ["✻", "◎", "⬢", "π"]),
        (StatusIndicatorStyle::AgentShapes, ["◉", "◎", "⬢", "π"]),
        (StatusIndicatorStyle::AgentLetters, ["A", "X", "U", "P"]),
    ];
    for (style, marks) in expected {
        for (label, mark) in ["claude", "codex", "cursor", "pi"].into_iter().zip(marks) {
            assert_eq!(
                agent_status_icon(AgentStatus::Working, style, Some(label)),
                mark,
                "{style:?} {label}"
            );
            assert_eq!(
                agent_status_icon(AgentStatus::Idle, style, Some(&label.to_uppercase())),
                mark,
                "labels match without case: {style:?} {label}"
            );
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(mark),
                1,
                "{mark} must be one cell"
            );
        }
        for agent in [Some("aider"), None] {
            assert_eq!(agent_status_icon(AgentStatus::Working, style, agent), "●");
            assert_eq!(agent_status_icon(AgentStatus::Blocked, style, agent), "●");
            assert_eq!(agent_status_icon(AgentStatus::Idle, style, agent), "○");
            assert_eq!(agent_status_icon(AgentStatus::Unknown, style, agent), "·");
        }
        assert_eq!(status_icon(AgentStatus::Idle, style), "○");
    }
    for status in [
        AgentStatus::Working,
        AgentStatus::Blocked,
        AgentStatus::Done,
        AgentStatus::Idle,
        AgentStatus::Unknown,
    ] {
        for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
            assert_eq!(
                agent_status_icon(status, style, Some("claude")),
                status_icon(status, style),
                "dots and symbols ignore the agent"
            );
        }
    }
    assert_eq!(
        status_icon(AgentStatus::Blocked, StatusIndicatorStyle::Symbols),
        "×"
    );
}

#[test]
fn representative_agent_prefers_labelled_agents_with_the_displayed_status() {
    let mut focused_codex = agent("p3", "ws_1", Some("codex"), AgentStatus::Working);
    focused_codex.focused = true;
    let agents = vec![
        agent("p1", "ws_1", None, AgentStatus::Working),
        agent("p2", "ws_1", Some("claude"), AgentStatus::Working),
        focused_codex,
        agent("p4", "ws_1", Some("cursor"), AgentStatus::Idle),
        agent("p5", "ws_2", Some("pi"), AgentStatus::Working),
    ];
    let in_ws1 = |id: &str| id == "ws_1";
    assert_eq!(
        representative_agent(&agents, in_ws1, AgentStatus::Working),
        Some("codex")
    );
    assert_eq!(
        representative_agent(&agents[..2], in_ws1, AgentStatus::Working),
        Some("claude")
    );
    assert_eq!(
        representative_agent(&agents, in_ws1, AgentStatus::Idle),
        Some("cursor")
    );
    assert_eq!(
        representative_agent(&agents, in_ws1, AgentStatus::Blocked),
        None
    );
}

#[test]
fn default_sidebar_marks_claude_workspace_and_keeps_dot_for_unknown_agent() {
    let mut snapshot = snapshot();
    snapshot.workspaces[0].agent_status = AgentStatus::Working;
    snapshot.workspaces[0].label = "claude-space".into();
    let mut other = snapshot.workspaces[0].clone();
    other.workspace_id = "ws_2".into();
    other.label = "other-space".into();
    other.number = 2;
    other.focused = false;
    other.agent_status = AgentStatus::Idle;
    snapshot.workspaces.push(other);
    snapshot.agents = vec![
        agent("pane_1", "ws_1", Some("claude"), AgentStatus::Working),
        agent("pane_9", "ws_2", Some("aider"), AgentStatus::Idle),
    ];

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot.clone()));
    state.set_pane_surface(surface());
    let text = frame_text(&mut state);
    let claude_line = text
        .lines()
        .find(|line| line.contains("claude-space"))
        .unwrap_or_else(|| panic!("frame: {text}"));
    let other_line = text
        .lines()
        .find(|line| line.contains("other-space"))
        .unwrap_or_else(|| panic!("frame: {text}"));
    assert!(claude_line.contains('✻'), "frame: {text}");
    assert!(!claude_line.contains('●'), "frame: {text}");
    assert!(other_line.contains('○'), "frame: {text}");

    let mut config = Config::default();
    config.ui.status_indicators = StatusIndicatorStyle::Dots;
    let mut dots = ClientShellState::new(ClientShellConfig::from_config(&config));
    dots.set_snapshot(Box::new(snapshot));
    dots.set_pane_surface(surface());
    let text = frame_text(&mut dots);
    let claude_line = text
        .lines()
        .find(|line| line.contains("claude-space"))
        .unwrap_or_else(|| panic!("frame: {text}"));
    assert!(claude_line.contains('●'), "frame: {text}");
    assert!(!claude_line.contains('✻'), "frame: {text}");
}

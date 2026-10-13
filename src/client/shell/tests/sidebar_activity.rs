//! Fork (Q1): idle age, stale, attention and close-stale tests. Every clock is injected.

use super::*;
use crate::api::schema::{AgentStatus, Method};
use crate::client::shell::close_stale::{close_stale_rows, workspace_close_audit_line};
use crate::client::shell::sidebar_activity::{
    attention_style, workspace_badge, workspace_is_stale, AttentionTracker, SidebarActivityContext,
    PULSE_WINDOW,
};
use std::time::{Duration, Instant};

const NOW_MS: u64 = 1_800_000_000_000;
const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const STALE_AFTER: u64 = 2 * HOUR;

fn ws(
    number: usize,
    label: &str,
    status: AgentStatus,
    idle_ms: Option<u64>,
) -> ClientShellWorkspace {
    let mut workspace = snapshot().workspaces[0].clone();
    workspace.workspace_id = format!("ws_{number}");
    workspace.active_tab_id = format!("tab_{number}");
    workspace.number = number;
    workspace.label = label.into();
    workspace.branch = None;
    workspace.focused = number == 1;
    workspace.agent_status = status;
    workspace.last_activity_ms = idle_ms.map(|idle| NOW_MS - idle);
    workspace
}

fn agent(workspace: usize, background_jobs: Option<&str>) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: format!("pane_{workspace}"),
        workspace_id: format!("ws_{workspace}"),
        tab_id: format!("tab_{workspace}"),
        name: None,
        display_agent: Some("claude".into()),
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: background_jobs
            .map(|value| vec![("background_jobs".to_owned(), value.to_owned())])
            .unwrap_or_default(),
        focused: false,
    }
}

/// Sidebar fixture: working, waiting, fresh idle, stale shell, stale agent with unknown
/// background, stale agent reporting zero jobs, and an agent running a background job.
fn activity_snapshot() -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces = vec![
        ws(1, "billing-api", AgentStatus::Working, Some(12_000)),
        ws(2, "telemetry", AgentStatus::Blocked, Some(6 * MIN)),
        ws(3, "mobile-app", AgentStatus::Idle, Some(15_000)),
        ws(4, "docs-site", AgentStatus::Idle, Some(3 * HOUR + 10 * MIN)),
        ws(5, "sandbox", AgentStatus::Done, Some(2 * HOUR + 30 * MIN)),
        ws(6, "infra-helm", AgentStatus::Done, Some(26 * HOUR)),
        ws(7, "erp-ui", AgentStatus::Done, Some(5 * HOUR)),
    ];
    projected.focused_workspace_id = Some("ws_1".into());
    projected.agents = vec![agent(5, None), agent(6, Some("0")), agent(7, Some("2"))];
    projected
}

fn state_at(clock: Instant, snapshot: ClientShellSnapshot) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.config.animations = crate::config::AnimationsConfig::Full;
    state.sidebar_test_clock = Some((clock, NOW_MS));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state
}

fn ctx(
    attention: Option<&AttentionTracker>,
    now: Instant,
    pulses: bool,
) -> SidebarActivityContext<'_> {
    SidebarActivityContext {
        now,
        now_ms: NOW_MS,
        stale_after_ms: STALE_AFTER,
        attention,
        pulses,
    }
}

fn endpoint_methods(outcome: &ClientShellInput) -> Vec<&Method> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(&request.method),
            _ => None,
        })
        .collect()
}

fn sidebar_lines(state: &mut ClientShellState, cols: u16, rows: u16) -> Vec<String> {
    let frame = state.compose(cols, rows).expect("frame");
    let width = state.hits.sidebar_divider.x as usize;
    frame_rows(&frame)
        .into_iter()
        .map(|row| {
            row.chars()
                .take(width)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn stale_is_computed_from_the_injected_clock_and_never_marks_busy_rows() {
    let projected = activity_snapshot();
    let stale = |number: usize, now_ms: u64| {
        let workspace = &projected.workspaces[number - 1];
        workspace_is_stale(
            &projected,
            workspace,
            workspace.agent_status,
            now_ms,
            STALE_AFTER,
        )
    };
    assert!(stale(4, NOW_MS), "idle shell past 2h is stale");
    assert!(
        !stale(4, NOW_MS - 2 * HOUR),
        "same row two hours earlier is not"
    );
    assert!(stale(5, NOW_MS) && stale(6, NOW_MS));
    assert!(!stale(3, NOW_MS), "fresh idle row");
    assert!(!stale(7, NOW_MS), "background job running is never stale");

    let mut working = projected.clone();
    working.workspaces[0].last_activity_ms = Some(NOW_MS - 30 * HOUR);
    let row = &working.workspaces[0];
    assert!(
        !workspace_is_stale(&working, row, AgentStatus::Working, NOW_MS, STALE_AFTER),
        "a working workspace is never stale, however old its stamp"
    );
    let mut waiting = projected.clone();
    waiting.workspaces[2].open_questions = 1;
    waiting.workspaces[2].last_activity_ms = Some(NOW_MS - 30 * HOUR);
    let row = &waiting.workspaces[2];
    assert!(!workspace_is_stale(
        &waiting,
        row,
        AgentStatus::Idle,
        NOW_MS,
        STALE_AFTER
    ));
    let mut unknown = projected;
    unknown.workspaces[3].last_activity_ms = None;
    let row = &unknown.workspaces[3];
    assert!(
        !workspace_is_stale(&unknown, row, AgentStatus::Idle, NOW_MS, STALE_AFTER),
        "no stamp (older server) is never stale"
    );
}

#[test]
fn badges_carry_text_cues_not_only_colour() {
    let projected = activity_snapshot();
    let palette = Palette::terminal();
    let now = Instant::now();
    let badge = |number: usize| {
        let workspace = &projected.workspaces[number - 1];
        workspace_badge(
            &projected,
            workspace,
            workspace.agent_status,
            &ctx(None, now, true),
            &palette,
        )
    };
    assert_eq!(badge(1), None, "working rows carry no idle badge");
    let waiting = badge(2).expect("waiting badge");
    assert_eq!((waiting.text.as_str(), waiting.short), ("!! waiting", "!!"));
    let idle = badge(3).expect("idle badge");
    assert_eq!(idle.text, "idle 15s");
    assert!(!idle.stale);
    let stale = badge(4).expect("stale badge");
    assert_eq!(stale.text, "stale 3h10m");
    assert!(stale.stale);
    assert_eq!(stale.style.fg, Some(palette.peach));
    assert_eq!(
        palette.peach, palette.yellow,
        "terminal theme: peach equals Working yellow, so the word \"stale\" is required"
    );
    assert_eq!(badge(6).expect("day-old").text, "stale 1d");
}

#[test]
fn waiting_row_pulses_for_five_seconds_then_stays_steady() {
    let start = Instant::now();
    let mut tracker = AttentionTracker::default();
    tracker.sync(&activity_snapshot(), start);
    let at = |ms: u64| start + Duration::from_millis(ms);
    assert_eq!(tracker.pulse_phase("ws_2", at(0), true), Some(true));
    assert_eq!(tracker.pulse_phase("ws_2", at(600), true), Some(false));
    assert_eq!(tracker.pulse_phase("ws_2", at(1_100), true), Some(true));
    assert!(tracker.pulse_phase("ws_2", at(4_999), true).is_some());
    assert_eq!(
        tracker.pulse_phase("ws_2", at(5_000), true),
        None,
        "steady after 5 s"
    );
    assert_eq!(
        tracker.pulse_phase("ws_3", at(0), true),
        None,
        "idle row never pulses"
    );
    assert_eq!(tracker.next_deadline(at(0), true), Some(at(500)));
    assert_eq!(tracker.next_deadline(at(4_700), true), Some(at(5_000)));
    assert_eq!(tracker.next_deadline(at(5_000), true), None);

    // The steady marker after the window is red, bold and still says "!!".
    let projected = activity_snapshot();
    let palette = Palette::catppuccin();
    let badge = workspace_badge(
        &projected,
        &projected.workspaces[1],
        AgentStatus::Blocked,
        &ctx(Some(&tracker), at(6_000), true),
        &palette,
    )
    .expect("badge");
    assert_eq!(badge.text, "!! waiting");
    assert_eq!(badge.style, attention_style(&palette, None));
    assert_eq!(badge.style.fg, Some(palette.red));

    // A new question re-arms an already-waiting row; leaving waiting forgets it.
    let mut asked = projected.clone();
    asked.workspaces[1].open_questions = 1;
    tracker.sync(&asked, at(9_000));
    assert_eq!(tracker.pulse_phase("ws_2", at(9_000), true), Some(true));
    let mut resolved = asked;
    resolved.workspaces[1].agent_status = AgentStatus::Idle;
    resolved.workspaces[1].open_questions = 0;
    tracker.sync(&resolved, at(9_100));
    assert_eq!(tracker.pulse_phase("ws_2", at(9_100), true), None);
    assert_eq!(tracker.next_deadline(at(9_100), true), None);
}

#[test]
fn animations_none_or_basic_never_pulse() {
    for mode in [
        crate::config::AnimationsConfig::None,
        crate::config::AnimationsConfig::Basic,
    ] {
        let start = Instant::now();
        let mut state = state_at(start, activity_snapshot());
        state.config.animations = mode;
        assert!(!state.attention_pulses_allowed());
        assert_eq!(state.next_attention_deadline(start), None);
        assert_eq!(
            state.timer_delay(start),
            Duration::from_millis(100),
            "no extra wakeup"
        );
        let badge_style = {
            let projected = state.snapshot.as_deref().unwrap();
            workspace_badge(
                projected,
                &projected.workspaces[1],
                AgentStatus::Blocked,
                &ctx(
                    Some(&state.attention),
                    start,
                    state.attention_pulses_allowed(),
                ),
                &state.config.palette,
            )
            .unwrap()
            .style
        };
        assert_eq!(badge_style, attention_style(&state.config.palette, None));
    }
}

#[test]
fn unfocused_window_pauses_the_pulse_and_refocus_rearms_it() {
    let start = Instant::now();
    let mut state = state_at(start, activity_snapshot());
    assert!(state.attention_pulses_allowed());
    assert_eq!(state.timer_delay(start), Duration::from_millis(100));
    assert_eq!(
        state.timer_delay(start + Duration::from_millis(450)),
        Duration::from_millis(50),
        "the next pulse flip is folded into timer_delay"
    );

    let lost = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(lost.repaint, "steady marker repaints immediately");
    assert!(!state.attention_pulses_allowed());
    assert_eq!(state.next_attention_deadline(start), None);

    // Long after the first window, refocus starts a fresh one.
    let later = start + Duration::from_secs(60);
    state.sidebar_test_clock = Some((later, NOW_MS + 60_000));
    let gained = state.handle_raw_events(vec![RawInputEvent::OuterFocusGained]);
    assert!(gained.repaint);
    assert!(state.attention_pulses_allowed());
    assert_eq!(
        state.attention.pulse_phase("ws_2", later, true),
        Some(true),
        "refocus re-arms the pulse"
    );
    assert_eq!(
        state.next_attention_deadline(later),
        Some(later + Duration::from_millis(500))
    );
    assert_eq!(
        state
            .attention
            .pulse_phase("ws_2", later + PULSE_WINDOW, true),
        None
    );
}

#[test]
fn nothing_pulsing_means_no_timer_wakeups_or_repaints() {
    let start = Instant::now();
    let mut quiet = snapshot();
    quiet.workspaces[0].agent_status = AgentStatus::Idle;
    quiet.workspaces[0].last_activity_ms = None;
    let mut state = state_at(start, quiet);
    for offset in [0, 100, 1_000, 60_000] {
        let now = start + Duration::from_millis(offset);
        assert_eq!(state.next_attention_deadline(now), None);
        assert_eq!(state.timer_delay(now), Duration::from_millis(100));
        assert!(!state.tick_sidebar_activity(now), "tick at +{offset} ms");
    }

    // A waiting row asks for exactly the phase flips, then stops after the window.
    let mut state = state_at(start, activity_snapshot());
    state.sidebar_label_deadline = Some(start + Duration::from_secs(3600));
    assert!(!state.tick_sidebar_activity(start));
    let mut flips = 0;
    let mut now = start;
    while now < start + Duration::from_secs(7) {
        now += Duration::from_millis(100);
        if state.tick_sidebar_activity(now) {
            flips += 1;
        }
    }
    assert_eq!(flips, 10, "10 half-periods in the 5 s window, then none");
    assert_eq!(state.next_attention_deadline(now), None);
}

#[test]
fn sixteen_colour_pulse_toggles_reversed_never_blink() {
    let terminal = Palette::terminal();
    let on = attention_style(&terminal, Some(true));
    let off = attention_style(&terminal, Some(false));
    assert!(on.add_modifier.contains(Modifier::REVERSED));
    assert!(!off.add_modifier.contains(Modifier::REVERSED));
    assert_eq!(on.fg, Some(terminal.red));
    let truecolor = Palette::catppuccin();
    let on = attention_style(&truecolor, Some(true));
    assert_eq!(on.bg, Some(truecolor.red));
    assert!(!on.add_modifier.contains(Modifier::REVERSED));
    for style in [
        on,
        attention_style(&terminal, Some(true)),
        attention_style(&truecolor, None),
    ] {
        assert!(!style.add_modifier.contains(Modifier::SLOW_BLINK));
        assert!(!style.add_modifier.contains(Modifier::RAPID_BLINK));
    }
}

#[test]
fn sidebar_renders_idle_stale_and_attention_badges() {
    let start = Instant::now();
    let mut state = state_at(start, activity_snapshot());
    state.config.palette = Palette::terminal();
    let lines = sidebar_lines(&mut state, 110, 30);
    let text = lines.join("\n");
    println!("{text}");
    for needle in [
        "!! waiting",
        "idle 15s",
        "stale 3h10m",
        "stale 2h30m",
        "stale 1d",
        "idle 5h",
    ] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }
    let frame = state.compose(110, 30).unwrap();
    let (x, y) = cell_symbol_position(&frame, state.hits.workspace_body, "!!");
    let cell = &frame.cells[y as usize * frame.width as usize + x as usize];
    assert_ne!(
        cell.modifier & Modifier::REVERSED.bits(),
        0,
        "16-colour pulse phase on is REVERSED"
    );
}

#[test]
fn close_stale_defaults_exclude_risky_rows() {
    let projected = activity_snapshot();
    let rows = close_stale_rows(&projected, NOW_MS, STALE_AFTER);
    let summary = rows
        .iter()
        .map(|row| (row.workspace_id.as_str(), row.ticked, row.risks.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            ("ws_4", true, vec![]),
            ("ws_5", false, vec!["background state unknown"]),
            ("ws_6", true, vec![]),
        ],
        "working, waiting, fresh and background-active rows are not offered"
    );

    let mut pending = projected;
    pending.workspaces[3].pending_receipts = 1;
    let rows = close_stale_rows(&pending, NOW_MS, STALE_AFTER);
    assert!(!rows[0].ticked);
    assert_eq!(rows[0].risks, vec!["supervisor submit pending"]);
}

#[test]
fn close_stale_dialog_closes_only_ticked_rows_through_workspace_close() {
    let start = Instant::now();
    let mut state = state_at(start, activity_snapshot());
    let opened = state.handle_raw_events(vec![
        key(KeyCode::Char('b'), KeyModifiers::CONTROL),
        key(KeyCode::Char('S'), KeyModifiers::SHIFT),
    ]);
    assert!(opened.repaint);
    assert!(
        endpoint_methods(&opened).is_empty(),
        "opening closes nothing"
    );
    let Some(ClientShellOverlay::CloseStale(dialog)) = state.overlay.as_ref() else {
        panic!("close stale dialog");
    };
    assert_eq!(dialog.rows.len(), 3);

    let text = {
        let frame = state.compose(100, 30).unwrap();
        frame_rows(&frame).join("\n")
    };
    println!("{text}");
    assert!(text.contains("Close stale workspaces (3)"), "{text}");
    assert!(text.contains("[x] 4 docs-site"), "{text}");
    assert!(text.contains("[ ] 5 sandbox"), "{text}");
    assert!(text.contains("background state unknown"), "{text}");
    assert!(text.contains("[ ] also terminate (t)"), "{text}");
    assert!(text.contains("2 to close"), "{text}");

    // Tick the risky row without "also terminate": Enter refuses and closes nothing.
    state.handle_input_bytes(b"j");
    state.handle_input_bytes(b" ");
    let refused = state.handle_input_bytes(b"\r");
    assert!(endpoint_methods(&refused).is_empty());
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::CloseStale(dialog)) if dialog.notice.is_some()
    ));
    // Untick the safe row 6, keep risky row 5 with "also terminate", close.
    state.handle_input_bytes(b"t");
    state.handle_input_bytes(b"j");
    state.handle_input_bytes(b" ");
    let closed = state.handle_input_bytes(b"\r");
    let closes = endpoint_methods(&closed)
        .into_iter()
        .map(|method| match method {
            Method::WorkspaceClose(params) => (params.workspace_id.clone(), params.close_group),
            other => panic!("unexpected {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        closes,
        vec![("ws_4".to_owned(), false), ("ws_5".to_owned(), false)]
    );
    assert!(state.overlay.is_none());
    assert_eq!(
        state.last_close_audit.as_deref(),
        Some(
            "workspace.close trigger=close_stale count=2 terminated_risky=1 \
             workspaces=[ws_4:2h-6h,ws_5:2h-6h]"
        )
    );
}

#[test]
fn close_stale_escape_and_empty_dialog_close_nothing() {
    let start = Instant::now();
    let mut quiet = activity_snapshot();
    for workspace in &mut quiet.workspaces {
        workspace.last_activity_ms = Some(NOW_MS - MIN);
    }
    let mut state = state_at(start, quiet);
    let mut outcome = ClientShellInput::default();
    state.open_close_stale_overlay(&mut outcome);
    let text = {
        let frame = state.compose(100, 30).unwrap();
        frame_rows(&frame).join("\n")
    };
    assert!(
        text.contains("No workspace has been idle longer than 2h."),
        "{text}"
    );
    let enter = state.handle_input_bytes(b"\r");
    assert!(endpoint_methods(&enter).is_empty());
    let escape = state.handle_input_bytes(b"\x1b");
    assert!(endpoint_methods(&escape).is_empty());
}

#[test]
fn audit_line_is_bounded_and_carries_no_names() {
    let ids = (0..20).map(|n| format!("ws_{n}")).collect::<Vec<_>>();
    let entries = ids
        .iter()
        .map(|id| (id.as_str(), Some(3 * HOUR)))
        .collect::<Vec<_>>();
    let line = workspace_close_audit_line("close_stale", &entries, 0);
    assert!(line.starts_with("workspace.close trigger=close_stale count=20 "));
    assert!(line.ends_with(",+4]"), "{line}");
    assert_eq!(line.matches(":2h-6h").count(), 16);

    let start = Instant::now();
    let mut state = state_at(start, activity_snapshot());
    state.config.confirm_close = false;
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::CloseWorkspace),
        &mut outcome,
    );
    let audit = state.last_close_audit.clone().expect("single close audit");
    assert_eq!(
        audit,
        "workspace.close trigger=close_workspace count=1 terminated_risky=0 workspaces=[ws_1:lt2h]"
    );
    assert!(!audit.contains("billing-api"));
}

#[test]
fn close_stale_binding_does_not_collide_with_navigate_keys() {
    let config = Config::default();
    assert_eq!(
        config.keys.close_stale_workspaces,
        crate::config::BindingConfig::one("prefix+shift+s")
    );
    let groups = crate::input::keybind_help_groups(&config.keybinds(), &[]);
    assert!(groups
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .any(|(key, label)| label == "close stale workspaces" && key == "prefix+shift+s"));
    assert!(crate::DEFAULT_CONFIG.contains("# close_stale_workspaces = \"prefix+shift+s\""));
    assert!(crate::DEFAULT_CONFIG.contains("# stale_after = \"2h\""));

    // Navigate mode: S opens the dialog; x keeps closing the pane, X the tab (upstream).
    let start = Instant::now();
    let mut state = state_at(start, activity_snapshot());
    state.handle_input_bytes(&[0x02]);
    state.handle_input_bytes(b"w");
    assert_eq!(state.mode, ClientShellMode::Navigate);
    let outcome = state.handle_raw_events(vec![key(KeyCode::Char('S'), KeyModifiers::SHIFT)]);
    assert!(endpoint_methods(&outcome).is_empty());
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::CloseStale(_))
    ));
    let keybinds = config.keybinds();
    let x = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty());
    assert_eq!(
        crate::input::resolve_non_indexed_action(
            &keybinds,
            &x,
            crate::input::KeybindDispatch::Prefix
        ),
        Some(crate::input::KeybindAction::ClosePane)
    );
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> RawInputEvent {
    RawInputEvent::Key(crate::input::TerminalKey::new(code, modifiers))
}

//! Fork: delegation indicators (sidebar badge, agent rows, tab tint, expiry notice).

use super::*;
use crate::api::schema::AgentStatus;
use crate::client::shell::sidebar_activity::{
    delegation_remaining_label, delegation_style, next_delegation_change_ms, workspace_badge,
    workspace_delegation, SidebarActivityContext, DELEGATION_WARN_MS,
};
use crate::protocol::ClientShellDelegation;
use std::time::Instant;

const NOW_MS: u64 = 1_800_000_000_000;
const MIN: u64 = 60_000;

fn timed(remaining_ms: u64) -> Option<ClientShellDelegation> {
    Some(ClientShellDelegation {
        expires_at_ms: Some(NOW_MS + remaining_ms),
    })
}

fn permanent() -> Option<ClientShellDelegation> {
    Some(ClientShellDelegation {
        expires_at_ms: None,
    })
}

fn delegated_snapshot(delegations: &[Option<ClientShellDelegation>]) -> ClientShellSnapshot {
    let mut projected = snapshot();
    let template = projected.panes[0].clone();
    projected.panes = delegations
        .iter()
        .enumerate()
        .map(|(index, delegation)| {
            let mut pane = template.clone();
            pane.pane_id = format!("pane_{}", index + 1);
            pane.delegation = *delegation;
            pane
        })
        .collect();
    projected.focused_pane_id = Some("pane_1".into());
    projected
}

fn badge_text(snapshot: &ClientShellSnapshot, now_ms: u64, pulses: bool) -> Option<String> {
    let endpoint = ClientEndpointId::Local;
    let ctx = SidebarActivityContext {
        now: Instant::now(),
        now_ms,
        stale_after_ms: 2 * 60 * MIN,
        endpoint: &endpoint,
        attention: None,
        pulses,
    };
    let palette = crate::app::state::Palette::catppuccin();
    workspace_badge(
        snapshot,
        &snapshot.workspaces[0],
        snapshot.workspaces[0].agent_status,
        &ctx,
        &palette,
    )
    .map(|badge| badge.text)
}

#[test]
fn badge_shows_remaining_time_or_infinity() {
    for (delegation, expected) in [
        (timed(102 * MIN), "◈ bot 1h42m"),
        (timed(4 * MIN + 30_000), "◈ bot 4m"),
        (timed(45_000), "◈ bot 45s"),
        (permanent(), "◈ bot ∞"),
    ] {
        let snap = delegated_snapshot(&[delegation]);
        assert_eq!(
            badge_text(&snap, NOW_MS, true).as_deref(),
            Some(expected),
            "{delegation:?}"
        );
    }
    // Not delegated, or already expired before the server caught up: the idle badge.
    let snap = delegated_snapshot(&[None]);
    assert_eq!(badge_text(&snap, NOW_MS, true), None);
    let snap = delegated_snapshot(&[Some(ClientShellDelegation {
        expires_at_ms: Some(NOW_MS),
    })]);
    assert_eq!(badge_text(&snap, NOW_MS, true), None);
}

#[test]
fn workspace_row_uses_the_shortest_delegation_and_infinity_only_when_all_permanent() {
    let snap = delegated_snapshot(&[permanent(), timed(30 * MIN), timed(4 * MIN)]);
    let shown = workspace_delegation(&snap, "ws_1", NOW_MS).unwrap();
    assert_eq!(shown.expires_at_ms, Some(NOW_MS + 4 * MIN));
    let snap = delegated_snapshot(&[permanent(), None, permanent()]);
    assert_eq!(
        workspace_delegation(&snap, "ws_1", NOW_MS)
            .unwrap()
            .expires_at_ms,
        None
    );
}

#[test]
fn waiting_keeps_its_badge_and_gains_a_violet_lead() {
    let mut snap = delegated_snapshot(&[timed(30 * MIN)]);
    snap.workspaces[0].agent_status = AgentStatus::Blocked;
    let endpoint = ClientEndpointId::Local;
    let ctx = SidebarActivityContext {
        now: Instant::now(),
        now_ms: NOW_MS,
        stale_after_ms: 2 * 60 * MIN,
        endpoint: &endpoint,
        attention: None,
        pulses: true,
    };
    let palette = crate::app::state::Palette::catppuccin();
    let badge = workspace_badge(
        &snap,
        &snap.workspaces[0],
        AgentStatus::Blocked,
        &ctx,
        &palette,
    )
    .expect("waiting badge");
    assert_eq!(badge.text, "!! waiting");
    assert_eq!(badge.lead.and_then(|style| style.fg), Some(palette.mauve));
}

#[test]
fn badge_pulses_only_in_the_last_five_minutes_and_only_when_allowed() {
    let palette = crate::app::state::Palette::catppuccin();
    let steady = Style::default()
        .fg(palette.mauve)
        .add_modifier(Modifier::BOLD);
    let early = timed(DELEGATION_WARN_MS + MIN).unwrap();
    let late = timed(DELEGATION_WARN_MS - MIN).unwrap();
    // Phase on (even half-second) and off (odd half-second).
    let on = NOW_MS - NOW_MS % 1000;
    let off = on + 500;
    assert_eq!(delegation_style(&palette, early, on, true), steady);
    assert_ne!(
        delegation_style(&palette, late, on, true),
        steady,
        "pulse on"
    );
    assert_eq!(
        delegation_style(&palette, late, off, true),
        steady,
        "pulse off"
    );
    assert_eq!(
        delegation_style(&palette, late, on, false),
        steady,
        "animations off keeps it steady"
    );
    assert_eq!(
        delegation_style(&palette, permanent().unwrap(), on, true),
        steady,
        "permanent never pulses"
    );
}

#[test]
fn repaint_deadline_follows_minutes_seconds_and_pulses() {
    let snap = delegated_snapshot(&[timed(42 * MIN + 10_000)]);
    assert_eq!(next_delegation_change_ms(&snap, NOW_MS, true), Some(10_001));
    let snap = delegated_snapshot(&[timed(30_500)]);
    assert_eq!(next_delegation_change_ms(&snap, NOW_MS, false), Some(501));
    let snap = delegated_snapshot(&[timed(DELEGATION_WARN_MS - 7_000)]);
    let half = 500 - NOW_MS % 500;
    assert_eq!(
        next_delegation_change_ms(&snap, NOW_MS, true),
        Some(half.min(53_001))
    );
    let snap = delegated_snapshot(&[permanent()]);
    assert_eq!(next_delegation_change_ms(&snap, NOW_MS, true), None);
    assert_eq!(
        delegation_remaining_label(permanent().unwrap(), NOW_MS),
        "∞"
    );
}

#[test]
fn sidebar_and_agent_row_render_the_badge() {
    let now = crate::supervisor::now_ms();
    let mut snap = snapshot();
    snap.panes[0].delegation = Some(ClientShellDelegation {
        expires_at_ms: Some(now + 102 * MIN + 30_000),
    });
    snap.agents = vec![ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: Some("claude".into()),
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    }];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.sidebar_test_clock = Some((Instant::now(), now));
    state.set_snapshot(Box::new(snap));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 30).unwrap();
    let text = frame_rows(&frame).join("\n");
    assert!(
        text.matches("◈ bot 1h42m").count() >= 2,
        "workspace row and agent row:\n{text}"
    );
}

#[test]
fn expiry_shows_a_notice_but_a_revoke_does_not() {
    let now = crate::supervisor::now_ms();
    let mut first = snapshot();
    first.panes[0].delegation = Some(ClientShellDelegation {
        expires_at_ms: Some(now.saturating_sub(1_000)),
    });
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.sidebar_test_clock = Some((Instant::now(), now));
    state.set_snapshot(Box::new(first));
    state.copy_feedback = None;
    let mut after = snapshot();
    after.revision += 1;
    state.set_snapshot(Box::new(after));
    let message = state
        .copy_feedback
        .as_ref()
        .map(|feedback| feedback.message.clone())
        .expect("expiry notice");
    assert!(
        message.contains("expired — back to you") && message.contains("pane_1"),
        "{message}"
    );

    // Revoked an hour early: no expiry notice.
    let mut delegated = snapshot();
    delegated.panes[0].delegation = Some(ClientShellDelegation {
        expires_at_ms: Some(now + 60 * MIN),
    });
    state.set_snapshot(Box::new(delegated));
    state.copy_feedback = None;
    let mut revoked = snapshot();
    revoked.revision += 2;
    state.set_snapshot(Box::new(revoked));
    assert!(state.copy_feedback.is_none());
}

#[test]
fn handover_reminder_toasts_once_with_the_live_keys_and_shows_in_the_agent_row() {
    let now = crate::supervisor::now_ms();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.sidebar_test_clock = Some((Instant::now(), now));
    state.set_snapshot(Box::new(snapshot()));
    state.copy_feedback = None;

    let mut waiting = snapshot();
    waiting.revision += 1;
    waiting.panes[0].handover_wanted = true;
    waiting.agents = vec![ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: Some("codex".into()),
        agent: Some("codex".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    }];
    state.set_snapshot(Box::new(waiting.clone()));
    let message = state
        .copy_feedback
        .take()
        .map(|feedback| feedback.message)
        .expect("hand-over toast");
    assert!(
        message.starts_with("press ctrl+b, then shift+a to hand") && message.contains("pane_1"),
        "{message}"
    );
    state.set_pane_surface(surface());
    let frame = state.compose(106, 30).unwrap();
    let text = frame_rows(&frame).join("\n");
    assert!(text.contains("◇ bot waiting"), "agent row:\n{text}");

    // Still waiting in the next snapshot: no repeat toast.
    waiting.revision += 1;
    state.set_snapshot(Box::new(waiting.clone()));
    assert!(state.copy_feedback.is_none());

    // A delegated pane never reminds.
    let mut delegated = waiting;
    delegated.revision += 1;
    delegated.panes[0].handover_wanted = false;
    state.set_snapshot(Box::new(delegated.clone()));
    delegated.revision += 1;
    delegated.panes[0].handover_wanted = true;
    delegated.panes[0].delegation = permanent();
    state.set_snapshot(Box::new(delegated));
    assert!(state.copy_feedback.is_none());
}

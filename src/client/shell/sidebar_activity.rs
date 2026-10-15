//! Fork (Q1): idle age, stale and attention presentation for sidebar workspace rows.
//!
//! Everything here is a pure function of `(snapshot, now)` plus the client-local
//! [`AttentionTracker`], which only records when a row entered "waiting". Timers never
//! change state; they only ask for a repaint (`tick_sidebar_activity`).

use std::time::{Duration, Instant};

use ratatui::style::Color;

use super::*;
use crate::api::schema::AgentStatus;

/// Attention pulse: 1 Hz (500 ms on, 500 ms off) for the first 5 s after entering waiting.
pub(super) const PULSE_WINDOW: Duration = Duration::from_secs(5);
pub(super) const PULSE_HALF_PERIOD: Duration = Duration::from_millis(500);

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

/// Compact idle age: `45s`, `12m`, `3h12m` (`3h` on the hour), `2d`.
pub(crate) fn format_idle(ms: u64) -> String {
    if ms < MINUTE_MS {
        format!("{}s", ms / 1000)
    } else if ms < HOUR_MS {
        format!("{}m", ms / MINUTE_MS)
    } else if ms < DAY_MS {
        let hours = ms / HOUR_MS;
        let minutes = (ms % HOUR_MS) / MINUTE_MS;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    } else {
        format!("{}d", ms / DAY_MS)
    }
}

/// Coarse bucket for the close audit line; never the exact age.
pub(super) fn idle_bucket(ms: Option<u64>) -> &'static str {
    match ms {
        None => "unknown",
        Some(ms) if ms < 2 * HOUR_MS => "lt2h",
        Some(ms) if ms < 6 * HOUR_MS => "2h-6h",
        Some(ms) if ms < DAY_MS => "6h-1d",
        Some(_) => "gt1d",
    }
}

pub(super) fn workspace_idle_ms(workspace: &ClientShellWorkspace, now_ms: u64) -> Option<u64> {
    workspace
        .last_activity_ms
        .map(|last| now_ms.saturating_sub(last))
}

/// Any agent pane of the workspace currently reports `status`. The workspace roll-up can
/// rank an unseen Idle pane above a Working one, so busy checks look at every pane.
fn any_agent_pane(snapshot: &ClientShellSnapshot, workspace_id: &str, status: AgentStatus) -> bool {
    snapshot
        .agents
        .iter()
        .any(|agent| agent.workspace_id == workspace_id && agent.agent_status == status)
}

/// Waiting on the owner: any agent pane is blocked/awaiting input, or a question is open.
pub(super) fn workspace_waiting(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    status: AgentStatus,
) -> bool {
    status == AgentStatus::Blocked
        || workspace.agent_status == AgentStatus::Blocked
        || workspace.open_questions > 0
        || any_agent_pane(snapshot, &workspace.workspace_id, AgentStatus::Blocked)
}

/// Any pane of the workspace is working, whatever the roll-up shows.
pub(super) fn workspace_working(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    status: AgentStatus,
) -> bool {
    status == AgentStatus::Working
        || workspace.agent_status == AgentStatus::Working
        || any_agent_pane(snapshot, &workspace.workspace_id, AgentStatus::Working)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackgroundWork {
    /// No agent pane, or every agent pane reported zero background jobs.
    None,
    Active,
    /// An agent pane never reported a usable `background_jobs` count.
    Unknown,
    /// A pane with no agent (plain shell) makes no background claim at all: kelpie does not
    /// track its child processes, so a quiet dev server looks exactly like an idle prompt.
    NoEvidence,
}

/// Background work from the agents' `background_jobs` metadata token. `None` only when
/// every pane of the workspace is an agent that reported zero jobs; inactivity alone never
/// proves a process is safe to end.
pub(super) fn workspace_background(
    snapshot: &ClientShellSnapshot,
    workspace_id: &str,
) -> BackgroundWork {
    let agents = snapshot
        .agents
        .iter()
        .filter(|agent| agent.workspace_id == workspace_id && agent.agent.is_some())
        .collect::<Vec<_>>();
    let mut result = if agents.is_empty() {
        BackgroundWork::NoEvidence
    } else {
        BackgroundWork::None
    };
    if snapshot.panes.iter().any(|pane| {
        pane.workspace_id == workspace_id
            && !agents.iter().any(|agent| agent.pane_id == pane.pane_id)
    }) {
        result = BackgroundWork::NoEvidence;
    }
    for agent in agents {
        let jobs = agent
            .tokens
            .iter()
            .find(|(key, _)| key == "background_jobs")
            .and_then(|(_, value)| value.trim().parse::<u32>().ok());
        match jobs {
            Some(0) => {}
            Some(_) => return BackgroundWork::Active,
            None if result == BackgroundWork::None => result = BackgroundWork::Unknown,
            None => {}
        }
    }
    result
}

/// Stale: idle longer than `stale_after_ms` and not working, waiting or running
/// background work. Unknown activity is never stale.
pub(super) fn workspace_is_stale(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    status: AgentStatus,
    now_ms: u64,
    stale_after_ms: u64,
) -> bool {
    let Some(idle) = workspace_idle_ms(workspace, now_ms) else {
        return false;
    };
    idle > stale_after_ms
        && !workspace_working(snapshot, workspace, status)
        && !workspace_waiting(snapshot, workspace, status)
        && workspace_background(snapshot, &workspace.workspace_id) != BackgroundWork::Active
}

/// Running work that makes closing a workspace terminate something the owner may want.
pub(super) fn close_risks(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
) -> Vec<&'static str> {
    let mut risks = Vec::new();
    if workspace_working(snapshot, workspace, workspace.agent_status) {
        risks.push("agent working");
    }
    if workspace_waiting(snapshot, workspace, workspace.agent_status) {
        risks.push("waiting on you");
    }
    match workspace_background(snapshot, &workspace.workspace_id) {
        BackgroundWork::Active => risks.push("background job running"),
        BackgroundWork::Unknown => risks.push("background state unknown"),
        BackgroundWork::NoEvidence => risks.push("no background evidence"),
        BackgroundWork::None => {}
    }
    if workspace.pending_receipts > 0 {
        risks.push("supervisor submit pending");
    }
    risks
}

/// Milliseconds until any shown idle label (or stale flag) changes; None without stamps.
pub(super) fn next_label_change_ms(
    snapshot: &ClientShellSnapshot,
    now_ms: u64,
    stale_after_ms: u64,
) -> Option<u64> {
    snapshot
        .workspaces
        .iter()
        .filter_map(|workspace| {
            let idle = workspace_idle_ms(workspace, now_ms)?;
            let step = if idle < MINUTE_MS {
                1000
            } else if idle < DAY_MS {
                MINUTE_MS
            } else {
                DAY_MS
            };
            let label = step - idle % step;
            let stale = (idle <= stale_after_ms)
                .then(|| stale_after_ms.saturating_sub(idle).saturating_add(1));
            Some(stale.map_or(label, |stale| stale.min(label)))
        })
        .min()
}

#[derive(Debug, Clone, Copy)]
struct AttentionEntry {
    armed_at: Instant,
    questions: u32,
}

/// When each waiting workspace entered waiting or was re-armed, keyed by endpoint and
/// workspace id (ids are only unique within one endpoint).
#[derive(Debug, Default)]
pub(crate) struct AttentionTracker {
    entries: HashMap<(ClientEndpointId, String), AttentionEntry>,
}

impl AttentionTracker {
    /// Arms `endpoint`'s rows that started waiting or gained a question; forgets its rows no
    /// longer waiting. Other endpoints' entries are left alone.
    pub(super) fn sync(
        &mut self,
        endpoint: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
        now: Instant,
    ) {
        self.entries.retain(|(owner, id), _| {
            owner != endpoint
                || snapshot.workspaces.iter().any(|workspace| {
                    workspace.workspace_id == *id
                        && workspace_waiting(snapshot, workspace, workspace.agent_status)
                })
        });
        for workspace in snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace_waiting(snapshot, workspace, workspace.agent_status))
        {
            let entry = self
                .entries
                .entry((endpoint.clone(), workspace.workspace_id.clone()))
                .or_insert(AttentionEntry {
                    armed_at: now,
                    questions: workspace.open_questions,
                });
            if workspace.open_questions > entry.questions {
                entry.armed_at = now;
            }
            entry.questions = workspace.open_questions;
        }
    }

    /// Host window regained focus: every waiting row pulses again for one window.
    pub(super) fn rearm_all(&mut self, now: Instant) {
        for entry in self.entries.values_mut() {
            entry.armed_at = now;
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    /// `Some(on)` while the row is inside its pulse window and pulses are allowed.
    pub(super) fn pulse_phase(
        &self,
        endpoint: &ClientEndpointId,
        workspace_id: &str,
        now: Instant,
        pulses: bool,
    ) -> Option<bool> {
        if !pulses {
            return None;
        }
        let entry = self
            .entries
            .get(&(endpoint.clone(), workspace_id.to_owned()))?;
        let elapsed = now.checked_duration_since(entry.armed_at)?;
        (elapsed < PULSE_WINDOW)
            .then(|| (elapsed.as_millis() / PULSE_HALF_PERIOD.as_millis()).is_multiple_of(2))
    }

    /// Next pulse phase flip or window end; None when nothing pulses (no wakeups).
    pub(super) fn next_deadline(&self, now: Instant, pulses: bool) -> Option<Instant> {
        if !pulses {
            return None;
        }
        self.entries
            .values()
            .filter_map(|entry| {
                let end = entry.armed_at + PULSE_WINDOW;
                if now >= end {
                    return None;
                }
                let elapsed = now.saturating_duration_since(entry.armed_at);
                let half = PULSE_HALF_PERIOD.as_millis();
                let steps = u32::try_from(elapsed.as_millis() / half + 1).unwrap_or(u32::MAX);
                Some((entry.armed_at + PULSE_HALF_PERIOD * steps).min(end))
            })
            .min()
    }
}

/// Inputs for one sidebar render: the injected clock and the client's attention state.
#[derive(Clone, Copy)]
pub(crate) struct SidebarActivityContext<'a> {
    pub(super) now: Instant,
    pub(super) now_ms: u64,
    pub(super) stale_after_ms: u64,
    /// Endpoint whose rows are being drawn (attention is keyed by it).
    pub(super) endpoint: &'a ClientEndpointId,
    /// None shows the steady marker only.
    pub(super) attention: Option<&'a AttentionTracker>,
    pub(super) pulses: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceBadge {
    pub(super) text: String,
    /// Fallback for narrow rows; empty means "drop the badge".
    pub(super) short: &'static str,
    pub(super) style: Style,
    pub(super) stale: bool,
}

/// 16-colour themes have no distinct dark red, so the pulse toggles REVERSED instead.
fn sixteen_colour(palette: &Palette) -> bool {
    !matches!(palette.red, Color::Rgb(..) | Color::Indexed(..))
}

pub(super) fn attention_style(palette: &Palette, phase_on: Option<bool>) -> Style {
    let steady = Style::default()
        .fg(palette.red)
        .add_modifier(Modifier::BOLD);
    match phase_on {
        Some(true) if sixteen_colour(palette) => steady.add_modifier(Modifier::REVERSED),
        Some(true) => Style::default()
            .fg(palette.text)
            .bg(palette.red)
            .add_modifier(Modifier::BOLD),
        Some(false) | None => steady,
    }
}

/// Right-hand badge of a workspace row: `!! waiting`, `stale 3h12m` or `idle 12m`.
/// Working rows carry none.
pub(super) fn workspace_badge(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    status: AgentStatus,
    ctx: &SidebarActivityContext<'_>,
    palette: &Palette,
) -> Option<WorkspaceBadge> {
    if workspace_waiting(snapshot, workspace, status) {
        let phase = ctx.attention.and_then(|attention| {
            attention.pulse_phase(ctx.endpoint, &workspace.workspace_id, ctx.now, ctx.pulses)
        });
        return Some(WorkspaceBadge {
            text: "!! waiting".into(),
            short: "!!",
            style: attention_style(palette, phase),
            stale: false,
        });
    }
    if workspace_working(snapshot, workspace, status) {
        return None;
    }
    let idle = workspace_idle_ms(workspace, ctx.now_ms)?;
    if workspace_is_stale(snapshot, workspace, status, ctx.now_ms, ctx.stale_after_ms) {
        return Some(WorkspaceBadge {
            text: format!("stale {}", format_idle(idle)),
            short: "stale",
            style: Style::default().fg(palette.peach),
            stale: true,
        });
    }
    Some(WorkspaceBadge {
        text: format!("idle {}", format_idle(idle)),
        short: "",
        style: Style::default().fg(palette.overlay0),
        stale: false,
    })
}

impl ClientShellState {
    /// Pulses run only with `animations = full` and while the host window has focus.
    pub(super) fn attention_pulses_allowed(&self) -> bool {
        self.config.animations.allows_pulse() && self.outer_focused != Some(false)
    }

    /// Wall clock for idle ages and monotonic clock for pulses; tests may inject both.
    pub(super) fn sidebar_clock(&self) -> (Instant, u64) {
        #[cfg(test)]
        if let Some(clock) = self.sidebar_test_clock {
            return clock;
        }
        (Instant::now(), crate::supervisor::now_ms())
    }

    pub(super) fn next_attention_deadline(&self, now: Instant) -> Option<Instant> {
        self.attention
            .next_deadline(now, self.attention_pulses_allowed())
    }

    /// Timer hook: repaint only when a pulse phase flips, a window ends, or an idle label
    /// changes. Returns false (no work) when nothing pulses and no label is due.
    pub(crate) fn tick_sidebar_activity(&mut self, now: Instant) -> bool {
        let mut repaint = false;
        if self.attention_repaint_at.is_some_and(|due| now >= due) {
            repaint = true;
        }
        self.attention_repaint_at = self.next_attention_deadline(now);
        if self.sidebar_label_deadline.is_some_and(|due| now >= due) {
            repaint = true;
            self.sidebar_label_deadline = None;
        }
        if self.sidebar_label_deadline.is_none() {
            if let Some(snapshot) = self.snapshot.as_deref() {
                let (_, now_ms) = self.sidebar_clock();
                self.sidebar_label_deadline =
                    next_label_change_ms(snapshot, now_ms, self.config.stale_after_ms)
                        .map(|ms| now + Duration::from_millis(ms));
            }
        }
        repaint
    }

    /// Snapshot install hook: track waiting rows of the active endpoint.
    pub(super) fn sync_attention(&mut self, now: Instant) {
        match self.snapshot.as_deref() {
            Some(snapshot) => self.attention.sync(&self.active_endpoint_id, snapshot, now),
            None => self.attention.clear(),
        }
        self.attention_repaint_at = self.next_attention_deadline(now);
        self.sidebar_label_deadline = None;
    }

    /// Host focus regained: re-arm the pulse of every waiting row.
    pub(super) fn rearm_attention(&mut self, now: Instant) {
        self.attention.rearm_all(now);
        self.attention_repaint_at = self.next_attention_deadline(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_formatting_is_compact() {
        assert_eq!(format_idle(0), "0s");
        assert_eq!(format_idle(45_000), "45s");
        assert_eq!(format_idle(59_999), "59s");
        assert_eq!(format_idle(60_000), "1m");
        assert_eq!(format_idle(12 * MINUTE_MS + 30_000), "12m");
        assert_eq!(format_idle(3 * HOUR_MS + 12 * MINUTE_MS), "3h12m");
        assert_eq!(format_idle(3 * HOUR_MS), "3h");
        assert_eq!(format_idle(2 * DAY_MS + 5 * HOUR_MS), "2d");
        assert_eq!(idle_bucket(None), "unknown");
        assert_eq!(idle_bucket(Some(HOUR_MS)), "lt2h");
        assert_eq!(idle_bucket(Some(3 * HOUR_MS)), "2h-6h");
        assert_eq!(idle_bucket(Some(7 * HOUR_MS)), "6h-1d");
        assert_eq!(idle_bucket(Some(2 * DAY_MS)), "gt1d");
    }

    #[test]
    fn next_label_change_tracks_seconds_minutes_and_the_stale_edge() {
        let mut snapshot = super::super::tests::snapshot();
        let now_ms = 10 * DAY_MS;
        snapshot.workspaces[0].last_activity_ms = None;
        assert_eq!(next_label_change_ms(&snapshot, now_ms, 2 * HOUR_MS), None);
        snapshot.workspaces[0].last_activity_ms = Some(now_ms - 45_300);
        assert_eq!(
            next_label_change_ms(&snapshot, now_ms, 2 * HOUR_MS),
            Some(700)
        );
        snapshot.workspaces[0].last_activity_ms = Some(now_ms - (12 * MINUTE_MS + 10_000));
        assert_eq!(
            next_label_change_ms(&snapshot, now_ms, 2 * HOUR_MS),
            Some(50_000)
        );
        snapshot.workspaces[0].last_activity_ms = Some(now_ms - HOUR_MS);
        assert_eq!(
            next_label_change_ms(&snapshot, now_ms, HOUR_MS + 500),
            Some(501),
            "the stale edge lands one ms after the threshold"
        );
    }
}

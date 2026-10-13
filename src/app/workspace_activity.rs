//! Fork (Q1): per-workspace activity facts for the client-shell snapshot.
//!
//! `last_activity_ms` reuses the supervisor `PaneActivity` stamps (output batch, owner
//! input) plus `TerminalState::last_state_change_ms`; nothing here stamps anything. The
//! published value is rounded down to `ACTIVITY_QUANTUM_MS` so continuous output changes
//! the snapshot at most once per quantum, and all panes cross a quantum boundary together.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::layout::PaneId;

/// Granularity of the published `last_activity_ms`.
pub(crate) const ACTIVITY_QUANTUM_MS: u64 = 5_000;
/// How often the server checks whether hidden-pane activity moved a published value.
pub(crate) const ACTIVITY_PUBLISH_INTERVAL: Duration = Duration::from_millis(ACTIVITY_QUANTUM_MS);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct WorkspaceActivity {
    pub(crate) last_activity_ms: Option<u64>,
    pub(crate) open_questions: u32,
    pub(crate) pending_receipts: u32,
}

/// Rounds a Unix-ms stamp down to the publish quantum.
pub(crate) fn quantize_activity_ms(ms: u64) -> u64 {
    ms - ms % ACTIVITY_QUANTUM_MS
}

/// Supervisor facts read once per snapshot build; empty when the surface is disabled.
#[derive(Default)]
pub(crate) struct SupervisorActivityFacts {
    pending_receipts: HashMap<PaneId, u32>,
}

/// Tracks the last activity fingerprint pushed to shell clients, so hidden-pane output (which
/// never triggers presentation work) still refreshes the idle age within one interval.
#[derive(Debug, Default)]
pub(crate) struct ActivityPublisher {
    next_check: Option<Instant>,
    published: Option<u64>,
}

impl ActivityPublisher {
    /// True when the fingerprint moved since the last check. The first check only records it.
    pub(crate) fn poll(&mut self, now: Instant, fingerprint: impl FnOnce() -> u64) -> bool {
        if self.next_check.is_some_and(|deadline| now < deadline) {
            return false;
        }
        self.next_check = Some(now + ACTIVITY_PUBLISH_INTERVAL);
        let current = fingerprint();
        let changed = self.published.is_some_and(|previous| previous != current);
        self.published = Some(current);
        changed
    }
}

impl super::App {
    pub(crate) fn supervisor_activity_facts(&self) -> SupervisorActivityFacts {
        if !crate::supervisor::enabled() {
            return SupervisorActivityFacts::default();
        }
        SupervisorActivityFacts {
            pending_receipts: self.state.supervisor.submits.accepted_by_pane(),
        }
    }

    /// Activity facts of one workspace: atomic loads and map lookups only, no allocation.
    pub(crate) fn workspace_activity(
        &self,
        ws_idx: usize,
        facts: &SupervisorActivityFacts,
    ) -> WorkspaceActivity {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return WorkspaceActivity::default();
        };
        let supervisor_on = crate::supervisor::enabled();
        let mut activity = WorkspaceActivity::default();
        for tab in &workspace.tabs {
            for (pane_id, pane) in &tab.panes {
                let (output, owner_input) = self
                    .state
                    .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, *pane_id)
                    .map(|runtime| {
                        let stamps = runtime.supervisor_activity();
                        (stamps.last_output_ms(), stamps.last_owner_input_ms())
                    })
                    .unwrap_or((None, None));
                let transition = self
                    .state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .and_then(|terminal| terminal.last_state_change_ms);
                let pane_last =
                    crate::supervisor::activity::last_activity_ms(output, owner_input, transition);
                activity.last_activity_ms = activity.last_activity_ms.max(pane_last);
                if supervisor_on {
                    if let Some(state) = self.state.supervisor.pane(*pane_id) {
                        let open = state
                            .questions
                            .iter()
                            .filter(|question| question.resolution.is_none())
                            .count();
                        activity.open_questions = activity
                            .open_questions
                            .saturating_add(u32::try_from(open).unwrap_or(u32::MAX));
                    }
                    activity.pending_receipts = activity
                        .pending_receipts
                        .saturating_add(facts.pending_receipts.get(pane_id).copied().unwrap_or(0));
                }
            }
        }
        activity.last_activity_ms = activity.last_activity_ms.map(quantize_activity_ms);
        activity
    }

    /// Order-sensitive digest of every workspace's published activity facts.
    pub(crate) fn workspace_activity_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let facts = self.supervisor_activity_facts();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for ws_idx in 0..self.state.workspaces.len() {
            self.workspace_activity(ws_idx, &facts).hash(&mut hasher);
        }
        hasher.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_rounds_down_to_the_quantum() {
        assert_eq!(quantize_activity_ms(0), 0);
        assert_eq!(quantize_activity_ms(4_999), 0);
        assert_eq!(quantize_activity_ms(5_000), 5_000);
        assert_eq!(quantize_activity_ms(12_345), 10_000);
    }

    #[test]
    fn publisher_reports_only_changes_once_per_interval() {
        let start = Instant::now();
        let mut publisher = ActivityPublisher::default();
        assert!(!publisher.poll(start, || 1), "first check only records");
        assert!(
            !publisher.poll(start + Duration::from_millis(10), || 2),
            "not due yet"
        );
        let due = start + ACTIVITY_PUBLISH_INTERVAL;
        assert!(publisher.poll(due, || 2), "moved fingerprint is reported");
        assert!(
            !publisher.poll(due + ACTIVITY_PUBLISH_INTERVAL, || 2),
            "unchanged fingerprint is not"
        );
    }

    #[tokio::test]
    async fn workspace_activity_is_the_newest_quantized_pane_stamp() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("activity");
        let first = workspace.tabs[0].root_pane;
        let second = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let (runtime_a, _input_a) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 4,
            );
        let (runtime_b, _input_b) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 4,
            );
        runtime_a
            .supervisor_activity()
            .set_stamps_for_test(12_345, 0);
        runtime_b
            .supervisor_activity()
            .set_stamps_for_test(0, 31_000);
        workspace.insert_test_runtime(first, runtime_a);
        workspace.insert_test_runtime(second, runtime_b);
        app.state.workspaces = vec![workspace];

        let facts = app.supervisor_activity_facts();
        let activity = app.workspace_activity(0, &facts);
        assert_eq!(activity.last_activity_ms, Some(30_000));
        assert_eq!(activity.open_questions, 0);
        assert_eq!(activity.pending_receipts, 0);
        assert_eq!(
            app.workspace_activity(5, &facts),
            WorkspaceActivity::default()
        );
        for workspace in &mut app.state.workspaces {
            for (_, runtime) in workspace.test_runtimes.drain() {
                runtime.shutdown();
            }
        }
    }
}

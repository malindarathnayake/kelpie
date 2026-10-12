//! Second stage of supervisor question detection: App reads the grid for panes whose
//! state transition (recorded in AppState) left a pending scan.

use super::App;

impl App {
    /// Performs pending supervisor scans right after an event is handled.
    ///
    /// Perf (tier hot): at most one 24-row text snapshot per transition to Idle/Blocked
    /// (plus the one-shot quiescence rescan); never on the PTY byte path. The scan itself
    /// is O(24 rows) and takes no locks beyond the runtime's own snapshot.
    pub(crate) fn run_supervisor_scans(&mut self) {
        let pending = self.state.supervisor.panes_pending_scan();
        for pane in pending {
            let text = self.find_pane(pane).and_then(|(ws_idx, _)| {
                self.state
                    .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane)
                    .map(|runtime| runtime.recent_text_snapshot(24).text)
            });
            match text {
                Some(text) => {
                    self.state
                        .supervisor
                        .apply_scan(pane, &text, crate::supervisor::now_ms());
                }
                None => self.state.supervisor.clear_pending_scan(pane),
            }
        }
    }

    /// Whether the pane's agent manifest currently matches a background-work rule.
    ///
    /// Perf (tier standard): one manifest evaluation, called on demand from the digest
    /// only; never on the PTY byte path and never cached.
    #[allow(dead_code)] // Consumed by the digest in a later unit.
    pub(crate) fn background_rule_hit(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> bool {
        let Some((pane, _workspace_id)) = self.lookup_runtime(ws_idx, pane_id) else {
            return false;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
        else {
            return false;
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return false;
        };
        if terminal.full_lifecycle_hook_authority_active() {
            return false;
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return false;
        };
        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        explain.matched_rule.is_some_and(|rule| {
            crate::supervisor::background::is_background_rule(
                crate::detect::agent_label(agent),
                &rule.id,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::supervisor::{Confidence, QuestionKind};
    use crate::app::App;
    use crate::detect::AgentState;
    use crate::events::AppEvent;

    /// Resets the supervisor enable override even when a test panics.
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            crate::supervisor::set_enabled_for_test(None);
        }
    }

    fn hook_state(pane_id: crate::layout::PaneId, state: AgentState, seq: u64) -> AppEvent {
        AppEvent::HookStateReported {
            pane_id,
            source: "custom:worker".into(),
            agent_label: "worker".into(),
            state,
            message: None,
            seq: Some(seq),
            session_ref: None,
        }
    }

    fn app_with_screen(name: &str, bytes: &[u8]) -> (App, crate::layout::PaneId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new(name)];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, bytes);
        app.terminal_runtimes.insert(terminal_id, runtime);
        (app, pane_id)
    }

    #[tokio::test]
    async fn supervisor_transition_app_idle_records_prose_question() {
        crate::supervisor::set_enabled_for_test(Some(true));
        let _reset = ResetOverride;
        let (mut app, pane_id) = app_with_screen(
            "supervisor-scan",
            b"Plan ready.\r\n  Should I start the brief?\r\n",
        );
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));

        let state = app.state.supervisor.pane(pane_id).expect("pane tracked");
        assert!(!state.pending_prose);
        assert_eq!(state.pending_blocked, None);
        let open: Vec<_> = state
            .questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, QuestionKind::ProseQuestion);
        assert_eq!(open[0].confidence, Confidence::Low);
        assert_eq!(open[0].excerpt, "Should I start the brief?");
    }

    /// PaneDied on the RespawnShell path: the exit Idle transition queues a prose scan,
    /// and that scan must read the dying agent's screen before the replacement shell
    /// takes over the pane's runtime.
    #[tokio::test]
    async fn supervisor_transition_pane_died_scans_before_return() {
        crate::supervisor::set_enabled_for_test(Some(true));
        let _reset = ResetOverride;
        let (mut app, pane_id) = app_with_screen(
            "supervisor-pane-died",
            b"Plan ready.\r\n  Should I start the brief?\r\n",
        );
        app.state.default_shell = crate::app::api::test_support::exiting_test_command().into();
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        {
            let terminal = app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("test terminal should exist");
            terminal.respawn_shell_on_exit = true;
            terminal.detected_agent = Some(crate::detect::Agent::Claude);
        }
        // Detection path, not a hook: a live hook authority would keep the effective
        // state at Working through the process exit and no Idle transition would be
        // recorded.
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Claude),
            state: AgentState::Working,
            visible_blocker: false,
            visible_working: true,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        assert_eq!(
            app.state
                .supervisor
                .pane(pane_id)
                .and_then(|state| state.last_state),
            Some(AgentState::Working),
            "Working transition must be recorded before the pane dies"
        );

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });

        assert!(
            app.find_pane(pane_id).is_some(),
            "respawnable pane should survive PaneDied"
        );
        let state = app.state.supervisor.pane(pane_id).expect("pane tracked");
        let open: Vec<_> = state
            .questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .collect();
        assert_eq!(
            open.len(),
            1,
            "exactly one open question expected; registry state: {state:?}"
        );
        assert_eq!(open[0].kind, QuestionKind::ProseQuestion);
        assert_eq!(open[0].excerpt, "Should I start the brief?");
        assert!(
            !app.state.supervisor.panes_pending_scan().contains(&pane_id),
            "stale pending scan left after PaneDied"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    fn report_background_jobs(app: &mut App, pane_id: crate::layout::PaneId) {
        report_background_token(app, pane_id, Some("2".to_string()));
    }

    fn report_background_token(
        app: &mut App,
        pane_id: crate::layout::PaneId,
        value: Option<String>,
    ) {
        let target = app.public_pane_id(0, pane_id).unwrap();
        let mut tokens = std::collections::HashMap::new();
        tokens.insert("background_jobs".to_string(), value);
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "bg".into(),
            method: crate::api::schema::Method::PaneReportMetadata(
                crate::api::schema::PaneReportMetadataParams {
                    pane_id: target,
                    source: "user:metadata.test-1".into(),
                    agent: None,
                    applies_to_source: None,
                    title: None,
                    display_agent: None,
                    state_labels: std::collections::HashMap::new(),
                    tokens,
                    clear_title: false,
                    clear_display_agent: false,
                    clear_state_labels: false,
                    seq: None,
                    ttl_ms: None,
                },
            ),
        });
        assert!(response.contains("\"ok\""), "{response}");
    }

    #[tokio::test]
    async fn supervisor_background_hook_report_notes_generation_when_enabled() {
        crate::supervisor::set_enabled_for_test(Some(true));
        let _reset = ResetOverride;
        let (mut app, pane_id) = app_with_screen("supervisor-bg-hook", b"");
        report_background_jobs(&mut app, pane_id);
        assert!(app.state.supervisor.background_hook_current(pane_id));
    }

    #[tokio::test]
    async fn supervisor_background_clear_token_does_not_note_hook() {
        crate::supervisor::set_enabled_for_test(Some(true));
        let _reset = ResetOverride;
        let (mut app, pane_id) = app_with_screen("supervisor-bg-clear", b"");
        report_background_token(&mut app, pane_id, None);
        assert!(!app.state.supervisor.background_hook_current(pane_id));
        report_background_token(&mut app, pane_id, Some("1".to_string()));
        assert!(app.state.supervisor.background_hook_current(pane_id));
    }

    #[tokio::test]
    async fn supervisor_background_hook_report_ignored_when_disabled() {
        crate::supervisor::set_enabled_for_test(Some(false));
        let _reset = ResetOverride;
        let (mut app, pane_id) = app_with_screen("supervisor-bg-hook-off", b"");
        report_background_jobs(&mut app, pane_id);
        assert!(app.state.supervisor.pane(pane_id).is_none());
    }

    // CLI compatibility of the background rule-id table is UNVERIFIED here; it is
    // validated by live smoke tests, not unit tests.
    #[tokio::test]
    async fn supervisor_background_rule_hit_false_without_detected_agent() {
        let (app, pane_id) = app_with_screen("supervisor-bg-rule", b"anything\r\n");
        assert!(!app.background_rule_hit(0, pane_id));
    }

    #[tokio::test]
    async fn supervisor_transition_app_disabled_records_nothing() {
        crate::supervisor::set_enabled_for_test(None);
        let _reset = ResetOverride;
        let (mut app, pane_id) =
            app_with_screen("supervisor-scan-off", b"  Should I start the brief?\r\n");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));
        assert!(app.state.supervisor.pane(pane_id).is_none());
    }
}

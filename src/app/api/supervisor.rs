//! Fork-only `agents.digest` handler (supervisor facts, read-only). Disabled unless
//! `[supervisor] enabled`.

use crate::api::schema::supervisor::{AgentsDigest, AgentsDigestParams, DigestTruncation};
use crate::api::schema::{AgentInfo, ResponseResult};
use crate::app::App;
use crate::layout::PaneId;
use crate::supervisor::digest::{
    build_agent_facts, build_page, membership_fingerprint, parse_cursor, AgentFactInputs,
    CursorError, MAX_PAGE_BYTES,
};

use super::responses::{encode_error, encode_success};

/// Max logged chars of the client-supplied trace id.
const CLIENT_TRACE_ID_LOG_CHARS: usize = 64;
/// Max logged chars of the client-supplied request id.
const REQUEST_ID_LOG_CHARS: usize = 128;

/// Client-supplied value reduced to `[A-Za-z0-9-_.:]`, at most `max_chars` chars
/// (CWE-117). Logged only; never echoed in a response or used as our trace id.
pub(super) fn sanitize_log_token(raw: Option<&str>, max_chars: usize) -> String {
    raw.unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .take(max_chars)
        .collect()
}

/// Per-call context for the single `supervisor.request` event.
pub(super) struct RequestLog {
    /// Bounded: one of the supervisor method names.
    pub(super) method: &'static str,
    pub(super) started: std::time::Instant,
    pub(super) trace_id: String,
    pub(super) client_trace_id: String,
    pub(super) request_id: String,
}

impl RequestLog {
    /// Log context for a request without a client trace id.
    pub(super) fn new(method: &'static str, id: &str) -> Self {
        Self {
            method,
            started: std::time::Instant::now(),
            trace_id: crate::supervisor::next_trace_id(),
            client_trace_id: String::new(),
            request_id: sanitize_log_token(Some(id), REQUEST_ID_LOG_CHARS),
        }
    }

    pub(super) fn ok(&self, agents: usize, response: &str) {
        tracing::debug!(
            event = "supervisor.request",
            subsystem = "supervisor",
            method = self.method,
            outcome = "ok",
            trace_id = %self.trace_id,
            client_trace_id = %self.client_trace_id,
            request_id = %self.request_id,
            agents = u32::try_from(agents).unwrap_or(u32::MAX),
            bytes = response.len() as u64,
            duration_us = self.duration_us()
        );
    }

    pub(super) fn error(&self, error_code: &str, response: &str) {
        tracing::warn!(
            event = "supervisor.request",
            subsystem = "supervisor",
            method = self.method,
            outcome = "error",
            error_code = error_code,
            trace_id = %self.trace_id,
            client_trace_id = %self.client_trace_id,
            request_id = %self.request_id,
            agents = 0u32,
            bytes = response.len() as u64,
            duration_us = self.duration_us()
        );
    }

    fn duration_us(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

impl App {
    pub(super) fn handle_agents_digest(
        &mut self,
        id: String,
        params: AgentsDigestParams,
    ) -> String {
        let log = RequestLog {
            method: "agents.digest",
            started: std::time::Instant::now(),
            trace_id: crate::supervisor::next_trace_id(),
            client_trace_id: sanitize_log_token(
                params.trace_id.as_deref(),
                CLIENT_TRACE_ID_LOG_CHARS,
            ),
            request_id: sanitize_log_token(Some(&id), REQUEST_ID_LOG_CHARS),
        };
        if !crate::supervisor::enabled() {
            let response = encode_error(
                id,
                "supervisor_disabled",
                "supervisor mode is disabled; set [supervisor] enabled = true and restart the server",
            );
            log.error("supervisor_disabled", &response);
            return response;
        }

        let now = crate::supervisor::now_ms();
        // Snapshot first: every fact below is at least as new as this sequence.
        let as_of_seq = self.event_hub.current_sequence();
        let panes = self.digest_agent_panes(&params);
        let fingerprint =
            membership_fingerprint(panes.iter().map(|(_, _, info)| info.pane_id.as_str()));

        // Validate the cursor before any rescan or fact building. A stale cursor means
        // the membership changed between pages; the client restarts paging.
        let offset = match parse_cursor(params.page_cursor.as_deref(), panes.len(), &fingerprint) {
            Ok(offset) => offset,
            Err(error) => {
                let message = match error {
                    CursorError::Stale => "stale page_cursor",
                    CursorError::Invalid => "invalid page_cursor",
                };
                let response = encode_error(id, "invalid_request", message);
                log.error("invalid_request", &response);
                return response;
            }
        };
        let remaining = panes.get(offset..).unwrap_or_default();

        // D13: one-time quiescence rescan per Idle turn (registry-bounded), then one scan pass.
        for (_, pane_id, _) in remaining {
            self.state
                .supervisor
                .request_quiescence_rescan(*pane_id, now);
        }
        self.run_supervisor_scans();

        let facts: Vec<_> = remaining
            .iter()
            .map(|(ws_idx, pane_id, info)| {
                build_agent_facts(self.digest_agent_inputs(*ws_idx, *pane_id, info, now))
            })
            .collect();
        let page = build_page(facts, offset, MAX_PAGE_BYTES, &fingerprint);
        let mut truncated = DigestTruncation {
            questions: page.truncated_questions,
            resolved_history: false,
        };
        for (_, pane_id, _) in remaining.iter().take(page.agents.len()) {
            if let Some(state) = self.state.supervisor.pane(*pane_id) {
                truncated.questions |= state.truncated_questions;
                truncated.resolved_history |= state.truncated_resolved;
            }
        }

        let agent_count = page.agents.len();
        let digest = AgentsDigest {
            server_epoch: crate::supervisor::server_epoch().to_string(),
            trace_id: log.trace_id.clone(),
            as_of_seq,
            generated_at_ms: now,
            agents: page.agents,
            monitors: vec![],
            next_page: page.next_page,
            truncated,
        };
        let response = encode_success(id, ResponseResult::AgentsDigest(Box::new(digest)));
        log.ok(agent_count, &response);
        response
    }

    /// Agent panes in `collect_agent_infos` order, narrowed by the optional pane filter.
    /// Unknown filter ids are silently omitted.
    fn digest_agent_panes(&self, params: &AgentsDigestParams) -> Vec<(usize, PaneId, AgentInfo)> {
        let wanted = params.filter.as_ref().and_then(|f| f.pane_ids.as_ref());
        let mut panes = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for pane_id in tab.layout.pane_ids() {
                    let Some(info) = self.agent_info(ws_idx, pane_id) else {
                        continue;
                    };
                    if wanted.is_some_and(|ids| !ids.contains(&info.pane_id)) {
                        continue;
                    }
                    panes.push((ws_idx, pane_id, info));
                }
            }
        }
        panes
    }

    /// Gathers registry, runtime and hook/rule evidence for one agent pane. Reads no grid
    /// text beyond the single manifest evaluation in `background_rule_hit`.
    fn digest_agent_inputs(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
        info: &AgentInfo,
        now: u64,
    ) -> AgentFactInputs {
        let target_ref = self.state.supervisor.target_ref(pane_id, &info.pane_id);
        let (questions, lineage, last_transition_ms, exited, unresolved_overflow) =
            match self.state.supervisor.pane(pane_id) {
                Some(state) => (
                    state.questions.clone(),
                    state.lineage.iter().cloned().collect(),
                    state.last_transition_ms,
                    state.exited,
                    state.unresolved_overflow,
                ),
                None => (Vec::new(), Vec::new(), None, false, 0),
            };
        let (runtime_present, last_output_ms, last_owner_input_ms, draft_latched, handoff_armed) =
            match self
                .state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
            {
                Some(runtime) => {
                    let activity = runtime.supervisor_activity();
                    (
                        true,
                        activity.last_output_ms(),
                        activity.last_owner_input_ms(),
                        activity.draft_latched(),
                        activity.valid_handoff(target_ref.generation, now).is_some(),
                    )
                }
                None => (false, None, None, false, false),
            };
        let delegation = self
            .state
            .supervisor
            .delegations
            .active(pane_id, now)
            .map_or_else(Default::default, |delegation| {
                crate::api::schema::supervisor::DelegationFacts {
                    active: true,
                    permanent: delegation.permanent(),
                    expires_at_ms: delegation.expires_at_ms,
                }
            });
        // Read-time TTL check on the pane's attached terminal; never sweeps or mutates.
        let background_validity = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.pane_state(pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .and_then(|terminal| {
                terminal
                    .metadata_tokens
                    .validity("background_jobs", std::time::Instant::now())
            });
        let derived = crate::supervisor::background::derive_background(
            info.tokens.get("background_jobs").map(String::as_str),
            background_validity,
            self.state.supervisor.background_hook_current(pane_id),
            self.state.supervisor.background_hook_ms(pane_id),
            self.background_rule_hit(ws_idx, pane_id),
            now,
        );
        // No reporter sends a work/turn correlation yet (C18b open), so zero reads Unknown.
        let (background, background_jobs) =
            crate::supervisor::background::require_zero_correlation(derived, false);
        AgentFactInputs {
            target_ref,
            status: info.agent_status,
            questions,
            lineage,
            background,
            background_jobs,
            runtime_present,
            exited,
            last_output_ms,
            last_owner_input_ms,
            last_transition_ms,
            agent: info.agent.clone(),
            agent_session: info.agent_session.clone(),
            state_change_seq: info.state_change_seq,
            // AgentInfo reports None before the first completion; the wire field uses 0,
            // matching how `state_change_seq` is defaulted.
            completion_seq: info.completion_seq.unwrap_or(0),
            unresolved_overflow,
            draft: self.pane_draft_state(
                ws_idx,
                pane_id,
                info.agent.as_deref(),
                // A delegated pane reports the box as the writes will see it.
                draft_latched && !delegation.active,
                last_owner_input_ms,
                last_output_ms,
            ),
            handoff_armed,
            delegation,
            now_ms: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::supervisor::{
        Background, BlockingReason, Completion, Confidence, DigestFilter, FactSource, Foreground,
        QuestionKind,
    };
    use crate::api::schema::{Method, Request, SuccessResponse};
    use crate::detect::AgentState;
    use crate::events::AppEvent;

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::supervisor::set_enabled_for_test(None);
        }
    }

    fn empty_params() -> AgentsDigestParams {
        AgentsDigestParams {
            filter: None,
            page_cursor: None,
            trace_id: None,
        }
    }

    fn hook_state(pane_id: PaneId, state: AgentState, seq: u64) -> AppEvent {
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

    fn app_with_screen(name: &str, bytes: &[u8]) -> (App, PaneId) {
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

    /// TTL long enough to stay live for the duration of a test.
    const LIVE_TTL_MS: u64 = 600_000;

    fn background_report(
        app: &App,
        pane_id: PaneId,
        value: Option<String>,
    ) -> crate::api::schema::PaneReportMetadataParams {
        let mut tokens = std::collections::HashMap::new();
        tokens.insert("background_jobs".to_string(), value);
        crate::api::schema::PaneReportMetadataParams {
            pane_id: app.public_pane_id(0, pane_id).unwrap(),
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
        }
    }

    fn send_report(app: &mut App, params: crate::api::schema::PaneReportMetadataParams) {
        let response = app.handle_api_request(Request {
            id: "bg".into(),
            method: Method::PaneReportMetadata(params),
        });
        assert!(response.contains("\"ok\""), "{response}");
    }

    fn report_background_token(
        app: &mut App,
        pane_id: PaneId,
        value: Option<String>,
        ttl_ms: Option<u64>,
    ) {
        let mut params = background_report(app, pane_id, value);
        params.ttl_ms = ttl_ms;
        send_report(app, params);
    }

    fn digest_raw(app: &mut App, params: AgentsDigestParams) -> String {
        app.handle_api_request(Request {
            id: "dg".into(),
            method: Method::AgentsDigest(params),
        })
    }

    fn digest(app: &mut App, params: AgentsDigestParams) -> AgentsDigest {
        let raw = digest_raw(app, params);
        let response: SuccessResponse = serde_json::from_str(&raw).expect(&raw);
        assert_eq!(response.id, "dg");
        match response.result {
            ResponseResult::AgentsDigest(d) => *d,
            other => panic!("unexpected result {other:?}"),
        }
    }

    #[test]
    fn agents_digest_disabled_returns_supervisor_disabled() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(false));

        let raw = app.handle_api_request(Request {
            id: "d1".into(),
            method: Method::AgentsDigest(AgentsDigestParams {
                filter: None,
                page_cursor: None,
                trace_id: None,
            }),
        });
        let response: crate::api::schema::ErrorResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(response.id, "d1");
        assert_eq!(response.error.code, "supervisor_disabled");
    }

    #[test]
    fn supervisor_digest_sanitizes_log_tokens() {
        assert_eq!(sanitize_log_token(None, CLIENT_TRACE_ID_LOG_CHARS), "");
        let hostile = "ab-c_1.2:x\r\n\u{1b}[31m evil\tid";
        for cap in [CLIENT_TRACE_ID_LOG_CHARS, REQUEST_ID_LOG_CHARS] {
            let clean = sanitize_log_token(Some(hostile), cap);
            assert_eq!(clean, "ab-c_1.2:x31mevilid");
            assert!(!clean.chars().any(|c| c.is_control()));
        }
        let long = "z".repeat(300);
        assert_eq!(
            sanitize_log_token(Some(&long), CLIENT_TRACE_ID_LOG_CHARS).len(),
            64
        );
        assert_eq!(
            sanitize_log_token(Some(&long), REQUEST_ID_LOG_CHARS).len(),
            128
        );
    }

    #[tokio::test]
    async fn supervisor_digest_enabled_empty() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, _pane_id) = app_with_screen("digest-empty", b"");
        let seq = app.event_hub.current_sequence();
        let mut params = empty_params();
        params.trace_id = Some("client-trace".into());
        let d = digest(&mut app, params);
        assert!(d.agents.is_empty());
        assert_eq!(d.as_of_seq, seq);
        assert_eq!(d.server_epoch.len(), 32);
        assert!(d
            .server_epoch
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert!(d.trace_id.starts_with(&d.server_epoch[..8]));
        assert_ne!(d.trace_id, "client-trace");
        assert!(d.monitors.is_empty());
        assert_eq!(d.next_page, None);
    }

    #[tokio::test]
    async fn supervisor_digest_prose_question_awaiting_human() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen(
            "digest-prose",
            b"Plan ready.\r\n  Should I start the brief?\r\n",
        );
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));
        let public = app.public_pane_id(0, pane_id).unwrap();

        let d = digest(&mut app, empty_params());
        assert_eq!(d.agents.len(), 1);
        let a = &d.agents[0];
        assert_eq!(a.foreground.value, Foreground::AwaitingHuman);
        assert_eq!(a.foreground.source, FactSource::ScreenHeuristic);
        assert_eq!(a.foreground.confidence, Confidence::Low);
        let open: Vec<_> = a
            .pending_questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, QuestionKind::ProseQuestion);
        assert_eq!(open[0].excerpt, "Should I start the brief?");
        assert_eq!(a.completion, Completion::Incomplete);
        // First sight is generation 1; the first hook report starts the agent, which the
        // registry records as an AgentStarted bump before the digest runs.
        assert_eq!(a.target_ref.generation, 2);
        assert_eq!(
            Some(a.target_ref.generation),
            app.state.supervisor.pane(pane_id).map(|s| s.generation)
        );
        assert_eq!(a.lineage.len(), 1);
        assert_eq!(
            a.lineage[0].reason,
            crate::api::schema::supervisor::GenerationReason::AgentStarted
        );
        assert_eq!(a.target_ref.pane_id, public);
        assert_eq!(a.target_ref.server_epoch, d.server_epoch);
        let first_id = open[0].question_id.clone();

        let d2 = digest(&mut app, empty_params());
        let open2: Vec<_> = d2.agents[0]
            .pending_questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .collect();
        assert_eq!(open2.len(), 1);
        assert_eq!(open2[0].question_id, first_id);
        assert_ne!(d.trace_id, d2.trace_id);
    }

    fn open_questions(d: &AgentsDigest) -> Vec<String> {
        d.agents[0]
            .pending_questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .map(|q| q.excerpt.clone())
            .collect()
    }

    #[tokio::test]
    async fn supervisor_digest_d13_rescan_once() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-d13", b"Thinking about it.\r\n");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));
        assert!(open_questions(&digest(&mut app, empty_params())).is_empty());

        // Backdate the Idle start past the quiescence window instead of sleeping.
        {
            let state = app.state.supervisor.pane_mut(pane_id);
            let idle_since = state.idle_since_ms.expect("idle recorded");
            state.idle_since_ms = Some(
                idle_since.saturating_sub(crate::supervisor::registry::QUIESCENCE_RESCAN_MS + 1),
            );
            assert!(!state.rescanned_this_idle);
        }
        let feed = |app: &App, bytes: &[u8]| {
            app.state
                .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
                .expect("runtime")
                .test_process_pty_bytes(bytes);
        };
        feed(&app, b"  Should I deploy now?\r\n");

        let d = digest(&mut app, empty_params());
        assert!(
            app.state
                .supervisor
                .pane(pane_id)
                .expect("pane tracked")
                .rescanned_this_idle
        );
        assert_eq!(open_questions(&d), vec!["Should I deploy now?".to_string()]);
        let a = &d.agents[0];
        assert_eq!(a.foreground.value, Foreground::AwaitingHuman);
        assert!(a
            .pending_questions
            .iter()
            .all(|q| q.kind == QuestionKind::ProseQuestion));

        // One rescan per Idle turn: a later question line is not picked up.
        feed(&app, b"  Or should I wait for review?\r\n");
        let d = digest(&mut app, empty_params());
        assert_eq!(open_questions(&d), vec!["Should I deploy now?".to_string()]);
    }

    #[tokio::test]
    async fn supervisor_digest_bad_cursor_invalid_request() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, _pane_id) = app_with_screen("digest-cursor", b"");
        let mut params = empty_params();
        params.page_cursor = Some("o:9".into());
        let raw = digest_raw(&mut app, params);
        let response: crate::api::schema::ErrorResponse = serde_json::from_str(&raw).expect(&raw);
        assert_eq!(response.id, "dg");
        assert_eq!(response.error.code, "invalid_request");
        assert_eq!(response.error.message, "invalid page_cursor");
    }

    #[tokio::test]
    async fn supervisor_digest_filter_unknown_pane_omitted() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-filter", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        let mut params = empty_params();
        params.filter = Some(DigestFilter {
            pane_ids: Some(vec!["nope".into()]),
        });
        let d = digest(&mut app, params);
        assert!(d.agents.is_empty());
        assert_eq!(d.next_page, None);
    }

    #[tokio::test]
    async fn supervisor_digest_background_hook() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-bg", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));

        report_background_token(&mut app, pane_id, Some("1".to_string()), None);
        let d = digest(&mut app, empty_params());
        assert_eq!(d.agents.len(), 1);
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Active);
        assert_eq!(a.background.source, FactSource::Hook);
        assert_eq!(a.background_jobs, Some(1));
        assert_eq!(a.completion, Completion::Incomplete);
        assert_eq!(a.blocking_reason, Some(BlockingReason::BackgroundActive));

        // A current, live zero is still uncorrelated (C18b open), so it reads Unknown.
        report_background_token(&mut app, pane_id, Some("0".to_string()), Some(LIVE_TTL_MS));
        assert!(app.state.supervisor.background_hook_current(pane_id));
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Unknown);
        assert_eq!(a.background_jobs, None);
        assert_eq!(a.completion, Completion::Incomplete);
        assert_eq!(a.blocking_reason, Some(BlockingReason::BackgroundUnknown));

        // Re-reporting zero without a TTL removes the validity boundary.
        report_background_token(&mut app, pane_id, Some("0".to_string()), None);
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Unknown);
        assert_eq!(a.background_jobs, None);
        assert_ne!(a.completion, Completion::Complete);
    }

    /// Zero always reads Unknown (C18b interim), so the read-time deadline check is
    /// observed end to end through a positive count.
    #[tokio::test]
    async fn supervisor_digest_expired_positive_before_sweep_is_unknown() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-bg-expired-pos", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));
        report_background_token(&mut app, pane_id, Some("1".to_string()), Some(LIVE_TTL_MS));
        let d = digest(&mut app, empty_params());
        assert_eq!(d.agents[0].background.value, Background::Active);
        assert_eq!(d.agents[0].background_jobs, Some(1));

        report_background_token(&mut app, pane_id, Some("1".to_string()), Some(1));
        // Wait past the 1 ms deadline without running the expiry sweep.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
        while std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Unknown);
        assert_eq!(a.background_jobs, None);
        assert_eq!(a.blocking_reason, Some(BlockingReason::BackgroundUnknown));
    }

    #[tokio::test]
    async fn supervisor_digest_expired_zero_before_sweep_is_unknown() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-bg-expired", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));
        report_background_token(&mut app, pane_id, Some("0".to_string()), Some(1));
        // Wait past the 1 ms deadline without running the expiry sweep.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
        while std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Unknown);
        assert_ne!(a.completion, Completion::Complete);
        // The digest is read-only: the expired token is still stored.
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        assert!(app.state.terminals[&terminal_id]
            .metadata_tokens
            .values()
            .contains_key("background_jobs"));
    }

    #[tokio::test]
    async fn supervisor_digest_stale_seq_report_does_not_refresh_hook() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-bg-seq", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));

        let mut params = background_report(&app, pane_id, Some("1".to_string()));
        params.seq = Some(5);
        params.ttl_ms = Some(LIVE_TTL_MS);
        send_report(&mut app, params);
        assert!(app.state.supervisor.background_hook_current(pane_id));
        let (hook_seq, hook_ms) = {
            let state = app.state.supervisor.pane(pane_id).expect("pane tracked");
            (state.background_hook_seq, state.background_hook_ms)
        };
        assert!(hook_seq.is_some());

        // A new turn makes the report stale; a stale-sequence zero must not refresh it.
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 3));
        assert!(!app.state.supervisor.background_hook_current(pane_id));
        let mut params = background_report(&app, pane_id, Some("0".to_string()));
        params.seq = Some(3);
        params.ttl_ms = Some(LIVE_TTL_MS);
        send_report(&mut app, params);

        assert!(!app.state.supervisor.background_hook_current(pane_id));
        let state = app.state.supervisor.pane(pane_id).expect("pane tracked");
        assert_eq!(state.background_hook_seq, hook_seq);
        assert_eq!(state.background_hook_ms, hook_ms);
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Unknown);
        assert_eq!(a.background_jobs, None);
    }

    #[tokio::test]
    async fn supervisor_digest_first_label_report_keeps_background() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-bg-label", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 2));

        // One report that both carries presentation (dispatching HookMetadataReported)
        // and background_jobs: the hook is noted after the lifecycle change.
        let mut params = background_report(&app, pane_id, Some("1".to_string()));
        params.source = "custom:worker".into();
        params.agent = Some("worker".into());
        params.title = Some("building".into());
        send_report(&mut app, params);

        assert!(app.state.supervisor.background_hook_current(pane_id));
        let d = digest(&mut app, empty_params());
        assert_eq!(d.agents.len(), 1);
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Active);
        assert_eq!(a.background.source, FactSource::Hook);
        assert_eq!(a.background_jobs, Some(1));
    }

    fn app_with_two_panes(name: &str, screen: &[u8]) -> (App, PaneId) {
        let (mut app, _root) = app_with_screen(name, b"");
        let moved = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = app.state.workspaces[0]
            .pane_state(moved)
            .expect("moved pane state")
            .attached_terminal_id
            .clone();
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, screen);
        app.terminal_runtimes.insert(terminal_id, runtime);
        (app, moved)
    }

    fn move_pane_to_new_tab(app: &mut App, pane_id: PaneId) {
        let public = app.public_pane_id(0, pane_id).expect("pane public id");
        let response = app.handle_api_request(Request {
            id: "move".into(),
            method: Method::PaneMove(crate::api::schema::PaneMoveParams {
                pane_id: public,
                destination: crate::api::schema::PaneMoveDestination::NewTab {
                    workspace_id: None,
                    label: Some("moved".into()),
                },
                focus: true,
            }),
        });
        serde_json::from_str::<SuccessResponse>(&response)
            .unwrap_or_else(|e| panic!("pane move failed: {e}: {response}"));
    }

    #[tokio::test]
    async fn supervisor_digest_pane_move_keeps_hook_currency() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, moved) = app_with_two_panes("digest-bg-move", b"");
        app.handle_internal_event(hook_state(moved, AgentState::Working, 1));
        app.handle_internal_event(hook_state(moved, AgentState::Idle, 2));
        report_background_token(&mut app, moved, Some("0".to_string()), Some(LIVE_TTL_MS));
        let report_ms = app.state.supervisor.background_hook_ms(moved);

        move_pane_to_new_tab(&mut app, moved);

        assert!(app.state.supervisor.background_hook_current(moved));
        let public = app.public_pane_id(0, moved).expect("moved pane public id");
        let d = digest(&mut app, empty_params());
        let a = d
            .agents
            .iter()
            .find(|a| a.target_ref.pane_id == public)
            .expect("moved agent in digest");
        assert_eq!(
            a.lineage.last().map(|l| l.reason),
            Some(crate::api::schema::supervisor::GenerationReason::PaneMoved)
        );
        // Hook currency survives the move; the uncorrelated zero still reads Unknown.
        assert!(report_ms.is_some());
        assert_eq!(a.background.value, Background::Unknown);
    }

    #[tokio::test]
    async fn supervisor_digest_exited_survives_pane_move() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, moved) = app_with_two_panes("digest-exit-move", b"");
        app.handle_internal_event(hook_state(moved, AgentState::Working, 1));
        app.handle_internal_event(AppEvent::HookAgentReleased {
            pane_id: moved,
            source: "custom:worker".into(),
            agent_label: "worker".into(),
            known_agent: None,
            seq: Some(2),
        });
        assert!(app.state.supervisor.pane(moved).expect("tracked").exited);
        let generation = app
            .state
            .supervisor
            .pane(moved)
            .expect("tracked")
            .generation;
        // Keep the pane listed as an agent pane after release so the digest reports it.
        let terminal_id = app.state.workspaces[0]
            .pane_state(moved)
            .expect("moved pane state")
            .attached_terminal_id
            .clone();
        if let Some(terminal) = app.state.terminals.get_mut(&terminal_id) {
            terminal.agent_name = Some("named-worker".into());
        }

        move_pane_to_new_tab(&mut app, moved);

        let state = app.state.supervisor.pane(moved).expect("tracked");
        assert!(state.exited);
        assert_eq!(state.generation, generation + 1);
        let public = app.public_pane_id(0, moved).expect("moved pane public id");
        let d = digest(&mut app, empty_params());
        let a = d
            .agents
            .iter()
            .find(|a| a.target_ref.pane_id == public)
            .expect("moved agent in digest");
        assert_eq!(
            a.connection.value,
            crate::api::schema::supervisor::Connection::Disconnected
        );
        assert_ne!(a.completion, Completion::Complete);
    }

    #[tokio::test]
    async fn supervisor_digest_pane_move_keeps_question_incomplete() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, _root) = app_with_screen("digest-move", b"");
        let moved = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = app.state.workspaces[0]
            .pane_state(moved)
            .expect("moved pane state")
            .attached_terminal_id
            .clone();
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(
            80,
            24,
            b"Plan ready.
  Should I start the brief?
",
        );
        app.terminal_runtimes.insert(terminal_id, runtime);
        app.handle_internal_event(hook_state(moved, AgentState::Working, 1));
        app.handle_internal_event(hook_state(moved, AgentState::Idle, 2));

        let public = app.public_pane_id(0, moved).expect("moved pane public id");
        let d = digest(&mut app, empty_params());
        let a = d
            .agents
            .iter()
            .find(|a| a.target_ref.pane_id == public)
            .expect("moved agent in digest");
        let before_generation = a.target_ref.generation;
        let open_before: Vec<_> = a
            .pending_questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .map(|q| q.question_id.clone())
            .collect();
        assert_eq!(open_before.len(), 1);

        let response = app.handle_api_request(Request {
            id: "move".into(),
            method: Method::PaneMove(crate::api::schema::PaneMoveParams {
                pane_id: public,
                destination: crate::api::schema::PaneMoveDestination::NewTab {
                    workspace_id: None,
                    label: Some("moved".into()),
                },
                focus: true,
            }),
        });
        serde_json::from_str::<SuccessResponse>(&response)
            .unwrap_or_else(|e| panic!("pane move failed: {e}: {response}"));

        report_background_token(&mut app, moved, Some("0".to_string()), Some(LIVE_TTL_MS));
        let public = app.public_pane_id(0, moved).expect("moved pane public id");
        let d = digest(&mut app, empty_params());
        let a = d
            .agents
            .iter()
            .find(|a| a.target_ref.pane_id == public)
            .expect("moved agent in digest");
        assert_eq!(a.target_ref.generation, before_generation + 1);
        assert_eq!(
            a.lineage.last().map(|l| l.reason),
            Some(crate::api::schema::supervisor::GenerationReason::PaneMoved)
        );
        let open_after: Vec<_> = a
            .pending_questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .map(|q| q.question_id.clone())
            .collect();
        assert_eq!(open_after, open_before);
        assert_eq!(a.background.value, Background::Unknown);
        assert_eq!(a.completion, Completion::Incomplete);
    }

    fn cursor_error(app: &mut App, cursor: &str) -> String {
        let mut params = empty_params();
        params.page_cursor = Some(cursor.to_string());
        let raw = digest_raw(app, params);
        let response: crate::api::schema::ErrorResponse = serde_json::from_str(&raw).expect(&raw);
        assert_eq!(response.error.code, "invalid_request");
        response.error.message
    }

    fn with_cursor(cursor: String) -> AgentsDigestParams {
        let mut params = empty_params();
        params.page_cursor = Some(cursor);
        params
    }

    fn digest_fingerprint(d: &AgentsDigest) -> String {
        membership_fingerprint(d.agents.iter().map(|a| a.target_ref.pane_id.as_str()))
    }

    /// Two agent panes: the root and a split.
    fn app_with_two_agents(name: &str) -> (App, PaneId, PaneId) {
        let (mut app, split) = app_with_two_panes(name, b"");
        let root = app.state.workspaces[0].tabs[0].root_pane;
        app.handle_internal_event(hook_state(root, AgentState::Working, 1));
        app.handle_internal_event(hook_state(split, AgentState::Working, 1));
        (app, root, split)
    }

    #[tokio::test]
    async fn supervisor_digest_cursor_pages_with_unchanged_membership() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, _root, _split) = app_with_two_agents("digest-page-ok");
        let first = digest(&mut app, empty_params());
        assert_eq!(first.agents.len(), 2);
        let fp = digest_fingerprint(&first);

        let second = digest(&mut app, with_cursor(format!("o:1:{fp}")));
        assert_eq!(second.agents.len(), 1);
        assert_eq!(
            second.agents[0].target_ref.pane_id,
            first.agents[1].target_ref.pane_id
        );
        assert_eq!(second.next_page, None);
        // Offset 0 with a matching fingerprint is the first page.
        assert_eq!(
            digest(&mut app, with_cursor(format!("o:0:{fp}")))
                .agents
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn supervisor_digest_cursor_stale_after_agent_pane_closed() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, _root, split) = app_with_two_agents("digest-page-closed");
        let first = digest(&mut app, empty_params());
        assert_eq!(first.agents.len(), 2);
        let fp = digest_fingerprint(&first);

        let public = app.public_pane_id(0, split).expect("split public id");
        let response = app.handle_api_request(Request {
            id: "close".into(),
            method: Method::PaneClose(crate::api::schema::PaneTarget { pane_id: public }),
        });
        serde_json::from_str::<SuccessResponse>(&response)
            .unwrap_or_else(|e| panic!("pane close failed: {e}: {response}"));

        assert_eq!(
            cursor_error(&mut app, &format!("o:1:{fp}")),
            "stale page_cursor"
        );
    }

    #[tokio::test]
    async fn supervisor_digest_cursor_stale_after_agent_pane_added() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, split) = app_with_two_panes("digest-page-added", b"");
        let root = app.state.workspaces[0].tabs[0].root_pane;
        app.handle_internal_event(hook_state(root, AgentState::Working, 1));
        let first = digest(&mut app, empty_params());
        assert_eq!(first.agents.len(), 1);
        let fp = digest_fingerprint(&first);

        // The split pane's terminal becomes a real agent terminal.
        app.handle_internal_event(hook_state(split, AgentState::Working, 1));
        assert_eq!(digest(&mut app, empty_params()).agents.len(), 2);

        assert_eq!(
            cursor_error(&mut app, &format!("o:0:{fp}")),
            "stale page_cursor"
        );
    }

    #[tokio::test]
    async fn supervisor_digest_cursor_malformed_is_invalid() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, _root, _split) = app_with_two_agents("digest-page-bad");
        let fp = digest_fingerprint(&digest(&mut app, empty_params()));
        // Legacy cursor without a fingerprint.
        assert_eq!(cursor_error(&mut app, "o:1"), "invalid page_cursor");
        // Empty fingerprint segment.
        assert_eq!(cursor_error(&mut app, "o:1:"), "invalid page_cursor");
        // Over-long cursor.
        let long = format!("o:{}:{fp}", "0".repeat(60));
        assert_eq!(cursor_error(&mut app, &long), "invalid page_cursor");
        // Matching fingerprint but out of range.
        assert_eq!(
            cursor_error(&mut app, &format!("o:2:{fp}")),
            "invalid page_cursor"
        );
        // First page needs no cursor.
        assert_eq!(digest(&mut app, empty_params()).agents.len(), 2);
    }

    /// A zero count reported during Working goes stale on a later Working report even
    /// without a state edge; a zero reported after the last Working report counts.
    #[tokio::test]
    async fn supervisor_digest_working_report_without_edge_stales_zero() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-bg-rework", b"");
        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 1));
        report_background_token(&mut app, pane_id, Some("0".to_string()), Some(LIVE_TTL_MS));
        assert!(app.state.supervisor.background_hook_current(pane_id));
        let turn_started = app
            .state
            .supervisor
            .pane(pane_id)
            .expect("pane tracked")
            .turn_started_ms;

        app.handle_internal_event(hook_state(pane_id, AgentState::Working, 2));
        assert!(!app.state.supervisor.background_hook_current(pane_id));
        assert_eq!(
            app.state
                .supervisor
                .pane(pane_id)
                .expect("pane tracked")
                .turn_started_ms,
            turn_started
        );
        app.handle_internal_event(hook_state(pane_id, AgentState::Idle, 3));
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Unknown);
        assert_ne!(a.completion, Completion::Complete);

        report_background_token(&mut app, pane_id, Some("0".to_string()), Some(LIVE_TTL_MS));
        assert!(app.state.supervisor.background_hook_current(pane_id));
        let d = digest(&mut app, empty_params());
        assert_eq!(d.agents[0].background.value, Background::Unknown);
    }

    /// A detection state report without an edge does not start a new turn.
    #[tokio::test]
    async fn supervisor_digest_detection_without_edge_keeps_turn() {
        let _reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (mut app, pane_id) = app_with_screen("digest-detect-rework", b"");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        if let Some(terminal) = app.state.terminals.get_mut(&terminal_id) {
            terminal.detected_agent = Some(crate::detect::Agent::Claude);
        }
        let detect = |state: AgentState| AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Claude),
            state,
            visible_blocker: false,
            visible_working: state == AgentState::Working,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        };
        app.handle_internal_event(detect(AgentState::Working));
        let turn = app.state.supervisor.pane(pane_id).and_then(|s| s.turn_seq);
        assert!(turn.is_some());
        app.handle_internal_event(detect(AgentState::Working));
        assert_eq!(
            app.state.supervisor.pane(pane_id).and_then(|s| s.turn_seq),
            turn
        );
    }
}

//! Fork-only `agents.digest` handler (supervisor facts, read-only). Disabled unless
//! `[supervisor] enabled`.

use crate::api::schema::supervisor::{AgentsDigest, AgentsDigestParams, DigestTruncation};
use crate::api::schema::{AgentInfo, ResponseResult};
use crate::app::App;
use crate::layout::PaneId;
use crate::supervisor::digest::{
    build_agent_facts, build_page, parse_cursor, AgentFactInputs, MAX_PAGE_BYTES,
};

use super::responses::{encode_error, encode_success};

/// Max logged chars of the client-supplied trace id.
const CLIENT_TRACE_ID_LOG_CHARS: usize = 64;
/// Max logged chars of the client-supplied request id.
const REQUEST_ID_LOG_CHARS: usize = 128;

/// Client-supplied value reduced to `[A-Za-z0-9-_.:]`, at most `max_chars` chars
/// (CWE-117). Logged only; never echoed in a response or used as our trace id.
fn sanitize_log_token(raw: Option<&str>, max_chars: usize) -> String {
    raw.unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .take(max_chars)
        .collect()
}

/// Per-call context for the single `supervisor.request` event.
struct RequestLog {
    started: std::time::Instant,
    trace_id: String,
    client_trace_id: String,
    request_id: String,
}

impl RequestLog {
    fn ok(&self, agents: usize, response: &str) {
        tracing::debug!(
            event = "supervisor.request",
            subsystem = "supervisor",
            method = "agents.digest",
            outcome = "ok",
            trace_id = %self.trace_id,
            client_trace_id = %self.client_trace_id,
            request_id = %self.request_id,
            agents = u32::try_from(agents).unwrap_or(u32::MAX),
            bytes = response.len() as u64,
            duration_us = self.duration_us()
        );
    }

    fn error(&self, error_code: &str, response: &str) {
        tracing::warn!(
            event = "supervisor.request",
            subsystem = "supervisor",
            method = "agents.digest",
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

        // Validate the cursor before any rescan or fact building.
        let offset = match parse_cursor(params.page_cursor.as_deref(), panes.len()) {
            Ok(offset) => offset,
            Err(()) => {
                let response = encode_error(id, "invalid_request", "invalid page_cursor");
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
        let page = build_page(facts, offset, MAX_PAGE_BYTES);
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
        let (questions, lineage, last_transition_ms, exited) =
            match self.state.supervisor.pane(pane_id) {
                Some(state) => (
                    state.questions.clone(),
                    state.lineage.iter().cloned().collect(),
                    state.last_transition_ms,
                    state.exited,
                ),
                None => (Vec::new(), Vec::new(), None, false),
            };
        let (runtime_present, last_output_ms, last_owner_input_ms) = match self
            .state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        {
            Some(runtime) => {
                let activity = runtime.supervisor_activity();
                (
                    true,
                    activity.last_output_ms(),
                    activity.last_owner_input_ms(),
                )
            }
            None => (false, None, None),
        };
        let (background, background_jobs) = crate::supervisor::background::derive_background(
            info.tokens.get("background_jobs").map(String::as_str),
            self.state.supervisor.background_hook_current(pane_id),
            self.background_rule_hit(ws_idx, pane_id),
            now,
        );
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

    fn report_background_token(app: &mut App, pane_id: PaneId, value: Option<String>) {
        let target = app.public_pane_id(0, pane_id).unwrap();
        let mut tokens = std::collections::HashMap::new();
        tokens.insert("background_jobs".to_string(), value);
        let response = app.handle_api_request(Request {
            id: "bg".into(),
            method: Method::PaneReportMetadata(crate::api::schema::PaneReportMetadataParams {
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
            }),
        });
        assert!(response.contains("\"ok\""), "{response}");
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

        report_background_token(&mut app, pane_id, Some("1".to_string()));
        let d = digest(&mut app, empty_params());
        assert_eq!(d.agents.len(), 1);
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Active);
        assert_eq!(a.background.source, FactSource::Hook);
        assert_eq!(a.background_jobs, Some(1));
        assert_eq!(a.completion, Completion::Incomplete);
        assert_eq!(a.blocking_reason, Some(BlockingReason::BackgroundActive));

        report_background_token(&mut app, pane_id, Some("0".to_string()));
        let d = digest(&mut app, empty_params());
        let a = &d.agents[0];
        assert_eq!(a.background.value, Background::Inactive);
        assert_eq!(a.background_jobs, Some(0));
    }
}

//! Fork-only `agent.submit` / `agent.submit_status` handlers (S2/S3). Disabled unless
//! `[supervisor] enabled`. Every rejection happens before any PTY call and records nothing.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::api::schema::supervisor::{
    AgentSubmitParams, AgentSubmitStatusParams, DraftState, SubmitReceipt,
};
use crate::api::schema::ResponseResult;
use crate::app::App;
use crate::events::AppEvent;
use crate::supervisor::fence::SubmitFence;
use crate::supervisor::submit::{
    text_sha256_16, valid_submit_id, valid_submit_text, Repeat, SubmitEvent, SUBMIT_DEADLINE,
};

use super::agents::agent_not_found;
use super::responses::{encode_error, encode_success};
use super::supervisor::RequestLog;

/// Same paste-to-Enter delay as `agent.prompt`.
const SUBMIT_ENTER_DELAY: Duration = Duration::from_millis(300);

/// Debug builds only: delays a submission write so a fixture can trip the fence while it
/// is queued. Never compiled into release builds.
#[cfg(debug_assertions)]
pub(crate) const DEV_SUBMIT_HOLD_ENV: &str = "HERDR_DEV_SUBMIT_HOLD_MS";
#[cfg(debug_assertions)]
const DEV_SUBMIT_HOLD_MAX_MS: u64 = 60_000;

#[cfg(debug_assertions)]
fn dev_submit_hold() -> Option<Duration> {
    let ms: u64 = std::env::var(DEV_SUBMIT_HOLD_ENV)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (ms > 0).then(|| Duration::from_millis(ms.min(DEV_SUBMIT_HOLD_MAX_MS)))
}

#[cfg(not(debug_assertions))]
fn dev_submit_hold() -> Option<Duration> {
    None
}

fn draft_name(draft: DraftState) -> &'static str {
    match draft {
        DraftState::Clean => "clean",
        DraftState::Dirty => "dirty",
        DraftState::Unknown => "unknown",
    }
}

fn receipt_response(id: String, receipt: SubmitReceipt) -> String {
    encode_success(id, ResponseResult::AgentSubmitReceipt { receipt })
}

impl App {
    pub(super) fn handle_agent_submit(&mut self, id: String, params: AgentSubmitParams) -> String {
        let hold = dev_submit_hold();
        self.handle_agent_submit_with_hold(id, params, hold)
    }

    fn handle_agent_submit_with_hold(
        &mut self,
        id: String,
        params: AgentSubmitParams,
        hold: Option<Duration>,
    ) -> String {
        let log = RequestLog::new("agent.submit", &id);
        let reject = |id: String, code: &str, message: String| {
            let response = encode_error(id, code, message);
            log.error(code, &response);
            response
        };
        if !crate::supervisor::enabled() {
            return reject(
                id,
                "supervisor_disabled",
                "supervisor mode is disabled; set [supervisor] enabled = true and restart the server"
                    .into(),
            );
        }
        if !valid_submit_id(&params.submit_id) {
            return reject(
                id,
                "invalid_request",
                "submit_id must be 1-64 characters of [A-Za-z0-9._:-]".into(),
            );
        }
        if !valid_submit_text(&params.text) {
            return reject(id, "invalid_request", "text must be 1-8192 bytes".into());
        }
        let text_hash = text_sha256_16(&params.text);
        match self.state.supervisor.submits.repeat(
            &params.submit_id,
            &params.target_ref,
            &text_hash,
        ) {
            Repeat::Same(receipt) => {
                let response = receipt_response(id, *receipt);
                log.ok(1, &response);
                return response;
            }
            Repeat::Conflict => {
                return reject(
                    id,
                    "submit_id_conflict",
                    "submit_id was already used with a different target_ref or text".into(),
                );
            }
            Repeat::New => {}
        }
        if self.state.supervisor.submits.is_full() {
            return reject(
                id,
                "submit_capacity",
                "submit receipt ledger is full for this server process".into(),
            );
        }
        let parsed = self.parse_current_public_pane_id(&params.target_ref.pane_id);
        if let Err(stale) = self
            .state
            .supervisor
            .check_target_ref(&params.target_ref, parsed.map(|(_, pane)| pane))
        {
            let current = stale
                .current_generation
                .map_or_else(|| "none".to_string(), |g| g.to_string());
            return reject(
                id,
                "stale_target",
                format!("target_ref is stale; current_generation={current}"),
            );
        }
        let Some((ws_idx, pane_id)) = parsed else {
            return reject(
                id,
                "stale_target",
                "target_ref is stale; current_generation=none".into(),
            );
        };
        let validated = match self.validate_agent_target(id, &params.target_ref.pane_id, false) {
            Ok(validated) => validated,
            Err(response) => {
                log.error("agent_target_rejected", &response);
                return response;
            }
        };
        let id = validated.id;
        if validated.ws_idx != ws_idx || validated.pane_id != pane_id {
            let response = agent_not_found(id, &params.target_ref.pane_id);
            log.error("agent_not_found", &response);
            return response;
        }
        if validated.expected_agent != crate::detect::Agent::Claude {
            return reject(
                id,
                "unsupported_agent",
                format!(
                    "agent.submit supports claude only, not {}",
                    crate::detect::agent_label(validated.expected_agent)
                ),
            );
        }
        if validated.blocked {
            return reject(
                id,
                "agent_blocked",
                "agent is blocked and requires interactive input".into(),
            );
        }
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            let response = agent_not_found(id, &params.target_ref.pane_id);
            log.error("agent_not_found", &response);
            return response;
        };
        let activity = std::sync::Arc::clone(runtime.supervisor_activity());
        let now = crate::supervisor::now_ms();
        // A valid one-shot operator handoff lifts the sticky owner-input latch for this one
        // submit; the screen rule (and the equal-ms rule) still decide the draft.
        let operator_handoff = activity.valid_handoff(params.target_ref.generation, now);
        let latched = activity.draft_latched() && operator_handoff.is_none();
        let draft = self.pane_draft_state(
            ws_idx,
            pane_id,
            Some(crate::detect::agent_label(validated.expected_agent)),
            latched,
            activity.last_owner_input_ms(),
            activity.last_output_ms(),
        );
        if draft != DraftState::Clean {
            return reject(
                id,
                "draft_not_clean",
                format!(
                    "owner input draft is not clean; draft={}",
                    draft_name(draft)
                ),
            );
        }

        // Accept. The waiter starts first so a refused thread fails while nothing is
        // recorded or sent.
        let fence = SubmitFence::new();
        let handoff = match self.spawn_submit_waiter(params.submit_id.clone(), fence.clone()) {
            Ok(handoff) => handoff,
            Err(err) => {
                return reject(
                    id,
                    "submit_failed",
                    format!("could not start submit completion waiter: {err}"),
                );
            }
        };
        let Some(receipt) = self.state.supervisor.submits.insert_accepted(
            &params.submit_id,
            pane_id,
            params.target_ref.clone(),
            text_hash,
            fence.clone(),
            operator_handoff.is_some(),
            now,
        ) else {
            return reject(
                id,
                "submit_capacity",
                "submit receipt ledger is full for this server process".into(),
            );
        };
        // Consume the handoff at accept, before queueing. Owner input cancels it on this
        // same thread, so a failed consume means owner input won: fence the write.
        if let Some(handoff) = operator_handoff {
            if !activity.consume_handoff(handoff) {
                fence.trip(crate::supervisor::fence::FenceReason::OwnerInput);
            }
        }
        activity.arm_submit_fence(fence.clone());
        let Some((text, enter)) = self.lookup_runtime_sender(ws_idx, pane_id).map(|runtime| {
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text)
        }) else {
            // Unreachable in practice: the runtime was resolved above on this thread.
            let receipt = self
                .state
                .supervisor
                .submits
                .mark_queue_failed(&params.submit_id)
                .unwrap_or(receipt);
            let response = receipt_response(id, receipt);
            log.ok(1, &response);
            return response;
        };
        let deadline = Instant::now() + SUBMIT_DEADLINE + hold.unwrap_or_default();

        #[cfg(debug_assertions)]
        if let Some(hold) = hold {
            self.state.supervisor.submits.hold(
                &params.submit_id,
                crate::supervisor::submit::HeldWrite {
                    text: Bytes::from(text),
                    enter: Bytes::from(enter),
                    deadline,
                },
            );
            self.state
                .supervisor
                .submits
                .attach_handoff(&params.submit_id, handoff);
            let event_tx = self.event_tx.clone();
            let submit_id = params.submit_id.clone();
            let spawned = crate::thread_spawn::spawn_named("herdr-submit-hold", move || {
                std::thread::sleep(hold);
                let _ = event_tx.blocking_send(AppEvent::SupervisorSubmit(SubmitEvent::Release {
                    submit_id,
                }));
            });
            if spawned.is_err() {
                self.release_held_submit(&params.submit_id);
            }
            let response = receipt_response(id, receipt);
            log.ok(1, &response);
            return response;
        }
        #[cfg(not(debug_assertions))]
        let _ = hold;

        let queued = match self.lookup_runtime_sender(ws_idx, pane_id) {
            Some(runtime) => runtime.queue_fenced_user_input_submission(
                Bytes::from(text),
                Bytes::from(enter),
                SUBMIT_ENTER_DELAY,
                Some(deadline),
                fence,
            ),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pane runtime is gone",
            )),
        };
        let receipt = match queued {
            Ok(completion) => {
                let _ = handoff.send(completion);
                receipt
            }
            Err(_) => self
                .state
                .supervisor
                .submits
                .mark_queue_failed(&params.submit_id)
                .unwrap_or(receipt),
        };
        let response = receipt_response(id, receipt);
        log.ok(1, &response);
        response
    }

    pub(super) fn handle_agent_submit_status(
        &mut self,
        id: String,
        params: AgentSubmitStatusParams,
    ) -> String {
        let log = RequestLog::new("agent.submit_status", &id);
        if !crate::supervisor::enabled() {
            let response = encode_error(
                id,
                "supervisor_disabled",
                "supervisor mode is disabled; set [supervisor] enabled = true and restart the server",
            );
            log.error("supervisor_disabled", &response);
            return response;
        }
        match self.state.supervisor.submits.get(&params.submit_id) {
            Some(receipt) => {
                let response = receipt_response(id, receipt.clone());
                log.ok(1, &response);
                response
            }
            None => {
                let response = encode_error(
                    id,
                    "submit_not_found",
                    "submit_id is unknown to this server process",
                );
                log.error("submit_not_found", &response);
                response
            }
        }
    }

    /// Fork: arms a one-shot operator handoff on a pane's per-PTY activity. Reached only from
    /// the attached-client `HANDOFF_CONTROL_KIND` control message (the operator's
    /// `supervisor_handoff` keybinding); no API method or CLI command calls it. Replaces any
    /// previous handoff on the pane. Logs one bounded `supervisor.handoff` event (no text).
    pub(crate) fn arm_supervisor_handoff(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        public_pane_id: &str,
        now_ms: u64,
    ) -> Result<crate::supervisor::activity::Handoff, &'static str> {
        let pane = super::supervisor::sanitize_log_token(Some(public_pane_id), 64);
        if !crate::supervisor::enabled() {
            tracing::debug!(
                event = "supervisor.handoff",
                subsystem = "supervisor",
                outcome = "rejected",
                error_code = "supervisor_disabled",
                pane = %pane,
            );
            return Err("supervisor_disabled");
        }
        let Some(activity) = self
            .lookup_runtime_sender(ws_idx, pane_id)
            .map(|runtime| std::sync::Arc::clone(runtime.supervisor_activity()))
        else {
            tracing::debug!(
                event = "supervisor.handoff",
                subsystem = "supervisor",
                outcome = "rejected",
                error_code = "pane_not_found",
                pane = %pane,
            );
            return Err("pane_not_found");
        };
        let (generation, armed_seq) = self.state.supervisor.note_handoff_armed(pane_id);
        let handoff = crate::supervisor::activity::Handoff {
            armed_at_ms: now_ms,
            armed_seq,
            generation,
        };
        activity.arm_handoff(handoff);
        tracing::debug!(
            event = "supervisor.handoff",
            subsystem = "supervisor",
            outcome = "armed",
            pane = %pane,
            generation,
            armed_seq,
        );
        Ok(handoff)
    }

    /// Starts the thread that waits for the PTY write result and posts it to the App.
    /// The fence is closed before posting so a later trip cannot read as a late trip.
    fn spawn_submit_waiter(
        &self,
        submit_id: String,
        fence: SubmitFence,
    ) -> std::io::Result<std::sync::mpsc::Sender<std::sync::mpsc::Receiver<std::io::Result<()>>>>
    {
        let (handoff_tx, handoff_rx) =
            std::sync::mpsc::channel::<std::sync::mpsc::Receiver<std::io::Result<()>>>();
        let event_tx = self.event_tx.clone();
        crate::thread_spawn::spawn_named("herdr-agent-submit", move || {
            let Ok(completion) = handoff_rx.recv() else {
                // Queueing failed or was abandoned; the App already settled the receipt.
                return;
            };
            let result = completion.recv().unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ))
            });
            let fence = fence.close();
            let _ = event_tx.blocking_send(AppEvent::SupervisorSubmit(SubmitEvent::Completed {
                submit_id,
                result,
                fence,
            }));
        })?;
        Ok(handoff_tx)
    }

    pub(crate) fn handle_supervisor_submit_event(&mut self, event: SubmitEvent) {
        match event {
            SubmitEvent::Completed {
                submit_id,
                result,
                fence,
            } => {
                self.state.supervisor.complete_submit(
                    &submit_id,
                    &result,
                    fence,
                    crate::supervisor::now_ms(),
                );
            }
            #[cfg(debug_assertions)]
            SubmitEvent::Release { submit_id } => self.release_held_submit(&submit_id),
        }
    }

    /// Debug builds only: queues a write parked by the dev hold. The fence is still checked
    /// by the PTY writer before the first byte, so a trip during the hold yields `fenced`.
    #[cfg(debug_assertions)]
    fn release_held_submit(&mut self, submit_id: &str) {
        let submits = &mut self.state.supervisor.submits;
        let (Some(held), Some(handoff), Some(fence), Some(pane)) = (
            submits.take_held(submit_id),
            submits.take_handoff(submit_id),
            submits.fence(submit_id).cloned(),
            submits.pane(submit_id),
        ) else {
            return;
        };
        let runtime = self.find_pane(pane).and_then(|(ws_idx, _)| {
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane)
        });
        let queued = match runtime {
            Some(runtime) => runtime.queue_fenced_user_input_submission(
                held.text,
                held.enter,
                SUBMIT_ENTER_DELAY,
                Some(held.deadline),
                fence,
            ),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pane runtime is gone",
            )),
        };
        match queued {
            Ok(completion) => {
                let _ = handoff.send(completion);
            }
            Err(_) => {
                drop(handoff);
                self.state.supervisor.submits.mark_queue_failed(submit_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::supervisor::{SubmitReason, SubmitState, TargetRef};
    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};
    use crate::detect::{Agent, AgentState};
    use crate::layout::PaneId;

    const RULE: &str = "────────────────────────────────────────";

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::supervisor::set_enabled_for_test(None);
        }
    }

    struct Fixture {
        app: App,
        pane_id: PaneId,
        rx: tokio::sync::mpsc::Receiver<Bytes>,
        _reset: Reset,
    }

    fn screen(input: &str) -> Vec<u8> {
        format!("Done.\r\n\r\n{RULE}\r\n{input}\r\n{RULE}\r\n  ? for shortcuts").into_bytes()
    }

    fn fixture_with(agent: Agent, state: AgentState, input_row: &str) -> Fixture {
        let reset = Reset;
        crate::supervisor::set_enabled_for_test(Some(true));
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("submit")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = crate::app::Mode::Terminal;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("worker".into());
        terminal.set_detected_state(Some(agent), state);
        let (runtime, rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                &screen(input_row),
                8,
            );
        runtime.supervisor_activity().set_stamps_for_test(100, 0);
        app.state.insert_test_runtime(pane_id, runtime);
        Fixture {
            app,
            pane_id,
            rx,
            _reset: reset,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(Agent::Claude, AgentState::Idle, "❯ ")
    }

    impl Fixture {
        fn target(&mut self) -> TargetRef {
            let public = self.app.public_pane_id(0, self.pane_id).unwrap();
            self.app.state.supervisor.target_ref(self.pane_id, &public)
        }

        fn params(&mut self, submit_id: &str, text: &str) -> AgentSubmitParams {
            AgentSubmitParams {
                target_ref: self.target(),
                text: text.into(),
                submit_id: submit_id.into(),
            }
        }

        fn submit(&mut self, params: AgentSubmitParams) -> String {
            self.app.handle_api_request(Request {
                id: "s".into(),
                method: Method::AgentSubmit(params),
            })
        }

        fn status(&mut self, submit_id: &str) -> String {
            self.app.handle_api_request(Request {
                id: "st".into(),
                method: Method::AgentSubmitStatus(AgentSubmitStatusParams {
                    submit_id: submit_id.into(),
                }),
            })
        }

        /// Feeds internal events to the App until one submit event was handled.
        async fn pump_submit_event(&mut self) {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(2), self.app.event_rx.recv())
                    .await
                    .expect("submit event within 2s")
                    .expect("event channel open");
                let is_submit = matches!(event, AppEvent::SupervisorSubmit(_));
                self.app.handle_internal_event(event);
                if is_submit {
                    return;
                }
            }
        }

        fn activity(&self) -> std::sync::Arc<crate::supervisor::activity::PaneActivity> {
            std::sync::Arc::clone(
                self.app
                    .lookup_runtime_sender(0, self.pane_id)
                    .unwrap()
                    .supervisor_activity(),
            )
        }

        /// Owner input at `input_ms` (sets the sticky latch) followed by unrelated output at
        /// `output_ms`.
        fn owner_input_then_output(&self, input_ms: u64, output_ms: u64) {
            let activity = self.activity();
            activity.record_owner_input();
            activity.set_stamps_for_test(output_ms, input_ms);
        }

        fn arm_handoff(&mut self, now_ms: u64) -> crate::supervisor::activity::Handoff {
            let public = self.app.public_pane_id(0, self.pane_id).unwrap();
            self.app
                .arm_supervisor_handoff(0, self.pane_id, &public, now_ms)
                .expect("handoff armed")
        }

        fn handoff_valid(&mut self) -> bool {
            let generation = self.target().generation;
            self.activity()
                .valid_handoff(generation, crate::supervisor::now_ms())
                .is_some()
        }

        fn digest_handoff_armed(&mut self) -> bool {
            let raw = self.app.handle_api_request(Request {
                id: "dg".into(),
                method: Method::AgentsDigest(crate::api::schema::supervisor::AgentsDigestParams {
                    filter: None,
                    page_cursor: None,
                    trace_id: None,
                }),
            });
            let value: serde_json::Value = serde_json::from_str(&raw).expect(&raw);
            let agents = value["result"]["agents"].as_array().expect(&raw).clone();
            assert_eq!(agents.len(), 1, "{raw}");
            assert_eq!(agents[0]["owner_input"]["draft"], "unknown", "{raw}");
            agents[0]["owner_input"]["handoff_armed"]
                .as_bool()
                .expect(&raw)
        }

        fn assert_rejected_unknown(&mut self, submit_id: &str) {
            let params = self.params(submit_id, "hello");
            let raw = self.submit(params);
            assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
            assert!(raw.contains("draft=unknown"), "{raw}");
        }

        async fn assert_no_write(&mut self) {
            assert!(
                tokio::time::timeout(
                    SUBMIT_ENTER_DELAY + Duration::from_millis(100),
                    self.rx.recv()
                )
                .await
                .is_err(),
                "rejected submit wrote or scheduled terminal input"
            );
        }
    }

    fn receipt(raw: &str) -> SubmitReceipt {
        let response: SuccessResponse = serde_json::from_str(raw).expect(raw);
        match response.result {
            ResponseResult::AgentSubmitReceipt { receipt } => receipt,
            other => panic!("unexpected result {other:?}"),
        }
    }

    fn error_code(raw: &str) -> String {
        let response: ErrorResponse = serde_json::from_str(raw).expect(raw);
        response.error.code
    }

    #[tokio::test]
    async fn submit_rejections_write_nothing_and_record_nothing() {
        let mut f = fixture();
        let good = f.params("ok-1", "hello");

        let mut bad_id = good.clone();
        bad_id.submit_id = "bad id".into();
        let mut long_id = good.clone();
        long_id.submit_id = "x".repeat(65);
        let mut empty = good.clone();
        empty.text = String::new();
        let mut huge = good.clone();
        huge.text = "x".repeat(8193);
        let mut stale = good.clone();
        stale.target_ref.generation += 1;
        let mut wrong_epoch = good.clone();
        wrong_epoch.target_ref.server_epoch = "0".repeat(32);
        for (params, code) in [
            (bad_id, "invalid_request"),
            (long_id, "invalid_request"),
            (empty, "invalid_request"),
            (huge, "invalid_request"),
            (stale, "stale_target"),
            (wrong_epoch, "stale_target"),
        ] {
            let raw = f.submit(params);
            assert_eq!(error_code(&raw), code, "{raw}");
        }
        let mut stale = good.clone();
        stale.target_ref.generation = 7;
        let raw = f.submit(stale);
        assert!(raw.contains("current_generation=1"), "{raw}");

        crate::supervisor::set_enabled_for_test(Some(false));
        assert_eq!(error_code(&f.submit(good.clone())), "supervisor_disabled");
        assert_eq!(error_code(&f.status("ok-1")), "supervisor_disabled");
        crate::supervisor::set_enabled_for_test(Some(true));

        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn submit_rejects_agent_state_and_draft_without_writing() {
        for (agent, state, row, code) in [
            (Agent::Codex, AgentState::Idle, "❯ ", "unsupported_agent"),
            (Agent::Claude, AgentState::Blocked, "❯ ", "agent_blocked"),
            (
                Agent::Claude,
                AgentState::Idle,
                "❯ half typed",
                "draft_not_clean",
            ),
            (
                Agent::Claude,
                AgentState::Idle,
                "no prompt box",
                "draft_not_clean",
            ),
        ] {
            let mut f = fixture_with(agent, state, row);
            let params = f.params("id-1", "hello");
            let raw = f.submit(params);
            assert_eq!(error_code(&raw), code, "{raw}");
            assert_eq!(f.app.state.supervisor.submits.len(), 0);
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn submit_rejects_owner_input_at_last_output_ms() {
        let mut f = fixture();
        f.app
            .lookup_runtime_sender(0, f.pane_id)
            .unwrap()
            .supervisor_activity()
            .set_stamps_for_test(100, 100);
        let params = f.params("id-1", "hello");
        let raw = f.submit(params);
        assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
        assert!(raw.contains("draft=unknown"), "{raw}");
        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn submit_accepts_once_and_repeats_without_second_write() {
        let mut f = fixture();
        let params = f.params("run-1", "hello");
        let first = receipt(&f.submit(params.clone()));
        assert_eq!(first.state, SubmitState::Accepted);
        assert_eq!(first.text_sha256_16, text_sha256_16("hello"));
        assert_eq!(
            first.acknowledged,
            crate::api::schema::supervisor::SubmitAcknowledged::Unsupported
        );
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"hello"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        let written = receipt(&f.status("run-1"));
        assert_eq!(written.state, SubmitState::Written);
        assert!(written.written_at_ms.is_some());

        let again = receipt(&f.submit(params.clone()));
        assert_eq!(again, written);
        let mut changed = params.clone();
        changed.text = "other".into();
        assert_eq!(error_code(&f.submit(changed)), "submit_id_conflict");
        assert_eq!(f.app.state.supervisor.submits.len(), 1);
        f.assert_no_write().await;
        assert!(
            !f.status("run-1").contains("hello"),
            "receipt never echoes text"
        );
    }

    #[tokio::test]
    async fn submit_write_failure_is_indeterminate() {
        let mut f = fixture();
        f.rx.close();
        let params = f.params("run-err", "hello");
        assert_eq!(receipt(&f.submit(params)).state, SubmitState::Accepted);
        f.pump_submit_event().await;
        let r = receipt(&f.status("run-err"));
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Indeterminate, Some(SubmitReason::WriteFailed))
        );
    }

    #[tokio::test]
    async fn working_edge_after_write_marks_started() {
        let mut f = fixture();
        let params = f.params("run-s", "hello");
        f.submit(params);
        f.pump_submit_event().await;
        assert_eq!(receipt(&f.status("run-s")).state, SubmitState::Written);
        f.app.handle_internal_event(AppEvent::StateChanged {
            pane_id: f.pane_id,
            agent: Some(Agent::Claude),
            state: AgentState::Working,
            visible_blocker: false,
            visible_working: true,
            process_exited: false,
            observed_at: Instant::now(),
        });
        let r = receipt(&f.status("run-s"));
        assert_eq!(r.state, SubmitState::Started);
        assert!(r.started_at_ms.is_some());
    }

    #[tokio::test]
    async fn owner_input_during_hold_fences_with_zero_bytes() {
        let mut f = fixture();
        let params = f.params("held-1", "hello");
        let raw = f.app.handle_agent_submit_with_hold(
            "s".into(),
            params,
            Some(Duration::from_millis(30)),
        );
        assert_eq!(receipt(&raw).state, SubmitState::Accepted);
        f.app
            .lookup_runtime_sender(0, f.pane_id)
            .unwrap()
            .supervisor_activity()
            .trip_submit_fences();
        f.pump_submit_event().await; // hold released; write queued against a tripped fence
        f.pump_submit_event().await; // writer result
        let r = receipt(&f.status("held-1"));
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Fenced, Some(SubmitReason::OwnerInput))
        );
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn generation_change_during_hold_fences_with_zero_bytes() {
        let mut f = fixture();
        let params = f.params("held-2", "hello");
        f.app
            .handle_agent_submit_with_hold("s".into(), params, Some(Duration::from_millis(30)));
        f.app.state.supervisor.bump_generation(
            f.pane_id,
            crate::api::schema::supervisor::GenerationReason::PaneMoved,
            1,
        );
        f.pump_submit_event().await;
        f.pump_submit_event().await;
        let r = receipt(&f.status("held-2"));
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Fenced, Some(SubmitReason::GenerationChanged))
        );
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn ledger_capacity_refuses_new_ids_and_keeps_known_receipts() {
        let mut f = fixture();
        f.app.state.supervisor.submits.set_cap_for_test(1);
        let first = f.params("cap-1", "hello");
        f.submit(first.clone());
        f.pump_submit_event().await;
        while f.rx.try_recv().is_ok() {}
        let second = f.params("cap-2", "hello");
        assert_eq!(error_code(&f.submit(second)), "submit_capacity");
        assert_eq!(receipt(&f.submit(first)).submit_id, "cap-1");
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn submit_status_reports_known_and_unknown_ids() {
        let mut f = fixture();
        assert_eq!(error_code(&f.status("nope")), "submit_not_found");
        let params = f.params("known", "hello");
        f.submit(params);
        let r = receipt(&f.status("known"));
        assert_eq!(r.submit_id, "known");
    }

    // --- KELPIE-U/V: sticky owner-input latch -------------------------------------------

    #[tokio::test]
    async fn latched_owner_input_rejects_despite_later_output_and_time() {
        let mut f = fixture();
        // Owner input at T, unrelated output at T+2. The draft derivation reads no clock, so
        // neither T+300 nor T+10_000 can make the empty box read clean.
        f.owner_input_then_output(1_000, 1_002);
        f.assert_rejected_unknown("lat-300");
        f.activity().set_stamps_for_test(11_000, 1_000);
        f.assert_rejected_unknown("lat-10000");
        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn latch_survives_owner_enter_followed_by_working_edge() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.app.handle_internal_event(AppEvent::StateChanged {
            pane_id: f.pane_id,
            agent: Some(Agent::Claude),
            state: AgentState::Working,
            visible_blocker: false,
            visible_working: true,
            process_exited: false,
            observed_at: Instant::now(),
        });
        f.app.state.supervisor.note_working_report(f.pane_id);
        f.app.handle_internal_event(AppEvent::StateChanged {
            pane_id: f.pane_id,
            agent: Some(Agent::Claude),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: Instant::now(),
        });
        f.assert_rejected_unknown("lat-working");
        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn latch_survives_session_and_agent_generation_bumps_and_pane_move() {
        use crate::api::schema::supervisor::GenerationReason;
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        for (n, reason) in [
            GenerationReason::AgentStarted,
            GenerationReason::SessionChanged,
            GenerationReason::PaneMoved,
        ]
        .into_iter()
        .enumerate()
        {
            f.app
                .state
                .supervisor
                .bump_generation(f.pane_id, reason, 2_000 + n as u64);
            f.assert_rejected_unknown(&format!("lat-bump-{n}"));
        }
        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn fresh_runtime_starts_unlatched_and_reads_the_screen() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.assert_rejected_unknown("lat-old-pty");
        // A new PTY runtime brings a new activity struct: latch false, screen rule applies.
        let (runtime, rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                &screen("❯ "),
                8,
            );
        runtime.supervisor_activity().set_stamps_for_test(100, 0);
        f.app.state.insert_test_runtime(f.pane_id, runtime);
        f.rx = rx;
        assert!(!f.activity().draft_latched());
        let params = f.params("fresh-1", "hello");
        assert_eq!(receipt(&f.submit(params)).state, SubmitState::Accepted);
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"hello"));
    }

    // --- KELPIE-SHARED-HANDOFF: one-time operator handoff ---------------------------------

    #[tokio::test]
    async fn handoff_allows_exactly_one_submit_then_latch_rejects() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.assert_rejected_unknown("ho-before");
        assert!(!f.digest_handoff_armed());
        f.arm_handoff(crate::supervisor::now_ms());
        assert!(f.digest_handoff_armed(), "digest shows the armed handoff");

        let params = f.params("ho-1", "hello");
        let first = receipt(&f.submit(params));
        assert_eq!(first.state, SubmitState::Accepted);
        assert!(first.handoff, "receipt records the consumed handoff");
        assert!(!f.handoff_valid(), "consumed at accept");
        assert!(!f.digest_handoff_armed());
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"hello"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;

        f.assert_rejected_unknown("ho-2");
        assert_eq!(f.app.state.supervisor.submits.len(), 1);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn owner_input_after_arming_cancels_handoff() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.arm_handoff(crate::supervisor::now_ms());
        assert!(f.handoff_valid());
        f.activity().record_owner_input();
        f.activity().set_stamps_for_test(1_010, 1_005);
        assert!(!f.handoff_valid(), "owner input cancels the handoff");
        f.assert_rejected_unknown("ho-cancel");
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn generation_bump_cancels_handoff() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.arm_handoff(crate::supervisor::now_ms());
        f.app.state.supervisor.bump_generation(
            f.pane_id,
            crate::api::schema::supervisor::GenerationReason::PaneMoved,
            2_000,
        );
        assert!(!f.handoff_valid());
        f.assert_rejected_unknown("ho-gen");
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn expired_handoff_is_cancelled() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        let now = crate::supervisor::now_ms();
        f.arm_handoff(now - crate::supervisor::activity::HANDOFF_TTL_MS - 1);
        assert!(!f.handoff_valid());
        f.assert_rejected_unknown("ho-expired");
        f.assert_no_write().await;
        // Pure validity window with an injected clock.
        let h = crate::supervisor::activity::Handoff {
            armed_at_ms: 10,
            armed_seq: 1,
            generation: 1,
        };
        assert!(h.is_valid(1, 10 + crate::supervisor::activity::HANDOFF_TTL_MS - 1));
        assert!(!h.is_valid(1, 10 + crate::supervisor::activity::HANDOFF_TTL_MS));
        assert!(!h.is_valid(2, 10));
    }

    #[tokio::test]
    async fn handoff_with_typed_or_missing_box_rejects_and_keeps_handoff() {
        for (row, draft) in [("❯ owner typed", "dirty"), ("no prompt box", "unknown")] {
            let mut f = fixture_with(Agent::Claude, AgentState::Idle, row);
            f.owner_input_then_output(1_000, 1_002);
            f.arm_handoff(crate::supervisor::now_ms());
            let params = f.params("ho-screen", "hello");
            let raw = f.submit(params);
            assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
            assert!(raw.contains(&format!("draft={draft}")), "{raw}");
            assert!(f.handoff_valid(), "a rejected submit keeps the handoff");
            assert_eq!(f.app.state.supervisor.submits.len(), 0);
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn handoff_keeps_the_equal_ms_rule() {
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_000);
        f.arm_handoff(crate::supervisor::now_ms());
        f.assert_rejected_unknown("ho-equal");
        assert!(f.handoff_valid());
        f.assert_no_write().await;
    }

    #[test]
    fn no_api_method_arms_a_handoff() {
        let control = serde_json::json!({
            "id": "x", "method": "agent.submit_status", "params": {"submit_id": "a"}
        });
        assert!(serde_json::from_value::<Request>(control).is_ok());
        for method in [
            "supervisor.handoff",
            "agent.handoff",
            "agent.supervisor_handoff",
            "agents.handoff",
        ] {
            let raw = serde_json::json!({"id": "x", "method": method, "params": {}});
            assert!(
                serde_json::from_value::<Request>(raw).is_err(),
                "{method} must not be an API method"
            );
        }
    }
}

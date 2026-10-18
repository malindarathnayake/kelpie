//! Fork-only `agent.submit`, `agent.answer`, `agent.interrupt`, `agent.reset`,
//! `agent.resume` and `agent.submit_status` handlers (S2/S3, S7-S10). Disabled unless
//! `[supervisor] enabled`. Every rejection happens before any PTY call and records nothing.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::api::schema::supervisor::{
    AgentAnswerParams, AgentControlParams, AgentResumeParams, AgentSubmitParams,
    AgentSubmitStatusParams, DraftState, PaneReportPromptSubmittedParams, SubmitKind,
    SubmitReceipt, TargetRef,
};
use crate::api::schema::ResponseResult;
use crate::app::App;
use crate::events::AppEvent;
use crate::supervisor::draft::{derive_draft, draft_box_text};
use crate::supervisor::fence::SubmitFence;
use crate::supervisor::profile::{profile_for, ProfileKey, ResetEvidenceMode};
use crate::supervisor::question::WINDOW_LINES;
use crate::supervisor::registry::AnswerReject;
use crate::supervisor::submit::{
    prompt_ack_sha256_16, text_sha256_16, valid_prompt_sha256_16, valid_resume_session_id,
    valid_submit_id, valid_submit_text, NewReceipt, Repeat, RequestKey, SubmitEvent,
    CLEAR_DRAFT_HASH_TEXT, INTERRUPT_HASH_TEXT, MAX_RESUME_SESSION_ID_CHARS, RESET_TEXT,
    SUBMIT_DEADLINE,
};

use super::agents::agent_not_found;
use super::responses::{encode_error, encode_success};
use super::supervisor::RequestLog;

/// Upper bound on an `agent.answer` question_id (ids are short hashes; this only bounds
/// garbage input).
const MAX_QUESTION_ID_BYTES: usize = 128;

/// A profile key as the pane's terminal encodes it (the bytes an owner pressing that key
/// sends).
fn encode_profile_key(runtime: &crate::terminal::TerminalRuntime, key: ProfileKey) -> Vec<u8> {
    let event = match key {
        ProfileKey::Esc => crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
        ProfileKey::CtrlU => crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        ),
    };
    runtime.encode_terminal_key(event.into())
}

/// Debug builds only: delays a submission write so a fixture can trip the fence while it
/// is queued. Never compiled into release builds.
#[cfg(debug_assertions)]
pub(crate) const DEV_SUBMIT_HOLD_ENV: &str = "KELPIE_DEV_SUBMIT_HOLD_MS";
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

/// Agent name for an `agent.resume` launch: `resume-` and 16 hex chars of SHA-256 over the
/// submit id (a valid `agent.start` name that never echoes the id).
fn resume_agent_name(submit_id: &str) -> String {
    format!("resume-{}", text_sha256_16(submit_id))
}

fn receipt_response(id: String, receipt: SubmitReceipt) -> String {
    encode_success(id, ResponseResult::AgentSubmitReceipt { receipt })
}

/// One supervised write request (`agent.submit`, `agent.answer`, `agent.interrupt`,
/// `agent.reset`). All kinds share validation, ledger, fence, latch and handoff rules.
struct WriteRequest {
    /// Bounded method name for logs and messages.
    method: &'static str,
    kind: SubmitKind,
    target_ref: TargetRef,
    submit_id: String,
    /// `answer` only.
    question_id: Option<String>,
    /// Text sent through the submit path (text, the profile's Enter delay, Enter); None
    /// sends the profile's interrupt or clear key alone as one fenced write with no Enter
    /// (`interrupt` / `clear_draft`). For `reset` the bytes written come from the profile's reset text.
    text: Option<String>,
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
        self.handle_supervised_write(
            id,
            WriteRequest {
                method: "agent.submit",
                kind: SubmitKind::Submit,
                target_ref: params.target_ref,
                submit_id: params.submit_id,
                question_id: None,
                text: Some(params.text),
            },
            hold,
        )
    }

    /// S7: `agent.submit` that answers the newest open prose question still on screen.
    pub(super) fn handle_agent_answer(&mut self, id: String, params: AgentAnswerParams) -> String {
        let hold = dev_submit_hold();
        self.handle_supervised_write(
            id,
            WriteRequest {
                method: "agent.answer",
                kind: SubmitKind::Answer,
                target_ref: params.target_ref,
                submit_id: params.submit_id,
                question_id: Some(params.question_id),
                text: Some(params.text),
            },
            hold,
        )
    }

    /// S8: one fenced interrupt key (from the agent profile) to a Working agent pane.
    pub(super) fn handle_agent_interrupt(
        &mut self,
        id: String,
        params: AgentControlParams,
    ) -> String {
        let hold = dev_submit_hold();
        self.handle_agent_interrupt_with_hold(id, params, hold)
    }

    fn handle_agent_interrupt_with_hold(
        &mut self,
        id: String,
        params: AgentControlParams,
        hold: Option<Duration>,
    ) -> String {
        self.handle_supervised_write(
            id,
            WriteRequest {
                method: "agent.interrupt",
                kind: SubmitKind::Interrupt,
                target_ref: params.target_ref,
                submit_id: params.submit_id,
                question_id: None,
                text: None,
            },
            hold,
        )
    }

    /// Clears only a complete restored draft proved by adjacent submit/interrupt receipts.
    pub(super) fn handle_agent_clear_draft(
        &mut self,
        id: String,
        params: AgentControlParams,
    ) -> String {
        self.handle_agent_clear_draft_with_hold(id, params, dev_submit_hold())
    }

    fn handle_agent_clear_draft_with_hold(
        &mut self,
        id: String,
        params: AgentControlParams,
        hold: Option<Duration>,
    ) -> String {
        self.handle_supervised_write(
            id,
            WriteRequest {
                method: "agent.clear_draft",
                kind: SubmitKind::ClearDraft,
                target_ref: params.target_ref,
                submit_id: params.submit_id,
                question_id: None,
                text: None,
            },
            hold,
        )
    }

    /// S9: submits the agent profile's reset text (`/clear`) through the submit path.
    pub(super) fn handle_agent_reset(&mut self, id: String, params: AgentControlParams) -> String {
        let hold = dev_submit_hold();
        self.handle_agent_reset_with_hold(id, params, hold)
    }

    fn handle_agent_reset_with_hold(
        &mut self,
        id: String,
        params: AgentControlParams,
        hold: Option<Duration>,
    ) -> String {
        self.handle_supervised_write(
            id,
            WriteRequest {
                method: "agent.reset",
                kind: SubmitKind::Reset,
                target_ref: params.target_ref,
                submit_id: params.submit_id,
                question_id: None,
                text: Some(RESET_TEXT.to_string()),
            },
            hold,
        )
    }

    /// S10: launches the agent's resume command for `session_id` (default agent `claude`)
    /// in a pane hosting no agent, through the `agent.start` path. Receipt: `launched` or `failed`; resume evidence from the first
    /// session report after the launch.
    pub(super) fn handle_agent_resume(&mut self, id: String, params: AgentResumeParams) -> String {
        let log = RequestLog::new("agent.resume", &id);
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
        if !valid_resume_session_id(&params.session_id) {
            return reject(
                id,
                "invalid_request",
                format!(
                    "session_id must be 1-{MAX_RESUME_SESSION_ID_CHARS} characters of [A-Za-z0-9_-] and must not start with '-'"
                ),
            );
        }
        let requested_agent = params.agent.as_deref().unwrap_or("claude");
        let Some(resume_agent) = crate::detect::parse_agent_label(requested_agent)
            .filter(|agent| profile_for(*agent).is_some())
        else {
            return reject(
                id,
                "unsupported_agent",
                format!("agent.resume has no profile for {requested_agent}"),
            );
        };
        let session_hash = text_sha256_16(&params.session_id);
        let request_target = TargetRef {
            server_epoch: crate::supervisor::server_epoch().to_string(),
            pane_id: params.pane_id.clone(),
            generation: 0,
        };
        match self.state.supervisor.submits.repeat(
            &params.submit_id,
            RequestKey {
                kind: SubmitKind::Resume,
                question_id: None,
                target_ref: &request_target,
                text_sha256_16: &session_hash,
                session_id: Some(&params.session_id),
                resume_agent: Some(resume_agent),
            },
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
                    "submit_id was already used with a different method, pane, session_id or agent"
                        .into(),
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
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return reject(
                id,
                "agent_pane_not_found",
                format!("pane {} not found", params.pane_id),
            );
        };
        let Some(terminal) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
        else {
            return reject(
                id,
                "agent_pane_not_found",
                format!("pane {} not found", params.pane_id),
            );
        };
        if terminal.is_agent_terminal()
            || terminal.managed_agent_kind().is_some()
            || terminal.managed_agent_launch_pending()
        {
            return reject(
                id,
                "agent_running",
                "pane already hosts an agent or a pending agent launch".into(),
            );
        }
        let session_ref = crate::agent_resume::AgentSessionRef {
            kind: crate::agent_resume::AgentSessionRefKind::Id,
            value: params.session_id.clone(),
        };
        let executable = crate::detect::interactive_agent_executable(resume_agent);
        let resume_label = crate::detect::agent_label(resume_agent);
        let Some(args) = crate::agent_resume::plan(
            &format!("kelpie:{resume_label}"),
            resume_label,
            &session_ref,
        )
        .and_then(|plan| match plan.argv.split_first() {
            Some((program, args)) if program == executable => Some(args.to_vec()),
            _ => None,
        }) else {
            return reject(
                id,
                "unsupported_agent",
                "agent.resume has no resume plan for this session".into(),
            );
        };
        let name = resume_agent_name(&params.submit_id);
        if !self.agent_name_conflicts(&name, "").is_empty() {
            return reject(
                id,
                "agent_name_taken",
                "the launch name derived from submit_id is already used".into(),
            );
        }
        // Owner input in this PTY may have left text on the shell line, which the start
        // command would extend: the same sticky latch and one-time handoff rule as the
        // other write verbs. The line is never cleared.
        let Some(activity) = self
            .lookup_runtime_sender(ws_idx, pane_id)
            .map(|runtime| std::sync::Arc::clone(runtime.supervisor_activity()))
        else {
            return reject(
                id,
                "agent_pane_unavailable",
                format!("pane {} has no live terminal", params.pane_id),
            );
        };
        let target_ref = self.state.supervisor.target_ref(pane_id, &params.pane_id);
        let now = crate::supervisor::now_ms();
        let operator_handoff = activity
            .draft_latched()
            .then(|| activity.valid_handoff(target_ref.generation, now))
            .flatten();
        if activity.draft_latched() && operator_handoff.is_none() {
            return reject(
                id,
                "draft_not_clean",
                "owner input was typed in this pane; draft=unknown".into(),
            );
        }
        // Consume the handoff before anything is recorded or sent. Owner input cancels it
        // on this same thread, so a failed consume means owner input won.
        if let Some(handoff) = operator_handoff {
            if !activity.consume_handoff(handoff) {
                return reject(
                    id,
                    "draft_not_clean",
                    "owner input was typed in this pane; draft=unknown".into(),
                );
            }
        }

        // Accept, then launch through the agent.start path.
        let Some(receipt) = self.state.supervisor.submits.insert_accepted(
            &params.submit_id,
            NewReceipt {
                kind: SubmitKind::Resume,
                lazy_reset: false,
                question_id: None,
                pane: pane_id,
                target_ref,
                text_sha256_16: session_hash,
                ack_sha256_16: None,
                own_text_sha256_16: None,
                handoff: operator_handoff.is_some(),
                session_id: Some(params.session_id.clone()),
                resume_agent: Some(resume_agent),
            },
            SubmitFence::new(),
            now,
        ) else {
            return reject(
                id,
                "submit_capacity",
                "submit receipt ledger is full for this server process".into(),
            );
        };
        let started = self.start_agent(crate::api::schema::AgentStartParams {
            name,
            kind: resume_label.to_string(),
            pane_id: params.pane_id.clone(),
            args,
            timeout_ms: None,
        });
        let receipt = match started {
            Ok(_) => self
                .state
                .supervisor
                .mark_resume_launched(&params.submit_id, crate::supervisor::now_ms()),
            Err(err) => {
                let sent = err.command_sent();
                let code = self.agent_start_error_body(err).code;
                if sent {
                    self.state
                        .supervisor
                        .mark_resume_unconfirmed(&params.submit_id, &code)
                } else {
                    self.state
                        .supervisor
                        .submits
                        .mark_start_failed(&params.submit_id, &code)
                }
            }
        }
        .unwrap_or(receipt);
        let response = receipt_response(id, receipt);
        log.ok(1, &response);
        response
    }

    fn handle_supervised_write(
        &mut self,
        id: String,
        req: WriteRequest,
        hold: Option<Duration>,
    ) -> String {
        let log = RequestLog::new(req.method, &id);
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
        if !valid_submit_id(&req.submit_id) {
            return reject(
                id,
                "invalid_request",
                "submit_id must be 1-64 characters of [A-Za-z0-9._:-]".into(),
            );
        }
        if req
            .text
            .as_deref()
            .is_some_and(|text| !valid_submit_text(text))
        {
            return reject(id, "invalid_request", "text must be 1-8192 bytes".into());
        }
        if req
            .question_id
            .as_deref()
            .is_some_and(|question| question.is_empty() || question.len() > MAX_QUESTION_ID_BYTES)
        {
            return reject(
                id,
                "invalid_request",
                format!("question_id must be 1-{MAX_QUESTION_ID_BYTES} bytes"),
            );
        }
        let key_hash_text = if req.kind == SubmitKind::ClearDraft {
            CLEAR_DRAFT_HASH_TEXT
        } else {
            INTERRUPT_HASH_TEXT
        };
        let text_hash = text_sha256_16(req.text.as_deref().unwrap_or(key_hash_text));
        match self.state.supervisor.submits.repeat(
            &req.submit_id,
            RequestKey {
                kind: req.kind,
                question_id: req.question_id.as_deref(),
                target_ref: &req.target_ref,
                text_sha256_16: &text_hash,
                session_id: None,
                resume_agent: None,
            },
        ) {
            Repeat::Same(receipt) => {
                let response = receipt_response(id, *receipt);
                log.ok(1, &response);
                return response;
            }
            Repeat::Conflict => {
                let message = if req.kind == SubmitKind::Submit {
                    "submit_id was already used with a different target_ref or text"
                } else {
                    "submit_id was already used with a different method, target_ref, question or text"
                };
                return reject(id, "submit_id_conflict", message.into());
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
        let parsed = self.parse_current_public_pane_id(&req.target_ref.pane_id);
        if let Err(stale) = self
            .state
            .supervisor
            .check_target_ref(&req.target_ref, parsed.map(|(_, pane)| pane))
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
        let validated = match self.validate_agent_target(id, &req.target_ref.pane_id, false) {
            Ok(validated) => validated,
            Err(response) => {
                log.error("agent_target_rejected", &response);
                return response;
            }
        };
        let id = validated.id;
        if validated.ws_idx != ws_idx || validated.pane_id != pane_id {
            let response = agent_not_found(id, &req.target_ref.pane_id);
            log.error("agent_not_found", &response);
            return response;
        }
        let agent_label = crate::detect::agent_label(validated.expected_agent);
        let Some(profile) = profile_for(validated.expected_agent) else {
            return reject(
                id,
                "unsupported_agent",
                format!("{} has no profile for {agent_label}", req.method),
            );
        };
        if req.kind == SubmitKind::Answer && !profile.supports_answer {
            return reject(
                id,
                "unsupported_agent",
                format!("{} is not supported for {agent_label}", req.method),
            );
        }
        let control_key = if req.kind == SubmitKind::ClearDraft {
            let Some(key) = profile.clear_key.filter(|_| profile.restores_on_interrupt) else {
                return reject(
                    id,
                    "unsupported_agent",
                    format!("{} is not supported for {agent_label}", req.method),
                );
            };
            key
        } else {
            profile.interrupt
        };
        // Bytes written through the submit path: the profile's reset text for `reset`,
        // otherwise the request text. The idempotency hash above stays on the request text.
        let write_text: Option<&str> = if req.kind == SubmitKind::Reset {
            let Some(reset_text) = profile.reset_text else {
                return reject(
                    id,
                    "unsupported_agent",
                    format!("{} has no reset text for {agent_label}", req.method),
                );
            };
            Some(reset_text)
        } else {
            req.text.as_deref()
        };
        if let Some(question_id) = req.question_id.as_deref() {
            // Structured approvals and a Blocked agent are answered by the owner in the pane.
            if validated.blocked {
                return reject(
                    id,
                    "question_not_answerable",
                    "agent is blocked on a structured prompt; the owner answers it in the pane"
                        .into(),
                );
            }
            // Fresh scan of the current rows: the question must still be on screen now.
            let screen = self
                .state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
                .map(|runtime| runtime.recent_unwrapped_text_snapshot(24).text)
                .unwrap_or_default();
            match self
                .state
                .supervisor
                .check_answer(pane_id, question_id, &screen)
            {
                Ok(()) => {}
                Err(AnswerReject::NotAnswerable) => {
                    return reject(
                        id,
                        "question_not_answerable",
                        "question is a structured prompt; the owner answers it in the pane".into(),
                    );
                }
                Err(AnswerReject::NotCurrent) => {
                    return reject(
                        id,
                        "question_not_current",
                        "question is not the newest open prose question still shown on screen"
                            .into(),
                    );
                }
            }
        }
        if req.kind == SubmitKind::Interrupt && !validated.working {
            return reject(
                id,
                "agent_not_working",
                "agent is not working; interrupt needs a working foreground".into(),
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
            let response = agent_not_found(id, &req.target_ref.pane_id);
            log.error("agent_not_found", &response);
            return response;
        };
        let activity = std::sync::Arc::clone(runtime.supervisor_activity());
        let now = crate::supervisor::now_ms();
        // A valid one-shot operator handoff lifts the sticky owner-input latch for this one
        // write; the screen rule (and the equal-ms rule) still decide the draft. Esc can
        // discard a typed draft, so `interrupt` applies the same rule.
        // An owner-granted delegation lifts the latch for every write while it lasts, without
        // consuming anything; the profile still reads the box, so owner text there rejects.
        let delegated = self
            .state
            .supervisor
            .delegations
            .active(pane_id, now)
            .is_some();
        let operator_handoff = if delegated {
            None
        } else {
            activity.valid_handoff(req.target_ref.generation, now)
        };
        let latched = activity.draft_latched() && operator_handoff.is_none() && !delegated;
        if req.kind == SubmitKind::ClearDraft {
            let last_input = activity.last_owner_input_ms();
            let last_output = activity.last_output_ms();
            if latched
                || last_output.is_none()
                || last_input.is_some_and(|input| last_output.is_none_or(|output| input >= output))
            {
                return reject(
                    id,
                    "draft_not_clean",
                    "owner input draft is not clean; draft=unknown".into(),
                );
            }
            // One bounded snapshot supplies both classification and the transient ownership
            // text. Do not re-read a potentially different screen between the two checks.
            let ansi = self
                .state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
                .and_then(|runtime| runtime.screen_bottom_styled_ansi(WINDOW_LINES));
            let draft = derive_draft(
                ansi.as_deref(),
                Some(profile),
                latched,
                last_input,
                last_output,
            );
            if draft == DraftState::Clean {
                return reject(
                    id,
                    "draft_already_clean",
                    "agent input draft is already clean".into(),
                );
            }
            let own_hash = self
                .state
                .supervisor
                .submits
                .own_restored_text_hash(pane_id, req.target_ref.generation);
            let visible_hash = ansi
                .as_deref()
                .and_then(|ansi| draft_box_text(ansi, profile))
                .map(|text| text_sha256_16(&text));
            if draft != DraftState::Dirty || own_hash.is_none() || own_hash != visible_hash {
                return reject(
                    id,
                    "draft_not_own",
                    "agent input draft is not a proven restored submit".into(),
                );
            }
        } else {
            let draft = self.pane_draft_state(
                ws_idx,
                pane_id,
                Some(agent_label),
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
        }

        // Accept. The waiter starts first so a refused thread fails while nothing is
        // recorded or sent.
        let fence = SubmitFence::new();
        let handoff = match self.spawn_submit_waiter(req.submit_id.clone(), fence.clone()) {
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
            &req.submit_id,
            NewReceipt {
                kind: req.kind,
                lazy_reset: req.kind == SubmitKind::Reset
                    && profile.reset_evidence == ResetEvidenceMode::OnNextPrompt,
                question_id: req.question_id.clone(),
                pane: pane_id,
                target_ref: req.target_ref.clone(),
                text_sha256_16: text_hash,
                own_text_sha256_16: match req.kind {
                    SubmitKind::Submit | SubmitKind::Answer => write_text.map(|text| {
                        text_sha256_16(&text.split_whitespace().collect::<Vec<_>>().join(" "))
                    }),
                    _ => None,
                },
                ack_sha256_16: match req.kind {
                    SubmitKind::Submit | SubmitKind::Answer | SubmitKind::Reset => {
                        write_text.map(prompt_ack_sha256_16)
                    }
                    SubmitKind::Interrupt
                    | SubmitKind::Resume
                    | SubmitKind::ClearDraft
                    | SubmitKind::Unknown => None,
                },
                handoff: operator_handoff.is_some(),
                session_id: None,
                resume_agent: None,
            },
            fence.clone(),
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
        let encoded = self
            .lookup_runtime_sender(ws_idx, pane_id)
            .map(|runtime| match write_text {
                Some(text) => {
                    let (text, enter) =
                        crate::app::api_helpers::encode_api_submission_parts(runtime, text);
                    (
                        text,
                        enter,
                        Duration::from_millis(profile.submit_enter_delay_ms),
                    )
                }
                None => (
                    encode_profile_key(runtime, control_key),
                    Vec::new(),
                    Duration::ZERO,
                ),
            });
        let Some((text, enter, delay)) = encoded else {
            // Unreachable in practice: the runtime was resolved above on this thread.
            let receipt = self
                .state
                .supervisor
                .submits
                .mark_queue_failed(&req.submit_id)
                .unwrap_or(receipt);
            let response = receipt_response(id, receipt);
            log.ok(1, &response);
            return response;
        };
        let deadline = Instant::now() + SUBMIT_DEADLINE + hold.unwrap_or_default();

        #[cfg(debug_assertions)]
        if let Some(hold) = hold {
            self.state.supervisor.submits.hold(
                &req.submit_id,
                crate::supervisor::submit::HeldWrite {
                    text: Bytes::from(text),
                    enter: Bytes::from(enter),
                    delay,
                    deadline,
                },
            );
            self.state
                .supervisor
                .submits
                .attach_handoff(&req.submit_id, handoff);
            let event_tx = self.event_tx.clone();
            let submit_id = req.submit_id.clone();
            let spawned = crate::thread_spawn::spawn_named("kelpie-submit-hold", move || {
                std::thread::sleep(hold);
                let _ = event_tx.blocking_send(AppEvent::SupervisorSubmit(SubmitEvent::Release {
                    submit_id,
                }));
            });
            if spawned.is_err() {
                self.release_held_submit(&req.submit_id);
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
                delay,
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
                .mark_queue_failed(&req.submit_id)
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

    /// Fork (B1): an agent's prompt-submit hook reported the hash of the prompt it received
    /// in a pane. Confirms at most one matching `written` receipt of that pane (see
    /// `SubmitLedger::note_prompt_submitted`); never records or logs prompt text. A report
    /// with supervisor mode disabled, from another agent, or matching nothing is a no-op.
    pub(super) fn handle_pane_report_prompt_submitted(
        &mut self,
        id: String,
        params: PaneReportPromptSubmittedParams,
    ) -> String {
        let log = RequestLog::new("pane.report_prompt_submitted", &id);
        if !valid_prompt_sha256_16(&params.prompt_sha256_16) {
            let response = encode_error(
                id,
                "invalid_request",
                "prompt_sha256_16 must be 16 lowercase hex characters",
            );
            log.error("invalid_request", &response);
            return response;
        }
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            let response = encode_error(
                id,
                "pane_not_found",
                format!("pane {} not found", params.pane_id),
            );
            log.error("pane_not_found", &response);
            return response;
        };
        let matching_profile_agent = crate::supervisor::enabled()
            && crate::detect::parse_agent_label(&params.agent).is_some_and(|agent| {
                profile_for(agent).is_some_and(|profile| profile.prompt_ack)
                    && self.state.workspaces[ws_idx]
                        .pane_state(pane_id)
                        .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
                        .and_then(|terminal| terminal.effective_known_agent())
                        == Some(agent)
            });
        if matching_profile_agent {
            let confirmed = self.state.supervisor.note_prompt_submitted(
                pane_id,
                &params.prompt_sha256_16,
                crate::supervisor::now_ms(),
            );
            if confirmed.is_some() {
                tracing::debug!(
                    event = "supervisor.prompt_ack",
                    subsystem = "supervisor",
                    outcome = "confirmed",
                    trace_id = %log.trace_id,
                    "prompt-submit hook confirmed a receipt"
                );
            }
        }
        let response = encode_success(id, ResponseResult::Ok {});
        log.ok(0, &response);
        response
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
        crate::thread_spawn::spawn_named("kelpie-agent-submit", move || {
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
                held.delay,
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
    use crate::api::schema::supervisor::{
        ResetEvidence, ResumeEvidence, SubmitReason, SubmitState, TargetRef,
    };
    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};
    use crate::detect::{Agent, AgentState};
    use crate::layout::PaneId;

    const RULE: &str = "────────────────────────────────────────";
    /// Paste-to-Enter delay of every built-in profile (claude, codex, cursor).
    const TEST_ENTER_DELAY: Duration = Duration::from_millis(300);

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

    /// Codex idle composer: `›` with the faint placeholder, then a footer row.
    fn codex_screen() -> Vec<u8> {
        "Done.\r\n\r\n\u{203a} \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n  100% context left"
            .as_bytes()
            .to_vec()
    }

    /// Cursor idle composer: a three-row band on its background with `→` and the faint
    /// placeholder on the middle row, then a footer row.
    fn cursor_screen() -> Vec<u8> {
        const BG: &str = "\x1b[48;2;21;21;21m";
        format!(
            "Done.\r\n\r\n{BG}{pad}\x1b[0m\r\n{BG} \u{2192} \x1b[2mPlan, search, build anything\x1b[0m{BG}{rest}\x1b[0m\r\n{BG}{pad}\x1b[0m\r\n  / commands · @ files",
            pad = " ".repeat(78),
            rest = " ".repeat(48),
        )
        .into_bytes()
    }

    fn fixture_with(agent: Agent, state: AgentState, input_row: &str) -> Fixture {
        fixture_with_screen(agent, state, &screen(input_row))
    }

    fn fixture_with_screen(agent: Agent, state: AgentState, screen_bytes: &[u8]) -> Fixture {
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
                screen_bytes,
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

        fn digest_foreground(&mut self) -> String {
            let raw = self.app.handle_api_request(Request {
                id: "dg".into(),
                method: Method::AgentsDigest(crate::api::schema::supervisor::AgentsDigestParams {
                    filter: None,
                    page_cursor: None,
                    trace_id: None,
                }),
            });
            let value: serde_json::Value = serde_json::from_str(&raw).expect(&raw);
            value["result"]["agents"][0]["foreground"]["value"]
                .as_str()
                .expect(&raw)
                .to_string()
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
                    TEST_ENTER_DELAY + Duration::from_millis(100),
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
    async fn clear_draft_stub_writes_nothing_and_records_nothing() {
        let mut f = fixture();
        let params = f.control("clear-draft-1");
        let raw = f.app.handle_api_request(Request {
            id: "clear-draft-request".into(),
            method: Method::AgentClearDraft(params),
        });
        let response: ErrorResponse = serde_json::from_str(&raw).expect(&raw);
        assert_eq!(response.id, "clear-draft-request");
        assert_eq!(response.error.code, "unsupported_agent");
        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
    }

    const CLEAR_DRAFT_OWN_TEXT: &str = " first  line\nsecond\u{2003}line ";

    fn clear_draft_screen(first: &str, second: &str) -> Vec<u8> {
        const BG: &str = "\x1b[48;2;21;21;21m";
        format!("Done.\r\n\r\n{BG}          \x1b[0m\r\n{BG} \u{2192} {first}\x1b[0m\r\n{BG}   {second}\x1b[0m\r\n{BG}          \x1b[0m\r\n  Auto").into_bytes()
    }

    impl Fixture {
        fn clear_draft(&mut self, submit_id: &str) -> String {
            let params = self.control(submit_id);
            self.clear_draft_params(params)
        }

        fn clear_draft_params(&mut self, params: AgentControlParams) -> String {
            self.app.handle_api_request(Request {
                id: "clear-draft-request".into(),
                method: Method::AgentClearDraft(params),
            })
        }

        fn clear_draft_state(&mut self, state: AgentState) {
            self.app.handle_internal_event(AppEvent::StateChanged {
                pane_id: self.pane_id,
                agent: Some(Agent::Cursor),
                state,
                visible_blocker: state == AgentState::Blocked,
                visible_working: state == AgentState::Working,
                process_exited: false,
                observed_at: Instant::now(),
            });
        }

        fn clear_draft_key(&self, key: ProfileKey) -> Bytes {
            Bytes::from(encode_profile_key(
                self.app.lookup_runtime_sender(0, self.pane_id).unwrap(),
                key,
            ))
        }
    }

    /// A real submit and interrupt through API dispatch, their writer completions, and the
    /// resulting multiline restored composer. No receipt state is seeded by the fixture.
    async fn clear_draft_ready() -> Fixture {
        let mut f = fixture_with_screen(Agent::Cursor, AgentState::Idle, &cursor_screen());
        let expected = crate::app::api_helpers::encode_api_submission_parts(
            f.app.lookup_runtime_sender(0, f.pane_id).unwrap(),
            CLEAR_DRAFT_OWN_TEXT,
        );
        let params = f.params("own-prompt", CLEAR_DRAFT_OWN_TEXT);
        let accepted = receipt(&f.submit(params));
        assert_eq!(accepted.state, SubmitState::Accepted);
        assert_eq!(
            accepted.text_sha256_16,
            text_sha256_16(CLEAR_DRAFT_OWN_TEXT)
        );
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from(expected.0));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from(expected.1));
        f.pump_submit_event().await;
        assert_eq!(receipt(&f.status("own-prompt")).state, SubmitState::Written);
        f.clear_draft_state(AgentState::Working);
        let expected = f.clear_draft_key(ProfileKey::Esc);
        assert_eq!(
            receipt(&f.interrupt("own-interrupt")).state,
            SubmitState::Accepted
        );
        assert_eq!(f.rx.recv().await.unwrap(), expected);
        f.pump_submit_event().await;
        assert_eq!(
            receipt(&f.status("own-interrupt")).state,
            SubmitState::Written
        );
        f.clear_draft_state(AgentState::Idle);
        f.set_screen(&clear_draft_screen("first line", "second line"));
        f
    }

    #[tokio::test]
    async fn clear_draft_clears_only_own_restored_text() {
        use crate::api::schema::supervisor::SubmitAcknowledged;
        let mut f = clear_draft_ready().await;
        let expected = f.clear_draft_key(ProfileKey::CtrlU);
        assert!(!expected.is_empty());
        assert_ne!(expected, f.clear_draft_key(ProfileKey::Esc));
        let accepted = receipt(&f.clear_draft("clear-own"));
        assert_eq!(
            (accepted.kind, accepted.state),
            (SubmitKind::ClearDraft, SubmitState::Accepted)
        );
        assert_eq!(accepted.text_sha256_16, text_sha256_16("\u{15}"));
        assert_eq!(f.rx.recv().await.unwrap(), expected);
        f.pump_submit_event().await;
        let written = receipt(&f.status("clear-own"));
        assert_eq!(written.state, SubmitState::Written);
        assert_eq!(written.acknowledged, SubmitAcknowledged::Unsupported);
        f.report_prompt_as("cursor", &prompt_ack_sha256_16(CLEAR_DRAFT_HASH_TEXT));
        assert_eq!(receipt(&f.status("clear-own")), written);
        assert_eq!(
            receipt(&f.status("own-prompt")).text_sha256_16,
            text_sha256_16(CLEAR_DRAFT_OWN_TEXT)
        );
        assert_eq!(f.app.state.supervisor.submits.len(), 3);
        f.assert_no_write().await; // one encoded Ctrl+U, no delayed Enter

        // Ownership does not require Idle, an acknowledgement, or an observed turn end.
        let mut f = clear_draft_ready().await;
        f.clear_draft_state(AgentState::Working);
        assert_eq!(
            receipt(&f.clear_draft("working-clear")).state,
            SubmitState::Accepted
        );
        assert_eq!(
            f.rx.recv().await.unwrap(),
            f.clear_draft_key(ProfileKey::CtrlU)
        );
        f.pump_submit_event().await;
        assert_eq!(
            receipt(&f.status("working-clear")).state,
            SubmitState::Written
        );
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn clear_draft_refusals_record_and_write_nothing() {
        for (bytes, expected) in [
            (
                clear_draft_screen("first line", "second lineX"),
                "draft_not_own",
            ),
            (
                clear_draft_screen("first line", "second lin\x1b[7mX\x1b[27m"),
                "draft_not_own",
            ),
            (cursor_screen(), "draft_already_clean"),
            (b"unrecognized".to_vec(), "draft_not_own"),
            (
                "\x1b[48;2;21;21;21m \u{2192} first line second line\x1b[0m\r\nfooter"
                    .as_bytes()
                    .to_vec(),
                "draft_not_own",
            ),
        ] {
            let mut f = clear_draft_ready().await;
            f.set_screen(&bytes);
            assert_eq!(error_code(&f.clear_draft("refused")), expected);
            assert_eq!(f.app.state.supervisor.submits.len(), 2);
            f.assert_no_write().await;
        }
        // Visible matching text alone, with no accepted prompt or interrupt, is not ours.
        let mut f = fixture_with_screen(
            Agent::Cursor,
            AgentState::Idle,
            &clear_draft_screen("first line", "second line"),
        );
        assert_eq!(error_code(&f.clear_draft("unowned")), "draft_not_own");
        assert_eq!(f.app.state.supervisor.submits.len(), 0);
        f.assert_no_write().await;
        // A written submit alone cannot prove that a later draft was restored by Esc.
        let mut f = fixture_with_screen(Agent::Cursor, AgentState::Idle, &cursor_screen());
        let params = f.params("no-interrupt", CLEAR_DRAFT_OWN_TEXT);
        f.submit(params);
        f.rx.recv().await.unwrap();
        f.rx.recv().await.unwrap();
        f.pump_submit_event().await;
        f.set_screen(&clear_draft_screen("first line", "second line"));
        assert_eq!(
            error_code(&f.clear_draft("missing-interrupt")),
            "draft_not_own"
        );
        assert_eq!(f.app.state.supervisor.submits.len(), 1);
        f.assert_no_write().await;
        for (agent, bytes) in [
            (Agent::Claude, screen("\u{276f} text")),
            (Agent::Codex, codex_screen()),
        ] {
            let mut f = fixture_with_screen(agent, AgentState::Idle, &bytes);
            assert_eq!(
                error_code(&f.clear_draft("unsupported")),
                "unsupported_agent"
            );
            assert_eq!(f.app.state.supervisor.submits.len(), 0);
            f.assert_no_write().await;
        }
        let mut f = clear_draft_ready().await;
        assert_eq!(error_code(&f.clear_draft("bad id")), "invalid_request");
        let mut stale = f.control("stale");
        stale.target_ref.generation += 1;
        assert_eq!(error_code(&f.clear_draft_params(stale)), "stale_target");
        let mut stale = f.control("unknown-pane");
        stale.target_ref.pane_id = "w999:p999".into();
        assert_eq!(error_code(&f.clear_draft_params(stale)), "stale_target");
        crate::supervisor::set_enabled_for_test(Some(false));
        assert_eq!(
            error_code(&f.clear_draft("disabled")),
            "supervisor_disabled"
        );
        crate::supervisor::set_enabled_for_test(Some(true));
        f.app.state.supervisor.submits.set_cap_for_test(2);
        assert_eq!(error_code(&f.clear_draft("full")), "submit_capacity");
        f.app.state.supervisor.submits.set_cap_for_test(256);
        f.clear_draft_state(AgentState::Blocked);
        assert_eq!(error_code(&f.clear_draft("blocked")), "agent_blocked");
        assert_eq!(f.app.state.supervisor.submits.len(), 2);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn clear_draft_retries_and_fences() {
        let mut f = clear_draft_ready().await;
        let params = f.control("clear-repeat");
        // Dispatch directly until completion is pumped: API dispatch drains queued events.
        let first = receipt(&f.app.handle_agent_clear_draft_with_hold(
            "first".into(),
            params.clone(),
            None,
        ));
        assert_eq!(first.state, SubmitState::Accepted);
        assert_eq!(
            receipt(
                &f.app
                    .handle_agent_clear_draft("retry".into(), params.clone())
            ),
            first
        );
        let second = f.control("second-id");
        assert_eq!(
            error_code(&f.app.handle_agent_clear_draft("second".into(), second)),
            "draft_not_own"
        );
        assert_eq!(
            f.rx.recv().await.unwrap(),
            f.clear_draft_key(ProfileKey::CtrlU)
        );
        f.pump_submit_event().await;
        let written = receipt(&f.status("clear-repeat"));
        assert_eq!(written.state, SubmitState::Written);
        assert_eq!(error_code(&f.clear_draft("second-id")), "draft_not_own");
        assert_eq!(
            error_code(&f.interrupt("clear-repeat")),
            "submit_id_conflict"
        );
        let mut changed = params.clone();
        changed.target_ref.generation += 1;
        assert_eq!(
            error_code(&f.clear_draft_params(changed)),
            "submit_id_conflict"
        );
        f.set_screen(b"screen changed completely");
        assert_eq!(receipt(&f.clear_draft_params(params)), written);
        assert_eq!(f.app.state.supervisor.submits.len(), 3);
        f.assert_no_write().await;
        for reason in [
            SubmitReason::OwnerInput,
            SubmitReason::GenerationChanged,
            SubmitReason::PaneClosed,
        ] {
            let mut f = clear_draft_ready().await;
            let params = f.control("held-clear");
            let raw = f.app.handle_agent_clear_draft_with_hold(
                "held".into(),
                params,
                Some(Duration::from_millis(30)),
            );
            assert_eq!(receipt(&raw).state, SubmitState::Accepted);
            match reason {
                SubmitReason::OwnerInput => f.activity().before_owner_bytes(),
                SubmitReason::GenerationChanged => {
                    f.app.state.supervisor.bump_generation(
                        f.pane_id,
                        crate::api::schema::supervisor::GenerationReason::PaneMoved,
                        200,
                    );
                }
                SubmitReason::PaneClosed => f.app.state.supervisor.remove_pane(f.pane_id),
                _ => unreachable!(),
            }
            f.pump_submit_event().await;
            f.pump_submit_event().await;
            let r = receipt(&f.status("held-clear"));
            assert_eq!((r.state, r.reason), (SubmitState::Fenced, Some(reason)));
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn clear_draft_latch_and_handoff() {
        // Stamps are set after set_screen, which installs a fresh runtime and activity.
        for (latched, handoff, output, input, mismatch, code) in [
            (true, false, 102, 100, false, "draft_not_clean"),
            (false, false, 100, 100, false, "draft_not_clean"),
            (false, false, 100, 101, false, "draft_not_clean"),
            (false, false, 0, 0, false, "draft_not_clean"),
            (true, true, 102, 100, true, "draft_not_own"),
            (true, true, 100, 100, false, "draft_not_clean"),
            (true, true, 0, 0, false, "draft_not_clean"),
        ] {
            let mut f = clear_draft_ready().await;
            if mismatch {
                f.set_screen(&clear_draft_screen("owner text", "second line"));
            }
            if latched {
                f.activity().record_owner_input();
            }
            f.activity().set_stamps_for_test(output, input);
            if handoff {
                f.arm_handoff(crate::supervisor::now_ms());
            }
            assert_eq!(error_code(&f.clear_draft("protected")), code);
            assert_eq!(f.handoff_valid(), handoff);
            assert_eq!(f.app.state.supervisor.submits.len(), 2);
            f.assert_no_write().await;
        }
        let mut f = clear_draft_ready().await;
        f.owner_input_then_output(100, 102);
        f.arm_handoff(crate::supervisor::now_ms());
        let accepted = receipt(&f.clear_draft("handoff-clear"));
        assert!(accepted.handoff);
        assert!(!f.handoff_valid());
        assert!(f.activity().draft_latched());
        assert_eq!(
            f.rx.recv().await.unwrap(),
            f.clear_draft_key(ProfileKey::CtrlU)
        );
        f.pump_submit_event().await;
        assert_eq!(
            receipt(&f.status("handoff-clear")).state,
            SubmitState::Written
        );
        assert_eq!(
            error_code(&f.clear_draft("handoff-spent")),
            "draft_not_clean"
        );
        assert_eq!(f.app.state.supervisor.submits.len(), 3);
        f.assert_no_write().await;
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
            (Agent::Gemini, AgentState::Idle, "❯ ", "unsupported_agent"),
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
        assert_eq!(first.kind, SubmitKind::Submit);
        assert_eq!(first.question_id, None);
        assert_eq!(first.reset, None);
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

    impl Fixture {
        fn report_prompt(&mut self, hash: &str) -> String {
            self.report_prompt_as("claude", hash)
        }

        fn report_prompt_as(&mut self, agent: &str, hash: &str) -> String {
            let pane_id = self.app.public_pane_id(0, self.pane_id).unwrap();
            self.app.handle_api_request(Request {
                id: "hook".into(),
                method: Method::PaneReportPromptSubmitted(PaneReportPromptSubmittedParams {
                    pane_id,
                    source: format!("kelpie:{}", agent.trim()),
                    agent: agent.into(),
                    prompt_sha256_16: hash.into(),
                }),
            })
        }
    }

    #[tokio::test]
    async fn prompt_submitted_requires_matching_profile_agent() {
        use crate::api::schema::supervisor::SubmitAcknowledged;

        for (agent, bytes, matching_label) in [
            (Agent::Claude, screen("❯ "), " Claude "),
            (Agent::Codex, codex_screen(), " CODEX "),
            (Agent::Cursor, cursor_screen(), " Cursor-Agent "),
        ] {
            let label = crate::detect::agent_label(agent);
            let mut ignored: Vec<_> = ["claude", "codex", "cursor", "unknown-agent", "gemini"]
                .into_iter()
                .filter(|reported| *reported != label)
                .map(|reported| (reported, Some(agent)))
                .collect();
            ignored.extend([("gemini", Some(Agent::Gemini)), (label, None)]);

            for (reported, effective_agent) in ignored {
                let mut f = fixture_with_screen(agent, AgentState::Idle, &bytes);
                let params = f.params("profile-ack", "review this");
                assert_eq!(receipt(&f.submit(params)).state, SubmitState::Accepted);
                assert_eq!(
                    f.rx.recv().await.unwrap(),
                    Bytes::from_static(b"review this")
                );
                assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
                f.pump_submit_event().await;
                let written = receipt(&f.status("profile-ack"));
                assert_eq!(written.state, SubmitState::Written);
                assert_eq!(written.acknowledged, SubmitAcknowledged::Unsupported);
                let terminal_id = f.app.state.workspaces[0].tabs[0].panes[&f.pane_id]
                    .attached_terminal_id
                    .clone();
                f.app
                    .state
                    .terminals
                    .get_mut(&terminal_id)
                    .unwrap()
                    .set_detected_state(effective_agent, AgentState::Idle);

                let hash = prompt_ack_sha256_16("review this\n");
                let ok = f.report_prompt_as(reported, &hash);
                assert!(serde_json::from_str::<SuccessResponse>(&ok).is_ok(), "{ok}");
                assert_eq!(
                    receipt(&f.status("profile-ack")),
                    written,
                    "{agent:?}: {reported} with {effective_agent:?} must be ignored"
                );

                // The ignored matching hash must not retire the legitimate acknowledgement.
                f.app
                    .state
                    .terminals
                    .get_mut(&terminal_id)
                    .unwrap()
                    .set_detected_state(Some(agent), AgentState::Idle);
                let ok = f.report_prompt_as(matching_label, &hash);
                assert!(serde_json::from_str::<SuccessResponse>(&ok).is_ok(), "{ok}");
                let confirmed = receipt(&f.status("profile-ack"));
                assert_eq!(confirmed.state, SubmitState::Written);
                assert_eq!(
                    confirmed.acknowledged,
                    SubmitAcknowledged::Confirmed,
                    "{agent:?}: legitimate report after {reported} with {effective_agent:?}"
                );
                assert!(confirmed.acknowledged_at_ms.is_some());
                f.assert_no_write().await;
            }
        }
    }

    #[tokio::test]
    async fn prompt_submitted_cursor_reset_keeps_generation_after_followup() {
        for retained_authority in [false, true] {
            let mut f = fixture_with_screen(Agent::Cursor, AgentState::Idle, &cursor_screen());
            let session_report =
                |pane_id, value: &str, reason: &str, seq| AppEvent::AgentSessionReported {
                    pane_id,
                    source: "kelpie:cursor".into(),
                    agent_label: "cursor".into(),
                    seq: Some(seq),
                    session_ref: crate::agent_resume::AgentSessionRef::id(value),
                    session_start_source: Some(reason.into()),
                };
            f.app
                .handle_internal_event(session_report(f.pane_id, "cursor-old", "startup", 1));
            let terminal_id = f.app.state.workspaces[0].tabs[0].panes[&f.pane_id]
                .attached_terminal_id
                .clone();
            assert!(f.app.state.terminals[&terminal_id].hook_authority.is_none());
            if retained_authority {
                // Model retained same-owner authority; native Cursor events cannot create it.
                f.app
                    .state
                    .terminals
                    .get_mut(&terminal_id)
                    .unwrap()
                    .set_hook_authority_with_session_ref(
                        "kelpie:cursor".into(),
                        "cursor".into(),
                        AgentState::Idle,
                        None,
                        crate::agent_resume::AgentSessionRef::id("cursor-old"),
                        Some(2),
                    )
                    .expect("retained old same-owner Cursor authority");
            } else {
                f.app.handle_internal_event(AppEvent::HookStateReported {
                    pane_id: f.pane_id,
                    source: "kelpie:cursor".into(),
                    agent_label: "cursor".into(),
                    state: AgentState::Idle,
                    message: None,
                    seq: Some(2),
                    session_ref: crate::agent_resume::AgentSessionRef::id("cursor-old"),
                });
            }
            assert_eq!(
                f.app.state.terminals[&terminal_id].hook_authority.is_some(),
                retained_authority
            );
            assert_eq!(
                f.app.state.terminals[&terminal_id].current_session_identity(),
                Some((
                    "kelpie:cursor".into(),
                    "cursor".into(),
                    crate::agent_resume::AgentSessionRefKind::Id,
                    "cursor-old".into(),
                ))
            );
            assert_eq!(f.target().generation, 1);

            let first = receipt(&f.reset("cursor-reset"));
            assert_eq!(first.state, SubmitState::Accepted);
            assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"/clear"));
            assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
            f.pump_submit_event().await;
            let written = receipt(&f.status("cursor-reset"));
            assert_eq!(written.state, SubmitState::Written);
            assert_eq!(written.reset, Some(ResetEvidence::Pending));
            assert_eq!(f.target().generation, 2, "Written lazy reset bumps once");

            f.app
                .handle_internal_event(session_report(f.pane_id, "cursor-new", "prompt", 3));
            let verified = receipt(&f.status("cursor-reset"));
            assert_eq!(verified.reset, Some(ResetEvidence::Verified));
            assert_eq!(
                verified.new_session.as_ref().map(|s| s.value.as_str()),
                Some("cursor-new")
            );
            assert_eq!(verified.new_generation, Some(2));
            assert_eq!(verified.target_ref.generation, 1);
            assert_eq!(f.target().generation, 2, "verification does not bump again");
            let expected_identity = Some((
                "kelpie:cursor".into(),
                "cursor".into(),
                crate::agent_resume::AgentSessionRefKind::Id,
                "cursor-new".into(),
            ));
            assert!(f.app.state.terminals[&terminal_id].hook_authority.is_none());
            assert_eq!(
                f.app.state.terminals[&terminal_id].current_session_identity(),
                expected_identity
            );

            for followup in [
                AppEvent::StateChanged {
                    pane_id: f.pane_id,
                    agent: Some(Agent::Cursor),
                    state: AgentState::Working,
                    visible_blocker: false,
                    visible_working: true,
                    process_exited: false,
                    observed_at: Instant::now(),
                },
                session_report(f.pane_id, "cursor-new", "prompt", 3),
                session_report(f.pane_id, "cursor-new", "prompt", 4),
            ] {
                f.app.handle_internal_event(followup);
                assert_eq!(f.target().generation, 2);
                assert_eq!(
                    f.app.state.terminals[&terminal_id].current_session_identity(),
                    expected_identity
                );
                assert_eq!(receipt(&f.status("cursor-reset")), verified);
            }
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn prompt_hook_report_confirms_written_receipt_and_ignores_others() {
        let mut f = fixture();
        let params = f.params("run-ack", "review this");
        assert_eq!(receipt(&f.submit(params)).state, SubmitState::Accepted);
        assert_eq!(
            f.rx.recv().await.unwrap(),
            Bytes::from_static(b"review this")
        );
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        assert_eq!(receipt(&f.status("run-ack")).state, SubmitState::Written);

        // Wrong text: a no-op success; the receipt stays unsupported.
        let ok = f.report_prompt(&prompt_ack_sha256_16("something else"));
        assert!(serde_json::from_str::<SuccessResponse>(&ok).is_ok(), "{ok}");
        let r = receipt(&f.status("run-ack"));
        assert_eq!(
            r.acknowledged,
            crate::api::schema::supervisor::SubmitAcknowledged::Unsupported
        );

        // Claude's hook prompt carries a trailing newline: still the same text.
        let ok = f.report_prompt(&prompt_ack_sha256_16("review this\n"));
        assert!(serde_json::from_str::<SuccessResponse>(&ok).is_ok(), "{ok}");
        let raw = f.status("run-ack");
        let r = receipt(&raw);
        assert_eq!(
            r.acknowledged,
            crate::api::schema::supervisor::SubmitAcknowledged::Confirmed
        );
        assert!(r.acknowledged_at_ms.is_some());
        assert_eq!(r.state, SubmitState::Written);
        assert!(!raw.contains("review this"), "receipt never echoes text");

        // Malformed hash and unknown pane are rejected without touching receipts.
        assert_eq!(
            error_code(&f.report_prompt("not-a-hash")),
            "invalid_request"
        );
        let missing = f.app.handle_api_request(Request {
            id: "hook".into(),
            method: Method::PaneReportPromptSubmitted(PaneReportPromptSubmittedParams {
                pane_id: "w999:p1".into(),
                source: "kelpie:claude".into(),
                agent: "claude".into(),
                prompt_sha256_16: prompt_ack_sha256_16("review this"),
            }),
        });
        assert_eq!(error_code(&missing), "pane_not_found");
    }

    #[tokio::test]
    async fn prompt_hook_report_is_a_no_op_with_supervisor_disabled() {
        let mut f = fixture();
        let params = f.params("run-off", "hello");
        assert_eq!(receipt(&f.submit(params)).state, SubmitState::Accepted);
        f.pump_submit_event().await;
        crate::supervisor::set_enabled_for_test(Some(false));
        let ok = f.report_prompt(&prompt_ack_sha256_16("hello"));
        assert!(serde_json::from_str::<SuccessResponse>(&ok).is_ok(), "{ok}");
        crate::supervisor::set_enabled_for_test(Some(true));
        assert_eq!(
            receipt(&f.status("run-off")).acknowledged,
            crate::api::schema::supervisor::SubmitAcknowledged::Unsupported
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
    async fn working_activity_never_moves_a_receipt_past_written() {
        let mut f = fixture();
        let params = f.params("run-s", "hello");
        assert_eq!(
            receipt(&f.submit(params.clone())).state,
            SubmitState::Accepted
        );
        // The writer finished (both chunks reached the PTY queue) but the App has not yet
        // handled the completion: a Working edge and a Working hook report land first.
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"hello"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.state_changed(AgentState::Working);
        f.app.state.supervisor.note_working_report(f.pane_id);
        f.pump_submit_event().await;
        let written = receipt(&f.status("run-s"));
        assert_eq!(written.state, SubmitState::Written);
        assert!(written.written_at_ms.is_some());
        // Activity after the completion: still `written`.
        f.state_changed(AgentState::Idle);
        f.state_changed(AgentState::Working);
        f.app.state.supervisor.note_working_report(f.pane_id);
        let raw = f.status("run-s");
        assert_eq!(receipt(&raw), written);
        assert!(!raw.contains("started"), "{raw}");
        assert_eq!(
            written.acknowledged,
            crate::api::schema::supervisor::SubmitAcknowledged::Unsupported
        );
        // The digest reports the activity independently of the receipt.
        assert_eq!(f.digest_foreground(), "working");
        // Idempotent retry: same receipt, no second write.
        assert_eq!(receipt(&f.submit(params)), written);
        f.assert_no_write().await;
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

    // --- Sticky owner-input latch ---------------------------------------------------------

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

    // --- One-time operator handoff ----------------------------------------------------------

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

    // --- Owner-granted delegation ------------------------------------------------------------

    impl Fixture {
        fn delegate(&mut self, choice: crate::supervisor::delegation::DelegationChoice, now: u64) {
            let public = self.app.public_pane_id(0, self.pane_id).unwrap();
            self.app
                .apply_supervisor_delegation(0, self.pane_id, &public, choice, now)
                .expect("delegation applied");
        }

        fn undelegate(&mut self) -> String {
            let public = self.app.public_pane_id(0, self.pane_id).unwrap();
            self.app.handle_api_request(Request {
                id: "ud".into(),
                method: Method::AgentUndelegate(
                    crate::api::schema::supervisor::AgentUndelegateParams { pane_id: public },
                ),
            })
        }

        fn digest_owner_input(&mut self) -> serde_json::Value {
            let raw = self.app.handle_api_request(Request {
                id: "dg".into(),
                method: Method::AgentsDigest(crate::api::schema::supervisor::AgentsDigestParams {
                    filter: None,
                    page_cursor: None,
                    trace_id: None,
                }),
            });
            let value: serde_json::Value = serde_json::from_str(&raw).expect(&raw);
            value["result"]["agents"][0]["owner_input"].clone()
        }

        async fn assert_written(&mut self, submit_id: &str, text: &str) {
            let params = self.params(submit_id, text);
            let raw = self.submit(params);
            let accepted = receipt(&raw);
            assert_eq!(accepted.state, SubmitState::Accepted, "{raw}");
            assert!(!accepted.handoff, "a delegation consumes no handoff");
            assert_eq!(self.rx.recv().await.unwrap(), Bytes::from(text.to_owned()));
            assert_eq!(self.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
            self.pump_submit_event().await;
        }
    }

    #[tokio::test]
    async fn delegation_lets_every_write_through_after_owner_input() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.assert_rejected_unknown("dl-before");
        assert_eq!(f.digest_owner_input()["delegation"]["active"], false);

        let now = crate::supervisor::now_ms();
        f.delegate(DelegationChoice::Minutes30, now);
        let owner = f.digest_owner_input();
        assert_eq!(owner["delegation"]["active"], true, "{owner}");
        assert_eq!(owner["delegation"]["permanent"], false, "{owner}");
        assert_eq!(
            owner["delegation"]["expires_at_ms"],
            now + 30 * 60_000,
            "{owner}"
        );
        assert_eq!(
            owner["draft"], "clean",
            "a delegated pane reports the real box"
        );

        f.assert_written("dl-1", "first").await;
        f.assert_written("dl-2", "second").await;
        assert!(
            f.activity().draft_latched(),
            "the latch itself is untouched; the delegation only lifts it"
        );
    }

    #[tokio::test]
    async fn expired_or_revoked_delegation_restores_the_latch() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        let now = crate::supervisor::now_ms();
        // Granted 31 minutes ago for 30 minutes: already expired.
        f.delegate(DelegationChoice::Minutes30, now - 31 * 60_000);
        f.assert_rejected_unknown("dl-expired");
        assert!(
            f.app.expire_supervisor_delegations(now),
            "the tick drops it"
        );
        assert!(!f.app.expire_supervisor_delegations(now), "only once");

        f.delegate(DelegationChoice::Hours2, now);
        f.assert_written("dl-live", "hello").await;
        let raw = f.undelegate();
        assert!(raw.contains("\"result\""), "{raw}");
        f.assert_rejected_unknown("dl-revoked");
        assert_eq!(f.digest_owner_input()["delegation"]["active"], false);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn delegation_changes_mark_the_session_dirty() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        let now = crate::supervisor::now_ms();
        f.app.state.session_dirty = false;
        f.delegate(DelegationChoice::Permanent, now);
        assert!(f.app.state.session_dirty, "grant");

        f.app.state.session_dirty = false;
        f.delegate(DelegationChoice::Revoke, now);
        assert!(f.app.state.session_dirty, "owner-menu revoke");

        f.delegate(DelegationChoice::Permanent, now);
        f.app.state.session_dirty = false;
        let raw = f.undelegate();
        assert!(raw.contains("\"result\""), "{raw}");
        assert!(f.app.state.session_dirty, "api undelegate");

        f.app.state.session_dirty = false;
        let raw = f.undelegate();
        assert!(raw.contains("\"result\""), "{raw}");
        f.delegate(DelegationChoice::Revoke, now);
        assert!(!f.app.state.session_dirty, "nothing to revoke");
    }

    #[tokio::test]
    async fn undelegated_pane_is_not_in_the_next_session_save() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        f.app.state.supervisor.delegations.grant(
            f.pane_id,
            DelegationChoice::Permanent,
            7,
            Some("s-1".into()),
        );
        let saved = |app: &App| {
            let mut snapshot = crate::persist::capture(
                &app.state.workspaces,
                &app.state.terminals,
                &app.terminal_runtimes,
                app.state.active,
                app.state.selected,
            );
            crate::persist::record_delegations(&mut snapshot, &app.state.supervisor.delegations);
            crate::persist::restored_pane_delegations(&snapshot, &app.state.workspaces)
        };
        assert_eq!(saved(&f.app).len(), 1, "the permanent grant is saved");

        f.undelegate();
        assert!(f.app.state.session_dirty);
        assert!(
            saved(&f.app).is_empty(),
            "the revoked grant is not restored"
        );
    }

    #[tokio::test]
    async fn delegated_pane_still_rejects_owner_text_or_an_unreadable_box() {
        use crate::supervisor::delegation::DelegationChoice;
        for (row, draft) in [("❯ owner typed", "dirty"), ("no prompt box", "unknown")] {
            let mut f = fixture_with(Agent::Claude, AgentState::Idle, row);
            f.owner_input_then_output(1_000, 1_002);
            f.delegate(DelegationChoice::Permanent, crate::supervisor::now_ms());
            let params = f.params("dl-screen", "hello");
            let raw = f.submit(params);
            assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
            assert!(raw.contains(&format!("draft={draft}")), "{raw}");
            f.assert_no_write().await;
        }
        // The equal-ms rule (owner input with no output after it) still holds.
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_000);
        f.delegate(DelegationChoice::Permanent, crate::supervisor::now_ms());
        f.assert_rejected_unknown("dl-equal");
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn owner_keys_during_a_delegated_write_still_fence_it() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.delegate(DelegationChoice::Hours8, crate::supervisor::now_ms());
        let params = f.params("dl-held", "hello");
        let raw = f.app.handle_agent_submit_with_hold(
            "s".into(),
            params,
            Some(Duration::from_millis(30)),
        );
        assert_eq!(receipt(&raw).state, SubmitState::Accepted);
        f.activity().trip_submit_fences();
        f.pump_submit_event().await;
        f.pump_submit_event().await;
        let r = receipt(&f.status("dl-held"));
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Fenced, Some(SubmitReason::OwnerInput))
        );
        f.assert_no_write().await;
        assert!(
            f.app
                .state
                .supervisor
                .delegations
                .active(f.pane_id, crate::supervisor::now_ms())
                .is_some(),
            "owner typing never revokes"
        );
    }

    #[tokio::test]
    async fn delegation_survives_generation_bumps_and_ends_with_the_pane() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        f.owner_input_then_output(1_000, 1_002);
        f.delegate(DelegationChoice::Hours2, crate::supervisor::now_ms());
        for reason in [
            crate::api::schema::supervisor::GenerationReason::ResetWritten,
            crate::api::schema::supervisor::GenerationReason::SessionChanged,
            crate::api::schema::supervisor::GenerationReason::PaneMoved,
        ] {
            f.app
                .state
                .supervisor
                .bump_generation(f.pane_id, reason, 2_000);
        }
        f.assert_written("dl-gen", "after bumps").await;
        f.app.state.supervisor.remove_pane(f.pane_id);
        assert!(f
            .app
            .state
            .supervisor
            .delegations
            .active(f.pane_id, crate::supervisor::now_ms())
            .is_none());
    }

    #[tokio::test]
    async fn agent_get_shows_delegation_and_no_api_method_grants_one() {
        use crate::supervisor::delegation::DelegationChoice;
        let mut f = fixture();
        f.delegate(DelegationChoice::Permanent, crate::supervisor::now_ms());
        let public = f.app.public_pane_id(0, f.pane_id).unwrap();
        let raw = f.app.handle_api_request(Request {
            id: "g".into(),
            method: Method::AgentGet(crate::api::schema::AgentTarget { target: public }),
        });
        let value: serde_json::Value = serde_json::from_str(&raw).expect(&raw);
        let delegation = &value["result"]["agent"]["delegation"];
        assert_eq!(delegation["active"], true, "{raw}");
        assert_eq!(delegation["permanent"], true, "{raw}");

        // The socket API has no grant: a made-up method does not parse.
        for method in ["agent.delegate", "agent.set_delegation", "pane.delegate"] {
            let request = serde_json::json!({
                "id": "x", "method": method, "params": {"pane_id": "w1:p1", "duration": "2h"}
            });
            assert!(
                serde_json::from_value::<Request>(request).is_err(),
                "{method} must not exist"
            );
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

    // --- S7-S9: agent.answer / agent.interrupt / agent.reset -----------------------------

    const QUESTION: &str = "Should I start the brief?";

    fn screen_above(above: &str) -> Vec<u8> {
        format!("{above}\r\n\r\n{RULE}\r\n❯ \r\n{RULE}\r\n  ? for shortcuts").into_bytes()
    }

    impl Fixture {
        /// Replaces the pane runtime with one showing `bytes` (fresh, unlatched activity).
        fn set_screen(&mut self, bytes: &[u8]) {
            let (runtime, rx) =
                crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                    80, 24, 0, bytes, 8,
                );
            runtime.supervisor_activity().set_stamps_for_test(100, 0);
            self.app.state.insert_test_runtime(self.pane_id, runtime);
            self.rx = rx;
        }

        /// Runs the Idle prose scan against the current screen.
        fn scan_prose(&mut self) {
            self.app
                .state
                .supervisor
                .pane_mut(self.pane_id)
                .pending_prose = true;
            self.app.run_supervisor_scans();
        }

        fn open_questions(&self) -> Vec<String> {
            self.app
                .state
                .supervisor
                .pane(self.pane_id)
                .map(|p| {
                    p.questions
                        .iter()
                        .filter(|q| q.resolution.is_none())
                        .map(|q| q.question_id.clone())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn answer(&mut self, submit_id: &str, question_id: &str, text: &str) -> String {
            let params = AgentAnswerParams {
                target_ref: self.target(),
                question_id: question_id.into(),
                text: text.into(),
                submit_id: submit_id.into(),
            };
            self.app.handle_api_request(Request {
                id: "a".into(),
                method: Method::AgentAnswer(params),
            })
        }

        fn control(&mut self, submit_id: &str) -> AgentControlParams {
            AgentControlParams {
                target_ref: self.target(),
                submit_id: submit_id.into(),
            }
        }

        fn interrupt(&mut self, submit_id: &str) -> String {
            let params = self.control(submit_id);
            self.app.handle_api_request(Request {
                id: "i".into(),
                method: Method::AgentInterrupt(params),
            })
        }

        fn reset(&mut self, submit_id: &str) -> String {
            let params = self.control(submit_id);
            self.app.handle_api_request(Request {
                id: "r".into(),
                method: Method::AgentReset(params),
            })
        }

        fn state_changed(&mut self, state: AgentState) {
            self.app.handle_internal_event(AppEvent::StateChanged {
                pane_id: self.pane_id,
                agent: Some(Agent::Claude),
                state,
                visible_blocker: false,
                visible_working: state == AgentState::Working,
                process_exited: false,
                observed_at: Instant::now(),
            });
        }

        /// A Claude SessionStart report, as the shipped hook sends it.
        fn report_session(&mut self, value: &str, start_source: &str, seq: u64) {
            self.app
                .handle_internal_event(AppEvent::AgentSessionReported {
                    pane_id: self.pane_id,
                    source: "kelpie:claude".into(),
                    agent_label: "claude".into(),
                    seq: Some(seq),
                    session_ref: Some(crate::agent_resume::AgentSessionRef {
                        kind: crate::agent_resume::AgentSessionRefKind::Id,
                        value: value.into(),
                    }),
                    session_start_source: Some(start_source.into()),
                });
        }

        fn assert_nothing_recorded(&self) {
            assert_eq!(self.app.state.supervisor.submits.len(), 0);
        }
    }

    /// Fixture with a prose question on screen and recorded as the open question.
    fn question_fixture(state: AgentState) -> (Fixture, String) {
        let mut f = fixture_with(Agent::Claude, state, "❯ ");
        f.set_screen(&screen_above(QUESTION));
        f.scan_prose();
        let open = f.open_questions();
        assert_eq!(open.len(), 1, "question recorded");
        (f, open[0].clone())
    }

    #[tokio::test]
    async fn answer_accepts_newest_on_screen_prose_question_once() {
        let (mut f, question) = question_fixture(AgentState::Idle);
        let first = receipt(&f.answer("ans-1", &question, "yes"));
        assert_eq!(first.state, SubmitState::Accepted);
        assert_eq!(first.kind, SubmitKind::Answer);
        assert_eq!(first.question_id.as_deref(), Some(question.as_str()));
        assert_eq!(first.text_sha256_16, text_sha256_16("yes"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"yes"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        let written = receipt(&f.status("ans-1"));
        assert_eq!(written.state, SubmitState::Written);
        // The answer does not resolve the question.
        assert_eq!(f.open_questions(), vec![question.clone()]);
        // Idempotent: same receipt, no second write; a different question conflicts.
        assert_eq!(receipt(&f.answer("ans-1", &question, "yes")), written);
        assert_eq!(
            error_code(&f.answer("ans-1", "other-question", "yes")),
            "submit_id_conflict"
        );
        let params = f.params("ans-1", "yes");
        assert_eq!(error_code(&f.submit(params)), "submit_id_conflict");
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn answer_rejects_unknown_resolved_and_off_screen_questions() {
        let (mut f, question) = question_fixture(AgentState::Idle);
        assert_eq!(
            error_code(&f.answer("a-unknown", "0123456789abcdef", "yes")),
            "question_not_current"
        );
        // The question scrolls away: no longer re-detected on screen now.
        f.set_screen(&screen("❯ "));
        assert_eq!(
            error_code(&f.answer("a-offscreen", &question, "yes")),
            "question_not_current"
        );
        // Back on screen, but superseded by a conversation-replacing bump.
        f.set_screen(&screen_above(QUESTION));
        f.app.state.supervisor.bump_generation(
            f.pane_id,
            crate::api::schema::supervisor::GenerationReason::SessionChanged,
            5,
        );
        assert_eq!(
            error_code(&f.answer("a-superseded", &question, "yes")),
            "question_not_current"
        );
        f.assert_nothing_recorded();
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn answer_rejects_an_older_question_and_accepts_the_newer() {
        let (mut f, older) = question_fixture(AgentState::Idle);
        f.set_screen(&screen_above("Do you want the long version?"));
        f.scan_prose();
        let open = f.open_questions();
        assert_eq!(open.len(), 2, "{open:?}");
        let newer = open[1].clone();
        assert_ne!(older, newer);
        assert_eq!(
            error_code(&f.answer("a-old", &older, "yes")),
            "question_not_current"
        );
        f.assert_nothing_recorded();
        f.assert_no_write().await;
        assert_eq!(
            receipt(&f.answer("a-new", &newer, "short")).state,
            SubmitState::Accepted
        );
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"short"));
    }

    #[tokio::test]
    async fn answer_rejects_structured_approval_and_blocked_agent() {
        let (mut f, _) = question_fixture(AgentState::Idle);
        f.app.state.supervisor.insert_question(
            f.pane_id,
            crate::api::schema::supervisor::PendingQuestion {
                question_id: "approval-1".into(),
                kind: crate::api::schema::supervisor::QuestionKind::StructuredApproval,
                excerpt: String::new(),
                asked_at_ms: 1,
                source: crate::api::schema::supervisor::FactSource::ManifestRule,
                confidence: crate::api::schema::supervisor::Confidence::High,
                possible_response_at_ms: None,
                resolution: None,
            },
        );
        assert_eq!(
            error_code(&f.answer("a-approval", "approval-1", "1")),
            "question_not_answerable"
        );
        f.assert_nothing_recorded();
        f.assert_no_write().await;

        let (mut blocked, question) = question_fixture(AgentState::Blocked);
        assert_eq!(
            error_code(&blocked.answer("a-blocked", &question, "yes")),
            "question_not_answerable"
        );
        blocked.assert_nothing_recorded();
        blocked.assert_no_write().await;
    }

    #[tokio::test]
    async fn answer_keeps_submit_validation_and_draft_rule() {
        let (mut f, question) = question_fixture(AgentState::Idle);
        assert_eq!(
            error_code(&f.answer("bad id", &question, "yes")),
            "invalid_request"
        );
        assert_eq!(
            error_code(&f.answer("a-1", &question, "")),
            "invalid_request"
        );
        assert_eq!(error_code(&f.answer("a-1", "", "yes")), "invalid_request");
        f.owner_input_then_output(1_000, 1_002);
        let raw = f.answer("a-latched", &question, "yes");
        assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
        f.assert_nothing_recorded();
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn interrupt_writes_exactly_one_esc_and_records_a_later_idle_edge() {
        let mut f = fixture_with(Agent::Claude, AgentState::Working, "❯ ");
        let first = receipt(&f.interrupt("int-1"));
        assert_eq!(first.state, SubmitState::Accepted);
        assert_eq!(first.kind, SubmitKind::Interrupt);
        assert_eq!(first.text_sha256_16, text_sha256_16(INTERRUPT_HASH_TEXT));
        // The pane's terminal encoding of Esc (a win32-input press/release pair on Windows),
        // exactly as an owner pressing Esc would send it.
        let expected = {
            let runtime = f
                .app
                .lookup_runtime_sender(0, f.pane_id)
                .expect("pane runtime");
            Bytes::from(encode_profile_key(runtime, ProfileKey::Esc))
        };
        assert!(!expected.is_empty());
        assert_eq!(f.rx.recv().await.unwrap(), expected);
        f.pump_submit_event().await;
        let written = receipt(&f.status("int-1"));
        assert_eq!(written.state, SubmitState::Written);
        assert_eq!(written.turn_end_observed_after_write_at_ms, None);
        f.assert_no_write().await; // no Enter follows the key

        f.state_changed(AgentState::Idle);
        let effect = receipt(&f.status("int-1"));
        assert_eq!(effect.state, SubmitState::Written);
        assert!(effect.turn_end_observed_after_write_at_ms.is_some());

        // Idempotent: same receipt, no second Esc.
        assert_eq!(receipt(&f.interrupt("int-1")), effect);
        let params = f.params("int-1", "hello");
        assert_eq!(error_code(&f.submit(params)), "submit_id_conflict");
        assert_eq!(f.app.state.supervisor.submits.len(), 1);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn interrupt_rejects_when_not_working_and_when_latched() {
        for state in [AgentState::Idle, AgentState::Blocked] {
            let mut f = fixture_with(Agent::Claude, state, "❯ ");
            let raw = f.interrupt("int-x");
            assert_eq!(error_code(&raw), "agent_not_working", "{state:?}: {raw}");
            f.assert_nothing_recorded();
            f.assert_no_write().await;
        }
        let mut gemini = fixture_with(Agent::Gemini, AgentState::Working, "❯ ");
        assert_eq!(error_code(&gemini.interrupt("int-c")), "unsupported_agent");
        gemini.assert_nothing_recorded();
        gemini.assert_no_write().await;

        let mut f = fixture_with(Agent::Claude, AgentState::Working, "❯ ");
        f.owner_input_then_output(1_000, 1_002);
        let raw = f.interrupt("int-latched");
        assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
        assert!(raw.contains("draft=unknown"), "{raw}");
        let mut dirty = fixture_with(Agent::Claude, AgentState::Working, "❯ typed");
        assert_eq!(error_code(&dirty.interrupt("int-dirty")), "draft_not_clean");
        f.assert_nothing_recorded();
        dirty.assert_nothing_recorded();
        f.assert_no_write().await;
        dirty.assert_no_write().await;
    }

    #[tokio::test]
    async fn interrupt_fenced_during_hold_writes_zero_bytes() {
        let mut f = fixture_with(Agent::Claude, AgentState::Working, "❯ ");
        let params = f.control("int-held");
        let raw = f.app.handle_agent_interrupt_with_hold(
            "i".into(),
            params,
            Some(Duration::from_millis(30)),
        );
        assert_eq!(receipt(&raw).state, SubmitState::Accepted);
        f.activity().trip_submit_fences();
        f.pump_submit_event().await;
        f.pump_submit_event().await;
        let r = receipt(&f.status("int-held"));
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Fenced, Some(SubmitReason::OwnerInput))
        );
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn reset_submits_clear_and_verifies_on_a_new_clear_session() {
        let mut f = fixture();
        f.report_session("sess-old", "startup", 1);
        assert_eq!(f.target().generation, 1, "first session id is enrichment");
        let first = receipt(&f.reset("rst-1"));
        assert_eq!(first.kind, SubmitKind::Reset);
        assert_eq!(first.reset, Some(ResetEvidence::Pending));
        assert_eq!(first.text_sha256_16, text_sha256_16(RESET_TEXT));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"/clear"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        let written = receipt(&f.status("rst-1"));
        assert_eq!(written.state, SubmitState::Written);
        assert_eq!(written.reset, Some(ResetEvidence::Pending));
        assert!(written.new_session.is_none() && written.new_generation.is_none());

        f.report_session("sess-new", "clear", 2);
        assert_eq!(
            f.target().generation,
            2,
            "clear replacement bumps the generation"
        );
        let verified = receipt(&f.status("rst-1"));
        assert_eq!(verified.reset, Some(ResetEvidence::Verified));
        let session = verified.new_session.as_ref().expect("new session");
        assert_eq!(session.value, "sess-new");
        assert_eq!(verified.new_generation, Some(2));
        // The receipt keeps its original target_ref.
        assert_eq!(verified.target_ref.generation, 1);

        // Idempotent with the original target: same receipt, no second write.
        let again = f.app.handle_api_request(Request {
            id: "r".into(),
            method: Method::AgentReset(AgentControlParams {
                target_ref: first.target_ref.clone(),
                submit_id: "rst-1".into(),
            }),
        });
        assert_eq!(receipt(&again), verified);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn reset_without_clear_evidence_stays_pending_or_unverified() {
        let mut f = fixture();
        f.report_session("sess-a", "startup", 1);
        receipt(&f.reset("rst-p"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"/clear"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        // A turn and an idle edge are not reset evidence.
        f.state_changed(AgentState::Working);
        f.state_changed(AgentState::Idle);
        let pending = receipt(&f.status("rst-p"));
        assert_eq!(pending.reset, Some(ResetEvidence::Pending));
        assert!(pending.new_session.is_none());
        // A replacement with another start source is not this reset: unverified, no id.
        f.report_session("sess-b", "resume", 2);
        assert_eq!(f.target().generation, 2);
        let other = receipt(&f.status("rst-p"));
        assert_eq!(other.reset, Some(ResetEvidence::Unverified));
        assert!(other.new_session.is_none() && other.new_generation.is_none());
    }

    #[tokio::test]
    async fn reset_late_trip_from_clear_session_maps_to_written() {
        let mut f = fixture();
        f.report_session("sess-a", "startup", 1);
        receipt(&f.reset("rst-late"));
        // The text is written (fence committed); the clear SessionStart lands during the
        // 300 ms Enter delay and its generation bump trips the fence late.
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"/clear"));
        f.report_session("sess-b", "clear", 2);
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        let r = receipt(&f.status("rst-late"));
        assert_eq!((r.state, r.reason), (SubmitState::Written, None));
        assert_eq!(r.reset, Some(ResetEvidence::Verified));
        assert_eq!(r.new_session.as_ref().unwrap().value, "sess-b");
        assert_eq!(r.new_generation, Some(2));
    }

    #[tokio::test]
    async fn reset_late_trip_from_other_bump_stays_indeterminate() {
        let mut f = fixture();
        receipt(&f.reset("rst-gen"));
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"/clear"));
        f.app.state.supervisor.bump_generation(
            f.pane_id,
            crate::api::schema::supervisor::GenerationReason::PaneMoved,
            9,
        );
        assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        f.pump_submit_event().await;
        let r = receipt(&f.status("rst-gen"));
        assert_eq!(
            (r.state, r.reason, r.reset),
            (
                SubmitState::Indeterminate,
                Some(SubmitReason::GenerationChangedDuringWrite),
                Some(ResetEvidence::Unverified)
            )
        );
        assert!(r.new_session.is_none());
    }

    #[tokio::test]
    async fn reset_keeps_submit_rejections() {
        for (agent, state, row, code) in [
            (Agent::Gemini, AgentState::Idle, "❯ ", "unsupported_agent"),
            (Agent::Claude, AgentState::Blocked, "❯ ", "agent_blocked"),
            (
                Agent::Claude,
                AgentState::Idle,
                "❯ typed",
                "draft_not_clean",
            ),
        ] {
            let mut f = fixture_with(agent, state, row);
            assert_eq!(error_code(&f.reset("rst-x")), code);
            f.assert_nothing_recorded();
            f.assert_no_write().await;
        }
        let mut f = fixture();
        let mut stale = f.control("rst-stale");
        stale.target_ref.generation += 1;
        let raw = f.app.handle_api_request(Request {
            id: "r".into(),
            method: Method::AgentReset(stale),
        });
        assert_eq!(error_code(&raw), "stale_target");
        f.assert_nothing_recorded();
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn codex_and_cursor_idle_composers_read_clean() {
        for (agent, bytes) in [
            (Agent::Codex, codex_screen()),
            (Agent::Cursor, cursor_screen()),
        ] {
            let f = fixture_with_screen(agent, AgentState::Idle, &bytes);
            let draft = f.app.pane_draft_state(
                0,
                f.pane_id,
                Some(crate::detect::agent_label(agent)),
                false,
                Some(0),
                Some(100),
            );
            assert_eq!(draft, DraftState::Clean, "{agent:?}");
        }
    }

    #[tokio::test]
    async fn codex_and_cursor_submit_interrupt_reset_write_profile_keys() {
        for (agent, bytes) in [
            (Agent::Codex, codex_screen()),
            (Agent::Cursor, cursor_screen()),
        ] {
            // Submit: text, then Enter after the profile delay.
            let mut f = fixture_with_screen(agent, AgentState::Idle, &bytes);
            let params = f.params("sub-1", "hello");
            let first = receipt(&f.submit(params));
            assert_eq!(first.state, SubmitState::Accepted, "{agent:?}");
            assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"hello"));
            assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
            f.pump_submit_event().await;
            assert_eq!(receipt(&f.status("sub-1")).state, SubmitState::Written);
            f.assert_no_write().await;

            // Interrupt: exactly the profile's Esc encoding, no Enter.
            let mut f = fixture_with_screen(agent, AgentState::Working, &bytes);
            let first = receipt(&f.interrupt("int-1"));
            assert_eq!(first.kind, SubmitKind::Interrupt, "{agent:?}");
            let expected = {
                let runtime = f
                    .app
                    .lookup_runtime_sender(0, f.pane_id)
                    .expect("pane runtime");
                Bytes::from(encode_profile_key(runtime, ProfileKey::Esc))
            };
            assert!(!expected.is_empty());
            assert_eq!(f.rx.recv().await.unwrap(), expected);
            f.pump_submit_event().await;
            assert_eq!(receipt(&f.status("int-1")).state, SubmitState::Written);
            f.assert_no_write().await;

            // Reset: the profile's reset text through the submit path.
            let mut f = fixture_with_screen(agent, AgentState::Idle, &bytes);
            let first = receipt(&f.reset("rst-1"));
            assert_eq!(first.kind, SubmitKind::Reset, "{agent:?}");
            assert_eq!(first.text_sha256_16, text_sha256_16(RESET_TEXT));
            assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"/clear"));
            assert_eq!(f.rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
            f.pump_submit_event().await;
            let written = receipt(&f.status("rst-1"));
            assert_eq!(written.kind, SubmitKind::Reset);
            assert_eq!(written.state, SubmitState::Written);
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn codex_answer_is_unsupported() {
        let mut f = fixture_with_screen(Agent::Codex, AgentState::Idle, &codex_screen());
        let raw = f.answer("a-codex", "q-1", "yes");
        assert_eq!(error_code(&raw), "unsupported_agent", "{raw}");
        f.assert_nothing_recorded();
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

    // --- S10: agent.resume -------------------------------------------------------------------

    /// A pane running a plain shell: no agent detected, no managed launch.
    fn shell_fixture() -> Fixture {
        shell_fixture_with_capacity(8)
    }

    fn shell_fixture_with_capacity(capacity: usize) -> Fixture {
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
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("resume")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = crate::app::Mode::Terminal;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let (runtime, rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"$ ", capacity,
            );
        // `agent.start` resolves the pane's runtime through the terminal registry (not the
        // per-workspace test map), so the runtime goes there. It has no child process: the
        // start path treats it as an `sh` shell and nothing is spawned.
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.terminal_runtimes.insert(terminal_id, runtime);
        Fixture {
            app,
            pane_id,
            rx,
            _reset: reset,
        }
    }

    impl Fixture {
        fn public_pane(&self) -> String {
            self.app.public_pane_id(0, self.pane_id).unwrap()
        }

        fn resume(&mut self, submit_id: &str, session_id: &str) -> String {
            let pane_id = self.public_pane();
            self.resume_in(&pane_id, submit_id, session_id)
        }

        fn resume_in(&mut self, pane_id: &str, submit_id: &str, session_id: &str) -> String {
            self.resume_agent_in(pane_id, submit_id, session_id, None)
        }

        fn resume_agent_in(
            &mut self,
            pane_id: &str,
            submit_id: &str,
            session_id: &str,
            agent: Option<&str>,
        ) -> String {
            self.app.handle_api_request(Request {
                id: "rs".into(),
                method: Method::AgentResume(AgentResumeParams {
                    pane_id: pane_id.into(),
                    session_id: session_id.into(),
                    submit_id: submit_id.into(),
                    agent: agent.map(str::to_string),
                }),
            })
        }

        /// The exact bytes `agent.start` sends to the test shell for this argv.
        fn start_bytes(&self, argv: &[&str]) -> Bytes {
            let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
            let command =
                crate::platform::interactive_shell_command(&argv, "sh").expect("encodable command");
            let runtime = self
                .app
                .lookup_runtime_sender(0, self.pane_id)
                .expect("pane runtime");
            Bytes::from(crate::app::api_helpers::encode_api_submission(
                runtime, &command,
            ))
        }

        /// The next chunk written to the pane, bounded so a missing write fails, not hangs.
        async fn next_write(&mut self) -> Bytes {
            tokio::time::timeout(Duration::from_secs(2), self.rx.recv())
                .await
                .expect("a pane write within 2s")
                .expect("pane channel open")
        }

        /// Process detection of the launched Claude (no hook, no event).
        fn detect_claude(&mut self) {
            let terminal_id = self.app.state.workspaces[0].tabs[0].panes[&self.pane_id]
                .attached_terminal_id
                .clone();
            self.app
                .state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .set_detected_state(Some(Agent::Claude), AgentState::Idle);
        }
    }

    #[tokio::test]
    async fn resume_rejections_launch_nothing_and_record_nothing() {
        let mut f = shell_fixture();
        for (submit_id, session_id, code) in [
            ("bad id", "sess", "invalid_request"),
            ("r-1", "", "invalid_request"),
            ("r-1", "a\nb", "invalid_request"),
            ("r-1", "--dangerously-skip-permissions", "invalid_request"),
            ("r-1", "sess id", "invalid_request"),
            (
                "r-1",
                &*"x".repeat(MAX_RESUME_SESSION_ID_CHARS + 1),
                "invalid_request",
            ),
        ] {
            assert_eq!(error_code(&f.resume(submit_id, session_id)), code);
        }
        assert_eq!(
            error_code(&f.resume_in("w9:p9", "r-1", "sess")),
            "agent_pane_not_found"
        );
        crate::supervisor::set_enabled_for_test(Some(false));
        assert_eq!(error_code(&f.resume("r-1", "sess")), "supervisor_disabled");
        crate::supervisor::set_enabled_for_test(Some(true));
        f.app.state.supervisor.submits.set_cap_for_test(0);
        assert_eq!(error_code(&f.resume("r-1", "sess")), "submit_capacity");
        f.assert_nothing_recorded();
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn resume_rejects_a_pane_with_a_known_agent_as_agent_running() {
        for state in [AgentState::Idle, AgentState::Working, AgentState::Unknown] {
            let mut f = fixture_with(Agent::Claude, state, "❯ ");
            let raw = f.resume("r-busy", "sess");
            assert_eq!(error_code(&raw), "agent_running", "{state:?}: {raw}");
            f.assert_nothing_recorded();
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn resume_launches_once_through_agent_start_and_repeats_without_relaunch() {
        let mut f = shell_fixture();
        let first = receipt(&f.resume("r-1", "sess-123"));
        assert_eq!(first.kind, SubmitKind::Resume);
        assert_eq!(first.state, SubmitState::Launched);
        assert!(first.launched_at_ms.is_some());
        assert_eq!(first.session_id.as_deref(), Some("sess-123"));
        assert_eq!(first.resume, Some(ResumeEvidence::Pending));
        assert_eq!(first.text_sha256_16, text_sha256_16("sess-123"));
        assert_eq!(first.reset, None);
        assert_eq!(first.target_ref.pane_id, f.public_pane());
        assert_eq!(
            first.acknowledged,
            crate::api::schema::supervisor::SubmitAcknowledged::Unsupported
        );
        // Exactly one start: the agent.start command line for `claude --resume <id>`.
        let expected = f.start_bytes(&["claude", "--resume", "sess-123"]);
        assert_eq!(f.next_write().await, expected);
        f.assert_no_write().await;

        // Same id: same receipt, no second launch. Different session or verb: conflict.
        assert_eq!(receipt(&f.resume("r-1", "sess-123")), first);
        assert_eq!(
            error_code(&f.resume("r-1", "sess-other")),
            "submit_id_conflict"
        );
        // A new id into the same pane: the pending managed launch is a running agent.
        assert_eq!(error_code(&f.resume("r-2", "sess-123")), "agent_running");
        assert_eq!(f.app.state.supervisor.submits.len(), 1);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn resume_repeat_requires_same_profile_agent() {
        for (requested, canonical, equivalents) in [
            (
                None,
                "claude",
                &[Some("claude"), Some("CLAUDE"), Some("claude-code")][..],
            ),
            (Some("claude-code"), "claude", &[None, Some("Claude")][..]),
            (Some("codex"), "codex", &[Some("CODEX"), Some("Codex")][..]),
            (
                Some("cursor-agent"),
                "cursor",
                &[Some("cursor"), Some("CURSOR-AGENT")][..],
            ),
        ] {
            let mut f = shell_fixture();
            let pane = f.public_pane();
            let first =
                receipt(&f.resume_agent_in(&pane, "profile-repeat", "requested", requested));
            assert_eq!(first.state, SubmitState::Launched);
            assert_eq!(first.session_id.as_deref(), Some("requested"));
            assert_eq!(first.text_sha256_16, text_sha256_16("requested"));
            f.next_write().await;
            for equivalent in equivalents {
                assert_eq!(
                    receipt(&f.resume_agent_in(&pane, "profile-repeat", "requested", *equivalent)),
                    first,
                    "{requested:?} repeated as {equivalent:?}"
                );
            }
            for different in ["claude", "codex", "cursor"] {
                if different == canonical {
                    continue;
                }
                let raw = f.resume_agent_in(&pane, "profile-repeat", "requested", Some(different));
                let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
                assert_eq!(
                    response["error"]["code"], "submit_id_conflict",
                    "{requested:?} repeated as {different} must conflict before pane-busy checks"
                );
                assert_eq!(receipt(&f.status("profile-repeat")), first);
            }
            assert_eq!(f.app.state.supervisor.submits.len(), 1);
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn resume_foreign_agent_report_stays_pending() {
        for reported in ["requested", "different"] {
            let mut f = shell_fixture();
            let pane = f.public_pane();
            let pending =
                receipt(&f.resume_agent_in(&pane, "cursor-resume", "requested", Some("cursor")));
            assert_eq!(pending.state, SubmitState::Launched);
            assert_eq!(pending.resume, Some(ResumeEvidence::Pending));
            f.next_write().await;
            let terminal_id = f.app.state.workspaces[0].tabs[0].panes[&f.pane_id]
                .attached_terminal_id
                .clone();
            assert!(f.app.state.terminals[&terminal_id]
                .current_session_identity()
                .is_none());

            // Before process detection, this fresh canonical Claude startup report is
            // accepted by the terminal even though the pending launch requested Cursor.
            f.report_session(reported, "startup", 1);
            let identity = f.app.state.terminals[&terminal_id]
                .current_session_identity()
                .unwrap();
            assert_eq!(
                (
                    identity.0.as_str(),
                    identity.1.as_str(),
                    identity.3.as_str()
                ),
                ("kelpie:claude", "claude", reported),
                "foreign report must be accepted by the terminal"
            );
            assert_eq!(
                receipt(&f.status("cursor-resume")),
                pending,
                "accepted foreign report {reported} must leave the entire receipt pending"
            );

            // Real detection then permits the requested agent's startup report to take
            // ownership. The foreign evidence must not have consumed this settlement.
            f.app.handle_internal_event(AppEvent::StateChanged {
                pane_id: f.pane_id,
                agent: Some(Agent::Cursor),
                state: AgentState::Idle,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now(),
            });
            f.app.handle_internal_event(AppEvent::AgentSessionReported {
                pane_id: f.pane_id,
                source: "kelpie:cursor".into(),
                agent_label: "cursor".into(),
                seq: Some(1),
                session_ref: crate::agent_resume::AgentSessionRef::id("requested"),
                session_start_source: Some("startup".into()),
            });
            let identity = f.app.state.terminals[&terminal_id]
                .current_session_identity()
                .unwrap();
            assert_eq!(
                (
                    identity.0.as_str(),
                    identity.1.as_str(),
                    identity.3.as_str()
                ),
                ("kelpie:cursor", "cursor", "requested"),
                "requested report must be accepted by the terminal"
            );
            let settled = receipt(&f.status("cursor-resume"));
            assert_eq!(settled.resume, Some(ResumeEvidence::Verified));
            assert_eq!(settled.session_start_source.as_deref(), Some("startup"));
            assert!(settled.reported_session.is_none());
            assert_eq!(settled.new_generation, Some(f.target().generation));
            f.assert_no_write().await;
        }
    }

    #[tokio::test]
    async fn resume_and_submit_ids_conflict_across_kinds() {
        let mut f = fixture();
        let params = f.params("shared", "hello");
        assert_eq!(receipt(&f.submit(params)).kind, SubmitKind::Submit);
        // The resume repeat check runs before the pane checks: a conflict, not agent_running.
        assert_eq!(
            error_code(&f.resume("shared", "sess")),
            "submit_id_conflict"
        );

        let mut g = shell_fixture();
        receipt(&g.resume("shared-2", "sess"));
        let params = g.params("shared-2", "hello");
        assert_eq!(error_code(&g.submit(params)), "submit_id_conflict");
    }

    #[tokio::test]
    async fn resume_start_failure_is_failed_with_the_start_error() {
        // A full PTY input queue refuses the start command (agent_start_input_failed).
        let mut f = shell_fixture_with_capacity(1);
        f.app
            .lookup_runtime_sender(0, f.pane_id)
            .unwrap()
            .try_send_bytes(Bytes::from_static(b"occupied"))
            .unwrap();
        let r = receipt(&f.resume("r-fail", "sess"));
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Failed, Some(SubmitReason::StartFailed))
        );
        assert_eq!(r.start_error.as_deref(), Some("agent_start_input_failed"));
        assert_eq!(r.resume, Some(ResumeEvidence::Pending));
        assert!(r.launched_at_ms.is_none());
        assert_eq!(f.next_write().await, Bytes::from_static(b"occupied"));
        f.assert_no_write().await;
        // Idempotent: the failed receipt is returned, nothing is retried.
        assert_eq!(receipt(&f.resume("r-fail", "sess")), r);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn resume_verified_by_matching_session_report_after_launch() {
        let mut f = shell_fixture();
        receipt(&f.resume("r-v", "sess-v"));
        f.next_write().await;
        f.detect_claude();
        f.report_session("sess-v", "resume", 1);
        let r = receipt(&f.status("r-v"));
        assert_eq!(r.state, SubmitState::Launched);
        assert_eq!(r.resume, Some(ResumeEvidence::Verified));
        assert_eq!(r.session_start_source.as_deref(), Some("resume"));
        assert!(r.reported_session.is_none());
        assert_eq!(r.new_generation, Some(f.target().generation));
        // A later different session never rewrites the verified receipt.
        f.report_session("sess-later", "clear", 2);
        assert_eq!(
            receipt(&f.status("r-v")).resume,
            Some(ResumeEvidence::Verified)
        );
    }

    #[tokio::test]
    async fn resume_mismatch_records_the_reported_session() {
        let mut f = shell_fixture();
        receipt(&f.resume("r-m", "sess-wanted"));
        f.next_write().await;
        f.detect_claude();
        f.report_session("sess-fresh", "startup", 1);
        let r = receipt(&f.status("r-m"));
        assert_eq!(r.resume, Some(ResumeEvidence::Mismatch));
        let reported = r.reported_session.as_ref().expect("reported session");
        assert_eq!(reported.value, "sess-fresh");
        assert_eq!(r.session_start_source.as_deref(), Some("startup"));
        assert_eq!(r.session_id.as_deref(), Some("sess-wanted"));
    }

    #[tokio::test]
    async fn resume_without_a_session_report_stays_pending() {
        let mut f = shell_fixture();
        receipt(&f.resume("r-p", "sess-p"));
        f.next_write().await;
        f.detect_claude();
        f.state_changed(AgentState::Working);
        f.state_changed(AgentState::Idle);
        let r = receipt(&f.status("r-p"));
        assert_eq!(
            (r.state, r.resume),
            (SubmitState::Launched, Some(ResumeEvidence::Pending))
        );
        assert!(r.reported_session.is_none() && r.session_start_source.is_none());
    }

    /// Earlier Claude in the pane reported session X and exited; its identity can still
    /// resolve on the terminal. A resume of X whose launched agent reports Y must not read
    /// as verified from the resolved identity.
    #[tokio::test]
    async fn resume_evidence_comes_from_the_reported_session_not_the_resolved_identity() {
        let mut f = shell_fixture();
        f.detect_claude();
        f.report_session("sess-x", "startup", 1);
        let terminal_id = f.app.state.workspaces[0].tabs[0].panes[&f.pane_id]
            .attached_terminal_id
            .clone();
        {
            let terminal = f.app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.set_detected_state_with_visible_blocker(
                Some(Agent::Claude),
                AgentState::Idle,
                false,
                false,
                true,
            );
            terminal.set_detected_state(None, AgentState::Unknown);
        }
        let r = receipt(&f.resume("r-g", "sess-x"));
        assert_eq!(r.state, SubmitState::Launched);
        f.next_write().await;
        f.detect_claude();
        f.report_session("sess-y", "startup", 2);
        let identity = f.app.state.terminals[&terminal_id].current_session_identity();
        let r = receipt(&f.status("r-g"));
        // Here the terminal adopts the reported session, so the resolved identity is y
        // too; the registry test pins the case where it is not.
        assert_eq!(
            identity.map(|(_, _, _, value)| value).as_deref(),
            Some("sess-y")
        );
        assert_eq!(r.resume, Some(ResumeEvidence::Mismatch), "{r:?}");
        assert_eq!(r.reported_session.as_ref().unwrap().value, "sess-y");

        // The same history, then the requested id reported: verified.
        let mut g = shell_fixture();
        g.detect_claude();
        g.report_session("sess-x", "startup", 1);
        let terminal_id = g.app.state.workspaces[0].tabs[0].panes[&g.pane_id]
            .attached_terminal_id
            .clone();
        {
            let terminal = g.app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.set_detected_state_with_visible_blocker(
                Some(Agent::Claude),
                AgentState::Idle,
                false,
                false,
                true,
            );
            terminal.set_detected_state(None, AgentState::Unknown);
        }
        assert_eq!(
            receipt(&g.resume("r-g", "sess-x")).state,
            SubmitState::Launched
        );
        g.next_write().await;
        g.detect_claude();
        g.report_session("sess-x", "resume", 2);
        assert_eq!(
            receipt(&g.status("r-g")).resume,
            Some(ResumeEvidence::Verified)
        );
    }

    #[tokio::test]
    async fn resume_rejects_a_latched_pane_unless_a_handoff_is_consumed() {
        let mut f = shell_fixture();
        f.owner_input_then_output(1_000, 1_002);
        let raw = f.resume("r-latched", "sess");
        assert_eq!(error_code(&raw), "draft_not_clean", "{raw}");
        f.assert_nothing_recorded();
        f.assert_no_write().await;

        f.arm_handoff(crate::supervisor::now_ms());
        assert!(f.handoff_valid());
        let r = receipt(&f.resume("r-handoff", "sess"));
        assert_eq!(r.state, SubmitState::Launched);
        assert!(r.handoff, "receipt records the consumed handoff");
        assert!(!f.handoff_valid(), "handoff consumed at accept");
        let expected = f.start_bytes(&["claude", "--resume", "sess"]);
        assert_eq!(f.next_write().await, expected);
        f.assert_no_write().await;
    }

    #[tokio::test]
    async fn resume_agent_param_selects_profile() {
        let mut f = shell_fixture();
        let pane = f.public_pane();
        for label in ["gemini", "nope"] {
            let raw = f.resume_agent_in(&pane, "r-bad", "sess-1", Some(label));
            assert_eq!(error_code(&raw), "unsupported_agent", "{label}: {raw}");
        }
        f.assert_nothing_recorded();
        f.assert_no_write().await;

        let first = receipt(&f.resume_agent_in(&pane, "r-codex", "sess-1", Some("codex")));
        assert_eq!(first.kind, SubmitKind::Resume);
        assert_eq!(first.state, SubmitState::Launched);
        let expected = f.start_bytes(&["codex", "resume", "sess-1"]);
        assert_eq!(f.next_write().await, expected);
        f.assert_no_write().await;
    }
}

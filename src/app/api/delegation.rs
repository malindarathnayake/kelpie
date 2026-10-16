//! Fork: owner-granted pane delegation (see `crate::supervisor::delegation`).

use crate::api::schema::supervisor::AgentUndelegateParams;
use crate::api::schema::ResponseResult;
use crate::app::App;

use super::responses;
use super::supervisor::RequestLog;
use crate::supervisor::delegation::DelegationChoice;

/// What a client or digest shows for a pane's delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DelegationView {
    /// None = permanent.
    pub(crate) expires_at_ms: Option<u64>,
}

impl App {
    /// Applies the owner's delegate-menu choice for a pane. Reached only from the
    /// attached-client `DELEGATE_CONTROL_KIND` control (the server checks the client is an
    /// active shell client viewing the pane); no API method or CLI command calls it.
    pub(crate) fn apply_supervisor_delegation(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        public_pane_id: &str,
        choice: DelegationChoice,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        let pane = super::supervisor::sanitize_log_token(Some(public_pane_id), 64);
        match choice {
            DelegationChoice::Once => {
                return self
                    .arm_supervisor_handoff(ws_idx, pane_id, public_pane_id, now_ms)
                    .map(|_| ());
            }
            DelegationChoice::Revoke => {
                let revoked = self.state.supervisor.delegations.revoke(pane_id);
                tracing::debug!(
                    event = "supervisor.delegation",
                    subsystem = "supervisor",
                    outcome = if revoked { "revoked" } else { "not_delegated" },
                    source = "owner",
                    pane = %pane,
                );
                return Ok(());
            }
            _ => {}
        }
        if !crate::supervisor::enabled() {
            tracing::debug!(
                event = "supervisor.delegation",
                subsystem = "supervisor",
                outcome = "rejected",
                error_code = "supervisor_disabled",
                pane = %pane,
            );
            return Err("supervisor_disabled");
        }
        if self.lookup_runtime_sender(ws_idx, pane_id).is_none() {
            return Err("pane_not_found");
        }
        let session = self.pane_agent_session_value(ws_idx, pane_id);
        let granted = self
            .state
            .supervisor
            .delegations
            .grant(pane_id, choice, now_ms, session)
            .is_some();
        tracing::debug!(
            event = "supervisor.delegation",
            subsystem = "supervisor",
            outcome = if granted { "granted" } else { "ignored" },
            duration = choice.as_str(),
            pane = %pane,
        );
        Ok(())
    }

    /// The agent session id a pane's agent last reported, for permanent delegations.
    fn pane_agent_session_value(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<String> {
        let terminal = self
            .state
            .workspaces
            .get(ws_idx)?
            .pane_state(pane_id)
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))?;
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(|session| session.value.clone())
            .or_else(|| {
                terminal
                    .persisted_agent_session
                    .as_ref()
                    .map(|session| session.session_ref.value.clone())
            })
    }

    /// `agent.undelegate`: anyone (including the supervisor) may give a pane back.
    pub(super) fn handle_agent_undelegate(
        &mut self,
        id: String,
        params: AgentUndelegateParams,
    ) -> String {
        let log = RequestLog::new("agent.undelegate", &id);
        if !crate::supervisor::enabled() {
            let response = responses::encode_error(
                id,
                "supervisor_disabled",
                "supervisor mode is disabled; set [supervisor] enabled = true and restart the server",
            );
            log.error("supervisor_disabled", &response);
            return response;
        }
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            let response = responses::encode_error(
                id,
                "pane_not_found",
                format!("pane {} not found", params.pane_id),
            );
            log.error("pane_not_found", &response);
            return response;
        };
        let revoked = self.state.supervisor.delegations.revoke(pane_id);
        tracing::debug!(
            event = "supervisor.delegation",
            subsystem = "supervisor",
            outcome = if revoked { "revoked" } else { "not_delegated" },
            source = "api",
            trace_id = %log.trace_id,
        );
        let response = responses::encode_success(id, ResponseResult::Ok {});
        log.ok(usize::from(revoked), &response);
        response
    }

    /// Drops expired delegations; true when any ended (the snapshot changed).
    pub(crate) fn expire_supervisor_delegations(&mut self, now_ms: u64) -> bool {
        let expired = self.state.supervisor.delegations.expire_due(now_ms);
        for pane in &expired {
            tracing::debug!(
                event = "supervisor.delegation",
                subsystem = "supervisor",
                outcome = "expired",
                pane_raw = pane.raw(),
            );
        }
        !expired.is_empty()
    }

    /// The pane's active delegation, for the client snapshot and the digest.
    pub(crate) fn delegation_view(
        &self,
        pane_id: crate::layout::PaneId,
        now_ms: u64,
    ) -> Option<DelegationView> {
        if !crate::supervisor::enabled() {
            return None;
        }
        self.state
            .supervisor
            .delegations
            .active(pane_id, now_ms)
            .map(|delegation| DelegationView {
                expires_at_ms: delegation.expires_at_ms,
            })
    }
}

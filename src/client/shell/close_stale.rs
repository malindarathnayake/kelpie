//! Fork (Q1): "close stale workspaces" review dialog.
//!
//! Three layers stay separate: (1) layout, the workspace leaves the sidebar; (2) processes,
//! its pane processes end through the existing `workspace.close`; (3) history, agent sessions
//! are not deleted by kelpie and stay resumable from the agent. Nothing closes automatically:
//! rows with running or unknown work start unticked and also need "also terminate".

use super::sidebar_activity::{close_risks, format_idle, idle_bucket, workspace_idle_ms};
use super::*;

/// Most workspace entries one audit line lists before summarising the rest.
const AUDIT_MAX_ENTRIES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CloseStaleRow {
    pub(super) workspace_id: String,
    pub(super) number: usize,
    pub(super) label: String,
    pub(super) idle_ms: Option<u64>,
    pub(super) state: &'static str,
    pub(super) risks: Vec<&'static str>,
    pub(super) ticked: bool,
    pub(super) terminate: bool,
}

impl CloseStaleRow {
    pub(super) fn risky(&self) -> bool {
        !self.risks.is_empty()
    }

    /// Ticked, and either safe or explicitly confirmed with "also terminate".
    pub(super) fn eligible(&self) -> bool {
        self.ticked && (!self.risky() || self.terminate)
    }

    pub(super) fn blocked(&self) -> bool {
        self.ticked && self.risky() && !self.terminate
    }

    pub(super) fn idle_label(&self) -> String {
        self.idle_ms.map_or_else(|| "?".to_owned(), format_idle)
    }
}

#[derive(Debug)]
pub(super) struct ClientCloseStaleOverlay {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) rows: Vec<CloseStaleRow>,
    pub(super) cursor: usize,
    pub(super) notice: Option<String>,
    pub(super) stale_after_ms: u64,
}

impl ClientCloseStaleOverlay {
    pub(super) fn summary(&self) -> String {
        let picks = self.rows.iter().filter(|row| row.eligible()).count();
        let blocked = self.rows.iter().filter(|row| row.blocked()).count();
        if blocked == 0 {
            format!("{picks} to close")
        } else {
            format!("{picks} to close · {blocked} need \"also terminate\" (t)")
        }
    }

    pub(super) fn toggle_tick(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            row.ticked = !row.ticked;
            if !row.ticked {
                row.terminate = false;
            }
        }
        self.notice = None;
    }

    /// "Also terminate" applies only to risky rows; setting it also ticks the row.
    pub(super) fn toggle_terminate(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index).filter(|row| row.risky()) {
            row.terminate = !row.terminate;
            if row.terminate {
                row.ticked = true;
            }
        }
        self.notice = None;
    }
}

const CHANGED_NOTICE: &str =
    "Workspaces changed since review (closed, busy or new risk); check the ticks and press Enter again";

impl ClientCloseStaleOverlay {
    /// Rebuilds the rows from the current snapshot. Ticks carry over; a row that gained a
    /// risk loses its "also terminate"; a row that appeared after review starts unticked;
    /// a row that is gone or no longer stale is dropped. Returns true when a ticked row was
    /// dropped or gained a risk, so the owner must review again before anything closes.
    pub(super) fn refresh(&mut self, snapshot: &ClientShellSnapshot, now_ms: u64) -> bool {
        let mut ticked_changed = false;
        let mut rows = close_stale_rows(snapshot, now_ms, self.stale_after_ms);
        for row in &mut rows {
            match self
                .rows
                .iter()
                .find(|old| old.workspace_id == row.workspace_id)
            {
                Some(old) => {
                    let new_risk = row.risks.iter().any(|risk| !old.risks.contains(risk));
                    row.ticked = old.ticked;
                    row.terminate = old.terminate && !new_risk;
                    ticked_changed |= old.ticked && new_risk;
                }
                None => row.ticked = false,
            }
        }
        ticked_changed |= self
            .rows
            .iter()
            .any(|old| old.ticked && !rows.iter().any(|row| row.workspace_id == old.workspace_id));
        self.rows = rows;
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        ticked_changed
    }
}

/// Stale workspaces in sidebar order. Risky rows start unticked; safe rows start ticked.
pub(super) fn close_stale_rows(
    snapshot: &ClientShellSnapshot,
    now_ms: u64,
    stale_after_ms: u64,
) -> Vec<CloseStaleRow> {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| {
            super::sidebar_activity::workspace_is_stale(
                snapshot,
                workspace,
                workspace.agent_status,
                now_ms,
                stale_after_ms,
            )
        })
        .map(|workspace| {
            let risks = close_risks(snapshot, workspace);
            CloseStaleRow {
                workspace_id: workspace.workspace_id.clone(),
                number: workspace.number,
                label: workspace.label.clone(),
                idle_ms: workspace_idle_ms(workspace, now_ms),
                state: status_text(workspace.agent_status),
                ticked: risks.is_empty(),
                terminate: false,
                risks,
            }
        })
        .collect()
}

/// Bounded audit line: trigger, count and per-workspace id with an idle bucket. No labels,
/// paths or exact ages.
pub(super) fn workspace_close_audit_line(
    trigger: &str,
    entries: &[(&str, Option<u64>)],
    terminated_risky: usize,
) -> String {
    let mut listed = entries
        .iter()
        .take(AUDIT_MAX_ENTRIES)
        .map(|(id, idle)| format!("{id}:{}", idle_bucket(*idle)))
        .collect::<Vec<_>>();
    if entries.len() > AUDIT_MAX_ENTRIES {
        listed.push(format!("+{}", entries.len() - AUDIT_MAX_ENTRIES));
    }
    format!(
        "workspace.close trigger={trigger} count={} terminated_risky={terminated_risky} workspaces=[{}]",
        entries.len(),
        listed.join(",")
    )
}

fn emit_close_audit(line: &str) {
    tracing::info!(target: "kelpie::audit", "{line}");
}

impl ClientShellState {
    pub(super) fn open_close_stale_overlay(&mut self, outcome: &mut ClientShellInput) {
        let (_, now_ms) = self.sidebar_clock();
        let stale_after_ms = self.config.stale_after_ms;
        let rows = self
            .snapshot
            .as_deref()
            .map(|snapshot| close_stale_rows(snapshot, now_ms, stale_after_ms))
            .unwrap_or_default();
        self.overlay = Some(ClientShellOverlay::CloseStale(ClientCloseStaleOverlay {
            endpoint_id: self.active_endpoint_id.clone(),
            rows,
            cursor: 0,
            notice: None,
            stale_after_ms,
        }));
        if self.mode == ClientShellMode::Navigate {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
        }
        outcome.repaint = true;
    }

    pub(super) fn route_close_stale_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::CloseStale(dialog)) = self.overlay.as_mut() else {
            return;
        };
        outcome.repaint = true;
        let last = dialog.rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
            }
            KeyCode::Enter => self.accept_close_stale(outcome),
            KeyCode::Up | KeyCode::Char('k') => dialog.cursor = dialog.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => dialog.cursor = (dialog.cursor + 1).min(last),
            KeyCode::Char(' ') => {
                let cursor = dialog.cursor;
                dialog.toggle_tick(cursor);
            }
            KeyCode::Char('t') => {
                let cursor = dialog.cursor;
                dialog.toggle_terminate(cursor);
            }
            _ => outcome.repaint = false,
        }
    }

    /// Mouse on a dialog row: the row ticks it, the "also terminate" line confirms it.
    pub(super) fn click_close_stale(&mut self, point: (u16, u16)) -> bool {
        let Some((_, index, terminate)) = self
            .hits
            .close_stale_rows
            .iter()
            .find(|(rect, _, _)| super::contains(*rect, point))
            .copied()
        else {
            return false;
        };
        if let Some(ClientShellOverlay::CloseStale(dialog)) = self.overlay.as_mut() {
            dialog.cursor = index;
            if terminate {
                dialog.toggle_terminate(index);
            } else {
                dialog.toggle_tick(index);
            }
        }
        true
    }

    /// Closes every eligible row through the existing `workspace.close` path, one request
    /// per workspace, and writes one audit line. Refuses while a ticked risky row lacks
    /// "also terminate".
    pub(super) fn accept_close_stale(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::CloseStale(mut dialog)) = self.overlay.take() else {
            return;
        };
        outcome.repaint = true;
        if dialog.endpoint_id != self.active_endpoint_id {
            self.receive_endpoint_unavailable("Machine changed; open close stale again".to_owned());
            return;
        }
        // Re-check every row against the current snapshot and clock: rows were captured
        // when the dialog opened and their work may have changed since.
        let (_, now_ms) = self.sidebar_clock();
        let changed = match self.snapshot.as_deref() {
            Some(snapshot) => dialog.refresh(snapshot, now_ms),
            None => {
                self.receive_endpoint_unavailable(
                    "Snapshot unavailable; nothing closed".to_owned(),
                );
                return;
            }
        };
        if changed {
            dialog.notice = Some(CHANGED_NOTICE.to_owned());
            self.overlay = Some(ClientShellOverlay::CloseStale(dialog));
            return;
        }
        if dialog.rows.iter().any(CloseStaleRow::blocked) {
            dialog.notice = Some(
                "Ticked rows with running work need \"also terminate\" (t), or untick them"
                    .to_owned(),
            );
            self.overlay = Some(ClientShellOverlay::CloseStale(dialog));
            return;
        }
        let picks = dialog
            .rows
            .iter()
            .filter(|row| row.eligible())
            .collect::<Vec<_>>();
        if picks.is_empty() {
            dialog.notice = Some("Nothing ticked; space ticks the selected row".to_owned());
            self.overlay = Some(ClientShellOverlay::CloseStale(dialog));
            return;
        }
        let picks = picks.into_iter().cloned().collect::<Vec<_>>();
        let mut sent = Vec::new();
        for row in picks {
            let before = outcome.actions.len();
            self.push_endpoint_method(
                crate::api::schema::Method::WorkspaceClose(
                    crate::api::schema::WorkspaceCloseParams {
                        workspace_id: row.workspace_id.clone(),
                        close_group: false,
                    },
                ),
                outcome,
            );
            if outcome.actions.len() > before {
                sent.push(row);
            }
        }
        // Written after the requests, counting only the ones actually queued.
        let audit = workspace_close_audit_line(
            "close_stale",
            &sent
                .iter()
                .map(|row| (row.workspace_id.as_str(), row.idle_ms))
                .collect::<Vec<_>>(),
            sent.iter().filter(|row| row.risky()).count(),
        );
        emit_close_audit(&audit);
        #[cfg(test)]
        {
            self.last_close_audit = Some(audit);
        }
    }

    /// Snapshot install hook: keep an open dialog current with the endpoint's state.
    pub(super) fn refresh_close_stale_dialog(&mut self) {
        let (_, now_ms) = self.sidebar_clock();
        let active = self.active_endpoint_id.clone();
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        if let Some(ClientShellOverlay::CloseStale(dialog)) = self.overlay.as_mut() {
            if dialog.endpoint_id == active && dialog.refresh(snapshot, now_ms) {
                dialog.notice = Some(CHANGED_NOTICE.to_owned());
            }
        }
    }

    /// Sends one `workspace.close`, then audits it when the request was actually queued.
    pub(super) fn push_audited_workspace_close(
        &mut self,
        params: crate::api::schema::WorkspaceCloseParams,
        outcome: &mut ClientShellInput,
    ) {
        let workspace_id = params.workspace_id.clone();
        let before = outcome.actions.len();
        self.push_endpoint_method(crate::api::schema::Method::WorkspaceClose(params), outcome);
        if outcome.actions.len() > before {
            self.audit_single_workspace_close(&workspace_id);
        }
    }

    /// Audit line for a single-workspace close requested from this client.
    fn audit_single_workspace_close(&mut self, workspace_id: &str) {
        let (_, now_ms) = self.sidebar_clock();
        let idle = self.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
                .and_then(|workspace| workspace_idle_ms(workspace, now_ms))
        });
        let audit = workspace_close_audit_line("close_workspace", &[(workspace_id, idle)], 0);
        emit_close_audit(&audit);
        #[cfg(test)]
        {
            self.last_close_audit = Some(audit);
        }
    }
}

//! Fork: the delegate menu (`supervisor_handoff` binding). The owner hands the focused pane
//! to the supervisor once, for a fixed time, or permanently, or takes it back. Every entry is
//! sent as a client-protocol control message; the server accepts it only from an active
//! attached client viewing that pane, so a bot can never delegate a pane to itself.

use super::*;
use crate::supervisor::delegation::{encode_control, DelegationChoice};

/// The server drops an expired delegation on its next tick; a snapshot that arrives this
/// soon after the expiry time still counts as an expiry, not a revoke.
const EXPIRY_SLACK_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientDelegateOverlay {
    pub(super) pane_id: String,
    pub(super) choices: Vec<DelegationChoice>,
    pub(super) cursor: usize,
    /// Current delegation of the pane when the menu opened (None = not delegated).
    pub(super) current: Option<crate::protocol::ClientShellDelegation>,
}

/// The delegate key in words, e.g. `ctrl+b, then shift+a`; None when unbound.
pub(super) fn handover_keys(keybinds: &LiveKeybindConfig) -> Option<String> {
    let label = keybinds
        .keybinds
        .supervisor_handoff
        .labels()
        .into_iter()
        .next()?;
    Some(match label.strip_prefix("prefix+") {
        Some(rest) => format!("{}, then {rest}", keybinds.primary_prefix_label()),
        None => label,
    })
}

pub(super) fn choice_label(choice: DelegationChoice) -> &'static str {
    match choice {
        DelegationChoice::Once => "hand over once (next write only)",
        DelegationChoice::Minutes30 => "delegate 30m",
        DelegationChoice::Hours2 => "delegate 2h",
        DelegationChoice::Hours8 => "delegate 8h",
        DelegationChoice::Permanent => "delegate permanently",
        DelegationChoice::Revoke => "revoke delegation",
    }
}

impl ClientShellState {
    /// The pane's delegation as last published by the server.
    pub(super) fn pane_delegation(
        &self,
        pane_id: &str,
    ) -> Option<crate::protocol::ClientShellDelegation> {
        self.snapshot
            .as_deref()?
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)?
            .delegation
    }

    pub(super) fn open_delegate_overlay(&mut self, outcome: &mut ClientShellInput) {
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        let current = self.pane_delegation(&pane_id);
        let mut choices = vec![
            DelegationChoice::Once,
            DelegationChoice::Minutes30,
            DelegationChoice::Hours2,
            DelegationChoice::Hours8,
            DelegationChoice::Permanent,
        ];
        if current.is_some() {
            choices.push(DelegationChoice::Revoke);
        }
        // A delegated pane opens on "revoke" (the likely intent); otherwise on 2h.
        let preselect = if current.is_some() {
            DelegationChoice::Revoke
        } else {
            DelegationChoice::Hours2
        };
        let cursor = choices
            .iter()
            .position(|choice| *choice == preselect)
            .unwrap_or(0);
        self.overlay = Some(ClientShellOverlay::Delegate(ClientDelegateOverlay {
            pane_id,
            choices,
            cursor,
            current,
        }));
        outcome.repaint = true;
    }

    pub(super) fn route_delegate_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::Delegate(menu)) = self.overlay.as_mut() else {
            return;
        };
        outcome.repaint = true;
        let last = menu.choices.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.accept_delegate(outcome),
            KeyCode::Up | KeyCode::Char('k') => menu.cursor = menu.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => menu.cursor = (menu.cursor + 1).min(last),
            _ => outcome.repaint = false,
        }
    }

    /// Snapshot install hook: a timed delegation that ran out (not one that was revoked)
    /// tells the owner the pane is theirs again.
    pub(super) fn note_expired_delegations(&mut self, next: &ClientShellSnapshot) {
        let Some(previous) = self.snapshot.as_deref() else {
            return;
        };
        if previous.boot_id != next.boot_id {
            return;
        }
        let now_ms = self.sidebar_clock().1;
        let mut expired = previous
            .panes
            .iter()
            .filter(|pane| {
                // Expired (allowing for the server's tick), not revoked early.
                pane.delegation
                    .and_then(|delegation| delegation.expires_at_ms)
                    .is_some_and(|expires| expires <= now_ms.saturating_add(EXPIRY_SLACK_MS))
            })
            .filter(|pane| {
                next.panes
                    .iter()
                    .find(|candidate| candidate.pane_id == pane.pane_id)
                    .is_some_and(|candidate| candidate.delegation.is_none())
            })
            .map(|pane| {
                let workspace = next
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == pane.workspace_id)
                    .map_or(pane.workspace_id.as_str(), |workspace| {
                        workspace.label.as_str()
                    });
                format!(
                    "delegation of {workspace}/{} expired — back to you",
                    pane.pane_id
                )
            });
        let Some(message) = expired.next() else {
            return;
        };
        let more = expired.count();
        let message = if more > 0 {
            format!("{message} (+{more} more)")
        } else {
            message
        };
        self.copy_feedback = Some(crate::app::state::CopyFeedback { message });
        self.copy_feedback_deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(8));
    }

    /// Snapshot install hook: a pane that newly shows the hand-over reminder (the
    /// supervisor was refused because the owner typed there) tells the owner which keys
    /// hand it over. Wins over the expiry notice when both arrive together. Like the expiry
    /// notice it compares with the previous snapshot of the same server boot only, so
    /// switching machines or reconnecting does not repeat it; the border and agent row
    /// keep showing the reminder either way.
    pub(super) fn note_handover_wanted(&mut self, next: &ClientShellSnapshot) {
        let Some(previous) = self
            .snapshot
            .as_deref()
            .filter(|previous| previous.boot_id == next.boot_id)
        else {
            return;
        };
        let mut waiting = next
            .panes
            .iter()
            .filter(|pane| pane.handover_wanted && pane.delegation.is_none())
            .filter(|pane| {
                !previous
                    .panes
                    .iter()
                    .any(|old| old.pane_id == pane.pane_id && old.handover_wanted)
            })
            .map(|pane| {
                let workspace = next
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == pane.workspace_id)
                    .map_or(pane.workspace_id.as_str(), |workspace| {
                        workspace.label.as_str()
                    });
                format!("{workspace}/{}", pane.pane_id)
            });
        let Some(first) = waiting.next() else {
            return;
        };
        let more = waiting.count();
        let panes = if more > 0 {
            format!("{first} (+{more} more)")
        } else {
            first
        };
        let message = match handover_keys(&self.config.keybinds) {
            // Key first: the toast is one unwrapped line.
            Some(keys) => format!("press {keys} to hand {panes} to the waiting bot"),
            None => {
                format!("a bot is waiting on {panes}: bind supervisor_handoff to hand it over")
            }
        };
        self.copy_feedback = Some(crate::app::state::CopyFeedback { message });
        self.copy_feedback_deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    }

    /// Sends the selected choice for the pane and closes the menu.
    pub(super) fn accept_delegate(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Delegate(menu)) = self.overlay.take() else {
            return;
        };
        let Some(choice) = menu.choices.get(menu.cursor).copied() else {
            return;
        };
        let (kind, data, message) = match choice {
            // The one-shot handoff keeps its own control so older servers still accept it.
            DelegationChoice::Once => (
                crate::supervisor::HANDOFF_CONTROL_KIND,
                menu.pane_id.clone(),
                format!("supervisor handoff sent for {}", menu.pane_id),
            ),
            DelegationChoice::Revoke => (
                crate::supervisor::DELEGATE_CONTROL_KIND,
                encode_control(choice, &menu.pane_id),
                format!("{} is back to you", menu.pane_id),
            ),
            _ => (
                crate::supervisor::DELEGATE_CONTROL_KIND,
                encode_control(choice, &menu.pane_id),
                format!(
                    "{} delegated to the supervisor ({})",
                    menu.pane_id,
                    choice.as_str()
                ),
            ),
        };
        self.copy_feedback = Some(crate::app::state::CopyFeedback { message });
        self.copy_feedback_deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
        outcome
            .requests
            .push(crate::protocol::ClientMessage::EndpointControl {
                kind: kind.into(),
                data,
            });
        outcome.repaint = true;
    }
}

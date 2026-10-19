//! Fork: owner-granted pane delegation.
//!
//! The owner hands a pane to the supervisor for a fixed time or permanently, from an
//! attached shell client only (`DELEGATE_CONTROL_KIND`); no API method or CLI command
//! grants one. While a delegation is active, the sticky owner-input latch no longer
//! blocks supervised writes to that pane. Every other guard stays: target_ref checks, the
//! profile draft check (a dirty or unreadable box still rejects) and the write fence (owner
//! keys during an in-flight write still trip it). Owner typing never revokes a delegation.
//!
//! The same table keeps a short-lived hand-over reminder: when a supervised write is
//! refused only because the owner typed in the pane, the attached client shows the
//! delegate key on that pane until the owner hands it over or the reminder lapses.

use std::collections::HashMap;

use crate::layout::PaneId;

const MINUTE_MS: u64 = 60_000;

/// How long a refused write keeps the hand-over reminder on its pane.
pub const HANDOVER_REMINDER_MS: u64 = 10 * MINUTE_MS;

/// What the owner picked in the delegate menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationChoice {
    /// Today's one-shot operator handoff: exactly one write, nothing stored here.
    Once,
    Minutes30,
    Hours2,
    Hours8,
    Permanent,
    Revoke,
}

impl DelegationChoice {
    pub const ALL: [Self; 6] = [
        Self::Once,
        Self::Minutes30,
        Self::Hours2,
        Self::Hours8,
        Self::Permanent,
        Self::Revoke,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Minutes30 => "30m",
            Self::Hours2 => "2h",
            Self::Hours8 => "8h",
            Self::Permanent => "permanent",
            Self::Revoke => "revoke",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|choice| choice.as_str() == value)
    }

    /// Lifetime of a timed delegation; None for permanent, once and revoke.
    pub fn duration_ms(self) -> Option<u64> {
        match self {
            Self::Minutes30 => Some(30 * MINUTE_MS),
            Self::Hours2 => Some(120 * MINUTE_MS),
            Self::Hours8 => Some(480 * MINUTE_MS),
            Self::Once | Self::Permanent | Self::Revoke => None,
        }
    }
}

/// Encodes a delegate control payload: `<choice> <public pane id>`.
pub fn encode_control(choice: DelegationChoice, pane_id: &str) -> String {
    format!("{} {pane_id}", choice.as_str())
}

/// Parses a delegate control payload; None when malformed.
pub fn decode_control(data: &str) -> Option<(DelegationChoice, &str)> {
    let (choice, pane) = data.split_once(' ')?;
    let choice = DelegationChoice::parse(choice)?;
    (!pane.is_empty() && !pane.contains(char::is_whitespace)).then_some((choice, pane))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    pub granted_at_ms: u64,
    /// None = permanent (until revoked or the pane closes).
    pub expires_at_ms: Option<u64>,
    /// Agent session id at grant time; a permanent delegation is restored after a server
    /// restart only when the restored pane reports this same session.
    pub agent_session: Option<String>,
}

impl Delegation {
    pub fn is_active(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_none_or(|expires| now_ms < expires)
    }

    pub fn permanent(&self) -> bool {
        self.expires_at_ms.is_none()
    }
}

/// A permanent delegation waiting for its pane's agent to report the saved session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRestore {
    pub agent_session: String,
    pub granted_at_ms: u64,
}

/// Per-pane delegations of one server run. Kept outside the per-pane supervisor state so a
/// generation bump (the bot's own `reset`, a session change) does not end a delegation.
#[derive(Debug, Default)]
pub struct Delegations {
    active: HashMap<PaneId, Delegation>,
    pending_restore: HashMap<PaneId, PendingRestore>,
    /// Time of the newest write refused because the owner typed in the pane.
    handover_wanted: HashMap<PaneId, u64>,
}

impl Delegations {
    /// Grants (or replaces) a timed or permanent delegation. `Once` and `Revoke` are not
    /// grants and are ignored here; callers route them elsewhere.
    pub fn grant(
        &mut self,
        pane: PaneId,
        choice: DelegationChoice,
        now_ms: u64,
        agent_session: Option<String>,
    ) -> Option<&Delegation> {
        let expires_at_ms = match choice {
            DelegationChoice::Permanent => None,
            DelegationChoice::Once | DelegationChoice::Revoke => return None,
            timed => Some(now_ms.saturating_add(timed.duration_ms()?)),
        };
        self.pending_restore.remove(&pane);
        self.handover_wanted.remove(&pane);
        self.active.insert(
            pane,
            Delegation {
                granted_at_ms: now_ms,
                expires_at_ms,
                agent_session,
            },
        );
        self.active.get(&pane)
    }

    /// Ends the pane's delegation; true when one was active or pending restore.
    pub fn revoke(&mut self, pane: PaneId) -> bool {
        let pending = self.pending_restore.remove(&pane).is_some();
        self.active.remove(&pane).is_some() || pending
    }

    /// The pane's delegation when it is active at `now_ms`.
    pub fn active(&self, pane: PaneId, now_ms: u64) -> Option<&Delegation> {
        self.active
            .get(&pane)
            .filter(|delegation| delegation.is_active(now_ms))
    }

    /// Drops every delegation that has expired by `now_ms` and returns their panes.
    pub fn expire_due(&mut self, now_ms: u64) -> Vec<PaneId> {
        let expired = self
            .active
            .iter()
            .filter(|(_, delegation)| !delegation.is_active(now_ms))
            .map(|(pane, _)| *pane)
            .collect::<Vec<_>>();
        for pane in &expired {
            self.active.remove(pane);
        }
        expired
    }

    /// A supervised write to `pane` was refused only because the owner typed there.
    /// Returns true when the pane's reminder was not showing before.
    pub fn note_handover_wanted(&mut self, pane: PaneId, now_ms: u64) -> bool {
        let was_showing = self.handover_wanted(pane, now_ms);
        self.handover_wanted.insert(pane, now_ms);
        !was_showing
    }

    /// The owner handed the pane over once; true when a reminder entry was removed. A
    /// lapsed entry the expiry tick has not dropped yet still counts, so the caller
    /// rebuilds the snapshot that may still show it.
    pub fn clear_handover_wanted(&mut self, pane: PaneId) -> bool {
        self.handover_wanted.remove(&pane).is_some()
    }

    /// True while the pane shows the hand-over reminder: a refused write within
    /// `HANDOVER_REMINDER_MS` and no active delegation.
    pub fn handover_wanted(&self, pane: PaneId, now_ms: u64) -> bool {
        self.handover_wanted
            .get(&pane)
            .is_some_and(|at| now_ms < at.saturating_add(HANDOVER_REMINDER_MS))
            && self.active(pane, now_ms).is_none()
    }

    /// Drops lapsed reminders; true when any was dropped.
    pub fn expire_handover_reminders(&mut self, now_ms: u64) -> bool {
        let before = self.handover_wanted.len();
        self.handover_wanted
            .retain(|_, at| now_ms < at.saturating_add(HANDOVER_REMINDER_MS));
        self.handover_wanted.len() != before
    }

    pub fn remove_pane(&mut self, pane: PaneId) {
        self.active.remove(&pane);
        self.pending_restore.remove(&pane);
        self.handover_wanted.remove(&pane);
    }

    /// Permanent delegations with a known agent session, for the session save.
    pub fn persistable(&self) -> impl Iterator<Item = (PaneId, &Delegation)> {
        self.active
            .iter()
            .filter(|(_, delegation)| delegation.permanent() && delegation.agent_session.is_some())
            .map(|(pane, delegation)| (*pane, delegation))
    }

    /// A restored pane had a permanent delegation; it becomes active only once the pane's
    /// agent reports the same session (`note_session`).
    pub fn queue_restore(&mut self, pane: PaneId, restore: PendingRestore) {
        self.pending_restore.insert(pane, restore);
    }

    /// The pane's agent reported `session`: a pending permanent delegation for that same
    /// session becomes active; a different session drops it. Returns true when restored.
    pub fn note_session(&mut self, pane: PaneId, session: &str) -> bool {
        let Some(pending) = self.pending_restore.remove(&pane) else {
            return false;
        };
        if pending.agent_session != session {
            return false;
        }
        self.active.insert(
            pane,
            Delegation {
                granted_at_ms: pending.granted_at_ms,
                expires_at_ms: None,
                agent_session: Some(pending.agent_session),
            },
        );
        true
    }

    #[cfg(test)]
    pub fn pending_restore(&self, pane: PaneId) -> Option<&PendingRestore> {
        self.pending_restore.get(&pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u32) -> PaneId {
        PaneId::from_raw(n)
    }

    #[test]
    fn control_payload_round_trips_and_rejects_malformed() {
        for choice in DelegationChoice::ALL {
            let data = encode_control(choice, "w1:p2");
            assert_eq!(decode_control(&data), Some((choice, "w1:p2")));
        }
        assert_eq!(decode_control("2h"), None);
        assert_eq!(decode_control("2h "), None);
        assert_eq!(decode_control("forever w1:p1"), None);
        assert_eq!(decode_control("2h w1 p1"), None);
    }

    #[test]
    fn timed_permanent_and_expiry_math() {
        let mut d = Delegations::default();
        let granted = d
            .grant(pid(1), DelegationChoice::Minutes30, 1_000, None)
            .cloned()
            .unwrap();
        assert_eq!(granted.expires_at_ms, Some(1_000 + 30 * MINUTE_MS));
        assert!(d.active(pid(1), 1_000 + 30 * MINUTE_MS - 1).is_some());
        assert!(
            d.active(pid(1), 1_000 + 30 * MINUTE_MS).is_none(),
            "expiry is exclusive"
        );

        d.grant(pid(2), DelegationChoice::Hours2, 0, None);
        d.grant(pid(3), DelegationChoice::Hours8, 0, None);
        d.grant(pid(4), DelegationChoice::Permanent, 0, None);
        assert_eq!(
            d.active(pid(2), 0).unwrap().expires_at_ms,
            Some(120 * MINUTE_MS)
        );
        assert_eq!(
            d.active(pid(3), 0).unwrap().expires_at_ms,
            Some(480 * MINUTE_MS)
        );
        assert!(
            d.active(pid(4), u64::MAX).is_some(),
            "permanent never expires"
        );

        let mut expired = d.expire_due(121 * MINUTE_MS);
        expired.sort_by_key(|pane| pane.raw());
        assert_eq!(expired, vec![pid(1), pid(2)]);
        assert!(d.active(pid(3), 121 * MINUTE_MS).is_some());
        assert!(d.expire_due(121 * MINUTE_MS).is_empty(), "expired once");
    }

    #[test]
    fn once_and_revoke_are_not_grants() {
        let mut d = Delegations::default();
        assert!(d.grant(pid(1), DelegationChoice::Once, 0, None).is_none());
        assert!(d.grant(pid(1), DelegationChoice::Revoke, 0, None).is_none());
        assert!(d.active(pid(1), 0).is_none());
        d.grant(pid(1), DelegationChoice::Permanent, 0, None);
        assert!(d.revoke(pid(1)));
        assert!(!d.revoke(pid(1)));
        assert!(d.active(pid(1), 0).is_none());
    }

    #[test]
    fn pane_close_ends_delegation() {
        let mut d = Delegations::default();
        d.grant(pid(1), DelegationChoice::Hours2, 0, None);
        d.remove_pane(pid(1));
        assert!(d.active(pid(1), 0).is_none());
    }

    #[test]
    fn handover_reminder_shows_until_granted_cleared_or_lapsed() {
        let mut d = Delegations::default();
        assert!(!d.handover_wanted(pid(1), 0));
        assert!(d.note_handover_wanted(pid(1), 1_000), "newly showing");
        assert!(!d.note_handover_wanted(pid(1), 2_000), "already showing");
        assert!(d.handover_wanted(pid(1), 2_000 + HANDOVER_REMINDER_MS - 1));
        assert!(!d.handover_wanted(pid(1), 2_000 + HANDOVER_REMINDER_MS));
        assert!(!d.expire_handover_reminders(2_000));
        assert!(d.expire_handover_reminders(2_000 + HANDOVER_REMINDER_MS));
        assert!(d.note_handover_wanted(pid(1), 3_000), "lapsed shows again");

        d.grant(pid(1), DelegationChoice::Hours2, 3_000, None);
        assert!(!d.handover_wanted(pid(1), 3_000), "a grant clears it");
        d.revoke(pid(1));
        assert!(!d.handover_wanted(pid(1), 3_000), "and it stays cleared");

        d.note_handover_wanted(pid(2), 0);
        assert!(d.clear_handover_wanted(pid(2)));
        assert!(!d.clear_handover_wanted(pid(2)));
        // A lapsed entry not yet dropped by the tick still asks for a rebuild.
        d.note_handover_wanted(pid(2), 0);
        assert!(d.clear_handover_wanted(pid(2)));
        d.note_handover_wanted(pid(3), 0);
        d.remove_pane(pid(3));
        assert!(!d.handover_wanted(pid(3), 1));
    }

    #[test]
    fn only_permanent_with_session_persists_and_restore_needs_the_same_session() {
        let mut d = Delegations::default();
        d.grant(pid(1), DelegationChoice::Hours2, 0, Some("s-1".into()));
        d.grant(pid(2), DelegationChoice::Permanent, 0, None);
        d.grant(pid(3), DelegationChoice::Permanent, 5, Some("s-3".into()));
        let saved = d.persistable().map(|(pane, _)| pane).collect::<Vec<_>>();
        assert_eq!(saved, vec![pid(3)]);

        let mut restored = Delegations::default();
        let pending = PendingRestore {
            agent_session: "s-3".into(),
            granted_at_ms: 5,
        };
        restored.queue_restore(pid(3), pending.clone());
        assert!(
            restored.active(pid(3), 10).is_none(),
            "not before the session reports"
        );
        assert!(
            !restored.note_session(pid(3), "other"),
            "different session drops it"
        );
        assert!(restored.pending_restore(pid(3)).is_none());
        assert!(restored.active(pid(3), 10).is_none());

        restored.queue_restore(pid(3), pending);
        assert!(restored.note_session(pid(3), "s-3"));
        let active = restored.active(pid(3), u64::MAX).unwrap();
        assert!(active.permanent());
        assert_eq!(active.agent_session.as_deref(), Some("s-3"));
    }
}

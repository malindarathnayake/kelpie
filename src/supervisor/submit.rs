//! Fork-only `agent.submit` receipt ledger (S2/S3). Pure bookkeeping: no App access, no I/O.
//! Receipts never hold the submitted text, only a 16-hex SHA-256 prefix of it.

use std::collections::HashMap;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::api::schema::supervisor::{
    SubmitAcknowledged, SubmitReason, SubmitReceipt, SubmitState, TargetRef,
};
use crate::layout::PaneId;
use crate::supervisor::fence::{is_fenced_error, FenceReason, FenceSnapshot, SubmitFence};

/// Receipts kept per server process. Nothing is evicted; a new id at the cap is refused.
pub const MAX_SUBMIT_RECEIPTS: usize = 256;
pub const MAX_SUBMIT_ID_CHARS: usize = 64;
pub const MAX_SUBMIT_TEXT_BYTES: usize = 8192;
/// Write deadline measured from acceptance.
pub const SUBMIT_DEADLINE: Duration = Duration::from_secs(5);

/// `[A-Za-z0-9._:-]{1,64}`.
pub fn valid_submit_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SUBMIT_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// 1..=8192 bytes.
pub fn valid_submit_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_SUBMIT_TEXT_BYTES
}

/// First 16 lowercase hex chars of SHA-256 over the text bytes.
pub fn text_sha256_16(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Write-completion event posted by the waiter thread to the App.
#[derive(Debug)]
pub enum SubmitEvent {
    Completed {
        submit_id: String,
        result: std::io::Result<()>,
        fence: FenceSnapshot,
    },
    /// Debug builds only: the dev hold elapsed; queue the held write now.
    #[cfg(debug_assertions)]
    Release { submit_id: String },
}

/// Encoded write parked by the debug-only hold knob. Transient; dropped once queued.
#[cfg(debug_assertions)]
#[derive(Debug)]
pub struct HeldWrite {
    pub text: bytes::Bytes,
    pub enter: bytes::Bytes,
    pub deadline: std::time::Instant,
}

/// Result of looking up a submit id before validation.
#[derive(Debug, Clone, PartialEq)]
pub enum Repeat {
    New,
    Same(Box<SubmitReceipt>),
    Conflict,
}

#[derive(Debug)]
struct SubmitEntry {
    receipt: SubmitReceipt,
    pane: PaneId,
    fence: SubmitFence,
    /// Registry event sequence at which the write was confirmed.
    written_seq: Option<u64>,
    /// Time of a turn start seen after the writer committed the fence but before the
    /// completion event was handled; promotes the completion straight to `started`.
    start_candidate_ms: Option<u64>,
    #[cfg(debug_assertions)]
    held: Option<HeldWrite>,
    #[cfg(debug_assertions)]
    handoff: Option<CompletionHandoff>,
}

/// Hands the PTY completion receiver to the waiter thread.
pub type CompletionHandoff =
    std::sync::mpsc::Sender<std::sync::mpsc::Receiver<std::io::Result<()>>>;

#[derive(Debug, Default)]
pub struct SubmitLedger {
    entries: HashMap<String, SubmitEntry>,
    #[cfg(test)]
    cap_override: Option<usize>,
}

fn fence_reason(reason: FenceReason, during_write: bool) -> SubmitReason {
    match (reason, during_write) {
        (FenceReason::OwnerInput, false) => SubmitReason::OwnerInput,
        (FenceReason::GenerationChanged, false) => SubmitReason::GenerationChanged,
        (FenceReason::PaneClosed, false) => SubmitReason::PaneClosed,
        (FenceReason::OwnerInput, true) => SubmitReason::OwnerInputDuringWrite,
        (FenceReason::GenerationChanged, true) => SubmitReason::GenerationChangedDuringWrite,
        (FenceReason::PaneClosed, true) => SubmitReason::PaneClosedDuringWrite,
    }
}

impl SubmitLedger {
    fn cap(&self) -> usize {
        #[cfg(test)]
        if let Some(cap) = self.cap_override {
            return cap;
        }
        MAX_SUBMIT_RECEIPTS
    }

    #[cfg(test)]
    pub fn set_cap_for_test(&mut self, cap: usize) {
        self.cap_override = Some(cap);
    }

    pub fn get(&self, submit_id: &str) -> Option<&SubmitReceipt> {
        self.entries.get(submit_id).map(|e| &e.receipt)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.cap()
    }

    /// Same id with the same target and text returns the stored receipt; any difference
    /// is a conflict.
    pub fn repeat(&self, submit_id: &str, target_ref: &TargetRef, text_sha256_16: &str) -> Repeat {
        match self.entries.get(submit_id) {
            None => Repeat::New,
            Some(e)
                if e.receipt.target_ref == *target_ref
                    && e.receipt.text_sha256_16 == text_sha256_16 =>
            {
                Repeat::Same(Box::new(e.receipt.clone()))
            }
            Some(_) => Repeat::Conflict,
        }
    }

    /// Reserves an `accepted` receipt. None when the id exists or the ledger is full.
    pub fn insert_accepted(
        &mut self,
        submit_id: &str,
        pane: PaneId,
        target_ref: TargetRef,
        text_sha256_16: String,
        fence: SubmitFence,
        handoff: bool,
        now_ms: u64,
    ) -> Option<SubmitReceipt> {
        if self.is_full() || self.entries.contains_key(submit_id) {
            return None;
        }
        let receipt = SubmitReceipt {
            submit_id: submit_id.to_string(),
            target_ref,
            state: SubmitState::Accepted,
            accepted_at_ms: now_ms,
            written_at_ms: None,
            started_at_ms: None,
            reason: None,
            acknowledged: SubmitAcknowledged::Unsupported,
            text_sha256_16,
            handoff,
        };
        self.entries.insert(
            submit_id.to_string(),
            SubmitEntry {
                receipt: receipt.clone(),
                pane,
                fence,
                written_seq: None,
                start_candidate_ms: None,
                #[cfg(debug_assertions)]
                held: None,
                #[cfg(debug_assertions)]
                handoff: None,
            },
        );
        Some(receipt)
    }

    /// The queue call itself failed: nothing was handed to the writer. A fence tripped
    /// before that point reads `fenced`; otherwise the receipt is `indeterminate`.
    pub fn mark_queue_failed(&mut self, submit_id: &str) -> Option<SubmitReceipt> {
        let entry = self.entries.get_mut(submit_id)?;
        let fence = entry.fence.close();
        if entry.receipt.state == SubmitState::Accepted {
            match fence.reason {
                Some(reason) => {
                    entry.receipt.state = SubmitState::Fenced;
                    entry.receipt.reason = Some(fence_reason(reason, false));
                }
                None => {
                    entry.receipt.state = SubmitState::Indeterminate;
                    entry.receipt.reason = Some(SubmitReason::QueueFailed);
                }
            }
        }
        Some(entry.receipt.clone())
    }

    /// Applies the writer's result to an `accepted` receipt. Later results are ignored.
    pub fn complete(
        &mut self,
        submit_id: &str,
        result: &std::io::Result<()>,
        fence: FenceSnapshot,
        seq: u64,
        now_ms: u64,
    ) {
        let Some(entry) = self.entries.get_mut(submit_id) else {
            return;
        };
        if entry.receipt.state != SubmitState::Accepted {
            return;
        }
        let r = &mut entry.receipt;
        match result {
            Ok(()) => match fence.reason {
                None => {
                    r.state = SubmitState::Written;
                    r.written_at_ms = Some(now_ms);
                    entry.written_seq = Some(seq);
                    // A turn start handled between the writer's commit and this event
                    // (the completion is ordered only when the App handles it).
                    if let Some(started_at) = entry.start_candidate_ms {
                        r.state = SubmitState::Started;
                        r.started_at_ms = Some(started_at);
                    }
                }
                Some(reason) => {
                    // Bytes were written while the fence tripped: the outcome is unknown.
                    // This includes a trip after the Enter flush but before the waiter
                    // closed the fence: conservatively `indeterminate{*_during_write}`.
                    r.state = SubmitState::Indeterminate;
                    r.reason = Some(fence_reason(reason, true));
                }
            },
            Err(err) if is_fenced_error(err) => {
                r.state = SubmitState::Fenced;
                r.reason = Some(
                    fence
                        .reason
                        .map_or(SubmitReason::Unknown, |reason| fence_reason(reason, false)),
                );
            }
            Err(err) => {
                r.state = SubmitState::Indeterminate;
                r.reason = Some(match err.kind() {
                    std::io::ErrorKind::TimedOut => SubmitReason::DeadlineExpired,
                    std::io::ErrorKind::BrokenPipe => SubmitReason::PtyClosed,
                    _ => SubmitReason::WriteFailed,
                });
            }
        }
    }

    /// Trips the fences of this pane's in-flight submissions.
    pub fn trip_pane(&mut self, pane: PaneId, reason: FenceReason) {
        for entry in self
            .entries
            .values()
            .filter(|e| e.pane == pane && e.receipt.state == SubmitState::Accepted)
        {
            entry.fence.trip(reason);
        }
    }

    /// A turn started (Working edge or accepted Working hook report) at registry event
    /// `seq`: the first such event after a confirmed write marks the receipt `started`.
    /// For a receipt still `accepted` whose fence the writer already committed (and that is
    /// untripped), the event is kept as a start candidate and applied by `complete`.
    ///
    /// Limits: a submit sent while the agent is already Working produces no new edge, so the
    /// receipt may stay `written` and never reach `started`; nothing correlates the turn
    /// with this submission beyond event order.
    pub fn note_turn_start(&mut self, pane: PaneId, seq: u64, now_ms: u64) {
        for entry in self.entries.values_mut().filter(|e| e.pane == pane) {
            match entry.receipt.state {
                SubmitState::Written if entry.written_seq.is_some_and(|w| w < seq) => {
                    entry.receipt.state = SubmitState::Started;
                    entry.receipt.started_at_ms = Some(now_ms);
                }
                SubmitState::Accepted
                    if entry.start_candidate_ms.is_none()
                        && entry.fence.is_committed()
                        && !entry.fence.is_tripped() =>
                {
                    entry.start_candidate_ms = Some(now_ms);
                }
                _ => {}
            }
        }
    }

    #[cfg(debug_assertions)]
    pub fn fence(&self, submit_id: &str) -> Option<&SubmitFence> {
        self.entries.get(submit_id).map(|e| &e.fence)
    }

    #[cfg(debug_assertions)]
    pub fn pane(&self, submit_id: &str) -> Option<PaneId> {
        self.entries.get(submit_id).map(|e| e.pane)
    }

    #[cfg(debug_assertions)]
    pub fn hold(&mut self, submit_id: &str, write: HeldWrite) {
        if let Some(entry) = self.entries.get_mut(submit_id) {
            entry.held = Some(write);
        }
    }

    #[cfg(debug_assertions)]
    pub fn attach_handoff(&mut self, submit_id: &str, handoff: CompletionHandoff) {
        if let Some(entry) = self.entries.get_mut(submit_id) {
            entry.handoff = Some(handoff);
        }
    }

    #[cfg(debug_assertions)]
    pub fn take_handoff(&mut self, submit_id: &str) -> Option<CompletionHandoff> {
        self.entries
            .get_mut(submit_id)
            .and_then(|e| e.handoff.take())
    }

    #[cfg(debug_assertions)]
    pub fn take_held(&mut self, submit_id: &str) -> Option<HeldWrite> {
        self.entries.get_mut(submit_id).and_then(|e| e.held.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(generation: u64) -> TargetRef {
        TargetRef {
            server_epoch: "e".into(),
            pane_id: "p1".into(),
            generation,
        }
    }

    fn pid(n: u32) -> PaneId {
        PaneId::from_raw(n)
    }

    fn accept(ledger: &mut SubmitLedger, id: &str, text: &str) -> SubmitFence {
        let fence = SubmitFence::new();
        ledger
            .insert_accepted(
                id,
                pid(1),
                target(1),
                text_sha256_16(text),
                fence.clone(),
                false,
                10,
            )
            .expect("accepted");
        fence
    }

    #[test]
    fn submit_id_and_text_bounds() {
        assert!(valid_submit_id("a"));
        assert!(valid_submit_id("run-1.step_2:x"));
        assert!(valid_submit_id(&"x".repeat(64)));
        assert!(!valid_submit_id(""));
        assert!(!valid_submit_id(&"x".repeat(65)));
        assert!(!valid_submit_id("has space"));
        assert!(!valid_submit_id("emoji\u{1F600}"));
        assert!(valid_submit_text("x"));
        assert!(valid_submit_text(&"x".repeat(MAX_SUBMIT_TEXT_BYTES)));
        assert!(!valid_submit_text(""));
        assert!(!valid_submit_text(&"x".repeat(MAX_SUBMIT_TEXT_BYTES + 1)));
    }

    #[test]
    fn text_hash_is_16_hex_and_stable() {
        let h = text_sha256_16("hello");
        assert_eq!(h, "2cf24dba5fb0a30e");
        assert_eq!(h, text_sha256_16("hello"));
    }

    #[test]
    fn repeat_same_returns_receipt_and_difference_conflicts() {
        let mut l = SubmitLedger::default();
        accept(&mut l, "s1", "hello");
        let Repeat::Same(r) = l.repeat("s1", &target(1), &text_sha256_16("hello")) else {
            panic!("expected same");
        };
        assert_eq!(r.state, SubmitState::Accepted);
        assert_eq!(r.acknowledged, SubmitAcknowledged::Unsupported);
        assert_eq!(
            l.repeat("s1", &target(1), &text_sha256_16("other")),
            Repeat::Conflict
        );
        assert_eq!(
            l.repeat("s1", &target(2), &text_sha256_16("hello")),
            Repeat::Conflict
        );
        assert_eq!(
            l.repeat("s2", &target(1), &text_sha256_16("hello")),
            Repeat::New
        );
    }

    #[test]
    fn ledger_cap_refuses_new_ids_without_eviction() {
        let mut l = SubmitLedger::default();
        l.set_cap_for_test(2);
        accept(&mut l, "a", "x");
        accept(&mut l, "b", "x");
        assert!(l.is_full());
        assert!(l
            .insert_accepted(
                "c",
                pid(1),
                target(1),
                text_sha256_16("x"),
                SubmitFence::new(),
                false,
                1
            )
            .is_none());
        assert!(l.get("a").is_some() && l.get("b").is_some());
        assert!(l.get("c").is_none());
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn completion_maps_results_to_states() {
        let mut l = SubmitLedger::default();
        let clean = FenceSnapshot {
            committed: true,
            reason: None,
        };
        accept(&mut l, "ok", "x");
        l.complete("ok", &Ok(()), clean, 5, 20);
        let r = l.get("ok").unwrap();
        assert_eq!((r.state, r.written_at_ms), (SubmitState::Written, Some(20)));

        let f = accept(&mut l, "fenced", "x");
        f.trip(FenceReason::OwnerInput);
        let snap = f.close();
        l.complete(
            "fenced",
            &Err(crate::supervisor::fence::fenced_error()),
            snap,
            6,
            21,
        );
        let r = l.get("fenced").unwrap();
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Fenced, Some(SubmitReason::OwnerInput))
        );

        accept(&mut l, "late", "x");
        let late = FenceSnapshot {
            committed: true,
            reason: Some(FenceReason::GenerationChanged),
        };
        l.complete("late", &Ok(()), late, 7, 22);
        let r = l.get("late").unwrap();
        assert_eq!(
            (r.state, r.reason),
            (
                SubmitState::Indeterminate,
                Some(SubmitReason::GenerationChangedDuringWrite)
            )
        );

        for (id, kind, reason) in [
            (
                "timeout",
                std::io::ErrorKind::TimedOut,
                SubmitReason::DeadlineExpired,
            ),
            (
                "closed",
                std::io::ErrorKind::BrokenPipe,
                SubmitReason::PtyClosed,
            ),
            (
                "other",
                std::io::ErrorKind::Other,
                SubmitReason::WriteFailed,
            ),
        ] {
            accept(&mut l, id, "x");
            l.complete(id, &Err(std::io::Error::new(kind, "e")), clean, 8, 23);
            let r = l.get(id).unwrap();
            assert_eq!(
                (r.state, r.reason),
                (SubmitState::Indeterminate, Some(reason))
            );
        }

        // A second result never rewrites a settled receipt.
        l.complete("ok", &Err(std::io::Error::other("late")), clean, 9, 24);
        assert_eq!(l.get("ok").unwrap().state, SubmitState::Written);
    }

    #[test]
    fn queue_failure_is_indeterminate() {
        let mut l = SubmitLedger::default();
        accept(&mut l, "q", "x");
        let r = l.mark_queue_failed("q").unwrap();
        assert_eq!(
            (r.state, r.reason),
            (SubmitState::Indeterminate, Some(SubmitReason::QueueFailed))
        );
    }

    #[test]
    fn started_needs_a_turn_after_the_write() {
        let mut l = SubmitLedger::default();
        accept(&mut l, "s", "x");
        let clean = FenceSnapshot {
            committed: true,
            reason: None,
        };
        l.note_turn_start(pid(1), 3, 30);
        assert_eq!(l.get("s").unwrap().state, SubmitState::Accepted);
        l.complete("s", &Ok(()), clean, 5, 31);
        l.note_turn_start(pid(1), 4, 32);
        assert_eq!(l.get("s").unwrap().state, SubmitState::Written);
        l.note_turn_start(pid(2), 6, 33);
        assert_eq!(l.get("s").unwrap().state, SubmitState::Written);
        l.note_turn_start(pid(1), 6, 34);
        let r = l.get("s").unwrap();
        assert_eq!((r.state, r.started_at_ms), (SubmitState::Started, Some(34)));
        l.note_turn_start(pid(1), 7, 35);
        assert_eq!(l.get("s").unwrap().started_at_ms, Some(34));
    }

    #[test]
    fn turn_start_between_commit_and_completion_marks_started() {
        let mut l = SubmitLedger::default();
        let f = accept(&mut l, "race", "x");
        // Before the writer commits, a turn start is not attributed to this submission.
        l.note_turn_start(pid(1), 2, 40);
        assert!(f.try_commit());
        l.note_turn_start(pid(1), 3, 41);
        l.note_turn_start(pid(1), 4, 42);
        assert_eq!(l.get("race").unwrap().state, SubmitState::Accepted);
        let snap = f.close();
        l.complete("race", &Ok(()), snap, 5, 43);
        let r = l.get("race").unwrap();
        assert_eq!(
            (r.state, r.written_at_ms, r.started_at_ms),
            (SubmitState::Started, Some(43), Some(41))
        );

        // A tripped fence never records a candidate.
        let g = accept(&mut l, "tripped", "x");
        assert!(g.try_commit());
        g.trip(FenceReason::OwnerInput);
        l.note_turn_start(pid(1), 6, 44);
        l.complete("tripped", &Ok(()), g.close(), 7, 45);
        let r = l.get("tripped").unwrap();
        assert_eq!(r.state, SubmitState::Indeterminate);
        assert_eq!(r.started_at_ms, None);

        // No candidate: plain written.
        let h = accept(&mut l, "plain", "x");
        assert!(h.try_commit());
        l.complete("plain", &Ok(()), h.close(), 8, 46);
        assert_eq!(l.get("plain").unwrap().state, SubmitState::Written);
    }

    #[test]
    fn handoff_flag_is_recorded_on_the_receipt() {
        let mut l = SubmitLedger::default();
        let r = l
            .insert_accepted(
                "h",
                pid(1),
                target(1),
                text_sha256_16("x"),
                SubmitFence::new(),
                true,
                1,
            )
            .unwrap();
        assert!(r.handoff);
        accept(&mut l, "plain-h", "x");
        assert!(!l.get("plain-h").unwrap().handoff);
    }

    #[test]
    fn trip_pane_trips_only_in_flight_fences_of_that_pane() {
        let mut l = SubmitLedger::default();
        let f = accept(&mut l, "a", "x");
        let other = SubmitFence::new();
        l.insert_accepted(
            "b",
            pid(2),
            target(1),
            text_sha256_16("x"),
            other.clone(),
            false,
            1,
        );
        l.trip_pane(pid(1), FenceReason::GenerationChanged);
        assert!(!f.try_commit());
        assert!(other.try_commit());
    }
}

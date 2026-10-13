//! Fork-only receipt ledger for `agent.submit`, `agent.answer`, `agent.interrupt` and
//! `agent.reset` (S2/S3, S7-S9). Pure bookkeeping: no App access, no I/O. Receipts never
//! hold the submitted text, only a 16-hex SHA-256 prefix of it.

use std::collections::HashMap;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::agent_resume::AgentSessionRef;
use crate::api::schema::supervisor::{
    ReceiptSession, ResetEvidence, SubmitAcknowledged, SubmitKind, SubmitReason, SubmitReceipt,
    SubmitState, TargetRef,
};
use crate::layout::PaneId;
use crate::supervisor::fence::{is_fenced_error, FenceReason, FenceSnapshot, SubmitFence};

/// Receipts kept per server process. Nothing is evicted; a new id at the cap is refused.
pub const MAX_SUBMIT_RECEIPTS: usize = 256;
pub const MAX_SUBMIT_ID_CHARS: usize = 64;
pub const MAX_SUBMIT_TEXT_BYTES: usize = 8192;
/// Write deadline measured from acceptance.
pub const SUBMIT_DEADLINE: Duration = Duration::from_secs(5);
/// Text `agent.reset` submits through the submit path.
pub const RESET_TEXT: &str = "/clear";
/// Logical text hashed into an `agent.interrupt` receipt: the single Esc key.
pub const INTERRUPT_HASH_TEXT: &str = "\u{1b}";

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
    pub delay: Duration,
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
    /// `interrupt`: time of an Idle/AwaitingHuman edge seen between the writer's commit
    /// and the completion event; applied as `effect_observed_at_ms` by `complete`.
    effect_candidate_ms: Option<u64>,
    /// `reset`: the next generation-changed trip of this fence is the expected clear
    /// SessionChanged bump (armed only while committed and untripped).
    expected_reset_trip: bool,
    /// `reset`: clear session observed between the writer's commit and completion.
    reset_candidate: Option<(ReceiptSession, u64)>,
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

/// What a new request asks for; compared against a known submit_id.
#[derive(Debug, Clone, Copy)]
pub struct RequestKey<'a> {
    pub kind: SubmitKind,
    pub question_id: Option<&'a str>,
    pub target_ref: &'a TargetRef,
    pub text_sha256_16: &'a str,
}

/// Fields of a newly accepted receipt.
#[derive(Debug, Clone)]
pub struct NewReceipt {
    pub kind: SubmitKind,
    pub question_id: Option<String>,
    pub pane: PaneId,
    pub target_ref: TargetRef,
    pub text_sha256_16: String,
    pub handoff: bool,
}

fn session_wire(session: &AgentSessionRef) -> ReceiptSession {
    ReceiptSession {
        kind: session.kind,
        value: session.value.clone(),
    }
}

/// A reset that ended `fenced` or `indeterminate` was not cleanly written: never verified.
fn unverify_unwritten_reset(r: &mut SubmitReceipt) {
    if r.kind == SubmitKind::Reset
        && matches!(r.state, SubmitState::Fenced | SubmitState::Indeterminate)
    {
        r.reset = Some(ResetEvidence::Unverified);
        r.new_session = None;
        r.new_generation = None;
    }
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

    /// Same id with the same kind, question, target and text returns the stored receipt;
    /// any difference is a conflict.
    pub fn repeat(&self, submit_id: &str, key: RequestKey<'_>) -> Repeat {
        match self.entries.get(submit_id) {
            None => Repeat::New,
            Some(e)
                if e.receipt.kind == key.kind
                    && e.receipt.question_id.as_deref() == key.question_id
                    && e.receipt.target_ref == *key.target_ref
                    && e.receipt.text_sha256_16 == key.text_sha256_16 =>
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
        new: NewReceipt,
        fence: SubmitFence,
        now_ms: u64,
    ) -> Option<SubmitReceipt> {
        if self.is_full() || self.entries.contains_key(submit_id) {
            return None;
        }
        let receipt = SubmitReceipt {
            submit_id: submit_id.to_string(),
            target_ref: new.target_ref,
            state: SubmitState::Accepted,
            accepted_at_ms: now_ms,
            written_at_ms: None,
            started_at_ms: None,
            reason: None,
            acknowledged: SubmitAcknowledged::Unsupported,
            text_sha256_16: new.text_sha256_16,
            handoff: new.handoff,
            kind: new.kind,
            question_id: new.question_id,
            effect_observed_at_ms: None,
            reset: (new.kind == SubmitKind::Reset).then_some(ResetEvidence::Pending),
            new_session: None,
            new_generation: None,
        };
        self.entries.insert(
            submit_id.to_string(),
            SubmitEntry {
                receipt: receipt.clone(),
                pane: new.pane,
                fence,
                written_seq: None,
                start_candidate_ms: None,
                effect_candidate_ms: None,
                expected_reset_trip: false,
                reset_candidate: None,
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
            unverify_unwritten_reset(&mut entry.receipt);
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
        // Reset: the expected clear SessionChanged bump tripped the fence after the first
        // byte. That bump is the intended effect, not an unknown outcome. Any other late
        // trip (including a generation trip not armed by `note_clear_session`) stays
        // indeterminate.
        let expected_reset_trip = r.kind == SubmitKind::Reset
            && entry.expected_reset_trip
            && fence.committed
            && fence.reason == Some(FenceReason::GenerationChanged);
        match result {
            Ok(()) if fence.reason.is_none() || expected_reset_trip => {
                r.state = SubmitState::Written;
                r.written_at_ms = Some(now_ms);
                entry.written_seq = Some(seq);
                // A turn start handled between the writer's commit and this event
                // (the completion is ordered only when the App handles it).
                if let Some(started_at) = entry.start_candidate_ms {
                    r.state = SubmitState::Started;
                    r.started_at_ms = Some(started_at);
                }
                if let Some(effect_at) = entry.effect_candidate_ms {
                    r.effect_observed_at_ms = Some(effect_at);
                }
                if let Some((session, generation)) = entry.reset_candidate.take() {
                    r.reset = Some(ResetEvidence::Verified);
                    r.new_session = Some(session);
                    r.new_generation = Some(generation);
                }
            }
            Ok(()) => {
                // Bytes were written while the fence tripped: the outcome is unknown.
                // This includes a trip after the Enter flush but before the waiter
                // closed the fence: conservatively `indeterminate{*_during_write}`.
                r.state = SubmitState::Indeterminate;
                r.reason = fence.reason.map(|reason| fence_reason(reason, true));
            }
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
        unverify_unwritten_reset(r);
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
        // An interrupt is not a turn: a Working edge after Esc says nothing about it.
        for entry in self
            .entries
            .values_mut()
            .filter(|e| e.pane == pane && e.receipt.kind != SubmitKind::Interrupt)
        {
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

    /// An Idle or Blocked (AwaitingHuman) edge at registry event `seq`: the first such edge
    /// after a confirmed `interrupt` write records `effect_observed_at_ms`. Order-based
    /// only; the edge may have other causes.
    pub fn note_turn_end(&mut self, pane: PaneId, seq: u64, now_ms: u64) {
        for entry in self
            .entries
            .values_mut()
            .filter(|e| e.pane == pane && e.receipt.kind == SubmitKind::Interrupt)
        {
            if entry.receipt.effect_observed_at_ms.is_some() {
                continue;
            }
            match entry.receipt.state {
                SubmitState::Written if entry.written_seq.is_some_and(|w| w < seq) => {
                    entry.receipt.effect_observed_at_ms = Some(now_ms);
                }
                SubmitState::Accepted
                    if entry.effect_candidate_ms.is_none()
                        && entry.fence.is_committed()
                        && !entry.fence.is_tripped() =>
                {
                    entry.effect_candidate_ms = Some(now_ms);
                }
                _ => {}
            }
        }
    }

    /// A real session replacement (C16: a known ref replaced by a different one) whose
    /// start source is `clear`, observed at registry event `seq`, about to bump the pane
    /// to `new_generation`. Must run before the bump trips the fences.
    ///
    /// - A `reset` written before `seq` and still pending becomes `verified` with the new
    ///   session and generation.
    /// - A `reset` whose writer committed and whose fence is untripped expects the bump's
    ///   generation trip (mapped to `written` by `complete`) and keeps the session as a
    ///   candidate applied on completion.
    pub fn note_clear_session(
        &mut self,
        pane: PaneId,
        new_session: &AgentSessionRef,
        new_generation: u64,
        seq: u64,
    ) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && e.receipt.kind == SubmitKind::Reset
                && e.receipt.reset == Some(ResetEvidence::Pending)
        }) {
            match entry.receipt.state {
                SubmitState::Written | SubmitState::Started
                    if entry.written_seq.is_some_and(|w| w < seq) =>
                {
                    entry.receipt.reset = Some(ResetEvidence::Verified);
                    entry.receipt.new_session = Some(session_wire(new_session));
                    entry.receipt.new_generation = Some(new_generation);
                }
                SubmitState::Accepted
                    if entry.fence.is_committed() && !entry.fence.is_tripped() =>
                {
                    entry.expected_reset_trip = true;
                    entry.reset_candidate = Some((session_wire(new_session), new_generation));
                }
                _ => {}
            }
        }
    }

    /// Any other conversation-replacing generation change (not a pane move and not the
    /// expected clear): a written reset still pending can no longer be verified.
    pub fn note_other_replacement(&mut self, pane: PaneId) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && e.receipt.kind == SubmitKind::Reset
                && e.receipt.reset == Some(ResetEvidence::Pending)
                && matches!(e.receipt.state, SubmitState::Written | SubmitState::Started)
        }) {
            entry.receipt.reset = Some(ResetEvidence::Unverified);
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

    fn new_receipt(kind: SubmitKind, pane: u32, text: &str, handoff: bool) -> NewReceipt {
        NewReceipt {
            kind,
            question_id: None,
            pane: pid(pane),
            target_ref: target(1),
            text_sha256_16: text_sha256_16(text),
            handoff,
        }
    }

    fn accept_kind(ledger: &mut SubmitLedger, id: &str, kind: SubmitKind) -> SubmitFence {
        let fence = SubmitFence::new();
        ledger
            .insert_accepted(id, new_receipt(kind, 1, "x", false), fence.clone(), 10)
            .expect("accepted");
        fence
    }

    fn accept(ledger: &mut SubmitLedger, id: &str, text: &str) -> SubmitFence {
        let fence = SubmitFence::new();
        ledger
            .insert_accepted(
                id,
                new_receipt(SubmitKind::Submit, 1, text, false),
                fence.clone(),
                10,
            )
            .expect("accepted");
        fence
    }

    fn key<'a>(target_ref: &'a TargetRef, hash: &'a str) -> RequestKey<'a> {
        RequestKey {
            kind: SubmitKind::Submit,
            question_id: None,
            target_ref,
            text_sha256_16: hash,
        }
    }

    fn session(value: &str) -> AgentSessionRef {
        AgentSessionRef {
            kind: crate::agent_resume::AgentSessionRefKind::Id,
            value: value.into(),
        }
    }

    const CLEAN: FenceSnapshot = FenceSnapshot {
        committed: true,
        reason: None,
    };

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
        let (t1, t2) = (target(1), target(2));
        let (hello, other) = (text_sha256_16("hello"), text_sha256_16("other"));
        let Repeat::Same(r) = l.repeat("s1", key(&t1, &hello)) else {
            panic!("expected same");
        };
        assert_eq!(r.state, SubmitState::Accepted);
        assert_eq!(r.kind, SubmitKind::Submit);
        assert_eq!(r.acknowledged, SubmitAcknowledged::Unsupported);
        assert_eq!(l.repeat("s1", key(&t1, &other)), Repeat::Conflict);
        assert_eq!(l.repeat("s1", key(&t2, &hello)), Repeat::Conflict);
        assert_eq!(l.repeat("s2", key(&t1, &hello)), Repeat::New);
        // The same id under another verb or question conflicts.
        let mut answer = key(&t1, &hello);
        answer.kind = SubmitKind::Answer;
        assert_eq!(l.repeat("s1", answer), Repeat::Conflict);
        let mut with_question = key(&t1, &hello);
        with_question.question_id = Some("q1");
        assert_eq!(l.repeat("s1", with_question), Repeat::Conflict);
    }

    #[test]
    fn answer_receipt_keeps_question_id_and_kind() {
        let mut l = SubmitLedger::default();
        let mut new = new_receipt(SubmitKind::Answer, 1, "yes", false);
        new.question_id = Some("q-1".into());
        let r = l
            .insert_accepted("a1", new, SubmitFence::new(), 1)
            .expect("accepted");
        assert_eq!(r.kind, SubmitKind::Answer);
        assert_eq!(r.question_id.as_deref(), Some("q-1"));
        assert_eq!(r.reset, None);
        let t = target(1);
        let hash = text_sha256_16("yes");
        let mut same = key(&t, &hash);
        same.kind = SubmitKind::Answer;
        same.question_id = Some("q-1");
        assert!(matches!(l.repeat("a1", same), Repeat::Same(_)));
        same.question_id = Some("q-2");
        assert_eq!(l.repeat("a1", same), Repeat::Conflict);
    }

    #[test]
    fn interrupt_effect_is_the_first_idle_edge_after_the_write() {
        let mut l = SubmitLedger::default();
        accept_kind(&mut l, "i", SubmitKind::Interrupt);
        // Before the write: neither a turn end nor a turn start is attributed.
        l.note_turn_end(pid(1), 2, 20);
        l.complete("i", &Ok(()), CLEAN, 3, 21);
        let r = l.get("i").unwrap();
        assert_eq!(r.state, SubmitState::Written);
        assert_eq!(r.effect_observed_at_ms, None);
        // A Working edge never marks an interrupt started.
        l.note_turn_start(pid(1), 4, 22);
        assert_eq!(l.get("i").unwrap().state, SubmitState::Written);
        l.note_turn_end(pid(2), 5, 23);
        assert_eq!(l.get("i").unwrap().effect_observed_at_ms, None);
        l.note_turn_end(pid(1), 6, 24);
        assert_eq!(l.get("i").unwrap().effect_observed_at_ms, Some(24));
        l.note_turn_end(pid(1), 7, 25);
        assert_eq!(l.get("i").unwrap().effect_observed_at_ms, Some(24));

        // An edge between the writer's commit and completion is applied on completion.
        let f = accept_kind(&mut l, "race", SubmitKind::Interrupt);
        assert!(f.try_commit());
        l.note_turn_end(pid(1), 8, 30);
        l.complete("race", &Ok(()), f.close(), 9, 31);
        let r = l.get("race").unwrap();
        assert_eq!(
            (r.state, r.effect_observed_at_ms),
            (SubmitState::Written, Some(30))
        );

        // A submit receipt never gets an effect.
        accept(&mut l, "s", "x");
        l.complete("s", &Ok(()), CLEAN, 10, 32);
        l.note_turn_end(pid(1), 11, 33);
        assert_eq!(l.get("s").unwrap().effect_observed_at_ms, None);
    }

    #[test]
    fn reset_verified_only_by_a_clear_session_after_the_write() {
        let mut l = SubmitLedger::default();
        accept_kind(&mut l, "r", SubmitKind::Reset);
        assert_eq!(l.get("r").unwrap().reset, Some(ResetEvidence::Pending));
        // A clear session seen before the writer committed is not this reset's effect.
        l.note_clear_session(pid(1), &session("early"), 2, 1);
        l.complete("r", &Ok(()), CLEAN, 2, 20);
        let r = l.get("r").unwrap();
        assert_eq!(
            (r.state, r.reset),
            (SubmitState::Written, Some(ResetEvidence::Pending))
        );
        assert!(r.new_session.is_none() && r.new_generation.is_none());
        // Other pane: ignored.
        l.note_clear_session(pid(2), &session("other"), 5, 3);
        assert_eq!(l.get("r").unwrap().reset, Some(ResetEvidence::Pending));
        l.note_clear_session(pid(1), &session("new"), 3, 4);
        let r = l.get("r").unwrap();
        assert_eq!(r.reset, Some(ResetEvidence::Verified));
        assert_eq!(r.new_session.as_ref().unwrap().value, "new");
        assert_eq!(r.new_generation, Some(3));
        // Later evidence never rewrites a verified receipt.
        l.note_clear_session(pid(1), &session("newer"), 4, 5);
        l.note_other_replacement(pid(1));
        assert_eq!(
            l.get("r").unwrap().new_session.as_ref().unwrap().value,
            "new"
        );
        assert_eq!(l.get("r").unwrap().reset, Some(ResetEvidence::Verified));
    }

    #[test]
    fn reset_stays_pending_without_evidence_and_unverified_on_other_replacement() {
        let mut l = SubmitLedger::default();
        accept_kind(&mut l, "p", SubmitKind::Reset);
        l.complete("p", &Ok(()), CLEAN, 2, 20);
        l.note_turn_start(pid(1), 3, 21);
        let r = l.get("p").unwrap();
        assert_eq!(r.reset, Some(ResetEvidence::Pending));
        assert!(r.new_session.is_none() && r.new_generation.is_none());
        l.note_other_replacement(pid(1));
        let r = l.get("p").unwrap();
        assert_eq!(r.reset, Some(ResetEvidence::Unverified));
        assert!(r.new_session.is_none(), "never a fabricated session");

        // Fenced or indeterminate resets are unverified.
        let f = accept_kind(&mut l, "f", SubmitKind::Reset);
        f.trip(FenceReason::OwnerInput);
        l.complete(
            "f",
            &Err(crate::supervisor::fence::fenced_error()),
            f.close(),
            4,
            22,
        );
        let r = l.get("f").unwrap();
        assert_eq!(
            (r.state, r.reset),
            (SubmitState::Fenced, Some(ResetEvidence::Unverified))
        );
        accept_kind(&mut l, "q", SubmitKind::Reset);
        assert_eq!(
            l.mark_queue_failed("q").unwrap().reset,
            Some(ResetEvidence::Unverified)
        );
    }

    #[test]
    fn reset_late_trip_from_clear_session_is_written_and_verified() {
        let mut l = SubmitLedger::default();
        let f = accept_kind(&mut l, "late", SubmitKind::Reset);
        assert!(f.try_commit());
        // App order: classify the clear replacement, then bump (which trips the fence).
        l.note_clear_session(pid(1), &session("fresh"), 2, 5);
        l.trip_pane(pid(1), FenceReason::GenerationChanged);
        l.complete("late", &Ok(()), f.close(), 6, 30);
        let r = l.get("late").unwrap();
        assert_eq!((r.state, r.reason), (SubmitState::Written, None));
        assert_eq!(r.reset, Some(ResetEvidence::Verified));
        assert_eq!(r.new_session.as_ref().unwrap().value, "fresh");
        assert_eq!(r.new_generation, Some(2));
    }

    #[test]
    fn reset_late_trip_without_clear_session_stays_indeterminate() {
        let mut l = SubmitLedger::default();
        // Generation trip not armed by a clear session (e.g. agent exit, resume).
        let f = accept_kind(&mut l, "gen", SubmitKind::Reset);
        assert!(f.try_commit());
        l.trip_pane(pid(1), FenceReason::GenerationChanged);
        l.complete("gen", &Ok(()), f.close(), 2, 30);
        let r = l.get("gen").unwrap();
        assert_eq!(
            (r.state, r.reason, r.reset),
            (
                SubmitState::Indeterminate,
                Some(SubmitReason::GenerationChangedDuringWrite),
                Some(ResetEvidence::Unverified)
            )
        );
        // Owner input after a clear-armed fence: not the expected trip.
        let g = accept_kind(&mut l, "owner", SubmitKind::Reset);
        assert!(g.try_commit());
        g.trip(FenceReason::OwnerInput);
        l.note_clear_session(pid(1), &session("x"), 3, 3);
        l.complete("owner", &Ok(()), g.close(), 4, 31);
        let r = l.get("owner").unwrap();
        assert_eq!(r.state, SubmitState::Indeterminate);
        assert!(r.new_session.is_none());
        // A submit receipt never maps a generation late trip to written.
        let h = accept(&mut l, "submit", "x");
        assert!(h.try_commit());
        l.note_clear_session(pid(1), &session("y"), 4, 5);
        l.trip_pane(pid(1), FenceReason::GenerationChanged);
        l.complete("submit", &Ok(()), h.close(), 6, 32);
        assert_eq!(l.get("submit").unwrap().state, SubmitState::Indeterminate);
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
                new_receipt(SubmitKind::Submit, 1, "x", false),
                SubmitFence::new(),
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
                new_receipt(SubmitKind::Submit, 1, "x", true),
                SubmitFence::new(),
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
            new_receipt(SubmitKind::Submit, 2, "x", false),
            other.clone(),
            1,
        );
        l.trip_pane(pid(1), FenceReason::GenerationChanged);
        assert!(!f.try_commit());
        assert!(other.try_commit());
    }
}

//! Fork-only receipt ledger for `agent.submit`, `agent.answer`, `agent.interrupt`,
//! `agent.reset` and `agent.resume` (S2/S3, S7-S10). Pure bookkeeping: no App access, no
//! I/O. Receipts never hold the submitted text, only a 16-hex SHA-256 prefix of it.

use std::collections::HashMap;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::agent_resume::AgentSessionRef;
use crate::api::schema::supervisor::{
    ReceiptSession, ResetEvidence, ResumeEvidence, SubmitAcknowledged, SubmitKind, SubmitReason,
    SubmitReceipt, SubmitState, TargetRef,
};
use crate::detect::Agent;
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
/// Logical clear key, independent of the terminal/platform encoding.
pub const CLEAR_DRAFT_HASH_TEXT: &str = "\u{15}";

/// Upper bound on an `agent.resume` session id, in characters.
pub const MAX_RESUME_SESSION_ID_CHARS: usize = 256;
/// Upper bound on a recorded `agent.start` error code.
const MAX_START_ERROR_CHARS: usize = 64;
/// Session start sources that verify a resume when the reported id equals the requested one.
const RESUME_VERIFYING_SOURCES: [&str; 2] = ["resume", "startup"];
/// A prompt-submit hook report confirms only a receipt written at most this long before it
/// (measured from acceptance for a write still in flight).
pub const PROMPT_ACK_WINDOW_MS: u64 = 120_000;
/// Hook reports run as separate processes and can arrive out of order. A report that
/// matches nothing retires only receipts written more than this long before it (Claude's
/// hook timeout is 10 s), so a near-simultaneous owner prompt does not retire a receipt
/// whose own report is still on its way.
pub const PROMPT_ACK_REORDER_SLACK_MS: u64 = 10_000;

/// `[A-Za-z0-9_-]{1,256}`, not starting with `-`: the id becomes a `claude` argument, so
/// it can never read as an option (CWE-88).
pub fn valid_resume_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RESUME_SESSION_ID_CHARS
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

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

/// Lines Claude Code may wrap around expanded pasted content in a `UserPromptSubmit`
/// prompt (`<pasted_content id="…">` … `</pasted_content id="…">`).
const PASTED_CONTENT_OPEN: &str = "<pasted_content id=\"";
const PASTED_CONTENT_CLOSE: &str = "</pasted_content id=\"";

/// Text compared for a prompt-submit acknowledgement. Applied identically to the text
/// herdr submits and to the `prompt` an agent's hook reports:
/// - CRLF and lone CR become LF;
/// - surrounding whitespace (including trailing newlines) is trimmed;
/// - a prompt that is exactly one `<pasted_content id="X">` … `</pasted_content id="X">`
///   block (herdr's whole submission is one bracketed paste) is unwrapped and trimmed again.
pub fn prompt_ack_text(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = unified.trim();
    unwrap_single_pasted_content(trimmed).trim().to_string()
}

fn unwrap_single_pasted_content(text: &str) -> &str {
    let Some((first, rest)) = text.split_once('\n') else {
        return text;
    };
    let Some((body, last)) = rest.rsplit_once('\n') else {
        return text;
    };
    let Some(id) = first
        .strip_prefix(PASTED_CONTENT_OPEN)
        .and_then(|rest| rest.strip_suffix("\">"))
    else {
        return text;
    };
    let closes_same_id = last
        .strip_prefix(PASTED_CONTENT_CLOSE)
        .and_then(|rest| rest.strip_suffix("\">"))
        .is_some_and(|close_id| close_id == id);
    if !closes_same_id || body.contains(PASTED_CONTENT_OPEN) {
        return text;
    }
    body
}

/// `text_sha256_16` over `prompt_ack_text`: the only value a prompt-submit hook reports.
pub fn prompt_ack_sha256_16(text: &str) -> String {
    text_sha256_16(&prompt_ack_text(text))
}

/// Hash the normalized prompt in a supported agent's prompt-submit hook stdin JSON.
/// Claude and Codex use `UserPromptSubmit`; Cursor uses `beforeSubmitPrompt`. Claude
/// rejects imported Cursor hooks, and `UserPromptSubmit` rejects subagent reports.
/// Invalid JSON, unsupported agents/events, and empty prompts return None. The parsed
/// prompt text is dropped here; only its hash leaves this function.
pub fn hook_prompt_sha256_16(input: &str, agent: Agent) -> Option<String> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let object = value.as_object()?;
    let event = object.get("hook_event_name")?.as_str()?;
    match agent {
        Agent::Claude | Agent::Codex => {
            if event != "UserPromptSubmit"
                || (agent == Agent::Claude && object.contains_key("cursor_version"))
                || object
                    .get("agent_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| !id.is_empty())
            {
                return None;
            }
        }
        Agent::Cursor if event == "beforeSubmitPrompt" => {}
        _ => return None,
    }
    let prompt = object.get("prompt")?.as_str()?;
    let normalised = prompt_ack_text(prompt);
    (!normalised.is_empty()).then(|| text_sha256_16(&normalised))
}

/// `[0-9a-f]{16}`: the shape of a reported prompt hash.
pub fn valid_prompt_sha256_16(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
    /// The profile defers reset session evidence until the next prompt.
    lazy_reset: bool,
    /// `reset`: the next generation-changed trip of this fence is the expected clear
    /// SessionChanged bump (armed only while committed and untripped).
    expected_reset_trip: bool,
    /// `reset`: clear session observed between the writer's commit and completion.
    reset_candidate: Option<(ReceiptSession, u64)>,
    /// `resume`: registry event sequence at which the launch was accepted.
    launched_seq: Option<u64>,
    /// `resume`: canonical requested profile agent; never exposed on the receipt.
    resume_agent: Option<Agent>,
    /// `submit` / `answer` / `reset`: `prompt_ack_sha256_16` of the submitted text, matched
    /// against prompt-submit hook reports. None for kinds with their own evidence.
    ack_sha256_16: Option<String>,
    /// Submit/answer ownership proof only; collapsed whitespace, never raw text.
    own_text_sha256_16: Option<String>,
    /// A matching hook report applied while the write was in flight (first byte written,
    /// completion not yet handled): its time. Consumed once when the write completes;
    /// dropped unless the receipt ends `written`.
    pending_ack_ms: Option<u64>,
    /// Insertion order (entries are never evicted, so the ledger length at insert).
    order: usize,
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
    /// Per pane: `written_seq` of the last confirmed receipt. Earlier unconfirmed receipts
    /// of that pane can no longer be confirmed.
    ack_watermark: HashMap<PaneId, u64>,
    /// Per pane: the last hook report that matched nothing, as a time. Unconfirmed receipts
    /// of that pane written (or, in flight, accepted) before it can no longer be confirmed.
    ack_retired_before_ms: HashMap<PaneId, u64>,
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
    /// `resume` only: the requested session id.
    pub session_id: Option<&'a str>,
    /// `resume` only: the canonical requested profile agent.
    pub resume_agent: Option<Agent>,
}

/// Fields of a newly accepted receipt.
#[derive(Debug, Clone)]
pub struct NewReceipt {
    pub kind: SubmitKind,
    /// Internal profile policy; never exposed on the public receipt.
    pub lazy_reset: bool,
    pub question_id: Option<String>,
    pub pane: PaneId,
    pub target_ref: TargetRef,
    pub text_sha256_16: String,
    /// `submit` / `answer` / `reset`: `prompt_ack_sha256_16` of the text; None otherwise.
    pub ack_sha256_16: Option<String>,
    /// Submit/answer ownership proof only; never exposed on a public receipt.
    pub own_text_sha256_16: Option<String>,
    pub handoff: bool,
    /// `resume` only: the requested session id.
    pub session_id: Option<String>,
    /// `resume` only: the canonical requested profile agent.
    pub resume_agent: Option<Agent>,
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

    /// Only the newest two receipts in this pane generation can prove a restored draft.
    /// Select by acceptance order before checking kind/state: intervening attempts revoke
    /// ownership, even when they were fenced or never written. Bounded by the receipt cap.
    pub fn own_restored_text_hash(&self, pane: PaneId, generation: u64) -> Option<String> {
        let mut newest: Option<&SubmitEntry> = None;
        let mut previous: Option<&SubmitEntry> = None;
        for entry in self
            .entries
            .values()
            .filter(|entry| entry.pane == pane && entry.receipt.target_ref.generation == generation)
        {
            if newest.is_none_or(|latest| entry.order > latest.order) {
                previous = newest;
                newest = Some(entry);
            } else if previous.is_none_or(|prior| entry.order > prior.order) {
                previous = Some(entry);
            }
        }
        let interrupt = newest?;
        let prompt = previous?;
        if interrupt.receipt.kind != SubmitKind::Interrupt
            || interrupt.receipt.state != SubmitState::Written
            || !matches!(prompt.receipt.kind, SubmitKind::Submit | SubmitKind::Answer)
            || prompt.receipt.state != SubmitState::Written
            || prompt.written_seq? >= interrupt.written_seq?
        {
            return None;
        }
        prompt.own_text_sha256_16.clone()
    }

    /// Fork (Q1): receipts still `accepted` (write queued or in progress), per pane.
    /// Bounded by the receipt cap; empty ledger costs one length check.
    pub fn accepted_by_pane(&self) -> HashMap<PaneId, u32> {
        let mut counts = HashMap::new();
        for entry in self.entries.values() {
            if entry.receipt.state == SubmitState::Accepted {
                *counts.entry(entry.pane).or_insert(0u32) += 1;
            }
        }
        counts
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.cap()
    }

    /// Same id with the same kind, question, target and text returns the stored receipt;
    /// any difference is a conflict. A `resume` names a pane, not a generation (its own
    /// launch bumps the generation), so it compares epoch, pane, session id and agent.
    pub fn repeat(&self, submit_id: &str, key: RequestKey<'_>) -> Repeat {
        let Some(e) = self.entries.get(submit_id) else {
            return Repeat::New;
        };
        let r = &e.receipt;
        let target_same = if key.kind == SubmitKind::Resume {
            r.target_ref.server_epoch == key.target_ref.server_epoch
                && r.target_ref.pane_id == key.target_ref.pane_id
        } else {
            r.target_ref == *key.target_ref
        };
        if r.kind == key.kind
            && r.question_id.as_deref() == key.question_id
            && target_same
            && r.text_sha256_16 == key.text_sha256_16
            && r.session_id.as_deref() == key.session_id
            && e.resume_agent == key.resume_agent
        {
            Repeat::Same(Box::new(r.clone()))
        } else {
            Repeat::Conflict
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
            reason: None,
            acknowledged: SubmitAcknowledged::Unsupported,
            acknowledged_at_ms: None,
            text_sha256_16: new.text_sha256_16,
            handoff: new.handoff,
            kind: new.kind,
            question_id: new.question_id,
            turn_end_observed_after_write_at_ms: None,
            reset: (new.kind == SubmitKind::Reset).then_some(ResetEvidence::Pending),
            new_session: None,
            new_generation: None,
            session_id: new.session_id,
            resume: (new.kind == SubmitKind::Resume).then_some(ResumeEvidence::Pending),
            reported_session: None,
            session_start_source: None,
            launched_at_ms: None,
            start_error: None,
        };
        self.entries.insert(
            submit_id.to_string(),
            SubmitEntry {
                receipt: receipt.clone(),
                pane: new.pane,
                fence,
                written_seq: None,
                lazy_reset: new.lazy_reset,
                expected_reset_trip: false,
                reset_candidate: None,
                launched_seq: None,
                resume_agent: new.resume_agent,
                ack_sha256_16: new.ack_sha256_16,
                own_text_sha256_16: new.own_text_sha256_16,
                pending_ack_ms: None,
                order: self.entries.len(),
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
                // A hook report that arrived while this write was in flight. This is the
                // newest written receipt of the pane, so the watermark only moves forward.
                if let Some(at_ms) = entry.pending_ack_ms.take() {
                    r.acknowledged = SubmitAcknowledged::Confirmed;
                    r.acknowledged_at_ms = Some(at_ms.max(now_ms));
                    let mark = self.ack_watermark.entry(entry.pane).or_insert(seq);
                    *mark = (*mark).max(seq);
                }
                // Delivery ends at `written`. Activity edges handled before this event are
                // never attributed to the write; only the expected clear session of a
                // reset (order-based, see `note_clear_session`) is applied here.
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
        // Only a `written` receipt can be confirmed.
        entry.pending_ack_ms = None;
        unverify_unwritten_reset(r);
    }

    /// Only the completion event that newly wrote a lazy reset can replace the
    /// conversation. A clear already observed before completion has its own bump.
    pub(super) fn newly_written_lazy_reset(&self, submit_id: &str, seq: u64) -> Option<PaneId> {
        let entry = self.entries.get(submit_id)?;
        (entry.lazy_reset
            && entry.receipt.kind == SubmitKind::Reset
            && entry.receipt.state == SubmitState::Written
            && entry.receipt.reset == Some(ResetEvidence::Pending)
            && entry.written_seq == Some(seq))
        .then_some(entry.pane)
    }

    /// A prompt-submit hook of `pane` reported `hash` (`prompt_ack_sha256_16`) at registry
    /// event `seq`, while the pane's agent generation is `generation`. Order-based, consumed
    /// once, never inferred from activity. Only submit/answer/reset receipts of the pane
    /// accepted in `generation` and with that hash are candidates:
    /// Candidates must also be written (in flight: accepted) at most `PROMPT_ACK_WINDOW_MS`
    /// before `now_ms` and not before the pane's last unmatched report (below).
    /// - the earliest `written`, unconfirmed candidate written before `seq` and after the
    ///   pane's last confirmed receipt becomes `confirmed`; earlier unconfirmed receipts of
    ///   the pane can no longer be confirmed;
    /// - otherwise the earliest in-flight candidate whose first byte is written (fence
    ///   committed, not tripped) and with no report pending keeps the report pending,
    ///   applied only if the write completes `written`;
    /// - otherwise the report matched nothing: receipts of the pane written more than
    ///   `PROMPT_ACK_REORDER_SLACK_MS` before it are retired, so a receipt whose own report
    ///   was lost cannot be confirmed by a later identical prompt.
    ///
    /// Returns the confirmed submit id, if any.
    pub fn note_prompt_submitted(
        &mut self,
        pane: PaneId,
        hash: &str,
        generation: u64,
        seq: u64,
        now_ms: u64,
    ) -> Option<String> {
        let watermark = self.ack_watermark.get(&pane).copied().unwrap_or(0);
        let retired_before_ms = self.ack_retired_before_ms.get(&pane).copied().unwrap_or(0);
        let ackable = |e: &SubmitEntry| {
            e.pane == pane
                && e.receipt.target_ref.generation == generation
                && e.receipt.acknowledged == SubmitAcknowledged::Unsupported
                && e.ack_sha256_16.as_deref() == Some(hash)
        };
        let in_window = |since_ms: u64| {
            since_ms >= retired_before_ms && now_ms.saturating_sub(since_ms) <= PROMPT_ACK_WINDOW_MS
        };
        let written = self
            .entries
            .iter_mut()
            .filter(|(_, e)| {
                ackable(e)
                    && e.receipt.state == SubmitState::Written
                    && e.written_seq.is_some_and(|w| w > watermark && w < seq)
                    && e.receipt.written_at_ms.is_some_and(in_window)
            })
            .min_by_key(|(_, e)| e.written_seq);
        if let Some((submit_id, entry)) = written {
            let written_at_ms = entry.receipt.written_at_ms.unwrap_or(now_ms);
            entry.receipt.acknowledged = SubmitAcknowledged::Confirmed;
            entry.receipt.acknowledged_at_ms = Some(now_ms.max(written_at_ms));
            if let Some(written_seq) = entry.written_seq {
                self.ack_watermark.insert(pane, written_seq);
            }
            return Some(submit_id.clone());
        }
        if let Some((_, entry)) = self
            .entries
            .iter_mut()
            .filter(|(_, e)| {
                ackable(e)
                    && e.receipt.state == SubmitState::Accepted
                    && e.pending_ack_ms.is_none()
                    && e.fence.is_committed()
                    && !e.fence.is_tripped()
                    && in_window(e.receipt.accepted_at_ms)
            })
            .min_by_key(|(_, e)| e.order)
        {
            entry.pending_ack_ms = Some(now_ms);
            return None;
        }
        let retire = now_ms
            .saturating_sub(PROMPT_ACK_REORDER_SLACK_MS)
            .max(retired_before_ms);
        self.ack_retired_before_ms.insert(pane, retire);
        None
    }

    /// `resume`: the agent.start path accepted the launch at registry event `seq`.
    pub fn mark_launched(
        &mut self,
        submit_id: &str,
        seq: u64,
        now_ms: u64,
    ) -> Option<SubmitReceipt> {
        let entry = self.entries.get_mut(submit_id)?;
        if entry.receipt.kind == SubmitKind::Resume && entry.receipt.state == SubmitState::Accepted
        {
            entry.receipt.state = SubmitState::Launched;
            entry.receipt.launched_at_ms = Some(now_ms);
            entry.launched_seq = Some(seq);
        }
        Some(entry.receipt.clone())
    }

    /// `resume`: the agent.start path refused the launch with `code`; nothing was sent.
    /// The evidence stays `pending`: no session report can settle a launch that never ran.
    pub fn mark_start_failed(&mut self, submit_id: &str, code: &str) -> Option<SubmitReceipt> {
        let entry = self.entries.get_mut(submit_id)?;
        if entry.receipt.kind == SubmitKind::Resume && entry.receipt.state == SubmitState::Accepted
        {
            entry.receipt.state = SubmitState::Failed;
            entry.receipt.reason = Some(SubmitReason::StartFailed);
            entry.receipt.start_error = Some(code.chars().take(MAX_START_ERROR_CHARS).collect());
        }
        Some(entry.receipt.clone())
    }

    /// `resume`: the start command was sent at registry event `seq`, then the start path
    /// failed with `code`. The outcome is unknown: `indeterminate{start_unconfirmed}`. A
    /// later session report from the pane may still settle the evidence.
    pub fn mark_start_unconfirmed(
        &mut self,
        submit_id: &str,
        code: &str,
        seq: u64,
    ) -> Option<SubmitReceipt> {
        let entry = self.entries.get_mut(submit_id)?;
        if entry.receipt.kind == SubmitKind::Resume && entry.receipt.state == SubmitState::Accepted
        {
            entry.receipt.state = SubmitState::Indeterminate;
            entry.receipt.reason = Some(SubmitReason::StartUnconfirmed);
            entry.receipt.start_error = Some(code.chars().take(MAX_START_ERROR_CHARS).collect());
            entry.launched_seq = Some(seq);
        }
        Some(entry.receipt.clone())
    }

    /// `resume`: an AgentStarted bump to `generation` at registry event `seq`. The first
    /// one after a launch records the new target generation (until evidence settles it).
    pub fn note_agent_started(&mut self, pane: PaneId, generation: u64, seq: u64) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && e.receipt.kind == SubmitKind::Resume
                && matches!(
                    e.receipt.state,
                    SubmitState::Launched | SubmitState::Indeterminate
                )
                && e.receipt.resume == Some(ResumeEvidence::Pending)
                && e.receipt.new_generation.is_none()
                && e.launched_seq.is_some_and(|l| l < seq)
        }) {
            entry.receipt.new_generation = Some(generation);
        }
    }

    /// `resume`: a session report from `pane` applied at registry event `seq`, leaving the
    /// pane at `generation`. Only the first id reported after the launch settles the
    /// evidence: the requested id with start source `resume` or `startup` verifies it, a
    /// different id is a mismatch. The requested id with any other (or no) start source
    /// leaves it pending.
    pub fn note_resume_session(
        &mut self,
        pane: PaneId,
        reported_agent: Agent,
        session: &AgentSessionRef,
        start_source: Option<&str>,
        generation: u64,
        seq: u64,
    ) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && e.receipt.kind == SubmitKind::Resume
                && e.resume_agent == Some(reported_agent)
                && matches!(
                    e.receipt.state,
                    SubmitState::Launched | SubmitState::Indeterminate
                )
                && e.receipt.resume == Some(ResumeEvidence::Pending)
                && e.launched_seq.is_some_and(|l| l < seq)
        }) {
            let r = &mut entry.receipt;
            let same = session.kind == crate::agent_resume::AgentSessionRefKind::Id
                && r.session_id.as_deref() == Some(session.value.as_str());
            if same {
                let Some(source) =
                    start_source.filter(|source| RESUME_VERIFYING_SOURCES.contains(source))
                else {
                    continue;
                };
                r.resume = Some(ResumeEvidence::Verified);
                r.session_start_source = Some(source.to_string());
            } else {
                r.resume = Some(ResumeEvidence::Mismatch);
                r.reported_session = Some(session_wire(session));
                r.session_start_source = start_source.map(str::to_string);
            }
            r.new_generation = Some(generation);
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

    /// An Idle or Blocked (AwaitingHuman) edge at registry event `seq`: the first such edge
    /// ordered after a confirmed `interrupt` write records
    /// `turn_end_observed_after_write_at_ms`. Uncorrelated observation only: the edge may
    /// have other causes. An edge handled before the completion event (even after the
    /// writer committed) is never recorded, because it may precede the Esc byte.
    pub fn note_turn_end(&mut self, pane: PaneId, seq: u64, now_ms: u64) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && e.receipt.kind == SubmitKind::Interrupt
                && e.receipt.state == SubmitState::Written
                && e.receipt.turn_end_observed_after_write_at_ms.is_none()
                && e.written_seq.is_some_and(|w| w < seq)
        }) {
            entry.receipt.turn_end_observed_after_write_at_ms = Some(now_ms);
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
        self.note_clear_session_if(pane, new_session, new_generation, seq, true, true);
    }

    /// A lazy reset's session report arrives after its generation bump. Only
    /// earlier written resets can be verified; in-flight resets still need their
    /// own conversation replacement when their writes complete.
    pub(super) fn note_written_clear_session(
        &mut self,
        pane: PaneId,
        new_session: &AgentSessionRef,
        generation: u64,
        seq: u64,
    ) {
        self.note_clear_session_if(pane, new_session, generation, seq, true, false);
    }

    /// Legacy clear evidence can settle immediate resets without making a lazy
    /// reset trust an absent, unsupported or unchanged reported session identity.
    pub(super) fn note_immediate_clear_session(
        &mut self,
        pane: PaneId,
        new_session: &AgentSessionRef,
        new_generation: u64,
        seq: u64,
    ) {
        self.note_clear_session_if(pane, new_session, new_generation, seq, false, true);
    }

    fn note_clear_session_if(
        &mut self,
        pane: PaneId,
        new_session: &AgentSessionRef,
        new_generation: u64,
        seq: u64,
        allow_lazy_reset: bool,
        allow_inflight_reset: bool,
    ) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && (!e.lazy_reset || allow_lazy_reset)
                && e.receipt.kind == SubmitKind::Reset
                && e.receipt.reset == Some(ResetEvidence::Pending)
        }) {
            match entry.receipt.state {
                SubmitState::Written if entry.written_seq.is_some_and(|w| w < seq) => {
                    entry.receipt.reset = Some(ResetEvidence::Verified);
                    entry.receipt.new_session = Some(session_wire(new_session));
                    entry.receipt.new_generation = Some(new_generation);
                }
                SubmitState::Accepted
                    if allow_inflight_reset
                        && entry.fence.is_committed()
                        && !entry.fence.is_tripped() =>
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
        self.note_other_replacement_except(pane, None);
    }

    /// The reset that caused a lazy-reset bump stays pending; older resets do not.
    pub(super) fn note_other_replacement_except(&mut self, pane: PaneId, except: Option<&str>) {
        for entry in self.entries.values_mut().filter(|e| {
            e.pane == pane
                && Some(e.receipt.submit_id.as_str()) != except
                && e.receipt.kind == SubmitKind::Reset
                && e.receipt.reset == Some(ResetEvidence::Pending)
                && e.receipt.state == SubmitState::Written
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
            lazy_reset: false,
            question_id: None,
            pane: pid(pane),
            target_ref: target(1),
            text_sha256_16: text_sha256_16(text),
            ack_sha256_16: matches!(
                kind,
                SubmitKind::Submit | SubmitKind::Answer | SubmitKind::Reset
            )
            .then(|| prompt_ack_sha256_16(text)),
            own_text_sha256_16: matches!(kind, SubmitKind::Submit | SubmitKind::Answer)
                .then(|| text_sha256_16(&text.split_whitespace().collect::<Vec<_>>().join(" "))),
            handoff,
            session_id: None,
            resume_agent: None,
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
            session_id: None,
            resume_agent: None,
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
    fn interrupt_records_only_the_first_turn_end_after_the_write() {
        let mut l = SubmitLedger::default();
        accept_kind(&mut l, "i", SubmitKind::Interrupt);
        // Before the write: a turn end is not recorded.
        l.note_turn_end(pid(1), 2, 20);
        l.complete("i", &Ok(()), CLEAN, 3, 21);
        let r = l.get("i").unwrap();
        assert_eq!(r.state, SubmitState::Written);
        assert_eq!(r.turn_end_observed_after_write_at_ms, None);
        l.note_turn_end(pid(2), 5, 23);
        assert_eq!(
            l.get("i").unwrap().turn_end_observed_after_write_at_ms,
            None
        );
        l.note_turn_end(pid(1), 6, 24);
        assert_eq!(
            l.get("i").unwrap().turn_end_observed_after_write_at_ms,
            Some(24)
        );
        l.note_turn_end(pid(1), 7, 25);
        let r = l.get("i").unwrap();
        assert_eq!(
            (r.state, r.turn_end_observed_after_write_at_ms),
            (SubmitState::Written, Some(24))
        );

        // An edge between the writer's commit and completion may precede the Esc byte:
        // it is never recorded, and the receipt stays plain `written`.
        let f = accept_kind(&mut l, "race", SubmitKind::Interrupt);
        assert!(f.try_commit());
        l.note_turn_end(pid(1), 8, 30);
        l.complete("race", &Ok(()), f.close(), 9, 31);
        let r = l.get("race").unwrap();
        assert_eq!(
            (r.state, r.turn_end_observed_after_write_at_ms),
            (SubmitState::Written, None)
        );
        l.note_turn_end(pid(1), 10, 32);
        assert_eq!(
            l.get("race").unwrap().turn_end_observed_after_write_at_ms,
            Some(32)
        );

        // A submit receipt never records a turn end.
        accept(&mut l, "s", "x");
        l.complete("s", &Ok(()), CLEAN, 11, 33);
        l.note_turn_end(pid(1), 12, 34);
        let r = l.get("s").unwrap();
        assert_eq!(
            (r.state, r.turn_end_observed_after_write_at_ms),
            (SubmitState::Written, None)
        );
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
    fn completion_after_commit_is_plain_written() {
        let mut l = SubmitLedger::default();
        // Whatever activity the App handled between commit and completion, delivery ends at
        // `written`; the ledger has no turn-start input at all.
        let h = accept(&mut l, "plain", "x");
        assert!(h.try_commit());
        l.complete("plain", &Ok(()), h.close(), 8, 46);
        let r = l.get("plain").unwrap();
        assert_eq!((r.state, r.written_at_ms), (SubmitState::Written, Some(46)));
        let json = serde_json::to_value(r).unwrap();
        assert!(json.get("started_at_ms").is_none());
        assert_eq!(json["state"], "written");
        assert_eq!(json["acknowledged"], "unsupported");
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

    fn accept_resume(ledger: &mut SubmitLedger, id: &str, pane: u32, session: &str, agent: Agent) {
        let new = NewReceipt {
            kind: SubmitKind::Resume,
            lazy_reset: false,
            question_id: None,
            pane: pid(pane),
            target_ref: target(1),
            text_sha256_16: text_sha256_16(session),
            ack_sha256_16: None,
            own_text_sha256_16: None,
            handoff: false,
            session_id: Some(session.into()),
            resume_agent: Some(agent),
        };
        ledger
            .insert_accepted(id, new, SubmitFence::new(), 10)
            .expect("accepted");
    }

    fn resume_key<'a>(
        target_ref: &'a TargetRef,
        hash: &'a str,
        session: &'a str,
        agent: Agent,
    ) -> RequestKey<'a> {
        RequestKey {
            kind: SubmitKind::Resume,
            question_id: None,
            target_ref,
            text_sha256_16: hash,
            session_id: Some(session),
            resume_agent: Some(agent),
        }
    }

    #[test]
    fn resume_session_id_bounds() {
        assert!(valid_resume_session_id("a"));
        assert!(valid_resume_session_id(
            &"x".repeat(MAX_RESUME_SESSION_ID_CHARS)
        ));
        assert!(!valid_resume_session_id(""));
        assert!(!valid_resume_session_id(
            &"x".repeat(MAX_RESUME_SESSION_ID_CHARS + 1)
        ));
        assert!(!valid_resume_session_id("a\nb"));
        assert!(!valid_resume_session_id("a\u{1b}b"));
        assert!(valid_resume_session_id("0b9c-41d2_aa"));
        // CWE-88: never an option, a path, a quote or whitespace.
        for bad in [
            "--dangerously-skip-permissions",
            "-p",
            "-",
            "a b",
            "a'b",
            "a\"b",
            "a/b",
            "a\\b",
            "a=b",
            "a.b",
            "\u{e9}",
        ] {
            assert!(!valid_resume_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn resume_repeat_ignores_generation_and_conflicts_on_session_or_kind() {
        let mut l = SubmitLedger::default();
        accept_resume(&mut l, "r", 1, "sess-a", Agent::Claude);
        let hash = text_sha256_16("sess-a");
        // The launch bumps the generation; a retry naming the same pane is the same request.
        let bumped = target(5);
        assert!(matches!(
            l.repeat("r", resume_key(&bumped, &hash, "sess-a", Agent::Claude)),
            Repeat::Same(_)
        ));
        let other_hash = text_sha256_16("sess-b");
        assert_eq!(
            l.repeat(
                "r",
                resume_key(&bumped, &other_hash, "sess-b", Agent::Claude)
            ),
            Repeat::Conflict
        );
        let mut other_pane = target(1);
        other_pane.pane_id = "p2".into();
        assert_eq!(
            l.repeat("r", resume_key(&other_pane, &hash, "sess-a", Agent::Claude)),
            Repeat::Conflict
        );
        // Cross-kind: a submit with the resume's id conflicts, and the other way round.
        let t = target(1);
        assert_eq!(l.repeat("r", key(&t, &hash)), Repeat::Conflict);
        accept(&mut l, "s", "x");
        let x = text_sha256_16("x");
        assert_eq!(
            l.repeat("s", resume_key(&t, &x, "x", Agent::Claude)),
            Repeat::Conflict
        );
    }

    #[test]
    fn resume_launch_and_failure_states() {
        let mut l = SubmitLedger::default();
        accept_resume(&mut l, "ok", 1, "sess", Agent::Claude);
        let r = l.get("ok").unwrap();
        assert_eq!(
            (r.state, r.resume, r.session_id.as_deref()),
            (
                SubmitState::Accepted,
                Some(ResumeEvidence::Pending),
                Some("sess")
            )
        );
        assert_eq!(r.reset, None);
        let r = l.mark_launched("ok", 3, 40).unwrap();
        assert_eq!(
            (r.state, r.launched_at_ms),
            (SubmitState::Launched, Some(40))
        );
        // A later failure never rewrites a launched receipt.
        assert_eq!(
            l.mark_start_failed("ok", "agent_pane_busy").unwrap().state,
            SubmitState::Launched
        );

        accept_resume(&mut l, "bad", 1, "sess", Agent::Claude);
        let r = l
            .mark_start_failed("bad", &"x".repeat(MAX_START_ERROR_CHARS + 10))
            .unwrap();
        assert_eq!(
            (r.state, r.reason, r.resume),
            (
                SubmitState::Failed,
                Some(SubmitReason::StartFailed),
                Some(ResumeEvidence::Pending)
            )
        );
        assert_eq!(
            r.start_error.as_deref().map(str::len),
            Some(MAX_START_ERROR_CHARS)
        );
        // A failed launch is never settled by a session report.
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("sess"),
            Some("resume"),
            2,
            9,
        );
        assert_eq!(l.get("bad").unwrap().resume, Some(ResumeEvidence::Pending));
        // Fence trips and writer results never touch resume receipts.
        l.trip_pane(pid(1), FenceReason::GenerationChanged);
        l.complete("ok", &Ok(()), CLEAN, 10, 50);
        assert_eq!(l.get("ok").unwrap().state, SubmitState::Launched);
    }

    #[test]
    fn resume_failure_after_send_is_indeterminate_and_can_still_be_settled() {
        let mut l = SubmitLedger::default();
        accept_resume(&mut l, "u", 1, "sess", Agent::Claude);
        let r = l
            .mark_start_unconfirmed("u", "agent_pane_unavailable", 2)
            .unwrap();
        assert_eq!(
            (r.state, r.reason, r.start_error.as_deref()),
            (
                SubmitState::Indeterminate,
                Some(SubmitReason::StartUnconfirmed),
                Some("agent_pane_unavailable")
            )
        );
        assert!(r.launched_at_ms.is_none());
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("sess"),
            Some("resume"),
            2,
            3,
        );
        assert_eq!(l.get("u").unwrap().resume, Some(ResumeEvidence::Verified));
    }

    #[test]
    fn resume_verified_only_by_matching_session_after_launch() {
        let mut l = SubmitLedger::default();
        accept_resume(&mut l, "r", 1, "sess", Agent::Claude);
        // Before the launch: nothing is recorded.
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("sess"),
            Some("resume"),
            1,
            1,
        );
        assert_eq!(l.get("r").unwrap().resume, Some(ResumeEvidence::Pending));
        l.mark_launched("r", 2, 20);
        // The launched agent appears: the new generation is known.
        l.note_agent_started(pid(1), 2, 3);
        let r = l.get("r").unwrap();
        assert_eq!(
            (r.resume, r.new_generation),
            (Some(ResumeEvidence::Pending), Some(2))
        );
        // Other pane, or the same id with a non-verifying source: still pending.
        l.note_resume_session(
            pid(2),
            Agent::Claude,
            &session("sess"),
            Some("resume"),
            2,
            4,
        );
        l.note_resume_session(pid(1), Agent::Claude, &session("sess"), Some("clear"), 2, 5);
        l.note_resume_session(pid(1), Agent::Claude, &session("sess"), None, 2, 6);
        assert_eq!(l.get("r").unwrap().resume, Some(ResumeEvidence::Pending));
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("sess"),
            Some("resume"),
            2,
            7,
        );
        let r = l.get("r").unwrap();
        assert_eq!(r.resume, Some(ResumeEvidence::Verified));
        assert_eq!(r.session_start_source.as_deref(), Some("resume"));
        assert_eq!(r.new_generation, Some(2));
        assert!(r.reported_session.is_none());
        // Later evidence never rewrites a settled receipt.
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("other"),
            Some("resume"),
            3,
            8,
        );
        assert_eq!(l.get("r").unwrap().resume, Some(ResumeEvidence::Verified));

        // `startup` with the requested id also verifies, and is recorded.
        accept_resume(&mut l, "s", 3, "sess-s", Agent::Claude);
        l.mark_launched("s", 9, 30);
        l.note_resume_session(
            pid(3),
            Agent::Claude,
            &session("sess-s"),
            Some("startup"),
            4,
            10,
        );
        let r = l.get("s").unwrap();
        assert_eq!(
            (r.resume, r.session_start_source.as_deref()),
            (Some(ResumeEvidence::Verified), Some("startup"))
        );
    }

    #[test]
    fn resume_mismatch_records_the_reported_session_and_stays_pending_without_reports() {
        let mut l = SubmitLedger::default();
        accept_resume(&mut l, "m", 1, "wanted", Agent::Claude);
        l.mark_launched("m", 2, 20);
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("fresh"),
            Some("startup"),
            2,
            3,
        );
        let r = l.get("m").unwrap();
        assert_eq!(r.resume, Some(ResumeEvidence::Mismatch));
        assert_eq!(r.reported_session.as_ref().unwrap().value, "fresh");
        assert_eq!(r.session_start_source.as_deref(), Some("startup"));
        assert_eq!(r.new_generation, Some(2));
        // The requested id reported afterwards does not turn a mismatch into success.
        l.note_resume_session(
            pid(1),
            Agent::Claude,
            &session("wanted"),
            Some("resume"),
            2,
            4,
        );
        assert_eq!(l.get("m").unwrap().resume, Some(ResumeEvidence::Mismatch));

        accept_resume(&mut l, "p", 2, "wanted", Agent::Claude);
        l.mark_launched("p", 5, 21);
        let r = l.get("p").unwrap();
        assert_eq!(r.resume, Some(ResumeEvidence::Pending));
        assert!(r.reported_session.is_none() && r.new_generation.is_none());
    }

    /// Accept, commit and complete a submit of `text` on pane 1 at registry `seq`.
    fn written(l: &mut SubmitLedger, id: &str, text: &str, seq: u64) {
        let f = accept(l, id, text);
        assert!(f.try_commit());
        l.complete(id, &Ok(()), f.close(), seq, 100 + seq);
        assert_eq!(l.get(id).unwrap().state, SubmitState::Written);
    }

    fn ack(l: &SubmitLedger, id: &str) -> (SubmitAcknowledged, Option<u64>) {
        let r = l.get(id).unwrap();
        (r.acknowledged, r.acknowledged_at_ms)
    }

    #[test]
    fn prompt_ack_text_normalises_line_endings_whitespace_and_one_paste_block() {
        assert_eq!(prompt_ack_text("review this"), "review this");
        assert_eq!(prompt_ack_text("review this\n"), "review this");
        assert_eq!(prompt_ack_text("  a\r\nb\rc \r\n"), "a\nb\nc");
        let wrapped = "<pasted_content id=\"p1\">\nline one\nline two\n</pasted_content id=\"p1\">";
        assert_eq!(prompt_ack_text(wrapped), "line one\nline two");
        assert_eq!(
            prompt_ack_sha256_16(wrapped),
            prompt_ack_sha256_16("line one\r\nline two\n")
        );
        // Mismatched ids, a second block or extra typed text are not unwrapped.
        let other_id = "<pasted_content id=\"p1\">\nx\n</pasted_content id=\"p2\">";
        assert_eq!(prompt_ack_text(other_id), other_id);
        let typed_after = format!("{wrapped}\nplease");
        assert_eq!(prompt_ack_text(&typed_after), typed_after);
        // Plain text keeps the receipt hash.
        assert_eq!(prompt_ack_sha256_16("hello"), text_sha256_16("hello"));
        assert!(valid_prompt_sha256_16(&prompt_ack_sha256_16("hello")));
        assert!(!valid_prompt_sha256_16("ABCDEF0123456789"));
        assert!(!valid_prompt_sha256_16("abc"));
    }

    #[test]
    fn hook_payload_yields_only_the_prompt_hash() {
        let payload = serde_json::json!({
            "session_id": "s",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "secret prompt text\n",
        })
        .to_string();
        let hash = hook_prompt_sha256_16(&payload, Agent::Claude).unwrap();
        assert_eq!(hash, prompt_ack_sha256_16("secret prompt text"));
        assert!(valid_prompt_sha256_16(&hash));
        assert!(!hash.contains("secret"));
        for other in [
            serde_json::json!({"hook_event_name": "SessionStart", "prompt": "x"}),
            serde_json::json!({"hook_event_name": "UserPromptSubmit"}),
            serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": " \n"}),
            serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": 1}),
            serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": "x", "cursor_version": "1"}),
            serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": "x", "agent_id": "a"}),
        ] {
            assert!(
                hook_prompt_sha256_16(&other.to_string(), Agent::Claude).is_none(),
                "{other}"
            );
        }
        assert!(hook_prompt_sha256_16("not json", Agent::Claude).is_none());
    }

    #[test]
    fn hook_prompt_codex_docs_shaped_unverified() {
        // UNVERIFIED against a live Codex UserPromptSubmit capture; this is docs-shaped.
        let payload = serde_json::json!({
            "session_id": "example-session",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "  line one\r\nline two\r\n",
        })
        .to_string();
        assert_eq!(
            hook_prompt_sha256_16(&payload, Agent::Codex),
            Some(prompt_ack_sha256_16("line one\nline two"))
        );
    }

    #[test]
    fn hook_prompt_cursor_captured_payload_with_bom() {
        let record = include_str!("testdata/agents/cursor_hook_payloads.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("capture JSON"))
            .find(|record| record["payload"]["hook_event_name"] == "beforeSubmitPrompt")
            .expect("captured beforeSubmitPrompt record");
        assert_eq!(record["stdin_had_utf8_bom"].as_bool(), Some(true));
        let payload = record["payload"]
            .as_object()
            .expect("captured payload object");
        let prompt = payload["prompt"].as_str().expect("captured prompt string");
        let input = format!(
            "\u{feff}{}",
            serde_json::to_string(payload).expect("serialize captured payload")
        );
        assert_eq!(
            hook_prompt_sha256_16(&input, Agent::Cursor),
            Some(prompt_ack_sha256_16(prompt))
        );
    }

    #[test]
    fn hook_prompt_rejects_cross_agent_and_invalid_payloads() {
        for agent in Agent::ALL {
            for event in ["UserPromptSubmit", "beforeSubmitPrompt"] {
                let payload = serde_json::json!({"hook_event_name": event, "prompt": "x"});
                let accepted = matches!(
                    (agent, event),
                    (Agent::Claude | Agent::Codex, "UserPromptSubmit")
                        | (Agent::Cursor, "beforeSubmitPrompt")
                );
                let expected = accepted.then(|| prompt_ack_sha256_16("x"));
                let input = payload.to_string();
                assert_eq!(hook_prompt_sha256_16(&input, agent), expected);
                assert_eq!(
                    hook_prompt_sha256_16(&format!("\u{feff}{input}"), agent),
                    expected
                );
                assert!(
                    hook_prompt_sha256_16(&format!("\u{feff}\u{feff}{input}"), agent).is_none()
                );
            }
        }

        for (agent, event) in [
            (Agent::Claude, "UserPromptSubmit"),
            (Agent::Codex, "UserPromptSubmit"),
            (Agent::Cursor, "beforeSubmitPrompt"),
        ] {
            for input in ["not json", "[]", "null", "1", "{}"] {
                assert!(hook_prompt_sha256_16(input, agent).is_none());
            }
            for payload in [
                serde_json::json!({"hook_event_name": "other", "prompt": "x"}),
                serde_json::json!({"hook_event_name": 1, "prompt": "x"}),
                serde_json::json!({"prompt": "x"}),
                serde_json::json!({"hook_event_name": event}),
                serde_json::json!({"hook_event_name": event, "prompt": ""}),
                serde_json::json!({"hook_event_name": event, "prompt": " \r\n"}),
                serde_json::json!({"hook_event_name": event, "prompt": 1}),
                serde_json::json!({"hook_event_name": event, "prompt": null}),
            ] {
                assert!(hook_prompt_sha256_16(&payload.to_string(), agent).is_none());
            }
        }

        for agent in [Agent::Claude, Agent::Codex] {
            let mut payload = serde_json::json!({
                "hook_event_name": "UserPromptSubmit", "prompt": "x", "agent_id": "subagent"
            });
            assert!(hook_prompt_sha256_16(&payload.to_string(), agent).is_none());
            for agent_id in [
                serde_json::json!(""),
                serde_json::json!(1),
                serde_json::Value::Null,
            ] {
                payload["agent_id"] = agent_id;
                assert_eq!(
                    hook_prompt_sha256_16(&payload.to_string(), agent),
                    Some(prompt_ack_sha256_16("x"))
                );
            }
        }

        for cursor_version in [serde_json::json!("1"), serde_json::Value::Null] {
            let payload = serde_json::json!({
                "hook_event_name": "UserPromptSubmit", "prompt": "x", "cursor_version": cursor_version
            });
            assert!(hook_prompt_sha256_16(&payload.to_string(), Agent::Claude).is_none());
        }
    }

    #[test]
    fn matching_hook_confirms_a_written_receipt_once() {
        let mut l = SubmitLedger::default();
        written(&mut l, "a", "review this", 5);
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
        let hash = prompt_ack_sha256_16("review this\n");
        assert_eq!(
            l.note_prompt_submitted(pid(1), &hash, 1, 6, 200),
            Some("a".to_string())
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Confirmed, Some(200)));
        let json = serde_json::to_value(l.get("a").unwrap()).unwrap();
        assert_eq!(json["acknowledged"], "confirmed");
        assert_eq!(json["acknowledged_at_ms"], 200);
        // The owner typing the same prompt again confirms nothing further.
        assert_eq!(l.note_prompt_submitted(pid(1), &hash, 1, 7, 300), None);
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Confirmed, Some(200)));
    }

    #[test]
    fn mismatched_text_other_pane_or_earlier_report_stays_unsupported() {
        let mut l = SubmitLedger::default();
        written(&mut l, "a", "review this", 5);
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("other"), 1, 6, 200),
            None
        );
        assert_eq!(
            l.note_prompt_submitted(pid(2), &prompt_ack_sha256_16("review this"), 1, 7, 201),
            None
        );
        // A report ordered before the write completion is not after the write.
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("review this"), 1, 4, 202),
            None
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
    }

    #[test]
    fn hook_before_completion_confirms_once_on_written() {
        let mut l = SubmitLedger::default();
        let f = accept(&mut l, "a", "review this");
        let hash = prompt_ack_sha256_16("review this");
        // Nothing written yet: a report is not kept.
        assert_eq!(l.note_prompt_submitted(pid(1), &hash, 1, 2, 150), None);
        assert!(f.try_commit());
        assert_eq!(l.note_prompt_submitted(pid(1), &hash, 1, 3, 160), None);
        // A second report while the first is pending is not kept for this receipt.
        assert_eq!(l.note_prompt_submitted(pid(1), &hash, 1, 4, 170), None);
        l.complete("a", &Ok(()), f.close(), 5, 180);
        // The report time is clamped to the write time: never acknowledged before written.
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Confirmed, Some(180)));

        // A pending report is dropped when the write does not end `written`.
        let g = accept(&mut l, "b", "next");
        assert!(g.try_commit());
        let next = prompt_ack_sha256_16("next");
        assert_eq!(l.note_prompt_submitted(pid(1), &next, 1, 6, 190), None);
        g.trip(FenceReason::OwnerInput);
        l.complete("b", &Ok(()), g.close(), 7, 195);
        assert_eq!(l.get("b").unwrap().state, SubmitState::Indeterminate);
        assert_eq!(ack(&l, "b"), (SubmitAcknowledged::Unsupported, None));
    }

    #[test]
    fn two_quick_submits_match_their_own_receipts_in_order() {
        let mut l = SubmitLedger::default();
        written(&mut l, "a", "same", 5);
        written(&mut l, "b", "same", 6);
        written(&mut l, "c", "other", 7);
        let same = prompt_ack_sha256_16("same");
        assert_eq!(
            l.note_prompt_submitted(pid(1), &same, 1, 8, 200),
            Some("a".into())
        );
        assert_eq!(
            l.note_prompt_submitted(pid(1), &same, 1, 9, 201),
            Some("b".into())
        );
        assert_eq!(l.note_prompt_submitted(pid(1), &same, 1, 10, 202), None);
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("other"), 1, 11, 203),
            Some("c".into())
        );
    }

    #[test]
    fn a_later_confirmation_retires_earlier_unconfirmed_receipts() {
        let mut l = SubmitLedger::default();
        written(&mut l, "a", "first", 5);
        written(&mut l, "b", "second", 6);
        // The hook for "first" was lost; "second" is confirmed.
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("second"), 1, 7, 200),
            Some("b".into())
        );
        // A later identical owner prompt cannot confirm the older receipt.
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("first"), 1, 8, 201),
            None
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
    }

    /// Accept a submit of `text` on pane 1 in `generation`, then write it at `seq`/`now_ms`.
    fn written_in(
        l: &mut SubmitLedger,
        id: &str,
        text: &str,
        generation: u64,
        seq: u64,
        now_ms: u64,
    ) {
        let mut new = new_receipt(SubmitKind::Submit, 1, text, false);
        new.target_ref = target(generation);
        let f = SubmitFence::new();
        l.insert_accepted(id, new, f.clone(), now_ms)
            .expect("accepted");
        assert!(f.try_commit());
        l.complete(id, &Ok(()), f.close(), seq, now_ms);
        assert_eq!(l.get(id).unwrap().state, SubmitState::Written);
    }

    #[test]
    fn the_same_text_typed_outside_the_window_confirms_nothing() {
        let mut l = SubmitLedger::default();
        written_in(&mut l, "a", "review this", 1, 5, 1_000);
        let hash = prompt_ack_sha256_16("review this");
        let hour_later = 1_000 + 3_600_000;
        assert_eq!(
            l.note_prompt_submitted(pid(1), &hash, 1, 6, hour_later),
            None
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
        // The window edge still confirms.
        let b_at = hour_later + 1_000;
        written_in(&mut l, "b", "next", 1, 7, b_at);
        let next = prompt_ack_sha256_16("next");
        assert_eq!(
            l.note_prompt_submitted(pid(1), &next, 1, 8, b_at + PROMPT_ACK_WINDOW_MS),
            Some("b".into())
        );
    }

    #[test]
    fn a_lost_report_is_not_filled_by_a_later_identical_submit() {
        // A's report was lost long ago; B with the same text gets its own confirmation.
        let mut l = SubmitLedger::default();
        written_in(&mut l, "a", "same", 1, 5, 1_000);
        written_in(&mut l, "b", "same", 1, 6, 1_000 + 3_600_000);
        let same = prompt_ack_sha256_16("same");
        assert_eq!(
            l.note_prompt_submitted(pid(1), &same, 1, 7, 1_000 + 3_600_100),
            Some("b".into())
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
        assert_eq!(
            l.note_prompt_submitted(pid(1), &same, 1, 8, 1_000 + 3_600_200),
            None
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));

        // Within the window: an unmatched report later than the reorder slack retires A.
        let mut l = SubmitLedger::default();
        written_in(&mut l, "a", "same", 1, 5, 1_000);
        let owner_at = 1_001 + PROMPT_ACK_REORDER_SLACK_MS;
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("owner typed"), 1, 6, owner_at),
            None
        );
        written_in(&mut l, "b", "same", 1, 7, owner_at + 100);
        assert_eq!(
            l.note_prompt_submitted(pid(1), &same, 1, 8, owner_at + 200),
            Some("b".into())
        );
        assert_eq!(
            l.note_prompt_submitted(pid(1), &same, 1, 9, owner_at + 300),
            None
        );
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
    }

    #[test]
    fn an_unmatched_report_within_the_reorder_slack_retires_nothing() {
        // The owner's own prompt report overtakes the report for a just-written receipt.
        let mut l = SubmitLedger::default();
        written_in(&mut l, "a", "review this", 1, 5, 1_000);
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("owner typed"), 1, 6, 1_050),
            None
        );
        assert_eq!(
            l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16("review this"), 1, 7, 1_100),
            Some("a".into())
        );
    }

    #[test]
    fn a_generation_change_between_write_and_report_confirms_nothing() {
        let mut l = SubmitLedger::default();
        written_in(&mut l, "a", "review this", 1, 5, 1_000);
        let hash = prompt_ack_sha256_16("review this");
        assert_eq!(l.note_prompt_submitted(pid(1), &hash, 2, 6, 1_100), None);
        assert_eq!(ack(&l, "a"), (SubmitAcknowledged::Unsupported, None));
        // An in-flight write accepted in an older generation is not kept pending either.
        let mut new = new_receipt(SubmitKind::Submit, 1, "next", false);
        new.target_ref = target(1);
        let f = SubmitFence::new();
        l.insert_accepted("b", new, f.clone(), 1_200)
            .expect("accepted");
        assert!(f.try_commit());
        let next = prompt_ack_sha256_16("next");
        assert_eq!(l.note_prompt_submitted(pid(1), &next, 2, 7, 1_250), None);
        l.complete("b", &Ok(()), f.close(), 8, 1_300);
        assert_eq!(ack(&l, "b"), (SubmitAcknowledged::Unsupported, None));
    }

    #[test]
    fn interrupt_and_resume_receipts_are_never_confirmed_by_a_prompt_hook() {
        let mut l = SubmitLedger::default();
        let f = accept_kind(&mut l, "esc", SubmitKind::Interrupt);
        assert!(f.try_commit());
        l.complete("esc", &Ok(()), f.close(), 5, 20);
        accept_resume(&mut l, "resume", 1, "x", Agent::Claude);
        for text in ["x", INTERRUPT_HASH_TEXT] {
            assert_eq!(
                l.note_prompt_submitted(pid(1), &prompt_ack_sha256_16(text), 1, 6, 30),
                None
            );
        }
        assert_eq!(ack(&l, "esc"), (SubmitAcknowledged::Unsupported, None));
        assert_eq!(ack(&l, "resume"), (SubmitAcknowledged::Unsupported, None));
    }

    #[test]
    fn clear_draft_ownership_requires_adjacent_written_receipts() {
        fn pair() -> SubmitLedger {
            let mut ledger = SubmitLedger::default();
            written(&mut ledger, "prompt", "own text", 10);
            let fence = accept_kind(&mut ledger, "interrupt", SubmitKind::Interrupt);
            assert!(fence.try_commit());
            ledger.complete("interrupt", &Ok(()), fence.close(), 11, 101);
            ledger
        }
        let hash = Some(text_sha256_16("own text"));
        assert_eq!(pair().own_restored_text_hash(pid(1), 1), hash);
        let kinds = [
            SubmitKind::Submit,
            SubmitKind::Answer,
            SubmitKind::Interrupt,
            SubmitKind::Reset,
            SubmitKind::Resume,
            SubmitKind::ClearDraft,
            SubmitKind::Unknown,
        ];
        let states = [
            SubmitState::Accepted,
            SubmitState::Written,
            SubmitState::Fenced,
            SubmitState::Indeterminate,
            SubmitState::Launched,
            SubmitState::Failed,
            SubmitState::Unknown,
        ];
        for which in ["prompt", "interrupt"] {
            for kind in kinds {
                for state in states {
                    let mut ledger = pair();
                    let entry = ledger.entries.get_mut(which).unwrap();
                    entry.receipt.kind = kind;
                    entry.receipt.state = state;
                    let expected = state == SubmitState::Written
                        && if which == "prompt" {
                            matches!(kind, SubmitKind::Submit | SubmitKind::Answer)
                        } else {
                            kind == SubmitKind::Interrupt
                        };
                    assert_eq!(
                        ledger.own_restored_text_hash(pid(1), 1).is_some(),
                        expected,
                        "{which} {kind:?} {state:?}"
                    );
                }
            }
        }
        for kind in kinds {
            for state in states {
                let mut ledger = pair();
                accept_kind(&mut ledger, "intervening", kind);
                ledger.entries.get_mut("intervening").unwrap().receipt.state = state;
                assert!(
                    ledger.own_restored_text_hash(pid(1), 1).is_none(),
                    "{kind:?} {state:?}"
                );
            }
        }
        let mut ledger = pair();
        ledger.entries.get_mut("prompt").unwrap().own_text_sha256_16 = None;
        assert!(ledger.own_restored_text_hash(pid(1), 1).is_none());
        for which in ["prompt", "interrupt"] {
            for seq in [None, Some(10), Some(11), Some(12)] {
                let mut ledger = pair();
                ledger.entries.get_mut(which).unwrap().written_seq = seq;
                let expected = if which == "prompt" {
                    seq.is_some_and(|s| s < 11)
                } else {
                    seq.is_some_and(|s| s > 10)
                };
                assert_eq!(ledger.own_restored_text_hash(pid(1), 1).is_some(), expected);
            }
            let mut ledger = pair();
            ledger
                .entries
                .get_mut(which)
                .unwrap()
                .receipt
                .target_ref
                .generation = 0;
            assert!(ledger.own_restored_text_hash(pid(1), 1).is_none());
        }
        let mut ledger = pair();
        assert!(ledger.own_restored_text_hash(pid(1), 2).is_none());
        accept_kind(&mut ledger, "other-pane", SubmitKind::ClearDraft);
        ledger.entries.get_mut("other-pane").unwrap().pane = pid(2);
        // Put the other pane between the pair, then after them: neither can interfere.
        ledger.entries.get_mut("other-pane").unwrap().order = 1;
        ledger.entries.get_mut("interrupt").unwrap().order = 2;
        assert_eq!(ledger.own_restored_text_hash(pid(1), 1), hash);
        ledger.entries.get_mut("other-pane").unwrap().order = 3;
        assert_eq!(ledger.own_restored_text_hash(pid(1), 1), hash);
        accept_kind(&mut ledger, "old-generation", SubmitKind::ClearDraft);
        ledger
            .entries
            .get_mut("old-generation")
            .unwrap()
            .receipt
            .target_ref
            .generation = 0;
        assert_eq!(ledger.own_restored_text_hash(pid(1), 1), hash);
    }

    #[test]
    fn clear_draft_own_hash_preserves_existing_hash_contracts() {
        let text = "  first\tline\r\n second\u{2003}line  ";
        for kind in [
            SubmitKind::Submit,
            SubmitKind::Answer,
            SubmitKind::Reset,
            SubmitKind::Interrupt,
            SubmitKind::Resume,
            SubmitKind::ClearDraft,
            SubmitKind::Unknown,
        ] {
            let new = new_receipt(kind, 1, text, false);
            assert_eq!(new.text_sha256_16, text_sha256_16(text));
            assert_ne!(new.text_sha256_16, text_sha256_16("first line second line"));
            assert_eq!(
                new.ack_sha256_16,
                matches!(
                    kind,
                    SubmitKind::Submit | SubmitKind::Answer | SubmitKind::Reset
                )
                .then(|| prompt_ack_sha256_16(text))
            );
            let own = matches!(kind, SubmitKind::Submit | SubmitKind::Answer)
                .then(|| text_sha256_16("first line second line"));
            assert_eq!(new.own_text_sha256_16, own);
            let mut ledger = SubmitLedger::default();
            let r = ledger
                .insert_accepted("id", new, SubmitFence::new(), 1)
                .unwrap();
            assert_eq!(ledger.entries["id"].own_text_sha256_16, own);
            assert_eq!(r.text_sha256_16, text_sha256_16(text));
            assert!(!serde_json::to_string(&r).unwrap().contains("own_text"));
        }
        assert_ne!(
            text_sha256_16(CLEAR_DRAFT_HASH_TEXT),
            text_sha256_16(INTERRUPT_HASH_TEXT)
        );
    }
}

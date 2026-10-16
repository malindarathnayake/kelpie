//! Pure per-pane supervisor registry: generations, lineage and question bookkeeping.
//! No App access and no I/O.

use std::collections::{HashMap, VecDeque};

use crate::agent_resume::AgentSessionRef;
use crate::api::schema::supervisor::{
    Confidence, FactSource, GenerationReason, LineageEntry, PendingQuestion, QuestionKind,
    QuestionResolution, ResolutionBy, TargetRef,
};
use crate::detect::{parse_agent_label, AgentState};
use crate::layout::PaneId;
use crate::supervisor::profile::profile_for;
use crate::supervisor::question::{
    content_rows, detect_prose_question, normalize, question_id, sanitize_excerpt,
};

pub const MAX_OPEN_QUESTIONS: usize = 16;
pub const MAX_RESOLVED_QUESTIONS: usize = 8;
pub const MAX_LINEAGE: usize = 8;
pub const MAX_EXCERPT_CHARS: usize = 200;
/// Minimum Idle dwell before the one-shot quiescence rescan may be requested.
pub const QUIESCENCE_RESCAN_MS: u64 = 1500;

/// Serde wire name of a generation reason.
fn reason_name(reason: GenerationReason) -> &'static str {
    match reason {
        GenerationReason::SessionChanged => "session_changed",
        GenerationReason::ResetWritten => "reset_written",
        GenerationReason::AgentStarted => "agent_started",
        GenerationReason::AgentExited => "agent_exited",
        GenerationReason::PaneMoved => "pane_moved",
        GenerationReason::Unknown => "unknown",
    }
}

/// Derives why a generation changed. Priority: released > session changed > started.
pub fn generation_reason(
    session_ref_changed: bool,
    agent_released: bool,
    agent_started: bool,
) -> Option<GenerationReason> {
    if agent_released {
        Some(GenerationReason::AgentExited)
    } else if session_ref_changed {
        Some(GenerationReason::SessionChanged)
    } else if agent_started {
        Some(GenerationReason::AgentStarted)
    } else {
        None
    }
}

/// Terminal facts App captures around one accepted terminal-state mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalMutationFacts {
    /// Session identity (kind and value) before the mutation.
    pub session_before: Option<AgentSessionRef>,
    /// Session identity (kind and value) after the mutation.
    pub session_after: Option<AgentSessionRef>,
    pub agent_released: bool,
    pub agent_started: bool,
    /// Effective terminal state after the mutation.
    pub final_state: AgentState,
}

#[derive(Debug, Default)]
pub struct SupervisorState {
    panes: HashMap<PaneId, PaneSupervisorState>,
    /// Registry-wide monotonic event counter; orders hook reports against turn
    /// starts without relying on wall-clock milliseconds.
    event_seq: u64,
    /// `agent.submit` receipts (fork, S2/S3).
    pub submits: crate::supervisor::submit::SubmitLedger,
    /// Normalized session start source of the session report App is applying right now
    /// (`AgentSessionReported` only); None for every other mutation.
    mutation_start_source: Option<String>,
    /// The session ref (and agent label) carried by the `AgentSessionReported` App is
    /// applying right now; None for every other mutation. `agent.resume` evidence is
    /// settled only from this reported ref, never from the terminal's resolved identity.
    mutation_reported_session: Option<(String, Option<AgentSessionRef>)>,
    /// Fork: owner-granted delegations; survive generation bumps, end on pane close.
    pub delegations: crate::supervisor::delegation::Delegations,
}

#[derive(Debug, Clone)]
pub struct PaneSupervisorState {
    /// Starts at 1 on first sight.
    pub generation: u64,
    /// Conversation epoch used in question ids. Starts at 1 and bumps with every
    /// generation change except a pane move, which only changes routing.
    pub question_epoch: u64,
    /// Newest last, capped at `MAX_LINEAGE` (oldest dropped).
    pub lineage: VecDeque<LineageEntry>,
    /// Open and resolved questions in insertion order.
    pub questions: Vec<PendingQuestion>,
    /// Set when an open question was dropped for the cap. Sticky for the pane.
    pub truncated_questions: bool,
    /// Unresolved questions dropped at the open cap (drop events, saturating). A
    /// dropped question was never resolved, so it still counts as open for
    /// completion. Reset only by a conversation-replacing generation change.
    pub unresolved_overflow: u32,
    /// Last known session identity of the pane's terminal (kind and value only).
    /// Kept across loss (some to none) so a later different identity is a replacement.
    pub last_session_ref: Option<AgentSessionRef>,
    /// A written lazy reset has already replaced this conversation; its next
    /// different reported session verifies the reset without replacing it again.
    lazy_reset_pending: bool,
    /// Set when a resolved question was dropped for the cap.
    pub truncated_resolved: bool,
    pub turn_started_ms: Option<u64>,
    /// Event sequence of the most recent Working transition.
    pub turn_seq: Option<u64>,
    pub idle_since_ms: Option<u64>,
    pub rescanned_this_idle: bool,
    pub last_transition_ms: Option<u64>,
    /// Generation in which a background_jobs token was last reported.
    pub background_hook_generation: Option<u64>,
    /// Event sequence of the last `background_jobs` report.
    pub background_hook_seq: Option<u64>,
    /// Wall-clock time of the last `background_jobs` report.
    pub background_hook_ms: Option<u64>,
    pub exited: bool,
    /// Structured-approval scan for the Blocked transition with this sequence;
    /// independent of `pending_prose`. Cleared on generation change.
    pub pending_blocked: Option<u64>,
    /// Prose-question scan requested by an Idle transition or quiescence rescan.
    pub pending_prose: bool,
    /// State set by the most recent `on_transition`; None after a generation change.
    pub last_state: Option<AgentState>,
}

impl PaneSupervisorState {
    fn new() -> Self {
        Self {
            generation: 1,
            question_epoch: 1,
            lineage: VecDeque::new(),
            questions: Vec::new(),
            truncated_questions: false,
            unresolved_overflow: 0,
            last_session_ref: None,
            lazy_reset_pending: false,
            truncated_resolved: false,
            turn_started_ms: None,
            turn_seq: None,
            idle_since_ms: None,
            rescanned_this_idle: false,
            last_transition_ms: None,
            background_hook_generation: None,
            background_hook_seq: None,
            background_hook_ms: None,
            exited: false,
            pending_blocked: None,
            pending_prose: false,
            last_state: None,
        }
    }

    fn enforce_caps(&mut self) {
        while self
            .questions
            .iter()
            .filter(|q| q.resolution.is_none())
            .count()
            > MAX_OPEN_QUESTIONS
        {
            if let Some(i) = self.questions.iter().position(|q| q.resolution.is_none()) {
                self.questions.remove(i);
                self.truncated_questions = true;
                self.unresolved_overflow = self.unresolved_overflow.saturating_add(1);
            } else {
                break;
            }
        }
        while self
            .questions
            .iter()
            .filter(|q| q.resolution.is_some())
            .count()
            > MAX_RESOLVED_QUESTIONS
        {
            if let Some(i) = self.questions.iter().position(|q| q.resolution.is_some()) {
                self.questions.remove(i);
                self.truncated_resolved = true;
            } else {
                break;
            }
        }
    }
}

/// Why `agent.answer` rejected a question id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerReject {
    /// Unknown, resolved or superseded, not the newest open question, or no longer
    /// re-detected on screen.
    NotCurrent,
    /// A structured approval or dialog: the owner answers in the pane.
    NotAnswerable,
}

/// target_ref validator error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleTarget {
    pub current_generation: Option<u64>,
}

impl SupervisorState {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pane(&self, pane: PaneId) -> Option<&PaneSupervisorState> {
        self.panes.get(&pane)
    }

    /// Returns the pane state, inserting generation 1 if missing.
    pub fn pane_mut(&mut self, pane: PaneId) -> &mut PaneSupervisorState {
        self.panes
            .entry(pane)
            .or_insert_with(PaneSupervisorState::new)
    }

    /// Returns the next registry-wide event sequence number (starts at 1).
    fn next_event_seq(&mut self) -> u64 {
        self.event_seq += 1;
        self.event_seq
    }

    /// Bumps the routing generation. A pane move changes only routing: open
    /// questions, pending scans and turn/idle markers are kept and the question
    /// epoch is unchanged. Every other reason replaces the conversation.
    pub fn bump_generation(&mut self, pane: PaneId, reason: GenerationReason, now_ms: u64) -> u64 {
        self.bump_generation_except_reset(pane, reason, now_ms, None)
    }

    fn bump_generation_except_reset(
        &mut self,
        pane: PaneId,
        reason: GenerationReason,
        now_ms: u64,
        reset_submit_id: Option<&str>,
    ) -> u64 {
        // Any generation change fences this pane's queued submissions before their first byte.
        self.submits.trip_pane(
            pane,
            crate::supervisor::fence::FenceReason::GenerationChanged,
        );
        if reason != GenerationReason::PaneMoved {
            // A conversation replacement other than an already-classified clear: a written
            // reset still pending can no longer be verified.
            if let Some(submit_id) = reset_submit_id {
                self.submits
                    .note_other_replacement_except(pane, Some(submit_id));
            } else {
                self.submits.note_other_replacement(pane);
            }
        }
        let state = self.pane_mut(pane);
        let old = state.generation;
        state.generation += 1;
        let new = state.generation;
        state.lineage.push_back(LineageEntry {
            generation: new,
            reason,
            at_ms: now_ms,
        });
        while state.lineage.len() > MAX_LINEAGE {
            state.lineage.pop_front();
        }
        if reason == GenerationReason::PaneMoved {
            // The terminal and its tokens move with the pane, so hook evidence that
            // was current stays current; turn ordering still applies.
            if state.background_hook_generation == Some(old) {
                state.background_hook_generation = Some(new);
            }
            tracing::debug!(
                event = "supervisor.generation",
                subsystem = "supervisor",
                pane_id = pane.raw(),
                generation = new,
                reason = reason_name(reason)
            );
            return new;
        }
        state.question_epoch += 1;
        state.lazy_reset_pending = false;
        for q in state
            .questions
            .iter_mut()
            .filter(|q| q.resolution.is_none())
        {
            q.resolution = Some(QuestionResolution {
                at_ms: now_ms,
                by: ResolutionBy::Superseded,
            });
        }
        state.turn_started_ms = None;
        state.turn_seq = None;
        state.idle_since_ms = None;
        state.rescanned_this_idle = false;
        state.pending_blocked = None;
        state.pending_prose = false;
        state.last_state = None;
        state.unresolved_overflow = 0;
        state.enforce_caps();
        tracing::debug!(
            event = "supervisor.generation",
            subsystem = "supervisor",
            pane_id = pane.raw(),
            generation = new,
            reason = reason_name(reason)
        );
        new
    }

    /// Classifies a session identity observation and records the last known identity.
    /// Only a known identity replaced by a different one is a replacement: none to
    /// some is enrichment and some to none is loss. `before` seeds the last known
    /// identity when the registry has none (first sight or restart).
    pub fn observe_session_ref(
        &mut self,
        pane: PaneId,
        before: Option<AgentSessionRef>,
        after: Option<AgentSessionRef>,
    ) -> bool {
        if before.is_none() && after.is_none() && !self.panes.contains_key(&pane) {
            return false;
        }
        if let Some(session) = after.as_ref() {
            self.delegations.note_session(pane, &session.value);
        }
        let state = self.pane_mut(pane);
        let last_known = state.last_session_ref.take().or(before);
        let replaced = matches!((&last_known, &after), (Some(known), Some(now)) if known != now);
        state.last_session_ref = after.or(last_known);
        replaced
    }

    /// Records one accepted terminal-state mutation: classifies the session identity,
    /// bumps the generation when the conversation was replaced, and re-arms the Idle
    /// prose scan when the pane is Idle after the bump (a replacement without a
    /// state edge would otherwise never be scanned). Returns the bump reason.
    pub fn on_terminal_mutation(
        &mut self,
        pane: PaneId,
        facts: TerminalMutationFacts,
        now_ms: u64,
    ) -> Option<GenerationReason> {
        let resume_evidence = self
            .mutation_reported_session
            .as_ref()
            .filter(|(label, _)| parse_agent_label(label).and_then(profile_for).is_some())
            .and_then(|(_, session)| session.clone());
        let reported_session_changed = matches!(
            (self.pane(pane).and_then(|state| state.last_session_ref.as_ref())
                .or(facts.session_before.as_ref()), resume_evidence.as_ref()),
            (Some(before), Some(reported)) if before != reported
        );
        let lazy_reset_evidence = reported_session_changed
            && self
                .pane(pane)
                .is_some_and(|state| state.lazy_reset_pending);
        let session_changed =
            self.observe_session_ref(pane, facts.session_before, facts.session_after.clone());
        if lazy_reset_evidence && !facts.agent_released && !facts.agent_started {
            if let Some(session) = resume_evidence.as_ref() {
                let seq = self.next_event_seq();
                let state = self.pane_mut(pane);
                let generation = state.generation;
                state.lazy_reset_pending = false;
                state.last_session_ref = Some(session.clone());
                self.submits
                    .note_written_clear_session(pane, session, generation, seq);
            }
        }
        // Only the expected identity replacement is already accounted for. A
        // simultaneous release or start still replaces the conversation.
        let reason = generation_reason(
            session_changed && !lazy_reset_evidence,
            facts.agent_released,
            facts.agent_started,
        );
        let Some(reason) = reason else {
            if let Some(session) = resume_evidence {
                self.note_resume_session(pane, &session);
            }
            return None;
        };
        if reason == GenerationReason::SessionChanged
            && self.mutation_start_source.as_deref() == Some("clear")
        {
            if let Some(after) = facts.session_after.as_ref() {
                // Classify before the bump trips the fences, so an `agent.reset` write the
                // clear interrupted maps to `written` instead of indeterminate.
                let seq = self.next_event_seq();
                let new_generation = self.pane_mut(pane).generation + 1;
                if reported_session_changed && resume_evidence.as_ref() == Some(after) {
                    self.submits
                        .note_clear_session(pane, after, new_generation, seq);
                } else {
                    self.submits
                        .note_immediate_clear_session(pane, after, new_generation, seq);
                }
            }
        }
        let new_generation = self.record_generation_change(pane, reason, now_ms);
        if reason == GenerationReason::AgentStarted {
            // A new agent's first session ref is enrichment, not a replacement of the
            // previous agent's session. Kept across AgentExited so a session-only new
            // conversation after an exit still classifies as a replacement.
            self.pane_mut(pane).last_session_ref = facts.session_after;
            let seq = self.next_event_seq();
            self.submits.note_agent_started(pane, new_generation, seq);
        }
        if let Some(session) = resume_evidence {
            self.note_resume_session(pane, &session);
        }
        if facts.final_state == AgentState::Idle {
            let state = self.pane_mut(pane);
            state.last_state = Some(AgentState::Idle);
            state.idle_since_ms = Some(now_ms);
            state.rescanned_this_idle = false;
            state.pending_prose = true;
        }
        Some(reason)
    }

    /// Marks the start of one `AgentSessionReported` mutation with its normalized start
    /// source, agent label and reported session ref; App calls `end_session_report` right
    /// after that one mutation.
    pub fn begin_session_report(
        &mut self,
        start_source: Option<String>,
        agent_label: &str,
        reported: Option<AgentSessionRef>,
    ) {
        self.mutation_start_source = start_source;
        self.mutation_reported_session = Some((agent_label.to_string(), reported));
    }

    pub fn end_session_report(&mut self) {
        self.mutation_start_source = None;
        self.mutation_reported_session = None;
    }

    /// `agent.resume` evidence: the session ref a supported-agent report carried, applied
    /// (accepted) by the terminal in this mutation.
    fn note_resume_session(&mut self, pane: PaneId, session: &AgentSessionRef) {
        let Some(reported_agent) = self
            .mutation_reported_session
            .as_ref()
            .and_then(|(label, _)| parse_agent_label(label))
        else {
            return;
        };
        let seq = self.next_event_seq();
        let generation = self.pane_mut(pane).generation;
        let source = self.mutation_start_source.clone();
        self.submits.note_resume_session(
            pane,
            reported_agent,
            session,
            source.as_deref(),
            generation,
            seq,
        );
    }

    /// `agent.resume`: the start command was sent but the start path then failed.
    pub fn mark_resume_unconfirmed(
        &mut self,
        submit_id: &str,
        code: &str,
    ) -> Option<crate::api::schema::supervisor::SubmitReceipt> {
        let seq = self.next_event_seq();
        self.submits.mark_start_unconfirmed(submit_id, code, seq)
    }

    /// `agent.resume`: the launch was accepted now; later events order after it.
    pub fn mark_resume_launched(
        &mut self,
        submit_id: &str,
        now_ms: u64,
    ) -> Option<crate::api::schema::supervisor::SubmitReceipt> {
        let seq = self.next_event_seq();
        self.submits.mark_launched(submit_id, seq, now_ms)
    }

    /// An accepted hook report whose final state is Working starts a new turn for
    /// background-evidence ordering, with or without a state edge, so earlier zero
    /// counts stop being current. `turn_started_ms` is unchanged.
    pub fn note_working_report(&mut self, pane: PaneId) {
        let seq = self.next_event_seq();
        self.pane_mut(pane).turn_seq = Some(seq);
    }

    /// Fork (B1): a prompt-submit hook of `pane` reported `hash`, ordered on the registry
    /// event counter after every write completion already handled, and matched only against
    /// receipts of the pane's current generation. Returns the confirmed submit id, if any.
    pub fn note_prompt_submitted(
        &mut self,
        pane: PaneId,
        hash: &str,
        now_ms: u64,
    ) -> Option<String> {
        let seq = self.next_event_seq();
        // A pane with no supervisor state yet is at generation 1 (see `pane_mut`).
        let generation = self.pane(pane).map_or(1, |state| state.generation);
        self.submits
            .note_prompt_submitted(pane, hash, generation, seq, now_ms)
    }

    /// Bumps the generation and records whether the new generation is an exit.
    pub fn record_generation_change(
        &mut self,
        pane: PaneId,
        reason: GenerationReason,
        now_ms: u64,
    ) -> u64 {
        let new = self.bump_generation(pane, reason, now_ms);
        self.pane_mut(pane).exited = reason == GenerationReason::AgentExited;
        new
    }

    pub fn target_ref(&mut self, pane: PaneId, public_pane_id: &str) -> TargetRef {
        let generation = self.pane_mut(pane).generation;
        TargetRef {
            server_epoch: crate::supervisor::server_epoch().to_string(),
            pane_id: public_pane_id.to_string(),
            generation,
        }
    }

    /// Full target_ref check (epoch, pane, generation) used by `agent.submit`.
    pub fn check_target_ref(
        &self,
        target: &TargetRef,
        pane: Option<PaneId>,
    ) -> Result<(), StaleTarget> {
        if target.server_epoch != crate::supervisor::server_epoch() {
            return Err(StaleTarget {
                current_generation: None,
            });
        }
        let Some(state) = pane.and_then(|p| self.panes.get(&p)) else {
            return Err(StaleTarget {
                current_generation: None,
            });
        };
        if state.generation != target.generation {
            return Err(StaleTarget {
                current_generation: Some(state.generation),
            });
        }
        Ok(())
    }

    /// Returns false when a question with the same id is already present.
    pub fn insert_question(&mut self, pane: PaneId, mut question: PendingQuestion) -> bool {
        if question.excerpt.chars().count() > MAX_EXCERPT_CHARS {
            question.excerpt = question.excerpt.chars().take(MAX_EXCERPT_CHARS).collect();
        }
        let state = self.pane_mut(pane);
        if state
            .questions
            .iter()
            .any(|q| q.question_id == question.question_id)
        {
            return false;
        }
        state.questions.push(question);
        state.enforce_caps();
        true
    }

    /// `agent.answer` question checks against a fresh grid snapshot (`screen_text`, the
    /// same 24-row unwrapped text the scan reads). Pure; never resolves the question.
    ///
    /// Accepted only when the id names an open `prose_question` of this pane that is the
    /// newest open question, and a prose question re-detected on screen now hashes to the
    /// same id (same question epoch, same normalized text). A structured question is not
    /// answerable (the owner acts in the pane).
    pub fn check_answer(
        &self,
        pane: PaneId,
        id: &str,
        screen_text: &str,
    ) -> Result<(), AnswerReject> {
        let state = self.panes.get(&pane).ok_or(AnswerReject::NotCurrent)?;
        let question = state
            .questions
            .iter()
            .find(|q| q.question_id == id)
            .ok_or(AnswerReject::NotCurrent)?;
        if question.resolution.is_some() {
            return Err(AnswerReject::NotCurrent);
        }
        if question.kind != QuestionKind::ProseQuestion {
            return Err(AnswerReject::NotAnswerable);
        }
        let newest_open = state
            .questions
            .iter()
            .rev()
            .find(|q| q.resolution.is_none())
            .map(|q| q.question_id.as_str());
        if newest_open != Some(id) {
            return Err(AnswerReject::NotCurrent);
        }
        let rows = content_rows(screen_text);
        let on_screen = detect_prose_question(&rows).is_some_and(|text| {
            question_id(pane.raw(), state.question_epoch, "prose", &normalize(&text)) == id
        });
        if !on_screen {
            return Err(AnswerReject::NotCurrent);
        }
        Ok(())
    }

    /// Records that a `background_jobs` token was reported in the current generation,
    /// with its event sequence and report time.
    pub fn note_background_hook(&mut self, pane: PaneId, now_ms: u64) {
        let seq = self.next_event_seq();
        let state = self.pane_mut(pane);
        state.background_hook_generation = Some(state.generation);
        state.background_hook_seq = Some(seq);
        state.background_hook_ms = Some(now_ms);
    }

    /// True when a `background_jobs` token was reported in the pane's current
    /// generation and after the most recent Working transition (if any).
    pub fn background_hook_current(&self, pane: PaneId) -> bool {
        self.panes.get(&pane).is_some_and(|s| {
            s.background_hook_generation == Some(s.generation)
                && match s.turn_seq {
                    None => true,
                    Some(turn) => s.background_hook_seq.is_some_and(|h| h > turn),
                }
        })
    }

    /// Wall-clock time of the last `background_jobs` report, if any.
    pub fn background_hook_ms(&self, pane: PaneId) -> Option<u64> {
        self.panes.get(&pane).and_then(|s| s.background_hook_ms)
    }

    /// Note: `App::handle_pane_died` does not call this; a dying pane's submissions are
    /// fenced by the AgentExited generation bump, or end `indeterminate{pty_closed}` when
    /// the writer fails.
    pub fn remove_pane(&mut self, pane: PaneId) {
        self.submits
            .trip_pane(pane, crate::supervisor::fence::FenceReason::PaneClosed);
        self.panes.remove(&pane);
        self.delegations.remove_pane(pane);
    }

    /// Registry order and current generation for an operator handoff armed now.
    /// Returns `(generation, armed_seq)`.
    pub fn note_handoff_armed(&mut self, pane: PaneId) -> (u64, u64) {
        let seq = self.next_event_seq();
        (self.pane_mut(pane).generation, seq)
    }

    /// Applies a submission's write result, ordered on the registry event counter so a
    /// later turn end or clear session can be recognised as following the write.
    pub fn complete_submit(
        &mut self,
        submit_id: &str,
        result: &std::io::Result<()>,
        fence: crate::supervisor::fence::FenceSnapshot,
        now_ms: u64,
    ) {
        let seq = self.next_event_seq();
        self.submits.complete(submit_id, result, fence, seq, now_ms);
        if let Some(pane) = self.submits.newly_written_lazy_reset(submit_id, seq) {
            self.bump_generation_except_reset(
                pane,
                GenerationReason::ResetWritten,
                now_ms,
                Some(submit_id),
            );
            let state = self.pane_mut(pane);
            state.lazy_reset_pending = true;
            state.exited = false;
        }
    }

    /// Records an agent state transition and marks the scan App performs after the event.
    /// Never reads the grid and never changes question resolution.
    pub fn on_transition(
        &mut self,
        pane: PaneId,
        _previous: AgentState,
        new: AgentState,
        state_change_seq: u64,
        now_ms: u64,
    ) {
        // A Working edge never changes a receipt: delivery ends at `written`.
        let seq = matches!(new, AgentState::Working).then(|| self.next_event_seq());
        if matches!(new, AgentState::Idle | AgentState::Blocked) {
            // Idle or AwaitingHuman edge: an uncorrelated observation recorded on a written
            // `agent.interrupt` receipt, never an acknowledgement.
            let end_seq = self.next_event_seq();
            self.submits.note_turn_end(pane, end_seq, now_ms);
        }
        let state = self.pane_mut(pane);
        state.last_transition_ms = Some(now_ms);
        state.last_state = Some(new);
        if !matches!(new, AgentState::Idle) {
            // Leaving (or never entering) Idle: no quiescence rescan, no stale prose scan.
            state.idle_since_ms = None;
            state.rescanned_this_idle = false;
            state.pending_prose = false;
        }
        match new {
            AgentState::Working => {
                state.turn_started_ms = Some(now_ms);
                state.turn_seq = seq;
            }
            AgentState::Idle => {
                state.idle_since_ms = Some(now_ms);
                state.rescanned_this_idle = false;
                state.pending_prose = true;
            }
            AgentState::Blocked => {
                state.pending_blocked = Some(state_change_seq);
            }
            AgentState::Unknown => {}
        }
    }

    /// Consumes the pending scan for `pane` against the given grid text.
    pub fn apply_scan(&mut self, pane: PaneId, text: &str, now_ms: u64) {
        let state = self.pane_mut(pane);
        let blocked = state.pending_blocked.take();
        let prose = std::mem::take(&mut state.pending_prose);
        if blocked.is_none() && !prose {
            return;
        }
        let epoch = state.question_epoch;
        let still_blocked = matches!(state.last_state, Some(AgentState::Blocked));
        let rows = content_rows(text);
        if let Some(state_change_seq) = blocked {
            let question = PendingQuestion {
                question_id: question_id(
                    pane.raw(),
                    epoch,
                    "blocked",
                    &state_change_seq.to_string(),
                ),
                kind: QuestionKind::StructuredApproval,
                // If the pane left Blocked before the scan ran, the screen no longer
                // shows the approval prompt; do not copy it into the excerpt.
                excerpt: if still_blocked {
                    rows.last()
                        .map(|r| sanitize_excerpt(r.trim()))
                        .unwrap_or_default()
                } else {
                    String::new()
                },
                asked_at_ms: now_ms,
                source: FactSource::ManifestRule,
                confidence: Confidence::High,
                possible_response_at_ms: None,
                resolution: None,
            };
            self.insert_question(pane, question);
        }
        if prose {
            // The id hashes the full question; `insert_question` caps the excerpt.
            if let Some(text) = detect_prose_question(&rows) {
                let question = PendingQuestion {
                    question_id: question_id(pane.raw(), epoch, "prose", &normalize(&text)),
                    kind: QuestionKind::ProseQuestion,
                    excerpt: text,
                    asked_at_ms: now_ms,
                    source: FactSource::ScreenHeuristic,
                    confidence: Confidence::Low,
                    possible_response_at_ms: None,
                    resolution: None,
                };
                self.insert_question(pane, question);
            }
        }
    }

    pub fn panes_pending_scan(&self) -> Vec<PaneId> {
        self.panes
            .iter()
            .filter(|(_, s)| s.pending_blocked.is_some() || s.pending_prose)
            .map(|(p, _)| *p)
            .collect()
    }

    pub fn clear_pending_scan(&mut self, pane: PaneId) {
        if let Some(state) = self.panes.get_mut(&pane) {
            state.pending_blocked = None;
            state.pending_prose = false;
        }
    }

    /// Marks a one-shot prose rescan once the pane has been Idle for at least
    /// `QUIESCENCE_RESCAN_MS`. Only marks; App performs the scan.
    pub fn request_quiescence_rescan(&mut self, pane: PaneId, now_ms: u64) -> bool {
        let Some(state) = self.panes.get_mut(&pane) else {
            return false;
        };
        let Some(idle_since) = state.idle_since_ms else {
            return false;
        };
        if !matches!(state.last_state, Some(AgentState::Idle))
            || state.rescanned_this_idle
            || now_ms.saturating_sub(idle_since) < QUIESCENCE_RESCAN_MS
        {
            return false;
        }
        state.rescanned_this_idle = true;
        state.pending_prose = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::supervisor::{
        Confidence, FactSource, QuestionKind, ResetEvidence, ResumeEvidence, SubmitAcknowledged,
        SubmitKind, SubmitState,
    };
    use crate::detect::Agent;
    use crate::supervisor::fence::{fenced_error, FenceReason, SubmitFence};
    use crate::supervisor::profile::ResetEvidenceMode;
    use crate::supervisor::submit::{prompt_ack_sha256_16, text_sha256_16, NewReceipt};

    fn pid(n: u32) -> PaneId {
        PaneId::from_raw(n)
    }

    fn q(id: &str, resolution: Option<QuestionResolution>) -> PendingQuestion {
        PendingQuestion {
            question_id: id.to_string(),
            kind: QuestionKind::ProseQuestion,
            excerpt: String::new(),
            asked_at_ms: 1,
            source: FactSource::Derived,
            confidence: Confidence::Low,
            possible_response_at_ms: None,
            resolution,
        }
    }

    fn resolved(by: ResolutionBy) -> Option<QuestionResolution> {
        Some(QuestionResolution { at_ms: 5, by })
    }

    #[test]
    fn registry_first_sight_is_generation_1_and_bump_increments() {
        let mut s = SupervisorState::new();
        assert!(s.pane(pid(1)).is_none());
        assert_eq!(s.pane_mut(pid(1)).generation, 1);
        assert_eq!(
            s.bump_generation(pid(1), GenerationReason::AgentStarted, 10),
            2
        );
        assert_eq!(s.pane(pid(1)).unwrap().generation, 2);
    }

    #[test]
    fn registry_lineage_caps_at_8_keeping_newest() {
        let mut s = SupervisorState::new();
        for i in 0..10 {
            s.bump_generation(pid(1), GenerationReason::SessionChanged, i);
        }
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.lineage.len(), MAX_LINEAGE);
        assert_eq!(p.lineage.back().unwrap().generation, 11);
        assert_eq!(p.lineage.front().unwrap().generation, 4);
    }

    #[test]
    fn registry_bump_supersedes_open_questions_only() {
        let mut s = SupervisorState::new();
        s.insert_question(pid(1), q("open", None));
        s.insert_question(pid(1), q("done", resolved(ResolutionBy::Unknown)));
        s.bump_generation(pid(1), GenerationReason::AgentStarted, 99);
        let p = s.pane(pid(1)).unwrap();
        let open = p
            .questions
            .iter()
            .find(|x| x.question_id == "open")
            .unwrap();
        assert_eq!(
            open.resolution,
            Some(QuestionResolution {
                at_ms: 99,
                by: ResolutionBy::Superseded
            })
        );
        let done = p
            .questions
            .iter()
            .find(|x| x.question_id == "done")
            .unwrap();
        assert_eq!(done.resolution, resolved(ResolutionBy::Unknown));
    }

    #[test]
    fn registry_bump_invalidates_hook_generation() {
        let mut s = SupervisorState::new();
        {
            let p = s.pane_mut(pid(1));
            p.background_hook_generation = Some(1);
            p.turn_started_ms = Some(3);
            p.idle_since_ms = Some(4);
            p.rescanned_this_idle = true;
        }
        s.bump_generation(pid(1), GenerationReason::AgentExited, 20);
        let p = s.pane(pid(1)).unwrap();
        assert_ne!(p.background_hook_generation, Some(p.generation));
        assert_eq!(p.turn_started_ms, None);
        assert_eq!(p.idle_since_ms, None);
        assert!(!p.rescanned_this_idle);
    }

    #[test]
    fn supervisor_background_hook_generation_goes_stale_on_bump() {
        let mut s = SupervisorState::new();
        assert!(!s.background_hook_current(pid(1)));
        s.note_background_hook(pid(1), 1);
        assert!(s.background_hook_current(pid(1)));
        s.bump_generation(pid(1), GenerationReason::AgentStarted, 5);
        assert!(!s.background_hook_current(pid(1)));
    }

    #[test]
    fn registry_check_target_ref_rejects_old_epoch_old_generation_unknown_pane() {
        let mut s = SupervisorState::new();
        let t = s.target_ref(pid(1), "p_1");
        assert_eq!(s.check_target_ref(&t, Some(pid(1))), Ok(()));

        let mut old_epoch = t.clone();
        old_epoch.server_epoch = "deadbeef".to_string();
        assert_eq!(
            s.check_target_ref(&old_epoch, Some(pid(1))),
            Err(StaleTarget {
                current_generation: None
            })
        );

        assert_eq!(
            s.check_target_ref(&t, Some(pid(2))),
            Err(StaleTarget {
                current_generation: None
            })
        );
        assert_eq!(
            s.check_target_ref(&t, None),
            Err(StaleTarget {
                current_generation: None
            })
        );

        s.bump_generation(pid(1), GenerationReason::PaneMoved, 5);
        assert_eq!(
            s.check_target_ref(&t, Some(pid(1))),
            Err(StaleTarget {
                current_generation: Some(2)
            })
        );
        let fresh = s.target_ref(pid(1), "p_1");
        assert_eq!(s.check_target_ref(&fresh, Some(pid(1))), Ok(()));
    }

    #[test]
    fn registry_question_caps_open_16_resolved_8_with_flags() {
        let mut s = SupervisorState::new();
        for i in 0..16 {
            assert!(s.insert_question(pid(1), q(&format!("o{i}"), None)));
        }
        assert!(!s.pane(pid(1)).unwrap().truncated_questions);
        s.insert_question(pid(1), q("o16", None));
        let p = s.pane(pid(1)).unwrap();
        assert!(p.truncated_questions);
        assert!(!p.truncated_resolved);
        assert_eq!(p.questions.len(), 16);
        assert!(p.questions.iter().all(|x| x.question_id != "o0"));

        for i in 0..9 {
            s.insert_question(
                pid(2),
                q(&format!("r{i}"), resolved(ResolutionBy::OwnerAnswer)),
            );
        }
        let p = s.pane(pid(2)).unwrap();
        assert!(p.truncated_resolved);
        assert!(!p.truncated_questions);
        assert_eq!(p.questions.len(), 8);
        assert!(p.questions.iter().all(|x| x.question_id != "r0"));
    }

    #[test]
    fn registry_insert_question_dedupes_by_id() {
        let mut s = SupervisorState::new();
        assert!(s.insert_question(pid(1), q("a", None)));
        assert!(!s.insert_question(pid(1), q("a", None)));
        assert_eq!(s.pane(pid(1)).unwrap().questions.len(), 1);
    }

    #[test]
    fn registry_generation_reason_priority() {
        assert_eq!(
            generation_reason(true, true, true),
            Some(GenerationReason::AgentExited)
        );
        assert_eq!(
            generation_reason(true, false, true),
            Some(GenerationReason::SessionChanged)
        );
        assert_eq!(
            generation_reason(false, false, true),
            Some(GenerationReason::AgentStarted)
        );
        assert_eq!(generation_reason(false, false, false), None);
        assert_eq!(
            generation_reason(false, true, false),
            Some(GenerationReason::AgentExited)
        );
    }

    #[test]
    fn registry_record_generation_change_sets_and_clears_exited() {
        let mut s = SupervisorState::new();
        assert_eq!(
            s.record_generation_change(pid(1), GenerationReason::AgentExited, 1),
            2
        );
        assert!(s.pane(pid(1)).unwrap().exited);
        assert_eq!(
            s.record_generation_change(pid(1), GenerationReason::SessionChanged, 2),
            3
        );
        assert!(!s.pane(pid(1)).unwrap().exited);
    }

    #[test]
    fn registry_insert_question_caps_excerpt_at_200_chars() {
        let mut s = SupervisorState::new();
        let mut question = q("long", None);
        question.excerpt = "é".repeat(300);
        assert!(s.insert_question(pid(1), question));
        let stored = &s.pane(pid(1)).unwrap().questions[0].excerpt;
        assert_eq!(stored.chars().count(), 200);
        assert!(std::str::from_utf8(stored.as_bytes()).is_ok());
    }

    const TITLED: &str = include_str!("testdata/claude_idle_question_titled.txt");

    fn open_questions(s: &SupervisorState, pane: PaneId) -> Vec<&PendingQuestion> {
        s.pane(pane)
            .map(|p| {
                p.questions
                    .iter()
                    .filter(|q| q.resolution.is_none())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn supervisor_transition_idle_scan_records_one_prose_question_and_dedupes() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Unknown, AgentState::Working, 1, 100);
        assert!(!s.pane(pid(1)).unwrap().pending_prose);
        assert_eq!(s.pane(pid(1)).unwrap().pending_blocked, None);
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 2, 200);
        assert_eq!(s.panes_pending_scan(), vec![pid(1)]);
        s.apply_scan(pid(1), TITLED, 210);
        assert!(s.panes_pending_scan().is_empty());
        let open = open_questions(&s, pid(1));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, QuestionKind::ProseQuestion);
        assert_eq!(open[0].confidence, Confidence::Low);
        assert_eq!(open[0].source, FactSource::ScreenHeuristic);
        assert_eq!(open[0].excerpt, "Should I start the brief?");

        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 3, 300);
        s.apply_scan(pid(1), TITLED, 310);
        assert_eq!(s.pane(pid(1)).unwrap().questions.len(), 1);
    }

    #[test]
    fn supervisor_transition_cross_turn_question_stays_unresolved() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.apply_scan(pid(1), TITLED, 110);
        assert_eq!(open_questions(&s, pid(1)).len(), 1);
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 2, 200);
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 3, 300);
        s.apply_scan(pid(1), "Done. All tests pass.\n", 310);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].resolution, None);
    }

    #[test]
    fn supervisor_transition_blocked_then_working_keeps_structured_question_open() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Blocked, 7, 100);
        assert_eq!(s.pane(pid(1)).unwrap().pending_blocked, Some(7));
        assert!(!s.pane(pid(1)).unwrap().pending_prose);
        s.apply_scan(pid(1), "Allow edit to foo.rs?\n  1. Yes\n  2. No\n", 110);
        s.on_transition(pid(1), AgentState::Blocked, AgentState::Working, 8, 200);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.questions.len(), 1);
        let q = &p.questions[0];
        assert_eq!(q.kind, QuestionKind::StructuredApproval);
        assert_eq!(q.source, FactSource::ManifestRule);
        assert_eq!(q.confidence, Confidence::High);
        assert_eq!(q.excerpt, "2. No");
        assert_eq!(q.question_id, question_id(1, 1, "blocked", "7"));
        assert_eq!(q.resolution, None);
    }

    #[test]
    fn supervisor_transition_working_leaves_possible_response_unset() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.apply_scan(pid(1), TITLED, 110);
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 2, 200);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.turn_started_ms, Some(200));
        assert_eq!(p.questions[0].possible_response_at_ms, None);
        assert!(!p.pending_prose);
        assert_eq!(p.pending_blocked, None);
    }

    #[test]
    fn supervisor_transition_quiescence_rescan_once_after_1500ms() {
        let mut s = SupervisorState::new();
        assert!(!s.request_quiescence_rescan(pid(1), 10_000));
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 500);
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 2, 1000);
        s.apply_scan(pid(1), "", 1000);
        assert!(!s.request_quiescence_rescan(pid(1), 2499));
        assert!(!s.pane(pid(1)).unwrap().pending_prose);
        assert!(s.request_quiescence_rescan(pid(1), 2500));
        assert!(s.pane(pid(1)).unwrap().pending_prose);
        assert_eq!(s.pane(pid(1)).unwrap().pending_blocked, None);
        assert!(!s.request_quiescence_rescan(pid(1), 9000));
        // A new turn makes the idle stale.
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 3, 9500);
        assert!(!s.request_quiescence_rescan(pid(1), 20_000));
    }

    #[test]
    fn supervisor_transition_same_ms_working_then_idle_is_rescan_eligible() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 1000);
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 2, 1000);
        assert!(!s.request_quiescence_rescan(pid(1), 2499));
        assert!(s.request_quiescence_rescan(pid(1), 2500));
    }

    #[test]
    fn supervisor_transition_same_ms_idle_then_blocked_or_unknown_not_eligible() {
        for next in [AgentState::Blocked, AgentState::Unknown] {
            let mut s = SupervisorState::new();
            s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 1000);
            s.on_transition(pid(1), AgentState::Idle, next, 2, 1000);
            assert!(!s.request_quiescence_rescan(pid(1), 6000));
        }
    }

    #[test]
    fn supervisor_transition_stale_prose_scan_dropped_by_working() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 1000);
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 2, 1001);
        assert!(!s.pane(pid(1)).unwrap().pending_prose);
        s.apply_scan(pid(1), TITLED, 1010);
        assert!(s.pane(pid(1)).unwrap().questions.is_empty());
    }

    #[test]
    fn supervisor_transition_blocked_scan_after_leaving_blocked_has_empty_excerpt() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Blocked, 7, 1000);
        s.on_transition(pid(1), AgentState::Blocked, AgentState::Working, 8, 1001);
        s.apply_scan(pid(1), TITLED, 1010);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].kind, QuestionKind::StructuredApproval);
        assert_eq!(p.questions[0].excerpt, "");
    }

    #[test]
    fn supervisor_transition_generation_bump_clears_pending_scan() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.bump_generation(pid(1), GenerationReason::AgentStarted, 150);
        assert!(s.panes_pending_scan().is_empty());
        s.on_transition(pid(1), AgentState::Working, AgentState::Blocked, 2, 200);
        s.bump_generation(pid(1), GenerationReason::AgentStarted, 250);
        assert!(s.panes_pending_scan().is_empty());
    }

    #[test]
    fn supervisor_transition_blocked_then_idle_keeps_structured_question() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Blocked, 7, 100);
        s.on_transition(pid(1), AgentState::Blocked, AgentState::Idle, 8, 101);
        s.apply_scan(pid(1), "Work\n  Done.\n", 110);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].kind, QuestionKind::StructuredApproval);
        assert_eq!(p.questions[0].excerpt, "");
        assert_eq!(p.pending_blocked, None);
        assert!(!p.pending_prose);
        assert!(s.panes_pending_scan().is_empty());
    }

    #[test]
    fn supervisor_transition_blocked_then_idle_with_prose_records_both() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Blocked, 7, 100);
        s.on_transition(pid(1), AgentState::Blocked, AgentState::Idle, 8, 101);
        s.apply_scan(pid(1), "Work\n  Should I start the brief?\n", 110);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.questions.len(), 2);
        let structured: Vec<_> = p
            .questions
            .iter()
            .filter(|q| q.kind == QuestionKind::StructuredApproval)
            .collect();
        assert_eq!(structured.len(), 1);
        assert_eq!(structured[0].excerpt, "");
        let prose: Vec<_> = p
            .questions
            .iter()
            .filter(|q| q.kind == QuestionKind::ProseQuestion)
            .collect();
        assert_eq!(prose.len(), 1);
        assert_eq!(prose[0].excerpt, "Should I start the brief?");
    }

    #[test]
    fn registry_pane_move_keeps_open_questions() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.apply_scan(pid(1), TITLED, 110);
        assert_eq!(open_questions(&s, pid(1)).len(), 1);
        let epoch = s.pane(pid(1)).unwrap().question_epoch;
        assert_eq!(
            s.bump_generation(pid(1), GenerationReason::PaneMoved, 200),
            2
        );
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.generation, 2);
        assert_eq!(p.question_epoch, epoch);
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].resolution, None);
        let last = p.lineage.back().unwrap();
        assert_eq!(last.reason, GenerationReason::PaneMoved);
        assert_eq!(last.generation, 2);
        assert_eq!(p.last_state, Some(AgentState::Idle));
        assert_eq!(p.idle_since_ms, Some(100));
    }

    #[test]
    fn registry_pane_move_rescan_no_duplicate() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.apply_scan(pid(1), TITLED, 110);
        s.bump_generation(pid(1), GenerationReason::PaneMoved, 200);
        assert!(s.request_quiescence_rescan(pid(1), 2000));
        s.apply_scan(pid(1), TITLED, 2010);
        assert_eq!(open_questions(&s, pid(1)).len(), 1);
        assert_eq!(s.pane(pid(1)).unwrap().questions.len(), 1);
    }

    #[test]
    fn registry_pane_move_keeps_pending_scans() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.bump_generation(pid(1), GenerationReason::PaneMoved, 150);
        assert_eq!(s.panes_pending_scan(), vec![pid(1)]);
        s.apply_scan(pid(1), TITLED, 160);
        let open = open_questions(&s, pid(1));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, QuestionKind::ProseQuestion);

        let mut s = SupervisorState::new();
        s.on_transition(pid(2), AgentState::Working, AgentState::Blocked, 7, 100);
        s.bump_generation(pid(2), GenerationReason::PaneMoved, 150);
        assert_eq!(s.pane(pid(2)).unwrap().pending_blocked, Some(7));
        s.apply_scan(
            pid(2),
            "Allow edit to foo.rs?
  1. Yes
  2. No
",
            160,
        );
        let open = open_questions(&s, pid(2));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, QuestionKind::StructuredApproval);
        assert_eq!(open[0].excerpt, "2. No");
        assert_eq!(open[0].question_id, question_id(2, 1, "blocked", "7"));
    }

    #[test]
    fn registry_pane_move_keeps_exited() {
        let mut s = SupervisorState::new();
        s.record_generation_change(pid(1), GenerationReason::AgentExited, 10);
        assert!(s.pane(pid(1)).unwrap().exited);
        s.bump_generation(pid(1), GenerationReason::PaneMoved, 20);
        let p = s.pane(pid(1)).unwrap();
        assert!(p.exited);
        assert_eq!(p.generation, 3);
    }

    #[test]
    fn registry_pane_move_keeps_hook_currency() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 100);
        s.note_background_hook(pid(1), 150);
        assert!(s.background_hook_current(pid(1)));
        s.bump_generation(pid(1), GenerationReason::PaneMoved, 200);
        assert!(s.background_hook_current(pid(1)));
        assert_eq!(s.background_hook_ms(pid(1)), Some(150));
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.background_hook_generation, Some(p.generation));
    }

    #[test]
    fn registry_pane_move_keeps_pre_working_hook_stale() {
        let mut s = SupervisorState::new();
        s.note_background_hook(pid(1), 50);
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 100);
        assert!(!s.background_hook_current(pid(1)));
        s.bump_generation(pid(1), GenerationReason::PaneMoved, 200);
        assert!(!s.background_hook_current(pid(1)));
    }

    #[test]
    fn registry_session_change_still_supersedes() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.apply_scan(pid(1), TITLED, 110);
        let epoch = s.pane(pid(1)).unwrap().question_epoch;
        s.bump_generation(pid(1), GenerationReason::SessionChanged, 200);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.question_epoch, epoch + 1);
        assert_eq!(p.questions.len(), 1);
        assert_eq!(
            p.questions[0].resolution,
            Some(QuestionResolution {
                at_ms: 200,
                by: ResolutionBy::Superseded
            })
        );
        assert!(open_questions(&s, pid(1)).is_empty());
    }

    #[test]
    fn registry_overflow_counts_dropped_unresolved_questions() {
        let mut s = SupervisorState::new();
        for i in 0..17 {
            s.insert_question(pid(1), q(&format!("o{i}"), None));
        }
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(open_questions(&s, pid(1)).len(), 16);
        assert_eq!(p.unresolved_overflow, 1);
        assert!(p.truncated_questions);

        // Resolved drops do not count.
        for i in 0..9 {
            s.insert_question(
                pid(2),
                q(&format!("r{i}"), resolved(ResolutionBy::OwnerAnswer)),
            );
        }
        assert_eq!(s.pane(pid(2)).unwrap().unresolved_overflow, 0);
    }

    #[test]
    fn registry_overflow_kept_on_move_reset_on_replacement() {
        let mut s = SupervisorState::new();
        for i in 0..18 {
            s.insert_question(pid(1), q(&format!("o{i}"), None));
        }
        assert_eq!(s.pane(pid(1)).unwrap().unresolved_overflow, 2);
        s.bump_generation(pid(1), GenerationReason::PaneMoved, 10);
        assert_eq!(s.pane(pid(1)).unwrap().unresolved_overflow, 2);
        s.bump_generation(pid(1), GenerationReason::SessionChanged, 20);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.unresolved_overflow, 0);
        // The truncation flag stays sticky for the pane.
        assert!(p.truncated_questions);
    }

    fn sref(value: &str) -> Option<AgentSessionRef> {
        Some(AgentSessionRef {
            kind: crate::agent_resume::AgentSessionRefKind::Id,
            value: value.to_string(),
        })
    }

    #[test]
    fn restored_delegation_activates_only_for_the_reported_saved_session() {
        use crate::supervisor::delegation::PendingRestore;
        let mut s = SupervisorState::new();
        let restore = PendingRestore {
            agent_session: "saved".into(),
            granted_at_ms: 1,
        };
        s.delegations.queue_restore(pid(1), restore.clone());
        s.delegations.queue_restore(pid(2), restore);
        s.observe_session_ref(pid(1), None, sref("saved"));
        s.observe_session_ref(pid(2), None, sref("other"));
        assert!(s.delegations.active(pid(1), u64::MAX).is_some());
        assert!(s.delegations.active(pid(2), u64::MAX).is_none());
        // A later matching report does not bring a dropped one back.
        s.observe_session_ref(pid(2), sref("other"), sref("saved"));
        assert!(s.delegations.active(pid(2), u64::MAX).is_none());
    }

    #[test]
    fn registry_session_enrichment_and_loss_are_not_replacements() {
        let mut s = SupervisorState::new();
        assert!(!s.observe_session_ref(pid(1), None, None));
        assert!(s.pane(pid(1)).is_none());
        // None to some: enrichment.
        assert!(!s.observe_session_ref(pid(1), None, sref("a")));
        assert_eq!(s.pane(pid(1)).unwrap().last_session_ref, sref("a"));
        // Some to none: loss keeps the last known identity.
        assert!(!s.observe_session_ref(pid(1), sref("a"), None));
        assert_eq!(s.pane(pid(1)).unwrap().last_session_ref, sref("a"));
        // Same identity again.
        assert!(!s.observe_session_ref(pid(1), None, sref("a")));
    }

    #[test]
    fn registry_session_replacement_detected_through_loss() {
        let mut s = SupervisorState::new();
        assert!(!s.observe_session_ref(pid(1), None, sref("a")));
        assert!(s.observe_session_ref(pid(1), sref("a"), sref("b")));
        assert_eq!(s.pane(pid(1)).unwrap().last_session_ref, sref("b"));

        // a, then none, then b: the replacement is seen at b.
        let mut s = SupervisorState::new();
        assert!(!s.observe_session_ref(pid(1), None, sref("a")));
        assert!(!s.observe_session_ref(pid(1), sref("a"), None));
        assert!(s.observe_session_ref(pid(1), None, sref("b")));

        // Kind is part of the identity.
        let mut s = SupervisorState::new();
        assert!(!s.observe_session_ref(pid(1), None, sref("a")));
        let path = AgentSessionRef {
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: "a".to_string(),
        };
        assert!(s.observe_session_ref(pid(1), None, Some(path)));
    }

    #[test]
    fn registry_session_seeded_from_terminal_before() {
        // Registry first sees the pane while the terminal already has `a`.
        let mut s = SupervisorState::new();
        assert!(s.observe_session_ref(pid(1), sref("a"), sref("b")));
        let mut s = SupervisorState::new();
        assert!(!s.observe_session_ref(pid(1), sref("a"), sref("a")));
    }

    fn facts(
        before: Option<AgentSessionRef>,
        after: Option<AgentSessionRef>,
        final_state: AgentState,
    ) -> TerminalMutationFacts {
        TerminalMutationFacts {
            session_before: before,
            session_after: after,
            agent_released: false,
            agent_started: false,
            final_state,
        }
    }

    fn accept_profile_receipt(
        s: &mut SupervisorState,
        id: &str,
        pane: u32,
        label: &str,
        kind: SubmitKind,
    ) -> SubmitFence {
        let profile = parse_agent_label(label).and_then(profile_for).unwrap();
        let target_ref = s.target_ref(pid(pane), &format!("p{pane}"));
        let fence = SubmitFence::new();
        s.submits
            .insert_accepted(
                id,
                NewReceipt {
                    kind,
                    lazy_reset: kind == SubmitKind::Reset
                        && profile.reset_evidence == ResetEvidenceMode::OnNextPrompt,
                    question_id: None,
                    pane: pid(pane),
                    target_ref,
                    text_sha256_16: text_sha256_16("prompt"),
                    own_text_sha256_16: matches!(kind, SubmitKind::Submit | SubmitKind::Answer)
                        .then(|| text_sha256_16("prompt")),
                    ack_sha256_16: (kind == SubmitKind::Submit)
                        .then(|| prompt_ack_sha256_16("prompt")),
                    handoff: false,
                    session_id: (kind == SubmitKind::Resume).then(|| "requested".into()),
                    resume_agent: (kind == SubmitKind::Resume)
                        .then(|| parse_agent_label(label).unwrap()),
                },
                fence.clone(),
                1,
            )
            .expect("accepted");
        fence
    }

    fn profile_report(
        s: &mut SupervisorState,
        pane: u32,
        label: &str,
        reported: Option<&str>,
        resolved: Option<&str>,
        source: &str,
    ) -> Option<GenerationReason> {
        s.begin_session_report(Some(source.into()), label, reported.and_then(sref));
        let reason = s.on_terminal_mutation(
            pid(pane),
            facts(None, resolved.and_then(sref), AgentState::Idle),
            20,
        );
        s.end_session_report();
        reason
    }

    #[test]
    fn lazy_reset_written_bumps_once_and_verifies_without_second_bump() {
        for label in ["codex", "cursor"] {
            let mut s = SupervisorState::new();
            s.observe_session_ref(pid(1), None, sref("old"));
            s.insert_question(pid(1), q("old-question", None));
            s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 1);
            s.pane_mut(pid(1)).unresolved_overflow = 2;
            let queued = accept_profile_receipt(&mut s, "queued", 1, label, SubmitKind::Submit);
            let other = accept_profile_receipt(&mut s, "other", 2, label, SubmitKind::Submit);
            let reset = accept_profile_receipt(&mut s, "reset", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            let completed = reset.close();
            s.complete_submit("reset", &Ok(()), completed, 2);
            let p = s.pane(pid(1)).unwrap();
            assert_eq!((p.generation, p.question_epoch), (2, 2), "{label}");
            assert!(p.lazy_reset_pending);
            assert_eq!(p.lineage.len(), 1);
            assert_eq!(p.lineage[0].reason, GenerationReason::ResetWritten);
            assert_eq!((p.lineage[0].generation, p.lineage[0].at_ms), (2, 2));
            assert_eq!(
                p.questions[0].resolution.as_ref().unwrap().by,
                ResolutionBy::Superseded
            );
            assert_eq!(p.unresolved_overflow, 0);
            assert!(p.turn_seq.is_none() && p.turn_started_ms.is_none());
            assert!(p.pending_blocked.is_none() && !p.pending_prose);
            assert!(queued.is_tripped());
            assert!(!other.is_tripped());
            assert_eq!(s.pane(pid(2)).unwrap().generation, 1);
            let r = s.submits.get("reset").unwrap();
            assert_eq!(
                (r.state, r.reset),
                (SubmitState::Written, Some(ResetEvidence::Pending))
            );
            assert_eq!(r.target_ref.generation, 1);
            assert!(r.new_session.is_none() && r.new_generation.is_none());
            s.complete_submit("reset", &Ok(()), completed, 3);
            assert_eq!(s.pane(pid(1)).unwrap().generation, 2);
            for (report_label, reported, resolved) in [
                (label, Some("old"), Some("old")),
                (label, None, None),
                ("gemini", Some("old"), Some("old")),
            ] {
                assert_eq!(
                    profile_report(&mut s, 1, report_label, reported, resolved, "clear"),
                    None
                );
                assert_eq!(
                    s.submits.get("reset").unwrap().reset,
                    Some(ResetEvidence::Pending)
                );
                assert_eq!(s.pane(pid(1)).unwrap().last_session_ref, sref("old"));
            }
            s.on_terminal_mutation(pid(1), facts(sref("old"), None, AgentState::Working), 10);
            assert_eq!(
                s.submits.get("reset").unwrap().reset,
                Some(ResetEvidence::Pending)
            );
            // The explicit report is authoritative even while resolved identity lags.
            assert_eq!(
                profile_report(&mut s, 1, label, Some("new"), Some("old"), "startup"),
                None
            );
            let r = s.submits.get("reset").unwrap();
            assert_eq!(
                (r.state, r.reset),
                (SubmitState::Written, Some(ResetEvidence::Verified))
            );
            assert_eq!(r.new_session.as_ref().unwrap().value, "new");
            assert_eq!(r.new_generation, Some(2));
            let p = s.pane(pid(1)).unwrap();
            assert_eq!((p.generation, p.question_epoch, p.lineage.len()), (2, 2, 1));
            assert!(!p.lazy_reset_pending);
            assert_eq!(p.last_session_ref, sref("new"));
            let verified = r.clone();
            assert_eq!(
                profile_report(&mut s, 1, label, Some("new"), Some("new"), "startup"),
                None
            );
            s.complete_submit("reset", &Ok(()), completed, 21);
            assert_eq!(s.submits.get("reset"), Some(&verified));
            assert_eq!(
                profile_report(&mut s, 1, label, Some("next"), Some("next"), "startup"),
                Some(GenerationReason::SessionChanged)
            );
            assert_eq!(s.pane(pid(1)).unwrap().generation, 3);

            // Without a known prior identity, the first report is only enrichment.
            let mut s = SupervisorState::new();
            let reset = accept_profile_receipt(&mut s, "unknown", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            s.complete_submit("unknown", &Ok(()), reset.close(), 2);
            assert_eq!(
                profile_report(
                    &mut s,
                    1,
                    label,
                    Some("first-known"),
                    Some("first-known"),
                    "startup"
                ),
                None
            );
            let p = s.pane(pid(1)).unwrap();
            assert_eq!(p.generation, 2);
            assert!(p.lazy_reset_pending);
            assert_eq!(p.last_session_ref, sref("first-known"));
            assert_eq!(
                s.submits.get("unknown").unwrap().reset,
                Some(ResetEvidence::Pending)
            );

            // A valid clear between commit and completion already caused the bump.
            let mut s = SupervisorState::new();
            s.observe_session_ref(pid(1), None, sref("old"));
            let reset = accept_profile_receipt(&mut s, "race", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            assert_eq!(
                profile_report(&mut s, 1, label, Some("new"), Some("new"), "clear"),
                Some(GenerationReason::SessionChanged)
            );
            let completed = reset.close();
            assert_eq!(completed.reason, Some(FenceReason::GenerationChanged));
            s.complete_submit("race", &Ok(()), completed, 21);
            s.complete_submit("race", &Ok(()), completed, 22);
            let r = s.submits.get("race").unwrap();
            assert_eq!(
                (r.state, r.reset, r.new_generation),
                (SubmitState::Written, Some(ResetEvidence::Verified), Some(2))
            );
            assert_eq!(r.new_session.as_ref().unwrap().value, "new");
            let p = s.pane(pid(1)).unwrap();
            assert_eq!((p.generation, p.lineage.len()), (2, 1));
            assert!(!p.lazy_reset_pending);
            assert_eq!(p.lineage[0].reason, GenerationReason::SessionChanged);
        }
    }

    #[test]
    fn lazy_reset_verification_does_not_verify_later_inflight_reset() {
        for label in ["codex", "cursor"] {
            let mut s = SupervisorState::new();
            s.observe_session_ref(pid(1), None, sref("old"));
            let first = accept_profile_receipt(&mut s, "first", 1, label, SubmitKind::Reset);
            assert!(first.try_commit());
            s.complete_submit("first", &Ok(()), first.close(), 2);
            assert_eq!(s.pane(pid(1)).unwrap().generation, 2, "{label}");
            let r = s.submits.get("first").unwrap();
            assert_eq!(
                (r.state, r.reset),
                (SubmitState::Written, Some(ResetEvidence::Pending))
            );

            let second = accept_profile_receipt(&mut s, "second", 1, label, SubmitKind::Reset);
            assert_eq!(s.submits.get("second").unwrap().target_ref.generation, 2);
            assert!(second.try_commit());
            assert_eq!(
                profile_report(
                    &mut s,
                    1,
                    label,
                    Some("first-session"),
                    Some("first-session"),
                    "clear"
                ),
                None
            );
            let p = s.pane(pid(1)).unwrap();
            assert_eq!((p.generation, p.lineage.len()), (2, 1));
            assert!(!p.lazy_reset_pending);
            let verified_first = s.submits.get("first").unwrap().clone();
            assert_eq!(
                (
                    verified_first.state,
                    verified_first.reset,
                    verified_first.new_generation
                ),
                (SubmitState::Written, Some(ResetEvidence::Verified), Some(2))
            );
            assert_eq!(
                verified_first.new_session.as_ref().unwrap().value,
                "first-session"
            );
            let r = s.submits.get("second").unwrap();
            assert_eq!(
                (r.state, r.reset),
                (SubmitState::Accepted, Some(ResetEvidence::Pending))
            );
            assert!(r.new_session.is_none() && r.new_generation.is_none());
            assert!(!second.is_tripped());

            let completed = second.close();
            s.complete_submit("second", &Ok(()), completed, 21);
            let p = s.pane(pid(1)).unwrap();
            assert_eq!(
                p.generation, 3,
                "{label}: second reset needs its own generation bump"
            );
            assert!(p.lazy_reset_pending);
            assert_eq!(p.lineage.len(), 2);
            assert_eq!(p.lineage[1].reason, GenerationReason::ResetWritten);
            assert_eq!((p.lineage[1].generation, p.lineage[1].at_ms), (3, 21));
            let r = s.submits.get("second").unwrap();
            assert_eq!(
                (r.state, r.reset),
                (SubmitState::Written, Some(ResetEvidence::Pending))
            );
            assert!(r.new_session.is_none() && r.new_generation.is_none());
            assert_eq!(s.submits.get("first"), Some(&verified_first));
            s.complete_submit("second", &Ok(()), completed, 22);
            assert_eq!(s.pane(pid(1)).unwrap().generation, 3);

            assert_eq!(
                profile_report(
                    &mut s,
                    1,
                    label,
                    Some("second-session"),
                    Some("second-session"),
                    "clear"
                ),
                None
            );
            let r = s.submits.get("second").unwrap();
            assert_eq!(
                (r.state, r.reset, r.new_generation),
                (SubmitState::Written, Some(ResetEvidence::Verified), Some(3))
            );
            assert_eq!(r.new_session.as_ref().unwrap().value, "second-session");
            let p = s.pane(pid(1)).unwrap();
            assert_eq!((p.generation, p.lineage.len()), (3, 2));
            assert!(!p.lazy_reset_pending);
            assert_eq!(s.submits.get("first"), Some(&verified_first));
        }
    }

    #[test]
    fn lazy_reset_failed_writes_do_not_bump() {
        for label in ["codex", "cursor"] {
            for failure in ["write", "deadline", "pty", "fence", "late", "queue"] {
                let mut s = SupervisorState::new();
                s.observe_session_ref(pid(1), None, sref("old"));
                let reset = accept_profile_receipt(&mut s, "reset", 1, label, SubmitKind::Reset);
                let result = match failure {
                    "queue" => {
                        s.submits.mark_queue_failed("reset");
                        Ok(())
                    }
                    "fence" => {
                        reset.trip(FenceReason::OwnerInput);
                        assert!(!reset.try_commit());
                        Err(fenced_error())
                    }
                    "late" => {
                        assert!(reset.try_commit());
                        reset.trip(FenceReason::GenerationChanged);
                        Ok(())
                    }
                    other => {
                        assert!(reset.try_commit());
                        let kind = match other {
                            "deadline" => std::io::ErrorKind::TimedOut,
                            "pty" => std::io::ErrorKind::BrokenPipe,
                            _ => std::io::ErrorKind::Other,
                        };
                        Err(std::io::Error::new(kind, "test write failed"))
                    }
                };
                let completed = reset.close();
                s.complete_submit("reset", &result, completed, 2);
                s.complete_submit("reset", &Ok(()), completed, 3);
                let p = s.pane(pid(1)).unwrap();
                assert_eq!(
                    (p.generation, p.question_epoch, p.lineage.len()),
                    (1, 1, 0),
                    "{label}/{failure}"
                );
                assert!(!p.lazy_reset_pending);
                let r = s.submits.get("reset").unwrap();
                let expected = if failure == "fence" {
                    SubmitState::Fenced
                } else {
                    SubmitState::Indeterminate
                };
                assert_eq!(
                    (r.state, r.reset),
                    (expected, Some(ResetEvidence::Unverified))
                );
                assert!(r.new_session.is_none() && r.new_generation.is_none());
            }
        }
    }

    #[test]
    fn lazy_reset_unrelated_replacement_invalidates_pending() {
        for label in ["codex", "cursor"] {
            let mut s = SupervisorState::new();
            s.observe_session_ref(pid(1), None, sref("old"));
            let reset = accept_profile_receipt(&mut s, "first", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            s.complete_submit("first", &Ok(()), reset.close(), 2);
            s.insert_question(pid(1), q("keep-on-move", None));
            s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 3);
            s.note_background_hook(pid(1), 4);
            assert_eq!(s.bump_generation(pid(1), GenerationReason::PaneMoved, 5), 3);
            let p = s.pane(pid(1)).unwrap();
            assert!(p.lazy_reset_pending && p.pending_prose);
            assert_eq!(p.question_epoch, 2);
            assert!(p.questions[0].resolution.is_none());
            assert!(s.background_hook_current(pid(1)));
            assert_eq!(
                s.submits.get("first").unwrap().reset,
                Some(ResetEvidence::Pending)
            );
            assert_eq!(
                profile_report(&mut s, 1, label, Some("new"), Some("new"), "startup"),
                None
            );
            assert_eq!(s.submits.get("first").unwrap().new_generation, Some(3));
            assert_eq!(s.pane(pid(1)).unwrap().generation, 3);

            // A newer written reset exempts only itself from replacement invalidation.
            let reset = accept_profile_receipt(&mut s, "second", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            s.complete_submit("second", &Ok(()), reset.close(), 6);
            let reset = accept_profile_receipt(&mut s, "third", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            s.complete_submit("third", &Ok(()), reset.close(), 7);
            assert_eq!(
                s.submits.get("second").unwrap().reset,
                Some(ResetEvidence::Unverified)
            );
            assert_eq!(
                s.submits.get("third").unwrap().reset,
                Some(ResetEvidence::Pending)
            );
            assert_eq!(s.pane(pid(1)).unwrap().generation, 5);
            s.record_generation_change(pid(1), GenerationReason::AgentExited, 8);
            assert_eq!(
                s.submits.get("third").unwrap().reset,
                Some(ResetEvidence::Unverified)
            );
            assert!(s.pane(pid(1)).unwrap().exited);
            assert!(!s.pane(pid(1)).unwrap().lazy_reset_pending);

            for event in [
                "unreported",
                "unsupported",
                "absent",
                "same",
                "exit",
                "start",
            ] {
                let mut s = SupervisorState::new();
                s.observe_session_ref(pid(1), None, sref("old"));
                let reset = accept_profile_receipt(&mut s, "reset", 1, label, SubmitKind::Reset);
                assert!(reset.try_commit());
                s.complete_submit("reset", &Ok(()), reset.close(), 2);
                let mut mutation = facts(sref("old"), sref("new"), AgentState::Idle);
                mutation.agent_released = event == "exit";
                mutation.agent_started = event == "start";
                if event != "unreported" {
                    let report_label = if event == "unsupported" {
                        "gemini"
                    } else {
                        label
                    };
                    let reported = match event {
                        "absent" => None,
                        "same" => sref("old"),
                        _ => sref("new"),
                    };
                    s.begin_session_report(Some("clear".into()), report_label, reported);
                }
                let expected = match event {
                    "exit" => GenerationReason::AgentExited,
                    "start" => GenerationReason::AgentStarted,
                    _ => GenerationReason::SessionChanged,
                };
                assert_eq!(
                    s.on_terminal_mutation(pid(1), mutation, 3),
                    Some(expected),
                    "{label}/{event}"
                );
                s.end_session_report();
                assert_eq!(
                    s.submits.get("reset").unwrap().reset,
                    Some(ResetEvidence::Unverified),
                    "{label}/{event}"
                );
                let p = s.pane(pid(1)).unwrap();
                assert_eq!(p.generation, 3);
                assert!(!p.lazy_reset_pending);
                assert_eq!(p.exited, event == "exit");
            }
        }
    }

    #[test]
    fn lazy_reset_verification_preserves_first_post_reset_submit() {
        for label in ["codex", "cursor"] {
            let mut s = SupervisorState::new();
            s.observe_session_ref(pid(1), None, sref("old"));
            let reset = accept_profile_receipt(&mut s, "reset", 1, label, SubmitKind::Reset);
            assert!(reset.try_commit());
            s.complete_submit("reset", &Ok(()), reset.close(), 2);
            let first = accept_profile_receipt(&mut s, "first", 1, label, SubmitKind::Submit);
            let current = s.submits.get("first").unwrap().target_ref.clone();
            s.insert_question(pid(1), q("current-question", None));
            s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 3);
            let turn_seq = s.pane(pid(1)).unwrap().turn_seq;
            assert!(first.try_commit());
            assert_eq!(
                profile_report(&mut s, 1, label, Some("new"), Some("new"), "startup"),
                None
            );
            assert!(!first.is_tripped());
            assert_eq!(s.pane(pid(1)).unwrap().generation, current.generation);
            let p = s.pane(pid(1)).unwrap();
            assert_eq!(p.turn_seq, turn_seq);
            assert_eq!(p.turn_started_ms, Some(3));
            assert!(p.questions[0].resolution.is_none());
            // The native hook can arrive during the first post-reset write.
            let hash = prompt_ack_sha256_16("prompt");
            assert_eq!(s.note_prompt_submitted(pid(1), &hash, 21), None);
            s.complete_submit("first", &Ok(()), first.close(), 22);
            let r = s.submits.get("first").unwrap();
            assert_eq!(
                (r.state, r.acknowledged),
                (SubmitState::Written, SubmitAcknowledged::Confirmed)
            );
            assert_eq!(r.acknowledged_at_ms, Some(22));
            assert_eq!(s.pane(pid(1)).unwrap().generation, 2);
        }
    }

    #[test]
    fn resume_evidence_requires_requested_profile_agent() {
        for requested in ["claude", "codex", "cursor"] {
            for foreign in ["claude", "codex", "cursor"] {
                if foreign == requested {
                    continue;
                }
                for foreign_session in ["requested", "different"] {
                    for (valid_session, expected) in [
                        ("requested", ResumeEvidence::Verified),
                        ("different", ResumeEvidence::Mismatch),
                    ] {
                        let mut s = SupervisorState::new();
                        accept_profile_receipt(&mut s, "resume", 1, requested, SubmitKind::Resume);
                        s.mark_resume_launched("resume", 2);
                        let pending = s.submits.get("resume").unwrap().clone();
                        assert_eq!(pending.resume, Some(ResumeEvidence::Pending));
                        profile_report(
                            &mut s,
                            1,
                            foreign,
                            Some(foreign_session),
                            Some(foreign_session),
                            "startup",
                        );
                        assert_eq!(
                            s.submits.get("resume"),
                            Some(&pending),
                            "{requested} resume must ignore {foreign} report of {foreign_session}"
                        );
                        profile_report(
                            &mut s,
                            1,
                            requested,
                            Some(valid_session),
                            Some(valid_session),
                            "resume",
                        );
                        let settled = s.submits.get("resume").unwrap();
                        assert_eq!(
                            settled.resume,
                            Some(expected),
                            "{requested} report must settle after ignored {foreign} evidence"
                        );
                        assert_eq!(settled.session_start_source.as_deref(), Some("resume"));
                        assert_eq!(
                            settled.new_generation,
                            Some(s.pane(pid(1)).unwrap().generation)
                        );
                        assert_eq!(
                            settled
                                .reported_session
                                .as_ref()
                                .map(|session| session.value.as_str()),
                            (expected == ResumeEvidence::Mismatch).then_some(valid_session)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn resume_evidence_accepts_profile_agents() {
        for label in ["claude", "codex", "cursor", "cursor-agent"] {
            for (reported, resolved, expected) in [
                ("different", "requested", ResumeEvidence::Mismatch),
                ("requested", "different", ResumeEvidence::Verified),
            ] {
                let mut s = SupervisorState::new();
                accept_profile_receipt(&mut s, "resume", 1, label, SubmitKind::Resume);
                s.mark_resume_launched("resume", 2);
                for unsupported in ["gemini", "unknown-agent"] {
                    profile_report(&mut s, 1, unsupported, Some("requested"), None, "startup");
                    assert_eq!(
                        s.submits.get("resume").unwrap().resume,
                        Some(ResumeEvidence::Pending)
                    );
                }
                profile_report(&mut s, 1, label, None, Some("requested"), "startup");
                s.on_terminal_mutation(pid(1), facts(None, sref("requested"), AgentState::Idle), 3);
                profile_report(
                    &mut s,
                    2,
                    label,
                    Some("requested"),
                    Some("requested"),
                    "startup",
                );
                assert_eq!(
                    s.submits.get("resume").unwrap().resume,
                    Some(ResumeEvidence::Pending)
                );
                profile_report(&mut s, 1, label, Some(reported), Some(resolved), "startup");
                let r = s.submits.get("resume").unwrap();
                assert_eq!(
                    (r.state, r.resume),
                    (SubmitState::Launched, Some(expected)),
                    "{label}"
                );
                assert_eq!(r.session_start_source.as_deref(), Some("startup"));
                if expected == ResumeEvidence::Mismatch {
                    assert_eq!(r.reported_session.as_ref().unwrap().value, reported);
                } else {
                    assert!(r.reported_session.is_none());
                }
                let settled = r.clone();
                profile_report(&mut s, 1, label, Some("later"), Some(resolved), "resume");
                assert_eq!(s.submits.get("resume"), Some(&settled));
            }
        }
    }

    /// `agent.resume` evidence comes from the reported session ref of a supported report,
    /// never from the terminal's resolved identity (`session_after`).
    #[test]
    fn resume_evidence_uses_the_reported_ref_not_the_resolved_identity() {
        use crate::api::schema::supervisor::{ResumeEvidence, SubmitKind};
        use crate::supervisor::submit::NewReceipt;
        let launch = |s: &mut SupervisorState, id: &str, pane: u32| {
            let target_ref = s.target_ref(pid(pane), "p");
            s.submits
                .insert_accepted(
                    id,
                    NewReceipt {
                        kind: SubmitKind::Resume,
                        lazy_reset: false,
                        question_id: None,
                        pane: pid(pane),
                        target_ref,
                        text_sha256_16: String::new(),
                        ack_sha256_16: None,
                        own_text_sha256_16: None,
                        handoff: false,
                        session_id: Some("x".into()),
                        resume_agent: Some(Agent::Claude),
                    },
                    crate::supervisor::fence::SubmitFence::new(),
                    1,
                )
                .expect("accepted");
            s.mark_resume_launched(id, 2);
        };
        let report =
            |s: &mut SupervisorState, pane: u32, label: &str, reported: &str, resolved: &str| {
                s.begin_session_report(Some("startup".into()), label, sref(reported));
                s.on_terminal_mutation(pid(pane), facts(None, sref(resolved), AgentState::Idle), 3);
                s.end_session_report();
            };
        let mut s = SupervisorState::new();
        // Reported y while the resolved identity still reads the requested x: mismatch.
        launch(&mut s, "a", 1);
        report(&mut s, 1, "claude", "y", "x");
        let r = s.submits.get("a").unwrap();
        assert_eq!(r.resume, Some(ResumeEvidence::Mismatch));
        assert_eq!(r.reported_session.as_ref().unwrap().value, "y");
        // Reported x while the resolved identity reads something else: verified.
        launch(&mut s, "b", 2);
        report(&mut s, 2, "claude", "x", "z");
        assert_eq!(
            s.submits.get("b").unwrap().resume,
            Some(ResumeEvidence::Verified)
        );
        // A report labelled with another agent is ignored.
        launch(&mut s, "c", 3);
        report(&mut s, 3, "gemini", "x", "x");
        assert_eq!(
            s.submits.get("c").unwrap().resume,
            Some(ResumeEvidence::Pending)
        );
        // A mutation that is not a session report never settles evidence.
        s.on_terminal_mutation(pid(3), facts(None, sref("x"), AgentState::Idle), 4);
        assert_eq!(
            s.submits.get("c").unwrap().resume,
            Some(ResumeEvidence::Pending)
        );
    }

    #[test]
    fn registry_terminal_mutation_bumps_only_on_replacement() {
        let mut s = SupervisorState::new();
        assert_eq!(
            s.on_terminal_mutation(pid(1), facts(None, sref("a"), AgentState::Working), 1),
            None
        );
        assert_eq!(s.pane(pid(1)).unwrap().generation, 1);
        assert_eq!(
            s.on_terminal_mutation(pid(1), facts(sref("a"), None, AgentState::Working), 2),
            None
        );
        assert_eq!(
            s.on_terminal_mutation(pid(1), facts(None, sref("b"), AgentState::Working), 3),
            Some(GenerationReason::SessionChanged)
        );
        assert_eq!(s.pane(pid(1)).unwrap().generation, 2);
        let mut released = facts(sref("b"), sref("c"), AgentState::Unknown);
        released.agent_released = true;
        assert_eq!(
            s.on_terminal_mutation(pid(1), released, 4),
            Some(GenerationReason::AgentExited)
        );
        assert!(s.pane(pid(1)).unwrap().exited);
    }

    #[test]
    fn registry_replacement_while_idle_rearms_prose_scan() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.observe_session_ref(pid(1), None, sref("a"));
        s.apply_scan(pid(1), TITLED, 110);
        let first = open_questions(&s, pid(1))[0].question_id.clone();
        assert!(s.panes_pending_scan().is_empty());

        s.on_terminal_mutation(pid(1), facts(sref("a"), sref("b"), AgentState::Idle), 200);
        let p = s.pane(pid(1)).unwrap();
        assert!(p.pending_prose);
        assert_eq!(p.last_state, Some(AgentState::Idle));
        assert_eq!(p.idle_since_ms, Some(200));
        assert!(!p.rescanned_this_idle);
        s.apply_scan(pid(1), TITLED, 210);
        let open = open_questions(&s, pid(1));
        assert_eq!(open.len(), 1);
        assert_ne!(open[0].question_id, first);

        // Not Idle after the replacement: no re-arm.
        s.on_terminal_mutation(
            pid(1),
            facts(sref("b"), sref("c"), AgentState::Working),
            300,
        );
        assert!(!s.pane(pid(1)).unwrap().pending_prose);
    }

    #[test]
    fn registry_working_report_advances_turn_without_start_time() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 100);
        s.note_background_hook(pid(1), 150);
        assert!(s.background_hook_current(pid(1)));
        s.note_working_report(pid(1));
        assert!(!s.background_hook_current(pid(1)));
        assert_eq!(s.pane(pid(1)).unwrap().turn_started_ms, Some(100));
        s.note_background_hook(pid(1), 250);
        assert!(s.background_hook_current(pid(1)));
    }

    #[test]
    fn registry_long_questions_sharing_200_chars_get_distinct_ids() {
        let mut s = SupervisorState::new();
        let prefix = "x".repeat(200);
        let first = format!("{prefix}{}?", "a".repeat(49));
        let second = format!("{prefix}{}?", "b".repeat(49));
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 1, 100);
        s.apply_scan(pid(1), &format!("{first}\n"), 110);
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 2, 200);
        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 3, 300);
        s.apply_scan(pid(1), &format!("{second}\n"), 310);
        let open = open_questions(&s, pid(1));
        assert_eq!(open.len(), 2);
        assert_ne!(open[0].question_id, open[1].question_id);
        assert!(open.iter().all(|q| q.excerpt.chars().count() <= 200));
        assert_eq!(open[0].excerpt, prefix);
    }

    #[test]
    fn registry_new_agent_first_ref_is_enrichment() {
        let mut s = SupervisorState::new();
        assert_eq!(
            s.on_terminal_mutation(pid(1), facts(None, sref("a"), AgentState::Working), 1),
            None
        );
        let mut exited = facts(sref("a"), None, AgentState::Idle);
        exited.agent_released = true;
        assert_eq!(
            s.on_terminal_mutation(pid(1), exited, 2),
            Some(GenerationReason::AgentExited)
        );
        assert_eq!(s.pane(pid(1)).unwrap().last_session_ref, sref("a"));
        let mut started = facts(None, None, AgentState::Working);
        started.agent_started = true;
        assert_eq!(
            s.on_terminal_mutation(pid(1), started, 3),
            Some(GenerationReason::AgentStarted)
        );
        assert_eq!(s.pane(pid(1)).unwrap().last_session_ref, None);

        s.on_transition(pid(1), AgentState::Working, AgentState::Idle, 4, 400);
        s.apply_scan(pid(1), TITLED, 410);
        assert_eq!(open_questions(&s, pid(1)).len(), 1);
        let generation = s.pane(pid(1)).unwrap().generation;

        assert_eq!(
            s.on_terminal_mutation(pid(1), facts(None, sref("b"), AgentState::Idle), 500),
            None
        );
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.generation, generation);
        assert_eq!(p.last_session_ref, sref("b"));
        assert_eq!(open_questions(&s, pid(1)).len(), 1);
    }
}

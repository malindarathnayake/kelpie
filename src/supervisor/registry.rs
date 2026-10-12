//! Pure per-pane supervisor registry: generations, lineage and question bookkeeping.
//! No App access and no I/O.

use std::collections::{HashMap, VecDeque};

use crate::api::schema::supervisor::{
    Confidence, FactSource, GenerationReason, LineageEntry, PendingQuestion, QuestionKind,
    QuestionResolution, ResolutionBy, TargetRef,
};
use crate::detect::AgentState;
use crate::layout::PaneId;
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

#[derive(Debug, Default)]
pub struct SupervisorState {
    panes: HashMap<PaneId, PaneSupervisorState>,
    /// Registry-wide monotonic event counter; orders hook reports against turn
    /// starts without relying on wall-clock milliseconds.
    event_seq: u64,
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
    /// Set when an open question was dropped for the cap.
    pub truncated_questions: bool,
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

// target_ref validator error; first caller is step-3 write verbs.
#[allow(dead_code)]
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

    // target_ref validator; first caller is step-3 write verbs.
    #[allow(dead_code)]
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

    pub fn remove_pane(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
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
        let seq = matches!(new, AgentState::Working).then(|| self.next_event_seq());
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
            if let Some(excerpt) = detect_prose_question(&rows) {
                let question = PendingQuestion {
                    question_id: question_id(pane.raw(), epoch, "prose", &normalize(&excerpt)),
                    kind: QuestionKind::ProseQuestion,
                    excerpt,
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
    use crate::api::schema::supervisor::{Confidence, FactSource, QuestionKind};

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
}

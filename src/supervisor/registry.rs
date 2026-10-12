//! Pure per-pane supervisor registry: generations, lineage and question bookkeeping.
//! No App access and no I/O.

// Later units consume the full registry API.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};

use crate::api::schema::supervisor::{
    GenerationReason, LineageEntry, PendingQuestion, QuestionResolution, ResolutionBy, TargetRef,
};
use crate::layout::PaneId;

pub const MAX_OPEN_QUESTIONS: usize = 16;
pub const MAX_RESOLVED_QUESTIONS: usize = 8;
pub const MAX_LINEAGE: usize = 8;
pub const MAX_EXCERPT_CHARS: usize = 200;

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
}

#[derive(Debug, Clone)]
pub struct PaneSupervisorState {
    /// Starts at 1 on first sight.
    pub generation: u64,
    /// Newest last, capped at `MAX_LINEAGE` (oldest dropped).
    pub lineage: VecDeque<LineageEntry>,
    /// Open and resolved questions in insertion order.
    pub questions: Vec<PendingQuestion>,
    /// Set when an open question was dropped for the cap.
    pub truncated_questions: bool,
    /// Set when a resolved question was dropped for the cap.
    pub truncated_resolved: bool,
    pub turn_started_ms: Option<u64>,
    pub idle_since_ms: Option<u64>,
    pub rescanned_this_idle: bool,
    pub last_transition_ms: Option<u64>,
    /// Positive rule evidence only (Active); None means no evidence.
    pub background_rule_seen_ms: Option<u64>,
    /// Generation in which a background_jobs token was last reported.
    pub background_hook_generation: Option<u64>,
    pub exited: bool,
}

impl PaneSupervisorState {
    fn new() -> Self {
        Self {
            generation: 1,
            lineage: VecDeque::new(),
            questions: Vec::new(),
            truncated_questions: false,
            truncated_resolved: false,
            turn_started_ms: None,
            idle_since_ms: None,
            rescanned_this_idle: false,
            last_transition_ms: None,
            background_rule_seen_ms: None,
            background_hook_generation: None,
            exited: false,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleTarget {
    pub current_generation: Option<u64>,
}

impl SupervisorState {
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

    pub fn bump_generation(&mut self, pane: PaneId, reason: GenerationReason, now_ms: u64) -> u64 {
        let state = self.pane_mut(pane);
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
        // Rule evidence does not survive a generation change; the hook generation is
        // left as is and is now stale relative to `generation`.
        state.background_rule_seen_ms = None;
        state.turn_started_ms = None;
        state.idle_since_ms = None;
        state.rescanned_this_idle = false;
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

    pub fn remove_pane(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
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
    fn registry_bump_clears_rule_evidence_and_invalidates_hook_generation() {
        let mut s = SupervisorState::new();
        {
            let p = s.pane_mut(pid(1));
            p.background_rule_seen_ms = Some(7);
            p.background_hook_generation = Some(1);
            p.turn_started_ms = Some(3);
            p.idle_since_ms = Some(4);
            p.rescanned_this_idle = true;
        }
        s.bump_generation(pid(1), GenerationReason::AgentExited, 20);
        let p = s.pane(pid(1)).unwrap();
        assert_eq!(p.background_rule_seen_ms, None);
        assert_ne!(p.background_hook_generation, Some(p.generation));
        assert_eq!(p.turn_started_ms, None);
        assert_eq!(p.idle_since_ms, None);
        assert!(!p.rescanned_this_idle);
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
}

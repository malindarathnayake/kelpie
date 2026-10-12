//! Pure `agents.digest` derivation: foreground, completion, per-agent facts and paging.
//! No App access and no I/O; the handler gathers inputs and calls into this module.

use crate::api::schema::supervisor::{
    AgentFacts, Background, BlockingReason, Completion, Confidence, Connection, DraftState, Fact,
    FactSource, Foreground, LineageEntry, OwnerInput, PendingQuestion, QuestionKind, TargetRef,
    View,
};
use crate::api::schema::{AgentSessionInfo, AgentStatus};
use crate::supervisor::activity::last_activity_ms;
use sha2::{Digest, Sha256};

/// Maximum serialized digest page size in bytes.
pub const MAX_PAGE_BYTES: usize = 262_144;
/// Bytes reserved for the response envelope around the agents array.
const ENVELOPE_RESERVE: usize = 4096;
/// Longest accepted page cursor, in bytes (CWE-20).
const MAX_CURSOR_LEN: usize = 64;
/// Hex chars of the membership fingerprint carried in a page cursor.
const FINGERPRINT_HEX_CHARS: usize = 16;

/// Derives the foreground fact from the legacy status and the pane's open questions.
pub fn derive_foreground(
    status: AgentStatus,
    open_questions: &[&PendingQuestion],
    now_ms: u64,
) -> Fact<Foreground> {
    let fact = |value, source, confidence| Fact {
        value,
        source,
        observed_at_ms: Some(now_ms),
        confidence,
    };
    match status {
        AgentStatus::Working => fact(Foreground::Working, FactSource::Derived, Confidence::High),
        AgentStatus::Blocked => fact(
            Foreground::AwaitingHuman,
            FactSource::ManifestRule,
            Confidence::High,
        ),
        AgentStatus::Idle | AgentStatus::Done if !open_questions.is_empty() => {
            let structured = open_questions.iter().any(|q| {
                matches!(
                    q.kind,
                    QuestionKind::StructuredApproval | QuestionKind::StructuredDialog
                )
            });
            if structured {
                fact(
                    Foreground::AwaitingHuman,
                    FactSource::ManifestRule,
                    Confidence::High,
                )
            } else {
                fact(
                    Foreground::AwaitingHuman,
                    FactSource::ScreenHeuristic,
                    Confidence::Low,
                )
            }
        }
        AgentStatus::Idle | AgentStatus::Done => {
            fact(Foreground::Idle, FactSource::Derived, Confidence::High)
        }
        AgentStatus::Unknown => Fact {
            value: Foreground::Unknown,
            source: FactSource::Unknown,
            observed_at_ms: None,
            confidence: Confidence::Unknown,
        },
    }
}

/// Completion is `Complete` only on positive evidence for every input; otherwise the
/// first matching blocking reason in priority order.
pub fn derive_completion(
    foreground: Foreground,
    background: Background,
    has_open_question: bool,
    connection: Connection,
) -> (Completion, Option<BlockingReason>) {
    let reason = if connection == Connection::Disconnected {
        Some(BlockingReason::Disconnected)
    } else if foreground == Foreground::Working {
        Some(BlockingReason::ForegroundWorking)
    } else if foreground == Foreground::AwaitingHuman {
        Some(BlockingReason::AwaitingHuman)
    } else if has_open_question {
        Some(BlockingReason::OpenQuestion)
    } else if background == Background::Active {
        Some(BlockingReason::BackgroundActive)
    } else if matches!(background, Background::Unknown | Background::Unsupported) {
        Some(BlockingReason::BackgroundUnknown)
    } else if foreground == Foreground::Unknown {
        Some(BlockingReason::ForegroundUnknown)
    } else {
        None
    };
    // No reason left means foreground Idle, background Inactive, no open question and
    // not disconnected: every enum value other than those maps to a reason above.
    match reason {
        None => (Completion::Complete, None),
        Some(reason) => (Completion::Incomplete, Some(reason)),
    }
}

/// Owner input after the question was asked is evidence of a possible response only;
/// it never resolves the question.
pub fn possible_response_at(asked_at_ms: u64, last_owner_input_ms: Option<u64>) -> Option<u64> {
    last_owner_input_ms.filter(|t| *t > asked_at_ms)
}

/// Everything the digest needs for one agent pane, gathered by the handler.
pub struct AgentFactInputs {
    pub target_ref: TargetRef,
    pub status: AgentStatus,
    /// Registry order, open and resolved.
    pub questions: Vec<PendingQuestion>,
    pub lineage: Vec<LineageEntry>,
    pub background: Fact<Background>,
    pub background_jobs: Option<u32>,
    pub runtime_present: bool,
    pub exited: bool,
    pub last_output_ms: Option<u64>,
    pub last_owner_input_ms: Option<u64>,
    pub last_transition_ms: Option<u64>,
    pub agent: Option<String>,
    pub agent_session: Option<AgentSessionInfo>,
    pub state_change_seq: u64,
    pub completion_seq: u64,
    /// Unresolved questions dropped at the registry cap; counts as an open question
    /// for completion only.
    pub unresolved_overflow: u32,
    pub now_ms: u64,
}

/// Builds the wire facts for one agent from gathered inputs.
pub fn build_agent_facts(inputs: AgentFactInputs) -> AgentFacts {
    let AgentFactInputs {
        target_ref,
        status,
        mut questions,
        lineage,
        background,
        background_jobs,
        runtime_present,
        exited,
        last_output_ms,
        last_owner_input_ms,
        last_transition_ms,
        agent,
        agent_session,
        state_change_seq,
        completion_seq,
        unresolved_overflow,
        now_ms,
    } = inputs;

    let connection = if exited {
        Fact {
            value: Connection::Disconnected,
            source: FactSource::Derived,
            observed_at_ms: Some(now_ms),
            confidence: Confidence::High,
        }
    } else if runtime_present {
        Fact {
            value: Connection::Connected,
            source: FactSource::Derived,
            observed_at_ms: Some(now_ms),
            confidence: Confidence::High,
        }
    } else {
        Fact {
            value: Connection::Unknown,
            source: FactSource::Unknown,
            observed_at_ms: None,
            confidence: Confidence::Unknown,
        }
    };

    for q in questions.iter_mut().filter(|q| q.resolution.is_none()) {
        q.possible_response_at_ms = possible_response_at(q.asked_at_ms, last_owner_input_ms);
    }

    let open: Vec<&PendingQuestion> = questions
        .iter()
        .filter(|q| q.resolution.is_none())
        .collect();
    let foreground = derive_foreground(status, &open, now_ms);
    // A question dropped at the cap was never resolved: completion stays Incomplete
    // (OpenQuestion) even when no open question is left to list.
    let has_open_question = !open.is_empty() || unresolved_overflow > 0;
    let (completion, blocking_reason) = derive_completion(
        foreground.value,
        background.value,
        has_open_question,
        connection.value,
    );

    AgentFacts {
        target_ref,
        foreground,
        background,
        background_jobs,
        connection,
        completion,
        blocking_reason,
        pending_questions: questions,
        owner_input: OwnerInput {
            last_owner_input_at_ms: last_owner_input_ms,
            draft: DraftState::Unknown,
        },
        view: View::Unknown,
        last_activity_at_ms: last_activity_ms(
            last_output_ms,
            last_owner_input_ms,
            last_transition_ms,
        ),
        last_output_at_ms: last_output_ms,
        agent,
        agent_session,
        lineage,
        agent_status: status,
        state_change_seq,
        completion_seq,
    }
}

/// Why a page cursor was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorError {
    /// Malformed, too long, out of range, or a non-first page without a fingerprint.
    Invalid,
    /// Well formed, but the agent membership changed since the cursor was issued.
    Stale,
}

/// First 16 lowercase hex chars of SHA-256 over the ordered public pane ids of the
/// full filtered membership, each id terminated by `\n` (never part of an id).
pub fn membership_fingerprint<'a>(pane_ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut h = Sha256::new();
    for id in pane_ids {
        h.update(id.as_bytes());
        h.update(b"\n");
    }
    let digest = h.finalize();
    let mut hex = String::with_capacity(FINGERPRINT_HEX_CHARS);
    for b in &digest[..FINGERPRINT_HEX_CHARS / 2] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Parses an opaque `o:<n>:<fp>` page cursor against `count` agents whose membership
/// fingerprint is `fingerprint`. None means the first page; a bare `o:0` is also the
/// first page. A fingerprint, when present, must match the current membership.
pub fn parse_cursor(
    cursor: Option<&str>,
    count: usize,
    fingerprint: &str,
) -> Result<usize, CursorError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    if cursor.len() > MAX_CURSOR_LEN {
        return Err(CursorError::Invalid);
    }
    let rest = cursor.strip_prefix("o:").ok_or(CursorError::Invalid)?;
    let (digits, fp) = match rest.split_once(':') {
        Some((digits, fp)) => (digits, Some(fp)),
        None => (rest, None),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CursorError::Invalid);
    }
    let n: usize = digits.parse().map_err(|_| CursorError::Invalid)?;
    match fp {
        Some(fp) => {
            let well_formed = fp.len() == FINGERPRINT_HEX_CHARS
                && fp.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !well_formed {
                return Err(CursorError::Invalid);
            }
            // Checked before the range so a shrunken membership reads as stale.
            if fp != fingerprint {
                return Err(CursorError::Stale);
            }
        }
        None if n > 0 => return Err(CursorError::Invalid),
        None => {}
    }
    if n < count || (count == 0 && n == 0) {
        Ok(n)
    } else {
        Err(CursorError::Invalid)
    }
}

/// One page of digest agents.
pub struct Page {
    pub agents: Vec<AgentFacts>,
    pub next_page: Option<String>,
    pub truncated_questions: bool,
}

fn serialized_len(agent: &AgentFacts) -> usize {
    serde_json::to_vec(agent)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

/// Drops questions (resolved first, then open; oldest first) until `agent` fits `budget`
/// or none remain. Returns whether any question was dropped.
fn trim_questions(agent: &mut AgentFacts, budget: usize) -> bool {
    let mut dropped = false;
    while serialized_len(agent) > budget {
        let index = agent
            .pending_questions
            .iter()
            .position(|q| q.resolution.is_some())
            .or_else(|| (!agent.pending_questions.is_empty()).then_some(0));
        let Some(index) = index else {
            break;
        };
        agent.pending_questions.remove(index);
        dropped = true;
    }
    dropped
}

/// Builds one page from `remaining`, the agents from absolute position `offset` onward,
/// bounded by `max_bytes` (CWE-770). Always includes at least one agent when any remain;
/// `next_page` carries the absolute offset of the first agent left off the page and the
/// membership `fingerprint` it was computed against.
///
/// Perf (tier standard): one serialization per agent on the page, plus one per dropped
/// question when a single oversized agent must be trimmed.
pub fn build_page(
    remaining: Vec<AgentFacts>,
    offset: usize,
    max_bytes: usize,
    fingerprint: &str,
) -> Page {
    let total = remaining.len();
    let budget = max_bytes.saturating_sub(ENVELOPE_RESERVE);
    let mut agents = Vec::new();
    let mut used = 0usize;
    let mut truncated_questions = false;
    for mut agent in remaining {
        let size = serialized_len(&agent);
        if agents.is_empty() {
            if size > budget {
                truncated_questions |= trim_questions(&mut agent, budget);
            }
            used = serialized_len(&agent);
            agents.push(agent);
            continue;
        }
        if used.saturating_add(size) > budget {
            break;
        }
        used += size;
        agents.push(agent);
    }
    let taken = agents.len();
    let next_page =
        (taken < total).then(|| format!("o:{}:{fingerprint}", offset.saturating_add(taken)));
    Page {
        agents,
        next_page,
        truncated_questions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::supervisor::{
        AgentsDigest, DigestTruncation, GenerationReason, QuestionResolution, ResolutionBy,
    };
    use crate::supervisor::registry::{SupervisorState, MAX_RESOLVED_QUESTIONS};

    fn question(id: &str, kind: QuestionKind, asked: u64, resolved: bool) -> PendingQuestion {
        PendingQuestion {
            question_id: id.to_string(),
            kind,
            excerpt: format!("{id}?"),
            asked_at_ms: asked,
            source: FactSource::ScreenHeuristic,
            confidence: Confidence::Low,
            possible_response_at_ms: None,
            resolution: resolved.then_some(QuestionResolution {
                at_ms: asked + 1,
                by: ResolutionBy::Superseded,
            }),
        }
    }

    fn hook_background(value: Background) -> Fact<Background> {
        Fact {
            value,
            source: FactSource::Hook,
            observed_at_ms: Some(1),
            confidence: Confidence::High,
        }
    }

    fn inputs(status: AgentStatus, questions: Vec<PendingQuestion>) -> AgentFactInputs {
        AgentFactInputs {
            target_ref: TargetRef {
                server_epoch: "e".into(),
                pane_id: "p1".into(),
                generation: 1,
            },
            status,
            questions,
            lineage: vec![],
            background: hook_background(Background::Inactive),
            background_jobs: Some(0),
            runtime_present: true,
            exited: false,
            last_output_ms: None,
            last_owner_input_ms: None,
            last_transition_ms: None,
            agent: None,
            agent_session: None,
            state_change_seq: 0,
            completion_seq: 0,
            unresolved_overflow: 0,
            now_ms: 100,
        }
    }

    #[test]
    fn supervisor_digest_completion_table() {
        use Background as B;
        use BlockingReason as R;
        use Connection as C;
        use Foreground as F;
        let rows = [
            (F::Idle, B::Inactive, false, C::Connected, None),
            (
                F::Idle,
                B::Unknown,
                false,
                C::Connected,
                Some(R::BackgroundUnknown),
            ),
            (
                F::Idle,
                B::Active,
                false,
                C::Connected,
                Some(R::BackgroundActive),
            ),
            (
                F::Idle,
                B::Inactive,
                true,
                C::Connected,
                Some(R::OpenQuestion),
            ),
            (
                F::AwaitingHuman,
                B::Inactive,
                true,
                C::Connected,
                Some(R::AwaitingHuman),
            ),
            (
                F::Working,
                B::Inactive,
                false,
                C::Connected,
                Some(R::ForegroundWorking),
            ),
            (
                F::Idle,
                B::Inactive,
                false,
                C::Disconnected,
                Some(R::Disconnected),
            ),
            (
                F::Working,
                B::Active,
                true,
                C::Disconnected,
                Some(R::Disconnected),
            ),
            (
                F::Unknown,
                B::Inactive,
                false,
                C::Connected,
                Some(R::ForegroundUnknown),
            ),
            (
                F::Idle,
                B::Unsupported,
                false,
                C::Connected,
                Some(R::BackgroundUnknown),
            ),
            (F::Idle, B::Inactive, false, C::Unknown, None),
        ];
        for (fg, bg, open, conn, reason) in rows {
            let completion = if reason.is_none() {
                Completion::Complete
            } else {
                Completion::Incomplete
            };
            assert_eq!(
                derive_completion(fg, bg, open, conn),
                (completion, reason),
                "row {fg:?} {bg:?} {open} {conn:?}"
            );
        }
    }

    #[test]
    fn supervisor_digest_foreground_table() {
        let prose = question("p", QuestionKind::ProseQuestion, 1, false);
        let structured = question("s", QuestionKind::StructuredApproval, 1, false);

        let f = derive_foreground(AgentStatus::Working, &[], 7);
        assert_eq!(
            (f.value, f.source, f.confidence, f.observed_at_ms),
            (
                Foreground::Working,
                FactSource::Derived,
                Confidence::High,
                Some(7)
            )
        );
        let f = derive_foreground(AgentStatus::Blocked, &[], 7);
        assert_eq!(
            (f.value, f.source, f.confidence),
            (
                Foreground::AwaitingHuman,
                FactSource::ManifestRule,
                Confidence::High
            )
        );
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            let f = derive_foreground(status, &[], 7);
            assert_eq!(
                (f.value, f.source, f.confidence),
                (Foreground::Idle, FactSource::Derived, Confidence::High)
            );
        }
        let f = derive_foreground(AgentStatus::Idle, &[&prose], 7);
        assert_eq!(
            (f.value, f.source, f.confidence),
            (
                Foreground::AwaitingHuman,
                FactSource::ScreenHeuristic,
                Confidence::Low
            )
        );
        let f = derive_foreground(AgentStatus::Done, &[&prose, &structured], 7);
        assert_eq!(
            (f.value, f.source, f.confidence),
            (
                Foreground::AwaitingHuman,
                FactSource::ManifestRule,
                Confidence::High
            )
        );
        let facts = build_agent_facts(inputs(
            AgentStatus::Idle,
            vec![question("r", QuestionKind::ProseQuestion, 1, true)],
        ));
        assert_eq!(facts.foreground.value, Foreground::Idle);
        let f = derive_foreground(AgentStatus::Unknown, &[&prose], 7);
        assert_eq!(
            (f.value, f.source, f.confidence, f.observed_at_ms),
            (
                Foreground::Unknown,
                FactSource::Unknown,
                Confidence::Unknown,
                None
            )
        );
    }

    #[test]
    fn supervisor_digest_possible_response() {
        assert_eq!(possible_response_at(10, Some(11)), Some(11));
        assert_eq!(possible_response_at(10, Some(10)), None);
        assert_eq!(possible_response_at(10, Some(9)), None);
        assert_eq!(possible_response_at(10, None), None);

        let mut i = inputs(
            AgentStatus::Idle,
            vec![
                question("open", QuestionKind::ProseQuestion, 10, false),
                question("done", QuestionKind::ProseQuestion, 10, true),
            ],
        );
        i.last_owner_input_ms = Some(50);
        let facts = build_agent_facts(i);
        let open = &facts.pending_questions[0];
        let done = &facts.pending_questions[1];
        assert_eq!(open.possible_response_at_ms, Some(50));
        assert!(open.resolution.is_none());
        assert_eq!(done.possible_response_at_ms, None);
        assert_eq!(facts.owner_input.last_owner_input_at_ms, Some(50));
        assert_eq!(facts.owner_input.draft, DraftState::Unknown);
        assert_eq!(facts.view, View::Unknown);
    }

    #[test]
    fn supervisor_digest_question_after_turn_stays_incomplete() {
        let facts = build_agent_facts(inputs(
            AgentStatus::Idle,
            vec![question("q", QuestionKind::ProseQuestion, 10, false)],
        ));
        assert_eq!(facts.completion, Completion::Incomplete);
        assert_eq!(facts.blocking_reason, Some(BlockingReason::AwaitingHuman));

        let facts = build_agent_facts(inputs(
            AgentStatus::Idle,
            vec![question("q", QuestionKind::ProseQuestion, 10, true)],
        ));
        assert_eq!(facts.completion, Completion::Complete);
        assert_eq!(facts.blocking_reason, None);
    }

    #[test]
    fn supervisor_digest_connection() {
        let mut i = inputs(AgentStatus::Idle, vec![]);
        i.exited = true;
        let facts = build_agent_facts(i);
        assert_eq!(facts.connection.value, Connection::Disconnected);
        assert_eq!(facts.completion, Completion::Incomplete);
        assert_eq!(facts.blocking_reason, Some(BlockingReason::Disconnected));

        let facts = build_agent_facts(inputs(AgentStatus::Idle, vec![]));
        assert_eq!(facts.connection.value, Connection::Connected);
        assert_eq!(facts.connection.source, FactSource::Derived);
        assert_eq!(facts.connection.confidence, Confidence::High);

        let mut i = inputs(AgentStatus::Idle, vec![]);
        i.runtime_present = false;
        let facts = build_agent_facts(i);
        assert_eq!(facts.connection.value, Connection::Unknown);
        assert_eq!(facts.connection.source, FactSource::Unknown);
        assert_eq!(facts.connection.confidence, Confidence::Unknown);
    }

    #[test]
    fn supervisor_digest_activity_stamps() {
        let mut i = inputs(AgentStatus::Idle, vec![]);
        i.last_output_ms = Some(30);
        i.last_owner_input_ms = Some(20);
        i.last_transition_ms = Some(40);
        let facts = build_agent_facts(i);
        assert_eq!(facts.last_output_at_ms, Some(30));
        assert_eq!(facts.last_activity_at_ms, Some(40));
    }

    #[test]
    fn supervisor_digest_overflow_keeps_completion_incomplete() {
        let mut i = inputs(AgentStatus::Idle, vec![]);
        i.unresolved_overflow = 1;
        let facts = build_agent_facts(i);
        assert_eq!(facts.foreground.value, Foreground::Idle);
        assert_eq!(facts.completion, Completion::Incomplete);
        assert_eq!(facts.blocking_reason, Some(BlockingReason::OpenQuestion));
        assert!(facts.pending_questions.is_empty());

        // Retained questions all resolved, but one unresolved was dropped earlier.
        let mut i = inputs(
            AgentStatus::Idle,
            vec![question("r", QuestionKind::ProseQuestion, 1, true)],
        );
        i.unresolved_overflow = 1;
        assert_eq!(build_agent_facts(i).completion, Completion::Incomplete);
    }

    #[test]
    fn supervisor_digest_cursor() {
        let fp = membership_fingerprint(["p0", "p1", "p2"]);
        assert_eq!(fp.len(), 16);
        assert!(fp.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        let at = |n: usize| format!("o:{n}:{fp}");
        assert_eq!(parse_cursor(None, 3, &fp), Ok(0));
        assert_eq!(parse_cursor(Some("o:0"), 3, &fp), Ok(0));
        assert_eq!(parse_cursor(Some(&at(0)), 3, &fp), Ok(0));
        assert_eq!(parse_cursor(Some(&at(2)), 3, &fp), Ok(2));
        assert_eq!(
            parse_cursor(Some(&at(3)), 3, &fp),
            Err(CursorError::Invalid)
        );
        // Legacy cursor without a fingerprint.
        assert_eq!(parse_cursor(Some("o:2"), 3, &fp), Err(CursorError::Invalid));
        // Empty or malformed fingerprint segment.
        for bad in [
            "o:1:".to_string(),
            "o:0:".to_string(),
            format!("o:1:{}", fp.to_uppercase()),
            format!("o:1:{fp}0"),
            format!("o:1:{fp}:x"),
        ] {
            assert_eq!(
                parse_cursor(Some(&bad), 3, &fp),
                Err(CursorError::Invalid),
                "{bad}"
            );
        }
        for bad in ["o:x", "x", "o:-1", "", "o:", "o:+1"] {
            assert_eq!(
                parse_cursor(Some(bad), 3, &fp),
                Err(CursorError::Invalid),
                "{bad}"
            );
        }
        let long = format!("o:{}:{fp}", "0".repeat(60));
        assert_eq!(parse_cursor(Some(&long), 3, &fp), Err(CursorError::Invalid));
        // Membership changed: stale at any offset, also when now out of range.
        let other = membership_fingerprint(["p0", "p2"]);
        assert_ne!(other, fp);
        assert_eq!(
            parse_cursor(Some(&at(1)), 2, &other),
            Err(CursorError::Stale)
        );
        assert_eq!(
            parse_cursor(Some(&at(2)), 2, &other),
            Err(CursorError::Stale)
        );
        assert_eq!(
            parse_cursor(Some(&at(0)), 2, &other),
            Err(CursorError::Stale)
        );
        let empty = membership_fingerprint(std::iter::empty::<&str>());
        assert_eq!(parse_cursor(Some("o:0"), 0, &empty), Ok(0));
        assert_eq!(
            parse_cursor(Some("o:1"), 0, &empty),
            Err(CursorError::Invalid)
        );
    }

    #[test]
    fn supervisor_digest_fingerprint_order_and_boundaries() {
        assert_ne!(
            membership_fingerprint(["a", "b"]),
            membership_fingerprint(["b", "a"])
        );
        assert_ne!(
            membership_fingerprint(["ab", "c"]),
            membership_fingerprint(["a", "bc"])
        );
        assert_eq!(
            membership_fingerprint(["a", "b"]),
            membership_fingerprint(["a", "b"])
        );
    }

    fn full_agent(n: usize) -> AgentFacts {
        let excerpt = "a".repeat(200);
        let mut questions = Vec::new();
        for i in 0..8 {
            let mut q = question(
                &format!("{n:04}-r{i:02}-{:0>24}", 0),
                QuestionKind::ProseQuestion,
                1,
                true,
            );
            q.excerpt = excerpt.clone();
            questions.push(q);
        }
        for i in 0..16 {
            let mut q = question(
                &format!("{n:04}-o{i:02}-{:0>24}", 0),
                QuestionKind::ProseQuestion,
                1,
                false,
            );
            q.excerpt = excerpt.clone();
            questions.push(q);
        }
        let mut i = inputs(AgentStatus::Idle, questions);
        i.target_ref.pane_id = format!("w-pane-{n}");
        i.lineage = (2..10)
            .map(|g| LineageEntry {
                generation: g,
                reason: GenerationReason::SessionChanged,
                at_ms: 1_700_000_000_000 + g,
            })
            .collect();
        i.last_owner_input_ms = Some(1_700_000_000_000);
        i.last_output_ms = Some(1_700_000_000_000);
        i.agent = Some("claude".into());
        build_agent_facts(i)
    }

    #[test]
    fn supervisor_digest_t5e_fifteen_agents_fit_one_page() {
        let facts: Vec<AgentFacts> = (0..15).map(full_agent).collect();
        let page = build_page(facts, 0, MAX_PAGE_BYTES, "0123456789abcdef");
        assert_eq!(page.agents.len(), 15);
        assert_eq!(page.next_page, None);
        assert!(!page.truncated_questions);
        let digest = AgentsDigest {
            server_epoch: "0".repeat(32),
            trace_id: format!("{}-{:016x}", "0".repeat(8), u64::MAX),
            as_of_seq: u64::MAX,
            generated_at_ms: u64::MAX,
            agents: page.agents,
            monitors: vec![],
            next_page: None,
            truncated: DigestTruncation {
                questions: true,
                resolved_history: true,
            },
        };
        // Same serialization as the handler's `encode_success` (not reachable from here).
        let envelope = crate::api::schema::SuccessResponse {
            id: "id".into(),
            result: crate::api::schema::ResponseResult::AgentsDigest(Box::new(digest)),
        };
        let len = serde_json::to_string(&envelope).unwrap().len();
        assert!(len <= MAX_PAGE_BYTES, "response is {len} bytes");
    }

    #[test]
    fn supervisor_digest_t5e_resolved_cap() {
        let pane = crate::layout::PaneId::from_raw(1);
        let mut s = SupervisorState::new();
        for i in 0..20 {
            s.insert_question(
                pane,
                question(&format!("r{i}"), QuestionKind::ProseQuestion, i, true),
            );
        }
        let p = s.pane(pane).unwrap();
        let resolved = p
            .questions
            .iter()
            .filter(|q| q.resolution.is_some())
            .count();
        assert_eq!(resolved, MAX_RESOLVED_QUESTIONS);
        assert_eq!(resolved, 8);
        assert!(p.truncated_resolved);
        assert!(!p.truncated_questions);
    }

    #[test]
    fn supervisor_digest_paging_and_trim() {
        let small = |n: usize| {
            let mut i = inputs(AgentStatus::Idle, vec![]);
            i.target_ref.pane_id = format!("p{n}");
            build_agent_facts(i)
        };
        let facts: Vec<AgentFacts> = (0..5).map(small).collect();
        let fp = membership_fingerprint(facts.iter().map(|a| a.target_ref.pane_id.as_str()));
        let one = serialized_len(&facts[0]);
        let max = ENVELOPE_RESERVE + 2 * one + one / 2;
        let page = build_page(facts.clone(), 0, max, &fp);
        assert_eq!(page.agents.len(), 2);
        let next = format!("o:2:{fp}");
        assert_eq!(page.next_page.as_deref(), Some(next.as_str()));
        assert_eq!(parse_cursor(Some(&next), 5, &fp), Ok(2));
        assert!(!page.truncated_questions);
        let page = build_page(facts[2..].to_vec(), 2, max, &fp);
        assert_eq!(page.agents.len(), 2);
        let next = format!("o:4:{fp}");
        assert_eq!(page.next_page.as_deref(), Some(next.as_str()));
        assert_eq!(page.agents[0].target_ref.pane_id, "p2");
        let page = build_page(facts[4..].to_vec(), 4, max, &fp);
        assert_eq!(page.agents.len(), 1);
        assert_eq!(page.next_page, None);

        let big = full_agent(0);
        let big_len = serialized_len(&big);
        let max = ENVELOPE_RESERVE + big_len - 3000;
        let page = build_page(vec![big, small(1)], 0, max, &fp);
        assert_eq!(page.agents.len(), 1);
        assert!(page.truncated_questions);
        let next = format!("o:1:{fp}");
        assert_eq!(page.next_page.as_deref(), Some(next.as_str()));
        let kept = &page.agents[0].pending_questions;
        assert!(kept.len() < 24);
        // Resolved questions go first, oldest first; all 16 open questions remain.
        assert_eq!(kept.iter().filter(|q| q.resolution.is_none()).count(), 16);
        assert!(serialized_len(&page.agents[0]) <= max - ENVELOPE_RESERVE);
    }
}

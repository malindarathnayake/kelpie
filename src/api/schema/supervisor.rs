//! Read-only supervisor wire types (facts and digest).

use serde::{Deserialize, Serialize};

/// Identifies one agent target by server epoch, pane, and generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TargetRef {
    pub server_epoch: String,
    pub pane_id: String,
    pub generation: u64,
}

/// Where a fact originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    Hook,
    Protocol,
    ManifestRule,
    ScreenHeuristic,
    Derived,
    #[serde(other)]
    Unknown,
}

/// How much to trust a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
    #[serde(other)]
    Unknown,
}

/// A value together with its provenance and confidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Fact<T> {
    pub value: T,
    pub source: FactSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<u64>,
    pub confidence: Confidence,
}

/// Foreground activity of the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Foreground {
    Working,
    Idle,
    AwaitingHuman,
    #[serde(other)]
    Unknown,
}

/// Background job activity of the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    Active,
    Inactive,
    Unsupported,
    #[serde(other)]
    Unknown,
}

/// Connection state between kelpie and the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Connected,
    Disconnected,
    Stale,
    #[serde(other)]
    Unknown,
}

/// Whether the agent's work is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Complete,
    Incomplete,
    #[serde(other)]
    Unknown,
}

/// Why an agent is not considered complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockingReason {
    ForegroundWorking,
    AwaitingHuman,
    OpenQuestion,
    BackgroundActive,
    BackgroundUnknown,
    ForegroundUnknown,
    Disconnected,
    #[serde(other)]
    Unknown,
}

/// How a pending question was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    StructuredApproval,
    StructuredDialog,
    ProseQuestion,
    #[serde(other)]
    Unknown,
}

/// Who or what resolved a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionBy {
    OwnerAnswer,
    OwnerResolved,
    HostReported,
    Superseded,
    #[serde(other)]
    Unknown,
}

/// Resolution record for a question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QuestionResolution {
    pub at_ms: u64,
    pub by: ResolutionBy,
}

/// A question the agent has asked and that may still be open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PendingQuestion {
    pub question_id: String,
    pub kind: QuestionKind,
    pub excerpt: String,
    pub asked_at_ms: u64,
    pub source: FactSource,
    pub confidence: Confidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub possible_response_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<QuestionResolution>,
}

/// State of the owner's input draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftState {
    Clean,
    Dirty,
    #[serde(other)]
    Unknown,
}

/// Owner input activity for an agent pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OwnerInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_owner_input_at_ms: Option<u64>,
    pub draft: DraftState,
    /// A one-shot operator handoff is armed: the next `agent.submit` to this pane may be
    /// accepted once while `draft` stays the latch result. Read-only.
    #[serde(default)]
    pub handoff_armed: bool,
    /// The owner delegated this pane: while `active`, supervised writes are not blocked by
    /// earlier owner input (the draft check and the write fence still apply). Read-only;
    /// only the owner grants a delegation, from an attached Kelpie window.
    #[serde(default)]
    pub delegation: DelegationFacts,
}

/// Owner-granted delegation of an agent pane.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct DelegationFacts {
    pub active: bool,
    /// Lasts until revoked or the pane closes.
    #[serde(default)]
    pub permanent: bool,
    /// Unix ms when a timed delegation ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// Which view the pane is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Main,
    NonMain,
    #[serde(other)]
    Unknown,
}

/// Why a target generation changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GenerationReason {
    SessionChanged,
    ResetWritten,
    AgentStarted,
    AgentExited,
    PaneMoved,
    #[serde(other)]
    Unknown,
}

/// One entry in a target's generation lineage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LineageEntry {
    pub generation: u64,
    pub reason: GenerationReason,
    pub at_ms: u64,
}

/// Full set of supervisor-visible facts for one agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentFacts {
    pub target_ref: TargetRef,
    pub foreground: Fact<Foreground>,
    pub background: Fact<Background>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_jobs: Option<u32>,
    pub connection: Fact<Connection>,
    pub completion: Completion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_reason: Option<BlockingReason>,
    #[serde(default)]
    pub pending_questions: Vec<PendingQuestion>,
    pub owner_input: OwnerInput,
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<super::AgentSessionInfo>,
    #[serde(default)]
    pub lineage: Vec<LineageEntry>,
    pub agent_status: super::AgentStatus,
    pub state_change_seq: u64,
    pub completion_seq: u64,
}

/// Filter selecting which agents a digest covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DigestFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_ids: Option<Vec<String>>,
}

/// Parameters for an agents digest request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentsDigestParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<DigestFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Flags showing which digest sections were truncated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DigestTruncation {
    #[serde(default)]
    pub questions: bool,
    #[serde(default)]
    pub resolved_history: bool,
}

/// Liveness info for a connected monitor principal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MonitorInfo {
    pub principal_id: String,
    pub last_seen_ms_ago: u64,
    pub stale: bool,
}

/// Point-in-time digest of all agents' facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentsDigest {
    pub server_epoch: String,
    pub trace_id: String,
    pub as_of_seq: u64,
    pub generated_at_ms: u64,
    pub agents: Vec<AgentFacts>,
    pub monitors: Vec<MonitorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
    pub truncated: DigestTruncation,
}

/// Parameters for `agent.submit`: one prompt submission to an exact target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSubmitParams {
    pub target_ref: TargetRef,
    /// Prompt text, 1-8192 bytes.
    pub text: String,
    /// Caller-chosen idempotency key, 1-64 chars of `[A-Za-z0-9._:-]`.
    pub submit_id: String,
}

/// Parameters for `pane.report_prompt_submitted`: an agent's prompt-submit hook reports
/// the hash of the prompt it received. Never carries the prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneReportPromptSubmittedParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    /// First 16 lowercase hex chars of SHA-256 over the normalised prompt text (see
    /// `supervisor::submit::prompt_ack_sha256_16`).
    pub prompt_sha256_16: String,
}

/// Parameters for `agent.answer`: `agent.submit` plus the open prose question it answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentAnswerParams {
    pub target_ref: TargetRef,
    /// The newest open `prose_question` of the pane, still shown on screen.
    pub question_id: String,
    /// Answer text, 1-8192 bytes.
    pub text: String,
    /// Caller-chosen idempotency key, 1-64 chars of `[A-Za-z0-9._:-]`.
    pub submit_id: String,
}

/// Parameters for `agent.interrupt`, `agent.reset`, and `agent.clear_draft`: no text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentControlParams {
    pub target_ref: TargetRef,
    /// Caller-chosen idempotency key, 1-64 chars of `[A-Za-z0-9._:-]`.
    pub submit_id: String,
}

/// Parameters for `agent.resume`: launches the agent's resume command for `session_id` in a
/// pane that hosts no agent, through the `agent.start` path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentResumeParams {
    /// Pane to launch in; it must host no running or known agent and no pending launch.
    pub pane_id: String,
    /// Agent session id to resume, 1-256 characters, no control characters.
    pub session_id: String,
    /// Caller-chosen idempotency key, 1-64 chars of `[A-Za-z0-9._:-]`.
    pub submit_id: String,
    /// Agent label to resume (claude, codex, cursor); default claude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Parameters for `agent.undelegate`: ends the owner-granted delegation of a pane. There is
/// no method that grants one; only the owner can, from an attached Kelpie window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentUndelegateParams {
    pub pane_id: String,
}

/// Parameters for `agent.submit_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSubmitStatusParams {
    pub submit_id: String,
}

/// Lifecycle of one submission receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmitState {
    /// Reserved; the write is queued or in progress.
    Accepted,
    /// Text and Enter were written to the pane. Terminal for delivery: kelpie never claims
    /// the agent started or acknowledged the turn (activity facts are reported separately).
    Written,
    /// Dropped by the write-boundary fence; zero bytes were written.
    Fenced,
    /// The outcome is not known; never resend automatically.
    Indeterminate,
    /// `resume` only: the agent.start path accepted the launch (the command was sent to
    /// the pane's shell). Says nothing about whether the session was resumed.
    Launched,
    /// `resume` only: the agent.start path refused the launch; nothing was sent.
    Failed,
    #[serde(other)]
    Unknown,
}

/// Why a receipt is `fenced` or `indeterminate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmitReason {
    OwnerInput,
    GenerationChanged,
    PaneClosed,
    OwnerInputDuringWrite,
    GenerationChangedDuringWrite,
    PaneClosedDuringWrite,
    DeadlineExpired,
    QueueFailed,
    WriteFailed,
    PtyClosed,
    /// `resume`: the agent.start path refused the launch (`start_error` has its code).
    StartFailed,
    /// `resume`: the start command was sent to the pane, then the start path failed; the
    /// launch outcome is not known (`start_error` has the code).
    StartUnconfirmed,
    #[serde(other)]
    Unknown,
}

/// Which verb created a receipt. All kinds share one ledger and one idempotency space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmitKind {
    Submit,
    Answer,
    Interrupt,
    Reset,
    Resume,
    ClearDraft,
    #[serde(other)]
    Unknown,
}

/// Evidence state of an `agent.reset` receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetEvidence {
    /// No replacement session has been observed (yet); the reader decides.
    Pending,
    /// A different session ref was observed after the write: with start source `clear`
    /// for immediate resets, or in a supported-agent report for lazy resets.
    Verified,
    /// The write did not complete cleanly, or another conversation replacement was
    /// observed first. Never carries a session.
    Unverified,
    #[serde(other)]
    Unknown,
}

/// Evidence state of an `agent.resume` receipt. Order-based: only the first session
/// report from the pane after the launch counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResumeEvidence {
    /// No session report with the requested id (or a different id) seen after the launch.
    Pending,
    /// The pane's agent reported the requested session id after the launch, with start
    /// source `resume` or `startup`.
    Verified,
    /// The first session id the pane's agent reported after the launch differs from the
    /// requested one (`reported_session` holds it). Never a success.
    Mismatch,
    #[serde(other)]
    Unknown,
}

/// Session identity (kind and value) recorded by a verified reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReceiptSession {
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}

/// Whether the agent acknowledged the submission natively. `confirmed`: the agent's own
/// prompt-submit hook reported the same text hash for the pane after the write (Claude
/// `UserPromptSubmit`). Otherwise `unsupported`; never inferred from activity edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmitAcknowledged {
    Unsupported,
    Confirmed,
    #[serde(other)]
    Unknown,
}

/// Receipt for one `agent.submit`, `agent.answer`, `agent.interrupt`, `agent.reset` or
/// `agent.resume`. Never contains the submitted text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitReceipt {
    pub submit_id: String,
    pub target_ref: TargetRef,
    pub state: SubmitState,
    pub accepted_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SubmitReason>,
    pub acknowledged: SubmitAcknowledged,
    /// When `acknowledged` is `confirmed`: time the matching prompt-submit hook report was
    /// applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at_ms: Option<u64>,
    /// First 16 lowercase hex chars of SHA-256 over the submitted text bytes (`resume`:
    /// over the requested session id).
    pub text_sha256_16: String,
    /// True when acceptance consumed a one-shot operator handoff.
    #[serde(default)]
    pub handoff: bool,
    /// Verb that created the receipt.
    #[serde(default = "default_submit_kind")]
    pub kind: SubmitKind,
    /// `answer` only: the question the text answers. The question is not resolved by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
    /// `interrupt` only: time of the first Idle or AwaitingHuman edge ordered after the
    /// write completed. An uncorrelated observation, not an acknowledgement: the edge may
    /// have other causes, and edges before the write completed are never recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_end_observed_after_write_at_ms: Option<u64>,
    /// `reset` only: evidence state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<ResetEvidence>,
    /// `reset` only, when `verified`: the observed new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_session: Option<ReceiptSession>,
    /// `reset`, when `verified`: the target generation after the session change.
    /// `resume`: the pane's generation once the launched agent was observed (after its
    /// AgentStarted bump), or at the session report that settled the evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_generation: Option<u64>,
    /// `resume` only: the requested session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `resume` only: evidence state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeEvidence>,
    /// `resume` only, when `mismatch`: the session the agent reported instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_session: Option<ReceiptSession>,
    /// `resume` only, when `verified` or `mismatch`: the reported session start source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
    /// `resume` only, when `launched`: time the agent.start path accepted the launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_at_ms: Option<u64>,
    /// `resume` only, when `failed`: the agent.start error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
}

fn default_submit_kind() -> SubmitKind {
    SubmitKind::Submit
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn supervisor_enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(Foreground::AwaitingHuman).unwrap(),
            json!("awaiting_human")
        );
        assert_eq!(
            serde_json::to_value(FactSource::ManifestRule).unwrap(),
            json!("manifest_rule")
        );
        assert_eq!(
            serde_json::to_value(BlockingReason::BackgroundUnknown).unwrap(),
            json!("background_unknown")
        );
    }

    #[test]
    fn supervisor_enums_decode_unknown_strings_as_unknown() {
        let v = json!("telepathy");
        assert_eq!(
            serde_json::from_value::<Foreground>(v.clone()).unwrap(),
            Foreground::Unknown
        );
        assert_eq!(
            serde_json::from_value::<Background>(v.clone()).unwrap(),
            Background::Unknown
        );
        assert_eq!(
            serde_json::from_value::<QuestionKind>(v.clone()).unwrap(),
            QuestionKind::Unknown
        );
        assert_eq!(
            serde_json::from_value::<ResolutionBy>(v.clone()).unwrap(),
            ResolutionBy::Unknown
        );
        assert_eq!(
            serde_json::from_value::<GenerationReason>(v).unwrap(),
            GenerationReason::Unknown
        );
    }

    #[test]
    fn reset_written_generation_reason_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(GenerationReason::ResetWritten).unwrap(),
            json!("reset_written")
        );
        assert_eq!(
            serde_json::from_value::<GenerationReason>(json!("reset_written")).unwrap(),
            GenerationReason::ResetWritten
        );
    }

    #[test]
    fn agent_facts_omits_absent_optionals() {
        let facts = AgentFacts {
            target_ref: TargetRef {
                server_epoch: "e1".into(),
                pane_id: "p1".into(),
                generation: 1,
            },
            foreground: Fact {
                value: Foreground::Idle,
                source: FactSource::Hook,
                observed_at_ms: None,
                confidence: Confidence::High,
            },
            background: Fact {
                value: Background::Inactive,
                source: FactSource::Derived,
                observed_at_ms: None,
                confidence: Confidence::Medium,
            },
            background_jobs: None,
            connection: Fact {
                value: Connection::Connected,
                source: FactSource::Protocol,
                observed_at_ms: None,
                confidence: Confidence::High,
            },
            completion: Completion::Complete,
            blocking_reason: None,
            pending_questions: vec![],
            owner_input: OwnerInput {
                last_owner_input_at_ms: None,
                draft: DraftState::Clean,
                handoff_armed: false,
                delegation: Default::default(),
            },
            view: View::Main,
            last_activity_at_ms: None,
            last_output_at_ms: None,
            agent: None,
            agent_session: None,
            lineage: vec![],
            agent_status: super::super::AgentStatus::Idle,
            state_change_seq: 0,
            completion_seq: 0,
        };
        let value = serde_json::to_value(&facts).unwrap();
        let obj = value.as_object().unwrap();
        for key in [
            "background_jobs",
            "blocking_reason",
            "last_activity_at_ms",
            "last_output_at_ms",
            "agent",
            "agent_session",
        ] {
            assert!(!obj.contains_key(key), "unexpected key {key}");
        }
        let back: AgentFacts = serde_json::from_value(value).unwrap();
        assert_eq!(back, facts);
    }

    #[test]
    fn agents_digest_params_accept_empty_object() {
        let p: AgentsDigestParams = serde_json::from_str("{}").unwrap();
        assert!(p.filter.is_none());
        assert!(p.page_cursor.is_none());
        assert!(p.trace_id.is_none());
    }
}

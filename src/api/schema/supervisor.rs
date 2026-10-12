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

/// Connection state between herdr and the agent.
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

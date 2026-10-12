//! Pure background-work evidence derivation. No App access and no I/O.

use crate::api::schema::supervisor::{Background, Confidence, Fact, FactSource};
use crate::metadata_tokens::TokenValidity;

/// Manifest rules `(agent label, rule id)` whose match is positive evidence of
/// background work. Consumed on demand; upstream detection is not modified.
pub const BACKGROUND_RULES: &[(&str, &str)] = &[
    ("claude", "background_agents_working"),
    ("claude", "background_mcp_task_working"),
];

pub fn is_background_rule(agent: &str, rule_id: &str) -> bool {
    BACKGROUND_RULES
        .iter()
        .any(|(a, r)| *a == agent && *r == rule_id)
}

/// Derives the background fact plus the job count when a current hook reported one.
/// A hook token counts only when `hook_current` (reported in the current generation and
/// after the latest turn start); hook facts are observed at `hook_report_ms`.
/// `validity` is the token's TTL state at digest time: a positive count needs no TTL
/// but must not be expired; a zero counts only while its reporter-supplied TTL is live,
/// because zero is the only background claim that can lead to Complete.
/// Positive evidence vetoes zero: a rule hit yields Active even when the hook said 0.
/// Anything else is Unknown, never Inactive. An unparseable token is ignored.
pub fn derive_background(
    token: Option<&str>,
    validity: Option<TokenValidity>,
    hook_current: bool,
    hook_report_ms: Option<u64>,
    rule_hit: bool,
    now_ms: u64,
) -> (Fact<Background>, Option<u32>) {
    let hook_jobs = if hook_current {
        token.and_then(|t| t.trim().parse::<u32>().ok())
    } else {
        None
    };
    let positive_valid = matches!(validity, Some(TokenValidity::NoTtl | TokenValidity::Live));
    let zero_valid = validity == Some(TokenValidity::Live);
    if let Some(n) = hook_jobs.filter(|n| *n > 0 && positive_valid) {
        return (
            Fact {
                value: Background::Active,
                source: FactSource::Hook,
                observed_at_ms: hook_report_ms,
                confidence: Confidence::High,
            },
            Some(n),
        );
    }
    if rule_hit {
        return (
            Fact {
                value: Background::Active,
                source: FactSource::ManifestRule,
                observed_at_ms: Some(now_ms),
                confidence: Confidence::Medium,
            },
            None,
        );
    }
    if hook_jobs == Some(0) && zero_valid {
        return (
            Fact {
                value: Background::Inactive,
                source: FactSource::Hook,
                observed_at_ms: hook_report_ms,
                confidence: Confidence::High,
            },
            Some(0),
        );
    }
    (
        Fact {
            value: Background::Unknown,
            source: FactSource::Unknown,
            observed_at_ms: None,
            confidence: Confidence::Unknown,
        },
        None,
    )
}

/// Interim fail-closed policy for open finding C18b: a zero report has no reporter
/// work/turn correlation yet, so a delayed zero that arrives after new work started
/// cannot be told apart from a current one. Until a correlation exists, a derived
/// Inactive becomes Unknown unless `zero_correlated`. Cost: hook evidence alone can no
/// longer produce Inactive, so Complete is unreachable from it. Active and Unknown
/// results pass through unchanged.
pub fn require_zero_correlation(
    derived: (Fact<Background>, Option<u32>),
    zero_correlated: bool,
) -> (Fact<Background>, Option<u32>) {
    if zero_correlated || derived.0.value != Background::Inactive {
        return derived;
    }
    (
        Fact {
            value: Background::Unknown,
            source: FactSource::Unknown,
            observed_at_ms: None,
            confidence: Confidence::Unknown,
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;
    use crate::layout::PaneId;
    use crate::supervisor::registry::SupervisorState;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn supervisor_background_rule_table() {
        for (agent, id) in BACKGROUND_RULES {
            assert!(is_background_rule(agent, id));
            assert!(!is_background_rule("not_an_agent_label", id));
        }
        assert!(!is_background_rule(
            BACKGROUND_RULES[0].0,
            "not_a_background_rule"
        ));
    }

    const LIVE: Option<TokenValidity> = Some(TokenValidity::Live);
    const NO_TTL: Option<TokenValidity> = Some(TokenValidity::NoTtl);
    const EXPIRED: Option<TokenValidity> = Some(TokenValidity::Expired);

    #[test]
    fn supervisor_background_derive_table() {
        let (f, n) = derive_background(Some("2"), NO_TTL, true, Some(7), false, 10);
        assert_eq!(n, Some(2));
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.confidence, Confidence::High);
        assert_eq!(f.observed_at_ms, Some(7));

        let (f, n) = derive_background(Some("0"), LIVE, true, Some(7), false, 10);
        assert_eq!(n, Some(0));
        assert_eq!(f.value, Background::Inactive);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.confidence, Confidence::High);
        assert_eq!(f.observed_at_ms, Some(7));

        let (f, n) = derive_background(Some("x"), LIVE, true, Some(7), false, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
        assert_eq!(f.source, FactSource::Unknown);

        let (f, n) = derive_background(Some("1"), LIVE, false, Some(7), false, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);

        let (f, n) = derive_background(None, None, false, None, true, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::ManifestRule);
        assert_eq!(f.confidence, Confidence::Medium);
        assert_eq!(f.observed_at_ms, Some(10));

        let (f, _) = derive_background(Some("x"), LIVE, true, Some(7), true, 10);
        assert_eq!(f.source, FactSource::ManifestRule);

        let (f, n) = derive_background(None, None, false, None, false, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
        assert_eq!(f.confidence, Confidence::Unknown);
        assert_eq!(f.observed_at_ms, None);
    }

    fn assert_unknown((f, n): (Fact<Background>, Option<u32>)) {
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
        assert_eq!(f.source, FactSource::Unknown);
        assert_eq!(f.observed_at_ms, None);
    }

    #[test]
    fn supervisor_background_zero_without_ttl_is_unknown() {
        assert_unknown(derive_background(
            Some("0"),
            NO_TTL,
            true,
            Some(7),
            false,
            10,
        ));
    }

    #[test]
    fn supervisor_background_zero_with_live_ttl_is_inactive_at_report_ms() {
        let (f, n) = derive_background(Some("0"), LIVE, true, Some(7), false, 10);
        assert_eq!(n, Some(0));
        assert_eq!(f.value, Background::Inactive);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.confidence, Confidence::High);
        assert_eq!(f.observed_at_ms, Some(7));
    }

    #[test]
    fn supervisor_background_zero_with_expired_ttl_is_unknown() {
        assert_unknown(derive_background(
            Some("0"),
            EXPIRED,
            true,
            Some(7),
            false,
            10,
        ));
    }

    #[test]
    fn supervisor_background_positive_without_ttl_is_active() {
        let (f, n) = derive_background(Some("4"), NO_TTL, true, Some(7), false, 10);
        assert_eq!(n, Some(4));
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.observed_at_ms, Some(7));
    }

    #[test]
    fn supervisor_background_positive_expired_is_unknown() {
        assert_unknown(derive_background(
            Some("4"),
            EXPIRED,
            true,
            Some(7),
            false,
            10,
        ));
    }

    #[test]
    fn supervisor_background_positive_before_working_is_unknown() {
        let mut s = SupervisorState::new();
        s.note_background_hook(pid(1), 900);
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 1000);
        assert_unknown(derive_from(&s, "3", LIVE, 2000));
        assert_unknown(derive_from(&s, "3", NO_TTL, 2000));
    }

    #[test]
    fn supervisor_background_zero_hook_with_rule_hit_is_active() {
        let (f, n) = derive_background(Some("0"), LIVE, true, Some(7), true, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::ManifestRule);
        assert_eq!(f.confidence, Confidence::Medium);
        assert_eq!(f.observed_at_ms, Some(10));
    }

    #[test]
    fn supervisor_background_positive_hook_observed_at_report_ms() {
        let (f, n) = derive_background(Some("3"), LIVE, true, Some(42), true, 1000);
        assert_eq!(n, Some(3));
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.observed_at_ms, Some(42));
    }

    #[test]
    fn supervisor_background_uncorrelated_zero_is_unknown() {
        let derived = derive_background(Some("0"), LIVE, true, Some(7), false, 10);
        assert_eq!(derived.0.value, Background::Inactive);
        assert_unknown(require_zero_correlation(derived, false));
    }

    #[test]
    fn supervisor_background_correlated_zero_stays_inactive() {
        let derived = derive_background(Some("0"), LIVE, true, Some(7), false, 10);
        let (f, n) = require_zero_correlation(derived, true);
        assert_eq!(n, Some(0));
        assert_eq!(f.value, Background::Inactive);
        assert_eq!(f.observed_at_ms, Some(7));
    }

    #[test]
    fn supervisor_background_correlation_policy_keeps_active_and_unknown() {
        let (f, n) = require_zero_correlation(
            derive_background(Some("2"), NO_TTL, true, Some(7), false, 10),
            false,
        );
        assert_eq!((f.value, n), (Background::Active, Some(2)));
        let (f, _) = require_zero_correlation(
            derive_background(Some("0"), LIVE, true, Some(7), true, 10),
            false,
        );
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::ManifestRule);
        assert_unknown(require_zero_correlation(
            derive_background(Some("0"), EXPIRED, true, Some(7), false, 10),
            false,
        ));
    }

    fn pid(n: u32) -> PaneId {
        PaneId::from_raw(n)
    }

    fn derive_from(
        s: &SupervisorState,
        token: &str,
        validity: Option<TokenValidity>,
        now_ms: u64,
    ) -> (Fact<Background>, Option<u32>) {
        derive_background(
            Some(token),
            validity,
            s.background_hook_current(pid(1)),
            s.background_hook_ms(pid(1)),
            false,
            now_ms,
        )
    }

    #[test]
    fn supervisor_background_zero_hook_then_working_same_ms_is_unknown() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Unknown, AgentState::Idle, 1, 1000);
        s.note_background_hook(pid(1), 1000);
        assert!(s.background_hook_current(pid(1)));
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 2, 1000);
        assert!(!s.background_hook_current(pid(1)));
        let (f, n) = derive_from(&s, "0", LIVE, 1000);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
    }

    #[test]
    fn supervisor_background_working_then_zero_hook_is_current_inactive() {
        let mut s = SupervisorState::new();
        s.on_transition(pid(1), AgentState::Idle, AgentState::Working, 1, 1000);
        s.note_background_hook(pid(1), 1000);
        assert!(s.background_hook_current(pid(1)));
        assert_eq!(s.background_hook_ms(pid(1)), Some(1000));
        let (f, n) = derive_from(&s, "0", LIVE, 5000);
        assert_eq!(n, Some(0));
        assert_eq!(f.value, Background::Inactive);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.observed_at_ms, Some(1000));
    }

    #[test]
    fn supervisor_background_expired_token_reads_unknown() {
        let mut tokens = crate::metadata_tokens::MetadataTokens::default();
        let now = Instant::now();
        let ttl = Duration::from_millis(100);
        let mut patch = HashMap::new();
        patch.insert("background_jobs".to_string(), Some("0".to_string()));
        assert!(tokens.patch(patch, Some(ttl), now));
        let values = tokens.values();
        let token = values.get("background_jobs").map(String::as_str);
        assert_eq!(token, Some("0"));
        // Live before the deadline.
        let (f, _) = derive_background(
            token,
            tokens.validity("background_jobs", now),
            true,
            Some(10),
            false,
            20,
        );
        assert_eq!(f.value, Background::Inactive);
        // At the deadline the sweep has not run, but the read-time check says Expired.
        assert_unknown(derive_background(
            token,
            tokens.validity("background_jobs", now + ttl),
            true,
            Some(10),
            false,
            20,
        ));
        // After the sweep the token is gone.
        assert!(tokens.expire_at(now + ttl + Duration::from_millis(1)));
        let values = tokens.values();
        assert!(!values.contains_key("background_jobs"));
        assert_unknown(derive_background(
            values.get("background_jobs").map(String::as_str),
            tokens.validity("background_jobs", now + ttl),
            true,
            Some(10),
            false,
            20,
        ));
    }
}

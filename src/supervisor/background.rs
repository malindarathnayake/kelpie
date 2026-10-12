//! Pure background-work evidence derivation. No App access and no I/O.

use crate::api::schema::supervisor::{Background, Confidence, Fact, FactSource};

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
/// A hook token counts only when `hook_current` (reported in the current generation).
/// An unparseable token is ignored.
// Wired in by the agents.digest handler (p4).
#[allow(dead_code)]
pub fn derive_background(
    token: Option<&str>,
    hook_current: bool,
    rule_hit: bool,
    now_ms: u64,
) -> (Fact<Background>, Option<u32>) {
    if hook_current {
        if let Some(n) = token.and_then(|t| t.trim().parse::<u32>().ok()) {
            return (
                Fact {
                    value: if n > 0 {
                        Background::Active
                    } else {
                        Background::Inactive
                    },
                    source: FactSource::Hook,
                    observed_at_ms: Some(now_ms),
                    confidence: Confidence::High,
                },
                Some(n),
            );
        }
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

    #[test]
    fn supervisor_background_derive_table() {
        let (f, n) = derive_background(Some("2"), true, false, 10);
        assert_eq!(n, Some(2));
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::Hook);
        assert_eq!(f.confidence, Confidence::High);
        assert_eq!(f.observed_at_ms, Some(10));

        let (f, n) = derive_background(Some("0"), true, false, 10);
        assert_eq!(n, Some(0));
        assert_eq!(f.value, Background::Inactive);
        assert_eq!(f.confidence, Confidence::High);

        let (f, n) = derive_background(Some("x"), true, false, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
        assert_eq!(f.source, FactSource::Unknown);

        let (f, n) = derive_background(Some("1"), false, false, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);

        let (f, n) = derive_background(None, false, true, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Active);
        assert_eq!(f.source, FactSource::ManifestRule);
        assert_eq!(f.confidence, Confidence::Medium);
        assert_eq!(f.observed_at_ms, Some(10));

        let (f, _) = derive_background(Some("x"), true, true, 10);
        assert_eq!(f.source, FactSource::ManifestRule);

        let (f, n) = derive_background(None, false, false, 10);
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
        assert_eq!(f.confidence, Confidence::Unknown);
        assert_eq!(f.observed_at_ms, None);
    }

    #[test]
    fn supervisor_background_expired_token_reads_unknown() {
        let mut tokens = crate::metadata_tokens::MetadataTokens::default();
        let now = Instant::now();
        let ttl = Duration::from_millis(100);
        let mut patch = HashMap::new();
        patch.insert("background_jobs".to_string(), Some("1".to_string()));
        assert!(tokens.patch(patch, Some(ttl), now));
        assert_eq!(
            tokens.values().get("background_jobs").map(String::as_str),
            Some("1")
        );
        assert!(tokens.expire_at(now + ttl + Duration::from_millis(1)));
        let values = tokens.values();
        assert!(!values.contains_key("background_jobs"));
        let (f, n) = derive_background(
            values.get("background_jobs").map(String::as_str),
            true,
            false,
            20,
        );
        assert_eq!(n, None);
        assert_eq!(f.value, Background::Unknown);
    }
}

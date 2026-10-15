//! Built-in per-agent profiles for supervisor writes and draft detection.
//! Static, no config.
#![allow(dead_code)] // callers are wired in later units

use crate::detect::Agent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxShape {
    RuleBox { glyphs: &'static [char] },
    LastGlyphLine { glyph: char },
    BackgroundBand { rgb: (u8, u8, u8), glyph: char },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKey {
    Esc,
    CtrlU,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetEvidenceMode {
    Immediate,
    OnNextPrompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentProfile {
    pub agent: Agent,
    pub box_shape: BoxShape,
    pub submit_enter_delay_ms: u64,
    pub interrupt: ProfileKey,
    pub clear_key: Option<ProfileKey>,
    pub reset_text: Option<&'static str>,
    pub reset_evidence: ResetEvidenceMode,
    pub restores_on_interrupt: bool,
    pub prompt_ack: bool,
    pub supports_answer: bool,
}

static CLAUDE: AgentProfile = AgentProfile {
    agent: Agent::Claude,
    box_shape: BoxShape::RuleBox {
        glyphs: &['\u{276F}', '>'],
    },
    submit_enter_delay_ms: 300,
    interrupt: ProfileKey::Esc,
    // Claude 2.1.299: Ctrl+U cleared only the last line of a two-line restored draft.
    // No verified whole-draft clear key, so clearing remains unsupported.
    clear_key: None,
    reset_text: Some("/clear"),
    reset_evidence: ResetEvidenceMode::Immediate,
    restores_on_interrupt: true,
    prompt_ack: true,
    supports_answer: true,
};

static CODEX: AgentProfile = AgentProfile {
    agent: Agent::Codex,
    box_shape: BoxShape::LastGlyphLine { glyph: '\u{203A}' },
    submit_enter_delay_ms: 300,
    interrupt: ProfileKey::Esc,
    clear_key: Some(ProfileKey::CtrlU),
    reset_text: Some("/clear"),
    reset_evidence: ResetEvidenceMode::OnNextPrompt,
    restores_on_interrupt: false,
    prompt_ack: true,
    supports_answer: false,
};

static CURSOR: AgentProfile = AgentProfile {
    agent: Agent::Cursor,
    box_shape: BoxShape::BackgroundBand {
        rgb: (21, 21, 21),
        glyph: '\u{2192}',
    },
    submit_enter_delay_ms: 300,
    interrupt: ProfileKey::Esc,
    clear_key: Some(ProfileKey::CtrlU),
    reset_text: Some("/clear"),
    reset_evidence: ResetEvidenceMode::OnNextPrompt,
    restores_on_interrupt: true,
    prompt_ack: true,
    supports_answer: false,
};

pub fn profile_for(agent: Agent) -> Option<&'static AgentProfile> {
    match agent {
        Agent::Claude => Some(&CLAUDE),
        Agent::Codex => Some(&CODEX),
        Agent::Cursor => Some(&CURSOR),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_for_is_some_exactly_for_claude_codex_cursor() {
        let mut some = 0;
        for a in Agent::ALL {
            let expected = matches!(a, Agent::Claude | Agent::Codex | Agent::Cursor);
            let got = profile_for(a);
            assert_eq!(got.is_some(), expected, "{a:?}");
            if let Some(p) = got {
                some += 1;
                assert_eq!(p.agent, a);
            }
        }
        assert_eq!(some, 3);
    }

    #[test]
    fn claude_profile_values() {
        let expected = AgentProfile {
            agent: Agent::Claude,
            box_shape: BoxShape::RuleBox {
                glyphs: &['\u{276F}', '>'],
            },
            submit_enter_delay_ms: 300,
            interrupt: ProfileKey::Esc,
            clear_key: None,
            reset_text: Some("/clear"),
            reset_evidence: ResetEvidenceMode::Immediate,
            restores_on_interrupt: true,
            prompt_ack: true,
            supports_answer: true,
        };
        assert_eq!(profile_for(Agent::Claude), Some(&expected));
    }

    #[test]
    fn codex_profile_values() {
        let expected = AgentProfile {
            agent: Agent::Codex,
            box_shape: BoxShape::LastGlyphLine { glyph: '\u{203A}' },
            submit_enter_delay_ms: 300,
            interrupt: ProfileKey::Esc,
            clear_key: Some(ProfileKey::CtrlU),
            reset_text: Some("/clear"),
            reset_evidence: ResetEvidenceMode::OnNextPrompt,
            restores_on_interrupt: false,
            prompt_ack: true,
            supports_answer: false,
        };
        assert_eq!(profile_for(Agent::Codex), Some(&expected));
    }

    #[test]
    fn cursor_profile_values() {
        let expected = AgentProfile {
            agent: Agent::Cursor,
            box_shape: BoxShape::BackgroundBand {
                rgb: (21, 21, 21),
                glyph: '\u{2192}',
            },
            submit_enter_delay_ms: 300,
            interrupt: ProfileKey::Esc,
            clear_key: Some(ProfileKey::CtrlU),
            reset_text: Some("/clear"),
            reset_evidence: ResetEvidenceMode::OnNextPrompt,
            restores_on_interrupt: true,
            prompt_ack: true,
            supports_answer: false,
        };
        assert_eq!(profile_for(Agent::Cursor), Some(&expected));
    }
}

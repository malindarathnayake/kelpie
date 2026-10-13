//! Pure owner-input draft derivation (S1). Claude only: reads the prompt box at the bottom of
//! the recent screen rows. Anything not positively recognised is `Unknown`.

use crate::api::schema::supervisor::DraftState;
use crate::supervisor::question::{is_rule_row, MAX_BOX_GAP, WINDOW_LINES};

/// Agent label whose prompt box layout this derivation knows.
pub const DRAFT_AGENT: &str = "claude";

/// Derives the draft state from the last screen rows.
///
/// Clean: a prompt box (rule / input row / rule, rules at most `MAX_BOX_GAP` rows apart)
/// sits near the bottom, its first non-empty row is exactly a prompt glyph (`❯` or `>`)
/// followed only by whitespace, no other row in the box has text, and no owner input is
/// at or after the last PTY output (equal milliseconds do not prove the echo was rendered,
/// so the screen may not show the input yet; conservative).
/// Dirty: the box's input row has text after the glyph, or another box row has text.
/// Unknown: anything else, including other agents, and always while `latched` (any owner
/// input reached this PTY; see `PaneActivity::draft_latched`). Neither timestamps, a quiet
/// window, a Working edge nor a generation bump clears the latch: output stamps are not
/// correlated to the input echo, so they cannot prove the box shows the owner's input.
///
/// Perf (tier hot): O(24 rows), no regex, one Vec of row slices, no locks, no I/O; called
/// once per Claude pane per digest page and once per accepted `agent.submit` check.
pub fn derive_draft(
    text: &str,
    agent: Option<&str>,
    latched: bool,
    last_owner_input_ms: Option<u64>,
    last_output_ms: Option<u64>,
) -> DraftState {
    if agent != Some(DRAFT_AGENT) || latched {
        return DraftState::Unknown;
    }
    if let Some(input) = last_owner_input_ms {
        if last_output_ms.is_none_or(|output| input >= output) {
            return DraftState::Unknown;
        }
    }
    let all: Vec<&str> = text.lines().collect();
    let window = &all[all.len().saturating_sub(WINDOW_LINES)..];
    let Some(bottom) = window.iter().rposition(|r| is_rule_row(r)) else {
        return DraftState::Unknown;
    };
    let below = window[bottom + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    if below > MAX_BOX_GAP {
        return DraftState::Unknown;
    }
    let lo = bottom.saturating_sub(MAX_BOX_GAP + 1);
    let Some(top) = (lo..bottom).rev().find(|&i| is_rule_row(window[i])) else {
        return DraftState::Unknown;
    };
    let mut rows = window[top + 1..bottom]
        .iter()
        .filter(|r| !r.trim().is_empty());
    let Some(input) = rows.next() else {
        return DraftState::Unknown;
    };
    let input = input.trim_start();
    let Some(rest) = input.strip_prefix('❯').or_else(|| input.strip_prefix('>')) else {
        return DraftState::Unknown;
    };
    if !rest.trim().is_empty() || rows.next().is_some() {
        DraftState::Dirty
    } else {
        DraftState::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "────────────────────────────────────────";

    fn screen(input: &str) -> String {
        format!("Done. Tests pass.\n\n{RULE}\n{input}\n{RULE}\n  ? for shortcuts\n")
    }

    #[test]
    fn draft_empty_box_is_clean() {
        assert_eq!(
            derive_draft(&screen("❯ "), Some("claude"), false, None, Some(5)),
            DraftState::Clean
        );
        assert_eq!(
            derive_draft(&screen(">\u{a0}"), Some("claude"), false, Some(4), Some(5)),
            DraftState::Clean
        );
    }

    #[test]
    fn draft_equal_input_and_output_ms_is_unknown() {
        // Empty prompt rendered at T, owner typed at the same T before the echo: the screen
        // is stale, so equal timestamps must not read Clean.
        assert_eq!(
            derive_draft(&screen("❯ "), Some("claude"), false, Some(5), Some(5)),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_typed_text_is_dirty() {
        assert_eq!(
            derive_draft(
                &screen("❯ fix the tests"),
                Some("claude"),
                false,
                None,
                Some(5)
            ),
            DraftState::Dirty
        );
        let multiline = format!("{RULE}\n❯ \n  second line\n{RULE}\n");
        assert_eq!(
            derive_draft(&multiline, Some("claude"), false, None, None),
            DraftState::Dirty
        );
    }

    #[test]
    fn draft_without_box_is_unknown() {
        assert_eq!(
            derive_draft("just output\n❯ \n", Some("claude"), false, None, None),
            DraftState::Unknown
        );
        let lone_rule = format!("text\n{RULE}\n❯ \n");
        assert_eq!(
            derive_draft(&lone_rule, Some("claude"), false, None, None),
            DraftState::Unknown
        );
        let no_glyph = format!("{RULE}\n  Do you want to proceed?\n{RULE}\n");
        assert_eq!(
            derive_draft(&no_glyph, Some("claude"), false, None, None),
            DraftState::Unknown
        );
        let far = format!("{RULE}\n❯ \n{RULE}\n{}", "footer\n".repeat(MAX_BOX_GAP + 1));
        assert_eq!(
            derive_draft(&far, Some("claude"), false, None, None),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_non_claude_is_unknown() {
        for agent in [None, Some("codex"), Some("pi")] {
            assert_eq!(
                derive_draft(&screen("❯ "), agent, false, None, None),
                DraftState::Unknown
            );
        }
    }

    #[test]
    fn draft_owner_input_after_output_is_unknown() {
        assert_eq!(
            derive_draft(&screen("❯ "), Some("claude"), false, Some(6), Some(5)),
            DraftState::Unknown
        );
        assert_eq!(
            derive_draft(&screen("❯ "), Some("claude"), false, Some(6), None),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_latched_is_unknown_regardless_of_screen_and_timestamps() {
        // Owner input at T=1000, unrelated output at T+2; the derivation takes no clock, so
        // T+300 or T+10_000 cannot change the answer either.
        let t = 1_000;
        for (input, output) in [
            (Some(t), Some(t + 2)),
            (Some(t), Some(t + 10_000)),
            (None, None),
        ] {
            for row in ["❯ ", "❯ typed"] {
                assert_eq!(
                    derive_draft(&screen(row), Some("claude"), true, input, output),
                    DraftState::Unknown
                );
            }
        }
        // Unlatched, the same evidence reads from the screen.
        assert_eq!(
            derive_draft(&screen("❯ "), Some("claude"), false, Some(t), Some(t + 2)),
            DraftState::Clean
        );
    }
}

//! Pure owner-input draft derivation (S1). Claude only: reads the prompt box at the bottom of
//! the recent screen rows. Anything not positively recognised is `Unknown`.

use crate::api::schema::supervisor::DraftState;
use crate::supervisor::question::{is_rule_row, MAX_BOX_GAP, WINDOW_LINES};

/// Agent label whose prompt box layout this derivation knows.
pub const DRAFT_AGENT: &str = "claude";

/// One screen row: plain text plus the faint (SGR 2) attribute of each char.
struct StyledRow {
    chars: Vec<(char, bool)>,
    text: String,
}

/// Splits formatter VT output (`read_ansi_screen`) into rows, tracking only the faint
/// attribute. SGR state carries across rows like a terminal. Other CSI, OSC and two-byte
/// escapes are skipped; C0 controls other than newline are dropped.
fn styled_rows(ansi: &str) -> Vec<StyledRow> {
    let mut rows = Vec::new();
    let mut chars = Vec::new();
    let mut faint = false;
    let mut it = ansi.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\n' => rows.push(std::mem::take(&mut chars)),
            '\u{1b}' => match it.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut fin = None;
                    for p in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&p) {
                            fin = Some(p);
                            break;
                        }
                        params.push(p);
                    }
                    if fin == Some('m') {
                        faint = apply_sgr_faint(&params, faint);
                    }
                }
                Some(']') => {
                    while let Some(p) = it.next() {
                        if p == '\u{7}' || (p == '\u{1b}' && it.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            c if c.is_control() => {}
            c => chars.push((c, faint)),
        }
    }
    if !chars.is_empty() {
        rows.push(chars);
    }
    rows.into_iter()
        .map(|chars| StyledRow {
            text: chars.iter().map(|&(c, _)| c).collect(),
            chars,
        })
        .collect()
}

/// Applies one SGR parameter list to the faint flag. Semicolon `38`/`48`/`58` colour
/// arguments are skipped so a colour component equal to 2 or 22 is not read as an
/// attribute; colon sub-parameters (`38:2::r:g:b`) stay inside their own part.
fn apply_sgr_faint(params: &str, mut faint: bool) -> bool {
    let mut parts = params.split(';');
    while let Some(part) = parts.next() {
        let code = part.split(':').next().unwrap_or_default();
        match code {
            "" | "0" | "22" => faint = false,
            "2" => faint = true,
            "38" | "48" | "58" if !part.contains(':') => match parts.next() {
                Some("5") => {
                    parts.next();
                }
                Some("2") => {
                    parts.next();
                    parts.next();
                    parts.next();
                }
                _ => {}
            },
            _ => {}
        }
    }
    faint
}

/// Derives the draft state from the last screen rows, given as formatter VT output with
/// cell styles. `None` means styles are unavailable: always `Unknown`, never a guess.
///
/// Clean: a prompt box (rule / input row / rule, rules at most `MAX_BOX_GAP` rows apart)
/// sits near the bottom, its first non-empty row is a prompt glyph (`❯` or `>`) followed
/// only by whitespace or by Claude's placeholder (every visible char after the glyph is
/// faint), no other row in the box has text, and no owner input is at or after the last
/// PTY output (equal milliseconds do not prove the echo was rendered, so the screen may
/// not show the input yet; conservative).
/// Dirty: the input row has a non-faint visible char after the glyph, or another box row
/// has text.
/// Unknown: anything else, including other agents, and always while `latched` (any owner
/// input reached this PTY; see `PaneActivity::draft_latched`). Neither timestamps, a quiet
/// window, a Working edge nor a generation bump clears the latch: output stamps are not
/// correlated to the input echo, so they cannot prove the box shows the owner's input.
///
/// Perf (tier hot): one pass over the VT text of 24 rows, no regex, one Vec per row, no
/// locks, no I/O; called once per Claude pane per digest page and once per accepted
/// `agent.submit` check.
pub fn derive_draft(
    ansi: Option<&str>,
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
    let Some(ansi) = ansi else {
        return DraftState::Unknown;
    };
    let all = styled_rows(ansi);
    let window = &all[all.len().saturating_sub(WINDOW_LINES)..];
    let Some(bottom) = window.iter().rposition(|r| is_rule_row(&r.text)) else {
        return DraftState::Unknown;
    };
    let below = window[bottom + 1..]
        .iter()
        .filter(|r| !r.text.trim().is_empty())
        .count();
    if below > MAX_BOX_GAP {
        return DraftState::Unknown;
    }
    let lo = bottom.saturating_sub(MAX_BOX_GAP + 1);
    let Some(top) = (lo..bottom).rev().find(|&i| is_rule_row(&window[i].text)) else {
        return DraftState::Unknown;
    };
    let mut rows = window[top + 1..bottom]
        .iter()
        .filter(|r| !r.text.trim().is_empty());
    let Some(input) = rows.next() else {
        return DraftState::Unknown;
    };
    let mut cells = input.chars.iter().skip_while(|(c, _)| c.is_whitespace());
    if !matches!(cells.next(), Some(('❯' | '>', _))) {
        return DraftState::Unknown;
    }
    // Only faint visible chars (or none) after the glyph: placeholder, so the box is empty.
    let typed = cells.any(|&(c, faint)| !c.is_whitespace() && !faint);
    if typed || rows.next().is_some() {
        DraftState::Dirty
    } else {
        DraftState::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "────────────────────────────────────────";
    /// Real Claude, empty box: `❯` + NBSP + faint `Try "write a test for <filepath>"`.
    const PLACEHOLDER: &str = include_str!("testdata/claude_empty_box_placeholder.ansi");
    /// Real Claude, typed box: `❯` + NBSP + normal-intensity `draft text here`.
    const TYPED: &str = include_str!("testdata/claude_typed_box.ansi");
    const PLACEHOLDER_SGR: &str = "\u{1b}[2mTry \"write a test for <filepath>\"";

    fn screen(input: &str) -> String {
        format!("Done. Tests pass.\n\n{RULE}\n{input}\n{RULE}\n  ? for shortcuts\n")
    }

    fn claude(ansi: &str) -> DraftState {
        derive_draft(Some(ansi), Some("claude"), false, None, Some(5))
    }

    #[test]
    fn draft_empty_box_is_clean() {
        assert_eq!(
            derive_draft(Some(&screen("❯ ")), Some("claude"), false, None, Some(5)),
            DraftState::Clean
        );
        assert_eq!(
            derive_draft(
                Some(&screen(">\u{a0}")),
                Some("claude"),
                false,
                Some(4),
                Some(5)
            ),
            DraftState::Clean
        );
    }

    #[test]
    fn draft_real_placeholder_capture_is_clean() {
        assert!(PLACEHOLDER.contains(PLACEHOLDER_SGR));
        assert_eq!(claude(PLACEHOLDER), DraftState::Clean);
    }

    #[test]
    fn draft_real_typed_capture_is_dirty() {
        assert_eq!(claude(TYPED), DraftState::Dirty);
    }

    #[test]
    fn draft_real_captures_have_a_box_of_full_width_rules() {
        for capture in [PLACEHOLDER, TYPED] {
            let rows = styled_rows(capture);
            let rules: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| is_rule_row(&r.text))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(rules.len(), 2);
            assert_eq!(rules[1] - rules[0], 2);
            assert!(rows[rules[0] + 1].text.starts_with("❯\u{a0}"));
            // No rule chars are marked faint; the rule colour is RGB 136;136;136.
            assert!(rows[rules[0]].chars.iter().all(|&(_, faint)| !faint));
        }
    }

    #[test]
    fn draft_real_placeholder_with_latch_is_unknown() {
        assert_eq!(
            derive_draft(Some(PLACEHOLDER), Some("claude"), true, None, Some(5)),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_faint_placeholder_plus_normal_char_is_dirty() {
        let mixed = PLACEHOLDER.replacen(PLACEHOLDER_SGR, &format!("x{PLACEHOLDER_SGR}"), 1);
        assert_ne!(mixed, PLACEHOLDER);
        assert_eq!(claude(&mixed), DraftState::Dirty);
        let synthetic = screen("❯\u{a0}\u{1b}[2mTry this\u{1b}[22m!");
        assert_eq!(claude(&synthetic), DraftState::Dirty);
        let faint_only = screen("❯\u{a0}\u{1b}[0;2mTry this\u{1b}[0m");
        assert_eq!(claude(&faint_only), DraftState::Clean);
    }

    #[test]
    fn draft_missing_style_info_is_unknown() {
        assert_eq!(
            derive_draft(None, Some("claude"), false, None, Some(5)),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_sgr_colour_arguments_are_not_attributes() {
        // Colour components 2 and 22 must not toggle faint.
        assert!(!apply_sgr_faint("38;2;2;2;2", false));
        assert!(apply_sgr_faint("48;5;22", true));
        assert!(apply_sgr_faint("38:2::22:2:2;2", false));
        assert!(!apply_sgr_faint("2;0", false));
        assert!(!apply_sgr_faint("", true));
    }

    #[test]
    fn draft_osc_sequences_are_skipped() {
        let osc = screen("❯\u{a0}\u{1b}]8;;https://x\u{7}\u{1b}[2mhint\u{1b}]8;;\u{1b}\\");
        assert_eq!(claude(&osc), DraftState::Clean);
    }

    #[test]
    fn draft_placeholder_is_not_a_prose_question() {
        let plain: Vec<String> = styled_rows(PLACEHOLDER)
            .into_iter()
            .map(|r| r.text)
            .collect();
        let plain = plain.join("\n");
        let content = crate::supervisor::question::content_rows(&plain);
        assert!(content.iter().all(|r| !r.contains("write a test")));
        assert!(crate::supervisor::question::detect_prose_question(&content).is_none());
    }

    #[test]
    fn draft_equal_input_and_output_ms_is_unknown() {
        // Empty prompt rendered at T, owner typed at the same T before the echo: the screen
        // is stale, so equal timestamps must not read Clean.
        assert_eq!(
            derive_draft(Some(&screen("❯ ")), Some("claude"), false, Some(5), Some(5)),
            DraftState::Unknown
        );
        assert_eq!(
            derive_draft(Some(PLACEHOLDER), Some("claude"), false, Some(5), Some(5)),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_typed_text_is_dirty() {
        assert_eq!(
            derive_draft(
                Some(&screen("❯ fix the tests")),
                Some("claude"),
                false,
                None,
                Some(5)
            ),
            DraftState::Dirty
        );
        let multiline = format!("{RULE}\n❯ \n  second line\n{RULE}\n");
        assert_eq!(
            derive_draft(Some(&multiline), Some("claude"), false, None, None),
            DraftState::Dirty
        );
    }

    #[test]
    fn draft_without_box_is_unknown() {
        assert_eq!(
            derive_draft(Some("just output\n❯ \n"), Some("claude"), false, None, None),
            DraftState::Unknown
        );
        let lone_rule = format!("text\n{RULE}\n❯ \n");
        assert_eq!(
            derive_draft(Some(&lone_rule), Some("claude"), false, None, None),
            DraftState::Unknown
        );
        let no_glyph = format!("{RULE}\n  Do you want to proceed?\n{RULE}\n");
        assert_eq!(
            derive_draft(Some(&no_glyph), Some("claude"), false, None, None),
            DraftState::Unknown
        );
        let far = format!("{RULE}\n❯ \n{RULE}\n{}", "footer\n".repeat(MAX_BOX_GAP + 1));
        assert_eq!(
            derive_draft(Some(&far), Some("claude"), false, None, None),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_non_claude_is_unknown() {
        for agent in [None, Some("codex"), Some("pi")] {
            assert_eq!(
                derive_draft(Some(&screen("❯ ")), agent, false, None, None),
                DraftState::Unknown
            );
            assert_eq!(
                derive_draft(Some(PLACEHOLDER), agent, false, None, None),
                DraftState::Unknown
            );
        }
    }

    #[test]
    fn draft_owner_input_after_output_is_unknown() {
        assert_eq!(
            derive_draft(Some(&screen("❯ ")), Some("claude"), false, Some(6), Some(5)),
            DraftState::Unknown
        );
        assert_eq!(
            derive_draft(Some(&screen("❯ ")), Some("claude"), false, Some(6), None),
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
                    derive_draft(Some(&screen(row)), Some("claude"), true, input, output),
                    DraftState::Unknown
                );
            }
        }
        // Unlatched, the same evidence reads from the screen.
        assert_eq!(
            derive_draft(
                Some(&screen("❯ ")),
                Some("claude"),
                false,
                Some(t),
                Some(t + 2)
            ),
            DraftState::Clean
        );
    }
}

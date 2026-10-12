//! Pure prose-question heuristic over terminal text. Chrome-aware: ignores the agent input box
//! and footer, so only agent output above them is considered.
// A later unit wires this into the Idle-transition path.
#![allow(dead_code)]

use sha2::{Digest, Sha256};

const WINDOW_LINES: usize = 24;
const MAX_CONTENT_ROWS: usize = 12;
const MAX_EXCERPT_CHARS: usize = 200;
const MAX_BOX_GAP: usize = 6;

fn is_rule_row(row: &str) -> bool {
    let is_rule = |c: char| matches!(c, '─' | '━' | '═' | '-');
    let t = row.trim();
    let total = t.chars().count();
    if total < 10 {
        return false;
    }
    // Plain rule: every char is a rule char.
    if t.chars().all(is_rule) {
        return true;
    }
    // Titled rule (e.g. Claude Code's `──── Title ─`): rule chars at both ends, at least
    // 10 rule chars overall and at least half of the row.
    let rules = t.chars().filter(|&c| is_rule(c)).count();
    t.chars().next().is_some_and(is_rule)
        && t.chars().next_back().is_some_and(is_rule)
        && rules >= 10
        && rules * 2 >= total
}

fn is_chrome_tail_row(row: &str) -> bool {
    let t = row.trim();
    if t.is_empty() || matches!(t, "❯" | ">" | "›" | "$") {
        return true;
    }
    let starts_status = t
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '✢' | '✳' | '✶' | '✻' | '✽' | '·' | '●' | '⏺'));
    starts_status && (t.contains('…') || t.contains("esc to interrupt"))
}

/// Perf (tier hot): O(24 rows) scan, no regex, one Vec of row slices, no locks, no I/O;
/// runs only on Idle transitions and one digest-time rescan, never on the PTY byte path.
pub fn content_rows(text: &str) -> Vec<&str> {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(WINDOW_LINES);
    let window = &all[start..];

    let mut end = window.len();
    if let Some(l) = window.iter().rposition(|r| is_rule_row(r)) {
        let lo = l.saturating_sub(MAX_BOX_GAP + 1);
        if let Some(p) = (lo..l).rev().find(|&i| is_rule_row(window[i])) {
            end = p;
        } else if window[l + 1..].iter().all(|r| is_chrome_tail_row(r)) {
            // A lone rule only cuts when everything below it is chrome (or nothing is);
            // otherwise it is an ordinary markdown horizontal rule inside agent output.
            end = l;
        }
    }
    let mut rows = &window[..end];
    while let Some(last) = rows.last() {
        if is_chrome_tail_row(last) {
            rows = &rows[..rows.len() - 1];
        } else {
            break;
        }
    }
    let non_empty: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|r| !r.trim().is_empty())
        .collect();
    let skip = non_empty.len().saturating_sub(MAX_CONTENT_ROWS);
    non_empty[skip..].to_vec()
}

pub fn detect_prose_question(content: &[&str]) -> Option<String> {
    let row = content.last()?;
    let t = row.trim();
    let body = t.strip_suffix('?').or_else(|| t.strip_suffix('？'))?;
    if body.chars().filter(|c| !c.is_whitespace()).count() >= 2 {
        Some(sanitize_excerpt(t))
    } else {
        None
    }
}

/// Skip CSI parameter/intermediate bytes through the final byte (0x40-0x7E); unterminated drops
/// to end of input.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for n in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&n) {
            break;
        }
    }
}

/// Skip an OSC/DCS/SOS/PM/APC payload through BEL, `ESC \` or C1 ST; unterminated drops to end.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(n) = chars.next() {
        match n {
            '\x07' | '\u{9c}' => break,
            '\x1b' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
            _ => {}
        }
    }
}

/// Strip escape sequences, control, bidi and zero-width characters, cap at 200 chars. [CWE-150]
pub fn sanitize_excerpt(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_EXCERPT_CHARS * 4));
    let mut chars = s.chars().peekable();
    let mut count = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => skip_csi(&mut chars),
                Some(']' | 'P' | 'X' | '^' | '_') => skip_string(&mut chars),
                _ => {}
            },
            '\u{9b}' => skip_csi(&mut chars),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars),
            _ => {}
        }
        if matches!(
            c,
            '\x1b' | '\u{9b}' | '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}'
        ) {
            continue;
        }
        let drop = matches!(c,
            '\u{00}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
            | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            | '\u{200b}'..='\u{200f}' | '\u{feff}'
            | '\u{061c}' | '\u{2060}' | '\u{2028}' | '\u{2029}');
        if drop {
            continue;
        }
        if count >= MAX_EXCERPT_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

pub fn normalize(s: &str) -> String {
    let clean = sanitize_excerpt(s);
    let mut out = String::with_capacity(clean.len());
    for word in clean.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.to_lowercase()
}

pub fn question_id(pane_raw: u32, generation: u64, kind_tag: &str, normalized: &str) -> String {
    let mut h = Sha256::new();
    h.update(pane_raw.to_le_bytes());
    h.update(generation.to_le_bytes());
    h.update(kind_tag.as_bytes());
    h.update([0u8]);
    h.update(normalized.as_bytes());
    let digest = h.finalize();
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("testdata/claude_idle_question.txt");
    const QLINE: &str = "  Should I start the brief?";

    fn detect(text: &str) -> Option<String> {
        detect_prose_question(&content_rows(text))
    }

    #[test]
    fn question_real_layout_detects_question_above_input_box_and_footer() {
        assert_eq!(
            detect(FIXTURE),
            Some("Should I start the brief?".to_string())
        );
    }

    #[test]
    fn question_real_layout_without_question_is_none() {
        let t = FIXTURE.replace(QLINE, "  Done. All tests pass.");
        assert_eq!(detect(&t), None);
    }

    #[test]
    fn question_owner_typed_question_in_input_box_is_ignored() {
        let t = FIXTURE
            .replace(QLINE, "  Done.")
            .replace("❯ \n", "❯ yes?\n");
        assert!(t.contains("❯ yes?"));
        assert_eq!(detect(&t), None);
    }

    const TITLED: &str = include_str!("testdata/claude_idle_question_titled.txt");

    #[test]
    fn question_titled_border_detects_question() {
        assert_eq!(
            detect(TITLED),
            Some("Should I start the brief?".to_string())
        );
    }

    #[test]
    fn question_titled_border_owner_draft_is_ignored() {
        let t = TITLED.replace(QLINE, "  Done.").replace("❯ \n", "❯ yes?\n");
        assert!(t.contains("❯ yes?"));
        assert_eq!(detect(&t), None);
    }

    #[test]
    fn question_is_rule_row_accepts_titled_and_rejects_prose() {
        let titled = format!("{} Foreman - kelpie ─", "─".repeat(40));
        assert!(is_rule_row(&titled));
        assert!(is_rule_row(&"─".repeat(40)));
        assert!(!is_rule_row("Should I start the brief?"));
        assert!(!is_rule_row("-- note --"));
    }

    #[test]
    fn question_one_line_synthetic_detected() {
        assert!(detect("Should I start the brief?\n").is_some());
    }

    #[test]
    fn question_id_is_stable_and_generation_scoped() {
        let a = question_id(7, 1, "prose", "should i start the brief?");
        let b = question_id(7, 1, "prose", "should i start the brief?");
        let c = question_id(7, 2, "prose", "should i start the brief?");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, c);
    }

    #[test]
    fn question_mid_line_question_mark_not_detected() {
        assert_eq!(detect("Is it? yes it is."), None);
        assert_eq!(detect("PS C:\\> "), None);
        assert_eq!(detect(" ?"), None);
    }

    #[test]
    fn question_sanitize_strips_escape_bidi_zero_width() {
        let s = "a\x1b[31mb\x1b]8;;http://x\x07c\u{202E}d\u{200B}e\x1b\\f";
        assert_eq!(sanitize_excerpt(s), "abcdef");
    }

    #[test]
    fn question_sanitize_osc_terminators() {
        assert_eq!(sanitize_excerpt("\x1b]0;t\x1b\\ok"), "ok");
        assert_eq!(sanitize_excerpt("\x1b]0;t\x07ok"), "ok");
        assert_eq!(sanitize_excerpt("\u{9d}0;t\u{9c}ok"), "ok");
        assert_eq!(sanitize_excerpt("\x1b]52;c;aGVsbG8=\x07ok"), "ok");
    }

    #[test]
    fn question_sanitize_c1_csi_and_strings() {
        assert_eq!(sanitize_excerpt("\u{9b}31mred"), "red");
        assert_eq!(sanitize_excerpt("\x1bPpayload\x1b\\ok"), "ok");
        assert_eq!(sanitize_excerpt("\x1b_apc\x1b\\ok"), "ok");
        assert_eq!(sanitize_excerpt("\x1b^pm\x1b\\ok"), "ok");
        assert_eq!(sanitize_excerpt("\x1bXsos\x1b\\ok"), "ok");
        assert_eq!(sanitize_excerpt("\u{90}dcs\u{9c}ok"), "ok");
    }

    #[test]
    fn question_sanitize_unterminated_sequences_drop_to_end() {
        assert_eq!(sanitize_excerpt("ok\x1b]8;;http://x"), "ok");
        assert_eq!(sanitize_excerpt("ok\x1b[31"), "ok");
    }

    #[test]
    fn question_sanitize_drops_extra_invisibles() {
        for c in [
            '\u{200e}', '\u{200f}', '\u{061c}', '\u{2060}', '\u{2028}', '\u{2029}',
        ] {
            let s = format!("a{c}b");
            assert_eq!(sanitize_excerpt(&s), "ab", "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn question_sanitize_cap_mixed_width() {
        let s = format!("{}{}", "a".repeat(150), "é".repeat(100));
        let out = sanitize_excerpt(&s);
        assert_eq!(out.chars().count(), 200);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.ends_with('é'));
    }

    #[test]
    fn question_single_markdown_rule_does_not_hide_trailing_question() {
        assert_eq!(
            detect("Notes\n----------\nShould I start the brief?\n"),
            Some("Should I start the brief?".to_string())
        );
    }

    #[test]
    fn question_single_rule_above_prompt_only_still_cuts() {
        assert_eq!(
            detect("Should I start the brief?\n────────────\n❯\n"),
            Some("Should I start the brief?".to_string())
        );
    }

    #[test]
    fn question_sanitize_caps_at_200_chars() {
        let s = "é".repeat(300);
        assert_eq!(sanitize_excerpt(&s).chars().count(), 200);
    }
}

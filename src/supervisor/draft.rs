//! Pure owner-input draft derivation (S1). Reads the input box at the bottom of the recent
//! screen rows as an agent profile describes it (rule box, last glyph line or background
//! band). Anything not positively recognised is `Unknown`.

use crate::api::schema::supervisor::DraftState;
use crate::supervisor::profile::{AgentProfile, BoxShape};
use crate::supervisor::question::{is_rule_row, MAX_BOX_GAP, WINDOW_LINES};

/// Background colour of a cell as set by SGR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bg {
    Rgb(u8, u8, u8),
    Indexed(u8),
}

/// One screen cell: the char plus the SGR attributes the draft rules read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    faint: bool,
    reverse: bool,
    bg: Option<Bg>,
}

/// The current SGR pen: the attributes applied to the next printed char.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Sgr {
    faint: bool,
    reverse: bool,
    bg: Option<Bg>,
}

/// One screen row: its cells plus the plain text of those cells.
struct StyledRow {
    chars: Vec<Cell>,
    text: String,
}

/// Splits formatter VT output (`read_ansi_screen`) into rows, tracking the faint, reverse
/// and background attributes. SGR state carries across rows like a terminal. Other CSI, OSC
/// and two-byte escapes are skipped; C0 controls other than newline are dropped.
fn styled_rows(ansi: &str) -> Vec<StyledRow> {
    let mut rows = Vec::new();
    let mut chars = Vec::new();
    let mut pen = Sgr::default();
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
                        pen = apply_sgr(&params, pen);
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
            ch => chars.push(Cell {
                ch,
                faint: pen.faint,
                reverse: pen.reverse,
                bg: pen.bg,
            }),
        }
    }
    if !chars.is_empty() {
        rows.push(chars);
    }
    rows.into_iter()
        .map(|chars| StyledRow {
            text: chars.iter().map(|c| c.ch).collect(),
            chars,
        })
        .collect()
}

/// Applies one SGR parameter list to the pen: faint (2/22), reverse (7/27) and background
/// (40-47, 100-107, semicolon `48;5;n` and `48;2;r;g;b`, 49); 0 or an empty part resets all.
/// Semicolon `38`/`48`/`58` colour arguments are always consumed so a colour component equal
/// to 2 or 22 is not read as an attribute; colon sub-parameters (`38:2::r:g:b`) stay inside
/// their own part and are not interpreted. Foreground and underline colours are not tracked.
fn apply_sgr(params: &str, mut state: Sgr) -> Sgr {
    let mut parts = params.split(';');
    while let Some(part) = parts.next() {
        let code = part.split(':').next().unwrap_or_default();
        match code {
            "" | "0" => state = Sgr::default(),
            "2" => state.faint = true,
            "22" => state.faint = false,
            "7" => state.reverse = true,
            "27" => state.reverse = false,
            "49" => state.bg = None,
            _ => {
                if let Ok(n) = code.parse::<u8>() {
                    match n {
                        40..=47 => state.bg = Some(Bg::Indexed(n - 40)),
                        100..=107 => state.bg = Some(Bg::Indexed(n - 100 + 8)),
                        _ => {}
                    }
                }
            }
        }
        if !part.contains(':') {
            match code {
                "38" | "48" | "58" => {
                    let bg = match parts.next() {
                        Some("5") => parts.next().and_then(|n| n.parse().ok()).map(Bg::Indexed),
                        Some("2") => {
                            let r = parts.next().and_then(|n| n.parse().ok());
                            let g = parts.next().and_then(|n| n.parse().ok());
                            let b = parts.next().and_then(|n| n.parse().ok());
                            match (r, g, b) {
                                (Some(r), Some(g), Some(b)) => Some(Bg::Rgb(r, g, b)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if code == "48" && bg.is_some() {
                        state.bg = bg;
                    }
                }
                _ => {}
            }
        }
    }
    state
}

/// Derives the draft state from the last screen rows, given as formatter VT output with
/// cell styles, reading the input box the agent `profile` describes. `None` styles or a
/// `None` profile mean the box cannot be read: always `Unknown`, never a guess.
///
/// Three box shapes (`BoxShape`):
/// - `RuleBox` (Claude): a prompt box (rule / input row / rule, rules at most `MAX_BOX_GAP`
///   rows apart) sits near the bottom, its first non-empty row starts with one of the
///   profile's glyphs. Clean: only whitespace or faint placeholder chars follow the glyph
///   and no other box row has text. Dirty: a non-faint visible char follows the glyph, or
///   another box row has text.
/// - `LastGlyphLine` (Codex): the input row is the bottom-most row whose first non-blank
///   char is the glyph, with at most `MAX_BOX_GAP` text rows below it.
/// - `BackgroundBand` (Cursor): the band is the bottom-most contiguous run of rows with a
///   cell on the profile's background colour; the glyph row is the band row starting with
///   the glyph. Another band row with a non-faint visible char is Dirty; with only faint
///   ones it is Unknown.
///
/// Glyph row rule (Codex and Cursor), over cells after the glyph, ignoring reverse-video
/// cells (the cursor) and whitespace: a non-faint visible cell is Dirty; else a faint
/// visible cell (the placeholder) is Clean; else nothing visible is Unknown, so a missing
/// placeholder never reads Clean. For a band, Dirty wins over Unknown, and Clean needs the
/// glyph row Clean and no visible char on other band rows.
///
/// Any Clean also requires that no owner input is at or after the last PTY output (equal
/// milliseconds do not prove the echo was rendered, so the screen may not show the input
/// yet; conservative).
/// Unknown: anything else, including an agent without a profile, and always while `latched`
/// (any owner input reached this PTY; see `PaneActivity::draft_latched`). Neither
/// timestamps, a quiet window, a Working edge nor a generation bump clears the latch:
/// output stamps are not correlated to the input echo, so they cannot prove the box shows
/// the owner's input.
///
/// Perf (tier standard): one pass over the VT text of <=24 rows, no regex, one Vec per row,
/// no locks, no I/O; called once per profiled pane per digest page and once per accepted
/// `agent.submit` check.
pub fn derive_draft(
    ansi: Option<&str>,
    profile: Option<&AgentProfile>,
    latched: bool,
    last_owner_input_ms: Option<u64>,
    last_output_ms: Option<u64>,
) -> DraftState {
    let Some(profile) = profile else {
        return DraftState::Unknown;
    };
    if latched {
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
    match profile.box_shape {
        BoxShape::RuleBox { glyphs } => rule_box_draft(window, glyphs),
        BoxShape::LastGlyphLine { glyph } => last_glyph_line_draft(window, glyph),
        BoxShape::BackgroundBand { rgb, glyph } => background_band_draft(window, rgb, glyph),
    }
}

/// Transient ownership text from a complete, uniquely recognized composer. This stricter
/// extraction does not change ordinary draft classification. It keeps every non-faint
/// visible character, including reverse-video text under the cursor; only whitespace is
/// collapsed. An uncertain boundary or faint continuation cannot authorize a clear.
/// Work is bounded to the same bottom-row window as draft detection, once per request.
pub fn draft_box_text(ansi: &str, profile: &AgentProfile) -> Option<String> {
    let all = styled_rows(ansi);
    let window = &all[all.len().saturating_sub(WINDOW_LINES)..];
    match profile.box_shape {
        BoxShape::RuleBox { glyphs } => {
            let bottom = window.iter().rposition(|row| is_rule_row(&row.text))?;
            if window[bottom + 1..]
                .iter()
                .filter(|row| !is_blank(row))
                .count()
                > MAX_BOX_GAP
            {
                return None;
            }
            let top = (bottom.saturating_sub(MAX_BOX_GAP + 1)..bottom)
                .rev()
                .find(|&i| is_rule_row(&window[i].text))?;
            box_rows_text(&window[top + 1..bottom], glyphs, None)
        }
        BoxShape::LastGlyphLine { glyph } => {
            let start = window
                .iter()
                .rposition(|row| glyph_start(row, glyph).is_some())?;
            // A blank row ends the composer; a footer cannot be guessed away. Both ends
            // must be visible so clipping cannot turn a suffix into the full draft.
            if start == 0 || !is_blank(&window[start - 1]) {
                return None;
            }
            let end = (start + 1..window.len()).find(|&i| is_blank(&window[i]))?;
            if end - start > MAX_BOX_GAP {
                return None;
            }
            box_rows_text(&window[start..end], &[glyph], None)
        }
        BoxShape::BackgroundBand { rgb, glyph } => {
            let bg = Some(Bg::Rgb(rgb.0, rgb.1, rgb.2));
            let in_band = |row: &StyledRow| row.chars.iter().any(|cell| cell.bg == bg);
            let end = window.iter().rposition(in_band)?;
            let mut start = end;
            while start > 0 && in_band(&window[start - 1]) {
                start -= 1;
            }
            // Seeing a band at either window edge does not prove its full extent.
            if start == 0 || end + 1 == window.len() || end - start + 1 > MAX_BOX_GAP {
                return None;
            }
            box_rows_text(&window[start..=end], &[glyph], bg)
        }
    }
}

fn box_rows_text(rows: &[StyledRow], glyphs: &[char], background: Option<Bg>) -> Option<String> {
    let mut prompt = None;
    for (i, row) in rows.iter().enumerate() {
        let mut glyph_cells = row
            .chars
            .iter()
            .enumerate()
            .filter(|(_, cell)| glyphs.contains(&cell.ch));
        if let Some((start, cell)) = glyph_cells.next() {
            if prompt.is_some()
                || glyph_cells.next().is_some()
                || cell.faint
                || row.chars[..start]
                    .iter()
                    .any(|cell| !cell.ch.is_whitespace())
            {
                return None;
            }
            prompt = Some((i, start));
        }
        if background.is_some()
            && row
                .chars
                .iter()
                .any(|cell| !cell.ch.is_whitespace() && cell.bg != background)
        {
            return None;
        }
    }
    let (glyph_row, start) = prompt?;
    if rows[..glyph_row].iter().any(|row| !is_blank(row)) {
        return None;
    }
    let mut text = String::new();
    for (i, row) in rows.iter().enumerate().skip(glyph_row) {
        let cells = if i == glyph_row {
            &row.chars[start + 1..]
        } else {
            &row.chars[..]
        };
        if i != glyph_row
            && cells
                .iter()
                .any(|cell| cell.faint && !cell.ch.is_whitespace())
        {
            return None;
        }
        for cell in cells {
            if !cell.faint {
                text.push(cell.ch);
            }
        }
        text.push(' ');
    }
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn is_blank(row: &StyledRow) -> bool {
    row.text.trim().is_empty()
}

/// Claude: rule / input row / rule box.
fn rule_box_draft(window: &[StyledRow], glyphs: &[char]) -> DraftState {
    let Some(bottom) = window.iter().rposition(|r| is_rule_row(&r.text)) else {
        return DraftState::Unknown;
    };
    let below = window[bottom + 1..].iter().filter(|r| !is_blank(r)).count();
    if below > MAX_BOX_GAP {
        return DraftState::Unknown;
    }
    let lo = bottom.saturating_sub(MAX_BOX_GAP + 1);
    let Some(top) = (lo..bottom).rev().find(|&i| is_rule_row(&window[i].text)) else {
        return DraftState::Unknown;
    };
    let mut rows = window[top + 1..bottom].iter().filter(|r| !is_blank(r));
    let Some(input) = rows.next() else {
        return DraftState::Unknown;
    };
    let mut cells = input.chars.iter().skip_while(|c| c.ch.is_whitespace());
    if !cells.next().is_some_and(|c| glyphs.contains(&c.ch)) {
        return DraftState::Unknown;
    }
    // Only faint visible chars (or none) after the glyph: placeholder, so the box is empty.
    let typed = cells.any(|c| !c.ch.is_whitespace() && !c.faint);
    if typed || rows.next().is_some() {
        DraftState::Dirty
    } else {
        DraftState::Clean
    }
}

/// Index of the row's first non-whitespace cell when it is `glyph`.
fn glyph_start(row: &StyledRow, glyph: char) -> Option<usize> {
    let i = row.chars.iter().position(|c| !c.ch.is_whitespace())?;
    (row.chars[i].ch == glyph).then_some(i)
}

/// Glyph-row rule: cells after index `after`, ignoring reverse and whitespace cells.
fn glyph_row_state(row: &StyledRow, after: usize) -> DraftState {
    let mut faint = false;
    for c in row.chars[after + 1..]
        .iter()
        .filter(|c| !c.reverse && !c.ch.is_whitespace())
    {
        if !c.faint {
            return DraftState::Dirty;
        }
        faint = true;
    }
    if faint {
        DraftState::Clean
    } else {
        DraftState::Unknown
    }
}

/// Codex: the bottom-most row starting with the glyph.
fn last_glyph_line_draft(window: &[StyledRow], glyph: char) -> DraftState {
    let Some((idx, start)) = window
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, r)| glyph_start(r, glyph).map(|s| (i, s)))
    else {
        return DraftState::Unknown;
    };
    if window[idx + 1..].iter().filter(|r| !is_blank(r)).count() > MAX_BOX_GAP {
        return DraftState::Unknown;
    }
    glyph_row_state(&window[idx], start)
}

/// Cursor: the bottom-most contiguous run of rows with a cell on the band background.
fn background_band_draft(window: &[StyledRow], rgb: (u8, u8, u8), glyph: char) -> DraftState {
    let band_bg = Some(Bg::Rgb(rgb.0, rgb.1, rgb.2));
    let in_band = |r: &StyledRow| r.chars.iter().any(|c| c.bg == band_bg);
    let Some(end) = window.iter().rposition(in_band) else {
        return DraftState::Unknown;
    };
    let mut lo = end;
    while lo > 0 && in_band(&window[lo - 1]) {
        lo -= 1;
    }
    let band = &window[lo..=end];
    let Some((gi, start)) = band
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, r)| glyph_start(r, glyph).map(|s| (i, s)))
    else {
        return DraftState::Unknown;
    };
    let mut state = glyph_row_state(&band[gi], start);
    for (i, row) in band.iter().enumerate() {
        if i == gi {
            continue;
        }
        let mut visible = row
            .chars
            .iter()
            .filter(|c| !c.reverse && !c.ch.is_whitespace());
        if let Some(first) = visible.next() {
            if !first.faint || visible.any(|c| !c.faint) {
                return DraftState::Dirty;
            }
            // Faint-only text elsewhere in the band: not provably an empty box.
            if state == DraftState::Clean {
                state = DraftState::Unknown;
            }
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;
    use crate::supervisor::profile::profile_for;

    const RULE: &str = "────────────────────────────────────────";
    /// Real Claude, empty box: `❯` + NBSP + faint `Try "write a test for <filepath>"`.
    const PLACEHOLDER: &str = include_str!("testdata/claude_empty_box_placeholder.ansi");
    /// Real Claude, typed box: `❯` + NBSP + normal-intensity `draft text here`.
    const TYPED: &str = include_str!("testdata/claude_typed_box.ansi");
    const PLACEHOLDER_SGR: &str = "\u{1b}[2mTry \"write a test for <filepath>\"";

    fn screen(input: &str) -> String {
        format!("Done. Tests pass.\n\n{RULE}\n{input}\n{RULE}\n  ? for shortcuts\n")
    }

    fn claude_profile() -> Option<&'static AgentProfile> {
        profile_for(Agent::Claude)
    }

    fn claude(ansi: &str) -> DraftState {
        derive_draft(Some(ansi), claude_profile(), false, None, Some(5))
    }

    #[test]
    fn draft_empty_box_is_clean() {
        assert_eq!(
            derive_draft(Some(&screen("❯ ")), claude_profile(), false, None, Some(5)),
            DraftState::Clean
        );
        assert_eq!(
            derive_draft(
                Some(&screen(">\u{a0}")),
                claude_profile(),
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
            assert!(rows[rules[0]].chars.iter().all(|c| !c.faint));
        }
    }

    #[test]
    fn draft_real_placeholder_with_latch_is_unknown() {
        assert_eq!(
            derive_draft(Some(PLACEHOLDER), claude_profile(), true, None, Some(5)),
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
            derive_draft(None, claude_profile(), false, None, Some(5)),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_sgr_colour_arguments_are_not_attributes() {
        // Colour components 2 and 22 must not toggle faint.
        let faint = |p: &str, f: bool| {
            apply_sgr(
                p,
                Sgr {
                    faint: f,
                    ..Sgr::default()
                },
            )
            .faint
        };
        assert!(!faint("38;2;2;2;2", false));
        assert!(faint("48;5;22", true));
        assert!(faint("38:2::22:2:2;2", false));
        assert!(!faint("2;0", false));
        assert!(!faint("", true));
    }

    fn full_pen() -> Sgr {
        Sgr {
            faint: true,
            reverse: true,
            bg: Some(Bg::Rgb(1, 2, 3)),
        }
    }

    #[test]
    fn sgr_reset_clears_all_attributes() {
        assert_eq!(apply_sgr("0", full_pen()), Sgr::default());
        assert_eq!(apply_sgr("", full_pen()), Sgr::default());
        assert_eq!(
            apply_sgr("22", full_pen()),
            Sgr {
                faint: false,
                ..full_pen()
            }
        );
        assert_eq!(
            apply_sgr("27", full_pen()),
            Sgr {
                reverse: false,
                ..full_pen()
            }
        );
    }

    #[test]
    fn sgr_reverse_on_and_off() {
        let base = Sgr {
            faint: true,
            reverse: false,
            bg: Some(Bg::Indexed(4)),
        };
        let on = apply_sgr("7", base);
        assert_eq!(
            on,
            Sgr {
                reverse: true,
                ..base
            }
        );
        assert_eq!(apply_sgr("27", on), base);
    }

    #[test]
    fn sgr_background_forms() {
        let none = Sgr::default();
        let bg = |p: &str, s: Sgr| apply_sgr(p, s).bg;
        assert_eq!(bg("48;2;21;21;21", none), Some(Bg::Rgb(21, 21, 21)));
        assert_eq!(bg("48;5;236", none), Some(Bg::Indexed(236)));
        assert_eq!(bg("41", none), Some(Bg::Indexed(1)));
        assert_eq!(bg("107", none), Some(Bg::Indexed(15)));
        assert_eq!(bg("49", full_pen()), None);
        assert_eq!(bg("48:2::9:9:9", full_pen()), Some(Bg::Rgb(1, 2, 3)));
        assert_eq!(bg("38;2;48;5;7", none), None);
        assert_eq!(bg("48;2;300;0;0", full_pen()), Some(Bg::Rgb(1, 2, 3)));
    }

    #[test]
    fn styled_rows_track_reverse_and_background() {
        let rows =
            styled_rows("\u{1b}[48;2;21;21;21m\u{1b}[2m→\u{1b}[7mP\u{1b}[27mlan\u{1b}[0m x\n");
        let bg = Some(Bg::Rgb(21, 21, 21));
        let c = &rows[0].chars;
        assert_eq!(
            (c[0].ch, c[0].faint, c[0].reverse, c[0].bg),
            ('→', true, false, bg)
        );
        assert_eq!(
            (c[1].ch, c[1].faint, c[1].reverse, c[1].bg),
            ('P', true, true, bg)
        );
        assert_eq!(
            (c[2].ch, c[2].faint, c[2].reverse, c[2].bg),
            ('l', true, false, bg)
        );
        for cell in &c[5..] {
            assert!(!cell.faint && !cell.reverse && cell.bg.is_none());
        }
        assert_eq!(c[6].ch, 'x');
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
            derive_draft(
                Some(&screen("❯ ")),
                claude_profile(),
                false,
                Some(5),
                Some(5)
            ),
            DraftState::Unknown
        );
        assert_eq!(
            derive_draft(Some(PLACEHOLDER), claude_profile(), false, Some(5), Some(5)),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_typed_text_is_dirty() {
        assert_eq!(
            derive_draft(
                Some(&screen("❯ fix the tests")),
                claude_profile(),
                false,
                None,
                Some(5)
            ),
            DraftState::Dirty
        );
        let multiline = format!("{RULE}\n❯ \n  second line\n{RULE}\n");
        assert_eq!(
            derive_draft(Some(&multiline), claude_profile(), false, None, None),
            DraftState::Dirty
        );
    }

    #[test]
    fn draft_without_box_is_unknown() {
        assert_eq!(
            derive_draft(
                Some("just output\n❯ \n"),
                claude_profile(),
                false,
                None,
                None
            ),
            DraftState::Unknown
        );
        let lone_rule = format!("text\n{RULE}\n❯ \n");
        assert_eq!(
            derive_draft(Some(&lone_rule), claude_profile(), false, None, None),
            DraftState::Unknown
        );
        let no_glyph = format!("{RULE}\n  Do you want to proceed?\n{RULE}\n");
        assert_eq!(
            derive_draft(Some(&no_glyph), claude_profile(), false, None, None),
            DraftState::Unknown
        );
        let far = format!("{RULE}\n❯ \n{RULE}\n{}", "footer\n".repeat(MAX_BOX_GAP + 1));
        assert_eq!(
            derive_draft(Some(&far), claude_profile(), false, None, None),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_without_profile_is_unknown() {
        for ansi in [screen("❯ "), PLACEHOLDER.to_string()] {
            assert_eq!(
                derive_draft(Some(&ansi), None, false, None, None),
                DraftState::Unknown
            );
        }
    }

    #[test]
    fn draft_owner_input_after_output_is_unknown() {
        assert_eq!(
            derive_draft(
                Some(&screen("❯ ")),
                claude_profile(),
                false,
                Some(6),
                Some(5)
            ),
            DraftState::Unknown
        );
        assert_eq!(
            derive_draft(Some(&screen("❯ ")), claude_profile(), false, Some(6), None),
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
                    derive_draft(Some(&screen(row)), claude_profile(), true, input, output),
                    DraftState::Unknown
                );
            }
        }
        // Unlatched, the same evidence reads from the screen.
        assert_eq!(
            derive_draft(
                Some(&screen("❯ ")),
                claude_profile(),
                false,
                Some(t),
                Some(t + 2)
            ),
            DraftState::Clean
        );
    }

    fn codex_profile() -> Option<&'static AgentProfile> {
        profile_for(Agent::Codex)
    }

    fn cursor_profile() -> Option<&'static AgentProfile> {
        profile_for(Agent::Cursor)
    }

    fn codex(ansi: &str) -> DraftState {
        derive_draft(Some(ansi), codex_profile(), false, None, Some(5))
    }

    fn cursor(ansi: &str) -> DraftState {
        derive_draft(Some(ansi), cursor_profile(), false, None, Some(5))
    }

    const BAND: &str = "\u{1b}[48;2;21;21;21m";

    /// A cursor-style band: blank band row, `rows`, blank band row, then a footer.
    fn band(rows: &[&str]) -> String {
        let blank = format!("{BAND}{}\u{1b}[0m\n", " ".repeat(20));
        let mut out = format!("Tip\n\n{blank}");
        for r in rows {
            out.push_str(&format!("{BAND} {r}\u{1b}[0m\n"));
        }
        out.push_str(&blank);
        out.push_str("\n  Auto\n");
        out
    }

    #[test]
    fn draft_agent_captures_match_expected_states() {
        macro_rules! cap {
            ($n:literal) => {
                ($n, include_str!(concat!("testdata/agents/", $n, ".ansi")))
            };
        }
        let clean = [
            cap!("codex_empty"),
            cap!("codex_working"),
            cap!("codex_after_esc"),
            cap!("codex_after_ctrl_u"),
            cap!("codex_after_reset"),
            cap!("codex_after_clear"),
            cap!("codex_after_resume"),
            cap!("cursor_empty"),
            cap!("cursor_working"),
            cap!("cursor_after_ctrl_u"),
            cap!("cursor_after_reset"),
            cap!("cursor_after_resume"),
        ];
        let dirty = [
            cap!("codex_typed"),
            cap!("codex_reset_typed"),
            cap!("codex_clear_typed"),
            cap!("cursor_typed"),
            cap!("cursor_after_esc"),
            cap!("cursor_reset_typed"),
        ];
        for (expected, set) in [
            (DraftState::Clean, &clean[..]),
            (DraftState::Dirty, &dirty[..]),
        ] {
            for (name, ansi) in set {
                let profile = if name.starts_with("codex_") {
                    codex_profile()
                } else {
                    cursor_profile()
                };
                assert_eq!(
                    derive_draft(Some(ansi), profile, false, None, Some(5)),
                    expected,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn draft_glyph_row_without_faint_text_is_unknown() {
        assert_eq!(codex("\u{203a}   \n\n  footer\n"), DraftState::Unknown);
        assert_eq!(cursor(&band(&["\u{2192}  "])), DraftState::Unknown);
    }

    #[test]
    fn draft_reverse_only_cell_is_unknown() {
        assert_eq!(
            codex("\u{203a} \u{1b}[7mX\u{1b}[27m\n"),
            DraftState::Unknown
        );
        assert_eq!(
            cursor(&band(&["\u{2192} \u{1b}[7mX\u{1b}[27m"])),
            DraftState::Unknown
        );
    }

    #[test]
    fn draft_band_without_glyph_is_unknown() {
        assert_eq!(cursor(&band(&["plain row"])), DraftState::Unknown);
    }

    #[test]
    fn draft_claude_capture_with_codex_profile_is_unknown() {
        assert_eq!(codex(PLACEHOLDER), DraftState::Unknown);
    }

    #[test]
    fn draft_band_other_row_text_is_dirty() {
        let ansi = band(&["\u{2192} \u{1b}[2mAdd a follow-up\u{1b}[0m", "stray text"]);
        assert_eq!(cursor(&ansi), DraftState::Dirty);
        let clean = band(&["\u{2192} \u{1b}[2mAdd a follow-up\u{1b}[0m"]);
        assert_eq!(cursor(&clean), DraftState::Clean);
    }

    #[test]
    fn draft_codex_rows_below_glyph_bounded_by_max_box_gap() {
        let glyph = "\u{203a} \u{1b}[2mAsk Codex\u{1b}[0m\n";
        let at_gap = format!("{glyph}{}", "footer\n".repeat(MAX_BOX_GAP));
        assert_eq!(codex(&at_gap), DraftState::Clean);
        let over = format!("{glyph}{}", "footer\n".repeat(MAX_BOX_GAP + 1));
        assert_eq!(codex(&over), DraftState::Unknown);
        let spaced = format!("{glyph}{}", "footer\n\n".repeat(MAX_BOX_GAP));
        assert_eq!(codex(&spaced), DraftState::Clean);
    }

    #[test]
    fn draft_band_other_row_faint_only_is_unknown() {
        let ansi = band(&[
            "\u{2192} \u{1b}[2mAdd a follow-up\u{1b}[0m",
            "\u{1b}[2mfaint hint\u{1b}[0m",
        ]);
        assert_eq!(cursor(&ansi), DraftState::Unknown);
    }

    #[test]
    fn draft_band_dirty_glyph_row_wins_over_faint_other_row() {
        let faint = "\u{1b}[2mhint\u{1b}[0m";
        assert_eq!(
            cursor(&band(&["\u{2192} typed text", faint])),
            DraftState::Dirty
        );
        assert_eq!(
            cursor(&band(&[faint, "\u{2192} typed text"])),
            DraftState::Dirty
        );
    }

    #[test]
    fn draft_bottom_most_band_wins() {
        let blank = format!("{BAND}{}\u{1b}[0m\n", " ".repeat(20));
        let clean = format!("{BAND} \u{2192} \u{1b}[2mAdd a follow-up\u{1b}[0m\n");
        let dirty = format!("{BAND} \u{2192} typed text\u{1b}[0m\n");
        let two = |upper: &str, lower: &str| {
            format!("{blank}{upper}{blank}between\n{blank}{lower}{blank}\n  Auto\n")
        };
        assert_eq!(cursor(&two(&dirty, &clean)), DraftState::Clean);
        assert_eq!(cursor(&two(&clean, &dirty)), DraftState::Dirty);
    }

    #[test]
    fn draft_band_ignores_other_background_rows() {
        let other = "\u{1b}[48;2;36;36;40m queued message text\u{1b}[0m\n";
        let blank = format!("{BAND}{}\u{1b}[0m\n", " ".repeat(20));
        let adjacent = format!(
            "{other}{blank}{BAND} \u{2192} \u{1b}[2mAdd a follow-up\u{1b}[0m\n{blank}\n  Auto\n"
        );
        assert_eq!(cursor(&adjacent), DraftState::Clean);
    }

    #[test]
    fn draft_glyph_must_lead_the_row() {
        assert_eq!(
            codex("x \u{203a} \u{1b}[2mhint\u{1b}[0m\n"),
            DraftState::Unknown
        );
    }

    #[test]
    fn sgr_underline_colour_arguments_are_skipped() {
        assert!(!apply_sgr("58;5;2", Sgr::default()).faint);
        assert_eq!(apply_sgr("58;2;1;7;1", Sgr::default()), Sgr::default());
    }

    #[test]
    fn sgr_background_range_edges() {
        let bg = |p: &str| apply_sgr(p, Sgr::default()).bg;
        assert_eq!(bg("40"), Some(Bg::Indexed(0)));
        assert_eq!(bg("47"), Some(Bg::Indexed(7)));
        assert_eq!(bg("100"), Some(Bg::Indexed(8)));
        for p in ["39", "48", "108"] {
            assert_eq!(apply_sgr(p, full_pen()).bg, full_pen().bg, "{p}");
        }
    }

    #[test]
    fn clear_draft_box_text_ownership() {
        let profile = cursor_profile().unwrap();
        for (rows, expected) in [
            (
                vec!["\u{2192} first  line", "  second\u{a0}line  "],
                "first line second line",
            ),
            (vec!["\u{2192} \x1b[2mPlan, search, build\x1b[22m"], ""),
            (vec!["\u{2192} ow\x1b[7mn\x1b[27med", " text"], "owned text"),
            (vec!["\u{2192} owned text\x1b[7mX\x1b[27m"], "owned textX"),
        ] {
            assert_eq!(
                draft_box_text(&band(&rows), profile).as_deref(),
                Some(expected)
            );
        }
        assert_ne!(
            draft_box_text(&band(&["\u{2192} owned textX"]), profile).as_deref(),
            Some("owned text")
        );
        assert_eq!(
            draft_box_text(
                &screen("\u{276f} first\n  second"),
                claude_profile().unwrap()
            )
            .as_deref(),
            Some("first second")
        );
        assert_eq!(
            draft_box_text(
                "above\n\n\u{203a} first\n second\n\nfooter\n",
                codex_profile().unwrap()
            )
            .as_deref(),
            Some("first second")
        );
        // An older band is not part of the bottommost composer.
        let two = format!("{}{}", band(&["\u{2192} old"]), band(&["\u{2192} new"]));
        assert_eq!(draft_box_text(&two, profile).as_deref(), Some("new"));
    }

    #[test]
    fn clear_draft_box_text_rejects_ambiguous_or_clipped() {
        let profile = cursor_profile().unwrap();
        for rows in [
            vec!["\u{2192} own", "\u{2192} extra"],
            vec!["before \u{2192} own"],
            vec!["before", "\u{2192} own"],
            vec!["\u{2192} own", "\x1b[2mfaint continuation"],
            vec!["no glyph"],
            vec!["\u{2192} own", "\x1b[0moutside the band"],
        ] {
            assert!(draft_box_text(&band(&rows), profile).is_none(), "{rows:?}");
        }
        assert!(draft_box_text("\u{2192} own\n", profile).is_none());
        // Missing top or bottom witnesses; a 24-row crop cannot prove the band complete.
        let clipped_top = format!("{BAND} \u{2192} own\x1b[0m\nfooter\n");
        let clipped_bottom = format!("above\n{BAND} \u{2192} own\x1b[0m");
        let window_clipped = format!(
            "above\n{BAND} \u{2192} own\n{}\x1b[0m\nfooter\n",
            " \n".repeat(WINDOW_LINES)
        );
        let over_gap = band(&["\u{2192} own", "one", "two", "three", "four"]);
        for ansi in [clipped_top, clipped_bottom, window_clipped, over_gap] {
            assert!(draft_box_text(&ansi, profile).is_none());
        }
        assert!(draft_box_text(
            &screen("\u{276f} own\n> another"),
            claude_profile().unwrap()
        )
        .is_none());
        assert!(draft_box_text(
            &format!("{RULE}\n\u{276f} own\n"),
            claude_profile().unwrap()
        )
        .is_none());
        assert!(draft_box_text("\u{203a} own\n", codex_profile().unwrap()).is_none());
    }
}

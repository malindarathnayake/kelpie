//! Fork (Q1): draws the close-stale review dialog. State and rules live in `close_stale`.

use super::*;
use crate::client::shell::close_stale::{ClientCloseStaleOverlay, CloseStaleRow};

const WIDTH: u16 = 78;
const NAME_COLUMN: usize = 22;
const IDLE_COLUMN: usize = 7;
const STATE_COLUMN: usize = 9;

fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let used = UnicodeWidthStr::width(text.as_str());
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

fn row_lines(row: &CloseStaleRow) -> u16 {
    1 + u16::from(row.risky())
}

/// Main line of one row, also used by tests through the rendered buffer.
fn row_text(row: &CloseStaleRow) -> String {
    format!(
        "{} {}{}{}{}",
        if row.ticked { "[x]" } else { "[ ]" },
        pad(&format!("{} {}", row.number, row.label), NAME_COLUMN),
        pad(&row.idle_label(), IDLE_COLUMN),
        pad(row.state, STATE_COLUMN),
        if row.risky() {
            row.risks.join(", ")
        } else {
            "none".to_owned()
        },
    )
}

pub(super) fn render_close_stale_overlay(
    b: &mut Buffer,
    dialog: &ClientCloseStaleOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let body_lines = dialog.rows.iter().map(row_lines).sum::<u16>().max(1);
    // title, 2 layer lines, blank, column header, body, blank, summary, buttons, hint
    let wanted = 4 + 1 + body_lines + 5 + 2;
    let q = popup(b.area, WIDTH, wanted)?;
    let i = panel(b, q, p.peach, p.panel_bg)?;
    let base = Style::default().bg(p.panel_bg);
    let dim = base.fg(p.overlay0);
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &format!(" Close stale workspaces ({})", dialog.rows.len()),
        base.fg(p.peach).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        " 1 layout: leaves the sidebar · 2 processes: pane processes end",
        dim,
    );
    put_text(
        b,
        i.x,
        i.y + 2,
        i.width,
        " 3 history: agent sessions are not deleted and stay resumable",
        dim,
    );
    put_text(
        b,
        i.x,
        i.y + 4,
        i.width,
        &format!(
            "     {}{}{}running work",
            pad("workspace", NAME_COLUMN),
            pad("idle", IDLE_COLUMN),
            pad("state", STATE_COLUMN)
        ),
        base.fg(p.overlay1),
    );

    // Footer: summary, buttons and key hint sit at the bottom of the panel.
    let hint_y = i.bottom().saturating_sub(1);
    let buttons_y = hint_y.saturating_sub(1);
    let summary_y = buttons_y.saturating_sub(1);
    let body_top = i.y + 5;
    let body_bottom = summary_y.saturating_sub(1);
    let mut rows_hits = Vec::new();
    if dialog.rows.is_empty() {
        put_text(
            b,
            i.x,
            body_top,
            i.width,
            &format!(
                " No workspace has been idle longer than {}.",
                super::super::sidebar_activity::format_idle(dialog.stale_after_ms)
            ),
            base.fg(p.text),
        );
    }
    // Scroll so the cursor row stays visible.
    let visible = body_bottom.saturating_sub(body_top);
    let mut first = 0;
    while first < dialog.cursor {
        let span = dialog.rows[first..=dialog.cursor]
            .iter()
            .map(row_lines)
            .sum::<u16>();
        if span <= visible {
            break;
        }
        first += 1;
    }
    let mut y = body_top;
    for (index, row) in dialog.rows.iter().enumerate().skip(first) {
        if y.saturating_add(row_lines(row)) > body_bottom {
            break;
        }
        let selected = index == dialog.cursor;
        let marker = if selected { " ›" } else { "  " };
        let line = Rect::new(i.x, y, i.width, 1);
        if selected {
            b.set_style(line, base.bg(p.surface0));
        }
        let row_bg = if selected { p.surface0 } else { p.panel_bg };
        put_text(b, i.x, y, 2, marker, base.bg(row_bg).fg(p.accent));
        let text = row_text(row);
        put_text(
            b,
            i.x + 3,
            y,
            i.width.saturating_sub(3),
            &text,
            Style::default().bg(row_bg).fg(p.text),
        );
        if row.risky() {
            let risk_x = i.x + 3 + (4 + NAME_COLUMN + IDLE_COLUMN + STATE_COLUMN) as u16;
            put_text(
                b,
                risk_x,
                y,
                i.right().saturating_sub(risk_x),
                &row.risks.join(", "),
                Style::default().bg(row_bg).fg(p.peach),
            );
        }
        rows_hits.push((line, index, false));
        y += 1;
        if row.risky() {
            let terminate = Rect::new(i.x, y, i.width, 1);
            put_text(
                b,
                i.x + 7,
                y,
                i.width.saturating_sub(7),
                &format!(
                    "{} also terminate (t)",
                    if row.terminate { "[x]" } else { "[ ]" }
                ),
                base.fg(if row.blocked() { p.red } else { p.subtext0 }),
            );
            rows_hits.push((terminate, index, true));
            y += 1;
        }
    }

    let (summary, summary_style) = match dialog.notice.as_deref() {
        Some(notice) => (notice.to_owned(), base.fg(p.red)),
        None => (dialog.summary(), base.fg(p.text)),
    };
    put_text(
        b,
        i.x,
        summary_y,
        i.width,
        &format!(" {summary}"),
        summary_style,
    );
    let rs = row(Rect::new(i.x, buttons_y, i.width, 1), &[20, 12], 2, 0);
    let [ok, cancel] = rs.as_slice() else {
        return None;
    };
    button(
        b,
        *ok,
        " ↵ close selected ",
        Style::default()
            .fg(contrast(p))
            .bg(p.peach)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        " esc cancel ",
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        i.x,
        hint_y,
        i.width,
        " space tick · t also terminate · ↑↓ move · nothing closes until ↵",
        dim,
    );
    Some(OverlayRender {
        area: q,
        primary: *ok,
        cancel: *cancel,
        close_stale_rows: rows_hits,
        ..OverlayRender::default()
    })
}

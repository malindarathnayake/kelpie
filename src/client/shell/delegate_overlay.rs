//! Fork: draws the delegate menu. State and rules live in `delegate`.

use super::*;
use crate::client::shell::delegate::{choice_label, ClientDelegateOverlay};

const WIDTH: u16 = 52;

pub(super) fn render_delegate_overlay(
    b: &mut Buffer,
    menu: &ClientDelegateOverlay,
    now_ms: u64,
    p: &Palette,
) -> Option<OverlayRender> {
    // title, status, blank, choices, blank, buttons, hint
    let wanted = 3 + menu.choices.len() as u16 + 4 + 2;
    let q = popup(b.area, WIDTH, wanted)?;
    let i = panel(b, q, p.mauve, p.panel_bg)?;
    let base = Style::default().bg(p.panel_bg);
    let dim = base.fg(p.overlay0);
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &format!(" Hand {} to the supervisor", menu.pane_id),
        base.fg(p.mauve).add_modifier(Modifier::BOLD),
    );
    let status = match menu.current {
        None => "not delegated · you type, the bot asks".to_owned(),
        Some(current) => format!(
            "◈ delegated · {}",
            super::super::sidebar_activity::delegation_remaining_label(current, now_ms)
        ),
    };
    put_text(b, i.x, i.y + 1, i.width, &format!(" {status}"), dim);

    for (y, (index, choice)) in (i.y + 3..).zip(menu.choices.iter().enumerate()) {
        let selected = index == menu.cursor;
        let line = Rect::new(i.x, y, i.width, 1);
        let row_bg = if selected { p.surface0 } else { p.panel_bg };
        if selected {
            b.set_style(line, base.bg(row_bg));
        }
        put_text(
            b,
            i.x,
            y,
            i.width,
            &format!(
                " {} {}",
                if selected { "›" } else { " " },
                choice_label(*choice)
            ),
            Style::default().bg(row_bg).fg(p.text),
        );
    }

    let hint_y = i.bottom().saturating_sub(1);
    let buttons_y = hint_y.saturating_sub(1);
    let rs = row(Rect::new(i.x, buttons_y, i.width, 1), &[11, 12], 2, 0);
    let [ok, cancel] = rs.as_slice() else {
        return None;
    };
    button(
        b,
        *ok,
        " ↵ apply ",
        Style::default()
            .fg(contrast(p))
            .bg(p.mauve)
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
        " your typing always wins · revoke any time",
        dim,
    );
    Some(OverlayRender {
        area: q,
        primary: *ok,
        cancel: *cancel,
        ..OverlayRender::default()
    })
}

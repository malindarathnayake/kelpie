//! Fork: draws the read-only host facts inside the settings Host section. Text
//! and layout come from `host_facts_panel`; this module only maps tones onto
//! the active palette.

use super::*;
use crate::client::shell::host_facts_panel::{
    panel_content, ClientHostFactsOverlay, PanelLine, Tone,
};

pub(super) use crate::client::shell::host_facts_panel::{now_ms, popup_rect};

fn tone_style(tone: Tone, palette: &Palette) -> Style {
    let base = Style::default().bg(palette.panel_bg);
    match tone {
        Tone::Text => base.fg(palette.text),
        Tone::Dim => base.fg(palette.subtext0),
        Tone::Label => base.fg(palette.overlay1).add_modifier(Modifier::BOLD),
        Tone::Ok => base.fg(palette.green),
        Tone::Missing => base.fg(palette.overlay0),
        Tone::Failed => base.fg(palette.red),
        Tone::Warn => base.fg(palette.peach).add_modifier(Modifier::BOLD),
    }
}

fn draw_line(buffer: &mut Buffer, area: Rect, y: u16, line: &PanelLine, palette: &Palette) {
    let mut x = area.x;
    for (text, tone) in line {
        let room = area.right().saturating_sub(x);
        if room == 0 {
            break;
        }
        let width = display_width(text).min(room);
        put_text(buffer, x, y, width, text, tone_style(*tone, palette));
        x = x.saturating_add(width);
    }
}

/// Draws the Host section: header, rule and the scrolled body in `content`,
/// and the status and key hints on the one-row `hint` area.
pub(super) fn render_host_facts_section(
    buffer: &mut Buffer,
    content: Rect,
    hint: Rect,
    overlay: &ClientHostFactsOverlay,
    now_ms: u64,
    palette: &Palette,
) {
    if content.height == 0 || content.width == 0 {
        return;
    }
    let lines = panel_content(overlay, usize::from(content.width), now_ms);
    let rule = Style::default().fg(palette.surface0).bg(palette.panel_bg);

    draw_line(buffer, content, content.y, &lines.header, palette);
    if content.height > 1 {
        put_text(
            buffer,
            content.x,
            content.y + 1,
            content.width,
            &"─".repeat(usize::from(content.width)),
            rule,
        );
    }

    let visible = usize::from(content.height.saturating_sub(2));
    let max_scroll = lines.body.len().saturating_sub(visible);
    let scroll = overlay.scroll.min(max_scroll);
    for (offset, line) in lines.body.iter().skip(scroll).take(visible).enumerate() {
        let y = content.y + 2 + u16::try_from(offset).unwrap_or(u16::MAX);
        draw_line(buffer, content, y, line, palette);
    }

    let mut footer: PanelLine = Vec::new();
    if let Some((status, tone)) = lines.footer_status {
        footer.push((format!(" {status}"), tone));
        footer.push((" ·".into(), Tone::Dim));
    }
    if max_scroll > 0 {
        footer.push((" ↑↓ scroll ·".into(), Tone::Dim));
    }
    footer.push((" r refresh · tab section".into(), Tone::Dim));
    draw_line(buffer, hint, hint.y, &footer, palette);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::shell::host_facts_panel::{sample_facts, PendingHostFacts};

    fn loaded() -> ClientHostFactsOverlay {
        let mut overlay = ClientHostFactsOverlay::new(ClientEndpointId::Local);
        overlay.facts = Some(Box::new(sample_facts()));
        overlay
    }

    /// Renders the settings overlay on its Host section showing `overlay`.
    fn render(overlay: &ClientHostFactsOverlay, width: u16, height: u16) -> (Buffer, Rect) {
        let palette = Palette::catppuccin();
        let settings = ClientSettingsOverlay {
            section: ClientSettingsSection::Host,
            selected: 0,
            original_theme_name: String::new(),
            original_palette: palette.clone(),
            integrations: Vec::new(),
            integration_messages: Vec::new(),
            loading_integrations: false,
            installing_integrations: false,
            theme_picker: false,
            host: Some(overlay.clone()),
            access: Default::default(),
            handover_keys: None,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let rendered = super::super::settings_overlay::render_settings_overlay(
            &mut buffer,
            &settings,
            false,
            1_012_000,
            &palette,
        )
        .expect("panel fits");
        (buffer, rendered.area)
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        let area = buffer.area;
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Foreground of the first cell of `needle`.
    fn fg_of(buffer: &Buffer, needle: &str) -> ratatui::style::Color {
        let rows = rows(buffer);
        for (y, row) in rows.iter().enumerate() {
            if let Some(byte) = row.find(needle) {
                let x = row[..byte].chars().count();
                return buffer[(x as u16, y as u16)].fg;
            }
        }
        panic!("{needle:?} not rendered:\n{}", rows.join("\n"));
    }

    /// Foreground of `status` on the table row of `tool`.
    fn status_fg(buffer: &Buffer, tool: &str, status: &str) -> ratatui::style::Color {
        let rows = rows(buffer);
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.contains(&format!("│ {tool} ")))
            .unwrap_or_else(|| {
                panic!(
                    "no row for {tool}:
{}",
                    rows.join(
                        "
"
                    )
                )
            });
        let byte = row
            .find(&format!(" {status} "))
            .unwrap_or_else(|| panic!("no {status:?} word for {tool}: {row}"));
        let x = row[..byte].chars().count() + 1;
        buffer[(x as u16, y as u16)].fg
    }

    #[test]
    fn host_panel_wide_shows_header_summary_and_tool_status_words() {
        let (buffer, area) = render(&loaded(), 100, 40);
        let text = rows(&buffer).join("\n");
        println!("{text}");
        assert!(area.width <= 96);
        assert!(
            text.contains(" Host  ·  refreshed 12s ago  ·  every 60s"),
            "{text}"
        );
        for needle in [
            "os       Windows 11 24H2 build 26100.4061 (x86_64)",
            "memory   16-32 GB",
            "disk     100-500 GB free",
            "shell    pwsh 7.4.6 · ConstrainedLanguage",
            r"git      %ProgramFiles%\Git\cmd\git.exe · bash %ProgramFiles%\Git\bin\bash.exe",
            "gpu      nvidia-smi missing",
            "docker   running",
            "wsl      present: Ubuntu",
            "tool    status    version",
            r"↳ shadowed by %SystemRoot%\System32\find.exe",
            "r refresh · tab section",
        ] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }
        // Status is a word as well as a colour.
        let palette = Palette::catppuccin();
        assert_eq!(status_fg(&buffer, "rg", "ok"), palette.green);
        assert_eq!(status_fg(&buffer, "sd", "missing"), palette.overlay0);
        assert_eq!(status_fg(&buffer, "python", "failed"), palette.red);
        assert_eq!(status_fg(&buffer, "find", "shadowed"), palette.peach);
    }

    #[test]
    fn host_panel_flags_constrained_language_in_warning_colour_and_text() {
        let (buffer, _) = render(&loaded(), 100, 40);
        let text = rows(&buffer).join("\n");
        assert!(
            text.contains("warning: Constrained Language Mode, scripts are restricted"),
            "{text}"
        );
        let palette = Palette::catppuccin();
        assert_eq!(fg_of(&buffer, "ConstrainedLanguage"), palette.peach);
        assert_eq!(fg_of(&buffer, "warning: Constrained"), palette.peach);

        let mut full = loaded();
        if let Some(facts) = full.facts.as_mut() {
            facts.shells.constrained_language_mode = Some(false);
            if let Some(pwsh) = facts.shells.pwsh.as_mut() {
                pwsh.language_mode = Some("FullLanguage".into());
            }
        }
        let (buffer, _) = render(&full, 100, 40);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("pwsh 7.4.6 · FullLanguage"), "{text}");
        assert!(!text.contains("warning"), "{text}");
    }

    #[test]
    fn host_panel_cuts_long_paths_from_the_left() {
        let (buffer, _) = render(&loaded(), 100, 40);
        let text = rows(&buffer).join("\n");
        let rg = text
            .lines()
            .find(|line| line.contains(" rg "))
            .expect("rg row");
        assert!(rg.contains('…'), "{rg}");
        assert!(rg.contains(r"x86_64-pc-windows-msvc\rg.exe"), "{rg}");
        assert!(!rg.contains(r"%LOCALAPPDATA%\Microsoft\WinGet"), "{rg}");
    }

    #[test]
    fn host_panel_narrow_keeps_status_words_and_path_tails() {
        let (buffer, area) = render(&loaded(), 60, 40);
        let text = rows(&buffer).join("\n");
        println!("{text}");
        assert_eq!(area.width, 56);
        for needle in [
            "ok ",
            "missing ",
            "failed ",
            "shadowed ",
            r"\rg.exe",
            r"\find.exe",
            "warning: Constrained",
        ] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }
        // Rows that do not fit end with an ellipsis instead of a hard cut.
        let git = text
            .lines()
            .find(|line| line.contains(" git "))
            .expect("git row");
        assert!(git.contains("…│"), "{git}");
    }

    #[test]
    fn host_panel_small_terminal_scrolls_body_and_keeps_footer() {
        let mut overlay = loaded();
        let (buffer, _) = render(&overlay, 100, 14);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("↑↓ scroll"), "{text}");
        assert!(text.contains("esc close"), "{text}");
        assert!(text.contains("os       Windows"), "{text}");
        overlay.scroll = 100;
        let (buffer, _) = render(&overlay, 100, 14);
        let text = rows(&buffer).join("\n");
        assert!(!text.contains("os       Windows"), "{text}");
        assert!(text.contains("shadowed by"), "{text}");
    }

    #[test]
    fn host_panel_loading_unavailable_and_refreshing_states() {
        let mut overlay = ClientHostFactsOverlay::new(ClientEndpointId::Local);
        overlay.pending = Some(PendingHostFacts {
            id: "host-facts:1".into(),
            refresh: false,
            deadline: std::time::Instant::now(),
        });
        let (buffer, _) = render(&overlay, 100, 40);
        assert!(rows(&buffer).join("\n").contains("loading…"));

        overlay.pending = None;
        overlay.error = Some("not supported by this server".into());
        for width in [100, 60] {
            let (buffer, _) = render(&overlay, width, 40);
            let text = rows(&buffer).join("\n");
            assert!(
                text.contains("host facts unavailable (not supported by this server)"),
                "{text}"
            );
        }

        let mut overlay = loaded();
        overlay.pending = Some(PendingHostFacts {
            id: "host-facts:2".into(),
            refresh: true,
            deadline: std::time::Instant::now(),
        });
        let (buffer, _) = render(&overlay, 100, 40);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("refreshing…"), "{text}");
        assert!(
            text.contains("os       Windows 11"),
            "the old snapshot stays: {text}"
        );
    }
}

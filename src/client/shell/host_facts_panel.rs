//! Fork: read-only host facts, shown as the settings Host section (the
//! `host_facts` binding, default `prefix+i`, opens settings on it).
//!
//! The facts describe the machine running the attached server, so they are
//! fetched over that endpoint's control lane (`endpoint.host-facts.*`) and never
//! read from the client's own machine. Requests are fire-and-forget messages in
//! `ClientShellInput::requests`; the reply arrives later through
//! `receive_host_facts`, so the UI thread never waits.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::api::schema::host::{HostFacts, HostShellInfo, HostToolFact, HostToolStatus};

/// Extra time past the server's own bounded wait before the panel gives up.
const REPLY_GRACE: Duration = Duration::from_secs(5);
const LABEL_WIDTH: usize = 8;
const STATUS_WIDTH: usize = 8;
const MIN_PATH_WIDTH: usize = 12;

#[derive(Default)]
pub(super) struct HostFactsPanel {
    /// Endpoints whose welcome advertised `host_facts`.
    supported: HashSet<ClientEndpointId>,
    next_serial: u64,
    /// The request still awaiting its reply, kept across panel close so a reopen reuses it
    /// instead of sending a second request that the server would answer `host_facts_busy`.
    outstanding: Option<(ClientEndpointId, PendingHostFacts)>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingHostFacts {
    pub(super) id: String,
    pub(super) refresh: bool,
    pub(super) deadline: Instant,
}

#[derive(Debug, Clone)]
pub(super) struct ClientHostFactsOverlay {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) facts: Option<Box<HostFacts>>,
    pub(super) pending: Option<PendingHostFacts>,
    /// Why the last request failed, shown as `host facts unavailable (...)`.
    pub(super) error: Option<String>,
    pub(super) scroll: usize,
    last_tick_secs: u64,
}

impl ClientHostFactsOverlay {
    pub(super) fn new(endpoint_id: ClientEndpointId) -> Self {
        Self {
            endpoint_id,
            facts: None,
            pending: None,
            error: None,
            scroll: 0,
            last_tick_secs: 0,
        }
    }
}

/// Colour role of a text segment; the renderer maps it onto the active palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Text,
    Dim,
    Label,
    Ok,
    Missing,
    Failed,
    Warn,
}

pub(super) type Segment = (String, Tone);
pub(super) type PanelLine = Vec<Segment>;

/// Everything the renderer draws, already fitted to `width` columns.
pub(super) struct PanelContent {
    pub(super) header: PanelLine,
    pub(super) body: Vec<PanelLine>,
    pub(super) footer_status: Option<Segment>,
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn width_of(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Cuts `text` to `width` columns, ending with `…` when it was longer.
pub(super) fn fit_right(text: &str, width: usize) -> String {
    if width_of(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Keeps the end of `text` (the file name of a path), starting with `…` when cut.
pub(super) fn fit_left(text: &str, width: usize) -> String {
    if width_of(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        kept.push(ch);
        used += w;
    }
    let mut out = String::from("…");
    out.extend(kept.into_iter().rev());
    out
}

/// Cuts a multi-segment line to `width` columns, ending with `…` when cut.
fn fit_line(line: PanelLine, width: usize) -> PanelLine {
    let mut used = 0;
    let mut out = Vec::new();
    for (text, tone) in line {
        let room = width.saturating_sub(used);
        let w = width_of(&text);
        if w <= room {
            used += w;
            out.push((text, tone));
            continue;
        }
        if room > 0 {
            out.push((fit_right(&text, room), tone));
        }
        break;
    }
    out
}

fn pad(text: String, width: usize) -> String {
    let used = width_of(&text);
    if used >= width {
        text
    } else {
        format!("{text}{}", " ".repeat(width - used))
    }
}

fn opt(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn age_label(age_secs: u64) -> String {
    match age_secs {
        0..=119 => format!("{age_secs}s ago"),
        120..=7199 => format!("{}m ago", age_secs / 60),
        _ => format!("{}h ago", age_secs / 3600),
    }
}

fn unavailable_reason(code: Option<&str>, message: &str) -> String {
    match code {
        Some("host_facts_disabled") => {
            "disabled; set [host_facts] enabled = true and restart the server".into()
        }
        Some("host_facts_pending") => "not ready yet; press r to retry".into(),
        Some("host_facts_busy") => "another request is still running".into(),
        _ => message
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect(),
    }
}

fn preferred_shell(facts: &HostFacts) -> Option<(&'static str, &HostShellInfo)> {
    let shells = &facts.shells;
    match shells.preferred.as_deref() {
        Some("pwsh") => shells.pwsh.as_ref().map(|info| ("pwsh", info)),
        Some("powershell") => shells
            .windows_powershell
            .as_ref()
            .map(|info| ("powershell", info)),
        _ => None,
    }
    .or_else(|| shells.pwsh.as_ref().map(|info| ("pwsh", info)))
    .or_else(|| {
        shells
            .windows_powershell
            .as_ref()
            .map(|info| ("powershell", info))
    })
}

fn summary_rows(facts: &HostFacts) -> Vec<(&'static str, PanelLine)> {
    let text = |value: String| vec![(value, Tone::Text)];
    let os = &facts.os;
    let mut os_line = os.name.clone();
    if let Some(version) = &os.version {
        os_line.push_str(&format!(" {version}"));
    }
    if let Some(build) = &os.build {
        os_line.push_str(&format!(" build {build}"));
    }
    os_line.push_str(&format!(" ({}", os.arch));
    if os.process_arch != os.arch {
        os_line.push_str(&format!(", kelpie {}", os.process_arch));
    }
    os_line.push(')');

    let mut constrained_warning = false;
    let shell = match preferred_shell(facts) {
        None => text("no PowerShell found".into()),
        Some((name, info)) => {
            let mode = info.language_mode.as_deref();
            let constrained = facts.shells.constrained_language_mode == Some(true)
                || mode == Some("ConstrainedLanguage");
            let mut line = vec![(
                format!("{name} {} · ", opt(info.version.as_deref())),
                Tone::Text,
            )];
            if constrained {
                line.push((mode.unwrap_or("ConstrainedLanguage").to_owned(), Tone::Warn));
                constrained_warning = true;
            } else {
                line.push((opt(mode).to_owned(), Tone::Text));
            }
            line
        }
    };

    let gpu = if facts.gpu.names.is_empty() {
        format!("nvidia-smi {}", facts.gpu.nvidia_smi.label())
    } else {
        format!(
            "{} (driver {})",
            facts.gpu.names.join(", "),
            opt(facts.gpu.driver_version.as_deref())
        )
    };
    let wsl = match (facts.wsl.present, facts.wsl.distros.is_empty()) {
        (false, _) => "not installed".to_string(),
        (true, true) => "present".to_string(),
        (true, false) => format!("present: {}", facts.wsl.distros.join(", ")),
    };
    vec![
        ("os", text(os_line)),
        (
            "memory",
            text(opt(facts.memory.bucket.as_deref()).to_owned()),
        ),
        (
            "disk",
            text(format!("{} free", opt(facts.disk.bucket.as_deref()))),
        ),
        ("shell", shell),
        (
            "",
            if constrained_warning {
                vec![(
                    "warning: Constrained Language Mode, scripts are restricted".into(),
                    Tone::Warn,
                )]
            } else {
                Vec::new()
            },
        ),
        (
            "git",
            text(format!(
                "{} · bash {}",
                opt(facts.git.git_path.as_deref()),
                opt(facts.git.bash_path.as_deref())
            )),
        ),
        ("gpu", text(gpu)),
        ("docker", text(facts.docker.status.label().to_owned())),
        ("wsl", text(wsl)),
    ]
}

fn status_tone(status: HostToolStatus) -> Tone {
    match status {
        HostToolStatus::Ok => Tone::Ok,
        HostToolStatus::Missing => Tone::Missing,
        HostToolStatus::Failed => Tone::Failed,
        HostToolStatus::Shadowed => Tone::Warn,
    }
}

struct ToolColumns {
    name: usize,
    version: usize,
    source: usize,
    path: usize,
}

fn natural_columns(tools: &[HostToolFact]) -> ToolColumns {
    let max = |f: &dyn Fn(&HostToolFact) -> usize, floor: usize| {
        tools.iter().map(f).max().unwrap_or(floor).max(floor)
    };
    ToolColumns {
        name: max(&|tool| width_of(&tool.name), 4).min(16),
        version: max(&|tool| width_of(opt(tool.version.as_deref())), 7).min(20),
        source: max(
            &|tool| width_of(tool.source.map_or("-", |source| source.label())),
            6,
        ),
        path: max(&|tool| width_of(opt(tool.path.as_deref())), 4),
    }
}

/// Column widths for `width` columns: the path keeps at least `MIN_PATH_WIDTH`
/// by shrinking version, then source, before the path itself gives way.
fn fit_columns(tools: &[HostToolFact], width: usize) -> ToolColumns {
    let mut columns = natural_columns(tools);
    // Leading space plus four two-column gaps.
    let fixed = 1 + columns.name + 2 + STATUS_WIDTH + 2 + 2 + 2;
    let rest = width.saturating_sub(fixed);
    let mut over = (columns.version + columns.source + MIN_PATH_WIDTH).saturating_sub(rest);
    let version_cut = over.min(columns.version.saturating_sub(7));
    columns.version -= version_cut;
    over -= version_cut;
    let source_cut = over.min(columns.source.saturating_sub(6));
    columns.source -= source_cut;
    columns.path = rest.saturating_sub(columns.version + columns.source);
    columns
}

fn tool_lines(tools: &[HostToolFact], width: usize) -> Vec<PanelLine> {
    if tools.is_empty() {
        return vec![vec![(" no tools reported".into(), Tone::Dim)]];
    }
    let columns = fit_columns(tools, width);
    let cell = |text: &str, width: usize| pad(fit_right(text, width), width);
    let mut lines = vec![vec![(
        format!(
            " {}  {}  {}  {}  {}",
            cell("tool", columns.name),
            cell("status", STATUS_WIDTH),
            cell("version", columns.version),
            cell("source", columns.source),
            fit_right("path", columns.path),
        ),
        Tone::Label,
    )]];
    for tool in tools {
        let source = tool.source.map_or("-", |source| source.label());
        lines.push(vec![
            (format!(" {}  ", cell(&tool.name, columns.name)), Tone::Text),
            (
                cell(tool.status.label(), STATUS_WIDTH),
                status_tone(tool.status),
            ),
            (
                format!(
                    "  {}  {}  ",
                    cell(opt(tool.version.as_deref()), columns.version),
                    cell(source, columns.source),
                ),
                Tone::Text,
            ),
            (fit_left(opt(tool.path.as_deref()), columns.path), Tone::Dim),
        ]);
        if let Some(winner) = &tool.shadowed_by {
            let indent = 1 + columns.name + 2;
            let lead = "↳ shadowed by ";
            let room = width.saturating_sub(indent + width_of(lead));
            lines.push(vec![(
                format!("{}{lead}{}", " ".repeat(indent), fit_left(winner, room)),
                Tone::Warn,
            )]);
        }
    }
    lines
}

/// Natural inner width: wide enough for every row without truncation.
pub(super) fn natural_width(overlay: &ClientHostFactsOverlay) -> usize {
    let Some(facts) = overlay.facts.as_deref() else {
        let message = overlay
            .error
            .as_ref()
            .map_or(10, |error| width_of(error) + 30);
        return message.max(44);
    };
    let summary = summary_rows(facts)
        .iter()
        .map(|(_, line)| {
            1 + LABEL_WIDTH + 1 + line.iter().map(|(text, _)| width_of(text)).sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let columns = natural_columns(&facts.tools);
    let table = 1
        + columns.name
        + 2
        + STATUS_WIDTH
        + 2
        + columns.version
        + 2
        + columns.source
        + 2
        + columns.path;
    summary.max(table).max(44)
}

/// Builds the panel text for `width` inner columns at wall-clock `now_ms`.
pub(super) fn panel_content(
    overlay: &ClientHostFactsOverlay,
    width: usize,
    now_ms: u64,
) -> PanelContent {
    let mut header = vec![(" Host".to_owned(), Tone::Label)];
    let mut body = Vec::new();
    match overlay.facts.as_deref() {
        Some(facts) => {
            let age = now_ms.saturating_sub(facts.refreshed_at_ms) / 1000;
            header.push((
                format!(
                    "  ·  refreshed {}  ·  every {}s",
                    age_label(age),
                    facts.interval_secs
                ),
                Tone::Dim,
            ));
            for (label, value) in summary_rows(facts) {
                if label.is_empty() && value.is_empty() {
                    continue;
                }
                let mut line = vec![(
                    format!(" {}", pad(label.to_owned(), LABEL_WIDTH)),
                    Tone::Label,
                )];
                line.push((" ".into(), Tone::Text));
                line.extend(value);
                body.push(line);
            }
            body.push(Vec::new());
            body.extend(tool_lines(&facts.tools, width));
        }
        None => match (&overlay.pending, &overlay.error) {
            (_, Some(error)) => body.push(vec![(
                format!(" host facts unavailable ({error})"),
                Tone::Failed,
            )]),
            _ => body.push(vec![(" loading…".into(), Tone::Dim)]),
        },
    }
    let footer_status = match (&overlay.pending, &overlay.error, overlay.facts.is_some()) {
        (Some(pending), _, _) if pending.refresh => Some(("refreshing…".to_owned(), Tone::Text)),
        (None, Some(error), true) => Some((format!("refresh failed ({error})"), Tone::Failed)),
        _ => None,
    };
    PanelContent {
        header: fit_line(header, width),
        body: body.into_iter().map(|line| fit_line(line, width)).collect(),
        footer_status,
    }
}

/// Width the settings popup keeps for its other sections.
const SETTINGS_MIN_WIDTH: usize = 76;
/// Height the settings popup keeps for its other sections.
const SETTINGS_MIN_HEIGHT: usize = 22;
/// Settings rows around the Host body: border, title, tabs, rule, blank,
/// host header, host rule, hint, buttons, border.
const SETTINGS_CHROME_ROWS: usize = 11;

/// Settings popup rect for the Host section on `screen`: wide enough for the
/// facts at their natural size, capped to the screen.
pub(super) fn popup_rect(overlay: &ClientHostFactsOverlay, screen: Rect) -> Option<Rect> {
    let width = u16::try_from(
        natural_width(overlay)
            .saturating_add(2)
            .max(SETTINGS_MIN_WIDTH),
    )
    .unwrap_or(u16::MAX)
    .min(screen.width.saturating_sub(4));
    // The row count does not depend on the width.
    let body_rows = panel_content(overlay, usize::from(width), 0).body.len();
    let height = u16::try_from(
        body_rows
            .saturating_add(SETTINGS_CHROME_ROWS)
            .max(SETTINGS_MIN_HEIGHT),
    )
    .unwrap_or(u16::MAX)
    .min(screen.height.saturating_sub(2));
    if width < 4 || height < 4 {
        return None;
    }
    Some(Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    ))
}

/// Host body rows visible inside the settings `popup`.
pub(super) fn visible_body_rows(popup: Rect) -> usize {
    usize::from(popup.height).saturating_sub(SETTINGS_CHROME_ROWS)
}

impl ClientShellState {
    pub(crate) fn set_endpoint_host_facts_supported(
        &mut self,
        endpoint_id: &ClientEndpointId,
        supported: bool,
    ) {
        if supported {
            self.host_facts.supported.insert(endpoint_id.clone());
        } else {
            self.host_facts.supported.remove(endpoint_id);
        }
    }

    fn settings_host_mut(&mut self) -> Option<&mut ClientHostFactsOverlay> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::Settings(settings)) => settings.host.as_mut(),
            _ => None,
        }
    }

    fn put_settings_host(&mut self, overlay: ClientHostFactsOverlay) {
        if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
            settings.host = Some(overlay);
        }
    }

    /// Creates the settings Host section state on first selection: reuses a
    /// request still in flight, otherwise sends one.
    pub(super) fn ensure_settings_host_facts(&mut self, outcome: &mut ClientShellInput) {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Settings(settings)) if settings.host.is_none() => {}
            _ => return,
        }
        let endpoint_id = self.active_endpoint_id.clone();
        let mut overlay = ClientHostFactsOverlay::new(endpoint_id.clone());
        if !self.host_facts.supported.contains(&endpoint_id) {
            overlay.error = Some("not supported by this server".into());
        } else if !self.endpoint_is_online(&endpoint_id) {
            overlay.error = Some("server is not connected".into());
        } else if let Some(pending) = self
            .host_facts
            .outstanding
            .as_ref()
            .filter(|(owner, pending)| *owner == endpoint_id && Instant::now() < pending.deadline)
            .map(|(_, pending)| pending.clone())
        {
            overlay.pending = Some(pending);
        } else {
            self.request_host_facts(&mut overlay, false, outcome);
        }
        self.put_settings_host(overlay);
        outcome.repaint = true;
    }

    fn request_host_facts(
        &mut self,
        overlay: &mut ClientHostFactsOverlay,
        refresh: bool,
        outcome: &mut ClientShellInput,
    ) {
        self.host_facts.next_serial = self.host_facts.next_serial.wrapping_add(1);
        let request = crate::protocol::endpoint::EndpointHostFactsRequest {
            id: format!("host-facts:{}", self.host_facts.next_serial),
            refresh,
        };
        let data = match serde_json::to_string(&request) {
            Ok(data) => data,
            Err(error) => {
                overlay.error = Some(format!("could not encode the request: {error}"));
                return;
            }
        };
        outcome.requests.push(ClientMessage::EndpointControl {
            kind: crate::protocol::endpoint::HOST_FACTS_REQUEST_KIND.into(),
            data,
        });
        let pending = PendingHostFacts {
            id: request.id,
            refresh,
            deadline: Instant::now() + crate::host_facts::API_WAIT + REPLY_GRACE,
        };
        self.host_facts.outstanding = Some((overlay.endpoint_id.clone(), pending.clone()));
        overlay.pending = Some(pending);
    }

    /// Scroll and refresh keys of the settings Host section. Returns whether the
    /// key was used; section and close keys stay with the settings router.
    pub(super) fn route_settings_host_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::KeyModifiers;

        let host_section = matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::Settings(settings))
                if settings.section == ClientSettingsSection::Host && settings.host.is_some()
        );
        if !host_section || !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return false;
        }
        if key.code == KeyCode::Char('r') {
            let taken = match self.overlay.as_mut() {
                Some(ClientShellOverlay::Settings(settings)) => settings.host.take(),
                _ => None,
            };
            let Some(mut overlay) = taken else {
                return false;
            };
            // One request at a time; a refresh is ignored while one runs.
            if overlay.pending.is_none() {
                if !self.host_facts.supported.contains(&overlay.endpoint_id) {
                    overlay.error = Some("not supported by this server".into());
                } else if !self.endpoint_is_online(&overlay.endpoint_id) {
                    overlay.error = Some("server is not connected".into());
                } else {
                    overlay.error = None;
                    self.request_host_facts(&mut overlay, true, outcome);
                }
            }
            self.put_settings_host(overlay);
            outcome.repaint = true;
            return true;
        }
        let max_scroll = self.host_facts_max_scroll();
        let Some(overlay) = self.settings_host_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => overlay.scroll = overlay.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                overlay.scroll = (overlay.scroll + 1).min(max_scroll);
            }
            KeyCode::PageUp => overlay.scroll = overlay.scroll.saturating_sub(8),
            KeyCode::PageDown => overlay.scroll = (overlay.scroll + 8).min(max_scroll),
            KeyCode::Home => overlay.scroll = 0,
            KeyCode::End => overlay.scroll = max_scroll,
            _ => return false,
        }
        outcome.repaint = true;
        true
    }

    fn host_facts_max_scroll(&self) -> usize {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_ref() else {
            return 0;
        };
        let Some(overlay) = settings.host.as_ref() else {
            return 0;
        };
        let Some((cols, rows)) = self.last_composed_size else {
            return 0;
        };
        let Some(popup) = popup_rect(overlay, Rect::new(0, 0, cols, rows)) else {
            return 0;
        };
        let width = usize::from(popup.width.saturating_sub(2));
        panel_content(overlay, width, 0)
            .body
            .len()
            .saturating_sub(visible_body_rows(popup))
    }

    /// Applies a `host.facts` reply from `endpoint_id`. Returns whether to repaint.
    pub(crate) fn receive_host_facts(
        &mut self,
        endpoint_id: &ClientEndpointId,
        data: &str,
    ) -> bool {
        let reply_id = serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|value| value.get("id")?.as_str().map(str::to_owned));
        if self
            .host_facts
            .outstanding
            .as_ref()
            .is_some_and(|(owner, pending)| {
                owner == endpoint_id && reply_id.as_deref() == Some(pending.id.as_str())
            })
        {
            self.host_facts.outstanding = None;
        }
        let Some(overlay) = self.settings_host_mut() else {
            return false;
        };
        let Some(pending) = overlay.pending.as_ref() else {
            return false;
        };
        if &overlay.endpoint_id != endpoint_id {
            return false;
        }
        if reply_id.as_deref() != Some(pending.id.as_str()) {
            return false;
        }
        let result = crate::client::endpoint_commands::parse_response(&pending.id, data.as_bytes());
        overlay.pending = None;
        match result {
            Ok(crate::api::schema::ResponseResult::HostFacts(facts)) => {
                overlay.facts = Some(facts);
                overlay.error = None;
            }
            Ok(_) => overlay.error = Some("unexpected reply from server".into()),
            Err(error) => {
                overlay.error = Some(unavailable_reason(error.code.as_deref(), &error.message));
            }
        }
        true
    }

    /// Times out a lost reply and, while the Host section shows, repaints once a
    /// second so the age stays current.
    pub(crate) fn tick_host_facts(&mut self, now: Instant) -> bool {
        if self
            .host_facts
            .outstanding
            .as_ref()
            .is_some_and(|(_, pending)| now >= pending.deadline)
        {
            self.host_facts.outstanding = None;
        }
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() else {
            return false;
        };
        let showing = settings.section == ClientSettingsSection::Host;
        let Some(overlay) = settings.host.as_mut() else {
            return false;
        };
        if overlay
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            overlay.pending = None;
            overlay.error = Some("no reply from server".into());
            return showing;
        }
        if !showing || overlay.facts.is_none() {
            return false;
        }
        let secs = now_ms() / 1000;
        if secs != overlay.last_tick_secs {
            overlay.last_tick_secs = secs;
            return true;
        }
        false
    }
}

/// Test fixture: a Windows host with pwsh in Constrained Language Mode and one
/// tool in every status.
#[cfg(test)]
pub(super) fn sample_facts() -> HostFacts {
    use crate::api::schema::host::{
        HostBucketFact, HostDockerFacts, HostDockerStatus, HostGitFacts, HostGpuFacts, HostOsFacts,
        HostProbeStatus, HostShellFacts, HostToolSource, HostWslFacts,
    };
    let tool = |name: &str,
                status: HostToolStatus,
                version: Option<&str>,
                source: Option<HostToolSource>,
                path: Option<&str>,
                shadowed_by: Option<&str>| HostToolFact {
        name: name.into(),
        path: path.map(Into::into),
        version: version.map(Into::into),
        source,
        status,
        shadowed_by: shadowed_by.map(Into::into),
    };
    HostFacts {
        schema_version: 1,
        refreshed_at_ms: 1_000_000,
        interval_secs: 60,
        os: HostOsFacts {
            name: "Windows 11".into(),
            version: Some("24H2".into()),
            build: Some("26100.4061".into()),
            arch: "x86_64".into(),
            process_arch: "x86_64".into(),
            checked_at_ms: 1_000_000,
        },
        memory: HostBucketFact {
            bucket: Some("16-32 GB".into()),
            checked_at_ms: 1_000_000,
        },
        disk: HostBucketFact {
            bucket: Some("100-500 GB".into()),
            checked_at_ms: 1_000_000,
        },
        shells: HostShellFacts {
            preferred: Some("pwsh".into()),
            pwsh: Some(HostShellInfo {
                path: r"%ProgramFiles%\PowerShell\7\pwsh.exe".into(),
                version: Some("7.4.6".into()),
                language_mode: Some("ConstrainedLanguage".into()),
                psreadline_version: Some("2.3.5".into()),
                status: HostProbeStatus::Ok,
            }),
            windows_powershell: None,
            psreadline_version: Some("2.3.5".into()),
            constrained_language_mode: Some(true),
            checked_at_ms: 1_000_000,
        },
        git: HostGitFacts {
            git_path: Some(r"%ProgramFiles%\Git\cmd\git.exe".into()),
            bash_path: Some(r"%ProgramFiles%\Git\bin\bash.exe".into()),
            checked_at_ms: 1_000_000,
        },
        gpu: HostGpuFacts {
            nvidia_smi: HostProbeStatus::Missing,
            names: Vec::new(),
            driver_version: None,
            checked_at_ms: 1_000_000,
        },
        docker: HostDockerFacts {
            status: HostDockerStatus::Running,
            checked_at_ms: 1_000_000,
        },
        wsl: HostWslFacts {
            present: true,
            distros: vec!["Ubuntu".into()],
            checked_at_ms: 1_000_000,
        },
        tools: vec![
            tool(
                "rg",
                HostToolStatus::Ok,
                Some("ripgrep 14.1.1"),
                Some(HostToolSource::Winget),
                Some(concat!(
                    r"%LOCALAPPDATA%\Microsoft\WinGet\Packages\BurntSushi.ripgrep.MSVC",
                    r"_Microsoft.Winget.Source_8wekyb3d8bbwe\ripgrep-14.1.1-x86_64-pc-windows-msvc",
                    r"\rg.exe"
                )),
                None,
            ),
            tool("sd", HostToolStatus::Missing, None, None, None, None),
            tool(
                "python",
                HostToolStatus::Failed,
                None,
                Some(HostToolSource::Other),
                Some(r"%LOCALAPPDATA%\Microsoft\WindowsApps\python.exe"),
                None,
            ),
            tool(
                "find",
                HostToolStatus::Shadowed,
                Some("find (GNU findutils) 4.9.0"),
                Some(HostToolSource::GitForWindows),
                Some(r"%ProgramFiles%\Git\usr\bin\find.exe"),
                Some(r"%SystemRoot%\System32\find.exe"),
            ),
        ],
        tools_checked_at_ms: 1_000_000,
    }
}

use super::*;
use crate::client::shell::host_facts_panel::sample_facts;
use crate::input::TerminalKey;
use crate::protocol::endpoint::{EndpointHostFactsRequest, HOST_FACTS_REQUEST_KIND};

fn host_state(supported: bool) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_host_facts_supported(&ClientEndpointId::Local, supported);
    state
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> RawInputEvent {
    RawInputEvent::Key(TerminalKey::new(code, modifiers))
}

fn prefix_i() -> Vec<RawInputEvent> {
    vec![
        key(KeyCode::Char('b'), KeyModifiers::CONTROL),
        key(KeyCode::Char('i'), KeyModifiers::NONE),
    ]
}

/// The host facts requests the shell queued for the server, decoded.
fn host_requests(outcome: &ClientShellInput) -> Vec<EndpointHostFactsRequest> {
    outcome
        .requests
        .iter()
        .filter_map(|message| match message {
            ClientMessage::EndpointControl { kind, data } if kind == HOST_FACTS_REQUEST_KIND => {
                Some(serde_json::from_str(data).expect("request payload"))
            }
            _ => None,
        })
        .collect()
}

fn prefix_s() -> Vec<RawInputEvent> {
    vec![
        key(KeyCode::Char('b'), KeyModifiers::CONTROL),
        key(KeyCode::Char('s'), KeyModifiers::NONE),
    ]
}

fn settings_section(state: &ClientShellState) -> ClientSettingsSection {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::Settings(settings)) => settings.section,
        other => panic!("settings closed: {other:?}"),
    }
}

/// The settings Host section state.
fn overlay(state: &ClientShellState) -> &host_facts_panel::ClientHostFactsOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
            host: Some(overlay),
            ..
        })) => overlay,
        other => panic!("settings host section missing: {other:?}"),
    }
}

fn success(id: &str) -> String {
    serde_json::to_string(&crate::api::schema::SuccessResponse {
        id: id.into(),
        result: crate::api::schema::ResponseResult::HostFacts(Box::new(sample_facts())),
    })
    .unwrap()
}

fn error(id: &str, code: &str, message: &str) -> String {
    crate::server::client_commands::error_response(id.into(), code, message)
}

fn composed_text(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    let frame = state.compose(cols, rows).expect("frame");
    frame_rows(&frame).join("\n")
}

#[test]
fn host_panel_opens_with_one_async_request_and_shows_loading() {
    let mut state = host_state(true);
    let outcome = state.handle_raw_events(prefix_i());
    assert!(outcome.repaint);
    assert_eq!(settings_section(&state), ClientSettingsSection::Host);
    // The only message is the fire-and-forget request; nothing reaches the pane
    // and the shell does not wait for the reply.
    assert_eq!(outcome.requests.len(), 1, "{:?}", outcome.requests);
    let requests = host_requests(&outcome);
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].refresh);
    let pending = overlay(&state).pending.as_ref().expect("pending request");
    assert_eq!(pending.id, requests[0].id);
    assert!(overlay(&state).facts.is_none());

    let text = composed_text(&mut state, 100, 40);
    assert!(text.contains("loading…"), "{text}");

    assert!(state.receive_host_facts(&ClientEndpointId::Local, &success(&requests[0].id)));
    assert!(overlay(&state).pending.is_none());
    let text = composed_text(&mut state, 100, 40);
    assert!(text.contains(" Host  ·  refreshed "), "{text}");
    assert!(text.contains("every 60s"), "{text}");
    assert!(text.contains("docker   running"), "{text}");
}

#[test]
fn reopening_while_a_request_runs_reuses_it_and_accepts_its_reply() {
    let mut state = host_state(true);
    let opened = state.handle_raw_events(prefix_i());
    let first = host_requests(&opened).remove(0);
    state.handle_raw_events(vec![key(KeyCode::Esc, KeyModifiers::NONE)]);
    assert!(state.overlay.is_none());

    let reopened = state.handle_raw_events(prefix_i());
    assert!(
        host_requests(&reopened).is_empty(),
        "no second request that would be answered host_facts_busy: {:?}",
        reopened.requests
    );
    assert_eq!(overlay(&state).pending.as_ref().unwrap().id, first.id);
    assert!(state.receive_host_facts(&ClientEndpointId::Local, &success(&first.id)));
    assert!(overlay(&state).pending.is_none());
    assert!(overlay(&state).facts.is_some());

    // Once answered (even while closed), the next open sends a fresh request.
    state.handle_raw_events(vec![key(KeyCode::Esc, KeyModifiers::NONE)]);
    let again = state.handle_raw_events(prefix_i());
    let second = host_requests(&again).remove(0);
    assert_ne!(second.id, first.id);
    state.handle_raw_events(vec![key(KeyCode::Esc, KeyModifiers::NONE)]);
    assert!(!state.receive_host_facts(&ClientEndpointId::Local, &success(&second.id)));
    let third = state.handle_raw_events(prefix_i());
    assert_eq!(
        host_requests(&third).len(),
        1,
        "a reply while closed frees the slot"
    );
}

#[test]
fn settings_lists_a_host_section_that_requests_once_when_selected() {
    let mut state = host_state(true);
    let opened = state.handle_raw_events(prefix_s());
    assert!(
        host_requests(&opened).is_empty(),
        "no request until selected"
    );
    assert_eq!(settings_section(&state), ClientSettingsSection::Theme);
    let text = composed_text(&mut state, 100, 40);
    assert!(text.contains(" host "), "{text}");
    assert!(state
        .hits
        .settings_tabs
        .iter()
        .any(|(_, section)| *section == ClientSettingsSection::Host));

    // Host is the last section, so shift+tab from the first reaches it.
    let selected = state.handle_raw_events(vec![key(KeyCode::BackTab, KeyModifiers::SHIFT)]);
    assert_eq!(settings_section(&state), ClientSettingsSection::Host);
    let requests = host_requests(&selected);
    assert_eq!(requests.len(), 1, "{:?}", selected.requests);
    assert!(!requests[0].refresh);

    // Section keys still move away and back; coming back keeps the state.
    let away = state.handle_raw_events(vec![key(KeyCode::Tab, KeyModifiers::NONE)]);
    assert_eq!(settings_section(&state), ClientSettingsSection::Theme);
    assert!(host_requests(&away).is_empty());
    let back = state.handle_raw_events(vec![key(KeyCode::Char('h'), KeyModifiers::NONE)]);
    assert_eq!(settings_section(&state), ClientSettingsSection::Host);
    assert!(host_requests(&back).is_empty(), "{:?}", back.requests);
    assert_eq!(overlay(&state).pending.as_ref().unwrap().id, requests[0].id);

    // The reply lands while the section shows and renders inside settings.
    assert!(state.receive_host_facts(&ClientEndpointId::Local, &success(&requests[0].id)));
    let text = composed_text(&mut state, 100, 40);
    assert!(text.contains(" settings"), "{text}");
    assert!(text.contains("docker   running"), "{text}");
    let left = state.handle_raw_events(vec![key(KeyCode::Char('l'), KeyModifiers::NONE)]);
    assert_eq!(settings_section(&state), ClientSettingsSection::Theme);
    assert!(host_requests(&left).is_empty());
}

#[test]
fn host_section_renders_summary_and_tools_inside_settings_at_100_and_60() {
    for (cols, rows) in [(100, 40), (60, 40)] {
        let mut state = host_state(true);
        let outcome = state.handle_raw_events(prefix_i());
        let request = host_requests(&outcome).remove(0);
        assert!(state.receive_host_facts(&ClientEndpointId::Local, &success(&request.id)));
        let text = composed_text(&mut state, cols, rows);
        println!("{cols}x{rows}:\n{text}");
        for needle in [
            " settings",
            " host ",
            " Host  ·  refreshed ",
            "os       Windows 11",
            "memory   16-32 GB",
            "docker   running",
            "wsl      present: Ubuntu",
            " rg ",
            "missing ",
            "failed ",
            "shadowed ",
            "r refresh · tab section",
            "esc close",
        ] {
            assert!(text.contains(needle), "{cols}: missing {needle:?}:\n{text}");
        }
        assert!(!text.contains("↵ apply"), "read-only section: {text}");
    }
}

#[test]
fn host_section_closes_with_esc_and_enter_does_nothing() {
    let mut state = host_state(true);
    let opened = state.handle_raw_events(prefix_i());
    let request = host_requests(&opened).remove(0);
    state.receive_host_facts(&ClientEndpointId::Local, &success(&request.id));
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter, KeyModifiers::NONE)]);
    assert!(outcome.requests.is_empty(), "{:?}", outcome.requests);
    assert_eq!(settings_section(&state), ClientSettingsSection::Host);

    let outcome = state.handle_raw_events(vec![key(KeyCode::Esc, KeyModifiers::NONE)]);
    assert!(outcome.repaint);
    assert!(state.overlay.is_none());
    assert!(outcome.requests.is_empty());
}

#[test]
fn theme_picker_has_no_host_section() {
    let mut state = host_state(true);
    let outcome = state.handle_raw_events(vec![
        key(KeyCode::Char('b'), KeyModifiers::CONTROL),
        key(KeyCode::Char('t'), KeyModifiers::NONE),
    ]);
    assert!(host_requests(&outcome).is_empty());
    let outcome = state.handle_raw_events(vec![key(KeyCode::BackTab, KeyModifiers::SHIFT)]);
    assert!(host_requests(&outcome).is_empty());
    assert_eq!(settings_section(&state), ClientSettingsSection::Theme);
    let text = composed_text(&mut state, 100, 40);
    assert!(!text.contains(" host "), "{text}");
}

#[test]
fn host_panel_r_sends_exactly_one_refresh_until_the_reply() {
    let mut state = host_state(true);
    let opened = state.handle_raw_events(prefix_i());
    let first = host_requests(&opened).remove(0);
    state.receive_host_facts(&ClientEndpointId::Local, &success(&first.id));

    let outcome = state.handle_input_bytes(b"r");
    let refresh = host_requests(&outcome);
    assert_eq!(refresh.len(), 1, "{:?}", outcome.requests);
    assert!(refresh[0].refresh);
    assert_ne!(refresh[0].id, first.id);
    let text = composed_text(&mut state, 100, 40);
    assert!(text.contains("refreshing…"), "{text}");

    // A second r while the refresh runs sends nothing.
    let outcome = state.handle_input_bytes(b"r");
    assert!(host_requests(&outcome).is_empty(), "{:?}", outcome.requests);

    // A late reply to the first request does not end the refresh.
    assert!(!state.receive_host_facts(&ClientEndpointId::Local, &success(&first.id)));
    assert!(overlay(&state).pending.is_some());

    assert!(state.receive_host_facts(&ClientEndpointId::Local, &success(&refresh[0].id)));
    let text = composed_text(&mut state, 100, 40);
    assert!(!text.contains("refreshing…"), "{text}");
    assert!(text.contains("r refresh · tab section"), "{text}");
}

#[test]
fn host_panel_shows_unavailable_reasons() {
    // Older servers do not advertise the capability: no request is sent.
    let mut state = host_state(false);
    let outcome = state.handle_raw_events(prefix_i());
    assert!(host_requests(&outcome).is_empty());
    let text = composed_text(&mut state, 100, 40);
    assert!(
        text.contains("host facts unavailable (not supported by this server)"),
        "{text}"
    );

    // Host facts disabled in the server config.
    let mut state = host_state(true);
    let outcome = state.handle_raw_events(prefix_i());
    let request = host_requests(&outcome).remove(0);
    assert!(state.receive_host_facts(
        &ClientEndpointId::Local,
        &error(
            &request.id,
            "host_facts_disabled",
            "host facts are disabled"
        ),
    ));
    let text = composed_text(&mut state, 60, 30);
    assert!(text.contains("host facts unavailable (disabled;"), "{text}");
}

#[test]
fn host_panel_ignores_replies_for_other_endpoints_and_times_out() {
    let mut state = host_state(true);
    let outcome = state.handle_raw_events(prefix_i());
    let request = host_requests(&outcome).remove(0);
    let remote = ClientEndpointId::Ssh(
        crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
    );
    assert!(!state.receive_host_facts(&remote, &success(&request.id)));
    assert!(overlay(&state).pending.is_some());

    let deadline = overlay(&state).pending.as_ref().unwrap().deadline;
    assert!(!state.tick_host_facts(deadline - std::time::Duration::from_secs(1)));
    assert!(state.tick_host_facts(deadline));
    let text = composed_text(&mut state, 100, 40);
    assert!(
        text.contains("host facts unavailable (no reply from server)"),
        "{text}"
    );
}

#[test]
fn host_panel_binding_is_listed_in_help_and_defaults() {
    let config = Config::default();
    assert_eq!(
        config.keys.host_facts,
        crate::config::BindingConfig::one("prefix+i")
    );
    let groups = crate::input::keybind_help_groups(&config.keybinds(), &[]);
    assert!(
        groups
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .any(|(key, label)| label == "settings: host facts" && key == "prefix+i"),
        "{groups:?}"
    );
    assert!(crate::DEFAULT_CONFIG.contains("# host_facts = \"prefix+i\""));
}

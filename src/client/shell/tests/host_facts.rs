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

fn overlay(state: &ClientShellState) -> &host_facts_panel::ClientHostFactsOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::HostFacts(overlay)) => overlay,
        other => panic!("host panel closed: {other:?}"),
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
fn host_panel_closes_with_esc_or_its_own_key() {
    let mut state = host_state(true);
    state.handle_raw_events(prefix_i());
    let outcome = state.handle_raw_events(vec![key(KeyCode::Esc, KeyModifiers::NONE)]);
    assert!(outcome.repaint);
    assert!(state.overlay.is_none());
    assert!(outcome.requests.is_empty());

    state.handle_raw_events(prefix_i());
    let outcome = state.handle_raw_events(vec![key(KeyCode::Char('i'), KeyModifiers::NONE)]);
    assert!(state.overlay.is_none(), "the bare key closes the panel");
    assert!(outcome.requests.is_empty());

    state.handle_raw_events(prefix_i());
    let outcome = state.handle_raw_events(prefix_i());
    assert!(state.overlay.is_none(), "the full chord closes the panel");
    assert!(outcome.requests.is_empty(), "{:?}", outcome.requests);
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
    assert!(text.contains("r refresh · Esc close"), "{text}");
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
            .any(|(key, label)| label == "host facts" && key == "prefix+i"),
        "{groups:?}"
    );
    assert!(crate::DEFAULT_CONFIG.contains("# host_facts = \"prefix+i\""));
}

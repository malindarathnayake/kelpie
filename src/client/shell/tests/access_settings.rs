//! Fork: Settings -> access toggles the supervisor and server access switches.

use super::*;

fn temp_config(name: &str, content: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "kelpie-access-settings-{name}-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, content).unwrap();
    path
}

fn access_state(path: &std::path::Path) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.config.local_config_path = path.to_path_buf();
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_settings_overlay();
    let mut outcome = ClientShellInput::default();
    state.select_settings_section(ClientSettingsSection::Access, &mut outcome);
    state
}

fn frame_text(state: &mut ClientShellState) -> String {
    let frame = state.compose(106, 40).unwrap();
    frame_rows(&frame).join("\n")
}

#[test]
fn access_section_lists_switches_with_their_current_values() {
    let path = temp_config("list", "[supervisor]\nenabled = true\n");
    let mut state = access_state(&path);
    if let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_mut() {
        settings.access.supervisor = true;
    }
    let text = frame_text(&mut state);
    let _ = std::fs::remove_file(&path);

    assert!(text.contains("supervisor  on"), "{text}");
    assert!(
        text.contains("applies after `kelpie server stop`"),
        "{text}"
    );
    if cfg!(windows) {
        assert!(
            text.contains("sandboxed tools (low integrity)  off"),
            "{text}"
        );
        assert!(
            text.contains("ordinary clients to an elevated server  off"),
            "{text}"
        );
    }
}

#[test]
fn enter_toggles_the_selected_switch_and_keeps_the_rest_of_the_file() {
    let original = "# keep me\n[ui]\nsidebar_width = 31\n";
    let path = temp_config("toggle", original);
    let mut state = access_state(&path);

    let outcome = state.handle_input_bytes(b"\r");
    let written = std::fs::read_to_string(&path).unwrap();

    assert!(written.starts_with(original), "{written}");
    assert!(
        written.contains("[supervisor]\nenabled = true"),
        "{written}"
    );
    assert!(state.config.access.supervisor);
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                access: crate::config::AccessSettings {
                    supervisor: true,
                    ..
                },
                ..
            }))
        ),
        "the open section shows the new value"
    );
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, crate::api::schema::Method::ServerReloadConfig(_))
        )),
        "{:?}",
        outcome.actions
    );

    // Second Enter turns it back off.
    state.handle_input_bytes(b"\r");
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("[supervisor]\nenabled = false"),
        "{written}"
    );

    if cfg!(windows) {
        state.handle_input_bytes(b"j");
        state.handle_input_bytes(b"\r");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("allow_low_integrity_clients = true"),
            "{written}"
        );
        assert!(state.config.access.low_integrity_clients);
    }
    let _ = std::fs::remove_file(&path);
}

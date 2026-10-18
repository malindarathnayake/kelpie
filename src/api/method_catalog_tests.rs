//! Catalog checks that keep the API method name table, the UI-change
//! classification and the client shell allowlist in step with `Method`.

use std::collections::HashSet;

use super::schema::{Method, Request};
use super::{api_method_name, request_changes_ui};

/// One minimal request per `Method` variant, as (wire method, params JSON).
const SAMPLES: &[(&str, &str)] = &[
    ("ping", r#"{}"#),
    ("server.stop", r#"{}"#),
    ("server.live_handoff", r#"{}"#),
    ("server.reload_config", r#"{}"#),
    (
        "server.ssh_agent.register",
        r#"{"socket_path":"/tmp/agent.sock"}"#,
    ),
    ("server.agent_manifests", r#"{}"#),
    ("server.reload_agent_manifests", r#"{}"#),
    ("host.facts", r#"{}"#),
    ("notification.show", r#"{"title":"t"}"#),
    (
        "product_announcement.dismiss",
        r#"{"version":"1.0.0","id":"a"}"#,
    ),
    ("release_notes.dismiss", r#"{"version":"1.0.0"}"#),
    ("command.invoke", r#"{"command_id":"c"}"#),
    ("client.window_title.set", r#"{"title":"t"}"#),
    ("client.window_title.clear", r#"{}"#),
    ("client_shell.surface.set", r#"{"active":true}"#),
    ("session.snapshot", r#"{}"#),
    ("workspace.create", r#"{}"#),
    ("workspace.list", r#"{}"#),
    ("workspace.get", r#"{"workspace_id":"w1"}"#),
    ("workspace.focus", r#"{"workspace_id":"w1"}"#),
    ("workspace.rename", r#"{"workspace_id":"w1","label":"x"}"#),
    (
        "workspace.move",
        r#"{"workspace_id":"w1","insert_index":0}"#,
    ),
    ("workspace.move_block", r#"{"workspace_ids":["w1"]}"#),
    (
        "workspace.report_metadata",
        r#"{"workspace_id":"w1","source":"s","tokens":{}}"#,
    ),
    ("workspace.close", r#"{"workspace_id":"w1"}"#),
    ("worktree.list", r#"{}"#),
    ("worktree.create", r#"{}"#),
    ("worktree.open", r#"{}"#),
    ("worktree.remove", r#"{"workspace_id":"w1"}"#),
    ("tab.create", r#"{}"#),
    ("tab.list", r#"{}"#),
    ("tab.get", r#"{"tab_id":"t1"}"#),
    ("tab.focus", r#"{"tab_id":"t1"}"#),
    ("tab.rename", r#"{"tab_id":"t1","label":"x"}"#),
    ("tab.move", r#"{"tab_id":"t1","insert_index":0}"#),
    ("tab.close", r#"{"tab_id":"t1"}"#),
    ("agent.list", r#"{}"#),
    ("agents.digest", r#"{}"#),
    ("agent.get", r#"{"target":"a1"}"#),
    ("agent.read", r#"{"target":"a1","source":"visible"}"#),
    ("agent.explain", r#"{"target":"a1"}"#),
    ("agent.rename", r#"{"target":"a1"}"#),
    ("agent.view.set", r#"{"source":"s"}"#),
    ("agent.view.clear", r#"{}"#),
    ("agent.focus", r#"{"target":"a1"}"#),
    (
        "agent.start",
        r#"{"name":"a","kind":"claude","pane_id":"p1"}"#,
    ),
    ("agent.wait", r#"{"target":"a1"}"#),
    (
        "agent.submit",
        r#"{"target_ref":{"server_epoch":"e1","pane_id":"p1","generation":1},"text":"hi","submit_id":"s1"}"#,
    ),
    (
        "agent.answer",
        r#"{"target_ref":{"server_epoch":"e1","pane_id":"p1","generation":1},"question_id":"q1","text":"hi","submit_id":"s1"}"#,
    ),
    (
        "agent.interrupt",
        r#"{"target_ref":{"server_epoch":"e1","pane_id":"p1","generation":1},"submit_id":"s1"}"#,
    ),
    (
        "agent.reset",
        r#"{"target_ref":{"server_epoch":"e1","pane_id":"p1","generation":1},"submit_id":"s1"}"#,
    ),
    (
        "agent.clear_draft",
        r#"{"target_ref":{"server_epoch":"e1","pane_id":"p1","generation":1},"submit_id":"s1"}"#,
    ),
    (
        "agent.resume",
        r#"{"pane_id":"p1","session_id":"s1","submit_id":"s1"}"#,
    ),
    ("agent.submit_status", r#"{"submit_id":"s1"}"#),
    ("agent.undelegate", r#"{"pane_id":"p1"}"#),
    (
        "pane.report_prompt_submitted",
        r#"{"pane_id":"p1","source":"s","agent":"claude","prompt_sha256_16":"0123456789abcdef"}"#,
    ),
    ("pane.split", r#"{"direction":"right"}"#),
    ("pane.swap", r#"{}"#),
    (
        "pane.move",
        r#"{"pane_id":"p1","destination":{"type":"new_tab"}}"#,
    ),
    ("pane.zoom", r#"{}"#),
    ("pane.layout", r#"{}"#),
    ("pane.process_info", r#"{}"#),
    ("layout.export", r#"{}"#),
    ("layout.apply", r#"{"root":{"type":"pane"}}"#),
    ("layout.set_split_ratio", r#"{"path":[],"ratio":0.5}"#),
    ("pane.neighbor", r#"{"direction":"right"}"#),
    ("pane.edges", r#"{}"#),
    ("pane.focus_direction", r#"{"direction":"right"}"#),
    ("pane.resize", r#"{"direction":"right"}"#),
    ("pane.scroll", r#"{"pane_id":"p1","offset_from_bottom":0}"#),
    ("pane.clear", r#"{"pane_id":"p1"}"#),
    ("pane.edit_scrollback", r#"{"pane_id":"p1"}"#),
    (
        "pane.selection.read",
        r#"{"pane_id":"p1","anchor":{"row":0,"col":0},"cursor":{"row":0,"col":0}}"#,
    ),
    (
        "pane.copy_motion",
        r#"{"pane_id":"p1","cursor":{"row":0,"col":0},"motion":"line_end"}"#,
    ),
    (
        "pane.copy_search",
        r#"{"pane_id":"p1","cursor":{"row":0,"col":0},"query":"q","direction":"forward","content_revision":0}"#,
    ),
    ("pane.list", r#"{}"#),
    ("pane.current", r#"{}"#),
    ("pane.get", r#"{"pane_id":"p1"}"#),
    ("pane.focus", r#"{"pane_id":"p1"}"#),
    ("pane.input.set", r#"{"pane_id":"p1","right_click":"pane"}"#),
    (
        "pane.link.activate",
        r#"{"pane_id":"p1","viewport_row":0,"col":0}"#,
    ),
    (
        "pane.link.resolve",
        r#"{"pane_id":"p1","viewport_row":0,"col":0}"#,
    ),
    ("pane.rename", r#"{"pane_id":"p1"}"#),
    ("pane.send_text", r#"{"pane_id":"p1","text":"hi"}"#),
    ("pane.send_keys", r#"{"pane_id":"p1","keys":["Enter"]}"#),
    ("pane.send_input", r#"{"pane_id":"p1","keys":["Enter"]}"#),
    ("pane.read", r#"{"pane_id":"p1","source":"visible"}"#),
    (
        "pane.report_agent",
        r#"{"pane_id":"p1","source":"s","agent":"claude","state":"idle"}"#,
    ),
    (
        "pane.report_agent_session",
        r#"{"pane_id":"p1","source":"s","agent":"claude","kind":"id","value":"v"}"#,
    ),
    ("pane.report_metadata", r#"{"pane_id":"p1","source":"s"}"#),
    ("pane.clear_agent_authority", r#"{"pane_id":"p1"}"#),
    (
        "pane.release_agent",
        r#"{"pane_id":"p1","source":"s","agent":"claude"}"#,
    ),
    ("pane.close", r#"{"pane_id":"p1"}"#),
    ("popup.close", r#"{}"#),
    ("events.subscribe", r#"{"subscriptions":[]}"#),
    (
        "events.wait",
        r#"{"match_event":{"event":"workspace_created"}}"#,
    ),
    (
        "pane.wait_for_output",
        r#"{"pane_id":"p1","source":"visible","match":{"type":"substring","value":"x"}}"#,
    ),
    ("integration.list", r#"{}"#),
    ("integration.install", r#"{"target":"claude"}"#),
    ("integration.uninstall", r#"{"target":"claude"}"#),
    ("plugin.link", r#"{"path":"/tmp/plugin"}"#),
    ("plugin.list", r#"{}"#),
    ("plugin.unlink", r#"{"plugin_id":"x"}"#),
    ("plugin.enable", r#"{"plugin_id":"x"}"#),
    ("plugin.disable", r#"{"plugin_id":"x"}"#),
    ("plugin.action.list", r#"{}"#),
    ("plugin.action.invoke", r#"{"action_id":"x"}"#),
    ("plugin.log.list", r#"{}"#),
    ("plugin.pane.open", r#"{"plugin_id":"x","entrypoint":"e"}"#),
    ("plugin.pane.focus", r#"{"pane_id":"p1"}"#),
    ("plugin.pane.close", r#"{"pane_id":"p1"}"#),
];

/// Methods that `request_changes_ui` must report as changing the UI.
const CHANGES_UI: &[&str] = &[
    "server.reload_config",
    "server.reload_agent_manifests",
    "notification.show",
    "product_announcement.dismiss",
    "release_notes.dismiss",
    "command.invoke",
    "workspace.create",
    "workspace.focus",
    "workspace.rename",
    "workspace.move",
    "workspace.move_block",
    "workspace.report_metadata",
    "workspace.close",
    "worktree.create",
    "worktree.open",
    "worktree.remove",
    "tab.create",
    "tab.focus",
    "tab.rename",
    "tab.move",
    "tab.close",
    "agent.rename",
    "agent.view.set",
    "agent.view.clear",
    "agent.focus",
    "agent.start",
    "agent.submit",
    "agent.answer",
    "agent.interrupt",
    "agent.reset",
    "agent.clear_draft",
    "agent.resume",
    "agent.undelegate",
    "pane.split",
    "pane.swap",
    "pane.move",
    "pane.zoom",
    "layout.apply",
    "layout.set_split_ratio",
    "pane.focus_direction",
    "pane.resize",
    "pane.scroll",
    "pane.clear",
    "pane.edit_scrollback",
    "pane.focus",
    "pane.input.set",
    "pane.rename",
    "pane.report_agent",
    "pane.report_agent_session",
    "pane.report_metadata",
    "pane.clear_agent_authority",
    "pane.release_agent",
    "pane.close",
    "popup.close",
    "plugin.unlink",
    "plugin.disable",
    "plugin.action.invoke",
    "plugin.pane.open",
    "plugin.pane.focus",
    "plugin.pane.close",
];

/// Methods that `request_changes_ui` must report as leaving the UI alone.
const NO_UI_CHANGE: &[&str] = &[
    "ping",
    "server.stop",
    "server.live_handoff",
    "server.ssh_agent.register",
    "server.agent_manifests",
    "host.facts",
    "client.window_title.set",
    "client.window_title.clear",
    "client_shell.surface.set",
    "session.snapshot",
    "workspace.list",
    "workspace.get",
    "worktree.list",
    "tab.list",
    "tab.get",
    "agent.list",
    "agents.digest",
    "agent.get",
    "agent.read",
    "agent.explain",
    "agent.wait",
    "agent.submit_status",
    "pane.report_prompt_submitted",
    "pane.layout",
    "pane.process_info",
    "layout.export",
    "pane.neighbor",
    "pane.edges",
    "pane.selection.read",
    "pane.copy_motion",
    "pane.copy_search",
    "pane.list",
    "pane.current",
    "pane.get",
    "pane.link.activate",
    "pane.link.resolve",
    "pane.send_text",
    "pane.send_keys",
    "pane.send_input",
    "pane.read",
    "events.subscribe",
    "events.wait",
    "pane.wait_for_output",
    "integration.list",
    "integration.install",
    "integration.uninstall",
    "plugin.link",
    "plugin.list",
    "plugin.enable",
    "plugin.action.list",
    "plugin.log.list",
];

/// Number of `Method` variants; `variant_ordinal` returns 0..VARIANT_COUNT.
const VARIANT_COUNT: usize = 111;

/// Returns the declaration position of a variant. The match is exhaustive on
/// purpose: adding a `Method` variant fails to compile here until the variant
/// gets an ordinal, a sample and a UI classification above.
fn variant_ordinal(method: &Method) -> usize {
    match method {
        Method::Ping(_) => 0,
        Method::ServerStop(_) => 1,
        Method::ServerLiveHandoff(_) => 2,
        Method::ServerReloadConfig(_) => 3,
        Method::ServerSshAgentRegister(_) => 4,
        Method::ServerAgentManifests(_) => 5,
        Method::ServerReloadAgentManifests(_) => 6,
        Method::HostFacts(_) => 7,
        Method::NotificationShow(_) => 8,
        Method::ProductAnnouncementDismiss(_) => 9,
        Method::ReleaseNotesDismiss(_) => 10,
        Method::CommandInvoke(_) => 11,
        Method::ClientWindowTitleSet(_) => 12,
        Method::ClientWindowTitleClear(_) => 13,
        Method::ClientShellSurfaceSet(_) => 14,
        Method::SessionSnapshot(_) => 15,
        Method::WorkspaceCreate(_) => 16,
        Method::WorkspaceList(_) => 17,
        Method::WorkspaceGet(_) => 18,
        Method::WorkspaceFocus(_) => 19,
        Method::WorkspaceRename(_) => 20,
        Method::WorkspaceMove(_) => 21,
        Method::WorkspaceMoveBlock(_) => 22,
        Method::WorkspaceReportMetadata(_) => 23,
        Method::WorkspaceClose(_) => 24,
        Method::WorktreeList(_) => 25,
        Method::WorktreeCreate(_) => 26,
        Method::WorktreeOpen(_) => 27,
        Method::WorktreeRemove(_) => 28,
        Method::TabCreate(_) => 29,
        Method::TabList(_) => 30,
        Method::TabGet(_) => 31,
        Method::TabFocus(_) => 32,
        Method::TabRename(_) => 33,
        Method::TabMove(_) => 34,
        Method::TabClose(_) => 35,
        Method::AgentList(_) => 36,
        Method::AgentsDigest(_) => 37,
        Method::AgentGet(_) => 38,
        Method::AgentRead(_) => 39,
        Method::AgentExplain(_) => 40,
        Method::AgentRename(_) => 41,
        Method::AgentViewSet(_) => 42,
        Method::AgentViewClear(_) => 43,
        Method::AgentFocus(_) => 44,
        Method::AgentStart(_) => 45,
        Method::AgentWait(_) => 46,
        Method::AgentSubmit(_) => 47,
        Method::AgentAnswer(_) => 48,
        Method::AgentInterrupt(_) => 49,
        Method::AgentReset(_) => 50,
        Method::AgentClearDraft(_) => 51,
        Method::AgentResume(_) => 52,
        Method::AgentSubmitStatus(_) => 53,
        Method::AgentUndelegate(_) => 54,
        Method::PaneReportPromptSubmitted(_) => 55,
        Method::PaneSplit(_) => 56,
        Method::PaneSwap(_) => 57,
        Method::PaneMove(_) => 58,
        Method::PaneZoom(_) => 59,
        Method::PaneLayout(_) => 60,
        Method::PaneProcessInfo(_) => 61,
        Method::LayoutExport(_) => 62,
        Method::LayoutApply(_) => 63,
        Method::LayoutSetSplitRatio(_) => 64,
        Method::PaneNeighbor(_) => 65,
        Method::PaneEdges(_) => 66,
        Method::PaneFocusDirection(_) => 67,
        Method::PaneResize(_) => 68,
        Method::PaneScroll(_) => 69,
        Method::PaneClear(_) => 70,
        Method::PaneEditScrollback(_) => 71,
        Method::PaneSelectionRead(_) => 72,
        Method::PaneCopyMotion(_) => 73,
        Method::PaneCopySearch(_) => 74,
        Method::PaneList(_) => 75,
        Method::PaneCurrent(_) => 76,
        Method::PaneGet(_) => 77,
        Method::PaneFocus(_) => 78,
        Method::PaneInputSet(_) => 79,
        Method::PaneLinkActivate(_) => 80,
        Method::PaneLinkResolve(_) => 81,
        Method::PaneRename(_) => 82,
        Method::PaneSendText(_) => 83,
        Method::PaneSendKeys(_) => 84,
        Method::PaneSendInput(_) => 85,
        Method::PaneRead(_) => 86,
        Method::PaneReportAgent(_) => 87,
        Method::PaneReportAgentSession(_) => 88,
        Method::PaneReportMetadata(_) => 89,
        Method::PaneClearAgentAuthority(_) => 90,
        Method::PaneReleaseAgent(_) => 91,
        Method::PaneClose(_) => 92,
        Method::PopupClose(_) => 93,
        Method::EventsSubscribe(_) => 94,
        Method::EventsWait(_) => 95,
        Method::PaneWaitForOutput(_) => 96,
        Method::IntegrationList(_) => 97,
        Method::IntegrationInstall(_) => 98,
        Method::IntegrationUninstall(_) => 99,
        Method::PluginLink(_) => 100,
        Method::PluginList(_) => 101,
        Method::PluginUnlink(_) => 102,
        Method::PluginEnable(_) => 103,
        Method::PluginDisable(_) => 104,
        Method::PluginActionList(_) => 105,
        Method::PluginActionInvoke(_) => 106,
        Method::PluginLogList(_) => 107,
        Method::PluginPaneOpen(_) => 108,
        Method::PluginPaneFocus(_) => 109,
        Method::PluginPaneClose(_) => 110,
    }
}

fn samples() -> Vec<(&'static str, Method)> {
    SAMPLES
        .iter()
        .map(|(name, params)| {
            let json = format!(r#"{{"method":"{name}","params":{params}}}"#);
            let method = serde_json::from_str::<Method>(&json)
                .unwrap_or_else(|err| panic!("sample for {name} does not parse: {err}"));
            (*name, method)
        })
        .collect()
}

#[test]
fn samples_cover_every_method_variant_once() {
    let mut seen = vec![None; VARIANT_COUNT];
    for (name, method) in samples() {
        let ordinal = variant_ordinal(&method);
        assert!(
            ordinal < VARIANT_COUNT,
            "{name} ordinal {ordinal} out of range"
        );
        if let Some(previous) = seen[ordinal].replace(name) {
            panic!("{name} and {previous} sample the same variant");
        }
    }
    let missing: Vec<usize> = (0..VARIANT_COUNT).filter(|i| seen[*i].is_none()).collect();
    assert!(missing.is_empty(), "variants without a sample: {missing:?}");
}

#[test]
fn api_method_name_matches_serde_method_tag() {
    for (name, method) in samples() {
        let value = serde_json::to_value(&method).unwrap();
        let tag = value["method"].as_str().unwrap();
        assert_eq!(tag, name, "sample {name} serialized under another tag");
        assert_eq!(api_method_name(&method), tag);
    }
}

#[test]
fn request_changes_ui_matches_catalog() {
    let changes: HashSet<&str> = CHANGES_UI.iter().copied().collect();
    let unchanged: HashSet<&str> = NO_UI_CHANGE.iter().copied().collect();
    assert_eq!(changes.len(), CHANGES_UI.len(), "CHANGES_UI has duplicates");
    assert_eq!(
        unchanged.len(),
        NO_UI_CHANGE.len(),
        "NO_UI_CHANGE has duplicates"
    );
    let both: Vec<&&str> = changes.intersection(&unchanged).collect();
    assert!(both.is_empty(), "methods in both lists: {both:?}");

    let all: HashSet<&str> = SAMPLES.iter().map(|(name, _)| *name).collect();
    let listed: HashSet<&str> = changes.union(&unchanged).copied().collect();
    assert_eq!(listed, all, "UI lists must name every method exactly once");

    for (name, method) in samples() {
        let request = Request {
            id: "catalog".into(),
            method,
        };
        assert_eq!(
            request_changes_ui(&request),
            changes.contains(name),
            "request_changes_ui disagrees with the catalog for {name}"
        );
    }
}

#[test]
fn client_shell_methods_are_real_api_methods() {
    let names: HashSet<&str> = samples()
        .iter()
        .map(|(_, method)| api_method_name(method))
        .collect();
    for method in crate::server::client_commands::supported_client_shell_method_names() {
        assert!(
            names.contains(method),
            "client shell method {method} is not an API method"
        );
    }
}

use std::collections::HashSet;
use std::io;
use std::path::Path;

use jsonc_parser::ast::{Array as AstArray, Object as AstObject, Value as AstValue};
use jsonc_parser::common::Ranged;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{parse_to_ast, CollectOptions, ParseOptions};
use serde_json::{Map, Value};

use super::command::hook_command;
use super::config_edit::{
    ensure_hooks_object, hook_command_variants, hooks_object_if_present, is_matching_command_hook,
};

// Claude's documented SessionStart sources. Grok imports Claude hooks but uses
// `new`/`load`; filter before it starts an unnecessary hook process.
const SESSION_START_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";
const CANONICAL_HOOK_TIMEOUT: u64 = 10;

/// One hook group kelpie owns in Claude settings.
struct CanonicalHook {
    event: &'static str,
    matcher: Option<&'static str>,
    action: &'static str,
    /// Claude `async: true`: runs in the background and never delays the event.
    background: bool,
}

const CANONICAL_HOOKS: &[CanonicalHook] = &[
    CanonicalHook {
        event: "SessionStart",
        matcher: Some(SESSION_START_MATCHER),
        action: "session",
        background: false,
    },
    // Fork (B1): reports only a hash of each submitted prompt, so kelpie can confirm an
    // agent.submit receipt. Background so it never delays the owner's prompt.
    CanonicalHook {
        event: "UserPromptSubmit",
        matcher: None,
        action: "prompt",
        background: true,
    },
];

fn canonical_for(event: &str) -> Option<&'static CanonicalHook> {
    CANONICAL_HOOKS.iter().find(|hook| hook.event == event)
}

struct HookRemoval {
    event: &'static str,
    actions: &'static [&'static str],
}

const HOOK_REMOVALS: &[HookRemoval] = &[
    HookRemoval {
        event: "PostToolUse",
        actions: &["working"],
    },
    HookRemoval {
        event: "PostToolUseFailure",
        actions: &["working"],
    },
    HookRemoval {
        event: "SubagentStop",
        actions: &["working"],
    },
    HookRemoval {
        event: "PermissionRequest",
        actions: &["blocked"],
    },
    HookRemoval {
        event: "SessionStart",
        actions: &["idle", "session"],
    },
    HookRemoval {
        event: "UserPromptSubmit",
        actions: &["working", "prompt"],
    },
    HookRemoval {
        event: "PreToolUse",
        actions: &["working"],
    },
    HookRemoval {
        event: "Stop",
        actions: &["idle"],
    },
    HookRemoval {
        event: "SessionEnd",
        actions: &["release"],
    },
];

pub(crate) fn install(content: &str, settings_path: &Path, hook_path: &Path) -> io::Result<String> {
    let original = parse_value(content, settings_path)?;
    let mut desired = original.clone();
    let hooks = ensure_hooks_object(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )?;
    apply_value_removals(hooks, hook_path, true)?;
    for canonical in CANONICAL_HOOKS {
        ensure_canonical_hook(hooks, canonical, hook_path)?;
    }

    if desired == original {
        return Ok(content.to_string());
    }

    rewrite(
        content,
        settings_path,
        hook_path,
        EditKind::Install,
        &desired,
    )
}

pub(crate) fn uninstall(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
) -> io::Result<String> {
    let original = parse_value(content, settings_path)?;
    let mut desired = original.clone();
    let mut removed = false;

    if let Some(hooks) = hooks_object_if_present(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )? {
        removed = apply_value_removals(hooks, hook_path, false)?;
    }

    if !removed {
        return Ok(content.to_string());
    }

    rewrite(
        content,
        settings_path,
        hook_path,
        EditKind::Uninstall,
        &desired,
    )
}

fn apply_value_removals(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    installing: bool,
) -> io::Result<bool> {
    let mut removed = false;
    for policy in HOOK_REMOVALS {
        let commands = removal_commands(policy, hook_path);
        let canonical = canonical_for(policy.event)
            .filter(|_| installing)
            .map(|canonical| canonical_hook_value(canonical, hook_path));
        removed |= remove_value_event_commands(hooks, policy.event, &commands, canonical.as_ref())?;
    }
    Ok(removed)
}

/// Adds the canonical group unless an identical one is already present. Value-level
/// mirror of the text edit in `append_canonical_entry`.
fn ensure_canonical_hook(
    hooks: &mut Map<String, Value>,
    canonical: &CanonicalHook,
    hook_path: &Path,
) -> io::Result<()> {
    let value = canonical_hook_value(canonical, hook_path);
    let entries = hooks
        .entry(canonical.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::other(format!(
                "hook entries for {} must be an array",
                canonical.event
            ))
        })?;
    if !entries.contains(&value) {
        entries.push(value);
    }
    Ok(())
}

fn remove_value_event_commands(
    hooks: &mut Map<String, Value>,
    event: &str,
    commands: &[String],
    canonical: Option<&Value>,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };
    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    let mut removed = false;
    let mut canonical_preserved = false;

    entries.retain_mut(|entry| {
        if !canonical_preserved && canonical.is_some_and(|canonical| entry == canonical) {
            canonical_preserved = true;
            return true;
        }
        let Some(command_entries) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = command_entries.len();
        command_entries.retain(|entry| {
            !commands
                .iter()
                .any(|command| is_matching_command_hook(entry, command))
        });
        removed |= command_entries.len() != before;
        !command_entries.is_empty()
    });

    if entries.is_empty() && canonical.is_none() {
        hooks.remove(event);
    }
    Ok(removed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Install,
    Uninstall,
}

fn rewrite(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    kind: EditKind,
    desired: &Value,
) -> io::Result<String> {
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })?;
    let root_value = root.value().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    reject_duplicate_keys(&root_value, settings_path)?;
    let root_object = root_value.as_object().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let hooks = match root_object.get("hooks") {
        Some(property) => property.object_value().ok_or_else(|| {
            io::Error::other(format!(
                "claude settings hooks at {} must be a JSON object",
                settings_path.display()
            ))
        })?,
        None if kind == EditKind::Install
            && direct_children_are_compact(&root_object.children()) =>
        {
            let updated = append_hooks_property_compact(content, hook_path, settings_path)?;
            return verify_updated(updated, settings_path, desired);
        }
        None if kind == EditKind::Install => root_object
            .append("hooks", CstInputValue::Object(Vec::new()))
            .object_value()
            .ok_or_else(|| io::Error::other("failed to create claude settings hooks object"))?,
        None => return Ok(content.to_string()),
    };

    let mut preserved = HashSet::new();
    for policy in HOOK_REMOVALS {
        let commands = removal_commands(policy, hook_path);
        let canonical = canonical_for(policy.event)
            .filter(|_| kind == EditKind::Install)
            .map(|canonical| canonical_hook_value(canonical, hook_path));
        if remove_event_commands(&hooks, policy.event, &commands, canonical.as_ref())? {
            preserved.insert(policy.event);
        }
    }

    let mut updated = root.to_string();
    if kind == EditKind::Install {
        for canonical in CANONICAL_HOOKS
            .iter()
            .filter(|canonical| !preserved.contains(canonical.event))
        {
            updated = append_canonical_entry(&updated, canonical, hook_path, settings_path)?;
        }
    }
    verify_updated(updated, settings_path, desired)
}

/// Appends one canonical group to `content`, keeping compact containers compact.
fn append_canonical_entry(
    content: &str,
    canonical: &CanonicalHook,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })?;
    let hooks = root
        .value()
        .and_then(|value| value.as_object())
        .and_then(|object| object.get("hooks"))
        .and_then(|property| property.object_value())
        .ok_or_else(|| {
            io::Error::other(format!(
                "claude settings hooks at {} must be a JSON object",
                settings_path.display()
            ))
        })?;
    match hooks.get(canonical.event) {
        Some(property) => {
            let entries = property.array_value().ok_or_else(|| {
                io::Error::other(format!(
                    "hook entries for {} must be an array",
                    canonical.event
                ))
            })?;
            if direct_children_are_compact(&entries.children()) {
                return append_event_entry_compact(content, canonical, hook_path, settings_path);
            }
            entries.append(canonical_hook_input(canonical, hook_path));
        }
        None if direct_children_are_compact(&hooks.children()) => {
            return append_event_property_compact(content, canonical, hook_path, settings_path);
        }
        None => {
            let entries = hooks
                .append(canonical.event, CstInputValue::Array(Vec::new()))
                .array_value()
                .ok_or_else(|| {
                    io::Error::other(format!("failed to create {} hook array", canonical.event))
                })?;
            entries.append(canonical_hook_input(canonical, hook_path));
        }
    }
    Ok(root.to_string())
}

fn remove_event_commands(
    hooks: &CstObject,
    event: &str,
    commands: &[String],
    canonical: Option<&Value>,
) -> io::Result<bool> {
    let Some(event_property) = hooks.get(event) else {
        return Ok(false);
    };
    let entries = event_property
        .array_value()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    let mut canonical_preserved = false;

    for entry in entries.elements() {
        if !canonical_preserved
            && canonical.is_some()
            && entry.to_serde_value().as_ref() == canonical
        {
            canonical_preserved = true;
            continue;
        }

        let Some(entry_object) = entry.as_object() else {
            continue;
        };
        let Some(command_entries) = entry_object
            .get("hooks")
            .and_then(|property| property.array_value())
        else {
            continue;
        };

        for command_entry in command_entries.elements() {
            let matches = command_entry.to_serde_value().is_some_and(|value| {
                commands
                    .iter()
                    .any(|command| is_matching_command_hook(&value, command))
            });
            if matches {
                command_entry.remove();
            }
        }

        if command_entries.elements().is_empty() {
            entry.remove();
        }
    }

    if entries.elements().is_empty() && canonical.is_none() {
        event_property.remove();
    }

    Ok(canonical_preserved)
}

fn removal_commands(policy: &HookRemoval, hook_path: &Path) -> Vec<String> {
    policy
        .actions
        .iter()
        .flat_map(|action| hook_command_variants(hook_path, Some(action)))
        .collect()
}

fn canonical_hook_value(canonical: &CanonicalHook, hook_path: &Path) -> Value {
    let mut command = Map::new();
    command.insert("type".into(), Value::from("command"));
    command.insert(
        "command".into(),
        Value::from(hook_command(hook_path, Some(canonical.action))),
    );
    command.insert("timeout".into(), Value::from(CANONICAL_HOOK_TIMEOUT));
    if canonical.background {
        command.insert("async".into(), Value::from(true));
    }
    let mut group = Map::new();
    if let Some(matcher) = canonical.matcher {
        group.insert("matcher".into(), Value::from(matcher));
    }
    group.insert("hooks".into(), Value::Array(vec![Value::Object(command)]));
    Value::Object(group)
}

fn canonical_hook_input(canonical: &CanonicalHook, hook_path: &Path) -> CstInputValue {
    let mut command = vec![
        ("type".to_string(), CstInputValue::from("command")),
        (
            "command".to_string(),
            CstInputValue::from(hook_command(hook_path, Some(canonical.action))),
        ),
        (
            "timeout".to_string(),
            CstInputValue::from(CANONICAL_HOOK_TIMEOUT),
        ),
    ];
    if canonical.background {
        command.push(("async".to_string(), CstInputValue::from(true)));
    }
    let mut group = Vec::new();
    if let Some(matcher) = canonical.matcher {
        group.push(("matcher".to_string(), CstInputValue::from(matcher)));
    }
    group.push((
        "hooks".to_string(),
        CstInputValue::Array(vec![CstInputValue::Object(command)]),
    ));
    CstInputValue::Object(group)
}

fn append_hooks_property_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let mut events = Vec::with_capacity(CANONICAL_HOOKS.len());
    for canonical in CANONICAL_HOOKS {
        events.push(format!(
            "{}:[{}]",
            serde_json::to_string(canonical.event)?,
            canonical_hook_json(canonical, hook_path)?
        ));
    }
    let value = format!("{{{}}}", events.join(","));
    Ok(append_object_property(content, &root, "hooks", &value))
}

fn append_event_property_compact(
    content: &str,
    canonical: &CanonicalHook,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let hooks = root.get_object("hooks").ok_or_else(|| {
        io::Error::other(format!(
            "claude settings hooks at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    let value = format!("[{}]", canonical_hook_json(canonical, hook_path)?);
    Ok(append_object_property(
        content,
        hooks,
        canonical.event,
        &value,
    ))
}

fn append_event_entry_compact(
    content: &str,
    canonical: &CanonicalHook,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let entries = root
        .get_object("hooks")
        .and_then(|hooks| hooks.get_array(canonical.event))
        .ok_or_else(|| {
            io::Error::other(format!(
                "hook entries for {} must be an array",
                canonical.event
            ))
        })?;
    Ok(append_array_element(
        content,
        entries,
        &canonical_hook_json(canonical, hook_path)?,
    ))
}

fn parse_ast_root_object<'a>(content: &'a str, settings_path: &Path) -> io::Result<AstObject<'a>> {
    let parsed = parse_to_ast(content, &CollectOptions::default(), &strict_parse_options())
        .map_err(|err| {
            io::Error::other(format!(
                "failed to parse {}: {err}",
                settings_path.display()
            ))
        })?;
    match parsed.value {
        Some(AstValue::Object(object)) => Ok(object),
        _ => Err(io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))),
    }
}

fn append_object_property(
    content: &str,
    object: &AstObject<'_>,
    name: &str,
    value: &str,
) -> String {
    let key = serde_json::to_string(name).expect("JSON object keys are serializable");
    let key_value_separator = object
        .properties
        .first()
        .map(|property| &content[property.name.range().end..property.value.range().start])
        .unwrap_or(":");
    let insertion = format!("{key}{key_value_separator}{value}");
    let delimiter = object_delimiter(content, object);
    append_to_container(
        content,
        object.range,
        !object.properties.is_empty(),
        delimiter,
        &insertion,
    )
}

fn append_array_element(content: &str, array: &AstArray<'_>, value: &str) -> String {
    let delimiter = array_delimiter(content, array);
    append_to_container(
        content,
        array.range,
        !array.elements.is_empty(),
        delimiter,
        value,
    )
}

fn object_delimiter<'a>(content: &'a str, object: &AstObject<'_>) -> &'a str {
    match object.properties.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range.end..second.range.start]),
        [first] => &content[object.range.start + 1..first.range.start],
        [] => "",
    }
}

fn array_delimiter<'a>(content: &'a str, array: &AstArray<'_>) -> &'a str {
    match array.elements.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range().end..second.range().start]),
        [first] => &content[array.range.start + 1..first.range().start],
        [] => "",
    }
}

fn delimiter_suffix(delimiter: &str) -> &str {
    delimiter
        .split_once(',')
        .map(|(_, suffix)| suffix)
        .unwrap_or(delimiter)
}

fn append_to_container(
    content: &str,
    range: jsonc_parser::common::Range,
    has_elements: bool,
    delimiter: &str,
    value: &str,
) -> String {
    let closing = range.end - 1;
    let insertion_index = if has_elements {
        content[..closing].trim_end_matches([' ', '\t']).len()
    } else {
        closing
    };
    let mut updated = String::with_capacity(content.len() + delimiter.len() + value.len() + 1);
    updated.push_str(&content[..insertion_index]);
    if has_elements {
        updated.push(',');
        updated.push_str(delimiter);
    }
    updated.push_str(value);
    updated.push_str(&content[insertion_index..]);
    updated
}

fn canonical_hook_json(canonical: &CanonicalHook, hook_path: &Path) -> io::Result<String> {
    let command = serde_json::to_string(&hook_command(hook_path, Some(canonical.action)))?;
    let matcher = match canonical.matcher {
        Some(matcher) => format!("\"matcher\":{},", serde_json::to_string(matcher)?),
        None => String::new(),
    };
    let background = if canonical.background {
        ",\"async\":true"
    } else {
        ""
    };
    Ok(format!(
        "{{{matcher}\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":{CANONICAL_HOOK_TIMEOUT}{background}}}]}}"
    ))
}

fn verify_updated(updated: String, settings_path: &Path, desired: &Value) -> io::Result<String> {
    let actual = parse_value(&updated, settings_path)?;
    if &actual != desired {
        return Err(io::Error::other(format!(
            "failed to safely update claude settings at {}",
            settings_path.display()
        )));
    }
    Ok(updated)
}

fn direct_children_are_compact(children: &[CstNode]) -> bool {
    !children.iter().any(CstNode::is_newline)
}

fn parse_value(content: &str, settings_path: &Path) -> io::Result<Value> {
    serde_json::from_str(content).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })
}

fn reject_duplicate_keys(node: &CstNode, settings_path: &Path) -> io::Result<()> {
    if let Some(object) = node.as_object() {
        let mut names = HashSet::new();
        for property in object.properties() {
            let name = property
                .name()
                .ok_or_else(|| io::Error::other("JSON object property is missing a name"))?
                .decoded_value()
                .map_err(|err| io::Error::other(format!("failed to decode JSON key: {err}")))?;
            if !names.insert(name.clone()) {
                return Err(io::Error::other(format!(
                    "claude settings at {} contains duplicate key {name:?}",
                    settings_path.display()
                )));
            }
            if let Some(value) = property.value() {
                reject_duplicate_keys(&value, settings_path)?;
            }
        }
    } else if let Some(array) = node.as_array() {
        for element in array.elements() {
            reject_duplicate_keys(&element, settings_path)?;
        }
    }
    Ok(())
}

fn strict_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: false,
        allow_loose_object_property_names: false,
        allow_trailing_commas: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (&'static Path, &'static Path) {
        (
            Path::new("/home/test/.claude/settings.json"),
            Path::new("/home/test/.claude/hooks/kelpie-agent-state.sh"),
        )
    }

    #[test]
    fn install_preserves_untouched_formatting_and_complete_trailing_suffix() {
        let (settings_path, hook_path) = paths();
        let input = concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"Notification\" : [{\"matcher\":\"keep\",\"hooks\":[]}]\r\n",
            "    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        );

        let updated = install(input, settings_path, hook_path).unwrap();

        assert!(updated.starts_with(concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"Notification\" : [{\"matcher\":\"keep\",\"hooks\":[]}],\r\n",
        )));
        assert!(updated.ends_with(concat!(
            "\r\n    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        )));
        assert!(!updated.replace("\r\n", "").contains('\n'));
        assert!(updated.contains("\"SessionStart\""));
        assert_eq!(
            serde_json::from_str::<Value>(&updated).unwrap()["zeta"]["number"],
            100.0
        );
    }

    #[test]
    fn install_keeps_compact_containers_compact() {
        let (settings_path, hook_path) = paths();
        let canonical = canonical_hook_json(&CANONICAL_HOOKS[0], hook_path).unwrap();
        let prompt = canonical_hook_json(&CANONICAL_HOOKS[1], hook_path).unwrap();
        let prompt = format!("\"UserPromptSubmit\":[{prompt}]");
        let cases = [
            (
                "{\"zeta\":{\"escaped\":\"\\u0061\",\"n\":1e+02},\"alpha\":1}\r\n",
                format!(
                    "{{\"zeta\":{{\"escaped\":\"\\u0061\",\"n\":1e+02}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}],{prompt}}}}}\r\n"
                ),
            ),
            (
                "{\"hooks\":{\"Notification\":[{\"matcher\":\"keep\",\"hooks\":[]}]}, \"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"Notification\":[{{\"matcher\":\"keep\",\"hooks\":[]}}],\"SessionStart\":[{canonical}],{prompt}}}, \"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"keep\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"keep\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}},{canonical}],{prompt}}}}}"
                ),
            ),
            (
                "{\"zeta\":{\n  \"x\":1\n},\"alpha\":1}",
                format!(
                    "{{\"zeta\":{{\n  \"x\":1\n}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}],{prompt}}}}}"
                ),
            ),
            (
                "{\"hooks\":{\"Notification\":[\n  {\"matcher\":\"keep\",\"hooks\":[]}\n]},\"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"Notification\":[\n  {{\"matcher\":\"keep\",\"hooks\":[]}}\n],\"SessionStart\":[{canonical}],{prompt}}},\"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\n  \"matcher\":\"keep\",\n  \"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]\n}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\n  \"matcher\":\"keep\",\n  \"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]\n}},{canonical}],{prompt}}}}}"
                ),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(install(input, settings_path, hook_path).unwrap(), expected);
        }
    }

    #[test]
    fn install_scopes_claude_session_start_sources() {
        let (settings_path, hook_path) = paths();
        let installed = install("{}", settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let matcher = settings["hooks"]["SessionStart"][0]["matcher"]
            .as_str()
            .unwrap();
        assert_eq!(matcher, "^(startup|resume|clear|compact|fork)$");
        let pattern = regex::Regex::new(matcher).unwrap();
        for source in ["startup", "resume", "clear", "compact", "fork"] {
            assert!(pattern.is_match(source), "Claude source: {source}");
        }
        for source in ["new", "load", "", "future-source", "startup-extra"] {
            assert!(!pattern.is_match(source), "non-Claude source: {source}");
        }
    }

    #[test]
    fn install_is_a_byte_exact_noop_for_a_canonical_hook() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let prompt = serde_json::to_string(&hook_command(hook_path, Some("prompt"))).unwrap();
        let input = format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"hooks\":[{{\"timeout\":10,\"command\":{command},\"type\":\"command\"}}],\"matcher\":\"{SESSION_START_MATCHER}\"}}],\"UserPromptSubmit\":[{{\"hooks\":[{{\"async\":true,\"timeout\":10,\"command\":{prompt},\"type\":\"command\"}}]}}]}},\"escaped\":\"\\u0061\"}}  \r\n\r\n"
        );

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert_eq!(updated, input);
    }

    #[test]
    fn install_migrates_wildcard_session_start_and_preserves_user_hook() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let user_hook = r#"{ "type" : "command", "command" : "echo keep", "timeout" : 3 }"#;
        let input = format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [{{\"matcher\":\"*\",\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":10}},{user_hook}]}}]\n  }}\n}}\n\n"
        );
        let installed = install(&input, settings_path, hook_path).unwrap();
        assert!(installed.contains(user_hook));
        assert!(installed.ends_with("}\n\n"));
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["matcher"], "*");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
        assert_eq!(
            groups[1],
            canonical_hook_value(&CANONICAL_HOOKS[0], hook_path)
        );
        assert_eq!(
            install(&installed, settings_path, hook_path).unwrap(),
            installed
        );

        let removed = uninstall(&installed, settings_path, hook_path).unwrap();
        assert!(removed.contains(user_hook));
        assert!(!removed.contains(&command));
        let settings: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn install_preserves_canonical_session_start_position_during_migration() {
        let (settings_path, hook_path) = paths();
        let canonical = canonical_hook_json(&CANONICAL_HOOKS[0], hook_path).unwrap();
        let prompt = canonical_hook_json(&CANONICAL_HOOKS[1], hook_path).unwrap();
        let old_command = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        let session_start = format!(
            "\"SessionStart\":[{canonical},{{\"matcher\":\"foreign\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}}]"
        );
        let old_event = [
            "\"PostToolUse\":[{\"matcher\":\"*\",\"hooks\":[{\"type\":\"command\",\"command\":",
            &old_command,
            "}]}]",
        ]
        .concat();
        let input = ["{\"hooks\":{", &session_start, ",", &old_event, "}}"].concat();
        let expected = [
            "{\"hooks\":{",
            &session_start,
            ",\"UserPromptSubmit\":[",
            &prompt,
            "]}}",
        ]
        .concat();

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert_eq!(updated, expected);
    }

    #[test]
    fn install_removes_only_owned_commands_from_shared_hook_groups() {
        let (settings_path, hook_path) = paths();
        let old_command = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"PostToolUse\": [{{\n",
                "      \"matcher\": \"*\",\n",
                "      \"hooks\": [\n",
                "        {{\"type\":\"command\",\"command\":{old_command},\"timeout\":10}},\n",
                "        {{  \"type\" : \"command\", \"command\" : \"echo keep\", \"timeout\" : 3  }}\n",
                "      ]\n",
                "    }}],\n",
                "    \"Notification\": [{{\"matcher\":\"keep\",\"hooks\":[]}}]\n",
                "  }}\n",
                "}}\n",
            ),
            old_command = old_command,
        );

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert!(!updated.contains(&old_command));
        assert!(updated.contains(
            "        {  \"type\" : \"command\", \"command\" : \"echo keep\", \"timeout\" : 3  }"
        ));
        assert!(updated.contains("    \"Notification\": [{\"matcher\":\"keep\",\"hooks\":[]}]"));
        let parsed: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "echo keep"
        );
        assert_eq!(parsed["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_preserves_unrelated_hook_text() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "    \"before\" : \"\\u0061\",\n",
                "    \"hooks\" : {{\n",
                "        \"SessionStart\" : [{{\n",
                "            \"matcher\" : \"*\",\n",
                "            \"hooks\" : [\n",
                "                {{\"type\":\"command\",\"command\":{command},\"timeout\":10}},\n",
                "                {{  \"type\" : \"command\", \"command\" : \"echo keep\"  }}\n",
                "            ]\n",
                "        }}]\n",
                "    }},\n",
                "    \"after\" : 1e+02\n",
                "}}\n\n",
            ),
            command = command,
        );

        let updated = uninstall(&input, settings_path, hook_path).unwrap();

        assert_ne!(updated, input);
        assert!(!updated.contains(&command));
        assert!(updated
            .contains("                {  \"type\" : \"command\", \"command\" : \"echo keep\"  }"));
        assert!(updated.starts_with("{\n    \"before\" : \"\\u0061\","));
        assert!(updated.ends_with("    \"after\" : 1e+02\n}\n\n"));
    }

    #[test]
    fn install_rejects_duplicate_keys() {
        let (settings_path, hook_path) = paths();
        let error = install(
            r#"{"alpha": 1, "alpha": 2, "hooks": {}}"#,
            settings_path,
            hook_path,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("duplicate key \"alpha\""), "{error}");
    }

    #[test]
    fn install_keeps_structurally_invalid_content_unchanged() {
        let (settings_path, hook_path) = paths();
        for input in ["[]", r#"{"hooks": []}"#, r#"{"hooks":{"SessionStart":{}}}"#] {
            assert!(install(input, settings_path, hook_path).is_err());
        }
    }

    #[test]
    fn install_adds_background_prompt_submit_hook_and_uninstall_removes_it() {
        let (settings_path, hook_path) = paths();
        let installed = install("{}", settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let groups = settings["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].get("matcher").is_none());
        let hook = &groups[0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["command"], hook_command(hook_path, Some("prompt")));
        assert_eq!(hook["async"], true);
        assert_eq!(hook["timeout"], 10);
        assert_eq!(
            install(&installed, settings_path, hook_path).unwrap(),
            installed
        );

        let removed = uninstall(&installed, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&removed).unwrap();
        assert!(settings["hooks"].get("UserPromptSubmit").is_none());
        assert!(settings["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn install_upgrades_a_session_only_install_and_migrates_a_blocking_prompt_hook() {
        let (settings_path, hook_path) = paths();
        let session = canonical_hook_json(&CANONICAL_HOOKS[0], hook_path).unwrap();
        let prompt = serde_json::to_string(&hook_command(hook_path, Some("prompt"))).unwrap();
        let user_hook = r#"{"type":"command","command":"echo keep"}"#;
        // A previous install (SessionStart only) plus a blocking variant of the prompt hook
        // sharing a group with a user hook.
        let input = format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [{session}],\n    \"UserPromptSubmit\": [{{\"hooks\":[{{\"type\":\"command\",\"command\":{prompt},\"timeout\":10}},{user_hook}]}}]\n  }}\n}}\n"
        );

        let installed = install(&input, settings_path, hook_path).unwrap();

        assert!(installed.contains(user_hook));
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let groups = settings["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "{installed}");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
        assert_eq!(
            groups[1],
            canonical_hook_value(&CANONICAL_HOOKS[1], hook_path)
        );
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            install(&installed, settings_path, hook_path).unwrap(),
            installed
        );
    }
}

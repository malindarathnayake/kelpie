#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    StatusIndicators(super::StatusIndicatorStyle),
    Sound(bool),
    ToastDelivery(super::ToastDelivery),
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::StatusIndicators(_) => "status indicators",
            Self::Sound(_) => "sound setting",
            Self::ToastDelivery(_) => "toast setting",
        }
    }

    pub(crate) fn apply(self, content: &str) -> String {
        match self {
            Self::Theme(name) => {
                let updated =
                    super::upsert_section_value(content, "theme", "name", &format!("\"{name}\""));
                // A chosen theme only shows when auto-switch is off, so turn it off when
                // the file enables it; otherwise leave every other key untouched.
                if theme_auto_switch_enabled(content) {
                    super::upsert_section_bool(&updated, "theme", "auto_switch", false)
                } else {
                    updated
                }
            }
            Self::StatusIndicators(style) => super::upsert_section_value(
                content,
                "ui",
                "status_indicators",
                &format!("\"{}\"", style.as_str()),
            ),
            Self::Sound(enabled) => {
                super::upsert_section_bool(content, "ui.sound", "enabled", enabled)
            }
            Self::ToastDelivery(delivery) => {
                let value = match delivery {
                    super::ToastDelivery::Off => "\"off\"",
                    super::ToastDelivery::Herdr => "\"herdr\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
        }
    }
}

pub(crate) fn update_file_at(
    path: &std::path::Path,
    description: &str,
    update: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create config directory: {error}"))?;
    }
    let content = match super::io::read_optional_config(path) {
        Ok(Some(content)) => content,
        Ok(None) => String::new(),
        Err(error) => {
            return Err(format!(
                "failed to read config before saving {description}: {error}"
            ));
        }
    };
    let updated = update(&content);
    if content.parse::<toml::Value>().is_ok() {
        if let Err(error) = updated.parse::<toml::Value>() {
            return Err(format!(
                "saving {description} would make the config invalid TOML ({error}); leaving it unchanged"
            ));
        }
    }
    write_atomically(path, updated.as_bytes())
        .map_err(|error| format!("failed to save {description}: {error}"))
}

fn theme_auto_switch_enabled(content: &str) -> bool {
    content
        .parse::<toml::Value>()
        .ok()
        .and_then(|value| value.get("theme")?.get("auto_switch")?.as_bool())
        .unwrap_or(false)
}

/// Replace `path` with `bytes` through a sibling temp file and a rename, so a reader
/// never sees a half-written config. A symlinked config is written through its target.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let target = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let tmp_path = parent.join(format!(".{file_name}.{}.{nanos}.tmp", std::process::id()));
    let written = std::fs::File::create(&tmp_path).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(error) = written.and_then(|()| std::fs::rename(&tmp_path, &target)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn write_edit(edit: ConfigEdit<'_>) -> Result<(), String> {
    update_file_at(&super::config_path(), edit.description(), |content| {
        edit.apply(content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED_CONFIG: &str = r##"# my herdr config
onboarding = false

[theme]
# picked by hand
name = "nord" # trailing note
dark_name = "tokyo-night"

[theme.custom]
accent = "#ff79c6"

[ui]
sidebar_width = 30
"##;

    #[test]
    fn theme_edit_changes_only_the_name_line() {
        let updated = ConfigEdit::Theme("midnight").apply(COMMENTED_CONFIG);
        let expected =
            COMMENTED_CONFIG.replace("name = \"nord\" # trailing note", "name = \"midnight\"");
        assert_eq!(updated, expected);
        let parsed: crate::config::Config = toml::from_str(&updated).unwrap();
        assert_eq!(parsed.theme.name.as_deref(), Some("midnight"));
        assert_eq!(parsed.theme.dark_name.as_deref(), Some("tokyo-night"));
    }

    #[test]
    fn theme_edit_adds_a_theme_section_when_missing() {
        let updated = ConfigEdit::Theme("ember").apply(
            "[ui]
sidebar_width = 30
",
        );
        assert_eq!(
            updated,
            "[ui]
sidebar_width = 30

[theme]
name = \"ember\"
"
        );
    }

    #[test]
    fn theme_edit_turns_off_auto_switch_only_when_it_is_on() {
        let updated = ConfigEdit::Theme("dusk").apply(
            "[theme]
name = \"nord\"
auto_switch = true
",
        );
        assert_eq!(
            updated,
            "[theme]
name = \"dusk\"
auto_switch = false
"
        );
    }

    #[test]
    fn update_file_at_writes_atomically_and_keeps_other_content() {
        let dir = std::env::temp_dir().join(format!("herdr-config-theme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, COMMENTED_CONFIG).unwrap();

        update_file_at(&path, "theme", |content| {
            ConfigEdit::Theme("forest").apply(content)
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "config.toml")
            .count();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            written,
            COMMENTED_CONFIG.replace("name = \"nord\" # trailing note", "name = \"forest\"")
        );
        assert_eq!(leftovers, 0, "temp file left behind");
    }

    #[test]
    fn update_file_at_refuses_to_break_valid_toml() {
        let dir = std::env::temp_dir().join(format!("herdr-config-invalid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[theme]
name = \"nord\"
",
        )
        .unwrap();

        let result = update_file_at(&path, "theme", |_| {
            "[theme
"
            .to_string()
        });
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert_eq!(
            written,
            "[theme]
name = \"nord\"
"
        );
    }

    #[test]
    fn update_file_at_does_not_move_a_leading_bom_into_the_file() {
        let dir = std::env::temp_dir().join(format!("herdr-config-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        )
        .unwrap();

        update_file_at(&path, "onboarding setting", |content| {
            crate::config::upsert_top_level_bool(content, "onboarding", false)
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);

        assert!(
            !written.contains('\u{feff}'),
            "unexpected BOM in {written:?}"
        );
        assert!(
            toml::from_str::<toml::Value>(&written).is_ok(),
            "written config is not valid TOML: {written:?}"
        );
    }
}

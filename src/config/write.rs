#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    StatusIndicators(super::StatusIndicatorStyle),
    Sound(bool),
    ToastDelivery(super::ToastDelivery),
    /// Fork: a server access or supervisor switch from Settings -> access.
    Access(AccessSetting, bool),
}

/// Server-start switches the Settings access section can flip. Each takes effect the
/// next time the server starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessSetting {
    /// `[supervisor] enabled`.
    Supervisor,
    /// `[server] allow_low_integrity_clients` (Windows).
    LowIntegrityClients,
    /// `[server] allow_unelevated_clients` (Windows).
    UnelevatedClients,
}

impl AccessSetting {
    /// Rows shown on this platform, in display order.
    pub(crate) const SHOWN: &'static [Self] = if cfg!(windows) {
        &[
            Self::Supervisor,
            Self::LowIntegrityClients,
            Self::UnelevatedClients,
        ]
    } else {
        &[Self::Supervisor]
    };

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Supervisor => "supervisor",
            Self::LowIntegrityClients => "sandboxed tools (low integrity)",
            Self::UnelevatedClients => "ordinary clients to an elevated server",
        }
    }

    fn section_and_key(self) -> (&'static str, &'static str) {
        match self {
            Self::Supervisor => ("supervisor", "enabled"),
            Self::LowIntegrityClients => ("server", "allow_low_integrity_clients"),
            Self::UnelevatedClients => ("server", "allow_unelevated_clients"),
        }
    }
}

/// Current values of the access switches, read from the loaded config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AccessSettings {
    pub(crate) supervisor: bool,
    pub(crate) low_integrity_clients: bool,
    pub(crate) unelevated_clients: bool,
}

impl AccessSettings {
    pub(crate) fn from_config(config: &super::Config) -> Self {
        Self {
            supervisor: config.supervisor.enabled,
            #[cfg(windows)]
            low_integrity_clients: config.server.allow_low_integrity_clients,
            #[cfg(not(windows))]
            low_integrity_clients: false,
            #[cfg(windows)]
            unelevated_clients: config.server.allow_unelevated_clients,
            #[cfg(not(windows))]
            unelevated_clients: false,
        }
    }

    pub(crate) fn set(&mut self, setting: AccessSetting, enabled: bool) {
        match setting {
            AccessSetting::Supervisor => self.supervisor = enabled,
            AccessSetting::LowIntegrityClients => self.low_integrity_clients = enabled,
            AccessSetting::UnelevatedClients => self.unelevated_clients = enabled,
        }
    }

    pub(crate) fn get(self, setting: AccessSetting) -> bool {
        match setting {
            AccessSetting::Supervisor => self.supervisor,
            AccessSetting::LowIntegrityClients => self.low_integrity_clients,
            AccessSetting::UnelevatedClients => self.unelevated_clients,
        }
    }
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::StatusIndicators(_) => "status indicators",
            Self::Sound(_) => "sound setting",
            Self::ToastDelivery(_) => "toast setting",
            Self::Access(_, _) => "access setting",
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
                    super::ToastDelivery::Kelpie => "\"kelpie\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
            Self::Access(setting, enabled) => {
                let (section, key) = setting.section_and_key();
                super::upsert_section_bool(content, section, key, enabled)
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
    write_atomically_with(
        path,
        bytes,
        |from, to| std::fs::rename(from, to),
        std::thread::sleep,
    )
}

/// Backoff between rename attempts when another process briefly holds the config open.
const RENAME_BACKOFF_MS: [u64; 4] = [20, 50, 100, 200];

/// On Windows an editor, antivirus or indexer can hold the config without delete sharing for
/// a moment; the rename then fails with access denied (5) or a sharing/lock violation (32, 33).
fn rename_error_is_transient(error: &std::io::Error) -> bool {
    cfg!(windows)
        && (error.kind() == std::io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(5 | 32 | 33)))
}

fn write_atomically_with(
    path: &std::path::Path,
    bytes: &[u8],
    mut rename: impl FnMut(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
    mut sleep: impl FnMut(std::time::Duration),
) -> std::io::Result<()> {
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
    let (tmp_path, mut file) = create_temp_sibling(parent, file_name)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| {
        let mut attempt = 0;
        loop {
            match rename(&tmp_path, &target) {
                Ok(()) => return Ok(()),
                Err(error)
                    if attempt < RENAME_BACKOFF_MS.len() && rename_error_is_transient(&error) =>
                {
                    sleep(std::time::Duration::from_millis(RENAME_BACKOFF_MS[attempt]));
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    });
    if result.is_err() && std::fs::remove_file(&tmp_path).is_err() && tmp_path.exists() {
        sleep(std::time::Duration::from_millis(
            RENAME_BACKOFF_MS[RENAME_BACKOFF_MS.len() - 1],
        ));
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Create a new, uniquely named temp file next to the config; never reuse an existing file.
fn create_temp_sibling(
    parent: &std::path::Path,
    file_name: &str,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    create_temp_sibling_with_stamp(parent, file_name, nanos)
}

fn create_temp_sibling_with_stamp(
    parent: &std::path::Path,
    file_name: &str,
    nanos: u32,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    let mut last_error = None;
    for attempt in 0..16u32 {
        let tmp_path = parent.join(format!(
            ".{file_name}.{}.{nanos}.{attempt}.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free temp file name next to the config",
        )
    }))
}

pub(crate) fn write_edit(edit: ConfigEdit<'_>) -> Result<(), String> {
    update_file_at(&super::config_path(), edit.description(), |content| {
        edit.apply(content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED_CONFIG: &str = r##"# my kelpie config
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

    fn temp_config_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kelpie-config-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn other_files(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "config.toml")
            .collect()
    }

    fn sharing_violation() -> std::io::Error {
        std::io::Error::from_raw_os_error(32)
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_retries_a_briefly_locked_config_then_succeeds() {
        let dir = temp_config_dir("rename-retry");
        let path = dir.join("config.toml");
        std::fs::write(&path, "old\n").unwrap();
        let mut failures = 3;
        let mut sleeps = Vec::new();
        let result = write_atomically_with(
            &path,
            b"new\n",
            |from, to| {
                if failures > 0 {
                    failures -= 1;
                    return Err(sharing_violation());
                }
                std::fs::rename(from, to)
            },
            |duration| sleeps.push(duration.as_millis()),
        );
        let written = std::fs::read_to_string(&path).unwrap();
        let leftovers = other_files(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        result.unwrap();
        assert_eq!(written, "new\n");
        assert_eq!(sleeps, vec![20, 50, 100]);
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn atomic_write_gives_up_removes_the_temp_file_and_keeps_the_old_config() {
        let dir = temp_config_dir("rename-fail");
        let path = dir.join("config.toml");
        std::fs::write(&path, "old\n").unwrap();
        let mut attempts = 0;
        let result = write_atomically_with(
            &path,
            b"new\n",
            |_, _| {
                attempts += 1;
                Err(sharing_violation())
            },
            |_| {},
        );
        let written = std::fs::read_to_string(&path).unwrap();
        let leftovers = other_files(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        let expected_attempts = if cfg!(windows) {
            RENAME_BACKOFF_MS.len() + 1
        } else {
            1
        };
        assert_eq!(attempts, expected_attempts);
        assert_eq!(written, "old\n");
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn atomic_write_does_not_retry_a_permanent_rename_error() {
        let dir = temp_config_dir("rename-permanent");
        let path = dir.join("config.toml");
        std::fs::write(&path, "old\n").unwrap();
        let mut attempts = 0;
        let result = write_atomically_with(
            &path,
            b"new\n",
            |_, _| {
                attempts += 1;
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            },
            |_| panic!("no backoff for a permanent error"),
        );
        let leftovers = other_files(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert_eq!(attempts, 1);
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn temp_sibling_skips_names_that_already_exist() {
        let dir = temp_config_dir("temp-name");
        let taken = dir.join(format!(".config.toml.{}.7.0.tmp", std::process::id()));
        std::fs::write(&taken, "someone else's").unwrap();
        let (created, file) = create_temp_sibling_with_stamp(&dir, "config.toml", 7).unwrap();
        drop(file);
        let untouched = std::fs::read_to_string(&taken).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_ne!(created, taken);
        assert_eq!(untouched, "someone else's");
    }

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
        let dir = std::env::temp_dir().join(format!("kelpie-config-theme-{}", std::process::id()));
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
        let dir =
            std::env::temp_dir().join(format!("kelpie-config-invalid-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("kelpie-config-bom-{}", std::process::id()));
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

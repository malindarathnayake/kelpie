//! `herdr theme list` / `herdr theme set <name>`: list built-in themes and persist
//! `theme.name` in the local config, then ask a running server to reload config.

use crate::api::schema::{EmptyParams, Method, Request};

pub(super) fn run_theme_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("list") if args.len() == 1 => {
            let config = crate::config::Config::load().config;
            print!("{}", theme_list_text(&config.theme));
            Ok(0)
        }
        Some("set") => theme_set(&args[1..]),
        Some("help" | "--help" | "-h") => {
            print_theme_help();
            Ok(0)
        }
        _ => {
            print_theme_help();
            Ok(2)
        }
    }
}

/// Theme name currently selected by config (`theme.name`, default `catppuccin`).
fn current_theme_name(theme: &crate::config::ThemeConfig) -> &'static str {
    theme
        .name
        .as_deref()
        .and_then(crate::config::canonical_theme_name)
        .unwrap_or("catppuccin")
}

fn theme_list_text(theme: &crate::config::ThemeConfig) -> String {
    let current = current_theme_name(theme);
    let mut text = String::new();
    let mut group = None;
    for name in crate::config::grouped_theme_names() {
        let label = if crate::config::theme_is_light(name) {
            "light"
        } else {
            "dark"
        };
        if group != Some(label) {
            text.push_str(label);
            text.push_str(":\n");
            group = Some(label);
        }
        let marker = if name == current { '*' } else { ' ' };
        text.push_str(&format!("  {marker} {name}\n"));
    }
    if theme.auto_switch {
        text.push_str("note: theme.auto_switch is on, so dark_name/light_name apply instead\n");
    }
    text
}

/// Validates a theme argument and returns its canonical built-in name.
fn parse_theme_set_args(args: &[String]) -> Result<&'static str, String> {
    let [name] = args else {
        return Err("usage: herdr theme set <name>".into());
    };
    crate::config::canonical_theme_name(name).ok_or_else(|| {
        format!(
            "unknown theme {name:?}; valid themes: {}",
            crate::config::THEME_NAMES.join(", ")
        )
    })
}

fn theme_set(args: &[String]) -> std::io::Result<i32> {
    let name = match parse_theme_set_args(args) {
        Ok(name) => name,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    if let Err(error) = crate::config::write_edit(crate::config::ConfigEdit::Theme(name)) {
        eprintln!("{error}");
        return Ok(1);
    }
    println!(
        "theme set to {name} in {}",
        crate::config::config_path().display()
    );

    match super::send_request(&Request {
        id: "cli:theme:set".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    }) {
        Ok(response) if response.get("error").is_some() => {
            eprintln!(
                "saved, but the running server did not reload config: {}",
                serde_json::to_string(&response).map_err(std::io::Error::other)?
            );
            Ok(1)
        }
        Ok(_) => {
            println!("running server reloaded config");
            Ok(0)
        }
        Err(error) if super::server_not_running_was_reported(&error) => {
            println!("no running server; the theme applies on next start");
            Ok(0)
        }
        Err(error) => Err(error),
    }
}

fn print_theme_help() {
    eprintln!("herdr theme commands:");
    eprintln!("  herdr theme list          list built-in themes (* marks the current one)");
    eprintln!("  herdr theme set <name>    save theme.name in config and reload a running server");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn theme_set_accepts_canonical_names_and_aliases() {
        assert_eq!(parse_theme_set_args(&args(&["midnight"])), Ok("midnight"));
        assert_eq!(parse_theme_set_args(&args(&["Kelpie"])), Ok("kelpie"));
        assert_eq!(
            parse_theme_set_args(&args(&["latte"])),
            Ok("catppuccin-latte")
        );
    }

    #[test]
    fn theme_set_rejects_unknown_names_and_bad_arity() {
        let error = parse_theme_set_args(&args(&["midnite"])).unwrap_err();
        assert!(error.contains("unknown theme \"midnite\""), "{error}");
        assert!(error.contains("midnight"), "{error}");
        assert!(parse_theme_set_args(&args(&[])).is_err());
        assert!(parse_theme_set_args(&args(&["dusk", "ember"])).is_err());
    }

    #[test]
    fn theme_list_groups_dark_then_light_and_marks_the_current_theme() {
        let theme = crate::config::ThemeConfig {
            name: Some("tokyonight".into()),
            ..Default::default()
        };
        let text = theme_list_text(&theme);
        let dark = text.find("dark:\n").unwrap();
        let light = text.find("light:\n").unwrap();
        assert!(dark < light);
        assert!(text.contains("  * tokyo-night\n"), "{text}");
        assert_eq!(text.matches('*').count(), 1, "{text}");
        assert!(text[dark..light].contains("    midnight\n"), "{text}");
        assert!(text[light..].contains("    catppuccin-latte\n"), "{text}");
        for name in crate::config::THEME_NAMES {
            assert!(
                text.contains(&format!(" {name}\n")),
                "{name} missing: {text}"
            );
        }
    }

    #[test]
    fn theme_list_defaults_to_catppuccin_and_notes_auto_switch() {
        let theme = crate::config::ThemeConfig {
            auto_switch: true,
            ..Default::default()
        };
        let text = theme_list_text(&theme);
        assert!(text.contains("  * catppuccin\n"), "{text}");
        assert!(text.contains("auto_switch is on"), "{text}");
    }
}

//! `kelpie host facts [--json] [--refresh]`: print the host facts the server's
//! worker wrote to `state_dir/host-facts.json`, or ask the server to refresh.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::schema::host::{
    HostDockerStatus, HostFacts, HostFactsParams, HostProbeStatus, HostShellInfo, HostToolSource,
    HostToolStatus,
};
use crate::api::schema::{Method, Request};

const USAGE: &str = "usage: kelpie host facts [--json] [--refresh]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FactsArgs {
    json: bool,
    refresh: bool,
}

pub(super) fn run_host_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("facts") => match parse_facts_args(&args[1..]) {
            Ok(parsed) => host_facts(parsed),
            Err(message) => {
                eprintln!("{message}");
                eprintln!("{USAGE}");
                Ok(2)
            }
        },
        Some("help" | "--help" | "-h") => {
            print_host_help();
            Ok(0)
        }
        _ => {
            print_host_help();
            Ok(2)
        }
    }
}

fn parse_facts_args(args: &[String]) -> Result<FactsArgs, String> {
    let mut parsed = FactsArgs {
        json: false,
        refresh: false,
    };
    for arg in args {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--refresh" => parsed.refresh = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(parsed)
}

fn host_facts(args: FactsArgs) -> std::io::Result<i32> {
    let facts = if args.refresh {
        let response = super::send_request(&Request {
            id: "cli:host:facts".into(),
            method: Method::HostFacts(HostFactsParams { refresh: true }),
        });
        let response = match response {
            Ok(response) => response,
            Err(error) if super::server_not_running_was_reported(&error) => return Ok(1),
            Err(error) => return Err(error),
        };
        if response.get("error").is_some() {
            eprintln!("{response}");
            return Ok(1);
        }
        let mut result = response["result"].clone();
        if let Some(object) = result.as_object_mut() {
            object.remove("type");
        }
        match serde_json::from_value::<HostFacts>(result) {
            Ok(facts) => facts,
            Err(err) => {
                eprintln!("server returned unreadable host facts: {err}");
                return Ok(1);
            }
        }
    } else {
        let path = crate::host_facts::facts_file_path();
        match crate::host_facts::read_facts_file(&path) {
            Ok(facts) => facts,
            Err(err) => {
                eprintln!(
                    "no host facts at {} ({err}); start the server (host facts are on by default) or run `kelpie host facts --refresh`",
                    path.display()
                );
                return Ok(1);
            }
        }
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&facts).map_err(std::io::Error::other)?
        );
    } else {
        print!("{}", render_facts(&facts, now_ms()));
    }
    Ok(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn opt(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn probe_status(status: HostProbeStatus) -> &'static str {
    match status {
        HostProbeStatus::Ok => "ok",
        HostProbeStatus::Missing => "missing",
        HostProbeStatus::Failed => "failed",
    }
}

fn tool_status(status: HostToolStatus) -> &'static str {
    match status {
        HostToolStatus::Ok => "ok",
        HostToolStatus::Missing => "missing",
        HostToolStatus::Failed => "failed",
        HostToolStatus::Shadowed => "shadowed",
    }
}

fn tool_source(source: Option<HostToolSource>) -> &'static str {
    match source {
        Some(HostToolSource::GitForWindows) => "git-for-windows",
        Some(HostToolSource::Scoop) => "scoop",
        Some(HostToolSource::Winget) => "winget",
        Some(HostToolSource::Chocolatey) => "chocolatey",
        Some(HostToolSource::System) => "system",
        Some(HostToolSource::Other) => "other",
        None => "-",
    }
}

fn shell_line(label: &str, shell: &HostShellInfo) -> String {
    format!(
        "{label} {} ({}), PSReadLine {}, {}, {}",
        opt(shell.version.as_deref()),
        shell.path,
        opt(shell.psreadline_version.as_deref()),
        opt(shell.language_mode.as_deref()),
        probe_status(shell.status),
    )
}

/// Human table. Pure so it can be tested with a fixed `now_ms`.
fn render_facts(facts: &HostFacts, now_ms: u64) -> String {
    let mut out = String::new();
    let age_secs = now_ms.saturating_sub(facts.refreshed_at_ms) / 1000;
    out.push_str(&format!(
        "host facts (refreshed {age_secs}s ago, every {}s)\n",
        facts.interval_secs
    ));
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
    let rows: Vec<(&str, String)> = vec![
        ("os", os_line),
        ("memory", opt(facts.memory.bucket.as_deref()).to_string()),
        (
            "disk",
            format!(
                "{} free (state dir volume)",
                opt(facts.disk.bucket.as_deref())
            ),
        ),
        (
            "shell",
            match (&facts.shells.pwsh, &facts.shells.windows_powershell) {
                (None, None) => "no PowerShell found".to_string(),
                (pwsh, legacy) => {
                    let mut parts = Vec::new();
                    if let Some(pwsh) = pwsh {
                        parts.push(shell_line("pwsh", pwsh));
                    }
                    if let Some(legacy) = legacy {
                        parts.push(shell_line("powershell", legacy));
                    }
                    parts.join("\n         ")
                }
            },
        ),
        (
            "clm",
            match facts.shells.constrained_language_mode {
                Some(true) => "ConstrainedLanguage (pwsh snippets degrade to no-op)".to_string(),
                Some(false) => "no".to_string(),
                None => "unknown".to_string(),
            },
        ),
        (
            "git",
            format!(
                "{}; bash {}",
                opt(facts.git.git_path.as_deref()),
                opt(facts.git.bash_path.as_deref())
            ),
        ),
        (
            "gpu",
            if facts.gpu.names.is_empty() {
                format!("nvidia-smi {}", probe_status(facts.gpu.nvidia_smi))
            } else {
                format!(
                    "{} (driver {})",
                    facts.gpu.names.join(", "),
                    opt(facts.gpu.driver_version.as_deref())
                )
            },
        ),
        (
            "docker",
            match facts.docker.status {
                HostDockerStatus::Running => "running",
                HostDockerStatus::NotRunning => "not running",
                HostDockerStatus::Missing => "missing",
                HostDockerStatus::Failed => "failed",
            }
            .to_string(),
        ),
        (
            "wsl",
            match (facts.wsl.present, facts.wsl.distros.is_empty()) {
                (false, _) => "not installed".to_string(),
                (true, true) => "present".to_string(),
                (true, false) => format!("present: {}", facts.wsl.distros.join(", ")),
            },
        ),
    ];
    for (label, value) in rows {
        out.push_str(&format!("{label:<8} {value}\n"));
    }

    out.push('\n');
    let name_width = facts
        .tools
        .iter()
        .map(|tool| tool.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let version_width = facts
        .tools
        .iter()
        .map(|tool| opt(tool.version.as_deref()).chars().count())
        .max()
        .unwrap_or(7)
        .clamp(7, 40);
    out.push_str(&format!(
        "{:<name_width$}  {:<8}  {:<version_width$}  {:<15}  path\n",
        "tool", "status", "version", "source"
    ));
    for tool in &facts.tools {
        let version: String = opt(tool.version.as_deref())
            .chars()
            .take(version_width)
            .collect();
        let mut line = format!(
            "{:<name_width$}  {:<8}  {:<version_width$}  {:<15}  {}",
            tool.name,
            tool_status(tool.status),
            version,
            tool_source(tool.source),
            opt(tool.path.as_deref()),
        );
        if let Some(winner) = &tool.shadowed_by {
            line.push_str(&format!("  (shadowed by {winner})"));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn print_host_help() {
    eprintln!("kelpie host commands:");
    eprintln!("  kelpie host facts             show host facts from the state directory file");
    eprintln!("  kelpie host facts --json      print them as JSON (ConvertFrom-Json friendly)");
    eprintln!("  kelpie host facts --refresh   ask the running server to refresh first");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::host::{
        HostBucketFact, HostDockerFacts, HostGitFacts, HostGpuFacts, HostOsFacts, HostShellFacts,
        HostToolFact, HostWslFacts,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn facts_args_parse() {
        assert_eq!(
            parse_facts_args(&args(&[])),
            Ok(FactsArgs {
                json: false,
                refresh: false
            })
        );
        assert_eq!(
            parse_facts_args(&args(&["--json", "--refresh"])),
            Ok(FactsArgs {
                json: true,
                refresh: true
            })
        );
        assert!(parse_facts_args(&args(&["--bogus"])).is_err());
    }

    #[test]
    fn table_lists_facts_and_shadowing() {
        let facts = HostFacts {
            schema_version: 1,
            refreshed_at_ms: 10_000,
            interval_secs: 60,
            os: HostOsFacts {
                name: "Windows 11".into(),
                version: Some("24H2".into()),
                build: Some("26100.4061".into()),
                arch: "aarch64".into(),
                process_arch: "x86_64".into(),
                checked_at_ms: 10_000,
            },
            memory: HostBucketFact {
                bucket: Some("16-32 GB".into()),
                checked_at_ms: 10_000,
            },
            disk: HostBucketFact {
                bucket: Some("100-500 GB".into()),
                checked_at_ms: 10_000,
            },
            shells: HostShellFacts {
                preferred: None,
                pwsh: None,
                windows_powershell: None,
                psreadline_version: None,
                constrained_language_mode: None,
                checked_at_ms: 10_000,
            },
            git: HostGitFacts {
                git_path: None,
                bash_path: None,
                checked_at_ms: 10_000,
            },
            gpu: HostGpuFacts {
                nvidia_smi: HostProbeStatus::Missing,
                names: Vec::new(),
                driver_version: None,
                checked_at_ms: 10_000,
            },
            docker: HostDockerFacts {
                status: HostDockerStatus::NotRunning,
                checked_at_ms: 10_000,
            },
            wsl: HostWslFacts {
                present: true,
                distros: vec!["Ubuntu".into()],
                checked_at_ms: 10_000,
            },
            tools: vec![HostToolFact {
                name: "find".into(),
                path: Some(r"%ProgramFiles%\Git\usr\bin\find.exe".into()),
                version: None,
                source: Some(HostToolSource::GitForWindows),
                status: HostToolStatus::Missing,
                shadowed_by: Some(r"%SystemRoot%\System32\find.exe".into()),
            }],
            tools_checked_at_ms: 10_000,
        };
        let text = render_facts(&facts, 52_000);
        assert!(text.starts_with("host facts (refreshed 42s ago, every 60s)\n"));
        assert!(
            text.contains("os       Windows 11 24H2 build 26100.4061 (aarch64, kelpie x86_64)\n")
        );
        assert!(text.contains("docker   not running\n"));
        assert!(text.contains("wsl      present: Ubuntu\n"));
        assert!(text.contains("shell    no PowerShell found\n"));
        assert!(text.contains(
            r"find  missing   -        git-for-windows  %ProgramFiles%\Git\usr\bin\find.exe  (shadowed by %SystemRoot%\System32\find.exe)"
        ));
    }
}

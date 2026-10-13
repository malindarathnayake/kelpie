//! Host facts (`host.facts`): a read-only snapshot of the machine the server runs
//! on: OS, shells, GPU, Docker, WSL, coarse memory/disk buckets and a detect-only
//! toolpack. Paths are normalised with environment variables (`%USERPROFILE%`,
//! `%LOCALAPPDATA%`, `%ProgramFiles%`, ...) and never carry user or host names.
//! All numbers are integers so the JSON round-trips through `ConvertFrom-Json`.

use serde::{Deserialize, Serialize};

/// Current `HostFacts::schema_version`.
pub const HOST_FACTS_SCHEMA_VERSION: u32 = 1;

/// Parameters for `host.facts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostFactsParams {
    /// Run every probe tier now and answer with the refreshed snapshot.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub refresh: bool,
}

/// Point-in-time host facts. `*_checked_at_ms` values are Unix epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostFacts {
    pub schema_version: u32,
    /// When the worker last finished a refresh.
    pub refreshed_at_ms: u64,
    /// Base refresh beat in seconds.
    pub interval_secs: u64,
    pub os: HostOsFacts,
    /// Total physical memory bucket.
    pub memory: HostBucketFact,
    /// Free space bucket of the volume holding the herdr state directory.
    pub disk: HostBucketFact,
    pub shells: HostShellFacts,
    pub git: HostGitFacts,
    pub gpu: HostGpuFacts,
    pub docker: HostDockerFacts,
    pub wsl: HostWslFacts,
    /// Detect-only toolpack. Nothing is downloaded and PATH is never changed.
    pub tools: Vec<HostToolFact>,
    pub tools_checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostOsFacts {
    /// For example `Windows 11`.
    pub name: String,
    /// For example `24H2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// For example `26100.4061`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Native machine architecture (`x86_64`, `aarch64`, `x86`).
    pub arch: String,
    /// Architecture of the herdr binary; differs from `arch` under emulation.
    pub process_arch: String,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostBucketFact {
    /// Coarse range such as `16-32 GB`; absent when the value could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostShellFacts {
    /// `pwsh` when PowerShell 7 is present, else `powershell` (5.1), else absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pwsh: Option<HostShellInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_powershell: Option<HostShellInfo>,
    /// PSReadLine version seen by the preferred shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psreadline_version: Option<String>,
    /// Constrained Language Mode in the preferred shell; absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_language_mode: Option<bool>,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostShellInfo {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `FullLanguage`, `ConstrainedLanguage`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psreadline_version: Option<String>,
    pub status: HostProbeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostGitFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_path: Option<String>,
    /// Git Bash `bash.exe` (never the WSL launcher in System32).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_path: Option<String>,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostGpuFacts {
    pub nvidia_smi: HostProbeStatus,
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostDockerFacts {
    pub status: HostDockerStatus,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostWslFacts {
    pub present: bool,
    /// Registered distribution names, read from the registry without starting WSL.
    pub distros: Vec<String>,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostToolFact {
    pub name: String,
    /// Where the real tool lives. With status `missing` this is an installed copy
    /// that a plain invocation from a pane does not reach (not on PATH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<HostToolSource>,
    pub status: HostToolStatus,
    /// What a plain invocation reaches instead: a System32 binary or a PowerShell alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostProbeStatus {
    Ok,
    Missing,
    /// Present but the probe failed or hit its timeout.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostDockerStatus {
    Running,
    NotRunning,
    /// No docker CLI on PATH and no engine endpoint.
    Missing,
    /// The engine endpoint exists but `_ping` failed or timed out.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostToolStatus {
    Ok,
    Missing,
    /// On PATH but `--version` failed or timed out.
    Failed,
    /// On PATH, but a System32 binary or a PowerShell alias wins a plain invocation.
    Shadowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HostToolSource {
    GitForWindows,
    Scoop,
    Winget,
    Chocolatey,
    System,
    Other,
}

impl HostProbeStatus {
    /// Lowercase word used in human output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }
}

impl HostDockerStatus {
    /// Lowercase words used in human output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NotRunning => "not running",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }
}

impl HostToolStatus {
    /// Lowercase word used in human output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Failed => "failed",
            Self::Shadowed => "shadowed",
        }
    }
}

impl HostToolSource {
    /// Kebab-case name, as serialized.
    pub fn label(self) -> &'static str {
        match self {
            Self::GitForWindows => "git-for-windows",
            Self::Scoop => "scoop",
            Self::Winget => "winget",
            Self::Chocolatey => "chocolatey",
            Self::System => "system",
            Self::Other => "other",
        }
    }
}

//! Assembles one `HostFacts` snapshot from the injected host probes.
//!
//! Tiers on the base beat: PATH stat, OS/memory/disk and the Docker ping run
//! every beat; `--version` and shell probes re-run only when the resolved path
//! or its file stamp changed; `nvidia-smi` runs every 300 s. A forced refresh
//! runs every tier but still reuses unchanged version results.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;

use crate::api::schema::host::{
    HostBucketFact, HostDockerFacts, HostDockerStatus, HostFacts, HostGitFacts, HostGpuFacts,
    HostOsFacts, HostProbeStatus, HostShellFacts, HostShellInfo, HostToolFact, HostToolStatus,
    HostWslFacts, HOST_FACTS_SCHEMA_VERSION,
};
use crate::platform::DockerEnginePing;

use super::paths::{disk_bucket, memory_bucket, PathNormalizer};
use super::probe::{FileStamp, HostProbes, ProbeRun};
use super::tools::{
    git_root_from_git_path, git_usr_bin_candidate, infer_source, resolve_tool, search_path,
    PathHit, TOOLPACK,
};

pub(crate) const DOCKER_PING_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const VERSION_TIMEOUT: Duration = Duration::from_secs(3);
/// PowerShell cold start plus `Get-Module -ListAvailable` can take seconds.
pub(crate) const SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const GPU_TIER_MS: u64 = 300_000;

const VERSION_TEXT_LIMIT: usize = 120;
const MAX_GPUS: usize = 8;
const MAX_WSL_DISTROS: usize = 16;
const NAME_TEXT_LIMIT: usize = 80;
const ALIAS_LIMIT: usize = 32;

/// Fixed PowerShell probe, sent with `-EncodedCommand` so no argument quoting or
/// interpolation is involved. Uses only string output so it also runs under
/// Constrained Language Mode.
const SHELL_PROBE_SCRIPT: &str = "$ErrorActionPreference = 'SilentlyContinue'\n\
'v=' + $PSVersionTable.PSVersion\n\
'lm=' + $ExecutionContext.SessionState.LanguageMode\n\
$m = Get-Module -ListAvailable -Name PSReadLine | Sort-Object Version -Descending | Select-Object -First 1\n\
if ($m) { 'psrl=' + $m.Version }\n\
Get-Alias -Name rg,fd,sd,jq,sed,awk,diff,grep,find,sort,git,bash,python,node,cargo,docker -ErrorAction SilentlyContinue | ForEach-Object { 'alias=' + $_.Name + '=' + $_.Definition }\n\
exit 0\n";

/// Which tiers run on this beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tiers {
    pub(crate) path: bool,
    pub(crate) docker: bool,
    pub(crate) gpu: bool,
}

/// Tier schedule on an injected monotonic millisecond clock.
#[derive(Debug, Clone)]
pub(crate) struct TierSchedule {
    gpu_every_ms: u64,
    next_gpu_at_ms: Option<u64>,
}

impl TierSchedule {
    pub(crate) fn new(gpu_every_ms: u64) -> Self {
        Self {
            gpu_every_ms,
            next_gpu_at_ms: None,
        }
    }

    pub(crate) fn due(&mut self, now_ms: u64, force: bool) -> Tiers {
        let gpu = force || self.next_gpu_at_ms.is_none_or(|next| now_ms >= next);
        if gpu {
            self.next_gpu_at_ms = Some(now_ms.saturating_add(self.gpu_every_ms));
        }
        Tiers {
            path: true,
            docker: true,
            gpu,
        }
    }
}

#[derive(Debug, Clone)]
struct Cached<T> {
    stamp: FileStamp,
    value: T,
}

/// Parsed output of the PowerShell probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ShellProbe {
    pub(crate) ok: bool,
    pub(crate) version: Option<String>,
    pub(crate) language_mode: Option<String>,
    pub(crate) psreadline_version: Option<String>,
    pub(crate) aliases: BTreeMap<String, String>,
}

pub(crate) struct Collector {
    normalizer: PathNormalizer,
    schedule: TierSchedule,
    versions: HashMap<PathBuf, Cached<Option<String>>>,
    shells: HashMap<PathBuf, Cached<ShellProbe>>,
    last_gpu: Option<HostGpuFacts>,
    interval_secs: u64,
    state_dir: PathBuf,
}

impl Collector {
    pub(crate) fn new(probes: &dyn HostProbes, interval_secs: u64, state_dir: PathBuf) -> Self {
        Self {
            normalizer: PathNormalizer::from_probes(probes),
            schedule: TierSchedule::new(GPU_TIER_MS),
            versions: HashMap::new(),
            shells: HashMap::new(),
            last_gpu: None,
            interval_secs,
            state_dir,
        }
    }

    pub(crate) fn collect(
        &mut self,
        probes: &dyn HostProbes,
        mono_ms: u64,
        wall_ms: u64,
        force: bool,
    ) -> HostFacts {
        let tiers = self.schedule.due(mono_ms, force);
        let normalizer = self.normalizer.clone();
        let normalize = move |path: &Path| normalizer.normalize(path);
        let windows = probes.windows();

        let os_info = probes.os_info();
        let os = HostOsFacts {
            name: os_info.name,
            version: os_info.version,
            build: os_info.build,
            arch: os_info.arch,
            process_arch: std::env::consts::ARCH.to_string(),
            checked_at_ms: wall_ms,
        };
        let memory = HostBucketFact {
            bucket: probes
                .total_memory_bytes()
                .map(|bytes| memory_bucket(bytes).to_string()),
            checked_at_ms: wall_ms,
        };
        let disk = HostBucketFact {
            bucket: probes
                .disk_free_bytes(&self.state_dir)
                .map(|bytes| disk_bucket(bytes).to_string()),
            checked_at_ms: wall_ms,
        };

        let mut seen_paths = HashSet::new();
        let shells = self.collect_shells(probes, &normalize, wall_ms, &mut seen_paths);
        let aliases = self.preferred_aliases(&shells);

        let git_hits = search_path(probes, "git", &normalize);
        let git_root = git_hits
            .first()
            .and_then(|hit| git_root_from_git_path(&hit.raw))
            .or_else(|| default_git_root(probes));

        let gpu = if tiers.gpu || self.last_gpu.is_none() {
            let gpu = self.probe_gpu(probes, &normalize, wall_ms);
            self.last_gpu = Some(gpu.clone());
            gpu
        } else {
            self.last_gpu.clone().unwrap_or_else(|| HostGpuFacts {
                nvidia_smi: HostProbeStatus::Missing,
                names: Vec::new(),
                driver_version: None,
                checked_at_ms: wall_ms,
            })
        };

        let mut tools = Vec::with_capacity(TOOLPACK.len());
        let mut docker_cli = false;
        for &name in TOOLPACK {
            let hits = if name == "git" {
                git_hits.clone()
            } else {
                search_path(probes, name, &normalize)
            };
            let off_path = git_usr_bin_candidate(probes, git_root.as_deref(), name, &normalize);
            let resolution = resolve_tool(name, &hits, off_path, &aliases);
            let on_path = resolution.status != HostToolStatus::Missing;
            if name == "docker" {
                docker_cli = on_path;
            }
            let mut status = resolution.status;
            let mut version = None;
            if let Some(hit) = resolution.hit.as_ref().filter(|_| on_path) {
                seen_paths.insert(hit.raw.clone());
                if name == "nvidia-smi" {
                    // Never run nvidia-smi off its own tier; report the driver.
                    version = gpu.driver_version.clone();
                    if gpu.nvidia_smi == HostProbeStatus::Failed {
                        status = HostToolStatus::Failed;
                    }
                } else {
                    version = self.tool_version(probes, &hit.raw);
                    if version.is_none() {
                        status = HostToolStatus::Failed;
                    }
                }
            }
            tools.push(HostToolFact {
                name: name.to_string(),
                source: resolution
                    .hit
                    .as_ref()
                    .map(|hit| infer_source(&hit.normalized)),
                path: resolution.hit.map(|hit| hit.normalized),
                version,
                status,
                shadowed_by: resolution.shadowed_by,
            });
        }
        self.versions.retain(|path, _| seen_paths.contains(path));
        self.shells.retain(|path, _| seen_paths.contains(path));

        let git = HostGitFacts {
            git_path: git_hits.first().map(|hit| hit.normalized.clone()),
            bash_path: git_bash_path(probes, git_root.as_deref(), windows, &normalize),
            checked_at_ms: wall_ms,
        };

        let docker_status = match probes.docker_ping(DOCKER_PING_TIMEOUT) {
            DockerEnginePing::Running => HostDockerStatus::Running,
            DockerEnginePing::NotRunning if docker_cli => HostDockerStatus::NotRunning,
            DockerEnginePing::NotRunning => HostDockerStatus::Missing,
            DockerEnginePing::Failed(_) => HostDockerStatus::Failed,
        };
        let docker = HostDockerFacts {
            status: docker_status,
            checked_at_ms: wall_ms,
        };

        let wsl = match probes.wsl_distros() {
            Some(distros) => HostWslFacts {
                present: true,
                distros: distros
                    .into_iter()
                    .take(MAX_WSL_DISTROS)
                    .map(|name| clip(&self.normalizer.redact_text(&name), NAME_TEXT_LIMIT))
                    .collect(),
                checked_at_ms: wall_ms,
            },
            None => HostWslFacts {
                present: false,
                distros: Vec::new(),
                checked_at_ms: wall_ms,
            },
        };

        let ok_tools = tools
            .iter()
            .filter(|tool| tool.status == HostToolStatus::Ok)
            .count();
        tracing::debug!(
            forced = force,
            gpu_tier = tiers.gpu,
            docker = ?docker.status,
            nvidia_smi = ?gpu.nvidia_smi,
            wsl = wsl.present,
            tools_ok = ok_tools,
            tools_total = tools.len(),
            "host facts refreshed"
        );

        HostFacts {
            schema_version: HOST_FACTS_SCHEMA_VERSION,
            refreshed_at_ms: wall_ms,
            interval_secs: self.interval_secs,
            os,
            memory,
            disk,
            shells,
            git,
            gpu,
            docker,
            wsl,
            tools,
            tools_checked_at_ms: wall_ms,
        }
    }

    fn collect_shells(
        &mut self,
        probes: &dyn HostProbes,
        normalize: &dyn Fn(&Path) -> String,
        wall_ms: u64,
        seen: &mut HashSet<PathBuf>,
    ) -> HostShellFacts {
        let pwsh_path = search_path(probes, "pwsh", normalize)
            .into_iter()
            .next()
            .map(|hit| hit.raw)
            .or_else(|| {
                let root = probes
                    .env_var("ProgramFiles")
                    .filter(|_| probes.windows())?;
                let candidate = PathBuf::from(root)
                    .join("PowerShell")
                    .join("7")
                    .join("pwsh.exe");
                probes.file_stamp(&candidate).map(|_| candidate)
            });
        let windows_powershell_path = probes
            .env_var("SystemRoot")
            .filter(|_| probes.windows())
            .map(|root| {
                PathBuf::from(root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
            })
            .filter(|path| probes.file_stamp(path).is_some());

        let mut info = |path: Option<PathBuf>| -> Option<HostShellInfo> {
            let path = path?;
            seen.insert(path.clone());
            let probe = self.shell_probe(probes, &path);
            Some(HostShellInfo {
                path: normalize(&path),
                version: probe.version,
                language_mode: probe.language_mode,
                psreadline_version: probe.psreadline_version,
                status: if probe.ok {
                    HostProbeStatus::Ok
                } else {
                    HostProbeStatus::Failed
                },
            })
        };
        let pwsh = info(pwsh_path);
        let windows_powershell = info(windows_powershell_path);

        let (preferred, chosen) = match (&pwsh, &windows_powershell) {
            (Some(pwsh), _) => (Some("pwsh"), Some(pwsh)),
            (None, Some(legacy)) => (Some("powershell"), Some(legacy)),
            (None, None) => (None, None),
        };
        HostShellFacts {
            preferred: preferred.map(str::to_string),
            psreadline_version: chosen.and_then(|shell| shell.psreadline_version.clone()),
            constrained_language_mode: chosen
                .and_then(|shell| shell.language_mode.as_deref())
                .map(|mode| mode == "ConstrainedLanguage"),
            pwsh,
            windows_powershell,
            checked_at_ms: wall_ms,
        }
    }

    /// Aliases from the preferred shell's probe (pwsh first).
    fn preferred_aliases(&self, shells: &HostShellFacts) -> BTreeMap<String, String> {
        let preferred_path = shells
            .pwsh
            .as_ref()
            .or(shells.windows_powershell.as_ref())
            .map(|shell| shell.path.clone());
        self.shells
            .iter()
            .find(|(path, _)| Some(self.normalizer.normalize(path)) == preferred_path)
            .map(|(_, cached)| cached.value.aliases.clone())
            .unwrap_or_default()
    }

    fn shell_probe(&mut self, probes: &dyn HostProbes, path: &Path) -> ShellProbe {
        let Some(stamp) = probes.file_stamp(path) else {
            return ShellProbe::default();
        };
        if let Some(cached) = self.shells.get(path).filter(|cached| cached.stamp == stamp) {
            return cached.value.clone();
        }
        let encoded = encode_powershell_command(SHELL_PROBE_SCRIPT);
        let run = probes.run(
            path,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &encoded,
            ],
            SHELL_PROBE_TIMEOUT,
        );
        // A non-terminating error in the script (for example `Get-Alias` with
        // a name that has no alias) can set a non-zero exit code; the parsed
        // version line, not the exit code, decides whether the probe worked.
        let value = match run {
            ProbeRun::Exited { stdout, .. } => parse_shell_probe(&stdout),
            ProbeRun::TimedOut | ProbeRun::SpawnFailed => ShellProbe::default(),
        };
        self.shells.insert(
            path.to_path_buf(),
            Cached {
                stamp,
                value: value.clone(),
            },
        );
        value
    }

    fn tool_version(&mut self, probes: &dyn HostProbes, path: &Path) -> Option<String> {
        let stamp = probes.file_stamp(path)?;
        if let Some(cached) = self
            .versions
            .get(path)
            .filter(|cached| cached.stamp == stamp)
        {
            return cached.value.clone();
        }
        let value = probes
            .run(path, &["--version"], VERSION_TIMEOUT)
            .success_stdout()
            .and_then(first_line)
            .map(|line| clip(&self.normalizer.redact_text(line), VERSION_TEXT_LIMIT));
        self.versions.insert(
            path.to_path_buf(),
            Cached {
                stamp,
                value: value.clone(),
            },
        );
        value
    }

    fn probe_gpu(
        &self,
        probes: &dyn HostProbes,
        normalize: &dyn Fn(&Path) -> String,
        wall_ms: u64,
    ) -> HostGpuFacts {
        let Some(hit) = search_path(probes, "nvidia-smi", normalize)
            .into_iter()
            .next()
        else {
            return HostGpuFacts {
                nvidia_smi: HostProbeStatus::Missing,
                names: Vec::new(),
                driver_version: None,
                checked_at_ms: wall_ms,
            };
        };
        let run = probes.run(
            &hit.raw,
            &["--query-gpu=name,driver_version", "--format=csv,noheader"],
            NVIDIA_SMI_TIMEOUT,
        );
        match run {
            ProbeRun::Exited {
                success: true,
                stdout,
            } => {
                let (names, driver_version) = parse_nvidia_smi(&stdout);
                HostGpuFacts {
                    nvidia_smi: HostProbeStatus::Ok,
                    names,
                    driver_version,
                    checked_at_ms: wall_ms,
                }
            }
            ProbeRun::Exited { .. } | ProbeRun::TimedOut | ProbeRun::SpawnFailed => HostGpuFacts {
                nvidia_smi: HostProbeStatus::Failed,
                names: Vec::new(),
                driver_version: None,
                checked_at_ms: wall_ms,
            },
        }
    }
}

fn default_git_root(probes: &dyn HostProbes) -> Option<PathBuf> {
    if !probes.windows() {
        return None;
    }
    let root = PathBuf::from(probes.env_var("ProgramFiles")?).join("Git");
    probes
        .file_stamp(&root.join("cmd").join("git.exe"))
        .map(|_| root)
}

fn git_bash_path(
    probes: &dyn HostProbes,
    git_root: Option<&Path>,
    windows: bool,
    normalize: &dyn Fn(&Path) -> String,
) -> Option<String> {
    if !windows {
        return search_path(probes, "bash", normalize)
            .into_iter()
            .next()
            .map(|hit: PathHit| hit.normalized);
    }
    let root = git_root?;
    [
        root.join("bin").join("bash.exe"),
        root.join("usr").join("bin").join("bash.exe"),
    ]
    .into_iter()
    .find(|candidate| probes.file_stamp(candidate).is_some())
    .map(|path| normalize(&path))
}

/// `-EncodedCommand` payload: base64 of the UTF-16LE script.
pub(crate) fn encode_powershell_command(script: &str) -> String {
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// Printable, length-capped text.
fn clip(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(limit)
        .collect::<String>()
        .trim()
        .to_string()
}

fn token(text: &str) -> Option<String> {
    let value: String = text
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'))
        .take(64)
        .collect();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn parse_shell_probe(stdout: &str) -> ShellProbe {
    let mut probe = ShellProbe::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("v=") {
            probe.version = token(value);
        } else if let Some(value) = line.strip_prefix("lm=") {
            probe.language_mode = token(value);
        } else if let Some(value) = line.strip_prefix("psrl=") {
            probe.psreadline_version = token(value);
        } else if let Some(value) = line.strip_prefix("alias=") {
            if probe.aliases.len() >= ALIAS_LIMIT {
                continue;
            }
            if let Some((name, definition)) = value.split_once('=') {
                if let (Some(name), Some(definition)) = (token(name), token(definition)) {
                    probe.aliases.insert(name.to_ascii_lowercase(), definition);
                }
            }
        }
    }
    probe.ok = probe.version.is_some();
    probe
}

pub(crate) fn parse_nvidia_smi(stdout: &str) -> (Vec<String>, Option<String>) {
    let mut names = Vec::new();
    let mut driver = None;
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (name, version) = match line.rsplit_once(',') {
            Some((name, version)) => (name.trim(), token(version)),
            None => (line, None),
        };
        if driver.is_none() {
            driver = version;
        }
        if names.len() < MAX_GPUS && !name.is_empty() {
            names.push(clip(name, NAME_TEXT_LIMIT));
        }
    }
    (names, driver)
}

// The collector tests model a Windows host with Windows path syntax.
#[cfg(all(test, windows))]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;

use super::*;
use crate::api::schema::host::HostToolSource;
use crate::platform::HostOsInfo;

/// Scripted host: files are stamps in a map, process runs are answered by
/// executable file name and recorded, and nothing touches the real machine.
struct FakeProbes {
    env: HashMap<String, OsString>,
    files: RefCell<HashMap<PathBuf, FileStamp>>,
    outputs: RefCell<HashMap<String, ProbeRun>>,
    runs: RefCell<Vec<(String, Vec<String>)>>,
    docker: DockerEnginePing,
    wsl: Option<Vec<String>>,
}

const STAMP: FileStamp = FileStamp {
    modified_ms: 1,
    len: 10,
};

impl FakeProbes {
    fn new(path: &str) -> Self {
        let mut env = HashMap::new();
        for (name, value) in [
            ("PATH", path),
            ("PATHEXT", ".COM;.EXE;.BAT;.CMD;.PS1"),
            ("USERPROFILE", r"C:\Users\alice"),
            ("LOCALAPPDATA", r"C:\Users\alice\AppData\Local"),
            ("ProgramFiles", r"C:\Program Files"),
            ("SystemRoot", r"C:\Windows"),
            ("USERNAME", "alice"),
            ("COMPUTERNAME", "ALICE-PC"),
        ] {
            env.insert(name.to_string(), OsString::from(value));
        }
        Self {
            env,
            files: RefCell::new(HashMap::new()),
            outputs: RefCell::new(HashMap::new()),
            runs: RefCell::new(Vec::new()),
            docker: DockerEnginePing::NotRunning,
            wsl: None,
        }
    }

    fn file(self, dir: &str, name: &str) -> Self {
        self.files
            .borrow_mut()
            .insert(PathBuf::from(dir).join(name), STAMP);
        self
    }

    fn output(self, file_name: &str, run: ProbeRun) -> Self {
        self.outputs
            .borrow_mut()
            .insert(file_name.to_ascii_lowercase(), run);
        self
    }

    fn runs_of(&self, file_name: &str) -> usize {
        self.runs
            .borrow()
            .iter()
            .filter(|(name, _)| name == file_name)
            .count()
    }
}

fn ok(stdout: &str) -> ProbeRun {
    ProbeRun::Exited {
        success: true,
        stdout: stdout.to_string(),
    }
}

impl HostProbes for FakeProbes {
    fn env_var(&self, name: &str) -> Option<OsString> {
        self.env.get(name).cloned()
    }

    fn file_stamp(&self, path: &Path) -> Option<FileStamp> {
        self.files.borrow().get(path).copied()
    }

    fn run(&self, program: &Path, args: &[&str], _timeout: Duration) -> ProbeRun {
        assert!(program.is_absolute(), "probe ran a relative program");
        let name = program
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        self.runs.borrow_mut().push((
            name.clone(),
            args.iter().map(|arg| arg.to_string()).collect(),
        ));
        self.outputs
            .borrow()
            .get(&name)
            .cloned()
            .unwrap_or(ProbeRun::SpawnFailed)
    }

    fn docker_ping(&self, _timeout: Duration) -> DockerEnginePing {
        self.docker
    }

    fn os_info(&self) -> HostOsInfo {
        HostOsInfo {
            name: "Windows 11".into(),
            version: Some("24H2".into()),
            build: Some("26100.4061".into()),
            arch: "x86_64".into(),
        }
    }

    fn total_memory_bytes(&self) -> Option<u64> {
        Some(31 * 1024 * 1024 * 1024)
    }

    fn disk_free_bytes(&self, _path: &Path) -> Option<u64> {
        Some(150 * 1024 * 1024 * 1024)
    }

    fn wsl_distros(&self) -> Option<Vec<String>> {
        self.wsl.clone()
    }

    fn windows(&self) -> bool {
        true
    }
}

const SYS32: &str = r"C:\Windows\System32";
const GIT_CMD: &str = r"C:\Program Files\Git\cmd";
const GIT_ROOT: &str = r"C:\Program Files\Git";
const GIT_USR_BIN: &str = r"C:\Program Files\Git\usr\bin";
const SCOOP_SHIMS: &str = r"C:\Users\alice\scoop\shims";
const PWSH_DIR: &str = r"C:\Program Files\PowerShell\7";

fn typical_host() -> FakeProbes {
    FakeProbes::new(&format!("{SYS32};{PWSH_DIR};{GIT_CMD};{SCOOP_SHIMS}"))
        .file(SYS32, "find.exe")
        .file(SYS32, "bash.exe")
        .file(&format!(r"{SYS32}\WindowsPowerShell\v1.0"), "powershell.exe")
        .file(PWSH_DIR, "pwsh.exe")
        .file(GIT_CMD, "git.exe")
        .file(&format!(r"{GIT_ROOT}\bin"), "bash.exe")
        .file(GIT_USR_BIN, "sed.exe")
        .file(GIT_USR_BIN, "find.exe")
        .file(SCOOP_SHIMS, "rg.exe")
        .output("rg.exe", ok("ripgrep 14.1.1\n\nfeatures:+pcre2\n"))
        .output("git.exe", ok("git version 2.47.0.windows.1\n"))
        .output(
            "pwsh.exe",
            ok("v=7.5.1\nlm=FullLanguage\npsrl=2.3.6\nalias=diff=Compare-Object\nalias=sort=Sort-Object\n"),
        )
        .output(
            "powershell.exe",
            ok("v=5.1.26100.4061\nlm=FullLanguage\npsrl=2.0.0\n"),
        )
}

fn collect_once(probes: &FakeProbes, mono_ms: u64, collector: &mut Collector) -> HostFacts {
    collector.collect(probes, mono_ms, 1_700_000_000_000 + mono_ms, false)
}

fn tool<'a>(facts: &'a HostFacts, name: &str) -> &'a HostToolFact {
    facts
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("tool {name} missing from report"))
}

fn new_collector(probes: &FakeProbes) -> Collector {
    Collector::new(
        probes,
        60,
        PathBuf::from(r"C:\Users\alice\AppData\Local\kelpie"),
    )
}

#[test]
fn tier_schedule_uses_injected_clock() {
    let mut schedule = TierSchedule::new(GPU_TIER_MS);
    assert!(schedule.due(0, false).gpu);
    assert!(!schedule.due(60_000, false).gpu);
    assert!(!schedule.due(299_999, false).gpu);
    assert!(schedule.due(300_000, false).gpu);
    let tiers = schedule.due(360_000, false);
    assert!(tiers.path && tiers.docker && !tiers.gpu);
    // A forced refresh runs the GPU tier and restarts its period.
    assert!(schedule.due(400_000, true).gpu);
    assert!(!schedule.due(650_000, false).gpu);
    assert!(schedule.due(700_000, false).gpu);
}

#[test]
fn missing_tools_report_missing() {
    let probes = typical_host();
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    for name in [
        "fd",
        "sd",
        "jq",
        "python",
        "node",
        "cargo",
        "docker",
        "nvidia-smi",
    ] {
        let fact = tool(&facts, name);
        assert_eq!(fact.status, HostToolStatus::Missing, "{name}");
        assert_eq!(fact.path, None, "{name}");
    }
    assert_eq!(facts.tools.len(), TOOLPACK.len());
}

#[test]
fn toolpack_reports_sources_shadowing_and_versions() {
    let probes = typical_host();
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));

    let rg = tool(&facts, "rg");
    assert_eq!(rg.status, HostToolStatus::Ok);
    assert_eq!(
        rg.path.as_deref(),
        Some(r"%USERPROFILE%\scoop\shims\rg.exe")
    );
    assert_eq!(rg.source, Some(HostToolSource::Scoop));
    assert_eq!(rg.version.as_deref(), Some("ripgrep 14.1.1"));

    // System32 find wins and Git's find is not on PATH.
    let find = tool(&facts, "find");
    assert_eq!(find.status, HostToolStatus::Missing);
    assert_eq!(
        find.path.as_deref(),
        Some(r"%ProgramFiles%\Git\usr\bin\find.exe")
    );
    assert_eq!(find.source, Some(HostToolSource::GitForWindows));
    assert_eq!(
        find.shadowed_by.as_deref(),
        Some(r"%SystemRoot%\System32\find.exe")
    );

    // diff is not installed but the pwsh alias is still reported.
    let diff = tool(&facts, "diff");
    assert_eq!(diff.status, HostToolStatus::Missing);
    assert_eq!(
        diff.shadowed_by.as_deref(),
        Some("pwsh alias diff -> Compare-Object")
    );

    // The WSL launcher is not Git Bash.
    let bash = tool(&facts, "bash");
    assert_eq!(bash.status, HostToolStatus::Missing);
    assert_eq!(
        bash.path.as_deref(),
        Some(r"%ProgramFiles%\Git\bin\bash.exe")
    );
    assert_eq!(
        facts.git.bash_path.as_deref(),
        Some(r"%ProgramFiles%\Git\bin\bash.exe")
    );
    assert_eq!(
        facts.git.git_path.as_deref(),
        Some(r"%ProgramFiles%\Git\cmd\git.exe")
    );
    assert_eq!(probes.runs_of("bash.exe"), 0, "never run the WSL launcher");
    assert_eq!(probes.runs_of("find.exe"), 0, "off-PATH tools are not run");
}

#[test]
fn version_probe_reruns_only_when_path_stamp_changes() {
    let probes = typical_host();
    let mut collector = new_collector(&probes);
    collect_once(&probes, 0, &mut collector);
    collect_once(&probes, 60_000, &mut collector);
    collect_once(&probes, 120_000, &mut collector);
    assert_eq!(probes.runs_of("rg.exe"), 1);
    assert_eq!(probes.runs_of("pwsh.exe"), 1);

    probes.files.borrow_mut().insert(
        PathBuf::from(SCOOP_SHIMS).join("rg.exe"),
        FileStamp {
            modified_ms: 2,
            len: 10,
        },
    );
    collect_once(&probes, 180_000, &mut collector);
    assert_eq!(probes.runs_of("rg.exe"), 2);
    assert_eq!(probes.runs_of("pwsh.exe"), 1);
}

#[test]
fn version_failure_reports_failed() {
    let probes = typical_host().output("rg.exe", ProbeRun::TimedOut);
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    let rg = tool(&facts, "rg");
    assert_eq!(rg.status, HostToolStatus::Failed);
    assert!(rg.path.is_some());
}

#[test]
fn nvidia_smi_missing_reports_missing_without_running_anything() {
    let probes = typical_host();
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.gpu.nvidia_smi, HostProbeStatus::Missing);
    assert!(facts.gpu.names.is_empty());
    assert_eq!(probes.runs_of("nvidia-smi.exe"), 0);
}

#[test]
fn nvidia_smi_hung_reports_failed_and_runs_on_its_own_tier() {
    let probes = typical_host()
        .file(SYS32, "nvidia-smi.exe")
        .output("nvidia-smi.exe", ProbeRun::TimedOut);
    let mut collector = new_collector(&probes);
    let facts = collect_once(&probes, 0, &mut collector);
    assert_eq!(facts.gpu.nvidia_smi, HostProbeStatus::Failed);
    assert_eq!(tool(&facts, "nvidia-smi").status, HostToolStatus::Failed);
    for beat in 1..5 {
        collect_once(&probes, beat * 60_000, &mut collector);
    }
    assert_eq!(
        probes.runs_of("nvidia-smi.exe"),
        1,
        "300 s tier, not every beat"
    );
    collect_once(&probes, 300_000, &mut collector);
    assert_eq!(probes.runs_of("nvidia-smi.exe"), 2);
}

#[test]
fn nvidia_smi_success_reports_gpus_and_driver() {
    let probes = typical_host().file(SYS32, "nvidia-smi.exe").output(
        "nvidia-smi.exe",
        ok("NVIDIA GeForce RTX 4090, 560.94\nNVIDIA RTX A2000, 560.94\n"),
    );
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.gpu.nvidia_smi, HostProbeStatus::Ok);
    assert_eq!(
        facts.gpu.names,
        vec!["NVIDIA GeForce RTX 4090", "NVIDIA RTX A2000"]
    );
    assert_eq!(facts.gpu.driver_version.as_deref(), Some("560.94"));
    let entry = tool(&facts, "nvidia-smi");
    assert_eq!(entry.status, HostToolStatus::Ok);
    assert_eq!(entry.version.as_deref(), Some("560.94"));
    let (_, args) = probes
        .runs
        .borrow()
        .iter()
        .find(|(name, _)| name == "nvidia-smi.exe")
        .cloned()
        .expect("nvidia-smi ran");
    assert_eq!(
        args,
        vec!["--query-gpu=name,driver_version", "--format=csv,noheader"]
    );
}

#[test]
fn docker_status_maps_ping_and_cli_presence() {
    let mut probes = typical_host();
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.docker.status, HostDockerStatus::Missing);

    probes = probes
        .file(SCOOP_SHIMS, "docker.exe")
        .output("docker.exe", ok("Docker version 27.3.1, build ce12230\n"));
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.docker.status, HostDockerStatus::NotRunning);

    probes.docker = DockerEnginePing::Running;
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.docker.status, HostDockerStatus::Running);

    probes.docker = DockerEnginePing::Failed("timeout");
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.docker.status, HostDockerStatus::Failed);
}

#[test]
fn shells_prefer_pwsh_and_detect_constrained_language() {
    let probes = typical_host();
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.shells.preferred.as_deref(), Some("pwsh"));
    assert_eq!(facts.shells.psreadline_version.as_deref(), Some("2.3.6"));
    assert_eq!(facts.shells.constrained_language_mode, Some(false));
    let pwsh = facts.shells.pwsh.as_ref().expect("pwsh");
    assert_eq!(pwsh.path, r"%ProgramFiles%\PowerShell\7\pwsh.exe");
    assert_eq!(pwsh.version.as_deref(), Some("7.5.1"));
    let legacy = facts.shells.windows_powershell.as_ref().expect("5.1");
    assert_eq!(legacy.version.as_deref(), Some("5.1.26100.4061"));
    // Both shells were probed with fixed, encoded arguments.
    let runs = probes.runs.borrow();
    let (_, args) = runs
        .iter()
        .find(|(name, _)| name == "pwsh.exe")
        .expect("pwsh ran");
    assert_eq!(
        &args[..4],
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand"
        ]
    );
    drop(runs);

    let probes = typical_host().output("pwsh.exe", ok("v=7.5.1\nlm=ConstrainedLanguage\n"));
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.shells.constrained_language_mode, Some(true));
}

#[test]
fn shell_probe_output_counts_even_with_nonzero_exit() {
    let probes = typical_host().output(
        "pwsh.exe",
        ProbeRun::Exited {
            success: false,
            stdout: "v=7.6.6\nlm=FullLanguage\n".into(),
        },
    );
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    let pwsh = facts.shells.pwsh.as_ref().expect("pwsh");
    assert_eq!(pwsh.status, HostProbeStatus::Ok);
    assert_eq!(pwsh.version.as_deref(), Some("7.6.6"));

    let probes = typical_host().output("pwsh.exe", ProbeRun::TimedOut);
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(
        facts.shells.pwsh.as_ref().expect("pwsh").status,
        HostProbeStatus::Failed
    );
    assert_eq!(facts.shells.constrained_language_mode, None);
}

#[test]
fn windows_powershell_is_used_when_pwsh_is_absent() {
    let probes = FakeProbes::new(SYS32)
        .file(
            &format!(r"{SYS32}\WindowsPowerShell\v1.0"),
            "powershell.exe",
        )
        .output(
            "powershell.exe",
            ok("v=5.1.1\nlm=ConstrainedLanguage\npsrl=2.0.0\n"),
        );
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.shells.preferred.as_deref(), Some("powershell"));
    assert!(facts.shells.pwsh.is_none());
    assert_eq!(facts.shells.constrained_language_mode, Some(true));
    assert_eq!(facts.shells.psreadline_version.as_deref(), Some("2.0.0"));
}

#[test]
fn report_never_contains_user_or_host_names() {
    let mut probes = typical_host().output(
        "rg.exe",
        ok("ripgrep 14.1.1 (built by alice on ALICE-PC)\n"),
    );
    probes.wsl = Some(vec!["Ubuntu-24.04".into(), "alice".into()]);
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    let json = serde_json::to_string(&facts).expect("serialize");
    let lower = json.to_lowercase();
    assert!(!lower.contains("alice"), "{json}");
    assert!(!lower.contains(r"c:\\users"), "{json}");
    assert!(facts.wsl.present);
    assert_eq!(facts.wsl.distros[0], "Ubuntu-24.04");
}

#[test]
fn memory_and_disk_are_bucketed() {
    let probes = typical_host();
    let facts = collect_once(&probes, 0, &mut new_collector(&probes));
    assert_eq!(facts.memory.bucket.as_deref(), Some("16-32 GB"));
    assert_eq!(facts.disk.bucket.as_deref(), Some("100-500 GB"));
    assert_eq!(facts.os.arch, "x86_64");
    assert_eq!(facts.refreshed_at_ms, 1_700_000_000_000);
    assert_eq!(facts.docker.checked_at_ms, facts.refreshed_at_ms);
}

#[test]
fn gpu_checked_at_keeps_its_own_tier_time() {
    let probes = typical_host();
    let mut collector = new_collector(&probes);
    let first = collect_once(&probes, 0, &mut collector);
    let second = collect_once(&probes, 60_000, &mut collector);
    assert_eq!(second.gpu.checked_at_ms, first.gpu.checked_at_ms);
    assert!(second.docker.checked_at_ms > first.docker.checked_at_ms);
}

#[test]
fn shell_probe_parsing_sanitises_values() {
    let probe = parse_shell_probe(
        "v=7.5.1\r\nlm=FullLanguage\r\npsrl=2.3.6\r\nalias=diff=Compare-Object\r\nalias=bad\r\nnoise\r\n",
    );
    assert!(probe.ok);
    assert_eq!(probe.version.as_deref(), Some("7.5.1"));
    assert_eq!(probe.language_mode.as_deref(), Some("FullLanguage"));
    assert_eq!(probe.psreadline_version.as_deref(), Some("2.3.6"));
    assert_eq!(
        probe.aliases,
        BTreeMap::from([("diff".to_string(), "Compare-Object".to_string())])
    );
    assert!(!parse_shell_probe("garbage").ok);
}

#[test]
fn nvidia_smi_parsing() {
    let (names, driver) = parse_nvidia_smi("NVIDIA GeForce RTX 3060, 552.22\r\n");
    assert_eq!(names, vec!["NVIDIA GeForce RTX 3060"]);
    assert_eq!(driver.as_deref(), Some("552.22"));
    assert_eq!(parse_nvidia_smi(""), (Vec::new(), None));
}

#[test]
fn encoded_command_is_utf16le_base64() {
    assert_eq!(encode_powershell_command("a"), "YQA=");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encode_powershell_command(SHELL_PROBE_SCRIPT))
        .expect("base64");
    let units: Vec<u16> = decoded
        .chunks(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    assert_eq!(
        String::from_utf16(&units).expect("utf16"),
        SHELL_PROBE_SCRIPT
    );
}

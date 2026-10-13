//! Bounded external probes and the injectable host interface.
//!
//! Every process probe runs an absolute, already-resolved program [CWE-427] with
//! fixed arguments (no shell, no interpolation), a null stdin, a capped stdout,
//! and a hard timeout after which the child is killed and reaped.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::platform::{DockerEnginePing, HostOsInfo};

const STDOUT_LIMIT: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long to wait for the stdout reader after the child exits. A grandchild
/// that inherited the pipe must not hold the probe open.
const READER_GRACE: Duration = Duration::from_millis(500);

/// Outcome of one bounded process probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeRun {
    Exited {
        success: bool,
        stdout: String,
    },
    /// Killed after the timeout.
    TimedOut,
    /// Not started: relative path, missing file, or the OS refused.
    SpawnFailed,
}

impl ProbeRun {
    /// Stdout of a successful run.
    pub(crate) fn success_stdout(&self) -> Option<&str> {
        match self {
            Self::Exited {
                success: true,
                stdout,
            } => Some(stdout),
            _ => None,
        }
    }
}

/// Identity of an executable on disk; a change re-runs its `--version` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub(crate) modified_ms: u64,
    pub(crate) len: u64,
}

/// Everything the collector reads from the host. Tests inject a fake so no test
/// touches real nvidia-smi, Docker, WSL or PATH.
pub(crate) trait HostProbes {
    fn env_var(&self, name: &str) -> Option<OsString>;
    /// Stamp of a regular file (App Execution Alias stubs included), or `None`.
    fn file_stamp(&self, path: &Path) -> Option<FileStamp>;
    fn run(&self, program: &Path, args: &[&str], timeout: Duration) -> ProbeRun;
    fn docker_ping(&self, timeout: Duration) -> DockerEnginePing;
    fn os_info(&self) -> HostOsInfo;
    fn total_memory_bytes(&self) -> Option<u64>;
    fn disk_free_bytes(&self, path: &Path) -> Option<u64>;
    fn wsl_distros(&self) -> Option<Vec<String>>;
    /// Windows path rules: `;` PATH separator, PATHEXT, System32 stand-ins.
    fn windows(&self) -> bool;
}

/// The real host.
pub(crate) struct SystemProbes;

impl HostProbes for SystemProbes {
    fn env_var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name).filter(|value| !value.is_empty())
    }

    fn file_stamp(&self, path: &Path) -> Option<FileStamp> {
        // App Execution Alias stubs cannot be opened for metadata through the
        // link, so fall back to the reparse point itself.
        let meta = std::fs::metadata(path)
            .or_else(|_| std::fs::symlink_metadata(path))
            .ok()?;
        if meta.is_dir() {
            return None;
        }
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        Some(FileStamp {
            modified_ms,
            len: meta.len(),
        })
    }

    fn run(&self, program: &Path, args: &[&str], timeout: Duration) -> ProbeRun {
        run_bounded(program, args, timeout, |_| {})
    }

    fn docker_ping(&self, timeout: Duration) -> DockerEnginePing {
        crate::platform::docker_engine_ping(timeout)
    }

    fn os_info(&self) -> HostOsInfo {
        crate::platform::host_os_info()
    }

    fn total_memory_bytes(&self) -> Option<u64> {
        crate::platform::host_total_memory_bytes()
    }

    fn disk_free_bytes(&self, path: &Path) -> Option<u64> {
        crate::platform::host_disk_free_bytes(path)
    }

    fn wsl_distros(&self) -> Option<Vec<String>> {
        crate::platform::host_wsl_distros()
    }

    fn windows(&self) -> bool {
        cfg!(windows)
    }
}

/// Runs `program args` with a hard timeout. `configure` may only add
/// environment for tests; production passes a no-op.
pub(crate) fn run_bounded(
    program: &Path,
    args: &[&str],
    timeout: Duration,
    configure: impl FnOnce(&mut Command),
) -> ProbeRun {
    if !program.is_absolute() {
        return ProbeRun::SpawnFailed;
    }
    let mut command = crate::noninteractive_process::command(program);
    // Run from the program's own directory, never the server's cwd (often a cloned
    // repo), so DLL and batch lookups cannot pick files from there [CWE-427]. That
    // directory is already first in the DLL search order.
    if let Some(dir) = program.parent() {
        command.current_dir(dir);
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure(&mut command);
    let Ok(mut child) = command.spawn() else {
        return ProbeRun::SpawnFailed;
    };

    let (output_tx, output_rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let spawned = crate::thread_spawn::spawn_named("herdr-host-probe-out", move || {
            let mut buffer = Vec::new();
            let _ = stdout.take(STDOUT_LIMIT as u64).read_to_end(&mut buffer);
            let _ = output_tx.send(buffer);
        });
        if spawned.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return ProbeRun::SpawnFailed;
        }
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        // Reap so no zombie or orphaned handle outlives the probe.
        let _ = child.wait();
        return ProbeRun::TimedOut;
    };
    let stdout = output_rx.recv_timeout(READER_GRACE).unwrap_or_default();
    ProbeRun::Exited {
        success: status.success(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
    }
}

/// Splits a PATH value with the host's separator, dropping empty and relative
/// entries: a relative PATH entry resolves against the server's cwd [CWE-427].
pub(crate) fn path_dirs(path: &OsString, windows: bool) -> Vec<PathBuf> {
    let separator = if windows { ';' } else { ':' };
    path.to_string_lossy()
        .split(separator)
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .filter(|dir| is_absolute_for(dir, windows))
        .collect()
}

/// Absolute-path check by the probed host's rules rather than the build host's.
pub(crate) fn is_absolute_for(path: &Path, windows: bool) -> bool {
    let text = path.to_string_lossy();
    if windows {
        let bytes = text.as_bytes();
        (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
            || text.starts_with(r"\\")
    } else {
        text.starts_with('/')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANG_ENV: &str = "HERDR_HOST_FACTS_TEST_HANG";

    /// Helper process for `probe_timeout_kills_hung_process`. A normal test run
    /// returns at once; only the probe test sets the env var that makes it hang.
    #[test]
    fn probe_timeout_helper_process() {
        if std::env::var_os(HANG_ENV).is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn probe_timeout_kills_hung_process() {
        let exe = std::env::current_exe().expect("test exe path");
        let started = Instant::now();
        let run = run_bounded(
            &exe,
            &[
                "host_facts::probe::tests::probe_timeout_helper_process",
                "--exact",
                "--test-threads=1",
            ],
            Duration::from_millis(300),
            |command| {
                command.env(HANG_ENV, "1");
            },
        );
        assert_eq!(run, ProbeRun::TimedOut);
        // Killed and reaped well before the helper's 30 s sleep would end.
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn probe_runs_helper_to_completion_within_timeout() {
        let exe = std::env::current_exe().expect("test exe path");
        let run = run_bounded(
            &exe,
            &[
                "host_facts::probe::tests::probe_timeout_helper_process",
                "--exact",
                "--test-threads=1",
            ],
            Duration::from_secs(60),
            |command| {
                command.env_remove(HANG_ENV);
            },
        );
        assert!(
            matches!(run, ProbeRun::Exited { success: true, .. }),
            "{run:?}"
        );
    }

    #[test]
    fn probe_refuses_relative_program() {
        assert_eq!(
            run_bounded(
                Path::new("git"),
                &["--version"],
                Duration::from_secs(1),
                |_| {}
            ),
            ProbeRun::SpawnFailed
        );
    }

    #[test]
    fn probe_missing_program_is_spawn_failure() {
        let missing = std::env::temp_dir().join("herdr-host-facts-definitely-missing.exe");
        assert_eq!(
            run_bounded(&missing, &["--version"], Duration::from_secs(1), |_| {}),
            ProbeRun::SpawnFailed
        );
    }

    #[test]
    fn path_dirs_drop_relative_and_empty_entries() {
        let dirs = path_dirs(
            &OsString::from(r#"C:\Tools;;.;"C:\Quoted Dir";bin;\\server\share"#),
            true,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from(r"C:\Tools"),
                PathBuf::from(r"C:\Quoted Dir"),
                PathBuf::from(r"\\server\share"),
            ]
        );
        let dirs = path_dirs(&OsString::from("/usr/bin::./x:/opt/bin"), false);
        assert_eq!(
            dirs,
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/opt/bin")]
        );
    }
}

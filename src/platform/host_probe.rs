//! Shared types and non-Windows fallbacks for host facts probes.
//!
//! The Windows implementations live in `windows/host_probe.rs`. Other platforms
//! answer the same questions with cheap local reads, or `None` when there is
//! no equally cheap answer.

/// Operating system identity without any user or machine names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostOsInfo {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) build: Option<String>,
    /// Native machine architecture (may differ from the process architecture).
    pub(crate) arch: String,
}

/// Outcome of a Docker engine `_ping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerEnginePing {
    Running,
    /// No engine endpoint exists (pipe or socket absent, or connection refused).
    NotRunning,
    /// The endpoint exists but did not answer `_ping` with HTTP 200 in time.
    Failed(&'static str),
}

/// Classifies the first bytes of an HTTP response to `GET /_ping`.
pub(crate) fn classify_docker_ping_response(response: &[u8]) -> DockerEnginePing {
    let line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .unwrap_or(response.len());
    let status_line = String::from_utf8_lossy(&response[..line_end]);
    let mut parts = status_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(version), Some("200")) if version.starts_with("HTTP/1.") => DockerEnginePing::Running,
        (Some(version), Some(_)) if version.starts_with("HTTP/1.") => {
            DockerEnginePing::Failed("http_status")
        }
        _ => DockerEnginePing::Failed("bad_response"),
    }
}

#[cfg(windows)]
pub(crate) use super::windows::host_probe::{
    docker_engine_ping, host_disk_free_bytes, host_os_info, host_total_memory_bytes,
    host_wsl_distros,
};

#[cfg(not(windows))]
pub(crate) fn host_os_info() -> HostOsInfo {
    HostOsInfo {
        name: std::env::consts::OS.to_string(),
        version: None,
        build: None,
        arch: std::env::consts::ARCH.to_string(),
    }
}

#[cfg(not(windows))]
pub(crate) fn host_total_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

#[cfg(not(windows))]
pub(crate) fn host_disk_free_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

#[cfg(not(windows))]
pub(crate) fn host_wsl_distros() -> Option<Vec<String>> {
    None
}

#[cfg(unix)]
pub(crate) fn docker_engine_ping(timeout: std::time::Duration) -> DockerEnginePing {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = match UnixStream::connect("/var/run/docker.sock") {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return DockerEnginePing::NotRunning;
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return DockerEnginePing::Failed("access_denied");
        }
        Err(_) => return DockerEnginePing::Failed("open"),
    };
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return DockerEnginePing::Failed("open");
    }
    if stream
        .write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return DockerEnginePing::Failed("write");
    }
    let mut response = [0u8; 256];
    match stream.read(&mut response) {
        Ok(read) => classify_docker_ping_response(&response[..read]),
        Err(_) => DockerEnginePing::Failed("timeout"),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn docker_engine_ping(_timeout: std::time::Duration) -> DockerEnginePing {
    DockerEnginePing::NotRunning
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_ping_response_classification() {
        assert_eq!(
            classify_docker_ping_response(b"HTTP/1.1 200 OK\r\nApi-Version: 1.47\r\n"),
            DockerEnginePing::Running
        );
        assert_eq!(
            classify_docker_ping_response(b"HTTP/1.1 500 Internal Server Error\r\n"),
            DockerEnginePing::Failed("http_status")
        );
        assert_eq!(
            classify_docker_ping_response(b""),
            DockerEnginePing::Failed("bad_response")
        );
        assert_eq!(
            classify_docker_ping_response(b"garbage"),
            DockerEnginePing::Failed("bad_response")
        );
    }
}

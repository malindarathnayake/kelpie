//! Windows host facts: OS build and native architecture, physical memory, free
//! disk space, WSL distributions and a Docker engine `_ping` over its named pipe.
//!
//! Every call here is bounded: registry and memory reads are local API calls, and
//! the Docker ping uses overlapped I/O with a hard deadline and `CancelIoEx`, so a
//! wedged engine cannot hold the caller past the timeout.

use std::{
    ffi::OsString,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
        ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceExW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::{
        Registry::{
            RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER,
            HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
        },
        SystemInformation::{
            GlobalMemoryStatusEx, IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64,
            IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
            MEMORYSTATUSEX,
        },
        Threading::{GetCurrentProcess, IsWow64Process2},
        IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED},
    },
};

use crate::platform::{DockerEnginePing, HostOsInfo};

const DOCKER_ENGINE_PIPE: &str = r"\\.\pipe\docker_engine";
const DOCKER_PING_REQUEST: &[u8] =
    b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n";
const DOCKER_RESPONSE_LIMIT: usize = 1024;
const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const LXSS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";
const MAX_WSL_DISTROS: u32 = 32;

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub(crate) fn host_os_info() -> HostOsInfo {
    let build = registry_string(HKEY_LOCAL_MACHINE, CURRENT_VERSION_KEY, "CurrentBuild");
    let build_number = build.as_deref().and_then(|value| value.parse::<u32>().ok());
    let ubr = registry_dword(HKEY_LOCAL_MACHINE, CURRENT_VERSION_KEY, "UBR");
    // ProductName still says "Windows 10" on Windows 11; the build number decides.
    let name = match build_number {
        Some(number) if number >= 22000 => "Windows 11",
        Some(_) => "Windows 10",
        None => "Windows",
    };
    let build = match (build, ubr) {
        (Some(build), Some(ubr)) => Some(format!("{build}.{ubr}")),
        (build, _) => build,
    };
    HostOsInfo {
        name: name.to_string(),
        version: registry_string(HKEY_LOCAL_MACHINE, CURRENT_VERSION_KEY, "DisplayVersion"),
        build,
        arch: native_arch().unwrap_or(std::env::consts::ARCH).to_string(),
    }
}

/// Native machine architecture, which differs from the process architecture for
/// an x64 build running under emulation on ARM64.
fn native_arch() -> Option<&'static str> {
    let mut process: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_UNKNOWN;
    // SAFETY: the pseudo handle is always valid and both out-pointers are live locals.
    let ok = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) };
    if ok == 0 {
        return None;
    }
    match native {
        IMAGE_FILE_MACHINE_AMD64 => Some("x86_64"),
        IMAGE_FILE_MACHINE_ARM64 => Some("aarch64"),
        IMAGE_FILE_MACHINE_I386 => Some("x86"),
        _ => None,
    }
}

pub(crate) fn host_total_memory_bytes() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a correctly sized MEMORYSTATUSEX with dwLength set.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0).then_some(status.ullTotalPhys)
}

pub(crate) fn host_disk_free_bytes(path: &Path) -> Option<u64> {
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller = 0u64;
    // SAFETY: the path is NUL-terminated and the out-pointer is a live local;
    // the two optional totals are null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut free_to_caller,
            null_mut(),
            null_mut(),
        )
    };
    (ok != 0).then_some(free_to_caller)
}

/// Registered WSL distribution names, or `None` when WSL is not installed.
/// Reads the per-user registry only; it never starts `wsl.exe` or the WSL VM.
pub(crate) fn host_wsl_distros() -> Option<Vec<String>> {
    let system_root = std::env::var_os("SystemRoot")?;
    let launcher = Path::new(&system_root).join("System32").join("wsl.exe");
    if !launcher.is_file() {
        return None;
    }
    let mut distros = Vec::new();
    let Some(key) = open_key(HKEY_CURRENT_USER, LXSS_KEY) else {
        return Some(distros);
    };
    for index in 0..MAX_WSL_DISTROS {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `key` is open, `name` is a writable buffer of `len` UTF-16 units,
        // and the class/time out-pointers are null.
        let status = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut len,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            break;
        }
        let subkey = format!(
            r"{LXSS_KEY}\{}",
            OsString::from_wide(&name[..len as usize]).to_string_lossy()
        );
        if let Some(distro) = registry_string(HKEY_CURRENT_USER, &subkey, "DistributionName") {
            distros.push(distro);
        }
    }
    Some(distros)
}

struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegOpenKeyExW and is closed exactly once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn open_key(root: HKEY, subkey: &str) -> Option<RegKey> {
    let subkey = wide(subkey);
    let mut key: HKEY = null_mut();
    // SAFETY: `subkey` is NUL-terminated and `key` is a live out-pointer.
    let status = unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    (status == ERROR_SUCCESS).then_some(RegKey(key))
}

fn registry_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut buffer = [0u16; 512];
    let mut bytes = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: both names are NUL-terminated; the buffer is writable for `bytes` bytes.
    let status = unsafe {
        RegGetValueW(
            root,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (bytes as usize / 2).min(buffer.len());
    let text = OsString::from_wide(&buffer[..units]);
    let text = text.to_string_lossy();
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn registry_dword(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated; `data` is a writable u32.
    let status = unsafe {
        RegGetValueW(
            root,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut data as *mut u32).cast(),
            &mut bytes,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// `GET /_ping` against the default Docker engine pipe with one overall deadline.
pub(crate) fn docker_engine_ping(timeout: Duration) -> DockerEnginePing {
    let deadline = Instant::now() + timeout;
    let name = wide(DOCKER_ENGINE_PIPE);
    // SAFETY: `name` is NUL-terminated. SECURITY_IDENTIFICATION stops the pipe
    // server from impersonating this process beyond identification.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        // SAFETY: reads the calling thread's last error immediately after the call.
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => DockerEnginePing::NotRunning,
            ERROR_PIPE_BUSY => DockerEnginePing::Failed("pipe_busy"),
            ERROR_ACCESS_DENIED => DockerEnginePing::Failed("access_denied"),
            _ => DockerEnginePing::Failed("open"),
        };
    }
    // SAFETY: `raw` is a valid handle we own from here on.
    let pipe = unsafe { OwnedHandle::from_raw_handle(raw) };

    if overlapped_io(&pipe, IoOp::Write(DOCKER_PING_REQUEST), deadline).is_none() {
        return DockerEnginePing::Failed("write");
    }
    let mut response = Vec::with_capacity(256);
    while response.len() < DOCKER_RESPONSE_LIMIT && !response.windows(2).any(|w| w == b"\r\n") {
        let mut chunk = [0u8; 256];
        match overlapped_io(&pipe, IoOp::Read(&mut chunk), deadline) {
            Some(0) => break,
            Some(read) => response.extend_from_slice(&chunk[..read]),
            None => return DockerEnginePing::Failed("timeout"),
        }
    }
    crate::platform::classify_docker_ping_response(&response)
}

enum IoOp<'a> {
    Write(&'a [u8]),
    Read(&'a mut [u8]),
}

/// One overlapped read or write that finishes before `deadline` or is cancelled.
/// Returns bytes transferred, or `None` on error or timeout.
fn overlapped_io(pipe: &OwnedHandle, op: IoOp<'_>, deadline: Instant) -> Option<usize> {
    let handle = pipe.as_raw_handle();
    let mut overlapped = OVERLAPPED::default();
    let mut transferred = 0u32;
    // SAFETY: the buffer and `overlapped` stay alive until the operation has
    // completed or been cancelled and drained below.
    let started = unsafe {
        match op {
            IoOp::Write(bytes) => WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                null_mut(),
                &mut overlapped,
            ),
            IoOp::Read(buffer) => ReadFile(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                null_mut(),
                &mut overlapped,
            ),
        }
    };
    // SAFETY: reads the last error right after the call above.
    if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
        return None;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let millis = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: `overlapped` belongs to the operation started above.
    let ok = unsafe { GetOverlappedResultEx(handle, &overlapped, &mut transferred, millis, 0) };
    if ok != 0 {
        return Some(transferred as usize);
    }
    // SAFETY: reads the last error right after the call above.
    if unsafe { GetLastError() } == WAIT_TIMEOUT {
        // Cancel the pending operation and wait for the cancellation to land, so
        // the kernel no longer references `overlapped` or the buffer.
        // SAFETY: same handle and OVERLAPPED as the started operation.
        unsafe {
            CancelIoEx(handle, &overlapped);
            GetOverlappedResult(handle, &overlapped, &mut transferred, 1);
        }
    }
    None
}

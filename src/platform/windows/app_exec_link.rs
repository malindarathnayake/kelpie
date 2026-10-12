//! App Execution Alias (`IO_REPARSE_TAG_APPEXECLINK`) resolution.
//!
//! Microsoft Store apps such as PowerShell 7 put zero-byte reparse-point stubs
//! in `%LOCALAPPDATA%\Microsoft\WindowsApps`. `CreateProcessW` launches them,
//! but ordinary file reads fail with `ERROR_CANT_ACCESS_FILE`, so executable
//! validation has to look at the alias target instead of the stub.

use std::{
    ffi::OsString,
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    ptr::null_mut,
};

use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    },
    System::{Ioctl::FSCTL_GET_REPARSE_POINT, IO::DeviceIoControl},
};

const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;
const REPARSE_HEADER_LEN: usize = 8;
const APPEXECLINK_VERSION: u32 = 3;
const APPEXECLINK_VERSION_LEN: usize = 4;
const APPEXECLINK_TARGET_INDEX: usize = 2;

/// Target executable of an App Execution Alias, or `None` when `path` is not
/// one or its reparse data cannot be read.
pub(crate) fn app_exec_link_target(path: &Path) -> Option<PathBuf> {
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let mut returned = 0u32;
    // SAFETY: the handle stays open for the call, the output buffer is valid
    // for `buffer.len()` bytes, and `returned` is a valid out-pointer.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            null_mut(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    parse_app_exec_link(buffer.get(..returned as usize)?)
}

/// Parse a `REPARSE_DATA_BUFFER` holding an App Execution Alias. After the
/// 8-byte header and a 4-byte version (3), the payload is a list of
/// NUL-terminated UTF-16 strings: package family name, app user model id,
/// target path, ... Any other version is rejected rather than guessed at.
fn parse_app_exec_link(data: &[u8]) -> Option<PathBuf> {
    let tag = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    if tag != IO_REPARSE_TAG_APPEXECLINK {
        return None;
    }
    let data_len = u16::from_le_bytes(data.get(4..6)?.try_into().ok()?) as usize;
    let payload = data.get(REPARSE_HEADER_LEN..REPARSE_HEADER_LEN.checked_add(data_len)?)?;
    let version = u32::from_le_bytes(payload.get(..APPEXECLINK_VERSION_LEN)?.try_into().ok()?);
    if version != APPEXECLINK_VERSION {
        return None;
    }
    let strings = payload.get(APPEXECLINK_VERSION_LEN..)?;
    let units = strings
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let target = units
        .split(|unit| *unit == 0)
        .nth(APPEXECLINK_TARGET_INDEX)?;
    if target.is_empty() {
        return None;
    }
    let target = PathBuf::from(OsString::from_wide(target));
    is_local_drive_path(&target).then_some(target)
}

/// Accept only absolute local drive paths, so validating an alias target never
/// opens a UNC share (an outbound SMB connection) or a device path.
fn is_local_drive_path(path: &Path) -> bool {
    let local_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    );
    local_drive && path.is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reparse_buffer(tag: u32, strings: &[&str]) -> Vec<u8> {
        reparse_buffer_with_version(tag, APPEXECLINK_VERSION, strings)
    }

    fn reparse_buffer_with_version(tag: u32, version: u32, strings: &[&str]) -> Vec<u8> {
        let mut payload = version.to_le_bytes().to_vec();
        for value in strings {
            for unit in value.encode_utf16().chain(std::iter::once(0)) {
                payload.extend_from_slice(&unit.to_le_bytes());
            }
        }
        let mut buffer = tag.to_le_bytes().to_vec();
        buffer.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        buffer.extend_from_slice(&payload);
        buffer
    }

    #[test]
    fn parses_store_pwsh_alias_target() {
        let target = r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.6.6.0_x64__8wekyb3d8bbwe\pwsh.exe";
        let buffer = reparse_buffer(
            IO_REPARSE_TAG_APPEXECLINK,
            &[
                "Microsoft.PowerShell_8wekyb3d8bbwe",
                "Microsoft.PowerShell_8wekyb3d8bbwe!App",
                target,
                "0",
            ],
        );
        assert_eq!(parse_app_exec_link(&buffer), Some(PathBuf::from(target)));
    }

    #[test]
    fn rejects_other_tags_truncated_and_relative_targets() {
        let target = r"C:\Apps\pwsh.exe";
        let symlink = reparse_buffer(0xA000_000C, &["a", "b", target]);
        assert_eq!(parse_app_exec_link(&symlink), None);

        let full = reparse_buffer(IO_REPARSE_TAG_APPEXECLINK, &["a", "b", target]);
        assert_eq!(parse_app_exec_link(&full[..full.len() - 6]), None);

        let missing = reparse_buffer(IO_REPARSE_TAG_APPEXECLINK, &["a", "b"]);
        assert_eq!(parse_app_exec_link(&missing), None);

        let relative = reparse_buffer(IO_REPARSE_TAG_APPEXECLINK, &["a", "b", "pwsh.exe"]);
        assert_eq!(parse_app_exec_link(&relative), None);
    }

    #[test]
    fn rejects_unknown_versions_and_non_local_targets() {
        let unknown_version =
            reparse_buffer_with_version(IO_REPARSE_TAG_APPEXECLINK, 4, &["a", "b", r"C:\pwsh.exe"]);
        assert_eq!(parse_app_exec_link(&unknown_version), None);

        for remote in [
            r"\\server\share\pwsh.exe",
            r"\\?\UNC\server\share\pwsh.exe",
            r"\\.\pipe\pwsh.exe",
        ] {
            let buffer = reparse_buffer(IO_REPARSE_TAG_APPEXECLINK, &["a", "b", remote]);
            assert_eq!(parse_app_exec_link(&buffer), None, "{remote}");
        }

        let verbatim = r"\\?\C:\Apps\pwsh.exe";
        let buffer = reparse_buffer(IO_REPARSE_TAG_APPEXECLINK, &["a", "b", verbatim]);
        assert_eq!(parse_app_exec_link(&buffer), Some(PathBuf::from(verbatim)));
    }

    #[test]
    fn resolves_installed_store_alias_when_present() {
        let Some(local) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let alias = PathBuf::from(local).join(r"Microsoft\WindowsApps\pwsh.exe");
        if std::fs::symlink_metadata(&alias).is_err() {
            return;
        }
        let target = app_exec_link_target(&alias).expect("store pwsh alias target");
        assert!(is_local_drive_path(&target));
        assert!(target.ends_with("pwsh.exe"));
    }
}

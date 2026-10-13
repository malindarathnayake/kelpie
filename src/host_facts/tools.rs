//! Detect-only toolpack: PATH resolution, install source inference and
//! shadowing by System32 stand-ins or PowerShell aliases. Nothing here
//! downloads anything or changes PATH.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::api::schema::host::{HostToolSource, HostToolStatus};

use super::probe::{path_dirs, HostProbes};

/// The V1.0 toolpack, in report order.
pub(crate) const TOOLPACK: &[&str] = &[
    "rg",
    "fd",
    "sd",
    "jq",
    "sed",
    "awk",
    "diff",
    "grep",
    "find",
    "git",
    "bash",
    "python",
    "node",
    "cargo",
    "docker",
    "nvidia-smi",
];

/// GNU tools that Git for Windows ships under `usr\bin` even when that
/// directory is not on PATH.
const GIT_USR_BIN_TOOLS: &[&str] = &["sed", "awk", "diff", "grep", "find", "bash"];

/// Only these extensions resolve a bare command name; scripts such as `.ps1`
/// or `.js` are not executables for this purpose.
const EXECUTABLE_EXTENSIONS: &[&str] = &[".exe", ".com", ".bat", ".cmd"];

/// One PATH hit: the raw path (for running and stamping) and its normalised
/// form (for reporting and rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathHit {
    pub(crate) raw: PathBuf,
    pub(crate) normalized: String,
}

/// Every PATH directory's first executable match for `name`, in PATH order.
pub(crate) fn search_path(
    probes: &dyn HostProbes,
    name: &str,
    normalize: &dyn Fn(&Path) -> String,
) -> Vec<PathHit> {
    let windows = probes.windows();
    let Some(path) = probes.env_var("PATH") else {
        return Vec::new();
    };
    let extensions = if windows {
        executable_extensions(probes.env_var("PATHEXT"))
    } else {
        vec![String::new()]
    };
    let mut hits = Vec::new();
    for dir in path_dirs(&path, windows) {
        let found = extensions.iter().find_map(|ext| {
            let candidate = dir.join(format!("{name}{ext}"));
            probes.file_stamp(&candidate).map(|_| candidate)
        });
        if let Some(raw) = found {
            if hits.iter().any(|hit: &PathHit| hit.raw == raw) {
                continue;
            }
            let normalized = normalize(&raw);
            hits.push(PathHit { raw, normalized });
        }
    }
    hits
}

fn executable_extensions(pathext: Option<OsString>) -> Vec<String> {
    let configured: Vec<String> = pathext
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(|ext| ext.trim().to_ascii_lowercase())
                .filter(|ext| EXECUTABLE_EXTENSIONS.contains(&ext.as_str()))
                .collect()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        vec![".com".into(), ".exe".into(), ".bat".into(), ".cmd".into()]
    } else {
        configured
    }
}

/// Lowercase with `/` separators, for rule matching.
fn rule_form(normalized: &str) -> String {
    normalized.replace('\\', "/").to_lowercase()
}

/// Install source inferred from a normalised path.
pub(crate) fn infer_source(normalized: &str) -> HostToolSource {
    let path = rule_form(normalized);
    if path.contains("/scoop/") {
        HostToolSource::Scoop
    } else if path.contains("/microsoft/winget/") {
        HostToolSource::Winget
    } else if path.contains("/chocolatey/") {
        HostToolSource::Chocolatey
    } else if [
        "/git/usr/bin/",
        "/git/bin/",
        "/git/cmd/",
        "/git/mingw64/",
        "/git/mingw32/",
        "/git/ucrt64/",
        "/git/clang64/",
        "/git/clangarm64/",
    ]
    .iter()
    .any(|marker| path.contains(marker))
    {
        HostToolSource::GitForWindows
    } else if path.starts_with("%systemroot%/")
        || ["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
    {
        HostToolSource::System
    } else {
        HostToolSource::Other
    }
}

/// A Windows binary that answers to a GNU tool's name but is not that tool:
/// System32 `find`/`sort`, and the WSL `bash.exe` launchers.
pub(crate) fn is_system_standin(tool: &str, normalized: &str) -> bool {
    let path = rule_form(normalized);
    let in_system =
        path.starts_with("%systemroot%/system32/") || path.starts_with("%systemroot%/syswow64/");
    match tool {
        "find" | "sort" => in_system,
        "bash" => in_system || path.contains("/microsoft/windowsapps/"),
        _ => false,
    }
}

/// Where a tool resolves and what, if anything, wins over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResolution {
    /// The real tool to report and version-probe.
    pub(crate) hit: Option<PathHit>,
    pub(crate) status: HostToolStatus,
    pub(crate) shadowed_by: Option<String>,
}

/// Applies the shadowing rules to `hits` (PATH order).
///
/// - The real tool is the first hit that is not a System32/WSL stand-in.
/// - A stand-in earlier on PATH, or a PowerShell alias of the same name, wins a
///   plain invocation: status `shadowed`, `shadowed_by` names the winner.
/// - No real tool on PATH: status `missing`; `off_path` (for example Git for
///   Windows `usr\bin`) is still reported as the installed location.
pub(crate) fn resolve_tool(
    tool: &str,
    hits: &[PathHit],
    off_path: Option<PathHit>,
    aliases: &BTreeMap<String, String>,
) -> ToolResolution {
    let real_index = hits
        .iter()
        .position(|hit| !is_system_standin(tool, &hit.normalized));
    let standin = hits
        .iter()
        .take(real_index.unwrap_or(hits.len()))
        .find(|hit| is_system_standin(tool, &hit.normalized));
    let mut winners = Vec::new();
    if let Some(standin) = standin {
        winners.push(standin.normalized.clone());
    }
    if let Some(definition) = aliases.get(tool) {
        winners.push(format!("pwsh alias {tool} -> {definition}"));
    }
    let shadowed_by = (!winners.is_empty()).then(|| winners.join("; "));
    match real_index {
        Some(index) => ToolResolution {
            hit: Some(hits[index].clone()),
            status: if shadowed_by.is_some() {
                HostToolStatus::Shadowed
            } else {
                HostToolStatus::Ok
            },
            shadowed_by,
        },
        None => ToolResolution {
            hit: off_path,
            status: HostToolStatus::Missing,
            shadowed_by,
        },
    }
}

/// Off-PATH Git for Windows copy of a GNU tool, given the Git install root.
pub(crate) fn git_usr_bin_candidate(
    probes: &dyn HostProbes,
    git_root: Option<&Path>,
    tool: &str,
    normalize: &dyn Fn(&Path) -> String,
) -> Option<PathHit> {
    if !probes.windows() || !GIT_USR_BIN_TOOLS.contains(&tool) {
        return None;
    }
    let root = git_root?;
    let mut candidates = Vec::new();
    if tool == "bash" {
        candidates.push(root.join("bin").join("bash.exe"));
    }
    candidates.push(root.join("usr").join("bin").join(format!("{tool}.exe")));
    candidates
        .into_iter()
        .find(|candidate| probes.file_stamp(candidate).is_some())
        .map(|raw| PathHit {
            normalized: normalize(&raw),
            raw,
        })
}

/// Git for Windows install root from a resolved `git.exe`
/// (`<root>\cmd\git.exe`, `<root>\bin\git.exe`, or `<root>\<msys>\bin\git.exe` with
/// `<msys>` one of `mingw64`, `mingw32`, `ucrt64`, `clang64`, `clangarm64`).
pub(crate) fn git_root_from_git_path(git: &Path) -> Option<PathBuf> {
    let parent = git.parent()?;
    let parent_name = parent.file_name()?.to_string_lossy().to_ascii_lowercase();
    match parent_name.as_str() {
        "cmd" => parent.parent().map(Path::to_path_buf),
        "bin" => {
            let grand = parent.parent()?;
            let grand_name = grand.file_name()?.to_string_lossy().to_ascii_lowercase();
            let msys_prefix = ["mingw", "ucrt", "clang"]
                .iter()
                .any(|prefix| grand_name.starts_with(prefix));
            if msys_prefix || grand_name == "usr" {
                grand.parent().map(Path::to_path_buf)
            } else {
                Some(grand.to_path_buf())
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(normalized: &str) -> PathHit {
        PathHit {
            raw: PathBuf::from(normalized),
            normalized: normalized.to_string(),
        }
    }

    #[test]
    fn source_inference_from_path() {
        assert_eq!(
            infer_source(r"%ProgramFiles%\Git\usr\bin\sed.exe"),
            HostToolSource::GitForWindows
        );
        assert_eq!(
            infer_source(r"%ProgramFiles%\Git\cmd\git.exe"),
            HostToolSource::GitForWindows
        );
        assert_eq!(
            infer_source(r"%ProgramFiles%\Git\ucrt64\bin\git.exe"),
            HostToolSource::GitForWindows
        );
        assert_eq!(
            infer_source(r"%USERPROFILE%\scoop\shims\rg.exe"),
            HostToolSource::Scoop
        );
        assert_eq!(
            infer_source(r"%USERPROFILE%\scoop\apps\git\current\usr\bin\awk.exe"),
            HostToolSource::Scoop
        );
        assert_eq!(
            infer_source(r"%LOCALAPPDATA%\Microsoft\WinGet\Links\jq.exe"),
            HostToolSource::Winget
        );
        assert_eq!(
            infer_source(r"%ProgramData%\chocolatey\bin\fd.exe"),
            HostToolSource::Chocolatey
        );
        assert_eq!(
            infer_source(r"%SystemRoot%\System32\find.exe"),
            HostToolSource::System
        );
        assert_eq!(infer_source("/usr/bin/grep"), HostToolSource::System);
        assert_eq!(
            infer_source(r"%USERPROFILE%\.cargo\bin\cargo.exe"),
            HostToolSource::Other
        );
        assert_eq!(
            infer_source(r"%ProgramFiles%\nodejs\node.exe"),
            HostToolSource::Other
        );
    }

    #[test]
    fn system32_find_shadows_gnu_find() {
        let hits = [
            hit(r"%SystemRoot%\system32\find.exe"),
            hit(r"%ProgramFiles%\Git\usr\bin\find.exe"),
        ];
        let resolved = resolve_tool("find", &hits, None, &BTreeMap::new());
        assert_eq!(resolved.status, HostToolStatus::Shadowed);
        assert_eq!(
            resolved.hit.map(|h| h.normalized),
            Some(r"%ProgramFiles%\Git\usr\bin\find.exe".to_string())
        );
        assert_eq!(
            resolved.shadowed_by.as_deref(),
            Some(r"%SystemRoot%\system32\find.exe")
        );
    }

    #[test]
    fn gnu_find_first_on_path_is_ok() {
        let hits = [
            hit(r"%ProgramFiles%\Git\usr\bin\find.exe"),
            hit(r"%SystemRoot%\System32\find.exe"),
        ];
        let resolved = resolve_tool("find", &hits, None, &BTreeMap::new());
        assert_eq!(resolved.status, HostToolStatus::Ok);
        assert_eq!(resolved.shadowed_by, None);
    }

    #[test]
    fn only_system32_find_is_missing_with_off_path_copy() {
        let hits = [hit(r"%SystemRoot%\System32\find.exe")];
        let off_path = hit(r"%ProgramFiles%\Git\usr\bin\find.exe");
        let resolved = resolve_tool("find", &hits, Some(off_path.clone()), &BTreeMap::new());
        assert_eq!(resolved.status, HostToolStatus::Missing);
        assert_eq!(resolved.hit, Some(off_path));
        assert_eq!(
            resolved.shadowed_by.as_deref(),
            Some(r"%SystemRoot%\System32\find.exe")
        );
    }

    #[test]
    fn wsl_bash_launcher_is_a_standin() {
        let hits = [
            hit(r"%SystemRoot%\System32\bash.exe"),
            hit(r"%ProgramFiles%\Git\bin\bash.exe"),
        ];
        let resolved = resolve_tool("bash", &hits, None, &BTreeMap::new());
        assert_eq!(resolved.status, HostToolStatus::Shadowed);
        assert!(is_system_standin(
            "bash",
            r"%LOCALAPPDATA%\Microsoft\WindowsApps\bash.exe"
        ));
        // System32 binaries do not shadow tools without a stand-in rule.
        assert!(!is_system_standin(
            "grep",
            r"%SystemRoot%\System32\grep.exe"
        ));
    }

    #[test]
    fn powershell_alias_shadows_diff() {
        let aliases = BTreeMap::from([("diff".to_string(), "Compare-Object".to_string())]);
        let hits = [hit(r"%ProgramFiles%\Git\usr\bin\diff.exe")];
        let resolved = resolve_tool("diff", &hits, None, &aliases);
        assert_eq!(resolved.status, HostToolStatus::Shadowed);
        assert_eq!(
            resolved.shadowed_by.as_deref(),
            Some("pwsh alias diff -> Compare-Object")
        );
        // The alias is still reported when the tool is not installed.
        let resolved = resolve_tool("diff", &[], None, &aliases);
        assert_eq!(resolved.status, HostToolStatus::Missing);
        assert!(resolved.shadowed_by.is_some());
    }

    #[test]
    fn missing_tool_is_missing() {
        let resolved = resolve_tool("sd", &[], None, &BTreeMap::new());
        assert_eq!(
            resolved,
            ToolResolution {
                hit: None,
                status: HostToolStatus::Missing,
                shadowed_by: None,
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn git_root_from_known_layouts() {
        let root = PathBuf::from(r"C:\Program Files\Git");
        assert_eq!(
            git_root_from_git_path(&root.join("cmd").join("git.exe")),
            Some(root.clone())
        );
        assert_eq!(
            git_root_from_git_path(&root.join("bin").join("git.exe")),
            Some(root.clone())
        );
        assert_eq!(
            git_root_from_git_path(&root.join("mingw64").join("bin").join("git.exe")),
            Some(root.clone())
        );
        assert_eq!(
            git_root_from_git_path(&root.join("ucrt64").join("bin").join("git.exe")),
            Some(root.clone())
        );
        assert_eq!(
            git_root_from_git_path(&PathBuf::from(r"C:\tools\git.exe")),
            None
        );
    }

    #[test]
    fn pathext_keeps_only_executables() {
        let exts = executable_extensions(Some(OsString::from(".COM;.EXE;.BAT;.CMD;.VBS;.JS;.PS1")));
        assert_eq!(exts, vec![".com", ".exe", ".bat", ".cmd"]);
        assert_eq!(executable_extensions(None).len(), 4);
    }
}
